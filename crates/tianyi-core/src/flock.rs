//! 跨进程排他文件锁
//!
//! GUI 与 CLI 会同时读写同一份 config.json / accounts.json / tasks.json。原子写只保证
//! 「不会读到半截文件」，管不住「后写的那份把先写的那份的改动顶掉」——两个进程各自
//! 读-改-写时，后写的那个用的是更早的快照。这里提供一个可退让的排他锁，让读-改-写
//! 变成真正的临界区。
//!
//! 实现用 std 自带的本平台文件锁（Unix flock / Windows LockFileEx，Rust 1.89 起稳定），
//! 因此不引入任何额外依赖，也不会在最小化容器里缺库。
//!
//! 语义与取舍：
//! * 抢不到就按 5ms 步进重试，最长等调用方给的 timeout；超时返回 None，**由调用方决定
//!   退化策略**（配置/任务表的策略是「跳过本次写入并打 warn」，绝不硬写）；
//! * 锁文件本身留在磁盘上（不删）：删锁文件与别人 open 之间存在竞态，留着最省心；
//! * 进程崩溃时操作系统会释放锁，不存在「死锁永远锁住」的情形。

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

/// 已持有的排他锁；drop 时释放
#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // 显式释放：Windows 上关闭句柄即可，Unix 上释放 flock。
        // 失败只说明锁已经不在了（或文件描述符已失效），没有补救动作。
        let _ = self.file.unlock();
    }
}

/// 尝试抢排他锁，抢不到返回 None
pub fn try_lock_exclusive(path: &Path) -> Option<FileLock> {
    let file = open_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Some(FileLock { file }),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(e)) => {
            log::debug!("文件锁 {} 获取失败：{e}", path.display());
            None
        }
    }
}

/// 在 timeout 内轮询抢排他锁，超时返回 None
///
/// 轮询间隔取 5ms：一次读-改-写是毫秒级的，5ms 的粒度既不会空转吃 CPU，也不会让
/// 等待方明显延迟。timeout 为 0 时等价于 try_lock_exclusive。
pub fn lock_exclusive(path: &Path, timeout: Duration) -> Option<FileLock> {
    let started = Instant::now();
    loop {
        if let Some(lock) = try_lock_exclusive(path) {
            return Some(lock);
        }
        if started.elapsed() >= timeout {
            log::warn!(
                "文件锁 {} 等待 {:?} 超时，本次操作按「不写」处理",
                path.display(),
                timeout
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 打开（必要时创建）锁文件
fn open_lock_file(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            log::debug!("打开锁文件 {} 失败：{e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tianyi-flock-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir.join("x.lock")
    }

    /// 同一路径上第二个锁必须抢不到；第一个释放后才能拿到
    #[test]
    fn second_lock_is_refused_until_first_is_dropped() {
        let path = tmp_path("excl");
        let first = lock_exclusive(&path, Duration::from_millis(50)).expect("首把锁必须拿到");
        assert!(
            try_lock_exclusive(&path).is_none(),
            "同一路径的第二把排他锁必须被拒绝"
        );
        drop(first);
        assert!(try_lock_exclusive(&path).is_some(), "释放后必须能重新拿到");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 超时路径：等不到锁时返回 None 而不是永久阻塞
    #[test]
    fn lock_timeout_returns_none() {
        let path = tmp_path("timeout");
        let held = lock_exclusive(&path, Duration::from_millis(50)).expect("首把锁");
        let started = Instant::now();
        assert!(lock_exclusive(&path, Duration::from_millis(30)).is_none());
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "应真的等过一轮再放弃"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
