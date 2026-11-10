//! 落盘小工具：整份覆盖写的原子路径
//!
//! `std::fs::write` 先截断目标再写：中途失败（进程被杀、掉电、磁盘写满）留下的就是
//! 半截文件，而读回方的解析失败几乎等价于「数据不存在」——一次写坏就等于丢光历史。
//! 这里统一成「同目录临时文件 → fsync → rename」，目标路径上于是永远只有
//! 「上一份完整内容」或「这一份完整内容」两种状态。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 原子覆盖写入
///
/// 临时文件必须与目标**同目录**：`%TEMP%` 与配置目录经常不在同一个卷上，跨卷 rename
/// 会直接失败（EXDEV）。先 `sync_all` 再 rename，rename 成功时内容已经在盘上，
/// 不会出现「文件名换了、内容却还在页缓存里丢了」。
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = sibling_path(path, "tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
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
