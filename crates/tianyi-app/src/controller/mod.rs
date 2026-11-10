//! 应用控制器：桥接 Slint UI 事件与 tianyi-core 异步操作
//!
//! 设计要点：
//! * 所有网络/文件操作都在 tokio 运行时执行，UI 线程只做属性写入；
//! * 目录加载、搜索、空间切换各有独立的 generation 计数，过期响应直接丢弃，
//!   避免“旧请求覆盖新结果”的竞态；
//! * 传输任务通过 `TransferControl` 做协作式暂停/继续/取消，暂停不等于取消；
//! * 进度回调只写任务表并置脏标记，由 UI 线程的定时器以 200ms 节流刷新界面，
//!   避免高频跨线程重绘。

mod dnd;
mod files;
mod session;
mod transfers;
use self::dnd::MovedFile;
/// 只服务 `ui_tests`：断言"选中态更新必须就地改模型、不能整表换"这条不变量
/// （换模型会重建 ListView 的行实例，双击判定随之失效）
#[cfg(test)]
pub(crate) use self::files::apply_entries;
use self::files::PendingConfirm;
use self::session::ConfigStore;
use self::transfers::TransferSnapshot;
use crate::app::{DirectLink, DragOutApi, FileEntry, MainWindow, Theme};
use crate::backend::Backend;
use slint::{ComponentHandle, SharedString, Weak};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tianyi_core::client::TianyiClient;
use tianyi_core::config::{AppConfig, ThemeMode};
use tianyi_core::models::FileObject;
use tianyi_core::task::{TaskManager, TransferControl};

/// 根目录 id
const ROOT_FOLDER_ID: &str = "-11";
/// 面板刷新节流间隔（毫秒）
const TICK_MS: u64 = 200;
/// 单次刷新时最多展示的任务数
const MAX_TRANSFER_ROWS: usize = 200;
/// 登录页最多展示的已保存账号数
const MAX_SAVED_ACCOUNTS: usize = 4;
/// 同时真正在跑的任务数上限（任务级）
///
/// 这是"任务级"并发上限：与 `transfer.rs` 内部按 `thread_count` 做的**分片级**并发
/// 是两层完全不同的控制——这里限制"同时有几个任务在传"，那里限制"单个任务内部开几个
/// 分片"。tokio runtime 只有 4 个 worker（见 `backend.rs`），一口气放 20 个任务进去会
/// 把登录、列目录这类轻请求挤在队尾，所以留出名额给交互请求。
const MAX_CONCURRENT_TRANSFERS: usize = 3;
/// 进度推送阈值（千分比：10‰ = 1 个百分点）
///
/// 低于这个幅度的进度变化不重建列表模型；200ms 一跳的定时器会把它们累积起来，
/// 最多晚 1% 才刷一次屏，肉眼无感但省掉大量字符串格式化。
const PROGRESS_PUBLISH_PERMILLE: u64 = 10;
/// 并发分片数允许区间（设置页 `−/+` 与写回配置共用同一处定义）
const MIN_THREADS: i32 = 1;
const MAX_THREADS: i32 = 8;
/// 主配置目录不可用时的降级目录（位于系统临时目录下）
const FALLBACK_CONFIG_DIR: &str = "tianyi-cloud-fallback";
/// 内存态配置（不落盘）时设置页「配置目录」一栏的显示文案
const MEMORY_CONFIG_LABEL: &str = "（配置目录不可写，本次运行不保存设置）";

/// 应用控制器
pub struct Controller {
    ui: Weak<MainWindow>,
    /// 创建控制器时的线程 id（`Controller::new` 在 UI 线程上调用）
    ///
    /// 用来判断"已经在 UI 线程上"的调用可以同步执行闭包，不必再排一次事件循环。
    ui_thread: std::thread::ThreadId,
    backend: Arc<Backend>,
    store: ConfigStore,
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
    /// 任务级并发许可：拿到许可才真正开始传输，拿不到就停在 Pending 排队
    transfer_permits: Arc<tokio::sync::Semaphore>,
    /// 上次推给 UI 的传输快照（增量刷新的判据）
    last_snapshot: Mutex<Vec<TransferSnapshot>>,
    /// 强制推送一次：筛选器切换这类"快照里看不出来"的变化走这里
    force_publish: AtomicBool,
    /// 关窗收尾是否已开始
    ///
    /// `begin_close` 把在跑任务归一化成「已暂停」之后，后台传输任务往往还要跑几毫秒才
    /// 走到收尾分支；那些写回会把「已暂停」改回终态。这个标志让收尾路径知道"现在是
    /// 退出流程"，只允许停在「已暂停」。
    shutting_down: AtomicBool,

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
        // 配置目录不可写时降级启动（主目录 → 临时目录 → 内存态），绝不 panic：
        // 一个只读的 AppData 不应该让整个客户端起不来。
        let (store, degraded) = ConfigStore::open();
        // 内存态下不配持久化路径：既写不进去，也没必要留下半个任务文件
        let persist_path = if store.is_persistent() {
            Some(store.root().join("tasks.json"))
        } else {
            None
        };
        let tasks = TaskManager::new(persist_path);
        let theme_mode = ThemeMode::parse(
            &store
                .load_config()
                .unwrap_or_else(|_| AppConfig::default_config())
                .theme,
        );
        let c = Arc::new(Controller {
            ui,
            ui_thread: std::thread::current().id(),
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
            transfer_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRANSFERS)),
            last_snapshot: Mutex::new(Vec::new()),
            // 首帧必须推一次：此时 last_snapshot 还是空的，但显式置 true 更稳
            force_publish: AtomicBool::new(true),
            shutting_down: AtomicBool::new(false),
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
        // 降级必须让用户看见（放在 initial_sync 之后，避免被"空提示"覆盖）
        if let Some(msg) = degraded {
            c.notice("warning", msg);
        }
        c
    }

    /// 初始化静态 UI 状态：版本号、配置目录、字体、主题、任务列表
    fn initial_sync(self: &Arc<Self>) {
        let version = env!("CARGO_PKG_VERSION").to_string();
        // 内存态（配置目录不可写）时不能把假路径发给用户：直接说明设置不会保存
        let config_dir = if self.store.is_persistent() {
            self.store.root().display().to_string()
        } else {
            MEMORY_CONFIG_LABEL.to_string()
        };
        let log_dir = crate::logging::log_dir().display().to_string();
        let font = platform_ui_font();
        let mono = platform_mono_font();
        self.apply_theme();
        let cfg = self
            .store
            .load_config()
            .unwrap_or_else(|_| AppConfig::default_config());
        let upload_thread = cfg.upload_thread.to_string();
        let download_thread = cfg.download_thread.to_string();
        // 并发数同时以 int 形式下发：设置页的 +/− 需要数值边界，字符串只用于显示
        let upload_count = cfg.upload_thread as i32;
        let download_count = cfg.download_thread as i32;
        let download_dir = cfg.download_dir.display().to_string();
        self.set_ui_state(move |ui| {
            ui.set_version_text(SharedString::from(version));
            ui.set_config_dir(SharedString::from(config_dir));
            ui.set_log_dir(SharedString::from(log_dir));
            ui.set_upload_thread(SharedString::from(upload_thread));
            ui.set_download_thread(SharedString::from(download_thread));
            ui.set_upload_thread_count(upload_count);
            ui.set_download_thread_count(download_count);
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
            ui.on_open_selected(move || {
                let c = c.clone();
                c.open_selected();
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
                // 筛选器变化不体现在任务快照里（快照只记 id/状态/进度），必须强制
                // 推一次，否则用户切页签会继续看到上一次筛选的旧列表
                c.force_publish.store(true, Ordering::SeqCst);
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

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_open_link(move |url| {
                let c = c.clone();
                c.open_link(url.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_copy_all_links(move || {
                let c = c.clone();
                c.copy_all_links();
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

        // 并发分片数：Slint 只上报"+/-被点了一下"的目标值，钳制/落盘/回填都在控制器里，
        // 保证"界面上显示的数字"永远等于 AppConfig 里真正生效的数字。
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_thread_count_changed(move |download, upload| {
                let c = c.clone();
                c.set_thread_counts(download, upload);
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
            ui.on_open_log_dir(move || {
                let c = c.clone();
                c.open_log_dir();
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

        // ---------------- 关窗 ----------------
        // 用户点 × 时不再"直接把传输 future 丢掉"：先取消在途传输、把任务状态归一化
        // 并落盘，再让窗口关闭。这条路径上**绝不等**传输 future 结束（那会把关窗卡住
        // 几秒），只做"发取消信号 + 用当前状态落盘"。
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            let weak = ui.as_weak();
            ui.window().on_close_requested(move || {
                c.begin_close();
                // 显式 hide：即便某个平台不按 CloseRequested 的返回值收窗口，也能关掉
                if let Some(win) = weak.upgrade() {
                    let _ = win.window().hide();
                }
                slint::CloseRequestResponse::HideWindow
            });
        });
    }

    // ==================================================================
    // 基础设施
    // ==================================================================
    /// 在 UI 线程上执行一段 UI 操作
    ///
    /// 快路径：当前就在 UI 线程上（`tick()`、`on_*` UI 回调、`poll_system_theme()`、以及
    /// 构造期的 `bind_ui`）时**同步**执行。原先一律 `invoke_from_event_loop`，等于白送
    /// 一帧延迟，还会打乱顺序（notice 之后紧接着的"关闭"动作可能被重排到下一帧）。
    /// 跨线程调用（tokio 任务、拖拽 winit 回调等）仍然排队投递。
    fn ui_set<F: FnOnce(&MainWindow) + Send + 'static>(&self, f: F) {
        if let Some(ui) = self.upgrade_on_ui_thread() {
            f(&ui);
            return;
        }
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                f(&ui);
            }
        });
    }

    /// 当前线程就是 UI 线程时返回强引用，否则返回 None（需要跨线程投递）
    fn upgrade_on_ui_thread(&self) -> Option<MainWindow> {
        if !should_apply_inline(std::thread::current().id(), self.ui_thread) {
            return None;
        }
        // 构造期 UI 已经完全建好（`MainWindow::new()` 先于 `Controller::new`），
        // 所以这里 upgrade 一定能拿到窗口；拿不到就退回投递路径，不会静默丢操作。
        self.ui.upgrade()
    }

    fn set_ui_state<F: FnOnce(&MainWindow) + Send + 'static>(&self, f: F) {
        // 与 ui_set 同一条路径：UI 线程同步执行，其它线程投递到事件循环
        self.ui_set(f);
    }

    fn client(&self) -> Option<Arc<TianyiClient>> {
        lock_or_recover(&self.client).clone()
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
            let (gen, count) = lock_or_recover(&slot).push(&text, now_millis());
            ui.set_notice_text(SharedString::from(text.as_str()));
            ui.set_notice_kind(SharedString::from(kind.as_str()));
            ui.set_notice_count(count);
            let ttl = notice_ttl(kind.as_str());
            let slot = Arc::clone(&slot);
            slint::Timer::single_shot(ttl, move || {
                if !lock_or_recover(&slot).expire(gen) {
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
        lock_or_recover(&self.notice_slot).dismiss();
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
}

// ==================================================================
// 基础设施：锁恢复 + 配置目录降级
// ==================================================================

/// 取锁；若锁已被 panic 毒化，记录 warn 并继续用内部数据（不连锁 panic）
///
/// 控制器里每个 `Mutex` 装的都是"可重建的 UI 状态"（当前目录、选择集、提示条……），
/// 中毒只说明之前某次持锁期间 panic 过一次，数据本身仍然可用。`unwrap()` 会让之后
/// 每一次取锁都 panic，把一次局部故障放大成整个进程不可用，所以统一改成继续用。
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        log::warn!("互斥锁已中毒，继续使用其中的数据（避免连锁 panic）");
        e.into_inner()
    })
}

/// 是否可以在当前线程上同步执行 UI 闭包
///
/// 只做一件事：比较线程 id。抽成纯函数是为了能单测——真正的线程分支没法在单测里
/// 造出一个 Slint 窗口。
fn should_apply_inline(current: std::thread::ThreadId, ui_thread: std::thread::ThreadId) -> bool {
    current == ui_thread
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

#[cfg(test)]
mod tests {
    use self::files::{
        files_to_entries, range_selection, rename_cancelled_state, sanitize_file_name, sort_files,
    };
    use self::session::{account_initial, clamp_threads, pick_store_root, qr_state_of, StoreRoot};
    use self::transfers::{
        close_safe_status, close_status_for, close_transitions, dedupe_key, dir_size,
        needs_publish, progress_marker, to_transfer_entry, transfer_snapshot, Outcome,
    };
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;
    use tianyi_core::models::FileObject;
    use tianyi_core::task::{Task, TaskDirection, TaskStatus};

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

    // ---------------- UI 线程直通 ----------------

    #[test]
    fn should_apply_inline_only_on_ui_thread() {
        let ui = std::thread::current().id();
        // 同一线程：同步执行
        assert!(should_apply_inline(ui, ui));
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("辅助线程应正常结束");
        assert_ne!(other, ui, "不同线程的 id 必须不同");
        // 后台线程 ← UI 线程：走投递
        assert!(!should_apply_inline(other, ui));
        // 反向也必须判否，判断不能是"单边"的
        assert!(!should_apply_inline(ui, other));
    }

    // ---------------- 上传去重键 ----------------

    #[test]
    fn dedupe_key_normalizes_paths_without_touching_disk() {
        let base = std::env::temp_dir();
        let dir = base.join("tianyi-dedupe");
        let plain = dir.join("a.bin");
        // `.` 与 `..` 都被折叠，等价路径得到同一个键
        assert_eq!(
            dedupe_key(&dir.join("sub").join("..").join("a.bin"), &base),
            dedupe_key(&plain, &base)
        );
        // 相对路径按 base 补全
        assert_eq!(
            dedupe_key(std::path::Path::new("./a.bin"), &dir),
            dedupe_key(&plain, &dir)
        );
        // 不同文件仍然不同键（去重不能过度）
        assert_ne!(
            dedupe_key(&plain, &base),
            dedupe_key(&dir.join("b.bin"), &base)
        );
        if cfg!(windows) {
            // NTFS 默认大小写不敏感：同一路径的两种写法必须去重
            assert_eq!(
                dedupe_key(&base.join("MiXeD.TXT"), &base),
                dedupe_key(&base.join("mixed.txt"), &base)
            );
            // `..` 不能吃掉盘符/根
            assert_eq!(
                dedupe_key(std::path::Path::new("C:/../../a.bin"), base.as_path()),
                dedupe_key(std::path::Path::new("C:/a.bin"), base.as_path()),
                "`..` 折叠不能越过根"
            );
        }
    }

    // ---------------- 传输列表增量刷新 ----------------

    #[test]
    fn needs_publish_ignores_sub_one_percent_progress() {
        let prev = [(1_u64, TaskStatus::Running, 100_u64)];
        // 与上次完全一致
        assert!(!needs_publish(&prev, &prev));
        // 0.5 个百分点（5‰）低于 1% 阈值
        assert!(!needs_publish(&prev, &[(1, TaskStatus::Running, 105)]));
        // 恰好 1%（10‰）：阈值是"超过"，边界上不推
        assert!(!needs_publish(&prev, &[(1, TaskStatus::Running, 110)]));
    }

    #[test]
    fn needs_publish_fires_on_significant_progress() {
        let prev = [(1_u64, TaskStatus::Running, 100_u64)];
        // 1.5 个百分点（15‰）超过阈值
        assert!(needs_publish(&prev, &[(1, TaskStatus::Running, 115)]));
        // 进度回退（重试把 progress 清零）必须立刻推，否则 UI 会停在旧进度
        assert!(needs_publish(&prev, &[(1, TaskStatus::Running, 0)]));
    }

    #[test]
    fn needs_publish_fires_on_status_and_task_set_change() {
        // 状态变化（哪怕进度没动）
        assert!(needs_publish(
            &[(1, TaskStatus::Running, 500)],
            &[(1, TaskStatus::Paused, 500)]
        ));
        // 新增任务
        assert!(needs_publish(
            &[(1, TaskStatus::Running, 500)],
            &[(1, TaskStatus::Running, 500), (2, TaskStatus::Pending, 0)]
        ));
        // 移除任务
        assert!(needs_publish(
            &[(1, TaskStatus::Running, 500), (2, TaskStatus::Pending, 0)],
            &[(1, TaskStatus::Running, 500)]
        ));
        // 数量不变但 id 换了（删一个加一个）
        assert!(needs_publish(
            &[(1, TaskStatus::Running, 500)],
            &[(2, TaskStatus::Running, 500)]
        ));
    }

    #[test]
    fn progress_marker_and_snapshot_are_order_independent() {
        let mut a = task(1, TaskDirection::Download, TaskStatus::Running);
        a.file_size = 200;
        a.bytes_done = 50;
        assert_eq!(progress_marker(&a), 250, "25.0% 应换算成 250‰");

        // 目录大小还在后台统计（file_size == 0）：退化成已传字节，仍能反映进度
        let mut b = task(2, TaskDirection::Upload, TaskStatus::Running);
        b.file_size = 0;
        b.bytes_done = 4096;
        assert_eq!(progress_marker(&b), 4096);

        // 超过总量的脏数据按总量截断，不越 1000‰
        let mut c = task(3, TaskDirection::Download, TaskStatus::Running);
        c.file_size = 100;
        c.bytes_done = 500;
        assert_eq!(progress_marker(&c), 1000);

        // 快照按 id 排序：任务表顺序变化不应造成"伪变化"
        let sorted = transfer_snapshot(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            sorted.iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(transfer_snapshot(&[c, a, b]), sorted);
    }

    // ---------------- 锁中毒：不再连锁 panic ----------------

    #[test]
    fn lock_or_recover_survives_poisoned_mutex() {
        // 刻意在持锁期间 panic 一次（stderr 里会出现一行 panic 信息），把锁真的毒化掉
        let m = Mutex::new(vec![1_u32, 2]);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().expect("首次取锁");
            panic!("刻意毒化互斥锁");
        }));
        assert!(poisoned.is_err(), "panic 应被 catch_unwind 捕获");
        assert!(m.is_poisoned(), "持有者 panic 后锁必须处于中毒状态");

        // 1) 中毒后仍然拿得到守卫，并且能改内部数据
        lock_or_recover(&m).push(3);
        // 2) 再取一次也不 panic：证明不会"连锁"
        assert_eq!(*lock_or_recover(&m), vec![1, 2, 3]);
    }

    // ---------------- 配置目录降级启动 ----------------

    fn unique_tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tianyi-{name}-{}", std::process::id()))
    }

    /// 构造"必然不能当目录打开"的路径：该位置先放一个普通文件
    fn blocked_dir(name: &str) -> (PathBuf, PathBuf) {
        let base = unique_tmp(name);
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("创建测试根目录");
        let blocked = base.join("occupied");
        fs::write(&blocked, b"x").expect("写入占位文件");
        (base, blocked)
    }

    #[test]
    fn pick_store_root_uses_primary_when_usable() {
        let primary = unique_tmp("store-primary");
        let _ = fs::remove_dir_all(&primary);
        let fallback = unique_tmp("store-primary-fb");
        let _ = fs::remove_dir_all(&fallback);

        match pick_store_root(Some(&primary), &fallback) {
            StoreRoot::Primary(root) => assert_eq!(root, primary),
            other => panic!("主目录可用时必须用主目录，实际 {other:?}"),
        }
        assert!(primary.is_dir(), "主目录应被创建出来");
        // 没有降级就不该顺手创建回退目录
        assert!(!fallback.exists(), "主目录可用时不应触碰回退目录");

        let _ = fs::remove_dir_all(&primary);
    }

    #[test]
    fn pick_store_root_falls_back_then_degrades_to_memory() {
        let (base, blocked) = blocked_dir("store-fallback");
        let fallback = unique_tmp("store-fallback-dir");
        let _ = fs::remove_dir_all(&fallback);

        // 主目录被普通文件占住（create_dir_all 必然失败）→ 退到回退目录
        match pick_store_root(Some(&blocked), &fallback) {
            StoreRoot::Fallback(root) => assert_eq!(root, fallback),
            other => panic!("主目录不可用时必须退到回退目录，实际 {other:?}"),
        }
        assert!(fallback.is_dir(), "回退目录应被创建出来");

        // 回退目录也不可用 → 内存态（全程不 panic）
        assert_eq!(
            pick_store_root(Some(&blocked), &blocked),
            StoreRoot::Memory,
            "两处都不可用时必须落到内存态而不是 panic"
        );

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&fallback);
    }

    // ---------------- 并发分片数钳制 ----------------

    #[test]
    fn clamp_threads_limits_to_one_through_eight() {
        assert_eq!(clamp_threads(0), 1, "0 线程无意义，抬到下限");
        assert_eq!(clamp_threads(1), 1);
        assert_eq!(clamp_threads(4), 4, "区间内的值必须原样保留");
        assert_eq!(clamp_threads(8), 8);
        assert_eq!(clamp_threads(9), 8, "超过上限钳到 8");
        assert_eq!(clamp_threads(-3), 1, "负数钳到下限");
        assert_eq!(clamp_threads(i32::MIN), 1);
        assert_eq!(clamp_threads(i32::MAX), 8);
    }

    // ---------------- 关窗：任务状态归一化 ----------------

    #[test]
    fn close_status_pauses_every_unfinished_task() {
        // 未结束的两种状态都要落成"已暂停"
        assert_eq!(
            close_status_for(TaskStatus::Running),
            Some(TaskStatus::Paused)
        );
        assert_eq!(
            close_status_for(TaskStatus::Pending),
            Some(TaskStatus::Paused),
            "排队中的任务关窗后没人会唤醒，必须一并落成已暂停"
        );
        // 已暂停 / 终态不需要改动（终态一旦改写就丢了"完成过"的事实）
        assert_eq!(close_status_for(TaskStatus::Paused), None);
        assert_eq!(close_status_for(TaskStatus::Completed), None);
        assert_eq!(close_status_for(TaskStatus::Failed), None);
        assert_eq!(close_status_for(TaskStatus::Canceled), None);
    }

    #[test]
    fn close_transitions_leave_no_running_or_pending_task() {
        let tasks = vec![
            task(1, TaskDirection::Download, TaskStatus::Running),
            task(2, TaskDirection::Upload, TaskStatus::Pending),
            task(3, TaskDirection::Download, TaskStatus::Paused),
            task(4, TaskDirection::Upload, TaskStatus::Completed),
            task(5, TaskDirection::Download, TaskStatus::Failed),
        ];
        let plan = close_transitions(&tasks);
        assert_eq!(
            plan,
            vec![(1, TaskStatus::Paused), (2, TaskStatus::Paused)],
            "只有 Pending/Running 需要改写"
        );

        // 按计划改写后的任务表：不应残留 Running / Pending
        let mut applied = tasks.clone();
        for (id, status) in plan {
            let t = applied.iter_mut().find(|t| t.id == id).expect("任务应存在");
            t.status = status;
        }
        assert!(
            applied
                .iter()
                .all(|t| !matches!(t.status, TaskStatus::Running | TaskStatus::Pending)),
            "关窗后不允许残留 Running / Pending（否则下次启动就是幽灵任务）"
        );
        assert_eq!(applied[2].status, TaskStatus::Paused, "已暂停保持不变");
        assert_eq!(applied[3].status, TaskStatus::Completed, "终态不受影响");
        assert_eq!(applied[4].status, TaskStatus::Failed, "终态不受影响");
    }

    /// 关窗窗口内的收尾写回：终态一律丢弃，只允许留在「已暂停」
    #[test]
    fn close_safe_status_never_overwrites_paused_during_shutdown() {
        // 不在关窗窗口：与改动前一致，任何目标状态都照常写回
        assert_eq!(
            close_safe_status(false, TaskStatus::Completed),
            Some(TaskStatus::Completed)
        );
        assert_eq!(
            close_safe_status(false, TaskStatus::Canceled),
            Some(TaskStatus::Canceled)
        );
        assert_eq!(
            close_safe_status(false, TaskStatus::Failed),
            Some(TaskStatus::Failed)
        );

        // 关窗中：三个终态全部丢弃（跳过写回，不覆盖 begin_close 归一化出的「已暂停」）
        assert_eq!(close_safe_status(true, TaskStatus::Completed), None);
        assert_eq!(close_safe_status(true, TaskStatus::Canceled), None);
        assert_eq!(close_safe_status(true, TaskStatus::Failed), None);
        // 「已暂停」本身就是关窗目标，放行
        assert_eq!(
            close_safe_status(true, TaskStatus::Paused),
            Some(TaskStatus::Paused)
        );
    }
}
