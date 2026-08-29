//! 登录与认证相关实现
//!
//! 天翼云盘登录流程：
//! 1. 旧框架初始化：unifyLoginForPC.action 抓取 captchaToken/lt/paramId/reqId（appId=8025431004）
//! 2. getUUID.do 获取二维码 uuid
//! 3. 轮询 qrcodeLoginState.do（用新版参数格式：clientType=1 + isOauth2=true + cb_SaveName + 表单编码）
//!    返回 status:-106(待扫)/-11001(过期)/-11002(待确认)/0(成功带redirectUrl)
//! 4. 成功后 getSessionForPC.action?redirectURL=<redirectUrl>&clientSuffix 换取 sessionKey/sessionSecret

use crate::client::consts;
use crate::client::TianyiClient;
use crate::crypto;
use crate::error::{Error, Result};
use crate::models::*;
use regex::Regex;

/// 登录参数（密码登录）
#[derive(Debug, Clone, Default)]
pub struct LoginParam {
    pub lt: String,
    pub req_id: String,
    pub param_id: String,
    pub return_url: String,
    pub captcha_token: String,
    pub rsa_username: String,
    pub rsa_password: String,
}

/// 二维码登录参数
#[derive(Debug, Clone, Default)]
pub struct QRParam {
    pub lt: String,
    pub req_id: String,
    pub param_id: String,
    pub return_url: String,
    pub uuid: String,
    pub encode_uuid: String,
    pub encry_uuid: String,
}

/// 登录回调类型（用于向 UI 报告验证码/二维码状态）
pub trait LoginNotifier: Send + Sync {
    /// 需要验证码（base64 PNG 图片）
    fn need_captcha(&self, image_base64: &str);
    /// 二维码内容（uuid URL），text 为提示
    fn qr_code(&self, uuid: &str, text: &str);
    /// 二维码状态变化
    fn qr_status(&self, status: &str);
}

/// 空实现（无 UI）
pub struct NullNotifier;
impl LoginNotifier for NullNotifier {
    fn need_captcha(&self, _image_base64: &str) {}
    fn qr_code(&self, _uuid: &str, _text: &str) {}
    fn qr_status(&self, _status: &str) {}
}

/// 从 HTML 中提取参数
fn extract_param(text: &str, pattern: &str) -> Option<String> {
    let re = Regex::new(pattern).ok()?;
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// 初始化登录参数（旧框架：unifyLoginForPC.action 抓取页面参数）
async fn init_base_params(http: &reqwest::Client) -> Result<LoginParam> {
    let url = format!(
        "{}/api/portal/unifyLoginForPC.action?appId={}&clientType={}&returnURL={}&timeStamp={}",
        consts::WEB_URL,
        consts::APP_ID,
        consts::CLIENT_TYPE,
        urlencoding(consts::RETURN_URL),
        crypto::timestamp_ms()
    );
    let resp = http
        .get(&url)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .send()
        .await
        .map_err(|e| Error::Login(format!("unifyLoginForPC: {e}")))?;
    let text = resp.text().await.map_err(|e| Error::Login(format!("page body: {e}")))?;

    let captcha_token = extract_param(&text, r"'captchaToken' value='(.+?)'")
        .ok_or_else(|| Error::Login("captchaToken not found".into()))?;
    let lt = extract_param(&text, r#"lt = "(.+?)""#)
        .ok_or_else(|| Error::Login("lt not found".into()))?;
    let param_id = extract_param(&text, r#"paramId = "(.+?)""#)
        .ok_or_else(|| Error::Login("paramId not found".into()))?;
    let req_id = extract_param(&text, r#"reqId = "(.+?)""#)
        .ok_or_else(|| Error::Login("reqId not found".into()))?;

    Ok(LoginParam {
        lt,
        req_id,
        param_id,
        return_url: consts::RETURN_URL.to_string(),
        captcha_token,
        rsa_username: String::new(),
        rsa_password: String::new(),
    })
}

/// URL 编码（近似 Go 的 url.QueryEscape）
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 初始化密码登录参数（含 RSA 加密）
async fn init_login_param(
    _client: &TianyiClient,
    http: &reqwest::Client,
    username: &str,
    password: &str,
) -> Result<LoginParam> {
    let mut param = init_base_params(http).await?;

    // 获取 RSA 公钥
    let resp = http
        .post(format!("{}/api/logbox/config/encryptConf.do", consts::AUTH_URL))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[("appId", consts::APP_ID)])
        .send()
        .await?;
    let text = resp.text().await?;
    let conf: EncryptConfResp = serde_json::from_str(&text)
        .map_err(|e| Error::Login(format!("parse encryptConf: {e}")))?;

    let pre = &conf.data.pre;
    let rsa_user = crypto::rsa_encrypt(&conf.data.pub_key, username)?;
    let rsa_pass = crypto::rsa_encrypt(&conf.data.pub_key, password)?;
    param.rsa_username = format!("{pre}{rsa_user}");
    param.rsa_password = format!("{pre}{rsa_pass}");

    Ok(param)
}

/// 判断是否需要验证码
async fn need_captcha(http: &reqwest::Client, param: &LoginParam) -> Result<bool> {
    let resp = http
        .post(format!("{}/api/logbox/oauth2/needcaptcha.do", consts::AUTH_URL))
        .header("REQID", &param.req_id)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[("appKey", consts::APP_ID), ("accountType", consts::ACCOUNT_TYPE), ("userName", &param.rsa_username)])
        .send()
        .await?;
    let text = resp.text().await?;
    Ok(text.trim() != "0")
}

/// 拉取验证码图片（返回 base64 PNG）
async fn fetch_captcha(http: &reqwest::Client, param: &LoginParam) -> Result<String> {
    let url = format!(
        "{}/api/logbox/oauth2/picCaptcha.do?token={}&REQID={}&rnd={}",
        consts::AUTH_URL,
        param.captcha_token,
        param.req_id,
        crypto::timestamp_ms()
    );
    let resp = http.get(&url).send().await?;
    let bytes = resp.bytes().await?;
    if bytes.len() <= 20 {
        return Err(Error::Login("captcha image empty".into()));
    }
    Ok(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes))
}

/// 密码登录
pub async fn login_by_password(
    client: &TianyiClient,
    http: &reqwest::Client,
    username: &str,
    password: &str,
    captcha_code: Option<&str>,
    notifier: &dyn LoginNotifier,
) -> Result<AppSessionResp> {
    let param = init_login_param(client, http, username, password).await?;

    // 判断是否需要验证码
    let need_cap = need_captcha(http, &param).await?;
    let captcha = captcha_code.map(|c| c.to_string()).unwrap_or_default();

    if need_cap {
        let img = fetch_captcha(http, &param).await?;
        notifier.need_captcha(&img);
        if captcha.is_empty() {
            return Err(Error::Login("NEED_CAPTCHA".into()));
        }
    }

    // 提交登录
    let form = [
        ("appKey", consts::APP_ID.to_string()),
        ("accountType", consts::ACCOUNT_TYPE.to_string()),
        ("userName", param.rsa_username.clone()),
        ("password", param.rsa_password.clone()),
        ("validateCode", captcha),
        ("captchaToken", param.captcha_token.clone()),
        ("returnUrl", param.return_url.clone()),
        ("dynamicCheck", "FALSE".to_string()),
        ("clientType", consts::CLIENT_TYPE.to_string()),
        ("cb_SaveName", "0".to_string()),
        ("isOauth2", "true".to_string()),
        ("state", String::new()),
        ("paramId", param.param_id.clone()),
    ];

    let resp = http
        .post(format!("{}/api/logbox/oauth2/loginSubmit.do", consts::AUTH_URL))
        .header("REQID", &param.req_id)
        .header("lt", &param.lt)
        .header(reqwest::header::CONTENT_TYPE, "application/json;charset=UTF-8")
        .form(&form)
        .send()
        .await?;
    let text = resp.text().await?;
    let login: LoginResp =
        serde_json::from_str(&text).map_err(|e| Error::Login(format!("parse login: {e}")))?;

    if login.to_url.is_empty() {
        return Err(Error::Login(format!(
            "login failed, no toUrl: {}",
            login.msg
        )));
    }

    // 获取会话
    get_session(http, &login.to_url).await
}

/// 通过 redirectUrl 换取 Session
async fn get_session(http: &reqwest::Client, redirect_url: &str) -> Result<AppSessionResp> {
    let mut query: Vec<(String, String)> = crypto::client_suffix();
    query.push(("redirectURL".to_string(), redirect_url.to_string()));

    let resp = http
        .post(format!("{}/getSessionForPC.action", consts::API_URL))
        .query(&query)
        .header(reqwest::header::ACCEPT, "application/json;charset=UTF-8")
        .send()
        .await?;
    let text = resp.text().await?;
    log::info!("getSessionForPC redirectURL={} resp={}", redirect_url.chars().take(150).collect::<String>(), text.chars().take(300).collect::<String>());

    let token: AppSessionResp = serde_json::from_str(&text)
        .map_err(|e| Error::Login(format!("parse session: {e}")))?;
    if token.has_error() || token.session_key.is_empty() {
        let msg = if token.res_message.is_empty() {
            text.clone()
        } else {
            token.res_message.clone()
        };
        return Err(Error::Login(format!("getSession failed: {msg}")));
    }
    Ok(token)
}

/// 二维码登录状态
#[derive(Debug, Clone, PartialEq)]
pub enum QrStatus {
    Waiting,
    Success,
    Expired,
    Scanned,
}

/// 回退方案：通过 getUserBriefInfo 读取 sessionKey（新框架登录态基于 cookie）
async fn get_session_from_brief(http: &reqwest::Client) -> Result<AppSessionResp> {
    // 依次尝试新框架/旧框架路径
    let urls = [
        format!("{}/api/portal/v2/getUserBriefInfo.action", consts::WEB_URL),
        format!("{}/v2/getUserBriefInfo.action", consts::WEB_URL),
    ];
    for url in &urls {
        let resp = match http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json;charset=UTF-8")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log::warn!("getUserBriefInfo {url} failed: {e}");
                continue;
            }
        };
        let text = resp.text().await?;
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let session_key = v["sessionKey"].as_str().unwrap_or("").to_string();
        let login_name = v["loginName"].as_str().unwrap_or("").to_string();
        if session_key.is_empty() {
            log::warn!("getUserBriefInfo no sessionKey: {}", text.chars().take(200).collect::<String>());
            continue;
        }
        let mut resp = AppSessionResp::default();
        resp.session_key = session_key;
        resp.login_name = login_name;
        return Ok(resp);
    }
    Err(Error::Login("getUserBriefInfo no sessionKey on all paths".into()))
}

/// 获取二维码参数（uuid/encryuuid 等）
async fn get_qr_uuid(http: &reqwest::Client) -> Result<QRParam> {
    let base = init_base_params(http).await?;
    let resp = http
        .post(format!("{}/api/logbox/oauth2/getUUID.do", consts::AUTH_URL))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("REQID", &base.req_id)
        .form(&[("appId", consts::APP_ID)])
        .send()
        .await?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Login(format!("parse uuid: {e}")))?;

    Ok(QRParam {
        lt: base.lt,
        req_id: base.req_id,
        param_id: base.param_id,
        return_url: base.return_url,
        uuid: v["uuid"].as_str().unwrap_or("").to_string(),
        encode_uuid: v["encodeuuid"].as_str().unwrap_or("").to_string(),
        encry_uuid: v["encryuuid"].as_str().unwrap_or("").to_string(),
    })
}

/// 轮询二维码登录状态（新版参数，与页面 JS 一致）
pub async fn check_qr_status(
    http: &reqwest::Client,
    param: &QRParam,
) -> Result<(QrStatus, Option<String>)> {
    // date: 本地时间 yyyy-MM-ddHH:mm:ss + 随机数(0-23)
    let now = chrono::Local::now();
    let rand_suffix = rand::random::<u8>() % 24;
    let date = format!("{}{}", now.format("%Y-%m-%d%H:%M:%S"), rand_suffix);
    let ts = chrono::Utc::now().timestamp_millis().to_string();

    let form = [
        ("appId", consts::APP_ID.to_string()),
        ("encryuuid", param.encry_uuid.clone()),
        ("date", date),
        ("uuid", param.uuid.clone()),
        ("returnUrl", param.return_url.clone()),
        // 轮询 clientType 固定为 1（新版接口要求，已验证）
        ("clientType", "1".to_string()),
        ("timeStamp", ts),
        ("cb_SaveName", "0".to_string()),
        ("isOauth2", "true".to_string()),
        ("state", String::new()),
        ("paramId", param.param_id.clone()),
    ];

    let resp = http
        .post(format!(
            "{}/api/logbox/oauth2/qrcodeLoginState.do",
            consts::AUTH_URL
        ))
        .header("Referer", consts::WEB_URL)
        .header("REQID", &param.req_id)
        .header("lt", &param.lt)
        .header("user-finger", "1586280166")
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Login(format!("parse qr status: {e}")))?;

    // 可能返回 result:-103（参数错误）或 status
    let status = v["status"].as_i64().unwrap_or_else(|| {
        // 参数错误等，用 result 兜底
        v["result"].as_i64().unwrap_or(-1)
    });
    match status {
        0 => {
            let redirect = v["redirectUrl"].as_str().unwrap_or("").to_string();
            Ok((QrStatus::Success, Some(redirect)))
        }
        -11001 => Ok((QrStatus::Expired, None)),
        -106 => Ok((QrStatus::Waiting, None)),
        -11002 => Ok((QrStatus::Scanned, None)),
        -103 => Err(Error::Login(format!(
            "qr poll param error: {}",
            v["msg"].as_str().unwrap_or("")
        ))),
        other => Err(Error::Login(format!("qr login failed status {other}"))),
    }
}

/// 二维码登录：获取 uuid 并通知 UI 渲染
pub async fn login_by_qrcode(
    _client: &TianyiClient,
    http: &reqwest::Client,
    notifier: &dyn LoginNotifier,
) -> Result<QRParam> {
    let param = get_qr_uuid(http).await?;
    notifier.qr_code(&param.uuid, "请使用天翼云盘 App 扫码登录");
    Ok(param)
}

/// 轮询二维码登录，成功后返回 AppSessionResp
pub async fn poll_qr_login(
    http: &reqwest::Client,
    param: &QRParam,
    notifier: &dyn LoginNotifier,
) -> Result<AppSessionResp> {
    loop {
        let (status, redirect) = check_qr_status(http, param).await?;
        match status {
            QrStatus::Success => {
                if let Some(url) = redirect {
                    notifier.qr_status("登录成功");
                    // 旧框架：扫码成功后 redirectUrl 直接给 getSessionForPC（OpenList 方式）
                    match get_session(http, &url).await {
                        Ok(s) => return Ok(s),
                        Err(Error::Login(msg)) if msg.contains("LoginRespIsNull") => {
                            // 兜底：GET redirectUrl 激活会话后重试，仍失败则回退 getUserBriefInfo
                            log::warn!("getSessionForPC LoginRespIsNull, try GET redirectUrl then retry");
                            if let Ok(resp) = http.get(&url).send().await {
                                let final_url = resp.url().to_string();
                                if let Ok(s) = get_session(http, &final_url).await {
                                    return Ok(s);
                                }
                            }
                            log::warn!("getSessionForPC still LoginRespIsNull, fallback to getUserBriefInfo");
                            return get_session_from_brief(http).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            QrStatus::Waiting => {
                notifier.qr_status("等待扫码...");
            }
            QrStatus::Scanned => {
                notifier.qr_status("已扫码，请在手机上确认");
            }
            QrStatus::Expired => {
                return Err(Error::Login("二维码已过期，请重新获取".into()));
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// 从 AppSessionResp 构造 TokenInfo
pub fn token_from_session(session: &AppSessionResp) -> TokenInfo {
    log::info!(
        "token_from_session: session_key_len={} session_secret_len={} family_key_len={} family_secret_len={} access_token_len={} refresh_token_len={} login_name={}",
        session.session_key.len(), session.session_secret.len(),
        session.family_session_key.len(), session.family_session_secret.len(),
        session.access_token.len(), session.refresh_token.len(),
        session.login_name
    );
    TokenInfo {
        access_token: session.access_token.clone(),
        refresh_token: session.refresh_token.clone(),
        session_key: session.session_key.clone(),
        session_secret: session.session_secret.clone(),
        family_session_key: session.family_session_key.clone(),
        family_session_secret: session.family_session_secret.clone(),
        login_name: session.login_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_param() {
        let html = "<input name=\"j-captchaToken\" 'captchaToken' value='abc123'> <script>var lt = \"ltval\"; var paramId = \"pid\"; var reqId = \"rid\";</script>";
        assert_eq!(extract_param(html, r"'captchaToken' value='(.+?)'").unwrap(), "abc123");
        assert_eq!(extract_param(html, r#"lt = "(.+?)""#).unwrap(), "ltval");
        assert_eq!(extract_param(html, r#"paramId = "(.+?)""#).unwrap(), "pid");
        assert_eq!(extract_param(html, r#"reqId = "(.+?)""#).unwrap(), "rid");
    }
}
