//! 挂载期读写统计
//!
//! GUI 要显示「当前网络读取速度 / 累计读取流量」，CLI 的 mount status 也要，
//! 因此统计放在挂载层而不是各自实现。这里是纯内存的原子/互斥计数，
//! **不落盘**——零磁盘缓存同样适用于统计（配置持久化是另一件事，不在这里）。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 速度统计的滑动窗口长度
const SPEED_WINDOW: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct ReadStats {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    bytes_total: u64,
    requests: u64,
    errors: u64,
    /// (采样时刻, 该时刻读到的字节数)
    window: VecDeque<(Instant, u64)>,
    last_error: Option<String>,
    last_error_at: Option<Instant>,
}

/// 统计快照（可序列化：CLI 的 `--json` 与 GUI 都直接用它）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StatsSnapshot {
    /// 累计读出的字节数
    pub bytes_total: u64,
    /// 累计 Range 请求次数
    pub requests: u64,
    /// 累计失败次数
    pub errors: u64,
    /// 最近 3 秒的平均读取速度（字节/秒）
    pub bytes_per_sec: f64,
    /// 最近一次错误（已脱敏：只保留错误摘要，不含直链与 token）
    pub last_error: Option<String>,
}

impl ReadStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一次成功的 Range 读
    pub fn record_read(&self, bytes: u64) {
        let mut g = self.lock();
        g.bytes_total = g.bytes_total.saturating_add(bytes);
        g.requests = g.requests.saturating_add(1);
        let now = Instant::now();
        g.window.push_back((now, bytes));
        prune(&mut g.window, now);
    }

    /// 记一次失败（消息会先脱敏再保存）
    pub fn record_error(&self, message: impl Into<String>) {
        let mut g = self.lock();
        g.errors = g.errors.saturating_add(1);
        g.last_error = Some(scrub(&message.into()));
        g.last_error_at = Some(Instant::now());
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let mut g = self.lock();
        let now = Instant::now();
        prune(&mut g.window, now);
        let window_bytes: u64 = g.window.iter().map(|(_, b)| *b).sum();
        let seconds = SPEED_WINDOW.as_secs_f64();
        StatsSnapshot {
            bytes_total: g.bytes_total,
            requests: g.requests,
            errors: g.errors,
            bytes_per_sec: window_bytes as f64 / seconds,
            last_error: g.last_error.clone(),
        }
    }

    /// 清空「最近一次错误」提示（GUI 里点掉错误条时用）
    pub fn clear_last_error(&self) {
        let mut g = self.lock();
        g.last_error = None;
        g.last_error_at = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn prune(window: &mut VecDeque<(Instant, u64)>, now: Instant) {
    while let Some((at, _)) = window.front() {
        if now.duration_since(*at) > SPEED_WINDOW {
            window.pop_front();
        } else {
            break;
        }
    }
}

/// 脱敏：任何看起来像签名参数的东西都不能进日志/界面
///
/// 天翼云直链带 `sign` / `accessToken` 之类的查询参数，错误消息里一旦带出来就是
/// 明文凭据泄露。这里统一按「查询串整体丢掉」处理——错误诊断只需要路径与状态码。
pub fn scrub(message: &str) -> String {
    let cut = message.find('?').unwrap_or(message.len());
    let head = &message[..cut];
    head.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_speed_are_reported() {
        let s = ReadStats::new();
        s.record_read(1024);
        s.record_read(1024);
        let snap = s.snapshot();
        assert_eq!(snap.bytes_total, 2048);
        assert_eq!(snap.requests, 2);
        assert!(snap.bytes_per_sec > 0.0, "窗口内刚读过，速度不应是 0");
    }

    #[test]
    fn errors_are_counted_and_scrubbed() {
        let s = ReadStats::new();
        s.record_error("http 403: https://cdn.example.com/a.bin?sign=SECRET&token=SECRET2");
        let snap = s.snapshot();
        assert_eq!(snap.errors, 1);
        let msg = snap.last_error.unwrap();
        assert!(!msg.contains("SECRET"), "签名参数必须脱敏：{msg}");
        assert!(msg.contains("403"));
    }

    #[test]
    fn clearing_error_keeps_counters() {
        let s = ReadStats::new();
        s.record_read(10);
        s.record_error("boom");
        s.clear_last_error();
        let snap = s.snapshot();
        assert!(snap.last_error.is_none());
        assert_eq!(snap.bytes_total, 10);
        assert_eq!(snap.errors, 1);
    }

    #[test]
    fn empty_snapshot_is_zeroed() {
        let snap = ReadStats::new().snapshot();
        assert_eq!(snap.bytes_total, 0);
        assert_eq!(snap.bytes_per_sec, 0.0);
        assert!(snap.last_error.is_none());
    }
}
