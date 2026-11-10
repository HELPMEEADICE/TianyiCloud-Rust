//! 登录与账号、主题与设置页、配置存储（含配置目录降级）。
use super::*;
use crate::app::{MainWindow, Theme};
use crate::backend::invoke_ui;
use slint::{ComponentHandle, SharedString, Weak};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tianyi_core::api::{self, LoginNotifier};
use tianyi_core::client::TianyiClient;
use tianyi_core::config::{Accounts, AppConfig, AppStore, ThemeMode};
use tianyi_core::file;
use tianyi_core::models::{AccountConfig, TokenInfo};
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
pub(super) fn qr_state_of(status: &str) -> &'static str {
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

impl Controller {
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
        *lock_or_recover(&self.theme_mode) = mode;
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
        if *lock_or_recover(&self.theme_mode) != ThemeMode::System {
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

    pub(super) fn set_theme_index(self: &Arc<Self>, idx: i32) {
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

    pub(super) fn set_reduce_motion(self: &Arc<Self>, on: bool) {
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.reduce_motion = on;
            if let Err(e) = self.store.save_config(&cfg) {
                log::warn!("save reduce_motion: {e}");
            }
        }
        self.apply_theme();
    }

    /// 设置页「并发分片数」：钳制到 1..=8 后写回配置
    ///
    /// 只改 `AppConfig`，**只对之后新启动的传输任务生效**：正在跑的任务在
    /// `run_transfer` 里已经拷走了 `thread_count`，这里刻意不去动它们——中途改变分片数
    /// 会让已建立的分片会话失去对应关系。
    pub(super) fn set_thread_counts(self: &Arc<Self>, download: i32, upload: i32) {
        let download_thread = clamp_threads(download);
        let upload_thread = clamp_threads(upload);
        let mut cfg = self
            .store
            .load_config()
            .unwrap_or_else(|_| AppConfig::default_config());
        cfg.download_thread = download_thread;
        cfg.upload_thread = upload_thread;
        let saved = match self.store.save_config(&cfg) {
            Ok(()) => true,
            Err(e) => {
                self.notice("error", tf("error.save_threads_failed", &[("e", &e)]));
                false
            }
        };
        // 回填"真正生效的数字"：连点 +/− 越过边界时停在 1 / 8，而不是显示越界值
        let (d, u) = (download_thread as i32, upload_thread as i32);
        self.set_ui_state(move |ui| {
            ui.set_download_thread_count(d);
            ui.set_upload_thread_count(u);
            ui.set_download_thread(SharedString::from(d.to_string()));
            ui.set_upload_thread(SharedString::from(u.to_string()));
        });
        if saved {
            if self.store.is_persistent() {
                self.notice(
                    "success",
                    tf(
                        "toast.threads_saved",
                        &[("download", &download_thread), ("upload", &upload_thread)],
                    ),
                );
            } else {
                // 内存态：值在本进程内生效，但没有落盘，必须如实说明
                self.notice("warning", t("toast.threads_saved_memory"));
            }
        }
    }

    pub(super) fn choose_download_dir(self: &Arc<Self>) {
        let current = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| PathBuf::from("."));
        let Some(dir) = rfd::FileDialog::new()
            .set_title(t("dialog.choose_download_dir_title"))
            .set_directory(&current)
            .pick_folder()
        else {
            return;
        };
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.download_dir = dir.clone();
            if let Err(e) = self.store.save_config(&cfg) {
                self.notice("error", tf("error.save_config_failed", &[("e", &e)]));
                return;
            }
        }
        let text = dir.display().to_string();
        self.set_ui_state(move |ui| ui.set_download_dir(SharedString::from(text)));
        self.notice("success", t("toast.download_dir_updated"));
    }

    pub(super) fn open_config_dir(self: &Arc<Self>) {
        let dir = self.store.root();
        let _ = std::fs::create_dir_all(dir.as_path());
        if let Err(e) = file::open_with_system(dir.as_path()) {
            self.notice("error", tf("error.open_config_dir_failed", &[("e", &e)]));
        }
    }

    /// 打开日志目录（设置页「关于」分组里的那一行）
    ///
    /// 目录由 `crate::logging` 解析（与日志初始化用的是同一个路径），用户可能一次都
    /// 没产生过日志文件，所以打开前先补建目录，避免系统弹“找不到路径”。
    pub(super) fn open_log_dir(self: &Arc<Self>) {
        let dir = crate::logging::log_dir();
        let _ = std::fs::create_dir_all(dir.as_path());
        if let Err(e) = file::open_with_system(dir.as_path()) {
            self.notice("error", tf("error.open_log_dir_failed", &[("e", &e)]));
        }
    }

    pub(super) fn open_download_dir(self: &Arc<Self>) {
        let dir = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| PathBuf::from("."));
        let _ = std::fs::create_dir_all(dir.as_path());
        if let Err(e) = file::open_with_system(dir.as_path()) {
            self.notice("error", tf("error.open_download_dir_failed", &[("e", &e)]));
        }
    }

    // ==================================================================
    // 登录
    // ==================================================================
    pub(super) fn handle_switch_mode(self: &Arc<Self>, qr: bool) {
        // 切换模式会终止正在进行的二维码轮询与密码登录结果回填
        let gen = self.login_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_ui_state(move |ui| {
            ui.set_qr_mode(qr);
            ui.set_login_error(SharedString::default());
            if qr {
                ui.set_qr_status(SharedString::from(t("status.qr_fetching")));
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

    pub(super) fn handle_qr_login(self: &Arc<Self>) {
        let gen = self.login_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_ui_state(|ui| {
            ui.set_login_error(SharedString::default());
            ui.set_qr_status(SharedString::from(t("status.qr_fetching")));
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
                    this.login_failed(gen, tf("error.network_init_failed", &[("e", &e)]));
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
    pub(super) fn handle_quick_login(self: &Arc<Self>, username: &str) {
        if username.is_empty() {
            return;
        }
        let accounts = self.store.load_accounts().unwrap_or_default();
        let Some(acc) = accounts.find(username).cloned() else {
            let msg = tf("error.account_not_found", &[("name", &username)]);
            self.set_ui_state(move |ui| {
                ui.set_login_error(SharedString::from(msg));
            });
            return;
        };
        if acc.token.access_token.is_empty() && acc.token.session_key.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_login_error(SharedString::from(t("error.no_valid_session")));
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
            *lock_or_recover(&this.client) = Some(client.clone());
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
        self.set_status(tf("status.restoring_login", &[("name", &last)]));
        self.handle_quick_login(&last);
    }

    pub(super) fn refresh_saved_accounts(&self) {
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
            t("error.need_captcha").to_string()
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

    pub(super) fn handle_login(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        captcha: &str,
        save: bool,
    ) {
        if username.trim().is_empty() || password.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_login_error(SharedString::from(t("error.credentials_required")));
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
                    this.login_failed(gen, tf("error.network_init_failed", &[("e", &e)]));
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

        *lock_or_recover(&self.current_user) = username.clone();
        *lock_or_recover(&self.space) = "personal".to_string();
        *lock_or_recover(&self.folder_id) = ROOT_FOLDER_ID.to_string();
        lock_or_recover(&self.path_stack).clear();
        lock_or_recover(&self.dir_rows).clear();
        lock_or_recover(&self.visible_rows).clear();

        let client = Arc::new(TianyiClient::new(
            self.store.load_config().unwrap_or_default(),
            acc,
        ));
        *lock_or_recover(&self.client) = Some(client);

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

    pub(super) fn logout(self: &Arc<Self>) {
        // 终止进行中的登录与传输
        self.login_gen.fetch_add(1, Ordering::SeqCst);
        self.dir_gen.fetch_add(1, Ordering::SeqCst);
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        self.space_gen.fetch_add(1, Ordering::SeqCst);
        {
            let controls = lock_or_recover(&self.controls);
            for ctl in controls.values() {
                ctl.cancel();
            }
        }
        *lock_or_recover(&self.client) = None;
        *lock_or_recover(&self.current_user) = String::new();
        *lock_or_recover(&self.space) = "personal".to_string();
        *lock_or_recover(&self.folder_id) = ROOT_FOLDER_ID.to_string();
        lock_or_recover(&self.path_stack).clear();
        lock_or_recover(&self.dir_rows).clear();
        lock_or_recover(&self.visible_rows).clear();
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
            ui.set_notice_text(SharedString::from(t("toast.logged_out")));
            ui.set_notice_kind(SharedString::from("info"));
            ui.set_status_text(SharedString::from(t("toast.logged_out")));
            ui.set_transfer_panel_open(false);
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
/// 把任意输入钳制到 1..=8（并发分片数的唯一钳制点）
pub(super) fn clamp_threads(v: i32) -> u32 {
    v.clamp(MIN_THREADS, MAX_THREADS) as u32
}

/// 配置根目录的选择结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StoreRoot {
    /// 主目录可用
    Primary(PathBuf),
    /// 主目录不可用，已退到回退目录
    Fallback(PathBuf),
    /// 两处都不可用：本次运行使用内存态配置，不落盘
    Memory,
}

/// 决定本次运行用哪个配置根目录
///
/// `primary = None` 表示"由 `AppStore` 自己解析系统默认配置目录"（生产路径），只有单测
/// 才需要显式指定一个必然失败的路径。任何一步失败都不 panic：最差落到 `Memory`。
pub(super) fn pick_store_root(primary: Option<&Path>, fallback: &Path) -> StoreRoot {
    match AppStore::new(primary.map(Path::to_path_buf)) {
        Ok(store) => return StoreRoot::Primary(store.root().clone()),
        Err(e) => log::warn!(
            "配置目录不可用（{}）：{e}",
            primary
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "系统默认".to_string())
        ),
    }
    match AppStore::new(Some(fallback.to_path_buf())) {
        Ok(store) => StoreRoot::Fallback(store.root().clone()),
        Err(e) => {
            log::warn!("回退配置目录 {} 也不可用：{e}", fallback.display());
            StoreRoot::Memory
        }
    }
}

/// 配置存储：优先磁盘，目录不可用时退化为内存态
///
/// 为什么要再包一层：`AppStore` 每个接口都直接摸文件系统，目录不可写时连
/// `load_config` 都会失败。这里把"内存态"显式建模出来，让控制器沿用同一套调用形状，
/// 同时保证内存态下 `save_config` 真的能改变本进程内的配置（否则设置页改了也白改）。
pub(super) struct ConfigStore {
    /// `None` = 内存态（本次运行不落盘）
    disk: Option<AppStore>,
    /// 内存态下配置的唯一副本；磁盘态时同步跟随，供回读兜底
    mem: Mutex<AppConfig>,
    /// 展示用配置目录
    root: PathBuf,
}

impl ConfigStore {
    /// 按"主目录 → 临时目录 → 内存态"的顺序打开，返回 (存储, 降级提示)
    pub(super) fn open() -> (Self, Option<String>) {
        let fallback = std::env::temp_dir().join(FALLBACK_CONFIG_DIR);
        match pick_store_root(None, &fallback) {
            StoreRoot::Primary(root) => (Self::open_disk(root), None),
            StoreRoot::Fallback(root) => {
                let msg = tf("state.config_fallback", &[("dir", &root.display())]);
                (Self::open_disk(root), Some(msg))
            }
            StoreRoot::Memory => (
                Self::memory(fallback),
                Some(t("state.config_memory").to_string()),
            ),
        }
    }

    /// 打开磁盘态；万一二次打开失败，退化为内存态而不是 panic
    fn open_disk(root: PathBuf) -> Self {
        match AppStore::new(Some(root.clone())) {
            Ok(store) => {
                let cfg = store.load_config().unwrap_or_else(|e| {
                    log::warn!("读取配置失败，使用默认值：{e}");
                    AppConfig::default_config()
                });
                Self {
                    disk: Some(store),
                    mem: Mutex::new(cfg),
                    root,
                }
            }
            Err(e) => {
                log::warn!(
                    "配置目录 {} 二次打开失败，退化为内存态：{e}",
                    root.display()
                );
                Self::memory(root)
            }
        }
    }

    fn memory(root: PathBuf) -> Self {
        Self {
            disk: None,
            mem: Mutex::new(AppConfig::default_config()),
            root,
        }
    }

    /// 是否真的会落盘（决定任务表要不要配持久化路径、提示条怎么说）
    pub(super) fn is_persistent(&self) -> bool {
        self.disk.is_some()
    }

    pub(super) fn root(&self) -> &PathBuf {
        &self.root
    }

    pub(super) fn load_config(&self) -> anyhow::Result<AppConfig> {
        match &self.disk {
            Some(store) => Ok(store.load_config()?),
            None => Ok(lock_or_recover(&self.mem).clone()),
        }
    }

    fn save_config(&self, cfg: &AppConfig) -> anyhow::Result<()> {
        *lock_or_recover(&self.mem) = cfg.clone();
        match &self.disk {
            Some(store) => Ok(store.save_config(cfg)?),
            // 内存态：只更新进程内副本，启动时的提示条已经说明"不会保存"
            None => Ok(()),
        }
    }

    fn load_accounts(&self) -> anyhow::Result<Accounts> {
        match &self.disk {
            Some(store) => Ok(store.load_accounts()?),
            None => Ok(Accounts::default()),
        }
    }

    fn save_accounts(&self, accounts: &Accounts) -> anyhow::Result<()> {
        match &self.disk {
            Some(store) => Ok(store.save_accounts(accounts)?),
            None => Ok(()),
        }
    }
}

/// 账号首字符（头像用）
pub(super) fn account_initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}
