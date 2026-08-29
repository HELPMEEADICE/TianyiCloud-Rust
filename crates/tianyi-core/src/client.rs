//! 天翼云盘 API 客户端
//!
//! 移植自 OpenList `drivers/189pc`，实现了签名请求、登录、会话刷新、
//! 文件列表/操作、批量任务、上传、下载等。

use crate::config::AppConfig;
use crate::crypto;
use crate::error::{Error, Result};
use crate::models::*;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;

/// 天翼云盘常量
pub mod consts {
    pub const ACCOUNT_TYPE: &str = "02";
    pub const APP_ID: &str = "8025431004";
    pub const CLIENT_TYPE: &str = "10020";
    pub const VERSION: &str = "6.2";

    pub const WEB_URL: &str = "https://cloud.189.cn";
    pub const AUTH_URL: &str = "https://open.e.189.cn";
    pub const API_URL: &str = "https://api.cloud.189.cn";
    pub const UPLOAD_URL: &str = "https://upload.cloud.189.cn";

    pub const RETURN_URL: &str = "https://m.cloud.189.cn/zhuanti/2020/loginErrorPc/index.html";

    pub const PC: &str = "TELEPC";
    pub const MAC: &str = "TELEMAC";
    pub const CHANNEL_ID: &str = "web_cloud.189.cn";

    pub const USER_INVALID_OPEN_TOKEN: &str = "UserInvalidOpenToken";
}

/// 天翼云盘 API 客户端
pub struct TianyiClient {
    http: Client,
    config: AppConfig,
    account: std::sync::Mutex<AccountConfig>,
    /// 会话缓存（刷新后更新），内部可变以便并发刷新
    session: std::sync::Mutex<SessionCache>,
}

/// 会话缓存（刷新后更新）
#[derive(Debug, Clone)]
struct SessionCache {
    token: TokenInfo,
}

impl Default for SessionCache {
    fn default() -> Self {
        SessionCache {
            token: TokenInfo::default(),
        }
    }
}

impl TianyiClient {
    /// 使用配置和账号创建客户端
    pub fn new(config: AppConfig, account: AccountConfig) -> Self {
        let http = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
            .cookie_store(true)
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                if let Ok(v) = reqwest::header::HeaderValue::from_str("application/json;charset=UTF-8") {
                    h.insert(reqwest::header::ACCEPT, v);
                }
                if let Ok(v) = reqwest::header::HeaderValue::from_str(consts::WEB_URL) {
                    h.insert(reqwest::header::REFERER, v);
                }
                h
            })
            .timeout(Duration::from_secs(120))
            .build()
            .expect("build http client");
        let mut session = SessionCache::default();
        session.token = account.token.clone();
        TianyiClient {
            http,
            config,
            account: std::sync::Mutex::new(account),
            session: std::sync::Mutex::new(session),
        }
    }

    /// 获取账号配置
    pub fn account(&self) -> AccountConfig {
        self.account.lock().unwrap().clone()
    }

    /// 修改账号配置（如切换空间类型）
    pub fn set_account<F: FnOnce(&mut AccountConfig)>(&self, f: F) {
        let mut acc = self.account.lock().unwrap();
        f(&mut acc);
    }

    /// 获取账号配置的可变引用
    pub fn account_mut(&self) -> std::sync::MutexGuard<'_, AccountConfig> {
        self.account.lock().unwrap()
    }

    /// 获取会话 token
    pub fn token(&self) -> TokenInfo {
        self.session.lock().unwrap().token.clone()
    }

    /// 获取会话 token 引用（调用方需在锁内）
    fn session_token(&self) -> TokenInfo {
        self.session.lock().unwrap().token.clone()
    }

    /// 判断是否家庭云
    pub fn is_family(&self) -> bool {
        self.account().is_family()
    }

    /// 获取家庭云 ID
    pub fn family_id(&self) -> String {
        self.account().family_id.clone()
    }

    /// 对参数进行 AES 加密
    fn encrypt_params(&self, params: &HashMap<String, String>, is_family: bool) -> Result<String> {
        let tok = self.session_token();
        let secret = if is_family {
            tok.family_session_secret.clone()
        } else {
            tok.session_secret.clone()
        };
        if params.is_empty() {
            return Ok(String::new());
        }
        let mut keys: Vec<&String> = params.keys().collect();
        keys.sort();
        let mut buf = String::new();
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                buf.push('&');
            }
            buf.push_str(k);
            buf.push('=');
            buf.push_str(&params[*k]);
        }
        crypto::aes_ecb_encrypt(&buf, &secret[..16.min(secret.len())])
    }

    /// 生成签名头
    fn signature_header(
        &self,
        url: &str,
        method: &str,
        params_enc: &str,
        is_family: bool,
    ) -> HeaderMap {
        let (session_key, session_secret) = if is_family {
            self.session_keys_family()
        } else {
            self.session_keys_personal()
        };
        let date = crypto::http_date();
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&date) {
            headers.insert("Date", v);
        }
        if let Ok(v) = HeaderValue::from_str(&session_key) {
            headers.insert("SessionKey", v);
        }
        let xid = uuid::Uuid::new_v4().to_string();
        if let Ok(v) = HeaderValue::from_str(&xid) {
            headers.insert("X-Request-ID", v);
        }
        let sig = crypto::signature(&session_secret, &session_key, method, url, &date, params_enc);
        if let Ok(v) = HeaderValue::from_str(&sig) {
            headers.insert("Signature", v);
        }
        headers
    }

    fn session_keys_personal(&self) -> (String, String) {
        let tok = self.session_token();
        (tok.session_key.clone(), tok.session_secret.clone())
    }

    fn session_keys_family(&self) -> (String, String) {
        let tok = self.session_token();
        (
            tok.family_session_key.clone(),
            tok.family_session_secret.clone(),
        )
    }

    /// 会话刷新
    pub async fn refresh_session(&self) -> Result<()> {
        let access_token = self.session_token().access_token;
        let mut query: Vec<(String, String)> = crypto::client_suffix();
        query.push(("appId".to_string(), consts::APP_ID.to_string()));
        query.push(("accessToken".to_string(), access_token));

        let resp = self
            .http
            .get(format!("{}/getSessionForPC.action", consts::API_URL))
            .query(&query)
            .header(
                reqwest::header::ACCEPT,
                "application/json;charset=UTF-8",
            )
            .send()
            .await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        let text = String::from_utf8_lossy(&body);

        if !status.is_success() {
            return Err(Error::Api(format!("refresh session http {status}")));
        }

        // 检查错误
        if let Ok(er) = serde_json::from_str::<RespErr>(&text) {
            if er.has_error() {
                if er.error_code == consts::USER_INVALID_OPEN_TOKEN
                    || er.code == consts::USER_INVALID_OPEN_TOKEN
                {
                    return self.refresh_token().await;
                }
                return Err(Error::Api(er.message()));
            }
        }

        let us: UserSessionResp =
            serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse session: {e}")))?;
        log::info!(
            "refresh_session ok: session_key_len={} session_secret_len={} family_key_len={} family_secret_len={} login_name={}",
            us.session_key.len(), us.session_secret.len(),
            us.family_session_key.len(), us.family_session_secret.len(),
            us.login_name
        );
        let mut session = self.session.lock().unwrap();
        session.token.session_key = us.session_key;
        session.token.session_secret = us.session_secret;
        session.token.family_session_key = us.family_session_key;
        session.token.family_session_secret = us.family_session_secret;
        session.token.login_name = us.login_name;
        Ok(())
    }

    /// token 刷新
    pub async fn refresh_token(&self) -> Result<()> {
        let refresh_token = self.session_token().refresh_token;
        let form = [
            ("clientId", consts::APP_ID),
            ("refreshToken", &refresh_token),
            ("grantType", "refresh_token"),
            ("format", "json"),
        ];

        let resp = self
            .http
            .post(format!("{}/api/oauth2/refreshToken.do", consts::AUTH_URL))
            .header(
                reqwest::header::ACCEPT,
                "application/json;charset=UTF-8",
            )
            .form(&form)
            .send()
            .await?;
        let body = resp.bytes().await?;
        let text = String::from_utf8_lossy(&body);

        if let Ok(er) = serde_json::from_str::<RespErr>(&text) {
            if er.has_error() {
                return Err(Error::Login(format!(
                    "token refresh failed: {}",
                    er.message()
                )));
            }
        }

        let tr: AppSessionResp = serde_json::from_str(&text)
            .map_err(|e| Error::Api(format!("parse token resp: {e}")))?;
        let mut session = self.session.lock().unwrap();
        session.token.access_token = tr.access_token;
        session.token.refresh_token = tr.refresh_token;
        Ok(())
    }

    /// keepalive（每 5 分钟调用）
    pub async fn keep_alive(&self) -> Result<()> {
        self.get_raw(
            &format!("{}/keepUserSession.action", consts::API_URL),
            HashMap::new(),
        )
        .await
        .map(|_| ())
    }

    /// 发起带签名的请求，返回 JSON 文本（自动处理会话失效）
    ///
    /// 参数以明文 query 形式发送，签名不含 params（与 OpenList 的文件操作一致）
    async fn signed_get_json(&self, url: &str, params: HashMap<String, String>, is_family: bool) -> Result<String> {
        let mut query: Vec<(String, String)> = crypto::client_suffix();
        query.extend(params.iter().map(|(k, v)| (k.clone(), v.clone())));
        let headers = self.signature_header(url, "GET", "", is_family);
        log::debug!(
            "signed GET {} is_family={} key={} secret_len={}",
            url,
            is_family,
            headers.get("SessionKey").and_then(|v| v.to_str().ok()).unwrap_or(""),
            headers.get("Signature").and_then(|v| v.to_str().ok()).map(|s| s.len()).unwrap_or(0),
        );

        let resp = self.http.get(url).query(&query).headers(headers).send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        let text = String::from_utf8_lossy(&body).to_string();

        if text.contains("userSessionBO is null") || text.contains("InvalidSessionKey") {
            return Err(Error::Session("session invalid".into()));
        }

        if !status.is_success() {
            return Err(Error::Api(format!("http {status}: {}", text.chars().take(500).collect::<String>())));
        }

        // 业务错误
        if let Ok(er) = serde_json::from_str::<RespErr>(&text) {
            if er.has_error() {
                return Err(Error::Api(er.message()));
            }
        }
        Ok(text)
    }

    /// 发起带签名的 POST 表单请求（明文表单，签名不含 params）
    async fn signed_post_form(
        &self,
        url: &str,
        form: &[(&str, String)],
        is_family: bool,
    ) -> Result<String> {
        let headers = self.signature_header(url, "POST", "", is_family);
        let query: Vec<(String, String)> = crypto::client_suffix();

        let resp = self
            .http
            .post(url)
            .query(&query)
            .headers(headers)
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        let text = String::from_utf8_lossy(&body).to_string();

        if text.contains("userSessionBO is null") || text.contains("InvalidSessionKey") {
            return Err(Error::Session("session invalid".into()));
        }

        if !status.is_success() {
            return Err(Error::Api(format!("http {status}: {}", text.chars().take(500).collect::<String>())));
        }

        if let Ok(er) = serde_json::from_str::<RespErr>(&text) {
            if er.has_error() {
                return Err(Error::Api(er.message()));
            }
        }
        Ok(text)
    }

    /// 发起带签名的 GET，params 加密进 `params` 查询参数（供上传接口使用）
    async fn signed_get_json_encrypted(
        &self,
        url: &str,
        params: HashMap<String, String>,
        is_family: bool,
    ) -> Result<String> {
        let enc_params = self.encrypt_params(&params, is_family)?;
        let mut query: Vec<(String, String)> = crypto::client_suffix();
        if !enc_params.is_empty() {
            query.push(("params".to_string(), enc_params.clone()));
        }
        let headers = self.signature_header(url, "GET", &enc_params, is_family);

        let resp = self.http.get(url).query(&query).headers(headers).send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        let text = String::from_utf8_lossy(&body).to_string();

        if text.contains("userSessionBO is null") || text.contains("InvalidSessionKey") {
            return Err(Error::Session("session invalid".into()));
        }

        if !status.is_success() {
            return Err(Error::Api(format!("http {status}: {}", text.chars().take(500).collect::<String>())));
        }

        if let Ok(er) = serde_json::from_str::<RespErr>(&text) {
            if er.has_error() {
                return Err(Error::Api(er.message()));
            }
        }
        Ok(text)
    }

    /// 带签名的 GET（返回原始 JSON 文本）
    pub async fn get_json(
        &self,
        url: &str,
        params: HashMap<String, String>,
        is_family: bool,
    ) -> Result<String> {
        self.signed_get_json(url, params, is_family).await
    }

    /// 带签名的 POST 表单（返回原始 JSON 文本）
    pub async fn post_form(
        &self,
        url: &str,
        form: &[(&str, String)],
        is_family: bool,
    ) -> Result<String> {
        self.signed_post_form(url, form, is_family).await
    }

    /// 带签名的 GET（params 加密进 `params` 查询参数，供上传等接口使用）
    pub async fn get_json_encrypted(
        &self,
        url: &str,
        params: HashMap<String, String>,
        is_family: bool,
    ) -> Result<String> {
        self.signed_get_json_encrypted(url, params, is_family).await
    }

    /// 无签名 GET（返回原始响应 body，用于验证码图片等）
    pub async fn get_raw(&self, url: &str, query: HashMap<String, String>) -> Result<bytes::Bytes> {        let mut req = self.http.get(url);
        if !query.is_empty() {
            req = req.query(&query);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        if !status.is_success() {
            return Err(Error::Api(format!("http {status}")));
        }
        Ok(body)
    }

    /// 无签名 POST 表单
    pub async fn post_raw(&self, url: &str, form: &[(&str, String)]) -> Result<bytes::Bytes> {
        let resp = self.http.post(url).form(form).send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        if !status.is_success() {
            return Err(Error::Api(format!("http {status}")));
        }
        Ok(body)
    }

    /// 获取文件列表
    pub async fn list_files(&self, folder_id: &str) -> Result<Vec<FileObject>> {
        let mut all = Vec::new();
        let is_family = self.is_family();
        let page_size = 1000i64;
        let mut page_num = 1i64;

        loop {
            let mut params = HashMap::new();
            params.insert("folderId".to_string(), folder_id.to_string());
            params.insert("fileType".to_string(), "0".to_string());
            params.insert("mediaAttr".to_string(), "0".to_string());
            params.insert("iconOption".to_string(), "5".to_string());
            params.insert("pageNum".to_string(), page_num.to_string());
            params.insert("pageSize".to_string(), page_size.to_string());
            params.insert("orderBy".to_string(), self.account().order_by.clone());
            params.insert(
                "descending".to_string(),
                if self.account().order_direction == "desc" { "true".to_string() } else { "false".to_string() },
            );
            if is_family {
                params.insert("familyId".to_string(), self.account().family_id.clone());
                params.insert("recursive".to_string(), "0".to_string());
            } else {
                params.insert("recursive".to_string(), "0".to_string());
            }

            let url = if is_family {
                format!("{}/family/file/listFiles.action", consts::API_URL)
            } else {
                format!("{}/listFiles.action", consts::API_URL)
            };

            let text = match self.signed_get_json(&url, params, is_family).await {
                Ok(t) => {
                    log::info!("listFiles resp: {}", t.chars().take(400).collect::<String>());
                    t
                }
                Err(Error::Session(_)) => {
                    // 标记会话失效，由上层统一刷新；这里直接返回错误，
                    // 调用方（ApiClient）会处理刷新重试
                    return Err(Error::Session("session invalid".into()));
                }
                Err(e) => return Err(e),
            };

            let resp: Cloud189FilesResp = serde_json::from_str(&text)
                .map_err(|e| Error::Api(format!("parse list: {e}")))?;

            let count = resp.file_list_ao.count;
            for f in &resp.file_list_ao.file_list {
                all.push(FileObject {
                    id: f.id.as_string(),
                    name: f.name.clone(),
                    size: f.size,
                    md5: f.md5.clone(),
                    parent_id: folder_id.to_string(),
                    is_dir: false,
                    last_op_time: f.last_op_time.clone(),
                    create_date: f.create_date.clone(),
                    thumb: f.icon.small_url.clone(),
                });
            }
            for d in &resp.file_list_ao.folder_list {
                all.push(FileObject {
                    id: d.id.as_string(),
                    name: d.name.clone(),
                    size: 0,
                    md5: String::new(),
                    parent_id: folder_id.to_string(),
                    is_dir: true,
                    last_op_time: d.last_op_time.clone(),
                    create_date: d.create_date.clone(),
                    thumb: String::new(),
                });
            }

            let page_count = resp.file_list_ao.folder_list.len() + resp.file_list_ao.file_list.len();
            if count == 0 || page_count < page_size as usize {
                break;
            }
            page_num += 1;
        }
        Ok(all)
    }

    /// 获取下载直链
    pub async fn get_download_url(&self, file_id: &str) -> Result<String> {
        let is_family = self.is_family();
        let mut params = HashMap::new();
        params.insert("fileId".to_string(), file_id.to_string());
        if is_family {
            params.insert("familyId".to_string(), self.account().family_id.clone());
        } else {
            params.insert("dt".to_string(), "3".to_string());
            params.insert("flag".to_string(), "1".to_string());
        }

        let url = if is_family {
            format!("{}/family/file/getFileDownloadUrl.action", consts::API_URL)
        } else {
            format!("{}/getFileDownloadUrl.action", consts::API_URL)
        };

        let text = self.signed_get_json(&url, params, is_family).await?;
        let resp: DownloadUrlResp =
            serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse dl url: {e}")))?;
        let mut dl = resp.file_download_url;
        dl = dl.replace("&amp;", "&");
        if dl.starts_with("http://") {
            dl = format!("https://{}", &dl[7..]);
        }
        Ok(dl)
    }

    /// 创建文件夹
    pub async fn create_folder(&self, parent_id: &str, name: &str) -> Result<FolderObject> {
        let is_family = self.is_family();
        let mut params = HashMap::new();
        params.insert("folderName".to_string(), name.to_string());
        params.insert("relativePath".to_string(), String::new());
        if is_family {
            params.insert("familyId".to_string(), self.account().family_id.clone());
            params.insert("parentId".to_string(), parent_id.to_string());
        } else {
            params.insert("parentFolderId".to_string(), parent_id.to_string());
        }
        let url = if is_family {
            format!("{}/family/file/createFolder.action", consts::API_URL)
        } else {
            format!("{}/createFolder.action", consts::API_URL)
        };
        let text = self.signed_get_json(&url, params, is_family).await?;
        let resp: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse folder: {e}")))?;
        Ok(FolderObject {
            id: resp["id"].as_str().unwrap_or("").to_string(),
            name: name.to_string(),
            parent_id: resp["parentId"].as_i64().unwrap_or(0),
            is_dir: true,
            last_op_time: resp["lastOpTime"].as_str().unwrap_or("").to_string(),
            create_date: resp["createDate"].as_str().unwrap_or("").to_string(),
        })
    }

    /// 批量任务（MOVE/COPY/DELETE）
    async fn create_batch_task(
        &self,
        task_type: &str,
        target_folder_id: &str,
        tasks: &[BatchTaskInfo],
    ) -> Result<String> {
        let is_family = self.is_family();
        let task_infos = serde_json::to_string(tasks)?;
        let mut form: Vec<(&str, String)> = vec![
            ("type", task_type.to_string()),
            ("taskInfos", task_infos),
        ];
        if !target_folder_id.is_empty() {
            form.push(("targetFolderId", target_folder_id.to_string()));
        }
        if is_family {
            form.push(("familyId", self.account().family_id.clone()));
        }

        let url = format!("{}/batch/createBatchTask.action", consts::API_URL);
        let text = self.signed_post_form(&url, &form, is_family).await?;
        let resp: CreateBatchTaskResp =
            serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse task: {e}")))?;
        Ok(resp.task_id)
    }

    /// 检查批量任务状态
    async fn check_batch_task(&self, task_type: &str, task_id: &str) -> Result<BatchTaskStateResp> {
        let is_family = self.is_family();
        let form: Vec<(&str, String)> = vec![
            ("type", task_type.to_string()),
            ("taskId", task_id.to_string()),
        ];
        let url = format!("{}/batch/checkBatchTask.action", consts::API_URL);
        let text = self.signed_post_form(&url, &form, is_family).await?;
        let resp: BatchTaskStateResp = serde_json::from_str(&text)
            .map_err(|e| Error::Api(format!("parse task state: {e}")))?;
        Ok(resp)
    }

    /// 等待批量任务完成
    pub async fn wait_batch_task(
        &self,
        task_type: &str,
        task_id: &str,
        interval: Duration,
    ) -> Result<()> {
        loop {
            let state = self.check_batch_task(task_type, task_id).await?;
            match state.task_status {
                2 => return Err(Error::Api("conflict with target object".into())),
                4 => return Ok(()),
                _ => {}
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// 删除文件（到回收站）
    pub async fn delete(&self, obj: &FileObject) -> Result<()> {
        let tasks = vec![BatchTaskInfo::new(&obj.id, &obj.name, obj.is_dir)];
        let task_id = self.create_batch_task("DELETE", "", &tasks).await?;
        self.wait_batch_task("DELETE", &task_id, Duration::from_millis(200))
            .await
    }

    /// 永久删除（清空回收站中指定文件）
    pub async fn delete_permanent(&self, obj: &FileObject) -> Result<()> {
        let tasks = vec![BatchTaskInfo::new(&obj.id, &obj.name, obj.is_dir)];
        let task_id = self.create_batch_task("CLEAR_RECYCLE", "", &tasks).await?;
        self.wait_batch_task("CLEAR_RECYCLE", &task_id, Duration::from_millis(200))
            .await
    }

    /// 重命名
    pub async fn rename(&self, obj: &FileObject, new_name: &str) -> Result<()> {
        let is_family = self.is_family();
        let mut params = HashMap::new();
        if is_family {
            params.insert("familyId".to_string(), self.account().family_id.clone());
        }
        let (endpoint, id_key, name_key) = if obj.is_dir {
            ("renameFolder.action", "folderId", "destFolderName")
        } else {
            ("renameFile.action", "fileId", "destFileName")
        };
        params.insert(id_key.to_string(), obj.id.clone());
        params.insert(name_key.to_string(), new_name.to_string());

        let url = if is_family {
            format!("{}/family/file/{}", consts::API_URL, endpoint)
        } else {
            format!("{}/{}", consts::API_URL, endpoint)
        };
        self.signed_get_json(&url, params, is_family).await?;
        Ok(())
    }

    /// 移动
    pub async fn move_to(&self, src: &FileObject, dst_folder_id: &str) -> Result<()> {
        let tasks = vec![BatchTaskInfo::new(&src.id, &src.name, src.is_dir)];
        let task_id = self
            .create_batch_task("MOVE", dst_folder_id, &tasks)
            .await?;
        self.wait_batch_task("MOVE", &task_id, Duration::from_millis(400))
            .await
    }

    /// 复制
    pub async fn copy_to(&self, src: &FileObject, dst_folder_id: &str) -> Result<()> {
        let tasks = vec![BatchTaskInfo::new(&src.id, &src.name, src.is_dir)];
        let task_id = self
            .create_batch_task("COPY", dst_folder_id, &tasks)
            .await?;
        self.wait_batch_task("COPY", &task_id, Duration::from_secs(1))
            .await
    }

    /// 获取容量信息
    pub async fn get_capacity(&self) -> Result<CapacityResp> {
        let url = format!("{}/portal/getUserSizeInfo.action", consts::API_URL);
        let text = self.signed_get_json(&url, HashMap::new(), false).await?;
        let resp: CapacityResp = serde_json::from_str(&text)
            .map_err(|e| Error::Api(format!("parse capacity: {e}")))?;
        Ok(resp)
    }

    /// 获取家庭云列表
    pub async fn get_family_list(&self) -> Result<Vec<FamilyInfo>> {
        let url = format!("{}/family/manage/getFamilyList.action", consts::API_URL);
        let text = self.signed_get_json(&url, HashMap::new(), true).await?;
        let resp: FamilyListResp = serde_json::from_str(&text)
            .map_err(|e| Error::Api(format!("parse family list: {e}")))?;
        Ok(resp.family_info_resp)
    }

    /// 自动获取家庭云 ID（优先匹配登录名，否则取第一个）
    pub async fn ensure_family_id(&self) -> Result<String> {
        let existing = self.family_id();
        if !existing.is_empty() {
            return Ok(existing);
        }
        let infos = self.get_family_list().await?;
        if infos.is_empty() {
            return Err(Error::Api("无法获取家庭云，请手动配置 family_id".into()));
        }
        let login_name = self.token().login_name;
        // 优先匹配 remarkName 包含 loginName 前缀（如手机号）
        let mut chosen = None;
        for info in &infos {
            if !login_name.is_empty() && info.remark_name.contains(login_name.trim()) {
                chosen = Some(info.family_id);
                break;
            }
        }
        let id = chosen.unwrap_or(infos[0].family_id).to_string();
        self.set_account(|acc| acc.family_id = id.clone());
        Ok(id)
    }

    /// 搜索文件
    pub async fn search_files(&self, keyword: &str) -> Result<Vec<FileObject>> {
        let is_family = self.is_family();
        let mut all = Vec::new();
        let page_size = 60i64;
        let mut page_num = 1i64;
        loop {
            let mut params = HashMap::new();
            params.insert("pageSize".to_string(), page_size.to_string());
            params.insert("pageNum".to_string(), page_num.to_string());
            params.insert("iconOption".to_string(), "5".to_string());
            params.insert("fileType".to_string(), "0".to_string());
            params.insert("recursive".to_string(), "0".to_string());
            params.insert("orderBy".to_string(), "lastOpTime".to_string());
            params.insert("descending".to_string(), "true".to_string());
            params.insert("searchValue".to_string(), keyword.to_string());
            if is_family {
                params.insert("familyId".to_string(), self.account().family_id.clone());
            }
            let url = if is_family {
                format!("{}/family/file/searchFiles.action", consts::API_URL)
            } else {
                format!("{}/searchFiles.action", consts::API_URL)
            };
            let text = self.signed_get_json(&url, params, is_family).await?;
            let resp: SearchResp = serde_json::from_str(&text)
                .map_err(|e| Error::Api(format!("parse search: {e}")))?;
            let count = resp.file_list_ao.count;
            for f in &resp.file_list_ao.file_list {
                all.push(FileObject {
                    id: f.id.as_string(),
                    name: f.name.clone(),
                    size: f.size,
                    md5: f.md5.clone(),
                    parent_id: f.id.as_string(),
                    is_dir: false,
                    last_op_time: f.last_op_time.clone(),
                    create_date: f.create_date.clone(),
                    thumb: f.icon.small_url.clone(),
                });
            }
            for d in &resp.file_list_ao.folder_list {
                all.push(FileObject {
                    id: d.id.as_string(),
                    name: d.name.clone(),
                    size: 0,
                    md5: String::new(),
                    parent_id: d.id.as_string(),
                    is_dir: true,
                    last_op_time: d.last_op_time.clone(),
                    create_date: d.create_date.clone(),
                    thumb: String::new(),
                });
            }
            let page_count = resp.file_list_ao.folder_list.len() + resp.file_list_ao.file_list.len();
            if count == 0 || page_count < page_size as usize {
                break;
            }
            page_num += 1;
        }
        Ok(all)
    }

    /// 获取当前用户信息（用于登录态检查）
    pub async fn get_user_info(&self) -> Result<String> {
        let url = format!("{}/getUserInfo.action", consts::API_URL);
        let text = self.signed_get_json(&url, HashMap::new(), false).await?;
        Ok(text)
    }

    /// 获取 http 客户端引用（供上传下载复用 cookie）
    pub fn http_client(&self) -> &Client {
        &self.http
    }

    /// 克隆一个独立客户端实例（供并发任务使用，共享 cookie store）
    pub fn clone_for_transfer(&self) -> Self {
        TianyiClient {
            http: self.http.clone(),
            config: self.config.clone(),
            account: std::sync::Mutex::new(self.account().clone()),
            session: std::sync::Mutex::new(SessionCache {
                token: self.session_token(),
            }),
        }
    }
}
