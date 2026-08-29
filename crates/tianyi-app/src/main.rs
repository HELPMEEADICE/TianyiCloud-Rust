//! 天翼云盘 Slint 桌面客户端

mod app;
mod backend;
mod controller;

use app::MainWindow;
use backend::Backend;
use slint::{ComponentHandle, Timer, Weak};
use std::sync::Arc;

/// 将主窗口在屏幕上居中显示（窗口创建后由事件循环单次触发执行）
fn center_window(ui: &Weak<MainWindow>) {
    use slint::winit_030::{winit, WinitWindowAccessor};

    let ui = ui.clone();
    Timer::single_shot(std::time::Duration::from_millis(100), move || {
        let Some(app) = ui.upgrade() else { return };
        app.window().with_winit_window(|win| {
            let Some(monitor) = win
                .current_monitor()
                .or_else(|| win.primary_monitor())
            else {
                return;
            };
            let mon_pos = monitor.position();
            let mon_size = monitor.size();
            let win_size = win.outer_size();
            let x = mon_pos.x + ((mon_size.width as i32 - win_size.width as i32) / 2).max(0);
            let y = mon_pos.y + ((mon_size.height as i32 - win_size.height as i32) / 2).max(0);
            win.set_outer_position(winit::dpi::Position::Physical(
                winit::dpi::PhysicalPosition::new(x, y),
            ));
        });
    });
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("tianyi-cloud-rust starting");

    let ui = MainWindow::new()?;
    let ui_handle = ui.as_weak();

    let backend = Arc::new(Backend::new(ui_handle.clone()));
    let controller = controller::Controller::new(ui_handle.clone(), backend);

    // 启动后台任务（keepalive 等）
    controller.spawn_background();

    // 尝试用上次使用的账号恢复登录态
    controller.auto_login();

    // 将窗口居中显示
    center_window(&ui_handle);

    ui.run()?;
    Ok(())
}
