//! 天翼云盘 Slint 桌面客户端

mod app;
mod backend;
mod controller;

use app::MainWindow;
use backend::Backend;
use slint::ComponentHandle;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("tianyi-cloud-rust starting");

    let ui = MainWindow::new()?;
    let ui_handle = ui.as_weak();

    let backend = Arc::new(Backend::new(ui_handle.clone()));
    let controller = controller::Controller::new(ui_handle.clone(), backend);

    // 启动后台任务（keepalive 等）
    controller.spawn_background();

    ui.run()?;
    Ok(())
}
