//! 应用控制器：桥接 UI 事件与 tianyi-core 异步操作

use crate::app::MainWindow;
use crate::backend::{invoke_ui, Backend};
use slint::{SharedString, Weak};
use std::sync::{Arc, Mutex};
use tianyi_core::api::{self, LoginNotifier};
use tianyi_core::client::TianyiClient;
use tianyi_core::config::{AppConfig, AppStore};
use tianyi_core::file::{self, FileKind};
use tianyi_core::models::{AccountConfig, FileObject, TokenInfo};
use tianyi_core::task::{TaskDirection, TaskManager};

/// 登录通知器（向 UI 推送验证码/二维码）
struct AppNotifier {
    ui: Weak<MainWindow>,
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
        let rgba = render_qr_rgba(uuid);
        let ui = self.ui.clone();
        let text = text.to_string();
        invoke_ui(&ui, move |win| {
            win.set_qr_image(rgba_to_slint_image(rgba));
            win.set_qr_status(SharedString::from(text));
        });
    }

    fn qr_status(&self, status: &str) {
        let ui = self.ui.clone();
        let status = status.to_string();
        invoke_ui(&ui, move |win| {
            win.set_qr_status(SharedString::from(status));
        });
    }
}

/// 渲染二维码为 RGBA 像素（线程安全）
fn render_qr_rgba(uuid: &str) -> image::RgbaImage {
    let code = match qrcode::QrCode::new(uuid.as_bytes()) {
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
    let width = img.width() as u32;
    let height = img.height() as u32;
    let raw = img.into_raw();
    let mut buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    buffer
        .make_mut_bytes()
        .copy_from_slice(&raw);
    slint::Image::from_rgba8(buffer)
}

/// 应用控制器
pub struct Controller {
    ui: Weak<MainWindow>,
    backend: Arc<Backend>,
    store: AppStore,
    client: Mutex<Option<Arc<TianyiClient>>>,
    current_user: Mutex<String>,
    path_stack: Mutex<Vec<(String, String)>>,
    current_folder: Mutex<String>,
    last_list: Mutex<Vec<FileObject>>,
    tasks: Arc<TaskManager>,
    /// 本会话启动时间戳（秒），传输列表只显示本会话创建的任务
    session_start: i64,
}

impl Controller {
    pub fn new(ui: Weak<MainWindow>, backend: Arc<Backend>) -> Arc<Self> {
        let store = AppStore::new(None).expect("init app store");
        let tasks = TaskManager::new(Some(store.root().join("tasks.json")));
        let c = Arc::new(Controller {
            ui,
            backend,
            store,
            client: Mutex::new(None),
            current_user: Mutex::new(String::new()),
            path_stack: Mutex::new(Vec::new()),
            current_folder: Mutex::new("-11".to_string()),
            last_list: Mutex::new(Vec::new()),
            tasks: Arc::new(tasks),
            session_start: chrono_used_now(),
        });
        Self::bind_ui(Arc::clone(&c));
        c.refresh_saved_accounts();
        c
    }

    /// 绑定 UI 回调（每个回调持有 Arc 克隆，方便跨线程）
    fn bind_ui(this: Arc<Self>) {
        // 登录
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_login_request(move |user, pass, captcha| {
                let c = c.clone();
                c.handle_login(user.as_str(), pass.as_str(), captcha.as_str());
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
            ui.on_quick_login(move |username| {
                let c = c.clone();
                c.handle_quick_login(username.as_str());
            });
        });

        // 文件浏览
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_open_item(move |id, name, is_dir| {
                let c = c.clone();
                c.handle_open_item(id.as_str(), name.as_str(), is_dir);
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_select_item(move |id| {
                let c = c.clone();
                c.handle_select_item(id.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_download_file(move |id| {
                let c = c.clone();
                c.download_selected(id.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_delete_file(move |id| {
                let c = c.clone();
                c.delete_selected(id.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_rename_file(move |id| {
                let c = c.clone();
                c.rename_selected(id.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_copy_file(move |id| {
                let c = c.clone();
                c.copy_selected(id.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_get_link(move |id| {
                let c = c.clone();
                c.get_link(id.as_str());
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

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_search_files(move |kw| {
                let c = c.clone();
                c.search_files(kw.as_str());
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_clear_search(move || {
                let c = c.clone();
                c.clear_search();
            });
        });

        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_cancel_task(move |id| {
                let c = c.clone();
                c.cancel_task(id as u64);
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
            ui.on_switch_space(move |space| {
                let c = c.clone();
                c.switch_space(space.as_str());
            });
        });
    }

    /// 在 UI 线程执行一段绑定代码
    fn ui_set<F: FnOnce(&MainWindow) + Send + 'static>(&self, f: F) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop({
            let ui = ui.clone();
            move || {
                if let Some(ui) = ui.upgrade() {
                    f(&ui);
                }
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

    fn handle_switch_mode(self: &Arc<Self>, qr: bool) {
        self.set_ui_state(move |ui| {
            ui.set_qr_mode(qr);
            ui.set_login_error(SharedString::default());
            if qr {
                ui.set_qr_status(SharedString::from("正在获取二维码..."));
            }
        });
        if qr {
            self.handle_qr_login();
        }
    }

    fn handle_qr_login(self: &Arc<Self>) {
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let notifier = AppNotifier { ui };
        self.set_ui_state(|ui| ui.set_login_loading(true));

        backend.spawn(move || async move {
            let http = reqwest::Client::builder()
                .cookie_store(true)
                .build()
                .expect("build http client");
            let dummy = TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
            match api::login_by_qrcode(&dummy, &http, &notifier).await {
                Ok(param) => match api::poll_qr_login(&http, &param, &notifier).await {
                    Ok(session) => {
                        let token = api::token_from_session(&session);
                        let username = session.login_name.clone();
                        this.on_login_success(username, token);
                    }
                    Err(e) => this.login_failed(e.to_string()),
                },
                Err(e) => this.login_failed(e.to_string()),
            }
        });
    }

    /// 使用已保存的账号快速登录（免输入账号密码）
    fn handle_quick_login(self: &Arc<Self>, username: &str) {
        if username.is_empty() {
            return;
        }
        let accounts = self.store.load_accounts().unwrap_or_default();
        let Some(acc) = accounts.find(username).cloned() else {
            self.login_failed(format!("未找到账号 {username} 的登录信息"));
            return;
        };
        self.set_ui_state(|ui| ui.set_login_loading(true));
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let username = username.to_string();
        backend.spawn(move || async move {
            if acc.token.access_token.is_empty() {
                this.login_failed("账号无有效 token，请重新登录".to_string());
                return;
            }
            let client = Arc::new(TianyiClient::new(
                this.store.load_config().unwrap_or_default(),
                acc,
            ));
            *this.client.lock().unwrap() = Some(client.clone());
            if let Err(e) = client.refresh_session().await {
                log::warn!("quick login refresh session: {e}");
            }
            let uname = username.clone();
            invoke_ui(&ui, move |win| {
                win.set_login_loading(false);
                win.set_logged_in(true);
                win.set_account_name(SharedString::from(uname));
                win.set_status_text(SharedString::from("已恢复登录态"));
            });
            this.refresh_files();
            this.load_capacity();
        });
    }

    /// 启动时尝试用上次使用的账号自动登录
    pub fn auto_login(self: &Arc<Self>) {
        let last = self.store.load_config().unwrap_or_default().last_account;
        if last.is_empty() {
            return;
        }
        let status = format!("正在恢复账号 {last} 的登录态...");
        self.set_ui_state(move |ui| {
            ui.set_qr_status(SharedString::from(status));
        });
        self.handle_quick_login(&last);
    }

    /// 刷新登录面板显示的已保存账号列表
    fn refresh_saved_accounts(&self) {
        let names: Vec<SharedString> = self
            .store
            .load_accounts()
            .unwrap_or_default()
            .accounts
            .iter()
            .filter(|a| !a.username.is_empty())
            .map(|a| SharedString::from(a.username.clone()))
            .collect();
        self.set_ui_state(move |ui| {
            ui.set_saved_accounts(
                slint::ModelRc::new(slint::VecModel::from(names)),
            );
        });
    }

    fn login_failed(&self, msg: String) {
        self.set_ui_state(move |ui| {
            ui.set_login_loading(false);
            if msg.contains("NEED_CAPTCHA") {
                ui.set_login_error(SharedString::from("请输入上方验证码后重新登录"));
            } else {
                ui.set_login_error(SharedString::from(msg));
            }
        });
    }

    fn handle_login(self: &Arc<Self>, username: &str, password: &str, captcha: &str) {
        if username.is_empty() || password.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_login_error(SharedString::from("请输入用户名和密码"));
            });
            return;
        }
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let notifier = AppNotifier { ui };
        let username = username.to_string();
        let password = password.to_string();
        let captcha = captcha.to_string();
        self.set_ui_state(|ui| ui.set_login_loading(true));

        backend.spawn(move || async move {
            let http = reqwest::Client::builder()
                .cookie_store(true)
                .build()
                .expect("build http client");
            let client =
                TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
            let cap_opt = if captcha.is_empty() {
                None
            } else {
                Some(captcha.as_str())
            };
            match api::login_by_password(&client, &http, &username, &password, cap_opt, &notifier)
                .await
            {
                Ok(session) => {
                    let token = api::token_from_session(&session);
                    let uname = if session.login_name.is_empty() {
                        username
                    } else {
                        session.login_name
                    };
                    this.on_login_success(uname, token);
                }
                Err(e) => this.login_failed(e.to_string()),
            }
        });
    }

    fn on_login_success(self: &Arc<Self>, username: String, token: TokenInfo) {
        let mut accounts = self.store.load_accounts().unwrap_or_default();
        let mut acc = accounts
            .find(&username)
            .cloned()
            .unwrap_or_else(|| AccountConfig {
                username: username.clone(),
                ..Default::default()
            });
        acc.token = token;
        acc.login_type = "password".to_string();
        accounts.upsert(acc.clone());
        let _ = self.store.save_accounts(&accounts);

        if let Ok(mut cfg) = self.store.load_config() {
            cfg.last_account = username.clone();
            let _ = self.store.save_config(&cfg);
        }

        *self.current_user.lock().unwrap() = username.clone();
        *self.current_folder.lock().unwrap() = "-11".to_string();
        *self.path_stack.lock().unwrap() = Vec::new();

        let client = Arc::new(TianyiClient::new(
            self.store.load_config().unwrap_or_default(),
            acc,
        ));
        *self.client.lock().unwrap() = Some(client);

        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        this.refresh_saved_accounts();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                if let Err(e) = client.refresh_session().await {
                    log::warn!("refresh session after login: {e}");
                }
            }
            let uname = username.clone();
            invoke_ui(&ui, move |win| {
                win.set_login_loading(false);
                win.set_logged_in(true);
                win.set_account_name(SharedString::from(uname));
                win.set_status_text(SharedString::from("登录成功"));
            });
            this.refresh_files();
            this.load_capacity();
        });
    }

    fn logout(&self) {
        *self.client.lock().unwrap() = None;
        *self.current_user.lock().unwrap() = String::new();
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.last_account = String::new();
            let _ = self.store.save_config(&cfg);
        }
        self.refresh_saved_accounts();
        self.set_ui_state(|ui| {
            ui.set_logged_in(false);
            ui.set_account_name(SharedString::default());
            ui.set_file_list(slint::ModelRc::default());
            ui.set_login_error(SharedString::default());
        });
    }

    fn switch_space(self: &Arc<Self>, space: &str) {
        if let Some(c) = self.client() {
            c.set_account(|acc| acc.space_type = space.to_string());
        }
        *self.current_folder.lock().unwrap() = "-11".to_string();
        *self.path_stack.lock().unwrap() = Vec::new();
        if space == "family" {
            // 家庭云需先自动获取 familyId
            let this = Arc::clone(&self);
            let backend = self.backend.clone();
            let ui = self.ui.clone();
            backend.spawn(move || async move {
                if let Some(client) = this.client() {
                    match client.ensure_family_id().await {
                        Ok(id) => {
                            log::info!("family_id 已获取: {id}");
                        }
                        Err(e) => {
                            invoke_ui(&ui, move |win| {
                                win.set_status_text(SharedString::from(format!("家庭云: {e}")));
                            });
                            // 获取失败则切回个人云
                            if let Some(client) = this.client() {
                                client.set_account(|acc| acc.space_type = "personal".to_string());
                            }
                            return;
                        }
                    }
                }
                this.refresh_files();
                this.load_capacity();
            });
        } else {
            self.refresh_files();
            self.load_capacity();
        }
    }

    fn handle_open_item(self: &Arc<Self>, id: &str, name: &str, is_dir: bool) {
        if !is_dir {
            self.preview_file(id, name);
            return;
        }
        let current = self.current_folder.lock().unwrap().clone();
        self.path_stack
            .lock()
            .unwrap()
            .push((current, name.to_string()));
        *self.current_folder.lock().unwrap() = id.to_string();
        self.refresh_files();
    }

    fn handle_select_item(&self, id: &str) {
        // 在 UI 中高亮
        let id = id.to_string();
        self.set_ui_state(move |win| {
            win.set_selected_id(SharedString::from(id));
        });
    }

    fn download_selected(self: &Arc<Self>, id: &str) {
        // 查找文件信息：从最近加载的列表中获取
        let name = self
            .last_list
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.name.clone());
        let size = self
            .last_list
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.size);
        let Some(name) = name else {
            log::warn!("selected file {id} not in last list");
            return;
        };
        let size = size.unwrap_or(0);
        let dest = self
            .store
            .load_config()
            .map(|c| c.download_dir.join(&name))
            .unwrap_or_else(|_| std::env::current_dir().unwrap().join(&name));
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let id = id.to_string();
        // 注册任务
        let task_id = self.tasks.next_id();
        let task = tianyi_core::task::Task {
            id: task_id,
            direction: TaskDirection::Download,
            file_name: name.clone(),
            file_size: size as u64,
            local_path: dest.clone(),
            remote_folder_id: id.clone(),
            status: tianyi_core::task::TaskStatus::Running,
            progress: 0.0,
            bytes_done: 0,
            speed: 0,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: chrono_used_now(),
        };
        self.tasks.add(task);
        self.refresh_transfers();

        backend.spawn(move || async move {
            let client = this.client();
            let file = FileObject {
                id,
                name,
                size,
                ..Default::default()
            };
            if let Some(client) = client {
                let opts = tianyi_core::transfer::DownloadOptions::default();
                let this_cb = this.clone();
                let last_refresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let cb: tianyi_core::transfer::ProgressCallback =
                    Arc::new(move |done| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let prev = last_refresh.load(std::sync::atomic::Ordering::SeqCst);
                        this_cb.update_progress(task_id, size.max(0) as u64, done);
                        // 节流：最多每 200ms 刷新一次 UI
                        if now - prev >= 200 {
                            last_refresh.store(now, std::sync::atomic::Ordering::SeqCst);
                            this_cb.refresh_transfers();
                        }
                        log::debug!("download progress: {done}/{size}");
                    });
                match tianyi_core::transfer::download(&client, &file, &dest, &opts, cb).await {
                    Ok(_) => {
                        this.tasks.update(task_id, |t| {
                            t.status = tianyi_core::task::TaskStatus::Completed;
                            t.progress = 100.0;
                        });
                        invoke_ui(&ui, |win| {
                            win.set_status_text(SharedString::from("下载完成"));
                        });
                    }
                    Err(e) => {
                        this.tasks.update(task_id, |t| {
                            t.status = tianyi_core::task::TaskStatus::Failed;
                            t.error = Some(e.to_string());
                        });
                    }
                }
                this.refresh_transfers();
            }
        });
    }

    fn delete_selected(self: &Arc<Self>, id: &str) {
        let Some(file) = self.find_in_list(id) else {
            return;
        };
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let file = file.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.delete(&file).await {
                    Ok(_) => {
                        invoke_ui(&ui, |win| {
                            win.set_status_text(SharedString::from("已删除"));
                        });
                        this.refresh_files();
                    }
                    Err(e) => {
                        let msg = format!("删除失败: {e}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                }
            }
        });
    }

    fn rename_selected(self: &Arc<Self>, id: &str) {
        let Some(file) = self.find_in_list(id) else {
            return;
        };
        // 简化：使用固定后缀重命名（后续可弹窗）
        let new_name = format!("{}_renamed", file.name);
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.rename(&file, &new_name).await {
                    Ok(_) => {
                        invoke_ui(&ui, |win| {
                            win.set_status_text(SharedString::from("已重命名"));
                        });
                        this.refresh_files();
                    }
                    Err(e) => {
                        let msg = format!("重命名失败: {e}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                }
            }
        });
    }

    fn copy_selected(self: &Arc<Self>, id: &str) {
        let Some(file) = self.find_in_list(id) else {
            return;
        };
        let dst = self.current_folder.lock().unwrap().clone();
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.copy_to(&file, &dst).await {
                    Ok(_) => {
                        invoke_ui(&ui, |win| {
                            win.set_status_text(SharedString::from("已复制"));
                        });
                        this.refresh_files();
                    }
                    Err(e) => {
                        let msg = format!("复制失败: {e}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                }
            }
        });
    }

    fn get_link(self: &Arc<Self>, id: &str) {
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let id = id.to_string();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.get_download_url(&id).await {
                    Ok(url) => {
                        // 复制到剪贴板（简化：显示在状态栏）
                        let msg = format!("链接: {url}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                    Err(e) => {
                        let msg = format!("获取链接失败: {e}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                }
            }
        });
    }

    /// 从最近加载的文件列表中按 id 查找
    fn find_in_list(&self, id: &str) -> Option<FileObject> {
        self.last_list.lock().unwrap().iter().find(|f| f.id == id).cloned()
    }

    fn navigate_up(self: &Arc<Self>) {
        let mut stack = self.path_stack.lock().unwrap();
        if let Some((parent_id, _)) = stack.pop() {
            *self.current_folder.lock().unwrap() = parent_id;
            drop(stack);
            self.refresh_files();
        }
    }

    fn current_path_text(&self) -> String {
        let stack = self.path_stack.lock().unwrap();
        stack
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn refresh_files(self: &Arc<Self>) {
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let folder_id = self.current_folder.lock().unwrap().clone();
        let path_text = self.current_path_text();

        backend.spawn(move || async move {
            match this.client() {
                Some(client) => match client.list_files(&folder_id).await {
                    Ok(files) => {
                        let entries = files_to_entries(&files);
                        let count = files.len();
                        *this.last_list.lock().unwrap() = files.clone();
                        invoke_ui(&ui, move |win| {
                                win.set_file_list(entries_model(entries));
                                win.set_current_path(SharedString::from(path_text));
                                win.set_status_text(SharedString::from(format!("共 {count} 项")));
                                win.set_searching(false);
                        });
                    }
                    Err(e) => {
                        invoke_ui(&ui, move |win| {
                                win.set_status_text(SharedString::from(format!("加载失败: {e}")));
                        });
                    }
                },
                None => {
                    invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from("未登录"));
                    });
                }
            }
        });
    }

    fn load_capacity(self: &Arc<Self>) {
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                if let Ok(cap) = client.get_capacity().await {
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
                    let total_s = tianyi_core::file::format_size(total.max(0) as u64);
                    let used_s = tianyi_core::file::format_size(used.max(0) as u64);
                    let capacity_ratio = if total > 0 {
                        (used.max(0) as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    invoke_ui(&ui, move |win| {
                        win.set_total_space(SharedString::from(total_s));
                        win.set_used_space(SharedString::from(used_s));
                        win.set_capacity_ratio(capacity_ratio);
                    });
                }
            }
        });
    }

    fn upload_files(self: &Arc<Self>) {
        // 文件选择对话框（阻塞 UI 线程，仅在用户点击时调用）
        let picked = rfd::FileDialog::new()
            .set_title("选择要上传的文件")
            .pick_file();
        let Some(path) = picked else {
            return;
        };
        let folder_id = self.current_folder.lock().unwrap().clone();
        let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();

        // 注册任务
        let task_id = self.tasks.next_id();
        let task = tianyi_core::task::Task {
            id: task_id,
            direction: TaskDirection::Upload,
            file_name: file_name.clone(),
            file_size,
            local_path: path.clone(),
            remote_folder_id: folder_id.clone(),
            status: tianyi_core::task::TaskStatus::Running,
            progress: 0.0,
            bytes_done: 0,
            speed: 0,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: chrono_used_now(),
        };
        self.tasks.add(task);
        self.refresh_transfers();

        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                let opts = tianyi_core::transfer::UploadOptions {
                    rapid_upload: true,
                    ..Default::default()
                };
                let this_cb = this.clone();
                let last_refresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let cb: tianyi_core::transfer::ProgressCallback =
                    Arc::new(move |done| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let prev = last_refresh.load(std::sync::atomic::Ordering::SeqCst);
                        this_cb.update_progress(task_id, file_size, done);
                        // 节流：最多每 200ms 刷新一次 UI
                        if now - prev >= 200 {
                            last_refresh.store(now, std::sync::atomic::Ordering::SeqCst);
                            this_cb.refresh_transfers();
                        }
                        log::debug!("upload progress: {done}/{file_size}");
                    });
                match tianyi_core::transfer::upload(
                    &client,
                    &folder_id,
                    &path,
                    &opts,
                    cb,
                )
                .await
                {
                    Ok(_) => {
                        this.tasks.update(task_id, |t| {
                            t.status = tianyi_core::task::TaskStatus::Completed;
                            t.progress = 100.0;
                        });
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from("上传完成"));
                        });
                        this.refresh_files();
                    }
                    Err(e) => {
                        this.tasks.update(task_id, |t| {
                            t.status = tianyi_core::task::TaskStatus::Failed;
                            t.error = Some(e.to_string());
                        });
                        let msg = format!("上传失败: {e}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                }
                this.refresh_transfers();
            }
        });
    }

    fn create_folder(self: &Arc<Self>) {
        let this = Arc::clone(&self);
        let folder_id = self.current_folder.lock().unwrap().clone();
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let name = "新建文件夹";
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.create_folder(&folder_id, name).await {
                    Ok(_) => {
                        invoke_ui(&ui, move |win| {
                                win.set_status_text(SharedString::from("文件夹已创建"));
                        });
                        this.refresh_files();
                    }
                    Err(e) => {
                        invoke_ui(&ui, move |win| {
                                win.set_status_text(SharedString::from(format!("创建失败: {e}")));
                        });
                    }
                }
            }
        });
    }

    fn preview_file(self: &Arc<Self>, id: &str, name: &str) {
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let id = id.to_string();
        let name = name.to_string();
        let kind = file::classify(&name);
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.get_download_url(&id).await {
                    Ok(url) => match this.download_to_temp(&client, &url, &name).await {
                        Ok(path) => {
                            if kind == FileKind::Image {
                                invoke_ui(&ui, move |win| {
                                        let img = load_image_file(&path);
                                        win.set_captcha_image(img);
                                        win.set_qr_status(SharedString::from(format!("预览: {name}")));
                                });
                            } else {
                                let _ = file::open_with_system(&path);
                            }
                        }
                        Err(e) => log::error!("preview download failed: {e}"),
                    },
                    Err(e) => log::error!("get download url failed: {e}"),
                }
            }
        });
    }

    async fn download_to_temp(
        &self,
        client: &TianyiClient,
        url: &str,
        name: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join("tianyi-preview");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        let resp = client.http_client().get(url).send().await?;
        let bytes = resp.bytes().await?;
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    fn search_files(self: &Arc<Self>, keyword: &str) {
        if keyword.trim().is_empty() {
            return;
        }
        let this = Arc::clone(&self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let keyword = keyword.to_string();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.search_files(&keyword).await {
                    Ok(files) => {
                        let entries = files_to_entries(&files);
                        let count = files.len();
                        invoke_ui(&ui, move |win| {
                                win.set_search_results(entries_model(entries));
                                win.set_searching(true);
                                win.set_search_keyword(SharedString::from(keyword.clone()));
                                win.set_status_text(SharedString::from(format!("找到 {count} 项")));
                        });
                    }
                    Err(e) => {
                        invoke_ui(&ui, move |win| {
                                win.set_status_text(SharedString::from(format!("搜索失败: {e}")));
                        });
                    }
                }
            }
        });
    }

    fn clear_search(&self) {
        self.set_ui_state(|ui| {
            ui.set_searching(false);
            ui.set_search_results(slint::ModelRc::default());
        });
    }

    fn cancel_task(&self, id: u64) {
        self.tasks.remove(id);
        self.refresh_transfers();
    }

    fn refresh_transfers(&self) {
        let session_start = self.session_start;
        let tasks = self.tasks.list();
        let entries = tasks
            .iter()
            .filter(|t| t.created_at >= session_start)
            .map(|t| crate::app::TransferEntry {
                id: t.id as i32,
                name: t.file_name.clone().into(),
                direction: if t.direction == TaskDirection::Upload {
                    "upload".into()
                } else {
                    "download".into()
                },
                size_text: file::format_size(t.file_size).into(),
                progress: t.progress as f32,
                status: format!("{:?}", t.status).into(),
                speed: file::format_speed(t.speed).into(),
            })
            .collect::<Vec<_>>();
        self.set_ui_state(move |ui| {
            ui.set_transfer_list(transfers_model(entries));
        });
    }

    /// 更新任务进度并自适应计算瞬时速度（bytes/s）
    fn update_progress(&self, task_id: u64, total_size: u64, done: u64) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.tasks.update(task_id, |t| {
            let delta_ms = now_ms.saturating_sub(t.last_speed_time);
            let delta_bytes = done.saturating_sub(t.last_done);
            // 至少 200ms 采样窗口才计算速度，避免抖动
            if delta_ms >= 200 && delta_bytes > 0 {
                t.speed = delta_bytes as u64 * 1000 / delta_ms;
                t.last_speed_time = now_ms;
                t.last_done = done;
            } else if t.last_speed_time == 0 {
                t.last_speed_time = now_ms;
                t.last_done = done;
            }
            t.bytes_done = done;
            t.progress = if total_size > 0 {
                (done as f32) / (total_size as f32) * 100.0
            } else {
                100.0
            };
        });
    }

    pub fn spawn_background(self: &Arc<Self>) {
        let this = Arc::clone(&self);
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

fn files_to_entries(files: &[FileObject]) -> Vec<crate::app::FileEntry> {
    files
        .iter()
        .map(|f| {
            let size_text = if f.is_dir {
                "文件夹".to_string()
            } else {
                file::format_size(f.size as u64)
            };
            crate::app::FileEntry {
                id: f.id.clone().into(),
                name: f.name.clone().into(),
                size: f.size as i32,
                size_text: size_text.into(),
                is_dir: f.is_dir,
                thumb: f.thumb.clone().into(),
                modified: f.last_op_time.clone().into(),
            }
        })
        .collect()
}

fn entries_model(entries: Vec<crate::app::FileEntry>) -> slint::ModelRc<crate::app::FileEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

fn transfers_model(entries: Vec<crate::app::TransferEntry>) -> slint::ModelRc<crate::app::TransferEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

fn load_image_file(path: &std::path::Path) -> slint::Image {
    match image::open(path) {
        Ok(img) => rgba_to_slint_image(img.to_rgba8()),
        Err(_) => slint::Image::default(),
    }
}

/// 当前 Unix 时间戳（秒）
fn chrono_used_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
