//! Tokio 运行时与「同步回调 → 异步请求」的桥接
//!
//! 文件系统回调（WinFsp 的 dispatcher 线程、FUSE 的请求线程）都是同步函数，而所有云盘
//! 请求都是异步的。这里集中放三件事，避免各处自己 new Runtime：
//!
//! 1. 进程内唯一的 Tokio 多线程运行时（懒创建）；
//! 2. `block_on` 封装——只在**非**运行时线程上调用；
//! 3. 带超时的 `block_on`——文件系统回调里绝不允许无限等待，否则一个卡住的请求会把整个
//!    挂载（乃至资源管理器窗口）拖死。

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// 进程内共享的运行时
///
/// worker 数量刻意压到 4：挂载期的并发是「若干个小 Range 读 + 偶发列目录」，
/// 线程开多了只会增加内存与上下文切换，不会提高吞吐（真正的瓶颈在云端）。
pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("tianyi-mount-io")
            .enable_all()
            .build()
            .expect("创建 Tokio 运行时失败")
    })
}

/// 运行时句柄（克隆代价很低，可以随结构体到处传）
pub fn handle() -> Handle {
    runtime().handle().clone()
}

/// 在同步上下文里把一个 future 跑完
///
/// 注意：本函数**不能**在运行时线程内部调用（tokio 会 panic）。挂载层的调用点都是
/// WinFsp / FUSE 自己的线程，因此是安全的。
pub fn block_on<F: Future>(fut: F) -> F::Output {
    assert!(
        Handle::try_current().is_err(),
        "block_on 不能在 Tokio 运行时线程内调用（会死锁/panic）"
    );
    runtime().block_on(fut)
}

/// 带超时的 `block_on`
///
/// 超时由 tokio 定时器驱动，因此网络请求会真正被取消（连接被 drop），
/// 不会留一个永远占着连接的僵尸任务。
pub fn block_on_timeout<F>(dur: Duration, fut: F) -> std::result::Result<F::Output, Duration>
where
    F: Future,
{
    block_on(async move { tokio::time::timeout(dur, fut).await }).map_err(|_| dur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_runs_futures_from_sync_context() {
        assert_eq!(block_on(async { 1 + 1 }), 2);
    }

    #[test]
    fn timeout_returns_err_on_expiry() {
        let r = block_on_timeout(Duration::from_millis(20), async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            1
        });
        assert!(r.is_err(), "超时必须真的打断");
    }

    #[test]
    fn timeout_returns_ok_when_fast_enough() {
        let r = block_on_timeout(Duration::from_secs(5), async { 7 });
        assert_eq!(r.unwrap(), 7);
    }

    #[test]
    fn runtime_is_shared_across_calls() {
        assert!(std::ptr::eq(runtime(), runtime()));
    }
}
