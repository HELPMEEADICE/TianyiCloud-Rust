//! 应用控制器：桥接 Slint UI 事件与 tianyi-core 异步操作
//!
//! 设计要点：
//! * 所有网络/文件操作都在 tokio 运行时执行，UI 线程只做属性写入；
//! * 目录加载、搜索、空间切换各有独立的 generation 计数，过期响应直接丢弃，
//!   避免“旧请求覆盖新结果”的竞态；
//! * 传输任务通过 `TransferControl` 做协作式暂停/继续/取消，暂停不等于取消；
//! * 进度回调只写任务表并置脏标记，由 UI 线程的定时器以 200ms 节流刷新界面，
//!   避免高频跨线程重绘。

use crate::app::{CrumbEntry, DirectLink, DragOutApi, FileEntry, MainWindow, Theme, TransferEntry};
use crate::backend::{invoke_ui, Backend};
use slint::{ComponentHandle, SharedString, Weak};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tianyi_core::api::{self, LoginNotifier};
use tianyi_core::client::TianyiClient;
use tianyi_core::config::{AppConfig, AppStore, ThemeMode};
use tianyi_core::file::{self, FileKind};
use tianyi_core::models::{AccountConfig, FileObject, TokenInfo};
use tianyi_core::task::{Task, TaskDirection, TaskManager, TaskStatus, TransferControl};
use tianyi_core::transfer::{self, DownloadOptions, DownloadOutcome, UploadOptions, UploadOutcome};

/// 根目录 id
const ROOT_FOLDER_ID: &str = "-11";
/// 面板刷新节流间隔（毫秒）
const TICK_MS: u64 = 200;
/// 单次刷新时最多展示的任务数
const MAX_TRANSFER_ROWS: usize = 200;
/// 登录页最多展示的已保存账号数
const MAX_SAVED_ACCOUNTS: usize = 4;

/// 登录通知器（向 UI 推送验证码 / 二维码状态）
///
/// 携带 generation：切换登录方式或刷新二维码后，旧会话的回调不会再写入 UI。
struct AppNotifier {
    ui: Weak<MainWindow>,
    /// 本次登录会话的序号
    gen: u64,
    /// 控制器的 login_gen，序号不一致说明会话已被刷新/取消
    live: Arc<AtomicU64>,
}

impl AppNotifier {
    fn is_current(&self) -> bool {
        self.live.load(Ordering::SeqCst) == self.gen
    }
}

impl LoginNotifier for AppNotifier {
    fn need_captcha(&self, image_base64: &str) {
        let rgba = decode_png_to_rgba(image_base64);
        let ui = self.ui.clone();
        invoke_ui(&ui, move |win| {
            win.set_show_captcha(true);
            win.set_captcha_image(rgba_to_slint_image(rgba));
        });
    }

    fn qr_code(&self, uuid: &str, text: &str) {
        if !self.is_current() {
            return;
        }
        let rgba = render_qr_rgba(uuid);
        let ui = self.ui.clone();
        let text = text.to_string();
        invoke_ui(&ui, move |win| {
            win.set_qr_image(rgba_to_slint_image(rgba));
            win.set_qr_status(SharedString::from(text));
            win.set_qr_state(SharedString::from("waiting"));
            win.set_login_loading(false);
        });
    }

    fn qr_status(&self, status: &str) {
        if !self.is_current() {
            return;
        }
        let ui = self.ui.clone();
        let status = status.to_string();
        let state = qr_state_of(&status);
        invoke_ui(&ui, move |win| {
            win.set_qr_status(SharedString::from(status));
            win.set_qr_state(SharedString::from(state));
        });
    }
}

/// 把天翼返回的中文扫码状态映射为 UI 状态键
fn qr_state_of(status: &str) -> &'static str {
    if status.contains("成功") {
        "success"
    } else if status.contains("已扫码") {
        "scanned"
    } else if status.contains("过期") {
        "expired"
    } else if status.contains("失败") || status.contains("错误") {
        "error"
    } else {
        "waiting"
    }
}

/// 渲染二维码为 RGBA 像素（线程安全）
fn render_qr_rgba(text: &str) -> image::RgbaImage {
    let code = match qrcode::QrCode::new(text.as_bytes()) {
        Ok(c) => c,
        Err(_) => return image::RgbaImage::new(1, 1),
    };
    code.render::<image::Rgba<u8>>()
        .min_dimensions(240, 240)
        .build()
}

/// 从 base64 PNG 解码为 RGBA 像素（线程安全）
fn decode_png_to_rgba(base64_png: &str) -> image::RgbaImage {
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(base64_png) {
        Ok(b) => b,
        Err(_) => return image::RgbaImage::new(1, 1),
    };
    match image::load_from_memory(&bytes) {
        Ok(i) => i.to_rgba8(),
        Err(_) => image::RgbaImage::new(1, 1),
    }
}

fn rgba_to_slint_image(img: image::RgbaImage) -> slint::Image {
    let width = img.width();
    let height = img.height();
    let raw = img.into_raw();
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(&raw);
    slint::Image::from_rgba8(buffer)
}

/// 应用控制器
pub struct Controller {
    ui: Weak<MainWindow>,
    backend: Arc<Backend>,
    store: AppStore,
    client: Mutex<Option<Arc<TianyiClient>>>,
    current_user: Mutex<String>,

    // ---------------- 目录状态 ----------------
    /// "personal" | "family"
    space: Mutex<String>,
    folder_id: Mutex<String>,
    /// 已进入的目录栈：(目录 id, 目录名)
    path_stack: Mutex<Vec<(String, String)>>,
    /// 当前目录内容（退出搜索时恢复）
    dir_rows: Mutex<Vec<FileObject>>,
    /// 当前实际显示的行（目录内容或搜索结果），供选择/操作解析
    visible_rows: Mutex<Vec<FileObject>>,
    /// 目录加载 / 搜索 / 空间切换的版本号
    dir_gen: AtomicU64,
    search_gen: AtomicU64,
    space_gen: AtomicU64,

    // ---------------- 选择 ----------------
    /// 多选：当前选中的文件 id（唯一事实来源；UI 只读 FileEntry.selected）
    selected_ids: Mutex<Vec<String>>,
    /// Shift 范围选择的锚点 id
    selection_anchor: Mutex<String>,
    /// 当前文件列表条目的缓存：选择变化时只更新 selected 标记，不重新构造文件对象
    entry_cache: Mutex<Vec<FileEntry>>,

    // ---------------- 排序 ----------------
    /// 0 = 名称，1 = 大小，2 = 修改时间
    sort_key: Mutex<i32>,
    sort_asc: Mutex<bool>,

    // ---------------- 确认对话框 ----------------
    /// 等待用户确认的动作（危险操作不直接执行）
    pending_confirm: Mutex<Option<PendingConfirm>>,

    // ---------------- 登录 ----------------
    login_gen: Arc<AtomicU64>,

    // ---------------- 传输 ----------------
    tasks: Arc<TaskManager>,
    /// 活跃任务的控制句柄（暂停/继续/取消都走这里）
    controls: Mutex<HashMap<u64, TransferControl>>,
    /// 进度脏标记：由后台线程置位，UI 定时器消费
    dirty: Arc<AtomicBool>,
    /// 进度节流：task_id -> (最后采样字节, 最后采样时间 ms)
    rate_samples: Arc<Mutex<HashMap<u64, (u64, u64)>>>,

    // ---------------- 直链 ----------------
    links: Mutex<Vec<DirectLink>>,

    // ---------------- 主题 ----------------
    theme_mode: Mutex<ThemeMode>,

    // ---------------- 提示条（Toast） ----------------
    /// 去重与代际状态：只在 UI 线程上读写，`Arc` 只为了跨闭包共享
    notice_slot: Arc<Mutex<NoticeSlot>>,

    // ---------------- 拖拽 ----------------
    drag_state: Mutex<Option<&'static crate::DragDropState>>,
    #[cfg(windows)]
    drag_out_busy: AtomicBool,
}

impl Controller {
    pub fn new(ui: Weak<MainWindow>, backend: Arc<Backend>) -> Arc<Self> {
        let store = AppStore::new(None).expect("init app store");
        let tasks = TaskManager::new(Some(store.root().join("tasks.json")));
        let theme_mode = ThemeMode::parse(
            &store
                .load_config()
                .unwrap_or_else(|_| AppConfig::default_config())
                .theme,
        );
        let c = Arc::new(Controller {
            ui,
            backend,
            store,
            client: Mutex::new(None),
            current_user: Mutex::new(String::new()),
            space: Mutex::new("personal".to_string()),
            folder_id: Mutex::new(ROOT_FOLDER_ID.to_string()),
            path_stack: Mutex::new(Vec::new()),
            dir_rows: Mutex::new(Vec::new()),
            visible_rows: Mutex::new(Vec::new()),
            dir_gen: AtomicU64::new(0),
            search_gen: AtomicU64::new(0),
            space_gen: AtomicU64::new(0),
            selected_ids: Mutex::new(Vec::new()),
            selection_anchor: Mutex::new(String::new()),
            entry_cache: Mutex::new(Vec::new()),
            sort_key: Mutex::new(0),
            sort_asc: Mutex::new(true),
            pending_confirm: Mutex::new(None),
            login_gen: Arc::new(AtomicU64::new(0)),
            tasks: Arc::new(tasks),
            controls: Mutex::new(HashMap::new()),
            dirty: Arc::new(AtomicBool::new(false)),
            rate_samples: Arc::new(Mutex::new(HashMap::new())),
            links: Mutex::new(Vec::new()),
            theme_mode: Mutex::new(theme_mode),
            notice_slot: Arc::new(Mutex::new(NoticeSlot::default())),
            drag_state: Mutex::new(None),
            #[cfg(windows)]
            drag_out_busy: AtomicBool::new(false),
        });
        Self::bind_ui(Arc::clone(&c));
        c.refresh_saved_accounts();
        c.initial_sync();
        c
    }

    /// 初始化静态 UI 状态：版本号、配置目录、字体、主题、任务列表
    fn initial_sync(self: &Arc<Self>) {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let config_dir = self.store.root().display().to_string();
        let font = platform_ui_font();
        let mono = platform_mono_font();
        self.apply_theme();
        let cfg = self
            .store
            .load_config()
            .unwrap_or_else(|_| AppConfig::default_config());
        let upload_thread = cfg.upload_thread.to_string();
        let download_thread = cfg.download_thread.to_string();
        let download_dir = cfg.download_dir.display().to_string();
        self.set_ui_state(move |ui| {
            ui.set_version_text(SharedString::from(version));
            ui.set_config_dir(SharedString::from(config_dir));
            ui.set_upload_thread(SharedString::from(upload_thread));
            ui.set_download_thread(SharedString::from(download_thread));
            ui.set_download_dir(SharedString::from(download_dir));
            let theme = ui.global::<Theme>();
            theme.set_font_family(SharedString::from(font.as_str()));
            theme.set_mono_font_family(SharedString::from(mono.as_str()));
        });
        self.refresh_transfers();
    }

    // ==================================================================
    // 绑定 UI 回调
    // ==================================================================
    fn bind_ui(this: Arc<Self>) {
        // ---------------- 登录 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_login_request(move |user, pass, captcha, save| {
                let c = c.clone();
                c.handle_login(user.as_str(), pass.as_str(), captcha.as_str(), save);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_qr_refresh(move || {
                let c = c.clone();
                c.handle_qr_login();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_switch_mode(move |qr| {
                let c = c.clone();
                c.handle_switch_mode(qr);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_quick_login(move |name| {
                let c = c.clone();
                c.handle_quick_login(name.as_str());
            });
        });

        // ---------------- 文件浏览 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_open_item(move |id, name, is_dir| {
                let c = c.clone();
                c.handle_open_item(id.as_str(), name.as_str(), is_dir);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_select_item(move |id, ctrl, shift| {
                let c = c.clone();
                c.handle_select_item(id.as_str(), ctrl, shift);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_select_all(move || {
                let c = c.clone();
                c.select_all_items();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_clear_selection(move || {
                let c = c.clone();
                c.set_selection(Vec::new());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_sort_changed(move |key| {
                let c = c.clone();
                c.sort_changed(key);
            });
        });

        // ---------------- 确认对话框 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_dialog_confirm(move || {
                let c = c.clone();
                c.dialog_confirm();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_dialog_cancel(move || {
                let c = c.clone();
                c.dialog_cancel();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_context_requested(move |id, name, is_dir, x, y| {
                let c = c.clone();
                c.handle_context(id.as_str(), name.as_str(), is_dir, x, y);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_refresh_files(move || {
                let c = c.clone();
                c.refresh_files();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_navigate_up(move || {
                let c = c.clone();
                c.navigate_up();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_navigate_crumb(move |idx| {
                let c = c.clone();
                c.navigate_crumb(idx.max(0) as usize);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_switch_space(move |space| {
                let c = c.clone();
                c.switch_space(space.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_upload_files(move || {
                let c = c.clone();
                c.upload_files();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_create_folder(move || {
                let c = c.clone();
                c.create_folder();
            });
        });

        // ---------------- 文件操作 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_download_selected(move || {
                let c = c.clone();
                c.download_selected();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_rename_selected(move || {
                let c = c.clone();
                c.rename_selected();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_rename_submitted(move |id, name| {
                let c = c.clone();
                c.rename_submitted(id.as_str(), name.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_rename_cancelled(move |id| {
                let c = c.clone();
                c.rename_cancelled(id.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_copy_selected(move || {
                let c = c.clone();
                c.copy_selected();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_move_selected_up(move || {
                let c = c.clone();
                c.move_selected_up();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_delete_selected(move || {
                let c = c.clone();
                c.delete_selected();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_link_selected(move || {
                let c = c.clone();
                c.link_selected();
            });
        });

        // ---------------- 搜索 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_search_changed(move |kw| {
                let c = c.clone();
                c.search_changed(kw.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_search_submitted(move |kw| {
                let c = c.clone();
                c.search_changed(kw.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_clear_search(move || {
                let c = c.clone();
                c.clear_search();
            });
        });

        // ---------------- 传输 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_pause_task(move |id| {
                let c = c.clone();
                c.pause_task(id.max(0) as u64);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_resume_task(move |id| {
                let c = c.clone();
                c.resume_task(id.max(0) as u64);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_cancel_task(move |id| {
                let c = c.clone();
                c.cancel_task(id.max(0) as u64);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_retry_task(move |id| {
                let c = c.clone();
                c.retry_task(id.max(0) as u64);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_remove_task(move |id| {
                let c = c.clone();
                c.remove_task(id.max(0) as u64);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_reveal_task(move |id| {
                let c = c.clone();
                c.reveal_task(id.max(0) as u64);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_clear_finished_tasks(move || {
                let c = c.clone();
                c.clear_finished_tasks();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_toggle_transfer_panel(move || {
                let c = c.clone();
                c.toggle_transfer_panel();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_transfer_filter_changed(move |_idx| {
                let c = c.clone();
                c.refresh_transfers();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_open_download_dir(move || {
                let c = c.clone();
                c.open_download_dir();
            });
        });

        // ---------------- 直链 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_copy_link(move |url| {
                let c = c.clone();
                c.copy_link(url.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_clear_links(move || {
                let c = c.clone();
                c.clear_links();
            });
        });

        // ---------------- 设置 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_theme_changed(move |idx| {
                let c = c.clone();
                c.set_theme_index(idx);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_reduce_motion_changed(move |on| {
                let c = c.clone();
                c.set_reduce_motion(on);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_choose_download_dir(move || {
                let c = c.clone();
                c.choose_download_dir();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_open_config_dir(move || {
                let c = c.clone();
                c.open_config_dir();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_copy_text(move |text| {
                let c = c.clone();
                let text = text.to_string();
                if text.trim().is_empty() {
                    return;
                }
                match copy_to_clipboard(&text) {
                    Ok(()) => c.notice("success", "已复制到剪贴板"),
                    Err(e) => c.notice("error", format!("复制失败：{e}")),
                }
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_logout(move || {
                let c = c.clone();
                c.logout();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_notice_closed(move || {
                let c = c.clone();
                c.dismiss_notice();
            });
        });

        // ---------------- 拖拽上传 ----------------
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_files_dropped(move |joined| {
                let c = c.clone();
                let joined = joined.to_string();
                let paths: Vec<PathBuf> = joined
                    .split('\n')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();
                c.handle_files_dropped(paths);
            });
        });

        // 网盘内拖动：构造拖拽载荷
        this.ui_set(move |ui| {
            ui.on_make_drag_data(move |id, name, _is_dir| {
                let mut dt = slint::private_unstable_api::re_exports::DataTransfer::default();
                dt.set_user_data(std::rc::Rc::new(MovedFile {
                    id: id.to_string(),
                    name: name.to_string(),
                }));
                dt
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_drop_can_accept(move |event, folder_id| {
                c.check_drop_target(&event, folder_id.as_str())
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_drop_requested(move |event, folder_id| {
                let c = c.clone();
                c.handle_drop_move(&event, folder_id.as_str());
                slint::language::DragAction::Move
            });
        });

        // 修饰键拖出下载
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            let api = ui.global::<DragOutApi>();
            api.on_drag_out(move |id, name| {
                let c = c.clone();
                c.handle_drag_out(id.as_str(), name.as_str());
            });
        });
    }

    // ==================================================================
    // 基础设施
    // ==================================================================
    fn ui_set<F: FnOnce(&MainWindow) + Send + 'static>(&self, f: F) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                f(&ui);
            }
        });
    }

    fn set_ui_state<F: FnOnce(&MainWindow) + Send + 'static>(&self, f: F) {
        let ui = self.ui.clone();
        invoke_ui(&ui, move |win| f(win));
    }

    fn client(&self) -> Option<Arc<TianyiClient>> {
        self.client.lock().unwrap().clone()
    }

    /// 统一的提示条（Toast）
    ///
    /// 到期自动消失；同一文案在去重窗口内重复出现时累加为 `×N` 而不是重开一段；
    /// 计时器带代际，只有最新一代能清空，避免旧计时器把新提示吞掉。
    fn notice(&self, kind: &str, text: impl Into<String>) {
        let text: String = text.into();
        let kind = kind.to_string();
        if kind == "error" {
            log::warn!("notice(error): {text}");
        }
        let slot = Arc::clone(&self.notice_slot);
        self.set_ui_state(move |ui| {
            // 计时器必须建在事件循环线程上，这个闭包正是运行在 UI 线程
            let weak = ui.as_weak();
            let (gen, count) = slot.lock().unwrap().push(&text, now_millis());
            ui.set_notice_text(SharedString::from(text.as_str()));
            ui.set_notice_kind(SharedString::from(kind.as_str()));
            ui.set_notice_count(count);
            let ttl = notice_ttl(kind.as_str());
            let slot = Arc::clone(&slot);
            slint::Timer::single_shot(ttl, move || {
                if !slot.lock().unwrap().expire(gen) {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_notice_text(SharedString::default());
                    ui.set_notice_count(1);
                }
            });
        });
    }

    /// 用户手动关闭提示：同时让在途的自动消失计时器失效
    fn dismiss_notice(&self) {
        self.notice_slot.lock().unwrap().dismiss();
        self.set_ui_state(|ui| {
            ui.set_notice_text(SharedString::default());
            ui.set_notice_count(1);
        });
    }

    fn set_status(&self, text: impl Into<String>) {
        let text: String = text.into();
        self.set_ui_state(move |ui| {
            ui.set_status_text(SharedString::from(text));
        });
    }

    // ==================================================================
    // 主题
    // ==================================================================
    /// 依据配置 + 系统偏好应用主题
    pub fn apply_theme(self: &Arc<Self>) {
        let cfg = self
            .store
            .load_config()
            .unwrap_or_else(|_| AppConfig::default_config());
        let mode = ThemeMode::parse(&cfg.theme);
        *self.theme_mode.lock().unwrap() = mode;
        let dark = match mode {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => tianyi_core::theme::system_prefers_dark(),
        };
        let reduce = cfg.reduce_motion;
        let index = match mode {
            ThemeMode::System => 0,
            ThemeMode::Light => 1,
            ThemeMode::Dark => 2,
        };
        self.set_ui_state(move |ui| {
            let theme = ui.global::<Theme>();
            theme.set_dark(dark);
            theme.set_reduce_motion(reduce);
            ui.set_theme_index(index);
            ui.set_reduce_motion(reduce);
        });
    }

    /// 跟随系统主题时的定时检查（由 UI 线程定时器调用）
    pub fn poll_system_theme(self: &Arc<Self>) {
        if *self.theme_mode.lock().unwrap() != ThemeMode::System {
            return;
        }
        let dark = tianyi_core::theme::system_prefers_dark();
        let ui = self.ui.clone();
        if let Some(win) = ui.upgrade() {
            if win.global::<Theme>().get_dark() != dark {
                win.global::<Theme>().set_dark(dark);
            }
        }
    }

    fn set_theme_index(self: &Arc<Self>, idx: i32) {
        let mode = match idx {
            1 => ThemeMode::Light,
            2 => ThemeMode::Dark,
            _ => ThemeMode::System,
        };
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.theme = mode.as_str().to_string();
            if let Err(e) = self.store.save_config(&cfg) {
                log::warn!("save theme: {e}");
            }
        }
        self.apply_theme();
    }

    fn set_reduce_motion(self: &Arc<Self>, on: bool) {
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.reduce_motion = on;
            if let Err(e) = self.store.save_config(&cfg) {
                log::warn!("save reduce_motion: {e}");
            }
        }
        self.apply_theme();
    }

    fn choose_download_dir(self: &Arc<Self>) {
        let current = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| PathBuf::from("."));
        let Some(dir) = rfd::FileDialog::new()
            .set_title("选择默认下载目录")
            .set_directory(&current)
            .pick_folder()
        else {
            return;
        };
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.download_dir = dir.clone();
            if let Err(e) = self.store.save_config(&cfg) {
                self.notice("error", format!("保存配置失败：{e}"));
                return;
            }
        }
        let text = dir.display().to_string();
        self.set_ui_state(move |ui| ui.set_download_dir(SharedString::from(text)));
        self.notice("success", "默认下载目录已更新");
    }

    fn open_config_dir(self: &Arc<Self>) {
        let dir = self.store.root();
        let _ = std::fs::create_dir_all(dir.as_path());
        if let Err(e) = file::open_with_system(dir.as_path()) {
            self.notice("error", format!("无法打开配置目录：{e}"));
        }
    }

    fn open_download_dir(self: &Arc<Self>) {
        let dir = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| PathBuf::from("."));
        let _ = std::fs::create_dir_all(dir.as_path());
        if let Err(e) = file::open_with_system(dir.as_path()) {
            self.notice("error", format!("无法打开下载目录：{e}"));
        }
    }

    fn toggle_transfer_panel(self: &Arc<Self>) {
        let ui = self.ui.clone();
        if let Some(win) = ui.upgrade() {
            let open = !win.get_transfer_panel_open();
            win.set_transfer_panel_open(open);
            if open {
                self.refresh_transfers();
            }
        }
    }

    // ==================================================================
    // 登录
    // ==================================================================
    fn handle_switch_mode(self: &Arc<Self>, qr: bool) {
        // 切换模式会终止正在进行的二维码轮询与密码登录结果回填
        let gen = self.login_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_ui_state(move |ui| {
            ui.set_qr_mode(qr);
            ui.set_login_error(SharedString::default());
            if qr {
                ui.set_qr_status(SharedString::from("正在获取二维码…"));
                ui.set_qr_state(SharedString::from("loading"));
                ui.set_login_loading(true);
            } else {
                ui.set_login_loading(false);
                ui.set_show_captcha(false);
            }
        });
        if qr {
            self.spawn_qr_login(gen);
        }
    }

    fn handle_qr_login(self: &Arc<Self>) {
        let gen = self.login_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_ui_state(|ui| {
            ui.set_login_error(SharedString::default());
            ui.set_qr_status(SharedString::from("正在获取二维码…"));
            ui.set_qr_state(SharedString::from("loading"));
            ui.set_login_loading(true);
        });
        self.spawn_qr_login(gen);
    }

    fn spawn_qr_login(self: &Arc<Self>, gen: u64) {
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let live = Arc::clone(&self.login_gen);
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            let notifier = AppNotifier { ui, gen, live };
            let http = match reqwest::Client::builder().cookie_store(true).build() {
                Ok(c) => c,
                Err(e) => {
                    this.login_failed(gen, format!("初始化网络失败：{e}"));
                    return;
                }
            };
            let dummy = TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
            match api::login_by_qrcode(&dummy, &http, &notifier).await {
                Ok(param) => match api::poll_qr_login(&http, &param, &notifier).await {
                    Ok(session) => {
                        if this.login_gen.load(Ordering::SeqCst) != gen {
                            return; // 已被刷新/切换模式取代
                        }
                        let token = api::token_from_session(&session);
                        let username = session.login_name.clone();
                        this.on_login_success(username, token, true);
                    }
                    Err(e) => this.login_failed(gen, e.to_string()),
                },
                Err(e) => this.login_failed(gen, e.to_string()),
            }
        });
    }

    /// 使用已保存账号快速登录
    fn handle_quick_login(self: &Arc<Self>, username: &str) {
        if username.is_empty() {
            return;
        }
        let accounts = self.store.load_accounts().unwrap_or_default();
        let Some(acc) = accounts.find(username).cloned() else {
            let msg = format!("未找到账号 {username} 的登录信息，请手动登录");
            self.set_ui_state(move |ui| {
                ui.set_login_error(SharedString::from(msg));
            });
            return;
        };
        if acc.token.access_token.is_empty() && acc.token.session_key.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_login_error(SharedString::from("该账号没有有效登录态，请重新登录"));
            });
            return;
        }
        self.login_gen.fetch_add(1, Ordering::SeqCst);
        self.set_ui_state(|ui| {
            ui.set_login_loading(true);
            ui.set_login_error(SharedString::default());
        });
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let username = username.to_string();
        backend.spawn(move || async move {
            let client = Arc::new(TianyiClient::new(
                this.store.load_config().unwrap_or_default(),
                acc,
            ));
            *this.client.lock().unwrap() = Some(client.clone());
            if let Err(e) = client.refresh_session().await {
                log::warn!("quick login refresh session: {e}");
            }
            this.on_login_success(username, TokenInfo::default(), false);
        });
    }

    /// 启动时尝试恢复上次账号
    pub fn auto_login(self: &Arc<Self>) {
        let last = self.store.load_config().unwrap_or_default().last_account;
        if last.is_empty() {
            return;
        }
        self.set_status(format!("正在恢复账号 {last} 的登录状态…"));
        self.handle_quick_login(&last);
    }

    fn refresh_saved_accounts(&self) {
        let names: Vec<SharedString> = self
            .store
            .load_accounts()
            .unwrap_or_default()
            .accounts
            .iter()
            .filter(|a| !a.username.is_empty())
            .take(MAX_SAVED_ACCOUNTS)
            .map(|a| SharedString::from(a.username.clone()))
            .collect();
        self.set_ui_state(move |ui| {
            ui.set_saved_accounts(slint::ModelRc::new(slint::VecModel::from(names)));
        });
    }

    fn login_failed(&self, gen: u64, msg: String) {
        if self.login_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        let state = if msg.contains("过期") {
            "expired"
        } else {
            "error"
        };
        let friendly = if msg.contains("NEED_CAPTCHA") {
            "请输入图片验证码后重试".to_string()
        } else {
            msg
        };
        self.set_ui_state(move |ui| {
            ui.set_login_loading(false);
            ui.set_login_error(SharedString::from(friendly.clone()));
            if ui.get_qr_mode() {
                ui.set_qr_status(SharedString::from(friendly.clone()));
                ui.set_qr_state(SharedString::from(state));
            }
        });
    }

    fn handle_login(self: &Arc<Self>, username: &str, password: &str, captcha: &str, save: bool) {
        if username.trim().is_empty() || password.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_login_error(SharedString::from("请输入账号和密码"));
            });
            return;
        }
        self.login_gen.fetch_add(1, Ordering::SeqCst);
        let gen = self.login_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_ui_state(|ui| {
            ui.set_login_loading(true);
            ui.set_login_error(SharedString::default());
        });

        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let notifier = AppNotifier {
            ui,
            gen,
            live: Arc::clone(&self.login_gen),
        };
        let username = username.trim().to_string();
        let password = password.to_string();
        let captcha = captcha.to_string();

        backend.spawn(move || async move {
            let http = match reqwest::Client::builder().cookie_store(true).build() {
                Ok(c) => c,
                Err(e) => {
                    this.login_failed(gen, format!("初始化网络失败：{e}"));
                    return;
                }
            };
            let client = TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
            let cap = if captcha.is_empty() {
                None
            } else {
                Some(captcha.as_str())
            };
            match api::login_by_password(&client, &http, &username, &password, cap, &notifier).await
            {
                Ok(session) => {
                    // 用户可能已经切换到扫码或退出，过期结果直接丢弃
                    if this.login_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let token = api::token_from_session(&session);
                    let uname = if session.login_name.is_empty() {
                        username
                    } else {
                        session.login_name
                    };
                    this.on_login_success(uname, token, save);
                }
                Err(e) => this.login_failed(gen, e.to_string()),
            }
        });
    }

    /// 登录成功后的统一收尾
    fn on_login_success(self: &Arc<Self>, username: String, token: TokenInfo, persist: bool) {
        let mut accounts = self.store.load_accounts().unwrap_or_default();
        let mut acc = accounts
            .find(&username)
            .cloned()
            .unwrap_or_else(|| AccountConfig {
                username: username.clone(),
                ..Default::default()
            });
        if !token.access_token.is_empty() || !token.session_key.is_empty() {
            acc.token = token;
            acc.login_type = "password".to_string();
        }
        if persist {
            accounts.upsert(acc.clone());
            if let Err(e) = self.store.save_accounts(&accounts) {
                log::warn!("save accounts: {e}");
            }
            if let Ok(mut cfg) = self.store.load_config() {
                cfg.last_account = username.clone();
                let _ = self.store.save_config(&cfg);
            }
        }

        *self.current_user.lock().unwrap() = username.clone();
        *self.space.lock().unwrap() = "personal".to_string();
        *self.folder_id.lock().unwrap() = ROOT_FOLDER_ID.to_string();
        self.path_stack.lock().unwrap().clear();
        self.dir_rows.lock().unwrap().clear();
        self.visible_rows.lock().unwrap().clear();

        let client = Arc::new(TianyiClient::new(
            self.store.load_config().unwrap_or_default(),
            acc,
        ));
        *self.client.lock().unwrap() = Some(client);

        // 已登录：把仍在进行中的其它登录尝试全部作废
        self.login_gen.fetch_add(1, Ordering::SeqCst);

        self.refresh_saved_accounts();
        let initial = account_initial(&username);
        let this = Arc::clone(self);
        self.set_ui_state(move |ui| {
            ui.set_login_loading(false);
            ui.set_logged_in(true);
            ui.set_account_name(SharedString::from(username.clone()));
            ui.set_account_initial(SharedString::from(initial));
            ui.set_login_error(SharedString::default());
            ui.set_current_page(0);
            ui.set_search_keyword(SharedString::default());
            ui.set_notice_text(SharedString::default());
        });
        this.load_dir(false);
        this.load_capacity();
        this.refresh_transfers();
    }

    fn logout(self: &Arc<Self>) {
        // 终止进行中的登录与传输
        self.login_gen.fetch_add(1, Ordering::SeqCst);
        self.dir_gen.fetch_add(1, Ordering::SeqCst);
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        self.space_gen.fetch_add(1, Ordering::SeqCst);
        {
            let controls = self.controls.lock().unwrap();
            for ctl in controls.values() {
                ctl.cancel();
            }
        }
        *self.client.lock().unwrap() = None;
        *self.current_user.lock().unwrap() = String::new();
        *self.space.lock().unwrap() = "personal".to_string();
        *self.folder_id.lock().unwrap() = ROOT_FOLDER_ID.to_string();
        self.path_stack.lock().unwrap().clear();
        self.dir_rows.lock().unwrap().clear();
        self.visible_rows.lock().unwrap().clear();
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.last_account = String::new();
            let _ = self.store.save_config(&cfg);
        }
        self.refresh_saved_accounts();
        self.set_ui_state(|ui| {
            ui.set_logged_in(false);
            ui.set_current_page(0);
            ui.set_account_name(SharedString::default());
            ui.set_account_initial(SharedString::from("?"));
            ui.set_file_list(slint::ModelRc::default());
            ui.set_crumbs(slint::ModelRc::default());
            ui.set_searching(false);
            ui.set_search_keyword(SharedString::default());
            ui.set_renaming_id(SharedString::default());
            ui.set_list_state(SharedString::from("idle"));
            ui.set_total_space(SharedString::default());
            ui.set_used_space(SharedString::default());
            ui.set_capacity_ratio(0.0);
            ui.set_login_error(SharedString::default());
            ui.set_qr_status(SharedString::default());
            ui.set_qr_state(SharedString::from("loading"));
            ui.set_qr_image(slint::Image::default());
            ui.set_show_captcha(false);
            ui.set_login_loading(false);
            ui.set_file_error(SharedString::default());
            ui.set_notice_text(SharedString::from("已退出登录"));
            ui.set_notice_kind(SharedString::from("info"));
            ui.set_status_text(SharedString::from("已退出登录"));
            ui.set_transfer_panel_open(false);
        });
    }

    // ==================================================================
    // 目录浏览
    // ==================================================================
    fn space_label(&self) -> &'static str {
        if self.space.lock().unwrap().as_str() == "family" {
            "家庭云"
        } else {
            "个人云"
        }
    }

    /// 构建面包屑：根 + 已进入的目录栈
    fn crumbs(&self) -> Vec<CrumbEntry> {
        let mut out = vec![CrumbEntry {
            id: ROOT_FOLDER_ID.into(),
            name: self.space_label().into(),
        }];
        for (id, name) in self.path_stack.lock().unwrap().iter() {
            out.push(CrumbEntry {
                id: id.clone().into(),
                name: name.clone().into(),
            });
        }
        out
    }

    fn push_crumbs(&self) {
        let crumbs = self.crumbs();
        self.set_ui_state(move |ui| {
            ui.set_crumbs(slint::ModelRc::new(slint::VecModel::from(crumbs)));
        });
    }

    /// 当前应当显示的行：处于搜索态时显示搜索结果，否则显示目录内容。
    ///
    /// 抽成纯函数，保证“清空搜索后恢复目录内容”这一行为有唯一实现。
    fn rows_for_display(dir: &[FileObject], search: Option<&[FileObject]>) -> Vec<FileObject> {
        match search {
            Some(hits) => hits.to_vec(),
            None => dir.to_vec(),
        }
    }

    /// 把行数据推送到 UI，并按当前选择集标记 selected
    fn push_rows(&self, rows: &[FileObject]) {
        let selected: HashSet<String> = self.selected_ids.lock().unwrap().iter().cloned().collect();
        let entries = files_to_entries(rows, &selected);
        *self.entry_cache.lock().unwrap() = entries.clone();
        let count = selected.len() as i32;
        self.set_ui_state(move |ui| {
            ui.set_file_list(entries_model(entries));
            ui.set_selected_count(count);
        });
    }

    /// 更新选择集
    ///
    /// 这是选择状态的唯一事实来源：UI 侧只读 FileEntry.selected 与 selected-count，
    /// 不允许自己维护一份"看起来选中了"的状态，避免出现界面多选、操作只作用于一个文件。
    fn set_selection(&self, ids: Vec<String>) {
        *self.selected_ids.lock().unwrap() = ids.clone();
        let set: HashSet<String> = ids.iter().cloned().collect();
        let mut cache = self.entry_cache.lock().unwrap();
        for e in cache.iter_mut() {
            e.selected = set.contains(e.id.as_str());
        }
        let entries = cache.clone();
        drop(cache);
        let count = ids.len() as i32;
        self.set_ui_state(move |ui| {
            ui.set_file_list(entries_model(entries));
            ui.set_selected_count(count);
        });
    }

    /// 丢弃已不在列表里的选中项（不会触发额外的 UI 更新）
    fn retain_selection(&self, rows: &[FileObject]) {
        let live: HashSet<&str> = rows.iter().map(|f| f.id.as_str()).collect();
        self.selected_ids
            .lock()
            .unwrap()
            .retain(|id| live.contains(id.as_str()));
    }

    fn sort_key_pair(&self) -> (i32, bool) {
        (
            *self.sort_key.lock().unwrap(),
            *self.sort_asc.lock().unwrap(),
        )
    }

    fn sorted_rows(&self, rows: &mut [FileObject]) {
        let (key, asc) = self.sort_key_pair();
        sort_files(rows, key, asc);
    }

    fn push_sort_state(&self) {
        let (key, asc) = self.sort_key_pair();
        self.set_ui_state(move |ui| {
            ui.set_sort_key(key);
            ui.set_sort_asc(asc);
        });
    }

    /// 列表状态与状态栏文案一起更新，避免"加载中"标志和文案不同步
    fn set_list_state(&self, state: &str, status: &str) {
        let state = state.to_string();
        let status = status.to_string();
        self.set_ui_state(move |ui| {
            ui.set_list_state(SharedString::from(state));
            ui.set_status_text(SharedString::from(status));
        });
    }

    fn set_file_error(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.set_ui_state(move |ui| {
            ui.set_file_error(SharedString::from(msg));
        });
    }

    /// 切换排序：同一列再次点击变为反向
    fn sort_changed(self: &Arc<Self>, key: i32) {
        let key = key.clamp(0, 2);
        {
            let mut cur = self.sort_key.lock().unwrap();
            let mut asc = self.sort_asc.lock().unwrap();
            if *cur == key {
                *asc = !*asc;
            } else {
                *cur = key;
                *asc = true;
            }
        }
        let mut rows = self.visible_rows.lock().unwrap().clone();
        self.sorted_rows(&mut rows);
        *self.visible_rows.lock().unwrap() = rows.clone();
        {
            let mut dir = self.dir_rows.lock().unwrap();
            self.sorted_rows(&mut dir);
        }
        let (key, asc) = self.sort_key_pair();
        self.push_sort_state();
        self.push_rows(&rows);
        self.set_status(format!(
            "已按{} {}排序",
            sort_label(key),
            if asc { "升序" } else { "降序" }
        ));
    }

    /// 加载当前目录（F5 / 刷新按钮 / 操作后刷新）
    ///
    /// 同一目录的刷新保留选中项与已有列表内容，只给非侵入式刷新指示；
    /// 目录切换由 `load_dir(false)` 走另一条路径。
    pub fn refresh_files(self: &Arc<Self>) {
        self.load_dir(true)
    }

    /// 重新加载当前目录
    ///
    /// keep_selection=true → 视为刷新：保留选中项与列表内容
    /// keep_selection=false → 视为目录切换：清空选择，重新进入加载态
    fn load_dir(self: &Arc<Self>, keep_selection: bool) {
        let gen = self.dir_gen.fetch_add(1, Ordering::SeqCst) + 1;
        // 任何目录加载都会终止搜索结果
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        if !keep_selection {
            self.set_selection(Vec::new());
        }
        let Some(client) = self.client() else {
            self.set_list_state("error", "未登录");
            self.set_file_error("未登录");
            return;
        };
        let had_rows = !self.visible_rows.lock().unwrap().is_empty();
        let silent = keep_selection && had_rows;
        let folder_id = self.folder_id.lock().unwrap().clone();
        let this = Arc::clone(self);
        let backend = self.backend.clone();

        self.push_crumbs();
        self.push_sort_state();
        self.set_ui_state(move |ui| {
            ui.set_searching(false);
            ui.set_renaming_id(SharedString::default());
            ui.set_file_error(SharedString::default());
        });
        self.set_list_state(
            if silent { "refreshing" } else { "loading" },
            if silent {
                "正在刷新目录…"
            } else {
                "正在加载目录…"
            },
        );

        backend.spawn(move || async move {
            match client.list_files(&folder_id).await {
                Ok(mut files) => {
                    if this.dir_gen.load(Ordering::SeqCst) != gen {
                        return; // 已被更新的请求取代
                    }
                    this.sorted_rows(&mut files);
                    let count = files.len();
                    *this.dir_rows.lock().unwrap() = files.clone();
                    *this.visible_rows.lock().unwrap() = files.clone();
                    // 目录内容变了：丢掉已不存在的选中项，保证后续操作对象始终有效
                    this.retain_selection(&files);
                    this.push_rows(&files);
                    this.set_list_state(
                        if count == 0 { "empty" } else { "ready" },
                        &format!("{count} 个项目"),
                    );
                }
                Err(e) => {
                    if this.dir_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let msg = format!("加载目录失败：{e}");
                    this.set_file_error(msg.clone());
                    this.set_list_state("error", &msg);
                }
            }
        });
    }

    fn load_capacity(self: &Arc<Self>) {
        let gen = self.space_gen.load(Ordering::SeqCst);
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let Ok(cap) = client.get_capacity().await else {
                return;
            };
            if this.space_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            let (total, used) = if client.is_family() {
                (
                    cap.family_capacity_info.total_size,
                    cap.family_capacity_info.used_size,
                )
            } else {
                (
                    cap.cloud_capacity_info.total_size,
                    cap.cloud_capacity_info.used_size,
                )
            };
            let total_s = file::format_size(total.max(0) as u64);
            let used_s = file::format_size(used.max(0) as u64);
            let ratio = if total > 0 {
                (used.max(0) as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            this.set_ui_state(move |ui| {
                ui.set_total_space(SharedString::from(total_s));
                ui.set_used_space(SharedString::from(used_s));
                ui.set_capacity_ratio(ratio);
            });
        });
    }

    fn navigate_crumb(self: &Arc<Self>, index: usize) {
        // index 0 = 根目录
        let mut stack = self.path_stack.lock().unwrap();
        if index == 0 {
            stack.clear();
            *self.folder_id.lock().unwrap() = ROOT_FOLDER_ID.to_string();
        } else if index <= stack.len() {
            stack.truncate(index);
            let (id, _) = stack.last().cloned().unwrap();
            *self.folder_id.lock().unwrap() = id;
        } else {
            return;
        }
        drop(stack);
        self.load_dir(false);
    }

    fn navigate_up(self: &Arc<Self>) {
        let mut stack = self.path_stack.lock().unwrap();
        if stack.pop().is_some() {
            let next = stack
                .last()
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| ROOT_FOLDER_ID.to_string());
            *self.folder_id.lock().unwrap() = next;
            drop(stack);
            self.load_dir(false);
        }
    }

    fn switch_space(self: &Arc<Self>, space: &str) {
        let space = if space == "family" {
            "family"
        } else {
            "personal"
        }
        .to_string();
        *self.space.lock().unwrap() = space.clone();
        let gen = self.space_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.dir_gen.fetch_add(1, Ordering::SeqCst);
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        *self.folder_id.lock().unwrap() = ROOT_FOLDER_ID.to_string();
        self.path_stack.lock().unwrap().clear();
        self.dir_rows.lock().unwrap().clear();
        self.visible_rows.lock().unwrap().clear();

        let Some(client) = self.client() else { return };
        client.set_account(|acc| acc.space_type = space.clone());

        self.set_selection(Vec::new());
        self.set_ui_state(|ui| {
            ui.set_file_list(slint::ModelRc::default());
            ui.set_searching(false);
            ui.set_search_keyword(SharedString::default());
            ui.set_renaming_id(SharedString::default());
            ui.set_file_error(SharedString::default());
        });
        self.set_list_state("loading", "正在切换云空间…");
        self.push_crumbs();

        if space == "personal" {
            self.load_dir(false);
            self.load_capacity();
            return;
        }

        // 家庭云需要先拿到 familyId
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.ensure_family_id().await {
                Ok(_) => {
                    if this.space_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    this.load_dir(false);
                    this.load_capacity();
                }
                Err(e) => {
                    if this.space_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    // 回退个人云，并如实告知失败原因
                    if let Some(c) = this.client() {
                        c.set_account(|acc| acc.space_type = "personal".to_string());
                    }
                    *this.space.lock().unwrap() = "personal".to_string();
                    this.set_ui_state(|ui| {
                        ui.set_current_page(0);
                    });
                    this.notice("error", format!("家庭云不可用，已返回个人云：{e}"));
                    this.load_dir(false);
                    this.load_capacity();
                }
            }
        });
    }

    fn handle_open_item(self: &Arc<Self>, id: &str, name: &str, is_dir: bool) {
        if !is_dir {
            self.preview_file(id, name);
            return;
        }
        let current = self.folder_id.lock().unwrap().clone();
        self.path_stack
            .lock()
            .unwrap()
            .push((current, name.to_string()));
        *self.folder_id.lock().unwrap() = id.to_string();
        self.load_dir(false);
    }

    /// 单击选择：普通点击=单选，Ctrl=切换，Shift=从锚点范围选择
    fn handle_select_item(self: &Arc<Self>, id: &str, ctrl: bool, shift: bool) {
        let rows = self.visible_rows.lock().unwrap().clone();
        if rows.is_empty() {
            return;
        }
        let anchor = self.selection_anchor.lock().unwrap().clone();
        let mut ids = self.selected_ids.lock().unwrap().clone();

        if shift && !anchor.is_empty() && !ctrl {
            // 范围选择：用锚点到目标之间的连续区间替换当前选择
            if let Some(range) = range_selection(&rows, &anchor, id) {
                self.set_selection(range);
                self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
                return;
            }
            // 锚点已被刷新掉：退化为单选
        }

        if ctrl {
            if let Some(pos) = ids.iter().position(|x| x == id) {
                ids.remove(pos);
            } else {
                ids.push(id.to_string());
            }
            *self.selection_anchor.lock().unwrap() = id.to_string();
            self.set_selection(ids);
            self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
            return;
        }

        *self.selection_anchor.lock().unwrap() = id.to_string();
        self.set_selection(vec![id.to_string()]);
        self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
    }

    /// 全选当前目录 / 搜索结果
    fn select_all_items(self: &Arc<Self>) {
        let ids: Vec<String> = self
            .visible_rows
            .lock()
            .unwrap()
            .iter()
            .map(|f| f.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        self.set_selection(ids);
        self.set_status(format!("已全选 {count} 项"));
    }

    fn handle_context(self: &Arc<Self>, id: &str, name: &str, is_dir: bool, x: f32, y: f32) {
        // 右键的行若不在当前选择里，则先把选择切到这一行，
        // 保证菜单里的操作对象与用户看到的选中项一致。
        let in_selection = self.selected_ids.lock().unwrap().iter().any(|s| s == id);
        if !in_selection {
            *self.selection_anchor.lock().unwrap() = id.to_string();
            self.set_selection(vec![id.to_string()]);
        }
        let id_s = id.to_string();
        let name_s = name.to_string();
        self.set_ui_state(move |ui| {
            ui.set_renaming_id(SharedString::default());
            ui.set_context_open(true);
            ui.set_context_x(x);
            ui.set_context_y(y);
            ui.set_context_item_id(SharedString::from(id_s));
            ui.set_context_item_name(SharedString::from(name_s));
            ui.set_context_is_dir(is_dir);
        });
    }

    /// 当前操作的目标文件集合（全部来自选择模型的唯一事实来源）
    fn targets(&self) -> Vec<FileObject> {
        let ids = self.selected_ids.lock().unwrap().clone();
        if ids.is_empty() {
            return Vec::new();
        }
        let set: HashSet<String> = ids.into_iter().collect();
        self.visible_rows
            .lock()
            .unwrap()
            .iter()
            .filter(|f| set.contains(&f.id))
            .cloned()
            .collect()
    }

    // ==================================================================
    // 文件操作
    // ==================================================================
    fn download_selected(self: &Arc<Self>) {
        let targets: Vec<FileObject> = self.targets().into_iter().filter(|f| !f.is_dir).collect();
        if targets.is_empty() {
            self.notice("info", "请先选择要下载的文件（文件夹请进入后再选择文件）");
            return;
        }
        let default_dir = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| PathBuf::from("."));
        if targets.len() == 1 {
            let f = &targets[0];
            let Some(dest) = rfd::FileDialog::new()
                .set_title("选择下载位置")
                .set_directory(&default_dir)
                .set_file_name(&f.name)
                .save_file()
            else {
                return;
            };
            self.enqueue_download(f.clone(), dest);
            return;
        }
        // 多选：选一个目录，按文件名保存
        let Some(dir) = rfd::FileDialog::new()
            .set_title(format!("选择下载目录（{} 个文件）", targets.len()))
            .set_directory(&default_dir)
            .pick_folder()
        else {
            return;
        };
        let count = targets.len();
        for f in targets {
            let dest = dir.join(sanitize_file_name(&f.name));
            self.enqueue_download(f, dest);
        }
        self.notice("info", format!("已加入 {count} 个下载任务"));
    }

    fn enqueue_download(self: &Arc<Self>, file: FileObject, dest: PathBuf) {
        let task_id = self.tasks.next_id();
        let size = file.size.max(0) as u64;
        let task = Task {
            id: task_id,
            direction: TaskDirection::Download,
            file_name: file.name.clone(),
            file_size: size,
            local_path: dest,
            remote_folder_id: file.id.clone(),
            status: TaskStatus::Running,
            progress: 0.0,
            bytes_done: 0,
            speed: 0,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: now_secs(),
        };
        self.tasks.add(task);
        self.set_ui_state(|ui| ui.set_transfer_panel_open(true));
        self.spawn_transfer(task_id);
        self.refresh_transfers();
    }

    fn rename_selected(self: &Arc<Self>) {
        let targets = self.targets();
        if targets.len() != 1 {
            self.notice("info", "重命名只支持单个文件");
            return;
        }
        let id = targets[0].id.clone();
        self.set_ui_state(move |ui| {
            ui.set_renaming_id(SharedString::from(id));
            ui.set_status_text(SharedString::from("输入新名称后按回车保存"));
        });
    }

    fn rename_submitted(self: &Arc<Self>, id: &str, new_name: &str) {
        let new_name = new_name.trim().to_string();
        let Some(file) = self.find_visible(id) else {
            self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
            return;
        };
        if new_name.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_renaming_id(SharedString::default());
            });
            self.notice("error", "名称不能为空");
            return;
        }
        if new_name == file.name {
            self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
            return;
        }
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.rename(&file, &new_name).await {
                Ok(_) => {
                    this.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
                    this.notice("success", format!("已重命名为 {new_name}"));
                    this.refresh_files();
                }
                Err(e) => {
                    this.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
                    this.notice("error", format!("重命名失败：{e}"));
                }
            }
        });
    }

    /// Esc 取消重命名：不碰后端，只把行从输入态退回展示态
    ///
    /// 必须与 rename_submitted 走同一条"清空 renaming-id"的路径——
    /// 行内 TouchArea 在 renaming 时被禁用，不清空就再也点不回来。
    fn rename_cancelled(&self, id: &str) {
        log::debug!("rename cancelled: {id}");
        let (renaming_id, status) = rename_cancelled_state();
        self.set_ui_state(move |ui| {
            ui.set_renaming_id(renaming_id);
            ui.set_status_text(SharedString::from(status));
        });
    }

    fn copy_selected(self: &Arc<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let dst = self.folder_id.lock().unwrap().clone();
        let Some(client) = self.client() else { return };
        let count = targets.len();
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let mut failed = Vec::new();
            for f in targets {
                if let Err(e) = client.copy_to(&f, &dst).await {
                    failed.push(format!("{}: {e}", f.name));
                }
            }
            if failed.is_empty() {
                this.notice("success", format!("已复制 {count} 项到当前目录"));
                this.refresh_files();
            } else {
                this.notice("error", format!("部分复制失败：{}", failed.join("；")));
                this.refresh_files();
            }
        });
    }

    fn move_selected_up(self: &Arc<Self>) {
        let parent_id = {
            let stack = self.path_stack.lock().unwrap();
            if stack.len() < 2 {
                // 栈中只有一项时，上级就是根目录
                if stack.is_empty() {
                    return;
                }
                ROOT_FOLDER_ID.to_string()
            } else {
                stack[stack.len() - 2].0.clone()
            }
        };
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let Some(client) = self.client() else { return };
        let count = targets.len();
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let mut failed = Vec::new();
            for f in targets {
                if let Err(e) = client.move_to(&f, &parent_id).await {
                    failed.push(format!("{}: {e}", f.name));
                }
            }
            if failed.is_empty() {
                this.notice("success", format!("已移动 {count} 项到上级目录"));
            } else {
                this.notice("error", format!("部分移动失败：{}", failed.join("；")));
            }
            this.refresh_files();
        });
    }

    /// 删除：先弹出应用内确认对话框（同步的 rfd::MessageDialog 会阻塞 UI 线程），
    /// 用户确认后才真正执行，取消则完全不动数据。
    fn delete_selected(self: &Arc<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            self.notice("info", "请先选择要删除的项目");
            return;
        }
        let message = if targets.len() == 1 {
            let t = &targets[0];
            if t.is_dir {
                format!(
                    "确定要删除文件夹「{}」吗？其中的内容会一并删除，且无法恢复。",
                    t.name
                )
            } else {
                format!("确定要删除「{}」吗？此操作无法恢复。", t.name)
            }
        } else {
            format!(
                "确定要删除选中的 {} 个项目吗？此操作无法恢复。",
                targets.len()
            )
        };
        let title = if targets.len() == 1 {
            "确认删除".to_string()
        } else {
            format!("确认删除 {} 个项目", targets.len())
        };
        *self.pending_confirm.lock().unwrap() = Some(PendingConfirm::DeleteFiles(targets));
        self.set_ui_state(move |ui| {
            ui.set_dialog_title(SharedString::from(title));
            ui.set_dialog_message(SharedString::from(message));
            ui.set_dialog_confirm_text(SharedString::from("删除"));
            ui.set_dialog_danger(true);
            ui.set_dialog_open(true);
        });
    }

    fn dialog_cancel(self: &Arc<Self>) {
        *self.pending_confirm.lock().unwrap() = None;
        self.set_ui_state(|ui| ui.set_dialog_open(false));
    }

    fn dialog_confirm(self: &Arc<Self>) {
        let action = self.pending_confirm.lock().unwrap().take();
        self.set_ui_state(|ui| ui.set_dialog_open(false));
        match action {
            Some(PendingConfirm::DeleteFiles(targets)) => self.perform_delete(targets),
            None => {}
        }
    }

    fn perform_delete(self: &Arc<Self>, targets: Vec<FileObject>) {
        let Some(client) = self.client() else { return };
        let count = targets.len();
        let this = Arc::clone(self);
        self.set_status(format!("正在删除 {count} 项…"));
        self.backend.spawn(move || async move {
            let mut failed = Vec::new();
            for f in targets {
                if let Err(e) = client.delete(&f).await {
                    failed.push(format!("{}: {e}", f.name));
                }
            }
            if failed.is_empty() {
                this.notice("success", format!("已删除 {count} 项"));
            } else {
                this.notice("error", format!("部分删除失败：{}", failed.join("；")));
            }
            this.refresh_files();
        });
    }

    fn link_selected(self: &Arc<Self>) {
        let targets: Vec<FileObject> = self.targets().into_iter().filter(|f| !f.is_dir).collect();
        if targets.is_empty() {
            self.notice("info", "请选择文件（文件夹没有直链）");
            return;
        }
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let mut ok = 0usize;
            for f in targets {
                match client.get_download_url(&f.id).await {
                    Ok(url) => {
                        let copied = copy_to_clipboard(&url).is_ok();
                        this.push_link(f.name.clone(), url, copied);
                        ok += 1;
                    }
                    Err(e) => {
                        this.notice("error", format!("{} 获取直链失败：{e}", f.name));
                    }
                }
            }
            if ok > 0 {
                this.notice(
                    "success",
                    format!("已提取 {ok} 个下载直链，可在“下载直链”页复制"),
                );
                this.set_ui_state(|ui| ui.set_current_page(3));
            }
        });
    }

    fn push_link(self: &Arc<Self>, name: String, url: String, copied: bool) {
        {
            let mut links = self.links.lock().unwrap();
            if let Some(exist) = links.iter_mut().find(|l| l.name == name) {
                exist.url = url.clone().into();
            } else {
                links.insert(
                    0,
                    DirectLink {
                        name: name.clone().into(),
                        url: url.clone().into(),
                    },
                );
                if links.len() > 50 {
                    links.truncate(50);
                }
            }
        }
        if copied {
            self.set_status(format!("{name} 的直链已复制到剪贴板"));
        }
        self.push_links();
    }

    fn push_links(&self) {
        let links: Vec<DirectLink> = self.links.lock().unwrap().clone();
        self.set_ui_state(move |ui| {
            ui.set_links(slint::ModelRc::new(slint::VecModel::from(links)));
        });
    }

    fn copy_link(&self, url: &str) {
        match copy_to_clipboard(url) {
            Ok(()) => self.notice("success", "直链已复制到剪贴板"),
            Err(e) => self.notice("error", format!("复制失败：{e}")),
        }
    }

    fn clear_links(self: &Arc<Self>) {
        self.links.lock().unwrap().clear();
        self.push_links();
        self.notice("info", "已清空直链列表");
    }

    fn find_visible(&self, id: &str) -> Option<FileObject> {
        self.visible_rows
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.id == id)
            .cloned()
    }

    // ==================================================================
    // 搜索
    // ==================================================================
    fn search_changed(self: &Arc<Self>, keyword: &str) {
        let keyword = keyword.trim().to_string();
        if keyword.is_empty() {
            self.clear_search();
            return;
        }
        let gen = self.search_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.set_selection(Vec::new());
        self.set_ui_state(|ui| {
            ui.set_searching(true);
        });
        self.set_list_state("loading", "搜索中…");
        // 去抖已在 UI 侧完成；这里只做乱序丢弃
        self.backend.spawn(move || async move {
            if this.search_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            match client.search_files(&keyword).await {
                Ok(mut files) => {
                    if this.search_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    this.sorted_rows(&mut files);
                    let count = files.len();
                    *this.visible_rows.lock().unwrap() = files.clone();
                    this.retain_selection(&files);
                    this.push_rows(&files);
                    this.set_ui_state(|ui| {
                        ui.set_searching(true);
                    });
                    this.set_list_state(
                        if count == 0 { "empty-search" } else { "ready" },
                        &format!("“{keyword}”的搜索结果：{count} 项"),
                    );
                }
                Err(e) => {
                    if this.search_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let msg = format!("搜索失败：{e}");
                    this.set_file_error(msg.clone());
                    this.set_list_state("error", &msg);
                    this.notice("error", msg);
                }
            }
        });
    }

    fn clear_search(self: &Arc<Self>) {
        // 让任何在途搜索失效，并恢复当前目录内容
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        let dir = self.dir_rows.lock().unwrap().clone();
        let rows = Self::rows_for_display(&dir, None);
        *self.visible_rows.lock().unwrap() = rows.clone();
        self.retain_selection(&rows);
        let count = rows.len();
        self.push_rows(&rows);
        self.set_ui_state(|ui| {
            ui.set_searching(false);
            ui.set_search_keyword(SharedString::default());
        });
        self.set_list_state(
            if count == 0 { "empty" } else { "ready" },
            &format!("{count} 个项目"),
        );
    }

    // ==================================================================
    // 预览
    // ==================================================================
    fn preview_file(self: &Arc<Self>, id: &str, name: &str) {
        let kind = file::classify(name);
        let Some(client) = self.client() else { return };
        let id = id.to_string();
        let name = name.to_string();
        let this = Arc::clone(self);
        self.set_status(format!("正在打开 {name}…"));
        self.backend.spawn(move || async move {
            let url = match client.get_download_url(&id).await {
                Ok(u) => u,
                Err(e) => {
                    this.notice("error", format!("获取直链失败：{e}"));
                    return;
                }
            };
            // 音视频：交给系统播放器流式播放，不落盘
            if matches!(kind, FileKind::Video | FileKind::Audio) {
                match file::open_media_url_with_player(&name, &url) {
                    Ok(()) => this.set_status(format!("已用系统播放器打开 {name}")),
                    Err(e) => this.notice("error", format!("打开播放器失败：{e}")),
                }
                return;
            }
            // 其它类型：下载到临时目录后用系统默认程序打开
            match this.download_preview(&client, &url, &name).await {
                Ok(path) => {
                    if let Err(e) = file::open_with_system(&path) {
                        this.notice("error", format!("打开文件失败：{e}"));
                    } else {
                        this.set_status(format!("已打开 {name}"));
                    }
                }
                Err(e) => this.notice("error", format!("预览失败：{e}")),
            }
        });
    }

    async fn download_preview(
        &self,
        client: &TianyiClient,
        url: &str,
        name: &str,
    ) -> anyhow::Result<PathBuf> {
        let dir = std::env::temp_dir().join("tianyi-preview");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(sanitize_file_name(name));
        if path.exists() {
            return Ok(path);
        }
        let resp = client.http_client().get(url).send().await?;
        let bytes = resp.bytes().await?;
        tokio::fs::write(&path, bytes).await?;
        Ok(path)
    }

    // ==================================================================
    // 上传
    // ==================================================================
    fn upload_files(self: &Arc<Self>) {
        if self.client().is_none() {
            self.notice("error", "请先登录");
            return;
        }
        let Some(paths) = rfd::FileDialog::new()
            .set_title("选择要上传的文件（可多选）")
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let folder_id = self.folder_id.lock().unwrap().clone();
        self.upload_paths(paths, &folder_id);
    }

    fn create_folder(self: &Arc<Self>) {
        let Some(client) = self.client() else { return };
        let folder_id = self.folder_id.lock().unwrap().clone();
        let name = "新建文件夹";
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.create_folder(&folder_id, name).await {
                Ok(_) => {
                    this.notice("success", "文件夹已创建，可按 F2 重命名");
                    this.refresh_files();
                }
                Err(e) => this.notice("error", format!("创建文件夹失败：{e}")),
            }
        });
    }

    /// 批量上传本地路径到指定云端目录
    pub fn upload_paths(self: &Arc<Self>, paths: Vec<PathBuf>, folder_id: &str) {
        if self.client().is_none() {
            self.notice("error", "请先登录");
            return;
        }
        // 去重：同一批里重复出现的路径只上传一次（避免拖拽多文件时重复入队）
        let mut seen = std::collections::HashSet::new();
        let mut unique = Vec::new();
        for p in paths {
            let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if seen.insert(key) {
                unique.push(p);
            }
        }
        if unique.is_empty() {
            return;
        }
        let count = unique.len();
        for path in unique {
            let task_id = self.tasks.next_id();
            let is_dir = path.is_dir();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());
            let size = if is_dir {
                dir_size(&path)
            } else {
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            };
            self.tasks.add(Task {
                id: task_id,
                direction: TaskDirection::Upload,
                file_name: name,
                file_size: size,
                local_path: path,
                remote_folder_id: folder_id.to_string(),
                status: TaskStatus::Running,
                progress: 0.0,
                bytes_done: 0,
                speed: 0,
                last_done: 0,
                last_speed_time: 0,
                error: None,
                created_at: now_secs(),
            });
            self.spawn_transfer(task_id);
        }
        self.set_ui_state(|ui| ui.set_transfer_panel_open(true));
        self.refresh_transfers();
        self.notice("info", format!("已加入 {count} 个上传任务"));
    }

    fn handle_files_dropped(self: &Arc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.client().is_none() {
            self.notice("error", "请先登录后再拖入文件");
            return;
        }
        let folder_id = self.folder_id.lock().unwrap().clone();
        self.upload_paths(paths, &folder_id);
    }

    // ==================================================================
    // 传输任务
    // ==================================================================
    /// 启动（或恢复）一个任务的实际传输
    fn spawn_transfer(self: &Arc<Self>, task_id: u64) {
        let Some(task) = self.tasks.get(task_id) else {
            return;
        };
        if self.controls.lock().unwrap().contains_key(&task_id) {
            return; // 已有存活的控制句柄，说明任务还在跑（可能是暂停挂起）
        }
        // Pending / 重试后重新开始；已是 Running 时 transition 返回 false 也无妨
        let _ = self.tasks.transition(task_id, TaskStatus::Running);
        let control = TransferControl::new();
        self.controls
            .lock()
            .unwrap()
            .insert(task_id, control.clone());
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            this.run_transfer(task, control).await;
        });
    }

    async fn run_transfer(self: &Arc<Self>, task: Task, control: TransferControl) {
        let task_id = task.id;
        let Some(client) = self.client() else {
            self.finish_task(task_id, TaskStatus::Failed, Some("未登录".to_string()));
            self.controls.lock().unwrap().remove(&task_id);
            return;
        };
        let progress = self.progress_callback(task_id, task.file_size);
        let cfg = self
            .store
            .load_config()
            .unwrap_or_else(|_| AppConfig::default_config());

        let outcome: Outcome = match task.direction {
            TaskDirection::Download => {
                let file = FileObject {
                    id: task.remote_folder_id.clone(),
                    name: task.file_name.clone(),
                    size: task.file_size as i64,
                    ..Default::default()
                };
                let opts = DownloadOptions {
                    thread_count: cfg.download_thread.max(1),
                    resume: true,
                    chunk_size: 4 * 1024 * 1024,
                };
                match transfer::download(
                    &client,
                    &file,
                    &task.local_path,
                    &opts,
                    progress,
                    &control,
                )
                .await
                {
                    Ok(DownloadOutcome::Completed) => Outcome::Done,
                    Ok(DownloadOutcome::Paused) => Outcome::Paused,
                    Ok(DownloadOutcome::Canceled) => Outcome::Canceled,
                    Err(e) if e.is_interrupted() => Outcome::from_error(&e),
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            }
            TaskDirection::Upload => {
                let opts = UploadOptions {
                    rapid_upload: cfg.rapid_upload,
                    thread_count: cfg.upload_thread.max(1),
                    generate_torrent: cfg.generate_torrent,
                    ..Default::default()
                };
                if task.local_path.is_dir() {
                    match transfer::upload_folder_recursive(
                        &client,
                        &task.remote_folder_id,
                        &task.local_path,
                        &opts,
                        progress,
                        &control,
                    )
                    .await
                    {
                        Ok(_) => Outcome::Done,
                        Err(e) if e.is_interrupted() => Outcome::from_error(&e),
                        Err(e) => Outcome::Failed(e.to_string()),
                    }
                } else {
                    match transfer::upload(
                        &client,
                        &task.remote_folder_id,
                        &task.local_path,
                        &opts,
                        progress,
                        &control,
                    )
                    .await
                    {
                        Ok(UploadOutcome::Completed(_)) => Outcome::Done,
                        Ok(UploadOutcome::Paused) => Outcome::Paused,
                        Ok(UploadOutcome::Canceled) => Outcome::Canceled,
                        Err(e) if e.is_interrupted() => Outcome::from_error(&e),
                        Err(e) => Outcome::Failed(e.to_string()),
                    }
                }
            }
        };

        match outcome {
            Outcome::Done => {
                self.tasks.update(task_id, |t| {
                    t.status = TaskStatus::Completed;
                    t.progress = 100.0;
                    t.bytes_done = t.file_size;
                    t.speed = 0;
                    t.error = None;
                });
                self.controls.lock().unwrap().remove(&task_id);
                self.rate_samples.lock().unwrap().remove(&task_id);
                let label = if task.direction == TaskDirection::Upload {
                    "上传完成"
                } else {
                    "下载完成"
                };
                self.notice("success", format!("{label}：{}", task.file_name));
                if task.direction == TaskDirection::Upload {
                    self.refresh_files();
                }
            }
            Outcome::Paused => {
                self.tasks.transition(task_id, TaskStatus::Paused);
                self.tasks.update(task_id, |t| t.speed = 0);
            }
            Outcome::Canceled => {
                self.tasks.transition(task_id, TaskStatus::Canceled);
                self.controls.lock().unwrap().remove(&task_id);
                self.rate_samples.lock().unwrap().remove(&task_id);
            }
            Outcome::Failed(msg) => {
                self.finish_task(task_id, TaskStatus::Failed, Some(msg));
                self.controls.lock().unwrap().remove(&task_id);
                self.rate_samples.lock().unwrap().remove(&task_id);
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn finish_task(&self, task_id: u64, status: TaskStatus, err: Option<String>) {
        self.tasks.transition(task_id, status);
        self.tasks.update(task_id, |t| {
            t.status = status;
            t.error = err.clone();
            if status == TaskStatus::Failed {
                t.speed = 0;
            }
        });
        if let Some(e) = err {
            let name = self
                .tasks
                .get(task_id)
                .map(|t| t.file_name)
                .unwrap_or_default();
            self.notice("error", format!("{name} 传输失败：{e}"));
        }
    }

    /// 进度回调：只写任务表 + 置脏，UI 刷新交给定时器节流
    fn progress_callback(&self, task_id: u64, total: u64) -> transfer::ProgressCallback {
        let tasks = Arc::clone(&self.tasks);
        let dirty = Arc::clone(&self.dirty);
        let samples = Arc::clone(&self.rate_samples);
        Arc::new(move |done: u64| {
            let now = now_millis();
            let mut guard = samples.lock().unwrap_or_else(|e| e.into_inner());
            let last_ms = guard.get(&task_id).map(|(_, ms)| *ms).unwrap_or(0);
            // 200ms 采样窗口：既算瞬时速度，也避免高频写盘与 UI 抖动
            if now.saturating_sub(last_ms) < TICK_MS {
                return;
            }
            guard.insert(task_id, (done, now));
            drop(guard);
            if tasks.get(task_id).map(|t| t.status) == Some(TaskStatus::Running) {
                tasks.set_progress(task_id, done, total, now);
                dirty.store(true, Ordering::SeqCst);
            }
        })
    }

    fn pause_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if !task.status.can_pause() {
            return;
        }
        let control = self.controls.lock().unwrap().get(&id).cloned();
        match control {
            Some(ctl) => {
                // 真正的协作式暂停：传输任务在检查点挂起，连接与会话保留
                ctl.pause();
                self.tasks.transition(id, TaskStatus::Paused);
                self.tasks.update(id, |t| t.speed = 0);
                self.notice("info", format!("已暂停：{}", task.file_name));
            }
            None => {
                // 没有控制句柄（例如上次进程遗留的任务）：标记为暂停即可
                self.tasks.transition(id, TaskStatus::Paused);
                self.notice("info", format!("已暂停：{}", task.file_name));
            }
        }
        self.refresh_transfers();
    }

    fn resume_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if !task.status.can_resume() {
            return;
        }
        let control = self.controls.lock().unwrap().get(&id).cloned();
        match control {
            Some(ctl) => {
                // 会话仍在：唤醒挂起的传输
                self.tasks.transition(id, TaskStatus::Running);
                ctl.resume();
                self.notice("info", format!("已继续：{}", task.file_name));
                self.refresh_transfers();
            }
            None => {
                // 会话已丢失（重启/失败后）：从断点重新开始
                self.tasks.update(id, |t| {
                    t.error = None;
                    t.status = TaskStatus::Pending;
                });
                self.spawn_transfer(id);
                self.notice("info", format!("已重新开始：{}", task.file_name));
                self.refresh_transfers();
            }
        }
    }

    fn cancel_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if !task.status.can_cancel() {
            // 已完成/失败的“取消”等价于从列表移除
            if task.status.is_terminal() {
                self.remove_task(id);
            }
            return;
        }
        let control = self.controls.lock().unwrap().get(&id).cloned();
        if let Some(ctl) = control {
            // 取消 → 检查点返回 Canceled → 传输任务自行收尾
            ctl.cancel();
            self.tasks.transition(id, TaskStatus::Canceled);
            self.tasks.update(id, |t| t.speed = 0);
            self.controls.lock().unwrap().remove(&id);
        } else {
            self.tasks.transition(id, TaskStatus::Canceled);
        }
        self.rate_samples.lock().unwrap().remove(&id);
        self.notice("info", format!("已取消：{}", task.file_name));
        self.refresh_transfers();
    }

    fn retry_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        self.tasks.update(id, |t| {
            t.status = TaskStatus::Pending;
            t.progress = 0.0;
            t.bytes_done = 0;
            t.speed = 0;
            t.error = None;
            t.last_done = 0;
            t.last_speed_time = 0;
        });
        self.spawn_transfer(id);
        self.notice("info", format!("已重试：{}", task.file_name));
        self.refresh_transfers();
    }

    fn remove_task(self: &Arc<Self>, id: u64) {
        if let Some(ctl) = self.controls.lock().unwrap().remove(&id) {
            ctl.cancel();
        }
        self.rate_samples.lock().unwrap().remove(&id);
        self.tasks.remove(id);
        self.refresh_transfers();
    }

    fn reveal_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        let target = if task.direction == TaskDirection::Download {
            task.local_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| task.local_path.clone())
        } else {
            self.store
                .load_config()
                .map(|c| c.download_dir)
                .unwrap_or_else(|_| PathBuf::from("."))
        };
        if let Err(e) = file::open_with_system(&target) {
            self.notice("error", format!("无法打开目录：{e}"));
        }
    }

    fn clear_finished_tasks(self: &Arc<Self>) {
        let before = self.tasks.list().len();
        self.tasks.clear_finished();
        let removed = before - self.tasks.list().len();
        self.refresh_transfers();
        if removed == 0 {
            self.notice("info", "没有可清理的已结束任务");
        } else {
            self.notice("success", format!("已清理 {removed} 个已结束任务"));
        }
    }

    /// UI 定时器调用：把脏数据刷到界面（200ms 节流）
    pub fn tick(self: &Arc<Self>) {
        if self.dirty.swap(false, Ordering::SeqCst) {
            self.refresh_transfers();
        }
    }

    fn refresh_transfers(self: &Arc<Self>) {
        let filter = self
            .ui
            .upgrade()
            .map(|ui| ui.get_transfer_filter())
            .unwrap_or(0);
        let mut tasks = self.tasks.list();
        tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        let active: Vec<TransferEntry> = tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                )
            })
            .take(MAX_TRANSFER_ROWS)
            .map(to_transfer_entry)
            .collect();
        let rows: Vec<TransferEntry> = tasks
            .iter()
            .filter(|t| match filter {
                1 => matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                ),
                2 => t.status == TaskStatus::Completed,
                3 => matches!(t.status, TaskStatus::Failed | TaskStatus::Canceled),
                _ => true,
            })
            .take(MAX_TRANSFER_ROWS)
            .map(to_transfer_entry)
            .collect();
        let active_count = active.len() as i32;
        let finished = self.tasks.finished_count() as i32;
        self.set_ui_state(move |ui| {
            ui.set_transfer_list(transfers_model(rows));
            ui.set_active_transfer_list(transfers_model(active));
            ui.set_active_transfers(active_count);
            ui.set_finished_transfers(finished);
        });
    }

    // ==================================================================
    // 拖拽
    // ==================================================================
    pub fn set_drag_state(&self, state: &'static crate::DragDropState) {
        *self.drag_state.lock().unwrap() = Some(state);
    }

    /// 判断拖放目标是否可接收
    fn check_drop_target(
        &self,
        event: &slint::language::DropEvent,
        folder_id: &str,
    ) -> slint::language::DragAction {
        let Some(src) = Self::dragged_file(event) else {
            return slint::language::DragAction::None;
        };
        if src.id == folder_id {
            return slint::language::DragAction::None;
        }
        slint::language::DragAction::Move
    }

    fn dragged_file(event: &slint::language::DropEvent) -> Option<MovedFile> {
        event
            .data
            .user_data()
            .and_then(|d| d.downcast::<MovedFile>().ok().map(|m| m.as_ref().clone()))
    }

    fn handle_drop_move(self: &Arc<Self>, event: &slint::language::DropEvent, folder_id: &str) {
        let Some(src) = Self::dragged_file(event) else {
            return;
        };
        if src.id == folder_id {
            return;
        }
        let Some(file) = self.find_visible(&src.id) else {
            return;
        };
        let Some(client) = self.client() else { return };
        let folder_id = folder_id.to_string();
        let name = src.name.clone();
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.move_to(&file, &folder_id).await {
                Ok(_) => {
                    this.notice("success", format!("已移动 {name}"));
                    this.refresh_files();
                }
                Err(e) => this.notice("error", format!("移动失败：{e}")),
            }
        });
    }

    #[cfg(windows)]
    fn handle_drag_out(self: &Arc<Self>, id: &str, name: &str) {
        if self.client().is_none() {
            self.notice("error", "请先登录");
            return;
        }
        let Some(file) = self.find_visible(id) else {
            return;
        };
        if self.drag_out_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let name = name.to_string();
        let this = Arc::clone(self);
        let ui = self.ui.clone();
        let id = id.to_string();
        self.set_ui_state(move |win| {
            win.set_status_text(SharedString::from("正在准备文件，拖到本地文件夹即可下载…"));
            let api = win.global::<DragOutApi>();
            api.set_active(true);
            api.set_active_id(SharedString::from(id.as_str()));
        });
        self.backend.spawn(move || async move {
            let temp_dir = std::env::temp_dir().join("tianyi-dragout");
            let _ = std::fs::create_dir_all(&temp_dir);
            let temp_path = temp_dir.join(sanitize_file_name(&name));
            let ok = if let Some(client) = this.client() {
                let opts = DownloadOptions::default();
                let ctl = TransferControl::new();
                let prog: transfer::ProgressCallback = Arc::new(|_| {});
                matches!(
                    transfer::download(&client, &file, &temp_path, &opts, prog, &ctl).await,
                    Ok(DownloadOutcome::Completed)
                )
            } else {
                false
            };
            if !ok {
                this.clear_drag_out_active();
                this.drag_out_busy.store(false, Ordering::SeqCst);
                this.notice("error", "拖出下载失败：无法获取文件");
                return;
            }
            let temp_refs = [temp_path.clone()];
            let dropped = crate::dragout::do_drag_drop(&temp_refs).unwrap_or(false);
            crate::dragout::cleanup_temp_paths(&temp_refs);
            this.drag_out_busy.store(false, Ordering::SeqCst);
            this.clear_drag_out_active();
            invoke_ui(&ui, move |win| {
                // 系统拖拽结束后松开左键的消息可能被 OLE 消费，主动派发一次 PointerReleased
                use slint::platform::{PointerEventButton, WindowEvent};
                use slint::LogicalPosition;
                let _ = win
                    .window()
                    .try_dispatch_event(WindowEvent::PointerReleased {
                        position: LogicalPosition::new(-1000.0, -1000.0),
                        button: PointerEventButton::Left,
                    });
                let msg = if dropped {
                    format!("已拖出 {name}")
                } else {
                    "已取消拖拽".to_string()
                };
                win.set_status_text(SharedString::from(msg));
            });
        });
    }

    #[cfg(not(windows))]
    fn handle_drag_out(self: &Arc<Self>, _id: &str, _name: &str) {
        self.notice("info", "当前系统暂不支持拖出下载");
    }

    #[cfg(windows)]
    fn clear_drag_out_active(&self) {
        self.set_ui_state(|win| {
            let api = win.global::<DragOutApi>();
            api.set_active(false);
            api.set_active_id(SharedString::default());
        });
    }

    // ==================================================================
    // 后台任务
    // ==================================================================
    pub fn spawn_background(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        backend.spawn(move || async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                if let Some(client) = this.client() {
                    if let Err(e) = client.keep_alive().await {
                        log::debug!("keepalive: {e}");
                    }
                }
            }
        });
    }
}

/// 传输结果归一化
/// 需要用户确认后才执行的动作
///
/// 危险操作不直接执行：先落到这里，由 UI 的对话框回传确认或取消。
enum PendingConfirm {
    DeleteFiles(Vec<FileObject>),
}

enum Outcome {
    Done,
    Paused,
    Canceled,
    Failed(String),
}

impl Outcome {
    fn from_error(e: &tianyi_core::error::Error) -> Self {
        match e {
            tianyi_core::error::Error::Paused => Outcome::Paused,
            tianyi_core::error::Error::Canceled => Outcome::Canceled,
            other => Outcome::Failed(other.to_string()),
        }
    }
}

/// 网盘内拖拽载荷
#[derive(Debug, Clone)]
struct MovedFile {
    id: String,
    name: String,
}

fn to_transfer_entry(t: &Task) -> TransferEntry {
    let direction = if t.direction == TaskDirection::Upload {
        "upload"
    } else {
        "download"
    };
    let remaining = t.file_size.saturating_sub(t.bytes_done);
    let eta = if t.status == TaskStatus::Running && t.speed > 0 {
        file::format_eta(remaining, t.speed)
    } else {
        String::new()
    };
    let detail = if t.file_size == 0 {
        file::format_size(t.bytes_done)
    } else {
        format!(
            "{} / {}{}",
            file::format_size(t.bytes_done),
            file::format_size(t.file_size),
            if eta.is_empty() {
                String::new()
            } else {
                format!(" · {eta}")
            }
        )
    };
    TransferEntry {
        id: t.id as i32,
        name: t.file_name.clone().into(),
        direction: direction.into(),
        size_text: file::format_size(t.file_size).into(),
        progress: file::percent(t.bytes_done, t.file_size),
        status: t.status.as_key().into(),
        status_text: t.status.label_zh().into(),
        speed: if t.status == TaskStatus::Running && t.speed > 0 {
            file::format_speed(t.speed).into()
        } else {
            SharedString::default()
        },
        detail: detail.into(),
        error: t.error.clone().unwrap_or_default().into(),
    }
}

fn files_to_entries(files: &[FileObject], selected: &HashSet<String>) -> Vec<FileEntry> {
    files
        .iter()
        .map(|f| {
            let kind = if f.is_dir {
                "folder"
            } else {
                match file::classify(&f.name) {
                    FileKind::Image => "image",
                    FileKind::Video => "video",
                    FileKind::Audio => "audio",
                    FileKind::Pdf => "pdf",
                    FileKind::Text => "text",
                    FileKind::Archive | FileKind::Torrent => "archive",
                    FileKind::Office => "office",
                    FileKind::Other => "other",
                }
            };
            let kind_text = if f.is_dir {
                "文件夹".to_string()
            } else {
                file::classify(&f.name).label().to_string()
            };
            FileEntry {
                id: f.id.clone().into(),
                name: f.name.clone().into(),
                size: f.size as i32,
                size_text: if f.is_dir {
                    "—".to_string()
                } else {
                    file::format_size(f.size.max(0) as u64)
                }
                .into(),
                is_dir: f.is_dir,
                thumb: f.thumb.clone().into(),
                modified: file::format_datetime(&f.last_op_time).into(),
                kind: kind.into(),
                kind_text: kind_text.into(),
                selected: selected.contains(&f.id),
            }
        })
        .collect()
}

/// Shift 范围选择：返回锚点到目标之间（含两端）的 id 列表，按显示顺序
///
/// 锚点或目标已不在当前列表中时返回 None，调用方退化为单选。
pub fn range_selection(rows: &[FileObject], anchor: &str, target: &str) -> Option<Vec<String>> {
    let from = rows.iter().position(|f| f.id == anchor)?;
    let to = rows.iter().position(|f| f.id == target)?;
    let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
    Some(rows[lo..=hi].iter().map(|f| f.id.clone()).collect())
}

/// 排序键的中文名称（状态栏提示用）
fn sort_label(key: i32) -> &'static str {
    match key {
        1 => "大小",
        2 => "修改时间",
        _ => "名称",
    }
}

/// 按指定列排序：文件夹始终排在文件前面，组内按列比较
///
/// * 名称：忽略大小写比较，完全相同再按原始字节比较，保证顺序稳定
/// * 大小：数值比较
/// * 修改时间：沿用接口返回的原始时间串（`yyyy-MM-dd HH:mm:ss`），空值始终排在最后
pub fn sort_files(rows: &mut [FileObject], key: i32, asc: bool) {
    rows.sort_by(|a, b| {
        let dir_ord = b.is_dir.cmp(&a.is_dir); // 文件夹（true）在前
        if dir_ord != std::cmp::Ordering::Equal {
            return dir_ord;
        }
        let ord = match key {
            1 => a.size.cmp(&b.size),
            2 => match (a.last_op_time.is_empty(), b.last_op_time.is_empty()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => return std::cmp::Ordering::Greater,
                (false, true) => return std::cmp::Ordering::Less,
                (false, false) => a.last_op_time.cmp(&b.last_op_time),
            },
            _ => {
                let la = a.name.to_lowercase();
                let lb = b.name.to_lowercase();
                la.cmp(&lb).then_with(|| a.name.cmp(&b.name))
            }
        };
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });
}

fn entries_model(entries: Vec<FileEntry>) -> slint::ModelRc<FileEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

fn transfers_model(entries: Vec<TransferEntry>) -> slint::ModelRc<TransferEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

/// 账号首字符（头像用）
fn account_initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// 去掉文件名中不安全的字符
fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// 递归统计本地目录大小
fn dir_size(root: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => total += dir_size(&p),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// 平台 UI 字体
fn platform_ui_font() -> String {
    if cfg!(target_os = "windows") {
        "Segoe UI Variable Text".to_string()
    } else if cfg!(target_os = "macos") {
        "SF Pro Text".to_string()
    } else {
        "Cantarell".to_string()
    }
}

fn platform_mono_font() -> String {
    if cfg!(target_os = "windows") {
        "Cascadia Mono".to_string()
    } else if cfg!(target_os = "macos") {
        "SF Mono".to_string()
    } else {
        "DejaVu Sans Mono".to_string()
    }
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    let mut clip = arboard::Clipboard::new()?;
    clip.set_text(text.to_string())?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
// ==================================================================
// 提示条（Toast）：分级时长 + 去重 + 代际
// ==================================================================

/// 去重窗口（毫秒）：窗口内出现相同文案时只累加次数，不重开一段提示
const NOTICE_DEDUP_MS: u64 = 3_000;

/// 提示条自动消失时长
///
/// 错误信息通常要读完才知道怎么处理，给最长 8 s；正常反馈 3 s 足够，
/// 警告居中取 5 s，未知类型按 info 处理。
fn notice_ttl(kind: &str) -> Duration {
    match kind {
        "warning" => Duration::from_secs(5),
        "error" => Duration::from_secs(8),
        _ => Duration::from_secs(3),
    }
}

/// 到期回调是否允许清空提示条
///
/// 连续操作会连排多个计时器；旧计时器到期时若直接抹掉文案，就会把刚出现的新提示吞掉，
/// 所以只有"最新一代"的计时器有权清空。
fn should_clear_notice(current_gen: u64, fired_gen: u64) -> bool {
    current_gen == fired_gen
}

/// Toast 的去重与代际状态
#[derive(Default)]
struct NoticeSlot {
    /// 每记录一条提示 +1；计时器只清空与自己同代的提示
    gen: u64,
    /// 当前展示的文案（空 = 没有提示）
    text: String,
    /// 最近一次记录时间（毫秒），用于判断去重窗口
    at_ms: u64,
    /// 去重窗口内的重复次数
    count: i32,
}

impl NoticeSlot {
    /// 记录一条提示，返回 (本代 gen, 应显示的重复次数)
    fn push(&mut self, text: &str, now_ms: u64) -> (u64, i32) {
        let repeat = !self.text.is_empty()
            && self.text == text
            && now_ms.saturating_sub(self.at_ms) <= NOTICE_DEDUP_MS;
        if repeat {
            self.count = self.count.saturating_add(1);
        } else {
            self.text = text.to_string();
            self.count = 1;
        }
        self.at_ms = now_ms;
        self.gen = self.gen.wrapping_add(1);
        (self.gen, self.count)
    }

    /// 用户手动关闭：推进代际，让在途计时器失效
    fn dismiss(&mut self) {
        self.text.clear();
        self.count = 1;
        self.gen = self.gen.wrapping_add(1);
    }

    /// 计时器到期：仅当代际仍是自己的时候清空
    fn expire(&mut self, fired_gen: u64) -> bool {
        if !should_clear_notice(self.gen, fired_gen) {
            return false;
        }
        self.text.clear();
        self.count = 1;
        true
    }
}

/// 取消重命名后要写回 UI 的 (renaming-id, 状态栏文案)
///
/// 抽成纯函数，让"取消之后 renaming-id 必定为空"这条约定能被单元测试钉住：
/// 重命名输入框原本只有 accepted 一个出口，Esc 必须走同一条清空路径。
fn rename_cancelled_state() -> (SharedString, &'static str) {
    (SharedString::default(), "已取消重命名")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(id: &str, name: &str, size: i64, is_dir: bool) -> FileObject {
        FileObject {
            id: id.to_string(),
            name: name.to_string(),
            size,
            is_dir,
            last_op_time: "2024-05-06 12:03:44".to_string(),
            ..Default::default()
        }
    }

    fn task(id: u64, direction: TaskDirection, status: TaskStatus) -> Task {
        Task {
            id,
            direction,
            file_name: "demo.bin".to_string(),
            file_size: 1000,
            local_path: PathBuf::from("demo.bin"),
            remote_folder_id: "f1".to_string(),
            status,
            progress: 0.0,
            bytes_done: 0,
            speed: 0,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: 1,
        }
    }

    // ---------------- 二维码状态机映射 ----------------

    #[test]
    fn qr_state_mapping_covers_all_login_stages() {
        assert_eq!(qr_state_of("等待扫码..."), "waiting");
        assert_eq!(qr_state_of("已扫码，请在手机上确认"), "scanned");
        assert_eq!(qr_state_of("登录成功"), "success");
        assert_eq!(qr_state_of("二维码已过期，请重新获取"), "expired");
        assert_eq!(qr_state_of("网络错误：连接超时"), "error");
        // 未知文案不应崩溃，回落到等待态
        assert_eq!(qr_state_of(""), "waiting");
    }

    // ---------------- 账号展示 ----------------

    #[test]
    fn account_initial_handles_ascii_cjk_and_empty() {
        assert_eq!(account_initial("19051035392@189.cn"), "1");
        assert_eq!(account_initial("张三"), "张");
        assert_eq!(account_initial(""), "");
    }

    // ---------------- 文件列表映射 ----------------

    #[test]
    fn files_to_entries_marks_selection_from_authoritative_set() {
        let rows = vec![file("1", "a.txt", 1, false), file("2", "b.txt", 2, false)];
        let selected: HashSet<String> = ["2".to_string()].into_iter().collect();
        let entries = files_to_entries(&rows, &selected);
        assert!(!entries[0].selected);
        assert!(entries[1].selected);
        // 空选择集：全部未选中，不能因为默认值而误显示为选中
        assert!(files_to_entries(&rows, &HashSet::new())
            .iter()
            .all(|e| !e.selected));
    }

    #[test]
    fn sort_files_keeps_folders_first_and_orders_by_key() {
        let mut rows = vec![
            file("1", "b.txt", 300, false),
            file("2", "a.txt", 100, false),
            file("3", "dir_z", 0, true),
            file("4", "dir_a", 0, true),
        ];

        // 名称升序：文件夹在前
        sort_files(&mut rows, 0, true);
        let names: Vec<&str> = rows.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["dir_a", "dir_z", "a.txt", "b.txt"]);

        // 名称降序：文件夹仍在前面（组内反向）
        sort_files(&mut rows, 0, false);
        let names: Vec<&str> = rows.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["dir_z", "dir_a", "b.txt", "a.txt"]);

        // 大小升序/降序只看文件组
        sort_files(&mut rows, 1, true);
        let files: Vec<i64> = rows.iter().filter(|f| !f.is_dir).map(|f| f.size).collect();
        assert_eq!(files, [100, 300]);
        sort_files(&mut rows, 1, false);
        let files: Vec<i64> = rows.iter().filter(|f| !f.is_dir).map(|f| f.size).collect();
        assert_eq!(files, [300, 100]);
    }

    #[test]
    fn sort_files_puts_unknown_time_last_in_both_directions() {
        let mut rows = vec![
            FileObject {
                id: "1".into(),
                name: "old.bin".into(),
                last_op_time: "2023-01-01 00:00:00".into(),
                ..Default::default()
            },
            FileObject {
                id: "2".into(),
                name: "new.bin".into(),
                last_op_time: "2024-12-31 23:59:59".into(),
                ..Default::default()
            },
            FileObject {
                id: "3".into(),
                name: "unknown.bin".into(),
                last_op_time: String::new(),
                ..Default::default()
            },
        ];
        sort_files(&mut rows, 2, true);
        assert_eq!(rows[0].name, "old.bin");
        assert_eq!(rows[2].name, "unknown.bin");
        sort_files(&mut rows, 2, false);
        assert_eq!(rows[0].name, "new.bin");
        // 时间未知的条目在两个方向上都排在最后，避免"空时间"抢占首位
        assert_eq!(rows[2].name, "unknown.bin");
    }

    // ---------------- 多选范围 ----------------

    #[test]
    fn range_selection_covers_both_directions() {
        let rows = vec![
            file("1", "a", 0, false),
            file("2", "b", 0, false),
            file("3", "c", 0, false),
            file("4", "d", 0, false),
        ];
        assert_eq!(
            range_selection(&rows, "2", "4").unwrap(),
            vec!["2".to_string(), "3".to_string(), "4".to_string()]
        );
        // 反向拖选与正向结果一致
        assert_eq!(
            range_selection(&rows, "4", "2").unwrap(),
            vec!["2".to_string(), "3".to_string(), "4".to_string()]
        );
        // 锚点不存在（例如刷新后行被移除）→ None，调用方退化为单选
        assert!(range_selection(&rows, "99", "2").is_none());
        assert!(range_selection(&rows, "2", "99").is_none());
    }

    #[test]
    fn files_to_entries_maps_kind_and_size_text() {
        let rows = vec![
            file("1", "文档", 0, true),
            file("2", "photo.PNG", 2048, false),
            file("3", "movie.mp4", 1024 * 1024, false),
            file("4", "archive.7z", 100, false),
            file("5", "noext", 10, false),
        ];
        let entries = files_to_entries(&rows, &HashSet::new());
        assert_eq!(entries.len(), 5);

        assert_eq!(entries[0].kind.as_str(), "folder");
        assert_eq!(entries[0].kind_text.as_str(), "文件夹");
        assert_eq!(entries[0].size_text.as_str(), "—");
        assert!(entries[0].is_dir);

        // 扩展名大小写不敏感
        assert_eq!(entries[1].kind.as_str(), "image");
        assert_eq!(entries[1].kind_text.as_str(), "图片");
        assert_eq!(entries[1].size_text.as_str(), "2.00 KB");

        assert_eq!(entries[2].kind.as_str(), "video");
        assert_eq!(entries[3].kind.as_str(), "archive");
        assert_eq!(entries[4].kind.as_str(), "other");
        assert_eq!(entries[4].kind_text.as_str(), "其他");

        // 修改时间统一格式化
        assert_eq!(entries[0].modified.as_str(), "2024/05/06 12:03");
    }

    #[test]
    fn files_to_entries_handles_empty_and_long_names() {
        assert!(files_to_entries(&[], &HashSet::new()).is_empty());
        let long = "长".repeat(300) + ".txt";
        let entries = files_to_entries(&[file("1", &long, 0, false)], &HashSet::new());
        // 名称不截断（由 UI 负责省略），但分类仍然正确
        assert_eq!(entries[0].name.len(), long.len());
        assert_eq!(entries[0].kind.as_str(), "text");
    }

    // ---------------- 传输条目映射 ----------------

    #[test]
    fn transfer_entry_reports_status_and_progress() {
        let mut t = task(7, TaskDirection::Upload, TaskStatus::Running);
        t.bytes_done = 250;
        t.speed = 1024;
        let e = to_transfer_entry(&t);
        assert_eq!(e.id, 7);
        assert_eq!(e.direction.as_str(), "upload");
        assert_eq!(e.status.as_str(), "running");
        assert_eq!(e.status_text.as_str(), "传输中");
        assert!((e.progress - 25.0).abs() < 0.001);
        assert_eq!(e.speed.as_str(), "1.00 KB/s");
        assert!(e.detail.contains("250 B"));
        assert!(e.detail.contains("剩余"));

        // 已完成：进度 100%，不再显示速度
        let mut done = task(8, TaskDirection::Download, TaskStatus::Completed);
        done.bytes_done = 1000;
        let e = to_transfer_entry(&done);
        assert_eq!(e.status.as_str(), "completed");
        assert_eq!(e.status_text.as_str(), "已完成");
        assert!((e.progress - 100.0).abs() < 0.001);
        assert_eq!(e.speed.as_str(), "—".repeat(0)); // 空串
    }

    #[test]
    fn transfer_entry_clamps_progress_and_shows_error() {
        // 进度越界（后端回调可能超过总量）不应产生 >100 的百分比
        let mut t = task(9, TaskDirection::Download, TaskStatus::Running);
        t.bytes_done = 5000;
        let e = to_transfer_entry(&t);
        assert!((0.0..=100.0).contains(&e.progress));

        let mut failed = task(10, TaskDirection::Download, TaskStatus::Failed);
        failed.error = Some("网络中断".to_string());
        let e = to_transfer_entry(&failed);
        assert_eq!(e.status_text.as_str(), "失败");
        assert_eq!(e.error.as_str(), "网络中断");

        // 大小为 0 的任务不能除零
        let mut zero = task(11, TaskDirection::Upload, TaskStatus::Running);
        zero.file_size = 0;
        zero.bytes_done = 0;
        let e = to_transfer_entry(&zero);
        assert!((e.progress - 0.0).abs() < 0.001);
    }

    // ---------------- 搜索结果与目录内容的切换 ----------------

    #[test]
    fn clear_search_restores_directory_rows() {
        let dir = vec![file("1", "a.txt", 1, false), file("2", "b", 0, true)];
        let hits = vec![file("9", "hit.txt", 1, false)];

        let searching = Controller::rows_for_display(&dir, Some(&hits));
        assert_eq!(searching.len(), 1);
        assert_eq!(searching[0].id, "9");

        // 清空搜索后必须回到当前目录内容（而不是空列表）
        let restored = Controller::rows_for_display(&dir, None);
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[1].name, "b");

        // 空搜索结果也是合法状态
        let empty: Vec<FileObject> = Vec::new();
        assert!(Controller::rows_for_display(&dir, Some(&empty)).is_empty());
    }

    // ---------------- 传输失败原因归一化 ----------------

    #[test]
    fn interrupted_errors_are_not_failures() {
        use tianyi_core::error::Error;
        assert!(matches!(
            Outcome::from_error(&Error::Paused),
            Outcome::Paused
        ));
        assert!(matches!(
            Outcome::from_error(&Error::Canceled),
            Outcome::Canceled
        ));
        match Outcome::from_error(&Error::Transfer("boom".into())) {
            Outcome::Failed(msg) => assert!(msg.contains("boom")),
            _ => panic!("普通错误必须归类为失败"),
        }
    }

    // ---------------- 文件名净化 ----------------

    #[test]
    fn sanitize_file_name_removes_reserved_characters() {
        assert_eq!(
            sanitize_file_name("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_file_name("正常文件名.txt"), "正常文件名.txt");
        assert_eq!(sanitize_file_name("   "), "unnamed");
        assert_eq!(sanitize_file_name(""), "unnamed");
        // 控制字符
        assert_eq!(sanitize_file_name("a\u{1}b"), "a_b");
    }

    // ---------------- 目录大小统计 ----------------

    #[test]
    fn dir_size_sums_nested_files() {
        let root = std::env::temp_dir().join(format!("tianyi-dirsize-{}", std::process::id()));
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("a.bin"), vec![0u8; 10]).unwrap();
        fs::write(sub.join("b.bin"), vec![0u8; 25]).unwrap();

        assert_eq!(dir_size(&root), 35);
        // 空目录
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(dir_size(&empty), 0);
        // 不存在的路径不应 panic
        assert_eq!(dir_size(&root.join("missing")), 0);

        let _ = fs::remove_dir_all(&root);
    }

    // ---------------- 时间戳 ----------------

    #[test]
    fn timestamps_are_sane() {
        assert!(now_secs() > 1_600_000_000);
        assert!(now_millis() > 1_600_000_000_000);
        assert!(now_millis() / 1000 >= now_secs() as u64 - 1);
    }

    // ---------------- 提示条：分级时长 ----------------

    #[test]
    fn notice_ttl_maps_each_kind() {
        assert_eq!(notice_ttl("info"), Duration::from_secs(3));
        assert_eq!(notice_ttl("success"), Duration::from_secs(3));
        assert_eq!(notice_ttl("warning"), Duration::from_secs(5));
        assert_eq!(notice_ttl("error"), Duration::from_secs(8));
        // 未知类型退化为 info 的时长，不会变成"永不消失"
        assert_eq!(notice_ttl("never-seen"), Duration::from_secs(3));
    }

    // ---------------- 提示条：去重 ----------------

    #[test]
    fn notice_same_text_within_window_accumulates() {
        let mut slot = NoticeSlot::default();
        assert_eq!(slot.push("已删除 1 项", 1_000).1, 1);
        assert_eq!(slot.push("已删除 1 项", 1_500).1, 2);
        // 窗口边界（恰好 3 s）仍算重复
        assert_eq!(slot.push("已删除 1 项", 4_000).1, 3);
        assert_eq!(slot.text, "已删除 1 项");
    }

    #[test]
    fn notice_dedup_window_expires() {
        let mut slot = NoticeSlot::default();
        assert_eq!(slot.push("已删除 1 项", 1_000).1, 1);
        // 超过 3 s：视为新提示，计数回到 1
        assert_eq!(slot.push("已删除 1 项", 4_001).1, 1);
    }

    #[test]
    fn notice_different_text_resets_count() {
        let mut slot = NoticeSlot::default();
        assert_eq!(slot.push("已复制链接", 1_000).1, 1);
        assert_eq!(slot.push("已复制链接", 1_100).1, 2);
        // 文案变了：不再叠加，计数回到 1
        assert_eq!(slot.push("已删除 1 项", 1_200).1, 1);
        assert_eq!(slot.text, "已删除 1 项");
    }

    // ---------------- 提示条：代际 ----------------

    #[test]
    fn stale_notice_timer_cannot_clear_newer_notice() {
        let mut slot = NoticeSlot::default();
        let (old_gen, _) = slot.push("第一条", 1_000);
        let (new_gen, _) = slot.push("第二条", 1_100);
        assert_ne!(old_gen, new_gen);
        assert!(!should_clear_notice(new_gen, old_gen));
        // 旧计时器到期：不动新提示
        assert!(!slot.expire(old_gen));
        assert_eq!(slot.text, "第二条");
        // 最新一代到期：允许清空
        assert!(should_clear_notice(new_gen, new_gen));
        assert!(slot.expire(new_gen));
        assert!(slot.text.is_empty());
    }

    #[test]
    fn manual_close_invalidates_pending_notice_timer() {
        let mut slot = NoticeSlot::default();
        let (gen, _) = slot.push("已复制链接", 1_000);
        slot.dismiss();
        // 手动关闭后，在途计时器即使到期也不能再动状态
        assert!(!slot.expire(gen));
        assert!(slot.text.is_empty());
        assert_eq!(slot.count, 1);
    }

    // ---------------- 重命名取消 ----------------

    #[test]
    fn rename_cancel_clears_renaming_id() {
        let (renaming_id, status) = rename_cancelled_state();
        // 空 id = 没有任何行处于输入态，行内 TouchArea 恢复可用
        assert!(renaming_id.is_empty());
        assert_eq!(status, "已取消重命名");
    }
}
