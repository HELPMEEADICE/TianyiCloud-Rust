//! 错误类型定义

/// 核心库统一错误类型
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP 请求失败
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// 天翼云 API 返回的业务错误
    #[error("api error: {0}")]
    Api(String),

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
    /// 判断是否是需要刷新会话的错误
    pub fn is_session_invalid(&self) -> bool {
        matches!(self, Error::Session(_))
            || self
                .to_string()
                .to_lowercase()
                .contains("invalid_session_key")
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
