//! 天翼云盘 Slint 桌面客户端

mod app;
mod backend;
mod controller;
#[cfg(windows)]
mod dragout;

#[cfg(test)]
mod ui_tests;

use app::MainWindow;
use backend::Backend;
use slint::{ComponentHandle, Timer, TimerMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 传输面板刷新间隔（毫秒）
const TICK_MS: u64 = 200;
/// 跟随系统主题时的轮询间隔（秒）
const THEME_POLL_SECS: u64 = 3;

/// 拖拽进入状态（跨线程由 winit 事件回调与控制器共享）
struct DragDropState {
    /// 是否正在拖拽（HoveredFile 已进入，尚未 Dropped/Cancelled）
    hovering: AtomicBool,
    /// 已累积的待上传路径（DroppedFile 逐个追加，落盘后清空）
    pending: Mutex<Vec<PathBuf>>,
    /// 当前悬停目标文件夹名（用于遮罩显示）
    hover_target: Mutex<String>,
    /// 批次序号：只有最后一次调度的定时器负责消费本批路径，避免重复入队
    batch: Mutex<u64>,
}

impl DragDropState {
    fn new() -> Self {
        DragDropState {
            hovering: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
            hover_target: Mutex::new(String::new()),
            batch: Mutex::new(0),
        }
    }
}

/// 将主窗口在屏幕上居中显示（窗口创建后由事件循环单次触发执行）
fn center_window(ui: &slint::Weak<MainWindow>) {
    use slint::winit_030::{winit, WinitWindowAccessor};

    let ui = ui.clone();
    Timer::single_shot(Duration::from_millis(100), move || {
        let Some(app) = ui.upgrade() else { return };
        app.window().with_winit_window(|win| {
            let Some(monitor) = win.current_monitor().or_else(|| win.primary_monitor()) else {
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
fn install_drag_drop(ui: &slint::Weak<MainWindow>, state: &'static DragDropState) {
    use slint::winit_030::{winit, EventResult, WinitWindowAccessor};

    let Some(app) = ui.upgrade() else { return };
    let window = app.window();
    let ui: slint::Weak<MainWindow> = ui.clone();
    window.on_winit_window_event(move |_slint_window, event| {
        match event {
            winit::event::WindowEvent::ModifiersChanged(mods) => {
                // Shift/Alt 拖文件行 = 拖出下载；普通拖动保留为网盘内移动。
                // 同一份修饰键状态也供列表行判定 Ctrl/Shift 多选（Slint 的点击事件不带修饰键）。
                use winit::keyboard::ModifiersState;
                let s = mods.state();
                let armed = s.contains(ModifiersState::SHIFT) || s.contains(ModifiersState::ALT);
                if let Some(app) = ui.upgrade() {
                    app.global::<crate::app::DragOutApi>()
                        .set_mod_pressed(armed);
                    let keys = app.global::<crate::app::ModifiersApi>();
                    keys.set_ctrl(s.contains(ModifiersState::CONTROL));
                    keys.set_shift(s.contains(ModifiersState::SHIFT));
                    keys.set_alt(s.contains(ModifiersState::ALT));
                }
            }
            winit::event::WindowEvent::HoveredFile(_) => {
                state.hovering.store(true, Ordering::SeqCst);
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
                // 同一批文件会在几十毫秒内陆续到达：每次调度都带批次号，
                // 只有批次号仍然匹配的定时器才消费路径，避免重复上传。
                let batch_id = {
                    let mut b = state.batch.lock().unwrap();
                    *b += 1;
                    *b
                };
                let ui = ui.clone();
                slint::Timer::single_shot(Duration::from_millis(80), move || {
                    let current = *state.batch.lock().unwrap();
                    if current != batch_id {
                        return; // 已有更新的批次，交给它统一处理
                    }
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

    // 后台任务（会话保活）
    controller.spawn_background();

    // 传输进度节流刷新：后台只置脏标记，这里统一以 200ms 节奏推送到 UI
    let ticker = Timer::default();
    {
        let c = Arc::clone(&controller);
        ticker.start(
            TimerMode::Repeated,
            Duration::from_millis(TICK_MS),
            move || {
                c.tick();
            },
        );
    }

    // 跟随系统主题时定期检查系统深色偏好的变化
    let theme_timer = Timer::default();
    {
        let c = Arc::clone(&controller);
        theme_timer.start(
            TimerMode::Repeated,
            Duration::from_secs(THEME_POLL_SECS),
            move || c.poll_system_theme(),
        );
    }

    // 尝试用上次使用的账号恢复登录态
    controller.auto_login();

    // 将窗口居中显示
    center_window(&ui_handle);

    ui.run()?;
    // 定时器需要在 UI 事件循环存活期间保持存在
    drop(ticker);
    drop(theme_timer);
    Ok(())
}
