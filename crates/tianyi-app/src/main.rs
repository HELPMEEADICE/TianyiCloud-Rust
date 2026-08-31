//! 天翼云盘 Slint 桌面客户端

mod app;
mod backend;
mod controller;

use app::MainWindow;
use backend::Backend;
use slint::{ComponentHandle, Timer, Weak};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

/// 拖拽进入状态（跨线程由 winit 事件回调与控制器共享）
struct DragDropState {
    /// 是否正在拖拽（HoveredFile 已进入，尚未 Dropped/Cancelled）
    hovering: AtomicBool,
    /// 已累积的待上传路径（DroppedFile 逐个追加，落盘后清空）
    pending: Mutex<Vec<PathBuf>>,
    /// 当前悬停目标文件夹名（用于遮罩显示）
    hover_target: Mutex<String>,
}

impl DragDropState {
    fn new() -> Self {
        DragDropState {
            hovering: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
            hover_target: Mutex::new(String::new()),
        }
    }
}

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

/// 注册 winit 窗口事件钩子，处理系统拖拽文件进入（上传）
fn install_drag_drop(ui: &Weak<MainWindow>, state: &'static DragDropState) {
    use slint::winit_030::{winit, EventResult, WinitWindowAccessor};

    let Some(app) = ui.upgrade() else { return };
    let window = app.window();
    let ui: Weak<MainWindow> = ui.clone();
    window.on_winit_window_event(move |_slint_window, event| {
        match event {
            winit::event::WindowEvent::HoveredFile(_) => {
                state.hovering.store(true, Ordering::SeqCst);
                // 由控制器根据当前目录刷新遮罩
                let ui = ui.clone();
                let target = state.hover_target.lock().unwrap().clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = ui.upgrade() {
                        app.set_drop_active(true);
                        app.set_drop_target_name(slint::SharedString::from(target));
                    }
                });
            }
            winit::event::WindowEvent::HoveredFileCancelled => {
                state.hovering.store(false, Ordering::SeqCst);
                let ui = ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = ui.upgrade() {
                        app.set_drop_active(false);
                    }
                });
            }
            winit::event::WindowEvent::DroppedFile(path) => {
                state.hovering.store(false, Ordering::SeqCst);
                state.pending.lock().unwrap().push(path.clone());
                // 等同一批文件都到达后统一处理：用一次性 Timer 延迟触发
                let ui = ui.clone();
                slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
                    let paths: Vec<PathBuf> = state.pending.lock().unwrap().drain(..).collect();
                    if paths.is_empty() {
                        return;
                    }
                    if let Some(app) = ui.upgrade() {
                        app.set_drop_active(false);
                        let joined = paths
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join("\n");
                        app.invoke_files_dropped(slint::SharedString::from(joined));
                    }
                });
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("tianyi-cloud-rust starting");

    let ui = MainWindow::new()?;
    let ui_handle = ui.as_weak();

    let backend = Arc::new(Backend::new(ui_handle.clone()));
    let controller = controller::Controller::new(ui_handle.clone(), backend);

    // 注册系统拖拽文件进入（上传）钩子
    let drag_state: &'static DragDropState = Box::leak(Box::new(DragDropState::new()));
    install_drag_drop(&ui_handle, drag_state);
    controller.set_drag_state(drag_state);

    // 启动后台任务（keepalive 等）
    controller.spawn_background();

    // 尝试用上次使用的账号恢复登录态
    controller.auto_login();

    // 将窗口居中显示
    center_window(&ui_handle);

    ui.run()?;
    Ok(())
}
