//! 应用控制器：桥接 UI 事件与 tianyi-core 异步操作

use crate::app::{DragOutApi, MainWindow};
use crate::backend::{invoke_ui, Backend};
use slint::{ComponentHandle, SharedString, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// 网盘内拖拽移动的数据载荷（存放在 DataTransfer.user_data）
#[derive(Debug, Clone)]
struct MovedFile {
    id: String,
    name: String,
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
    path_stack: Mutex<Vec<(String, String)>>,
    current_folder: Mutex<String>,
    last_list: Mutex<Vec<FileObject>>,
    /// 当前目录的快照；搜索结束时用于恢复文件操作上下文
    directory_list: Mutex<Vec<FileObject>>,
    /// 丢弃过期的目录、空间切换和搜索响应
    list_revision: AtomicU64,
    search_revision: AtomicU64,
    search_active: AtomicBool,
    space_revision: AtomicU64,
    status_notice: Mutex<Option<String>>,
    tasks: Arc<TaskManager>,
    /// 拖拽状态（与 winit 事件钩子共享）
    drag_state: Mutex<Option<&'static crate::DragDropState>>,
    /// 拖出下载进行中标记（防止重复触发，仅 Windows 实现使用）
    #[cfg(windows)]
    drag_out_busy: AtomicBool,
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
            directory_list: Mutex::new(Vec::new()),
            list_revision: AtomicU64::new(0),
            search_revision: AtomicU64::new(0),
            search_active: AtomicBool::new(false),
            space_revision: AtomicU64::new(0),
            status_notice: Mutex::new(None),
            tasks: Arc::new(tasks),
            drag_state: Mutex::new(None),
            #[cfg(windows)]
            drag_out_busy: AtomicBool::new(false),
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
            ui.on_rename_file(move |id, name| {
                let c = c.clone();
                if name.is_empty() {
                    c.begin_rename(id.as_str());
                } else {
                    c.rename_selected(id.as_str(), name.as_str());
                }
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

        // 拖拽上传（系统文件拖入窗口）
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_files_dropped(move |joined| {
                let c = c.clone();
                let joined = joined.to_string();
                let paths: Vec<std::path::PathBuf> = joined
                    .split('\n')
                    .map(std::path::PathBuf::from)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect();
                if paths.is_empty() {
                    return;
                }
                c.handle_files_dropped(paths);
            });
        });

        // 网盘内拖动：构造拖拽数据载荷
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

        // 网盘内拖动：目标可接受性判断（拒绝拖到自身/同目录）
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_drop_can_accept(move |event, folder_id| {
                c.check_drop_target(&event, folder_id.as_str())
            });
        });

        // 网盘内拖动：移动到目标文件夹
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            ui.on_drop_requested(move |event, folder_id| {
                let c = c.clone();
                c.handle_drop_move(&event, folder_id.as_str());
                slint::language::DragAction::Move
            });
        });

        // 修饰键拖出下载：拖拽云端文件到本地文件夹
        let c = Arc::clone(&this);
        this.ui_set(move |ui| {
            let api = ui.global::<DragOutApi>();
            api.on_drag_out(move |id, name| {
                let c = c.clone();
                c.handle_drag_out(id.as_str(), name.as_str());
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
        let this = Arc::clone(self);
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
        let this = Arc::clone(self);
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
            ui.set_saved_accounts(slint::ModelRc::new(slint::VecModel::from(names)));
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
        let this = Arc::clone(self);
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
            let client = TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
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

        let this = Arc::clone(self);
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
        self.list_revision.fetch_add(1, Ordering::SeqCst);
        self.search_revision.fetch_add(1, Ordering::SeqCst);
        self.search_active.store(false, Ordering::SeqCst);
        self.space_revision.fetch_add(1, Ordering::SeqCst);
        *self.client.lock().unwrap() = None;
        *self.current_user.lock().unwrap() = String::new();
        *self.current_folder.lock().unwrap() = "-11".to_string();
        self.path_stack.lock().unwrap().clear();
        self.last_list.lock().unwrap().clear();
        self.directory_list.lock().unwrap().clear();
        *self.status_notice.lock().unwrap() = None;
        if let Ok(mut cfg) = self.store.load_config() {
            cfg.last_account = String::new();
            let _ = self.store.save_config(&cfg);
        }
        self.refresh_saved_accounts();
        self.set_ui_state(|ui| {
            ui.set_logged_in(false);
            ui.set_current_page(0);
            ui.set_account_name(SharedString::default());
            ui.set_file_list(slint::ModelRc::default());
            ui.set_search_results(slint::ModelRc::default());
            ui.set_searching(false);
            ui.set_search_loading(false);
            ui.set_search_keyword(SharedString::default());
            ui.set_current_path(SharedString::default());
            ui.set_selected_id(SharedString::default());
            ui.set_renaming_id(SharedString::default());
            ui.set_total_space(SharedString::from("0 B"));
            ui.set_used_space(SharedString::from("0 B"));
            ui.set_capacity_ratio(0.0);
            ui.set_login_error(SharedString::default());
            ui.set_status_text(SharedString::from("已退出登录"));
        });
    }

    fn switch_space(self: &Arc<Self>, space: &str) {
        let space = if space == "family" {
            "family"
        } else {
            "personal"
        }
        .to_string();
        let revision = self.space_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.list_revision.fetch_add(1, Ordering::SeqCst);
        self.search_revision.fetch_add(1, Ordering::SeqCst);
        self.search_active.store(false, Ordering::SeqCst);
        if let Some(client) = self.client() {
            client.set_account(|acc| acc.space_type = space.clone());
        }
        *self.current_folder.lock().unwrap() = "-11".to_string();
        self.path_stack.lock().unwrap().clear();
        self.last_list.lock().unwrap().clear();
        self.directory_list.lock().unwrap().clear();
        *self.status_notice.lock().unwrap() = None;
        let switching = Arc::clone(self);
        self.set_ui_state(move |ui| {
            if switching.space_revision.load(Ordering::SeqCst) != revision {
                return;
            }
            ui.set_file_list(slint::ModelRc::default());
            ui.set_search_results(slint::ModelRc::default());
            ui.set_searching(false);
            ui.set_search_loading(false);
            ui.set_search_keyword(SharedString::default());
            ui.set_current_path(SharedString::default());
            ui.set_selected_id(SharedString::default());
            ui.set_renaming_id(SharedString::default());
            ui.set_total_space(SharedString::from("0 B"));
            ui.set_used_space(SharedString::from("0 B"));
            ui.set_capacity_ratio(0.0);
            ui.set_status_text(SharedString::from("正在切换云空间…"));
        });

        if space == "family" {
            // 家庭云需先自动获取 familyId
            let this = Arc::clone(self);
            let backend = self.backend.clone();
            let ui = self.ui.clone();
            backend.spawn(move || async move {
                if let Some(client) = this.client() {
                    match client.ensure_family_id().await {
                        Ok(id) => {
                            log::info!("family_id 已获取: {id}");
                        }
                        Err(e) => {
                            if this.space_revision.load(Ordering::SeqCst) != revision {
                                return;
                            }
                            let fallback = Arc::clone(&this);
                            let message = format!("家庭云不可用，已返回个人云: {e}");
                            invoke_ui(&ui, move |win| {
                                if fallback.space_revision.load(Ordering::SeqCst) != revision {
                                    return;
                                }
                                if let Some(client) = fallback.client() {
                                    client
                                        .set_account(|acc| acc.space_type = "personal".to_string());
                                }
                                *fallback.status_notice.lock().unwrap() = Some(message.clone());
                                win.set_current_page(0);
                                win.set_status_text(SharedString::from(message));
                                fallback.refresh_files();
                                fallback.load_capacity();
                            });
                            return;
                        }
                    }
                }
                let ready = Arc::clone(&this);
                invoke_ui(&ui, move |_| {
                    if ready.space_revision.load(Ordering::SeqCst) != revision {
                        return;
                    }
                    ready.refresh_files();
                    ready.load_capacity();
                });
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
        let Some(file) = self.find_in_list(id) else {
            log::warn!("selected file {id} not in last list");
            return;
        };
        let name = file.name;
        let size = file.size;
        let default_dir = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| std::env::current_dir().unwrap());
        let picked = rfd::FileDialog::new()
            .set_title("选择下载位置")
            .set_directory(&default_dir)
            .set_file_name(&name)
            .save_file();
        let Some(dest) = picked else {
            return;
        };
        let this = Arc::clone(self);
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
                let cb: tianyi_core::transfer::ProgressCallback = Arc::new(move |done| {
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
        let this = Arc::clone(self);
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

    fn begin_rename(&self, id: &str) {
        if self.find_in_list(id).is_none() {
            return;
        }
        let id = id.to_string();
        self.set_ui_state(move |win| {
            win.set_renaming_id(SharedString::from(id));
            win.set_status_text(SharedString::from("请输入新文件名，按回车保存"));
        });
    }

    fn rename_selected(self: &Arc<Self>, id: &str, new_name: &str) {
        let Some(file) = self.find_in_list(id) else {
            return;
        };
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            self.set_ui_state(|win| {
                win.set_status_text(SharedString::from("文件名不能为空"));
            });
            return;
        }
        if new_name == file.name {
            self.set_ui_state(|win| {
                win.set_renaming_id(SharedString::default());
                win.set_status_text(SharedString::from("文件名未改变"));
            });
            return;
        }
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.rename(&file, &new_name).await {
                    Ok(_) => {
                        invoke_ui(&ui, |win| {
                            win.set_renaming_id(SharedString::default());
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
        let this = Arc::clone(self);
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
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let id = id.to_string();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.get_download_url(&id).await {
                    Ok(url) => {
                        let copied = copy_to_clipboard(&url).is_ok();
                        let msg = if copied {
                            "下载链接已复制到剪贴板".to_string()
                        } else {
                            format!("链接: {url}")
                        };
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
        self.last_list
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.id == id)
            .cloned()
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
        let revision = self.list_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.search_revision.fetch_add(1, Ordering::SeqCst);
        self.search_active.store(false, Ordering::SeqCst);
        self.last_list.lock().unwrap().clear();
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let folder_id = self.current_folder.lock().unwrap().clone();
        let path_text = self.current_path_text();

        let this_ui = Arc::clone(self);
        let loading_path = path_text.clone();
        invoke_ui(&ui, move |win| {
            if this_ui.list_revision.load(Ordering::SeqCst) != revision {
                return;
            }
            if this_ui.search_active.load(Ordering::SeqCst) {
                return;
            }
            win.set_file_list(slint::ModelRc::default());
            win.set_current_path(SharedString::from(loading_path));
            win.set_status_text(SharedString::from("正在加载文件…"));
            win.set_searching(false);
            win.set_search_loading(false);
            win.set_search_keyword(SharedString::default());
            win.set_search_results(slint::ModelRc::default());
            win.set_selected_id(SharedString::default());
            win.set_renaming_id(SharedString::default());
        });

        backend.spawn(move || async move {
            match this.client() {
                Some(client) => match client.list_files(&folder_id).await {
                    Ok(files) => {
                        let entries = files_to_entries(&files);
                        let count = files.len();
                        invoke_ui(&ui, move |win| {
                            if this.list_revision.load(Ordering::SeqCst) != revision {
                                return;
                            }
                            *this.directory_list.lock().unwrap() = files.clone();
                            if !this.search_active.load(Ordering::SeqCst) {
                                *this.last_list.lock().unwrap() = files;
                                let status = this
                                    .status_notice
                                    .lock()
                                    .unwrap()
                                    .take()
                                    .map(|notice| format!("{notice} · 共 {count} 项"))
                                    .unwrap_or_else(|| format!("共 {count} 项"));
                                win.set_status_text(SharedString::from(status));
                            }
                            win.set_file_list(entries_model(entries));
                            win.set_current_path(SharedString::from(path_text));
                        });
                    }
                    Err(e) => {
                        invoke_ui(&ui, move |win| {
                            if this.list_revision.load(Ordering::SeqCst) != revision {
                                return;
                            }
                            if this.search_active.load(Ordering::SeqCst) {
                                return;
                            }
                            let status = this
                                .status_notice
                                .lock()
                                .unwrap()
                                .take()
                                .map(|notice| format!("{notice} · 加载失败: {e}"))
                                .unwrap_or_else(|| format!("加载失败: {e}"));
                            win.set_status_text(SharedString::from(status));
                        });
                    }
                },
                None => {
                    invoke_ui(&ui, move |win| {
                        if this.list_revision.load(Ordering::SeqCst) != revision {
                            return;
                        }
                        if !this.search_active.load(Ordering::SeqCst) {
                            win.set_status_text(SharedString::from("未登录"));
                        }
                    });
                }
            }
        });
    }

    fn load_capacity(self: &Arc<Self>) {
        let revision = self.space_revision.load(Ordering::SeqCst);
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                if let Ok(cap) = client.get_capacity().await {
                    if this.space_revision.load(Ordering::SeqCst) != revision {
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
                    let total_s = tianyi_core::file::format_size(total.max(0) as u64);
                    let used_s = tianyi_core::file::format_size(used.max(0) as u64);
                    let capacity_ratio = if total > 0 {
                        (used.max(0) as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    invoke_ui(&ui, move |win| {
                        if this.space_revision.load(Ordering::SeqCst) != revision {
                            return;
                        }
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
        self.upload_paths(vec![path], &folder_id);
    }

    /// 批量上传本地路径到指定云端目录（文件直接上传，文件夹递归）。
    /// 每个文件注册为一个独立任务。
    pub fn upload_paths(self: &Arc<Self>, paths: Vec<std::path::PathBuf>, folder_id: &str) {
        // 展开文件夹为文件列表，收集 (本地路径, 最终云端父目录)
        // 文件夹采用递归上传，顶层目录名由核心层自动创建
        let mut tasks_to_spawn: Vec<(std::path::PathBuf, u64)> = Vec::new();
        let mut spawned = 0usize;

        for path in paths {
            if path.is_dir() {
                // 递归上传整个文件夹：注册一个任务，进度累计
                let task_id = self.tasks.next_id();
                let total_files = count_local_files(&path);
                let file_size = total_files; // 用文件数作为进度分母的占位（进度由回调累计字节数）
                let task = tianyi_core::task::Task {
                    id: task_id,
                    direction: TaskDirection::Upload,
                    file_name: path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("folder")
                        .to_string(),
                    file_size,
                    local_path: path.clone(),
                    remote_folder_id: folder_id.to_string(),
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
                tasks_to_spawn.push((path.clone(), task_id));
                spawned += 1;
            } else if path.is_file() {
                tasks_to_spawn.push((path.clone(), 0));
                spawned += 1;
            }
        }
        if spawned == 0 {
            return;
        }
        self.refresh_transfers();

        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let folder_id = folder_id.to_string();
        let count = spawned;

        backend.spawn(move || async move {
            for (path, folder_task_id) in tasks_to_spawn {
                if path.is_dir() {
                    this.spawn_folder_upload(&folder_id, &path, folder_task_id)
                        .await;
                } else {
                    this.spawn_file_upload(&folder_id, &path, None).await;
                }
            }
            let msg = format!("拖入上传完成，共 {} 个文件/文件夹", count);
            invoke_ui(&ui, move |win| {
                win.set_status_text(SharedString::from(msg));
            });
            this.refresh_files();
        });
    }

    /// 上传单个文件（注册任务并执行）
    async fn spawn_file_upload(
        self: &Arc<Self>,
        folder_id: &str,
        path: &std::path::Path,
        _task_hint: Option<u64>,
    ) {
        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let task_id = self.tasks.next_id();
        let task = tianyi_core::task::Task {
            id: task_id,
            direction: TaskDirection::Upload,
            file_name: file_name.clone(),
            file_size,
            local_path: path.to_path_buf(),
            remote_folder_id: folder_id.to_string(),
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

        let Some(client) = self.client() else {
            return;
        };
        let opts = tianyi_core::transfer::UploadOptions {
            rapid_upload: true,
            ..Default::default()
        };
        let this_cb = Arc::clone(self);
        let ui = self.ui.clone();
        let last_refresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let path = path.to_path_buf();
        let folder_id = folder_id.to_string();
        let cb: tianyi_core::transfer::ProgressCallback = Arc::new(move |done| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let prev = last_refresh.load(std::sync::atomic::Ordering::SeqCst);
            this_cb.update_progress(task_id, file_size, done);
            if now - prev >= 200 {
                last_refresh.store(now, std::sync::atomic::Ordering::SeqCst);
                this_cb.refresh_transfers();
            }
        });
        match tianyi_core::transfer::upload(&client, &folder_id, &path, &opts, cb).await {
            Ok(_) => {
                self.tasks.update(task_id, |t| {
                    t.status = tianyi_core::task::TaskStatus::Completed;
                    t.progress = 100.0;
                });
            }
            Err(e) => {
                self.tasks.update(task_id, |t| {
                    t.status = tianyi_core::task::TaskStatus::Failed;
                    t.error = Some(e.to_string());
                });
                let msg = format!("上传失败 {}: {e}", file_name);
                invoke_ui(&ui, move |win| {
                    win.set_status_text(SharedString::from(msg));
                });
            }
        }
        self.refresh_transfers();
    }

    /// 递归上传文件夹（单个任务，进度累计）
    async fn spawn_folder_upload(
        self: &Arc<Self>,
        folder_id: &str,
        path: &std::path::Path,
        task_id: u64,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        let opts = tianyi_core::transfer::UploadOptions {
            rapid_upload: true,
            ..Default::default()
        };
        let this_cb = Arc::clone(self);
        let ui = self.ui.clone();
        let last_refresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let path = path.to_path_buf();
        let folder_id = folder_id.to_string();
        let cb: tianyi_core::transfer::ProgressCallback = Arc::new(move |done| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let prev = last_refresh.load(std::sync::atomic::Ordering::SeqCst);
            this_cb.update_progress(task_id, done, done);
            if now - prev >= 200 {
                last_refresh.store(now, std::sync::atomic::Ordering::SeqCst);
                this_cb.refresh_transfers();
            }
        });
        match tianyi_core::transfer::upload_folder_recursive(&client, &folder_id, &path, &opts, cb)
            .await
        {
            Ok(_) => {
                self.tasks.update(task_id, |t| {
                    t.status = tianyi_core::task::TaskStatus::Completed;
                    t.progress = 100.0;
                });
            }
            Err(e) => {
                self.tasks.update(task_id, |t| {
                    t.status = tianyi_core::task::TaskStatus::Failed;
                    t.error = Some(e.to_string());
                });
                let msg = format!("文件夹上传失败 {}: {e}", path.display());
                invoke_ui(&ui, move |win| {
                    win.set_status_text(SharedString::from(msg));
                });
            }
        }
        self.refresh_transfers();
    }

    /// 系统文件拖入窗口 → 上传到当前目录
    fn handle_files_dropped(self: &Arc<Self>, paths: Vec<std::path::PathBuf>) {
        if self.client().is_none() {
            self.set_ui_state(|ui| {
                ui.set_status_text(SharedString::from("请先登录"));
            });
            return;
        }
        let folder_id = self.current_folder.lock().unwrap().clone();
        self.upload_paths(paths, &folder_id);
    }

    /// 设置拖拽状态引用（由 main.rs 传入）
    pub fn set_drag_state(&self, state: &'static crate::DragDropState) {
        *self.drag_state.lock().unwrap() = Some(state);
    }

    /// 修饰键拖出下载：先把云端文件下载到临时目录，再启动系统拖拽，
    /// 用户把它拖到本地文件夹/桌面即可完成下载。
    #[cfg(windows)]
    fn handle_drag_out(self: &Arc<Self>, id: &str, name: &str) {
        if self.client().is_none() {
            self.set_ui_state(|ui| {
                ui.set_status_text(SharedString::from("请先登录"));
            });
            return;
        }
        let Some(file) = self.find_in_list(id) else {
            log::warn!("drag out file {id} not in last list");
            return;
        };
        if self.drag_out_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let file = file.clone();
        let name = name.to_string();
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let id = id.to_string();
        self.set_ui_state(move |win| {
            win.set_status_text(SharedString::from("正在准备文件，拖到本地文件夹即可下载…"));
            let api = win.global::<DragOutApi>();
            api.set_active(true);
            api.set_active_id(SharedString::from(id.as_str()));
        });
        backend.spawn(move || async move {
            // 下载到临时目录
            let temp_dir = std::env::temp_dir().join("tianyi-dragout");
            let _ = std::fs::create_dir_all(&temp_dir);
            let temp_path = temp_dir.join(&name);
            let dl_ok = if let Some(client) = this.client() {
                let opts = tianyi_core::transfer::DownloadOptions::default();
                let prog: tianyi_core::transfer::ProgressCallback = Arc::new(|_| {});
                tianyi_core::transfer::download(&client, &file, &temp_path, &opts, prog)
                    .await
                    .is_ok()
            } else {
                false
            };
            if !dl_ok {
                this.clear_drag_out_active();
                invoke_ui(&ui, move |win| {
                    win.set_status_text(SharedString::from("拖出下载失败：无法获取文件"));
                });
                this.drag_out_busy.store(false, Ordering::SeqCst);
                return;
            }
            // 启动系统拖拽（阻塞直到松开/取消）
            let temp_refs = [temp_path.clone()];
            let dropped = crate::dragout::do_drag_drop(&temp_refs).unwrap_or(false);
            crate::dragout::cleanup_temp_paths(&temp_refs);
            this.drag_out_busy.store(false, Ordering::SeqCst);
            this.clear_drag_out_active();
            invoke_ui(&ui, move |win| {
                // 系统拖拽结束后松开左键的消息可能被 OLE 消费，
                // 主动派发一次 PointerReleased 让 TouchArea 解除按压。
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
        self.set_ui_state(|ui| {
            ui.set_status_text(SharedString::from("当前系统暂不支持拖出下载"));
        });
    }

    /// 清除拖出下载的高亮状态
    #[cfg(windows)]
    fn clear_drag_out_active(&self) {
        self.set_ui_state(|win| {
            let api = win.global::<DragOutApi>();
            api.set_active(false);
            api.set_active_id(SharedString::default());
        });
    }

    /// 拖出下载：拖拽云端文件到本地
    /// 从 DropEvent 提取拖拽的源文件信息
    fn dragged_file(event: &slint::language::DropEvent) -> Option<MovedFile> {
        event
            .data
            .user_data()
            .and_then(|d| d.downcast::<MovedFile>().ok().map(|m| m.as_ref().clone()))
    }

    /// 判断目标文件夹是否可接收（拒绝拖到自身）
    fn check_drop_target(
        &self,
        event: &slint::language::DropEvent,
        folder_id: &str,
    ) -> slint::language::DragAction {
        let Some(src) = Self::dragged_file(event) else {
            return slint::language::DragAction::None;
        };
        // 不能把文件夹拖进它自己
        if src.id == folder_id {
            return slint::language::DragAction::None;
        }
        slint::language::DragAction::Move
    }

    /// 移动文件到目标文件夹
    fn handle_drop_move(self: &Arc<Self>, event: &slint::language::DropEvent, folder_id: &str) {
        let Some(src) = Self::dragged_file(event) else {
            return;
        };
        if src.id == folder_id {
            return;
        }
        let Some(file) = self.find_in_list(&src.id) else {
            return;
        };
        let file = file.clone();
        let folder_id = folder_id.to_string();
        let src_name = src.name.clone();
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.move_to(&file, &folder_id).await {
                    Ok(_) => {
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(format!("已移动 {src_name}")));
                        });
                        this.refresh_files();
                    }
                    Err(e) => {
                        let msg = format!("移动失败: {e}");
                        invoke_ui(&ui, move |win| {
                            win.set_status_text(SharedString::from(msg));
                        });
                    }
                }
            }
        });
    }

    fn create_folder(self: &Arc<Self>) {
        let this = Arc::clone(self);
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
        let this = Arc::clone(self);
        let backend = self.backend.clone();
        let ui = self.ui.clone();
        let id = id.to_string();
        let name = name.to_string();
        let kind = file::classify(&name);
        backend.spawn(move || async move {
            if let Some(client) = this.client() {
                match client.get_download_url(&id).await {
                    Ok(url) => {
                        // 视频/音频：取直链后交给默认播放器流式播放，无需下载到本地
                        if kind == FileKind::Video || kind == FileKind::Audio {
                            let opened = file::open_media_url_with_player(&name, &url);
                            let msg = match opened {
                                Ok(()) => format!("已在默认播放器中打开: {name}"),
                                Err(err) => format!("打开播放器失败: {err}"),
                            };
                            invoke_ui(&ui, move |win| {
                                win.set_status_text(SharedString::from(msg));
                            });
                            return;
                        }
                        match this.download_to_temp(&client, &url, &name).await {
                            Ok(path) => {
                                if kind == FileKind::Image {
                                    invoke_ui(&ui, move |win| {
                                        let img = load_image_file(&path);
                                        win.set_captcha_image(img);
                                        win.set_qr_status(SharedString::from(format!(
                                            "预览: {name}"
                                        )));
                                    });
                                } else {
                                    let _ = file::open_with_system(&path);
                                }
                            }
                            Err(e) => log::error!("preview download failed: {e}"),
                        }
                    }
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
        let keyword = keyword.trim().to_string();
        if keyword.is_empty() {
            self.clear_search();
            return;
        }

        let revision = self.search_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.search_active.store(true, Ordering::SeqCst);
        let directory_snapshot = self.directory_list.lock().unwrap().clone();
        *self.last_list.lock().unwrap() = directory_snapshot;
        let this = Arc::clone(self);
        let ui = self.ui.clone();
        let visible_keyword = keyword.clone();
        let search_ui = Arc::clone(self);
        self.set_ui_state(move |win| {
            if search_ui.search_revision.load(Ordering::SeqCst) != revision {
                return;
            }
            win.set_searching(true);
            win.set_search_loading(true);
            win.set_search_keyword(SharedString::from(visible_keyword));
            win.set_search_results(slint::ModelRc::default());
            win.set_status_text(SharedString::from("搜索中…"));
        });

        let backend = self.backend.clone();
        backend.spawn(move || async move {
            // 输入框逐字编辑时只保留最后一个关键词的请求。
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if this.search_revision.load(Ordering::SeqCst) != revision {
                return;
            }
            if let Some(client) = this.client() {
                match client.search_files(&keyword).await {
                    Ok(files) => {
                        let entries = files_to_entries(&files);
                        let count = files.len();
                        invoke_ui(&ui, move |win| {
                            if this.search_revision.load(Ordering::SeqCst) != revision {
                                return;
                            }
                            *this.last_list.lock().unwrap() = files;
                            win.set_search_results(entries_model(entries));
                            win.set_searching(true);
                            win.set_search_loading(false);
                            win.set_search_keyword(SharedString::from(keyword));
                            let status = if count == 0 {
                                "没有找到匹配的文件".to_string()
                            } else {
                                format!("找到 {count} 项")
                            };
                            win.set_status_text(SharedString::from(status));
                        });
                    }
                    Err(e) => {
                        invoke_ui(&ui, move |win| {
                            if this.search_revision.load(Ordering::SeqCst) != revision {
                                return;
                            }
                            win.set_search_loading(false);
                            win.set_status_text(SharedString::from(format!("搜索失败: {e}")));
                        });
                    }
                }
            }
        });
    }

    fn clear_search(self: &Arc<Self>) {
        let revision = self.search_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.search_active.store(false, Ordering::SeqCst);
        let files = self.directory_list.lock().unwrap().clone();
        *self.last_list.lock().unwrap() = files.clone();
        let entries = files_to_entries(&files);
        let count = files.len();
        let search_ui = Arc::clone(self);
        self.set_ui_state(move |ui| {
            if search_ui.search_revision.load(Ordering::SeqCst) != revision {
                return;
            }
            ui.set_searching(false);
            ui.set_search_loading(false);
            ui.set_search_keyword(SharedString::default());
            ui.set_search_results(slint::ModelRc::default());
            ui.set_file_list(entries_model(entries));
            ui.set_status_text(SharedString::from(format!("共 {count} 项")));
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
                progress: t.progress,
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
                t.speed = delta_bytes * 1000 / delta_ms;
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

/// 统计本地文件夹（递归）下的文件数量
fn count_local_files(root: &std::path::Path) -> u64 {
    let mut n = 0u64;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                n += count_local_files(&path);
            } else if path.is_file() {
                n += 1;
            }
        }
    }
    n
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

fn transfers_model(
    entries: Vec<crate::app::TransferEntry>,
) -> slint::ModelRc<crate::app::TransferEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

fn load_image_file(path: &std::path::Path) -> slint::Image {
    match image::open(path) {
        Ok(img) => rgba_to_slint_image(img.to_rgba8()),
        Err(_) => slint::Image::default(),
    }
}

/// 将文本复制到系统剪贴板
fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    let mut clip = arboard::Clipboard::new()?;
    clip.set_text(text.to_string())?;
    Ok(())
}

/// 当前 Unix 时间戳（秒）
fn chrono_used_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
