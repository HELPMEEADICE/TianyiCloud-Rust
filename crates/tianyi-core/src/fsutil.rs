//! 落盘小工具：整份覆盖写的原子路径
//!
//! `std::fs::write` 先截断目标再写：中途失败（进程被杀、掉电、磁盘写满）留下的就是
//! 半截文件，而读回方的解析失败几乎等价于「数据不存在」——一次写坏就等于丢光历史。
//! 这里统一成「同目录临时文件 → fsync → rename」，目标路径上于是永远只有
//! 「上一份完整内容」或「这一份完整内容」两种状态。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 进程内临时文件序号
///
/// 临时名只有带上 pid 和这个序号才唯一：固定 `\<name>.tmp\` 时，两个实例（或同一实例
/// 的两个线程）会写同一个临时文件，先 rename 的那个拿到的是别人还没写完的内容。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 原子覆盖写入
///
/// 临时文件必须与目标**同目录**：`%TEMP%` 与配置目录经常不在同一个卷上，跨卷 rename
/// 会直接失败（EXDEV）。先 `sync_all` 再 rename，rename 成功时内容已经在盘上，
/// 不会出现「文件名换了、内容却还在页缓存里丢了」。
///
/// 代价：崩在 write 与 rename 之间会留下一个孤儿 `<name>.tmp-<pid>-<序号>`。这类残留
/// 只能靠唯一名避开互相覆盖，**不能**靠写之前删掉同前缀文件来收——那删掉的正是别人
/// 此刻正在写的那个。
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = sibling_path(path, &format!("tmp-{}-{seq}", std::process::id()));
    let done = write_tmp(&tmp, path, bytes);
    if done.is_err() {
        // 失败路径自己收尾：否则磁盘写满这类持续故障会在配置目录里越堆越多
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// 写临时文件并 rename 到目标（`write_atomic` 的实体）
fn write_tmp(tmp: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut file = std::fs::File::create(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// 损坏文件的留档名：`config.json` + `corrupt-1` → `config.json.corrupt-1`
///
/// 保证返回的路径**当前不存在**：带 unix 秒的原始名撞上时（同一秒内第二次损坏，或
/// 上一轮的留档还躺在目录里），依次让到 `-1` … `-9`。这一条不是洁癖——Windows 上
/// `rename` 到已存在的目标会直接失败，而调用点是 `let _ = rename(..)`：一旦重名，
/// 坏文件就留在原位，紧接着写默认值把它覆盖掉，留档等于没做。
///
/// 9 个后缀全撞上时复用 `-9`（覆盖那份旧留档）：这是刻意的不无限循环——留档是为了
/// "还能把内容捞回来"，不是为了把磁盘写满；同一目录堆到 10 份同前缀留档本身说明有人
/// 在反复触发故障，此时保住最新的一份比继续加后缀更有价值。
pub(crate) fn corrupt_path(path: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = sibling_path(path, &format!("corrupt-{secs}"));
    if !base.exists() {
        return base;
    }
    for n in 1..=9 {
        let candidate = sibling_path(path, &format!("corrupt-{secs}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    sibling_path(path, &format!("corrupt-{secs}-9"))
}

/// 与目标同目录、同名前缀的兄弟路径：`config.json` + `corrupt-1` → `config.json.corrupt-1`
///
/// 只改文件名不动父目录，保证临时/留档文件与目标同卷。
pub(crate) fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tianyi-fsutil-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("读取临时目录")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect()
    }

    /// 两个线程同时写同一路径：临时名唯一，谁都不覆盖谁；结束后目标里必须是
    /// **某一份完整内容**（不是两份交错的半截），且不留 `.tmp-*` 残留。
    #[test]
    fn concurrent_write_atomic_keeps_target_intact() {
        let dir = tmp_dir("concurrent");
        let path = dir.join("shared.json");
        let payload_a = "a".repeat(64 * 1024);
        let payload_b = "b".repeat(64 * 1024);

        let handles: Vec<_> = [payload_a.clone(), payload_b.clone()]
            .into_iter()
            .map(|payload| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..16 {
                        write_atomic(&path, payload.as_bytes()).expect("并发写必须成功");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("写线程不应 panic");
        }

        let got = std::fs::read_to_string(&path).expect("目标文件必须存在");
        assert!(
            got == payload_a || got == payload_b,
            "目标内容必须是其中一份完整内容，实际长度 {}",
            got.len()
        );
        let left = leftovers(&dir);
        assert!(left.is_empty(), "rename 之后不应残留临时文件：{left:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
