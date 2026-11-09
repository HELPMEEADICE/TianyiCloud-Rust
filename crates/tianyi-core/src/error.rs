//! 错误类型定义

use crate::models::RespErr;

/// 判定「会话失效」的字符串错误码集合
///
/// 对应天翼云响应里的 `errorCode` / `code` 字段（比对时忽略大小写）。
/// `UserInvalidOpenToken` 取自 [`crate::client::consts::USER_INVALID_OPEN_TOKEN`]：
/// 天翼在 access token 过期时返回该码，语义等同于「需要重新认证」。
pub const SESSION_INVALID_CODE_NAMES: &[&str] = &[
    // 下划线写法：网关纯文本错误里出现的原始文案
    "invalid_session_key",
    // camelCase 错误码 `InvalidSessionKey` 的小写形式
    "invalidsessionkey",
    "userinvalidopentoken",
];

/// 核心库统一错误类型
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP 请求失败
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// 天翼云 API 返回的业务错误
    #[error("api error: {0}")]
    Api(String),

    /// 天翼云 API 返回的业务错误（携带结构化错误码）
    ///
    /// 由 [`Error::from_resp`] 从 [`RespErr`] 构造；`code` 为数值码（`res_code`），
    /// `code_name` 为字符串码（`errorCode` / `code`），缺失的字段分别为 `0` / 空串。
    #[error("api error: {message} (code={code}, codeName={code_name})")]
    ApiCoded {
        /// 数值错误码，缺失为 0
        code: i64,
        /// 字符串错误码，缺失为空串
        code_name: String,
        /// 可读错误信息
        message: String,
    },

    /// HTTP 状态码非 2xx（结构化保存状态码，供重试策略判定 5xx / 429）
    #[error("http {status}: {body}")]
    HttpStatus {
        /// HTTP 状态码
        status: u16,
        /// 响应体（已截断，仅用于日志与提示）
        body: String,
    },

    /// 会话失效（需要刷新）
    #[error("session invalid: {0}")]
    Session(String),

    /// 登录失败
    #[error("login failed: {0}")]
    Login(String),

    /// 加密/解密失败
    #[error("crypto error: {0}")]
    Crypto(String),

    /// 上传/下载过程中的错误
    #[error("transfer error: {0}")]
    Transfer(String),

    /// 传输被用户暂停
    #[error("传输已暂停")]
    Paused,

    /// 传输被用户取消
    #[error("传输已取消")]
    Canceled,

    /// IO 错误
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 序列化错误
    #[error("serialize error: {0}")]
    Serde(#[from] serde_json::Error),

    /// 配置错误
    #[error("config error: {0}")]
    Config(String),

    /// 其他错误
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// 构造带结构化错误码的 API 错误
    pub fn api_coded(code: i64, code_name: impl Into<String>, message: impl Into<String>) -> Self {
        Error::ApiCoded {
            code,
            code_name: code_name.into(),
            message: message.into(),
        }
    }

    /// 从通用错误响应构造错误
    ///
    /// 响应里带得到错误码时用 [`Error::ApiCoded`]（结构化），
    /// 完全拿不到错误码时退回 [`Error::Api`]，保持旧行为。
    pub fn from_resp(err: &RespErr) -> Self {
        let mut code = 0i64;
        let mut code_name = String::new();

        if let Some(rc) = &err.res_code {
            match rc {
                serde_json::Value::Number(n) => code = n.as_i64().unwrap_or(0),
                serde_json::Value::String(s) if !s.is_empty() => match s.parse::<i64>() {
                    Ok(n) => code = n,
                    Err(_) => code_name = s.clone(),
                },
                _ => {}
            }
        }
        if code_name.is_empty() && !err.error_code.is_empty() {
            code_name = err.error_code.clone();
        }
        if code_name.is_empty() && !err.code.is_empty() && err.code != "SUCCESS" {
            code_name = err.code.clone();
        }

        let message = err.message();
        if code == 0 && code_name.is_empty() {
            Error::Api(message)
        } else {
            Error::ApiCoded {
                code,
                code_name,
                message,
            }
        }
    }

    /// 数值错误码（仅 [`Error::ApiCoded`] 才有，缺失返回 `None`）
    pub fn code(&self) -> Option<i64> {
        match self {
            Error::ApiCoded { code, .. } if *code != 0 => Some(*code),
            _ => None,
        }
    }

    /// 字符串错误码（仅 [`Error::ApiCoded`] 才有，缺失返回 `None`）
    pub fn code_name(&self) -> Option<&str> {
        match self {
            Error::ApiCoded { code_name, .. } if !code_name.is_empty() => Some(code_name),
            _ => None,
        }
    }

    /// HTTP 状态码（仅 [`Error::HttpStatus`] 才有）
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Error::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// 错误自带的文本信息（用于日志与最后的兜底判定）
    fn message_text(&self) -> Option<&str> {
        match self {
            Error::Api(m)
            | Error::ApiCoded { message: m, .. }
            | Error::HttpStatus { body: m, .. }
            | Error::Session(m)
            | Error::Login(m)
            | Error::Crypto(m)
            | Error::Transfer(m)
            | Error::Config(m)
            | Error::Other(m) => Some(m),
            _ => None,
        }
    }

    /// 判断是否是需要刷新会话的错误
    ///
    /// 主判据是结构化错误码（比对 [`SESSION_INVALID_CODE_NAMES`]）；
    /// 只有在完全拿不到错误码的路径上（网关返回纯文本错误页、无 JSON 结构可解析，
    /// 例如 `userSessionBO is null`）才退回固定文案匹配。保留这一兜底是为了不让
    /// 非 JSON 响应导致会话失效被漏判 —— 但它不再是主判据。
    pub fn is_session_invalid(&self) -> bool {
        if matches!(self, Error::Session(_)) {
            return true;
        }
        if let Some(name) = self.code_name() {
            if SESSION_INVALID_CODE_NAMES
                .iter()
                .any(|k| name.eq_ignore_ascii_case(k))
            {
                return true;
            }
        }
        let msg = self.message_text().unwrap_or("").to_ascii_lowercase();
        msg.contains("invalid_session_key") || msg.contains("usersessionbo is null")
    }

    /// 判断是否是用户主动暂停/取消导致的中断
    ///
    /// 这类错误不是真正的失败：调用方应把它映射成“已暂停/已取消”状态，
    /// 而不是记成任务失败。
    pub fn is_interrupted(&self) -> bool {
        matches!(self, Error::Paused | Error::Canceled)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Api(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Api(s.to_string())
    }
}

/// 便捷 Result 别名
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn resp_code_name(name: &str) -> RespErr {
        RespErr {
            error_code: name.to_string(),
            res_message: name.to_string(),
            ..Default::default()
        }
    }

    fn resp_num(code: i64) -> RespErr {
        RespErr {
            res_code: Some(serde_json::Value::Number(code.into())),
            res_message: "x".to_string(),
            ..Default::default()
        }
    }

    /// 会话失效：结构化错误码命中即判定，不再依赖 to_string 匹配
    #[test]
    fn test_session_invalid_by_code() {
        let e = Error::from_resp(&resp_code_name("InvalidSessionKey"));
        assert!(matches!(e, Error::ApiCoded { .. }), "应携带结构化错误码");
        assert_eq!(e.code_name(), Some("InvalidSessionKey"));
        assert!(e.is_session_invalid());

        let e = Error::from_resp(&resp_code_name("UserInvalidOpenToken"));
        assert!(
            e.is_session_invalid(),
            "USER_INVALID_OPEN_TOKEN 视为会话失效"
        );

        // 数值错误码也走结构化路径
        let e = Error::from_resp(&resp_num(-11001));
        assert_eq!(e.code(), Some(-11001));
        assert!(!e.is_session_invalid(), "普通业务码不是会话失效");
    }

    /// 4xx/业务错误不能被误判为会话失效，且兜底文案匹配仍然保留
    #[test]
    fn test_session_invalid_fallback_and_negatives() {
        let e = Error::from_resp(&RespErr {
            res_message: "file not found".to_string(),
            ..Default::default()
        });
        assert!(matches!(e, Error::Api(_)), "无错误码时退回 Api");
        assert!(!e.is_session_invalid());

        // 纯文本路径（无结构化错误码）：保留固定文案兜底
        let e = Error::Api("userSessionBO is null".to_string());
        assert!(e.is_session_invalid());

        let e = Error::Session("x".to_string());
        assert!(e.is_session_invalid());
    }

    /// From<String>/From<&str> 必须继续可用（大量调用点依赖 `.into()` / `?`）
    #[test]
    fn test_from_string_still_api() {
        let a: Error = "boom".into();
        assert!(matches!(a, Error::Api(ref m) if m == "boom"));
        let b: Error = String::from("boom").into();
        assert!(matches!(b, Error::Api(ref m) if m == "boom"));
    }
}
