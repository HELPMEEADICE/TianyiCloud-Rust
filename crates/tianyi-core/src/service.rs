//! 共享会话与云盘操作服务：GUI 与 CLI 的同一套业务语义
//!
//! 分层意图（对应 docs/CLI.md 的架构图）：
//!
//! * [TianyiClient] 只管「协议」：签名、加密、分页、批量任务轮询；
//! * 本模块管「语义」：哪个账号、哪个云空间、路径怎么解析成 id、会话失效后怎么刷新并
//!   回写 token、家庭云哪些操作要额外带 familyId；
//! * GUI 控制器与 CLI 命令都只调用这一层，不再各自拼参数。
//!
//! 会话失效的处理是这里最关键的一条：天翼云在 session key 过期时返回
//! UserInvalidOpenToken / InvalidSessionKey，此时 [TianyiClient::refresh_session]
//! 会用 refresh token 换一套新 key。CLI 是**每次调用一个新进程**，刷新后必须把新
//! token 写回 accounts.json，否则下一个命令又要重新登录。因此 [CloudService::call]
//! 统一做「失效 → 刷新 → 存盘 → 重试一次」，业务代码不必各自实现。

use crate::api::{self, LoginNotifier};
use crate::client::TianyiClient;
use crate::cloudpath::{CloudPath, ROOT_FOLDER_ID};
use crate::config::AppConfig;
use crate::error::{Error, Result};
use crate::lock_or_recover;
use crate::models::{AccountConfig, FileObject, FolderObject, TokenInfo};
use crate::store::ConfigStore;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 云空间：个人云 / 家庭云
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Space {
    /// 个人云（默认空间）
    #[default]
    Personal,
    /// 家庭云
    Family,
}

impl Space {
    /// 配置与 JSON 里的稳定键名
    pub fn as_str(self) -> &'static str {
        match self {
            Space::Personal => "personal",
            Space::Family => "family",
        }
    }

    /// 解析空间名（大小写与中文别名都接受）
    pub fn parse(s: &str) -> Option<Space> {
        match s.trim().to_ascii_lowercase().as_str() {
            "personal" | "cloud" | "个人云" | "个人" => Some(Space::Personal),
            "family" | "家庭云" | "家庭" => Some(Space::Family),
            _ => None,
        }
    }
}

/// 当前会话快照（CLI status / GUI 登录页都可以直接用）
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// 是否已建立可用会话
    pub authenticated: bool,
    /// 是否明确被服务端判为失效（需要重新登录）
    pub expired: bool,
    /// 账号名（手机号 / 邮箱 / 登录名）
    pub username: String,
    /// 当前云空间
    pub space: Space,
    /// 家庭云 id（切到家庭云后才有）
    pub family_id: String,
}

/// 一次目录列表的结果
#[derive(Debug, Clone)]
pub struct ListResult {
    /// 被列出的目录（规范化后的绝对路径）
    pub path: CloudPath,
    /// 目录 id
    pub folder_id: String,
    /// 子项（目录在前，随后按名称升序）
    pub items: Vec<FileObject>,
}

/// 容量视图（已按当前空间取值）
#[derive(Debug, Clone, Copy, Default)]
pub struct CapacityView {
    pub total: i64,
    pub used: i64,
    pub free: i64,
    pub space: Space,
}

/// 服务内部可变状态
struct State {
    client: Option<Arc<TianyiClient>>,
    username: String,
    space: Space,
    family_id: String,
    expired: bool,
}

/// 共享云盘服务
pub struct CloudService {
    store: ConfigStore,
    state: Mutex<State>,
}

impl CloudService {
    /// 用配置存储构造服务（此时还没有会话）
    pub fn new(store: ConfigStore) -> Self {
        CloudService {
            store,
            state: Mutex::new(State {
                client: None,
                username: String::new(),
                space: Space::Personal,
                family_id: String::new(),
                expired: false,
            }),
        }
    }

    /// 配置存储引用
    pub fn store(&self) -> &ConfigStore {
        &self.store
    }

    /// 当前客户端（未登录时 None）
    pub fn client(&self) -> Option<Arc<TianyiClient>> {
        lock_or_recover(&self.state).client.clone()
    }

    /// 取客户端，未登录时给出可直接展示的错误
    pub fn require_client(&self) -> Result<Arc<TianyiClient>> {
        self.client()
            .ok_or_else(|| Error::NotAuthenticated("尚未登录，请先执行 login".to_string()))
    }

    /// 当前会话快照
    pub fn session(&self) -> SessionInfo {
        let st = lock_or_recover(&self.state);
        SessionInfo {
            authenticated: st.client.is_some() && !st.expired,
            expired: st.expired,
            username: st.username.clone(),
            space: st.space,
            family_id: st.family_id.clone(),
        }
    }

    /// 当前空间
    pub fn space(&self) -> Space {
        lock_or_recover(&self.state).space
    }

    // ==================================================================
    // 会话建立
    // ==================================================================
    /// 从已保存账号恢复会话
    ///
    /// preferred 为空时用配置里的 last_account。返回的快照里 authenticated=false 表示
    /// 「没有可用的已保存会话」，这不是错误——CLI 的 status / 需要登录的命令各自处理。
    pub async fn restore(&self, preferred: Option<&str>) -> Result<SessionInfo> {
        let cfg = self.store.load_config()?;
        let accounts = self.store.load_accounts()?;
        let wanted = preferred
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| (!cfg.last_account.is_empty()).then(|| cfg.last_account.clone()));
        let Some(name) = wanted else {
            return Ok(SessionInfo::default());
        };
        let Some(acc) = accounts.find(&name).cloned() else {
            // 配置记着上次账号、账号表里却没有：如实说明，不要静默当成未登录
            return Ok(SessionInfo {
                authenticated: false,
                username: name,
                ..Default::default()
            });
        };
        if acc.token.access_token.is_empty() && acc.token.session_key.is_empty() {
            return Ok(SessionInfo {
                authenticated: false,
                username: acc.username.clone(),
                ..Default::default()
            });
        }
        let space = if acc.is_family() {
            Space::Family
        } else {
            Space::Personal
        };
        let family_id = acc.family_id.clone();
        let client = Arc::new(TianyiClient::new(cfg, acc));
        self.install(client.clone(), &client.account().username, space, family_id);

        // 尽力刷新：拿到新 session key 就回写，失败要区分「网络抖」和「明确失效」
        if !client.token().access_token.is_empty() {
            match client.refresh_session().await {
                Ok(()) => self.persist_token(&client),
                Err(e) if e.is_session_invalid() => {
                    log::warn!("已保存会话确认失效：{e}");
                    lock_or_recover(&self.state).expired = true;
                }
                Err(e) => log::warn!("恢复会话时刷新失败（稍后会自动重试）：{e}"),
            }
        }
        Ok(self.session())
    }

    /// 直接用已保存的账号建立会话（account use）
    ///
    /// 与 [CloudService::restore] 的差别是**必须成功**：账号不存在或没有可用 token 时
    /// 报错，而不是返回一个未认证快照。
    pub async fn use_account(&self, username: &str) -> Result<SessionInfo> {
        let cfg = self.store.load_config()?;
        let accounts = self.store.load_accounts()?;
        let acc = accounts
            .find(username)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("账号不存在：{username}")))?;
        if acc.token.access_token.is_empty() && acc.token.session_key.is_empty() {
            return Err(Error::NotAuthenticated(format!(
                "账号 {username} 没有可用的登录态，请重新登录"
            )));
        }
        let space = if acc.is_family() {
            Space::Family
        } else {
            Space::Personal
        };
        let family_id = acc.family_id.clone();
        let client = Arc::new(TianyiClient::new(cfg.clone(), acc));
        self.install(client.clone(), username, space, family_id);
        match client.refresh_session().await {
            Ok(()) => self.persist_token(&client),
            Err(e) if e.is_session_invalid() => {
                lock_or_recover(&self.state).expired = true;
                return Err(Error::Session(format!(
                    "账号 {username} 的登录态已失效：{e}"
                )));
            }
            Err(e) => log::warn!("切换账号后刷新失败（稍后会自动重试）：{e}"),
        }
        // 记住这次选择：下次不带 --account 就是它
        let _ = self
            .store
            .update_config(|cfg| cfg.last_account = username.to_string());
        Ok(self.session())
    }

    /// 密码登录（成功后立即持久化）
    pub async fn login_password(
        &self,
        username: &str,
        password: &str,
        captcha: Option<&str>,
        notifier: &dyn LoginNotifier,
    ) -> Result<SessionInfo> {
        let http = build_login_http()?;
        let dummy = TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
        let session =
            api::login_by_password(&dummy, &http, username, password, captcha, notifier).await?;
        let token = api::token_from_session(&session);
        let uname = if session.login_name.trim().is_empty() {
            username.trim().to_string()
        } else {
            session.login_name.clone()
        };
        self.finish_login(&uname, token)
    }

    /// 二维码登录（二维码内容与状态通过 notifier 回调交给前端渲染）
    ///
    /// timeout 为 None 时一直轮询到成功/失败（由用户 Ctrl+C 结束）。
    pub async fn login_qr(
        &self,
        notifier: &dyn LoginNotifier,
        timeout: Option<Duration>,
    ) -> Result<SessionInfo> {
        let http = build_login_http()?;
        let dummy = TianyiClient::new(AppConfig::default_config(), AccountConfig::default());
        let param = api::login_by_qrcode(&dummy, &http, notifier).await?;
        let poll = api::poll_qr_login(&http, &param, notifier);
        let session = match timeout {
            Some(d) => tokio::time::timeout(d, poll)
                .await
                .map_err(|_| Error::Login(format!("二维码登录超时（{} 秒）", d.as_secs())))??,
            None => poll.await?,
        };
        let token = api::token_from_session(&session);
        let uname = session.login_name.clone();
        self.finish_login(&uname, token)
    }

    /// 登录成功后的统一收尾：写账号表 + 记 last_account + 安装客户端
    fn finish_login(&self, username: &str, token: TokenInfo) -> Result<SessionInfo> {
        let username = username.trim().to_string();
        if username.is_empty() {
            return Err(Error::Login("登录响应里没有账号名".to_string()));
        }
        let cfg = self.store.load_config()?;
        let acc = self.store.update_accounts(|accounts| {
            let mut acc = accounts.find(&username).cloned().unwrap_or(AccountConfig {
                username: username.clone(),
                ..Default::default()
            });
            acc.username = username.clone();
            acc.token = token;
            acc.login_type = "password".to_string();
            accounts.upsert(acc.clone());
            acc
        })?;
        self.store
            .update_config(|cfg| cfg.last_account = username.clone())?;
        let space = if acc.is_family() {
            Space::Family
        } else {
            Space::Personal
        };
        let family_id = acc.family_id.clone();
        let client = Arc::new(TianyiClient::new(cfg, acc));
        self.install(client, &username, space, family_id);
        Ok(self.session())
    }

    /// 退出登录：只清「当前会话」与 last_account，不删已保存账号
    ///
    /// 与 GUI 的行为保持一致：用户点退出后再进来，仍能在账号列表里一键切回。
    pub fn logout(&self) -> Result<SessionInfo> {
        {
            let mut st = lock_or_recover(&self.state);
            st.client = None;
            st.username = String::new();
            st.space = Space::Personal;
            st.family_id = String::new();
            st.expired = false;
        }
        self.store.update_config(|cfg| cfg.last_account.clear())?;
        Ok(self.session())
    }

    /// 切换到指定云空间（家庭云会先取到 familyId）
    pub async fn switch_space(&self, space: Space) -> Result<()> {
        let client = self.require_client()?;
        match space {
            Space::Personal => {
                client.set_account(|acc| acc.space_type = "personal".to_string());
                lock_or_recover(&self.state).space = Space::Personal;
                Ok(())
            }
            Space::Family => {
                client.set_account(|acc| acc.space_type = "family".to_string());
                match client.ensure_family_id().await {
                    Ok(id) => {
                        let mut st = lock_or_recover(&self.state);
                        st.space = Space::Family;
                        st.family_id = id;
                        Ok(())
                    }
                    Err(e) => {
                        // 回退个人云：否则后续每个请求都会带着 familyId 打过去
                        client.set_account(|acc| acc.space_type = "personal".to_string());
                        lock_or_recover(&self.state).space = Space::Personal;
                        Err(e)
                    }
                }
            }
        }
    }

    /// 家庭云可用性检查：给不支持的操作给出明确错误
    fn ensure_personal(&self, op: &str) -> Result<()> {
        if self.space() == Space::Family {
            return Err(Error::Unsupported(format!(
                "家庭云暂不支持 {op}，请先执行 space personal 切回个人云"
            )));
        }
        Ok(())
    }

    fn install(&self, client: Arc<TianyiClient>, username: &str, space: Space, family_id: String) {
        let mut st = lock_or_recover(&self.state);
        st.client = Some(client);
        st.username = username.to_string();
        st.space = space;
        st.family_id = family_id;
        st.expired = false;
    }

    /// 把客户端里最新的 token 写回账号表（失败只记日志）
    fn persist_token(&self, client: &Arc<TianyiClient>) {
        let username = lock_or_recover(&self.state).username.clone();
        if username.is_empty() {
            return;
        }
        let token = client.token();
        if let Err(e) = self.store.save_token(&username, &token) {
            log::warn!("回写 token 失败（不影响本次会话）：{e}");
        }
    }

    /// 带「会话失效 → 刷新 → 存盘 → 重试一次」的统一调用
    ///
    /// 只对**幂等**操作使用：列目录、查容量、搜文件、取直链都是只读的，重试安全。
    /// 写操作（删除、移动、重命名）刻意不走这里，避免「超时但已生效」时重复执行。
    pub async fn call<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: Fn(Arc<TianyiClient>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let client = self.require_client()?;
        match f(client.clone()).await {
            Err(e) if e.is_session_invalid() => {
                log::info!("会话失效，尝试刷新后重试");
                client.refresh_session().await.map_err(|e| {
                    lock_or_recover(&self.state).expired = true;
                    Error::Session(format!("会话已失效，请重新登录：{e}"))
                })?;
                self.persist_token(&client);
                f(client).await
            }
            other => other,
        }
    }

    // ==================================================================
    // 文件操作
    // ==================================================================
    /// 列出目录（原始 id 版本）
    pub async fn list_raw(&self, folder_id: &str) -> Result<Vec<FileObject>> {
        let id = folder_id.to_string();
        let mut items = self
            .call(move |client| {
                let id = id.clone();
                async move { client.list_files(&id).await }
            })
            .await?;
        sort_items(&mut items);
        Ok(items)
    }

    /// 列出目录（路径版本）
    pub async fn list(&self, path: &CloudPath) -> Result<ListResult> {
        let folder_id = self.resolve_dir(path).await?;
        let items = self.list_raw(&folder_id).await?;
        Ok(ListResult {
            path: path.clone(),
            folder_id,
            items,
        })
    }

    /// 把路径解析成目录 id（逐段列目录比对名称）
    pub async fn resolve_dir(&self, path: &CloudPath) -> Result<String> {
        let mut folder_id = ROOT_FOLDER_ID.to_string();
        let mut walked = CloudPath::root();
        for seg in path.segments() {
            let listing = self.list_raw(&folder_id).await?;
            walked = walked.child(seg);
            match listing.into_iter().find(|f| f.is_dir && f.name == *seg) {
                Some(dir) => folder_id = dir.id,
                None => return Err(Error::NotFound(format!("目录不存在：{}", walked.display()))),
            }
        }
        Ok(folder_id)
    }

    /// 把路径解析成对象（文件或目录）
    pub async fn resolve_object(&self, path: &CloudPath) -> Result<FileObject> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::InvalidRequest("根目录不是文件".to_string()))?;
        let name = path.name().unwrap_or_default().to_string();
        let parent_id = self.resolve_dir(&parent).await?;
        let listing = self.list_raw(&parent_id).await?;
        listing
            .into_iter()
            .find(|f| f.name == name)
            .ok_or_else(|| Error::NotFound(format!("文件或目录不存在：{}", path.display())))
    }

    /// 新建文件夹（父目录必须已存在）
    pub async fn mkdir(&self, path: &CloudPath) -> Result<FolderObject> {
        self.ensure_personal("新建文件夹")?;
        let name = path
            .name()
            .ok_or_else(|| Error::InvalidRequest("缺少文件夹名".to_string()))?
            .to_string();
        let parent = path
            .parent()
            .ok_or_else(|| Error::InvalidRequest("不能在根目录上新建目录".to_string()))?;
        let parent_id = self.resolve_dir(&parent).await?;
        let client = self.require_client()?;
        client.create_folder(&parent_id, &name).await
    }

    /// 重命名（只改名字，不移动）
    pub async fn rename(&self, path: &CloudPath, new_name: &str) -> Result<()> {
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.contains('/') {
            return Err(Error::InvalidRequest(format!("非法的目标名：{new_name}")));
        }
        let obj = self.resolve_object(path).await?;
        let client = self.require_client()?;
        client.rename(&obj, new_name).await
    }

    /// 删除（目录会在服务端递归删除，因此必须显式确认递归）
    pub async fn remove(&self, path: &CloudPath, recursive: bool) -> Result<FileObject> {
        let obj = self.resolve_object(path).await?;
        if obj.is_dir && !recursive {
            return Err(Error::InvalidRequest(format!(
                "{} 是目录，删除目录需要 --recursive",
                path.display()
            )));
        }
        let client = self.require_client()?;
        client.delete(&obj).await?;
        Ok(obj)
    }

    /// 移动到目标目录
    pub async fn move_to(&self, src: &CloudPath, dst_dir: &CloudPath) -> Result<FileObject> {
        let obj = self.resolve_object(src).await?;
        let dst_id = self.resolve_dir(dst_dir).await?;
        let client = self.require_client()?;
        client.move_to(&obj, &dst_id).await?;
        Ok(obj)
    }

    /// 复制到目标目录
    pub async fn copy_to(&self, src: &CloudPath, dst_dir: &CloudPath) -> Result<FileObject> {
        let obj = self.resolve_object(src).await?;
        let dst_id = self.resolve_dir(dst_dir).await?;
        let client = self.require_client()?;
        client.copy_to(&obj, &dst_id).await?;
        Ok(obj)
    }

    /// 取下载直链（要求是文件）
    pub async fn download_url(&self, path: &CloudPath) -> Result<String> {
        let obj = self.resolve_object(path).await?;
        if obj.is_dir {
            return Err(Error::InvalidRequest(format!(
                "{} 是目录，直链只对文件有效",
                path.display()
            )));
        }
        let id = obj.id.clone();
        self.call(move |client| {
            let id = id.clone();
            async move { client.get_download_url(&id).await }
        })
        .await
    }

    /// 容量（按当前空间取值）
    pub async fn capacity(&self) -> Result<CapacityView> {
        let cap = self
            .call(|client| async move { client.get_capacity().await })
            .await?;
        let space = self.space();
        let info = if space == Space::Family {
            cap.family_capacity_info
        } else {
            cap.cloud_capacity_info
        };
        Ok(CapacityView {
            total: info.total_size,
            used: info.used_size,
            free: info.free_size,
            space,
        })
    }

    /// 搜索（服务端搜索，返回文件名匹配项）
    pub async fn search(&self, keyword: &str) -> Result<Vec<FileObject>> {
        let kw = keyword.to_string();
        let mut items = self
            .call(move |client| {
                let kw = kw.clone();
                async move { client.search_files(&kw).await }
            })
            .await?;
        sort_items(&mut items);
        Ok(items)
    }

    /// 对象详情（路径解析 + 带上父目录 id）
    pub async fn info(&self, path: &CloudPath) -> Result<FileObject> {
        let mut obj = self.resolve_object(path).await?;
        if obj.parent_id.is_empty() {
            if let Some(parent) = path.parent() {
                if let Ok(id) = self.resolve_dir(&parent).await {
                    obj.parent_id = id;
                }
            }
        }
        Ok(obj)
    }

    /// 账号可用空间是否还够（上传前预检，只做提示不做拦截）
    pub async fn has_free_space(&self, need: u64) -> bool {
        match self.capacity().await {
            Ok(cap) => cap.free <= 0 || need <= cap.free as u64,
            Err(_) => true,
        }
    }
}

/// 登录用 HTTP 客户端（需要 cookie 复用：验证码会话与扫码会话都靠它）
fn build_login_http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .map_err(|e| Error::Other(format!("构建 HTTP 客户端失败：{e}")))
}

/// 目录项排序：目录在前，然后按名称升序（大小写不敏感，大小写差异时回到原始字节序）
///
/// 不用 locale 排序：GUI 与 CLI 必须得到完全一致的顺序，而 locale 排序结果依赖系统
/// 语言环境，两台机器上就会不一样。
pub fn sort_items(items: &mut [FileObject]) {
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let an = a.name.to_lowercase();
            let bn = b.name.to_lowercase();
            an.cmp(&bn).then_with(|| a.name.cmp(&b.name))
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FileObject;

    fn item(name: &str, is_dir: bool) -> FileObject {
        FileObject {
            name: name.to_string(),
            is_dir,
            ..Default::default()
        }
    }

    #[test]
    fn space_parses_all_documented_aliases() {
        assert_eq!(Space::parse("personal"), Some(Space::Personal));
        assert_eq!(Space::parse("个人云"), Some(Space::Personal));
        assert_eq!(Space::parse("FAMILY"), Some(Space::Family));
        assert_eq!(Space::parse("家庭云"), Some(Space::Family));
        assert_eq!(Space::parse("weird"), None);
        assert_eq!(Space::Personal.as_str(), "personal");
        assert_eq!(Space::Family.as_str(), "family");
        // 往返
        for s in [Space::Personal, Space::Family] {
            assert_eq!(Space::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn sort_puts_folders_first_then_case_insensitive_name() {
        let mut items = vec![
            item("b.txt", false),
            item("A", true),
            item("a.txt", false),
            item("C", true),
        ];
        sort_items(&mut items);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["A", "C", "a.txt", "b.txt"]);
    }

    #[test]
    fn anonymous_session_is_not_authenticated() {
        let dir = std::env::temp_dir().join(format!("tianyi-svc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::open_at(dir.clone()).expect("store");
        let svc = CloudService::new(store);
        let s = svc.session();
        assert!(!s.authenticated);
        assert!(!s.expired);
        assert_eq!(s.space, Space::Personal);
        // 未登录时必须给出明确的 NotAuthenticated，而不是别的错误
        assert!(matches!(
            svc.require_client(),
            Err(Error::NotAuthenticated(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn family_space_rejects_personal_only_operations() {
        let dir = std::env::temp_dir().join(format!("tianyi-svc-fam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::open_at(dir.clone()).expect("store");
        let svc = CloudService::new(store);
        lock_or_recover(&svc.state).space = Space::Family;
        let err = svc.ensure_personal("新建文件夹").expect_err("家庭云应拒绝");
        assert!(err.to_string().contains("家庭云"), "实际：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
