//! Ctrl+C 语义：先温和暂停，再取消
//!
//! 传输命令与普通命令要区别对待：
//! * 传输**不能**被硬中断——传输 future 被 drop 就等于放弃收尾机会，任务表里会留下
//!   一个永远 Running 的幽灵任务，下载的 .part 也不保证是合法前缀。因此传输走
//!   TransferControl 的协作式检查点：第一次 Ctrl+C 请求暂停（保住断点），第二次才取消；
//! * 其它命令（列目录、搜索、登录轮询）可以立即放弃：它们没有需要收尾的本地状态。
//!
//! 实现用 tokio 的信号监听 + watch 广播，而不是自己装信号处理器：跨平台（Unix SIGINT /
//! Windows Ctrl+C）由 tokio 统一处理，也避免了在非 Unix 平台上手写 unsafe 代码。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tianyi_core::task::TransferControl;
use tokio::sync::watch;

/// 中断枢纽
pub struct InterruptHub {
    /// 收到的中断次数（1 = 请求暂停，>=2 = 取消）
    hits: AtomicU8,
    /// 当前正在跑的传输控制句柄
    control: Mutex<Option<TransferControl>>,
    /// 中断广播（true = 至少收到过一次中断）
    tx: watch::Sender<bool>,
}

impl InterruptHub {
    /// 新建（不启动监听）
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = watch::channel(false);
        Arc::new(InterruptHub {
            hits: AtomicU8::new(0),
            control: Mutex::new(None),
            tx,
        })
    }

    /// 在当前 runtime 上启动信号监听
    ///
    /// 必须在 tokio runtime 里调用；重复调用是幂等的（只是多一个没有副作用的监听）。
    pub fn install(self: &Arc<Self>) {
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    // 平台不支持信号（极少数嵌入式环境）：直接退出监听，命令照常执行
                    return;
                }
                hub.on_interrupt();
            }
        });
    }

    /// 处理一次中断
    pub fn on_interrupt(&self) {
        let n = self.hits.fetch_add(1, Ordering::SeqCst) + 1;
        let control = self.control.lock().ok().and_then(|g| g.clone());
        match (n, control) {
            (1, Some(ctl)) => {
                // 第一次：请求暂停，保住断点（下载的 .part 仍是合法前缀）
                ctl.pause();
                eprintln!("已请求暂停，正在收尾…（再按一次 Ctrl+C 取消）");
            }
            (2, Some(ctl)) => {
                ctl.cancel();
                eprintln!("已请求取消…");
            }
            (_, Some(_)) => {}
            (1, None) => eprintln!("已中断（再按一次 Ctrl+C 强制退出）"),
            (_, None) => {}
        }
        let _ = self.tx.send(true);
    }

    /// 挂上当前传输的控制句柄
    pub fn attach(&self, ctl: &TransferControl) {
        if let Ok(mut slot) = self.control.lock() {
            *slot = Some(ctl.clone());
        }
    }

    /// 摘掉控制句柄并清零计数（下一条命令重新从「第一次」算起）
    pub fn detach(&self) {
        if let Ok(mut slot) = self.control.lock() {
            *slot = None;
        }
        self.hits.store(0, Ordering::SeqCst);
        let _ = self.tx.send(false);
    }

    /// 已经收到的中断次数
    pub fn hits(&self) -> u8 {
        self.hits.load(Ordering::SeqCst)
    }

    /// 等一次中断（已经中断过则立即返回）
    pub async fn interrupted_signal(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow() {
            return;
        }
        // changed() 依据版本号，因此「刚订阅就中断」也不会丢
        let _ = rx.changed().await;
    }
}

/// 被中断时的统一错误
pub fn interrupted_error() -> crate::error::CliError {
    crate::error::CliError::new(crate::error::ErrorCode::Canceled, "操作已被中断（Ctrl+C）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_hit_pauses_and_second_cancels() {
        let hub = InterruptHub::new();
        let ctl = TransferControl::new();
        hub.attach(&ctl);
        hub.on_interrupt();
        assert_eq!(ctl.state(), tianyi_core::task::ControlState::Paused);
        assert_eq!(hub.hits(), 1);
        hub.on_interrupt();
        assert_eq!(ctl.state(), tianyi_core::task::ControlState::Canceled);
        assert_eq!(hub.hits(), 2);
    }

    #[tokio::test]
    async fn detach_resets_and_drops_control() {
        let hub = InterruptHub::new();
        let ctl = TransferControl::new();
        hub.attach(&ctl);
        hub.on_interrupt();
        hub.detach();
        assert_eq!(hub.hits(), 0);
        // 再中断不应影响已经摘掉的句柄
        hub.on_interrupt();
        assert_eq!(ctl.state(), tianyi_core::task::ControlState::Paused);
    }

    #[tokio::test]
    async fn signal_wakes_waiters_and_is_not_lost() {
        let hub = InterruptHub::new();
        let waiter = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move { hub.interrupted_signal().await })
        };
        tokio::task::yield_now().await;
        hub.on_interrupt();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("必须被唤醒")
            .expect("任务不应 panic");
        // 已经中断过时再次等待应立即返回
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            hub.interrupted_signal(),
        )
        .await
        .expect("已中断应立即返回");
    }
}
