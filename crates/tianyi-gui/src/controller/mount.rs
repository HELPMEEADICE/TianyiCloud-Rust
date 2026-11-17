//! 云盘挂载页：云空间与挂载点选择、挂载/卸载、运行信息与错误显示
//!
//! # 线程模型（这一页最容易写错的地方）
//!
//! * 挂载与卸载**绝不在 UI 线程上做**：mount::prepare 要建会话（会发网络请求），
//!   MountHandle::unmount 要等后台线程退出（上限 10 秒）。两者都放在独立的
//!   std::thread 里，结果经 set_ui_state（跨线程投递到事件循环）回到 UI。
//! * 也**不能**放进 Backend::spawn：那是 tokio 运行时线程，而 prepare/mount 内部用的
//!   tianyi_mount::runtime::block_on 会断言"不在运行时线程内"并 panic。
//! * 状态采样（MountHandle::status）只读原子计数与一把短锁，直接在 1 秒定时器里做。
//! * 按钮的 busy 态有超时兜底：busy_since 超过 busy_limit(phase) 就解 busy 并报错。
//!   真结果晚到时（代际仍匹配但 busy 已复位）当场卸载并提示 —— 绝不让"迟到的挂载"
//!   变成一个没人管的后台挂载点。
//! * 关窗时必须卸载：shutdown_mount 把句柄搬进独立线程做卸载，UI 线程不等待；
//!   进程退出时 MountHandle::drop 还有一道幂等卸载 + 有界等待兜底。

use super::*;
use crate::app::{DriveRow, MountTexts};
use slint::{ModelRc, VecModel};
use std::time::Instant;
use tianyi_core::file::{format_size, format_speed};
use tianyi_core::Space;
use tianyi_mount::fs::FsOptions;
use tianyi_mount::mount::{MountHandle, MountOptions, MountStatus};

/// 盘符栅格每行的格数（页面侧按这个列数切分，见 ui/pages/mount.slint 的 DriveRow）
const LETTER_COLS: usize = 6;
/// 挂载等待上限：超过就解 busy 态（后台线程可以更慢，但界面不能一直卡着）
const MOUNT_LIMIT: Duration = Duration::from_secs(30);
/// 卸载等待上限（tianyi-mount 自己的线程 join 上限是 10 秒，这里留一倍余量）
const UNMOUNT_LIMIT: Duration = Duration::from_secs(20);
/// 运行信息刷新间隔
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// 挂载页的状态机档位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Phase {
    /// 没挂载（含挂载失败之后）
    #[default]
    Idle,
    /// 后台线程正在挂
    Mounting,
    /// 已挂载
    Mounted,
    /// 后台线程正在卸
    Unmounting,
}

/// 挂载页的会话内状态：**唯一事实来源**，UI 属性全部由它推出来
pub(super) struct MountState {
    /// 活着的挂载（None = 没挂载）
    handle: Option<Arc<MountHandle>>,
    /// 后台线程在途
    busy: bool,
    /// 在途起点（超时兜底用）
    busy_since: Option<Instant>,
    phase: Phase,
    /// 本次会话要显示的错误（用户可清除；下一次成功操作自动清）
    error: String,
    space: Space,
    /// 操作代际：只有最新一代的结果能写状态
    gen: u64,
    /// 平台后端可用性（None = 还没探测完）
    backend: Option<Result<(), String>>,
    /// 当前可用盘符
    letters: Vec<String>,
    /// 挂载点（用户可改）
    mount_point: String,
    /// 1 秒刷新链是否已经启动（避免重复起链）
    ticker_started: bool,
}

impl Default for MountState {
    fn default() -> Self {
        Self {
            handle: None,
            busy: false,
            busy_since: None,
            phase: Phase::Idle,
            error: String::new(),
            space: Space::Personal,
            gen: 0,
            backend: None,
            letters: Vec::new(),
            mount_point: String::new(),
            ticker_started: false,
        }
    }
}

/// MountStatus → 页面上的数值文案
///
/// 纯函数（除查词外不碰任何全局状态），单测直接喂一个手搓的 MountStatus 就能覆盖
/// "速度 / 累计流量 / 请求计数 / UNC 空值"这几类格式化，不需要真挂载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusView {
    pub speed: String,
    pub traffic: String,
    pub requests: String,
    pub errors: String,
    pub cached_dirs: String,
    pub unc: String,
}

/// 状态快照 → 展示文案
pub(super) fn status_view(status: &MountStatus) -> StatusView {
    let none = t("mount.value_none").to_string();
    // bytes_per_sec 是 f64 且可能为负（脏数据）：先夹到 0 再取整，绝不出现 "-3 B/s"
    let speed = format_speed(status.bytes_per_sec.max(0.0).round() as u64);
    StatusView {
        speed,
        traffic: format_size(status.bytes_total),
        requests: status.requests.to_string(),
        errors: status.errors.to_string(),
        cached_dirs: status.cached_dirs.to_string(),
        unc: if status.unc_path.trim().is_empty() {
            none
        } else {
            status.unc_path.clone()
        },
    }
}

/// 把盘符切成固定列数的栅格行
///
/// 页面侧不做下标运算（见 ui/pages/mount.slint 的 DriveRow 注释），切分只在这里发生：
/// 空列表得到空行集（页面显示"没有可用盘符"），不产生空行占位。
pub(super) fn letter_rows(letters: &[String], cols: usize) -> Vec<Vec<String>> {
    let cols = cols.max(1);
    letters.chunks(cols).map(|chunk| chunk.to_vec()).collect()
}

/// 当前档位对应的在途上限
pub(super) fn busy_limit(phase: Phase) -> Duration {
    if phase == Phase::Unmounting {
        UNMOUNT_LIMIT
    } else {
        MOUNT_LIMIT
    }
}

/// 挂载页用到的全部 i18n 键
///
/// 测试拿它逐个核对四份词表：漏一条就会在某个语种下静默回退成中文（甚至键名）。
/// **新增文案必须同时登记在这里**，否则覆盖测试管不住。
///
/// 只在测试里读（生产路径是 mount_texts() 里逐条查表），因此打上 cfg(test)：
/// 带进产线二进制只会多一份用不到的字符串表。
#[cfg(test)]
pub(super) const MOUNT_I18N_KEYS: &[&str] = &[
    "mount.title",
    "mount.section_target",
    "mount.space",
    "mount.space_personal",
    "mount.space_family",
    "mount.mount_point",
    "mount.free_letters",
    "mount.refresh_targets",
    "mount.no_free_letters",
    "mount.section_state",
    "mount.status",
    "mount.read_only",
    "mount.read_only_desc",
    "mount.zero_cache",
    "mount.zero_cache_desc",
    "mount.backend",
    "mount.backend_ready",
    "mount.backend_missing",
    "mount.mount",
    "mount.unmount",
    "mount.open_explorer",
    "mount.section_stats",
    "mount.speed",
    "mount.traffic",
    "mount.requests",
    "mount.errors",
    "mount.cached_dirs",
    "mount.unc",
    "mount.clear_error",
    "mount.winfsp_hint",
    "mount.point_desc_win",
    "mount.point_desc_unix",
    "mount.point_ph_win",
    "mount.point_ph_unix",
    "mount.status_idle",
    "mount.status_mounting",
    "mount.status_unmounting",
    "mount.status_mounted",
    "mount.error_mount",
    "mount.error_unmount",
    "mount.error_login_required",
    "mount.error_no_target",
    "mount.error_timeout",
    "mount.error_late_unmounted",
    "mount.error_open",
    "mount.toast_mounted",
    "mount.toast_unmounted",
    "mount.value_none",
];

/// 页面静态文案：一次查表填满，切语言时重推一次即可
fn mount_texts() -> MountTexts {
    MountTexts {
        title: t("mount.title").into(),
        section_target: t("mount.section_target").into(),
        space: t("mount.space").into(),
        space_personal: t("mount.space_personal").into(),
        space_family: t("mount.space_family").into(),
        mount_point: t("mount.mount_point").into(),
        free_letters: t("mount.free_letters").into(),
        refresh_targets: t("mount.refresh_targets").into(),
        no_free_letters: t("mount.no_free_letters").into(),
        section_state: t("mount.section_state").into(),
        status: t("mount.status").into(),
        read_only: t("mount.read_only").into(),
        read_only_desc: t("mount.read_only_desc").into(),
        zero_cache: t("mount.zero_cache").into(),
        zero_cache_desc: t("mount.zero_cache_desc").into(),
        backend: t("mount.backend").into(),
        mount: t("mount.mount").into(),
        unmount: t("mount.unmount").into(),
        open_explorer: t("mount.open_explorer").into(),
        section_stats: t("mount.section_stats").into(),
        speed: t("mount.speed").into(),
        traffic: t("mount.traffic").into(),
        requests: t("mount.requests").into(),
        errors: t("mount.errors").into(),
        cached_dirs: t("mount.cached_dirs").into(),
        unc: t("mount.unc").into(),
        clear_error: t("mount.clear_error").into(),
        winfsp_hint: t("mount.winfsp_hint").into(),
    }
}

/// 一次后台挂载的结局（在锁外处理：卸载不能持锁做）
enum MountSettle {
    /// 采用这次挂载
    Applied,
    /// 失败：错误已写进状态
    Failed,
    /// 迟到（已判超时）：立刻卸载，不进状态
    Late(Option<Arc<MountHandle>>),
    /// 过期（被更晚的操作取代）：句柄直接丢掉（Drop 自带幂等卸载）
    Stale(Option<Arc<MountHandle>>),
}

impl Controller {
    // ==================================================================
    // 初始化与推送
    // ==================================================================
    /// 挂载页初始化：默认挂载点、盘符、后端探测、1 秒刷新链
    pub(super) fn init_mount(self: &Arc<Self>) {
        let point = tianyi_mount::mount::suggested_mount_point();
        {
            let mut st = lock_or_recover(&self.mount);
            if st.mount_point.trim().is_empty() {
                st.mount_point = point.clone();
            }
        }
        self.set_ui_state(move |ui| ui.set_mount_point(SharedString::from(point)));
        self.refresh_mount_targets();
        self.check_mount_backend();
        self.push_mount_page();
        self.start_mount_ticker();
    }

    /// 整页重推（初始化与切语言走这里）
    pub(super) fn push_mount_page(self: &Arc<Self>) {
        let texts = mount_texts();
        let (desc, placeholder) = mount_point_hints();
        self.set_ui_state(move |ui| {
            ui.set_mount_texts(texts);
            ui.set_mount_point_desc(SharedString::from(desc));
            ui.set_mount_point_placeholder(SharedString::from(placeholder));
        });
        self.push_mount_targets();
        self.push_mount_state();
        self.push_mount_stats();
    }

    /// 可用盘符重推（模型在 UI 线程上现建：VecModel 不必跨线程搬）
    fn push_mount_targets(self: &Arc<Self>) {
        let letters = lock_or_recover(&self.mount).letters.clone();
        self.set_ui_state(move |ui| {
            let rows: Vec<DriveRow> = letter_rows(&letters, LETTER_COLS)
                .into_iter()
                .map(|cells| DriveRow {
                    cells: ModelRc::new(VecModel::from(
                        cells
                            .into_iter()
                            .map(SharedString::from)
                            .collect::<Vec<SharedString>>(),
                    )),
                })
                .collect();
            ui.set_mount_letter_rows(ModelRc::new(VecModel::from(rows)));
        });
    }

    /// 只推"状态类"属性：在途标志、状态文案、后端可用性、只读/零缓存、错误
    ///
    /// 刻意**不推挂载点**：它是用户可编辑的输入框，每秒回写一次会把正在输入的内容顶掉。
    fn push_mount_state(self: &Arc<Self>) {
        let snapshot = {
            let st = lock_or_recover(&self.mount);
            let state_text = state_text_of(st.phase, st.handle.as_ref());
            let (backend_ok, backend_text) = backend_of(&st);
            let detail = match &st.backend {
                Some(Err(detail)) => detail.clone(),
                _ => String::new(),
            };
            // 只读与零磁盘缓存的**真实值**：挂载中取 MountStatus 的同名字段，
            // 没挂载时取 MountOptions 的默认值（恒为 true）——都不写死文案。
            let (read_only, zero_cache) = match st.handle.as_ref() {
                Some(handle) => {
                    let status = handle.status();
                    (status.read_only, status.zero_disk_cache)
                }
                None => {
                    let options = MountOptions::default();
                    (options.read_only, options.zero_disk_cache)
                }
            };
            (
                st.phase == Phase::Mounting,
                st.phase == Phase::Unmounting,
                st.phase == Phase::Mounted && st.handle.is_some(),
                state_text,
                backend_ok,
                backend_text,
                detail,
                st.error.clone(),
                read_only,
                zero_cache,
            )
        };
        let (
            mounting,
            unmounting,
            active,
            state_text,
            backend_ok,
            backend_text,
            detail,
            error,
            read_only,
            zero_cache,
        ) = snapshot;
        self.set_ui_state(move |ui| {
            ui.set_mounting(mounting);
            ui.set_unmounting(unmounting);
            ui.set_mount_active(active);
            ui.set_mount_state_text(SharedString::from(state_text));
            ui.set_mount_backend_ok(backend_ok);
            ui.set_mount_backend_text(SharedString::from(backend_text));
            ui.set_mount_backend_detail(SharedString::from(detail));
            ui.set_mount_error_text(SharedString::from(error));
            ui.set_mount_read_only(read_only);
            ui.set_mount_zero_cache(zero_cache);
        });
    }

    /// 运行信息（1 秒一次）：速度、累计流量、请求/失败数、目录缓存、UNC
    fn push_mount_stats(self: &Arc<Self>) {
        let view = {
            let st = lock_or_recover(&self.mount);
            st.handle
                .as_ref()
                .map(|handle| status_view(&handle.status()))
        };
        let none = t("mount.value_none").to_string();
        let view = view.unwrap_or(StatusView {
            speed: none.clone(),
            traffic: none.clone(),
            requests: none.clone(),
            errors: none.clone(),
            cached_dirs: none.clone(),
            unc: none,
        });
        self.set_ui_state(move |ui| {
            ui.set_mount_speed_text(SharedString::from(view.speed));
            ui.set_mount_traffic_text(SharedString::from(view.traffic));
            ui.set_mount_requests_text(SharedString::from(view.requests));
            ui.set_mount_errors_text(SharedString::from(view.errors));
            ui.set_mount_cached_dirs_text(SharedString::from(view.cached_dirs));
            ui.set_mount_unc_text(SharedString::from(view.unc));
        });
    }

    // ==================================================================
    // 后端探测与盘符扫描
    // ==================================================================
    /// 探测平台后端（WinFsp / FUSE）
    ///
    /// 放后台线程：Windows 上要走一次 LoadLibraryW + 注册表查询，Linux 上要 stat
    /// /dev/fuse —— 都不该占 UI 线程，哪怕只要几毫秒。
    pub(super) fn check_mount_backend(self: &Arc<Self>) {
        let c = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("tianyi-gui-mount-probe".into())
            .spawn(move || {
                let result = tianyi_mount::mount::backend_available();
                c.mount_backend_probed(result);
            });
        if let Err(e) = spawned {
            log::warn!("后端探测线程创建失败：{e}");
        }
    }

    /// 后端探测结果落状态
    fn mount_backend_probed(self: &Arc<Self>, result: Result<(), String>) {
        {
            let mut st = lock_or_recover(&self.mount);
            st.backend = Some(result);
        }
        self.push_mount_state();
    }

    /// 重新扫描可用盘符（进入页面、"重新扫描"按钮、挂载结束都会调）
    pub(super) fn refresh_mount_targets(self: &Arc<Self>) {
        let letters = tianyi_mount::mount::free_drive_letters();
        {
            let mut st = lock_or_recover(&self.mount);
            st.letters = letters;
        }
        self.push_mount_targets();
    }

    // ==================================================================
    // 挂载 / 卸载
    // ==================================================================
    /// 「挂载」：校验前置条件 → 起后台线程 → 结果回到 mount_finished
    pub(super) fn request_mount(self: &Arc<Self>) {
        // 未登录：直接说清楚，不排线程（挂载本身也要用 accounts.json 里的凭据）
        if self.client().is_none() {
            self.set_mount_error(t("mount.error_login_required").to_string());
            return;
        }
        let (gen, options) = {
            let mut st = lock_or_recover(&self.mount);
            if st.busy || st.handle.is_some() {
                return;
            }
            if st.mount_point.trim().is_empty() {
                // 界面上按钮在空挂载点时本来就是禁用的，这里兜住其它入口（将来可能的快捷键）
                drop(st);
                self.set_mount_error(t("mount.error_no_target").to_string());
                return;
            }
            // 后端缺失就别把线程派出去：安装指引已经在页面上，这里只把原因抬成错误
            if let Some(Err(detail)) = st.backend.clone() {
                // 先放锁再报错：set_mount_error 自己会重新取锁推属性
                drop(st);
                self.set_mount_error(detail);
                return;
            }
            st.busy = true;
            st.busy_since = Some(Instant::now());
            st.phase = Phase::Mounting;
            st.gen += 1;
            st.error.clear();
            let options = MountOptions {
                space: st.space,
                // None = 用配置里的 last_account（GUI 只有一个当前账号）
                account: None,
                mount_point: st.mount_point.clone(),
                unc_prefix: None,
                read_only: true,
                zero_disk_cache: true,
                fs: FsOptions::default(),
            };
            (st.gen, options)
        };
        self.push_mount_state();
        self.push_mount_stats();

        let c = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("tianyi-gui-mount".into())
            .spawn(move || {
                let result = (|| -> Result<Arc<MountHandle>, String> {
                    let (store, _degrade) = ConfigStore::open();
                    let (_session, fs) =
                        tianyi_mount::mount::prepare(store, &options).map_err(|e| e.to_string())?;
                    let handle =
                        tianyi_mount::mount::mount(fs, options).map_err(|e| e.to_string())?;
                    Ok(Arc::new(handle))
                })();
                c.mount_finished(gen, result);
            });
        if let Err(e) = spawned {
            log::warn!("挂载线程创建失败：{e}");
            {
                let mut st = lock_or_recover(&self.mount);
                st.busy = false;
                st.busy_since = None;
                st.phase = Phase::Idle;
                st.error = tf("mount.error_mount", &[("e", &e)]);
            }
            self.push_mount_state();
        }
    }

    /// 后台挂载的结果（可能在判超时之后才到）
    fn mount_finished(self: &Arc<Self>, gen: u64, result: Result<Arc<MountHandle>, String>) {
        let settle = {
            let mut st = lock_or_recover(&self.mount);
            if st.gen != gen {
                MountSettle::Stale(result.ok())
            } else if !st.busy || st.phase != Phase::Mounting {
                // 已经判过超时：句柄不能留，立刻放掉
                MountSettle::Late(result.ok())
            } else {
                st.busy = false;
                st.busy_since = None;
                match result {
                    Ok(handle) => {
                        st.handle = Some(handle);
                        st.phase = Phase::Mounted;
                        st.error.clear();
                        MountSettle::Applied
                    }
                    Err(e) => {
                        st.phase = Phase::Idle;
                        st.error = tf("mount.error_mount", &[("e", &e)]);
                        MountSettle::Failed
                    }
                }
            }
        };
        match settle {
            MountSettle::Applied => {
                let point = lock_or_recover(&self.mount)
                    .handle
                    .as_ref()
                    .map(|handle| handle.mount_point())
                    .unwrap_or_default();
                self.refresh_mount_targets();
                self.push_mount_state();
                self.push_mount_stats();
                self.notice("success", tf("mount.toast_mounted", &[("point", &point)]));
            }
            MountSettle::Failed => {
                self.refresh_mount_targets();
                self.push_mount_state();
                self.push_mount_stats();
            }
            MountSettle::Late(Some(handle)) => {
                // 迟到的成功：当场卸载，并把这件事告诉用户（否则盘符会莫名消失）
                let point = handle.mount_point();
                let spawned = std::thread::Builder::new()
                    .name("tianyi-gui-mount-late".into())
                    .spawn(move || {
                        let _ = handle.unmount();
                    });
                if let Err(e) = spawned {
                    log::warn!("迟到挂载的卸载线程创建失败：{e}");
                }
                self.set_mount_error(tf("mount.error_late_unmounted", &[("point", &point)]));
            }
            MountSettle::Late(None) => self.push_mount_state(),
            MountSettle::Stale(Some(handle)) => {
                let spawned = std::thread::Builder::new()
                    .name("tianyi-gui-mount-stale".into())
                    .spawn(move || drop(handle));
                if let Err(e) = spawned {
                    log::warn!("过期挂载的清理线程创建失败：{e}");
                }
            }
            MountSettle::Stale(None) => {}
        }
    }

    /// 「卸载」：把句柄搬进后台线程，UI 立刻回到"在途"状态
    pub(super) fn request_unmount(self: &Arc<Self>) {
        let (gen, handle) = {
            let mut st = lock_or_recover(&self.mount);
            if st.busy {
                return;
            }
            let Some(handle) = st.handle.take() else {
                return;
            };
            st.busy = true;
            st.busy_since = Some(Instant::now());
            st.phase = Phase::Unmounting;
            st.gen += 1;
            (st.gen, handle)
        };
        self.push_mount_state();
        self.push_mount_stats();

        let c = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("tianyi-gui-unmount".into())
            .spawn(move || {
                let result = handle.unmount().map_err(|e| e.to_string());
                drop(handle);
                c.unmount_finished(gen, result);
            });
        if let Err(e) = spawned {
            log::warn!("卸载线程创建失败：{e}");
            {
                let mut st = lock_or_recover(&self.mount);
                st.busy = false;
                st.busy_since = None;
                st.phase = Phase::Idle;
                st.error = tf("mount.error_unmount", &[("e", &e)]);
            }
            self.push_mount_state();
        }
    }

    /// 后台卸载的结果
    fn unmount_finished(self: &Arc<Self>, gen: u64, result: Result<(), String>) {
        let ok = {
            let mut st = lock_or_recover(&self.mount);
            if st.gen != gen {
                return;
            }
            st.busy = false;
            st.busy_since = None;
            st.phase = Phase::Idle;
            match result {
                Ok(()) => {
                    st.error.clear();
                    true
                }
                Err(e) => {
                    st.error = tf("mount.error_unmount", &[("e", &e)]);
                    false
                }
            }
        };
        self.refresh_mount_targets();
        self.push_mount_state();
        self.push_mount_stats();
        if ok {
            self.notice("info", t("mount.toast_unmounted"));
        }
    }

    // ==================================================================
    // 1 秒刷新链
    // ==================================================================
    /// 起刷新链（只起一次）
    ///
    /// 用 Timer::single_shot 自续期而不是 Timer::start：Timer 句柄必须活在事件循环
    /// 线程上（lib.rs 里的两个定时器就是靠 run() 的局部变量活着的），而控制器要跨线程
    /// 共享，没有地方安放一个非 Send 的句柄。single_shot 的定时器由平台持有，回调里再
    /// 续一次就是一条链；窗口销毁后链自己停（回调里先 upgrade，拿不到就 return）。
    pub(super) fn start_mount_ticker(self: &Arc<Self>) {
        let already = {
            let mut st = lock_or_recover(&self.mount);
            std::mem::replace(&mut st.ticker_started, true)
        };
        if already {
            return;
        }
        let c = Arc::clone(self);
        // 定时器必须建在事件循环线程上：这里回到 UI 线程再起第一环
        self.set_ui_state(move |_ui| c.schedule_mount_tick());
    }

    fn schedule_mount_tick(self: &Arc<Self>) {
        let c = Arc::clone(self);
        slint::Timer::single_shot(REFRESH_INTERVAL, move || {
            // 窗口已经销毁：链到此为止，不再续期
            if c.upgrade_on_ui_thread().is_none() {
                return;
            }
            c.on_mount_tick();
            c.schedule_mount_tick();
        });
    }

    /// 一秒一次的采样：超时兜底 + 运行信息 + last_error
    fn on_mount_tick(self: &Arc<Self>) {
        let timed_out = {
            let mut st = lock_or_recover(&self.mount);
            let limit = busy_limit(st.phase);
            match st.busy_since {
                Some(since) if since.elapsed() >= limit => {
                    st.busy = false;
                    st.busy_since = None;
                    st.phase = Phase::Idle;
                    st.error = t("mount.error_timeout").to_string();
                    true
                }
                _ => false,
            }
        };
        if timed_out {
            log::warn!(
                "挂载操作超过上限（挂载 {MOUNT_LIMIT:?} / 卸载 {UNMOUNT_LIMIT:?}），已解 busy 态"
            );
            self.refresh_mount_targets();
            self.push_mount_state();
            self.push_mount_stats();
            return;
        }

        // 平台侧已经停了（用户从资源管理器断开、后端进程退出）：状态回落到未挂载
        let stopped = {
            let mut st = lock_or_recover(&self.mount);
            match st.handle.as_ref() {
                Some(handle) if !handle.status().mounted => {
                    st.handle = None;
                    st.phase = Phase::Idle;
                    true
                }
                _ => false,
            }
        };
        if stopped {
            log::info!("挂载点已由平台侧停止，状态回落为未挂载");
            self.refresh_mount_targets();
            self.push_mount_state();
        }

        // 挂载期的读取错误：MountStatus.last_error 有值就抬到页面（同一句不重复抬）
        let last_error = {
            let st = lock_or_recover(&self.mount);
            st.handle
                .as_ref()
                .and_then(|handle| handle.status().last_error)
        };
        if let Some(e) = last_error {
            let changed = {
                let mut st = lock_or_recover(&self.mount);
                if st.error == e {
                    false
                } else {
                    st.error = e;
                    true
                }
            };
            if changed {
                self.push_mount_state();
            }
        }
        self.push_mount_stats();
    }

    // ==================================================================
    // 用户交互
    // ==================================================================
    /// 切换云空间（0 = 个人云，1 = 家庭云）
    pub(super) fn set_mount_space(self: &Arc<Self>, index: i32) {
        let space = if index == 1 {
            Space::Family
        } else {
            Space::Personal
        };
        lock_or_recover(&self.mount).space = space;
        let index = if space == Space::Family { 1 } else { 0 };
        self.set_ui_state(move |ui| ui.set_mount_space_index(index));
    }

    /// 用户改了挂载点（输入框或盘符格子）
    pub(super) fn set_mount_point(&self, point: String) {
        lock_or_recover(&self.mount).mount_point = point;
    }

    /// 用户清掉错误显示（只影响本次会话的展示，不动挂载状态）
    pub(super) fn clear_mount_error(self: &Arc<Self>) {
        lock_or_recover(&self.mount).error.clear();
        self.push_mount_state();
    }

    /// 进入挂载页时重新探测后端（用户可能刚装完 WinFsp）
    pub(super) fn mount_page_opened(self: &Arc<Self>) {
        self.check_mount_backend();
        self.refresh_mount_targets();
        self.push_mount_state();
    }

    /// 在系统文件管理器里打开挂载点（仅挂载成功可用）
    pub(super) fn open_mount_point(self: &Arc<Self>) {
        let point = {
            let st = lock_or_recover(&self.mount);
            st.handle.as_ref().map(|handle| handle.mount_point())
        };
        let Some(point) = point else {
            return;
        };
        if let Err(e) = open_in_file_manager(&point) {
            self.set_mount_error(tf("mount.error_open", &[("e", &e)]));
        }
    }

    /// 关窗路径：把挂载搬进后台线程卸载，UI 线程不等待
    ///
    /// 不能直接 drop：MountHandle::drop 会 join 后台线程（上限 10 秒），窗口会僵住。
    /// 线程也建不出来时退回同步 drop（阻塞但正确，比留下一个孤儿挂载点强）。
    pub(super) fn shutdown_mount(self: &Arc<Self>) {
        let handle = lock_or_recover(&self.mount).handle.take();
        let Some(handle) = handle else {
            return;
        };
        log::info!("关窗：卸载挂载点 {}", handle.mount_point());
        let spawned = std::thread::Builder::new()
            .name("tianyi-gui-mount-exit".into())
            .spawn(move || {
                let _ = handle.unmount();
            });
        if let Err(e) = spawned {
            log::warn!("关窗卸载线程创建失败，改由 Drop 卸载：{e}");
        }
    }

    /// 写一条错误到页面（"提交类"失败的统一入口）
    fn set_mount_error(self: &Arc<Self>, text: String) {
        lock_or_recover(&self.mount).error = text;
        self.push_mount_state();
    }
}

/// 挂载点的说明与占位文案（两平台不同）
fn mount_point_hints() -> (String, String) {
    if cfg!(windows) {
        (
            t("mount.point_desc_win").to_string(),
            t("mount.point_ph_win").to_string(),
        )
    } else {
        (
            t("mount.point_desc_unix").to_string(),
            t("mount.point_ph_unix").to_string(),
        )
    }
}

/// 档位 → 状态文案
fn state_text_of(phase: Phase, handle: Option<&Arc<MountHandle>>) -> String {
    match phase {
        Phase::Idle => t("mount.status_idle").to_string(),
        Phase::Mounting => t("mount.status_mounting").to_string(),
        Phase::Unmounting => t("mount.status_unmounting").to_string(),
        Phase::Mounted => {
            let point = handle
                .map(|handle| handle.mount_point())
                .unwrap_or_else(|| t("mount.value_none").to_string());
            tf("mount.status_mounted", &[("point", &point)])
        }
    }
}

/// 后端可用性 → (是否可用, 展示文案)
fn backend_of(st: &MountState) -> (bool, String) {
    // 已挂载时显示平台真实后端名（WinFsp / FUSE），否则显示探测结论
    if let Some(handle) = st.handle.as_ref() {
        let backend = handle.status().backend;
        if !backend.trim().is_empty() {
            return (true, backend);
        }
    }
    match &st.backend {
        Some(Ok(())) => (true, t("mount.backend_ready").to_string()),
        Some(Err(_)) => (false, t("mount.backend_missing").to_string()),
        // 还没探测完：不禁用按钮（几毫秒后就有结论），避免界面闪一下"不可用"
        None => (true, t("mount.value_none").to_string()),
    }
}

/// 用系统文件管理器打开挂载点
fn open_in_file_manager(point: &str) -> std::result::Result<(), String> {
    #[cfg(windows)]
    {
        // 只给盘符时要补上根分隔符，否则 explorer 会按"当前目录下的 X:"理解
        let target = if point.len() == 2 && point.ends_with(':') {
            format!("{point}\\")
        } else {
            point.to_string()
        };
        std::process::Command::new("explorer.exe")
            .arg(&target)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(point)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(point)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = point;
        Err("unsupported platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手搓一份 MountStatus：字段全是公开的，不需要真挂载就能覆盖格式化
    fn status_fixture() -> MountStatus {
        MountStatus {
            mounted: true,
            mount_point: "X:".to_string(),
            unc_path: r"\\TianyiCloud-alice\Personal".to_string(),
            label: "TianyiCloud".to_string(),
            space: "personal".to_string(),
            username: "alice".to_string(),
            read_only: true,
            zero_disk_cache: true,
            backend: "WinFsp".to_string(),
            started_at_unix: 1_700_000_000,
            bytes_total: 5 * 1024 * 1024,
            requests: 3,
            errors: 0,
            bytes_per_sec: 1.5 * 1024.0 * 1024.0,
            cached_dirs: 2,
            memory_budget: 8 * 1024 * 1024,
            last_error: None,
        }
    }

    /// MountStatus → 页面属性：每个字段都取真实值，不写死
    #[test]
    fn status_view_maps_every_field() {
        let view = status_view(&status_fixture());
        assert_eq!(
            view.speed, "1.50 MB/s",
            "bytes_per_sec 要按 B/KB/MB 档位格式化"
        );
        assert_eq!(view.traffic, "5.00 MB", "bytes_total 是累计流量");
        assert_eq!(view.requests, "3");
        assert_eq!(view.errors, "0");
        assert_eq!(view.cached_dirs, "2");
        assert_eq!(view.unc, status_fixture().unc_path);
    }

    /// 未挂载 / UNC 为空：显示占位符而不是空串（空串会让这一行看起来像没渲染）
    #[test]
    fn status_view_uses_placeholder_for_empty_values() {
        let mut status = status_fixture();
        status.unc_path = String::new();
        // 负速度是脏数据，必须夹到 0 而不是显示负数
        status.bytes_per_sec = -12.0;
        let view = status_view(&status);
        assert_eq!(view.unc, t("mount.value_none"));
        assert_eq!(view.speed, "0 B/s");
    }

    /// 盘符按固定列数切分：页面侧不做下标运算，最后一行的空格不存在
    #[test]
    fn letter_rows_split_into_fixed_columns() {
        let letters: Vec<String> = (0..8u8)
            .map(|i| format!("{}:", (b'D' + i) as char))
            .collect();
        let rows = letter_rows(&letters, LETTER_COLS);
        assert_eq!(rows.len(), 2, "8 个盘符按 6 列切是两行");
        assert_eq!(rows[0].len(), 6);
        assert_eq!(rows[1], vec!["J:".to_string(), "K:".to_string()]);

        // 空列表 → 空行集（页面显示「没有可用盘符」，而不是一行空按钮）
        assert!(letter_rows(&[], LETTER_COLS).is_empty());
        // 列数下限保护：0 会被 chunks 当成越界
        assert_eq!(letter_rows(&letters, 0).len(), 8);
    }

    /// 在途上限按档位取：卸载比挂载宽限（卸载要等后台线程退出）
    #[test]
    fn busy_limit_follows_phase() {
        assert_eq!(busy_limit(Phase::Mounting), MOUNT_LIMIT);
        assert_eq!(busy_limit(Phase::Idle), MOUNT_LIMIT);
        assert_eq!(busy_limit(Phase::Unmounting), UNMOUNT_LIMIT);
        assert!(UNMOUNT_LIMIT >= tianyi_mount::mount::UNMOUNT_TIMEOUT);
    }

    /// 四份词表必须覆盖本页用到的每一条文案，且简中表里没有本页之外的 mount.* 漏登记
    #[test]
    fn mount_i18n_keys_exist_in_every_table() {
        const LANGS: [&str; 4] = ["zh-Hans", "en", "zh-Hant", "ja"];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("i18n")
            .join("rs");
        let mut tables: Vec<(&str, std::collections::BTreeMap<String, String>)> = Vec::new();
        for lang in LANGS {
            let path = dir.join(format!("{lang}.json"));
            let src = std::fs::read_to_string(&path).expect("词表应存在");
            let table: std::collections::BTreeMap<String, String> =
                serde_json::from_str(&src).expect("词表应是键到字符串的 JSON 对象");
            tables.push((lang, table));
        }
        let declared: std::collections::BTreeSet<&str> = MOUNT_I18N_KEYS.iter().copied().collect();
        for (lang, table) in &tables {
            for key in MOUNT_I18N_KEYS {
                match table.get(*key) {
                    None => panic!("词表 {lang} 缺少挂载页文案 {key}"),
                    Some(value) if value.trim().is_empty() => {
                        panic!("词表 {lang} 的 {key} 是空串")
                    }
                    Some(_) => {}
                }
            }
            // 反过来：词表里属于本页的键必须都登记进 MOUNT_I18N_KEYS，否则测试管不住新增文案
            let undeclared: Vec<&str> = table
                .keys()
                .filter(|key| key.starts_with("mount."))
                .map(String::as_str)
                .filter(|key| !declared.contains(key))
                .collect();
            assert!(
                undeclared.is_empty(),
                "词表 {lang} 里有挂载页文案没登记进 MOUNT_I18N_KEYS：{undeclared:?}"
            );
        }
    }

    /// 挂载页的 .slint 里不许出现界面文字：中文注释可以，字符串字面量不行
    ///
    /// 这是「文案全部经 i18n 词表」这条约束的守门员：只要有人往页面里塞一句中文字面量，
    /// 英文界面上就会冒出中文，而编译、运行、其它测试全绿。
    #[test]
    fn mount_page_has_no_hardcoded_ui_text() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ui")
            .join("pages")
            .join("mount.slint");
        let src = std::fs::read_to_string(&path).expect("挂载页应存在");
        assert!(
            !src.contains("@tr("),
            "挂载页的文案全部来自 i18n 词表（Rust 侧推来），不允许再出现 @tr"
        );

        let mut literals: Vec<String> = Vec::new();
        for line in src.lines() {
            // 整行注释里的引号不是界面文字
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut current = String::new();
            let mut in_string = false;
            for ch in line.chars() {
                if ch == '"' {
                    if in_string {
                        literals.push(std::mem::take(&mut current));
                    }
                    in_string = !in_string;
                } else if in_string {
                    current.push(ch);
                }
            }
        }
        assert!(!literals.is_empty(), "扫描不到字符串字面量：扫描器需要更新");
        let offenders: Vec<&String> = literals
            .iter()
            .filter(|literal| {
                literal
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "挂载页的字符串字面量里出现了中文（会绕过 i18n）：{offenders:?}"
        );
    }
}
