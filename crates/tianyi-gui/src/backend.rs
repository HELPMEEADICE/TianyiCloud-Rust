//! 后端逻辑：桥接 UI 事件与 tianyi-core 异步操作

use crate::app::MainWindow;
use slint::Weak;
use std::sync::Arc;

/// 后端调度器
pub struct Backend {
    runtime: Arc<tokio::runtime::Runtime>,
}

impl Backend {
    pub fn new(_ui: Weak<MainWindow>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("build tokio runtime");
        Backend {
            runtime: Arc::new(runtime),
        }
    }

    /// 在 tokio runtime 中执行异步任务，并回到 UI 线程更新
    pub fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(f());
    }
}

/// 封装跨线程调用 UI 的辅助函数
pub fn invoke_ui<F>(ui: &Weak<MainWindow>, f: F)
where
    F: FnOnce(&MainWindow) + Send + 'static,
{
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            f(&ui);
        }
    });
}
