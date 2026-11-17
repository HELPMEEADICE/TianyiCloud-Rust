//! 稳定错误码与进程退出码
//!
//! 这两样东西是 CLI 对外契约的一部分：脚本与 AI Agent 会按他们分支，因此**只能增不能改**。
//! 改一个错误码的字符串或退出码的数字，等于悄悄破坏所有已经写好的调用方。
//!
//! 分层：
//! * [ErrorCode]：机器可读的稳定错误码（JSON 里的 error.code）；
//! * [ErrorCode::exit_code]：进程退出码（shell 里 $? 看到的东西）；
//! * [CliError]：错误码 + 人类可读文案 + 可选结构化细节。

use serde::Serialize;
use std::fmt;

/// 稳定错误码集合
///
/// 约定：新增错误一律先复用已有码；确实需要新语义时才加新码，并同步更新 docs/CLI.md
/// 里的表格与退出码对照表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ErrorCode {
    /// 参数错误：未知命令、缺值、类型不对、非交互模式下缺 --yes 等
    Usage,
    /// 配置不可用（目录不可写、配置文件损坏且无法恢复）
    Config,
    /// 尚未登录
    AuthRequired,
    /// 登录态已失效，需要重新登录
    AuthExpired,
    /// 登录失败（账号密码错误、二维码过期等）
    LoginFailed,
    /// 需要输入验证码
    CaptchaRequired,
    /// 网络错误（连接失败、超时、TLS、5xx、429）
    Network,
    /// 远端 API 返回业务错误
    RemoteApi,
    /// 目标不存在（路径、账号、任务 id）
    NotFound,
    /// 冲突（同名、目标已存在、批量任务冲突）
    Conflict,
    /// 权限不足
    PermissionDenied,
    /// 本地 IO 错误（读写文件、创建目录）
    LocalIo,
    /// 忙碌（另一个 CLI 进程正在占用同一份传输状态）
    Busy,
    /// 被用户中断 / 任务已取消
    Canceled,
    /// 超时
    Timeout,
    /// 当前空间或构建不支持该操作
    Unsupported,
    /// 挂载环境不可用（Windows 缺 WinFsp / Linux 缺 /dev/fuse / 本平台没有挂载实现）
    MountUnavailable,
    /// 挂载或卸载的**执行**失败（挂载点被占用、建卷失败、卸载超时等）
    MountFailed,
    /// 目标本来就没有挂载：卸载时的「无事可做」，与「卸载失败」必须区分
    NotMounted,
    /// 其他未分类错误
    Internal,
}

impl ErrorCode {
    /// JSON 里的稳定字符串
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Usage => "USAGE",
            ErrorCode::Config => "CONFIG",
            ErrorCode::AuthRequired => "AUTH_REQUIRED",
            ErrorCode::AuthExpired => "AUTH_EXPIRED",
            ErrorCode::LoginFailed => "LOGIN_FAILED",
            ErrorCode::CaptchaRequired => "CAPTCHA_REQUIRED",
            ErrorCode::Network => "NETWORK",
            ErrorCode::RemoteApi => "REMOTE_API",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::LocalIo => "LOCAL_IO",
            ErrorCode::Busy => "BUSY",
            ErrorCode::Canceled => "CANCELED",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::Unsupported => "UNSUPPORTED",
            ErrorCode::MountUnavailable => "MOUNT_UNAVAILABLE",
            ErrorCode::MountFailed => "MOUNT_FAILED",
            ErrorCode::NotMounted => "NOT_MOUNTED",
            ErrorCode::Internal => "INTERNAL",
        }
    }

    /// 进程退出码
    ///
    /// 0 成功；1 未分类；2 参数；3 认证；4 网络；5 远端；6 本地 IO；7 中断；
    /// 8 不存在；9 忙碌；10 挂载环境不可用；11 挂载/卸载执行失败；12 本来就没挂载。
    /// 刻意不复用 126/127/128+n（shell 保留语义）。
    ///
    /// 挂载相关的三个码是**独立**的：脚本要能分开「这台机器缺 WinFsp」、
    /// 「这次挂载/卸载真的失败了」和「本来就没挂、不用管」这三种情况。
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorCode::Usage | ErrorCode::Unsupported => 2,
            ErrorCode::AuthRequired
            | ErrorCode::AuthExpired
            | ErrorCode::LoginFailed
            | ErrorCode::CaptchaRequired => 3,
            ErrorCode::Network | ErrorCode::Timeout => 4,
            ErrorCode::RemoteApi | ErrorCode::Conflict | ErrorCode::PermissionDenied => 5,
            ErrorCode::LocalIo | ErrorCode::Config => 6,
            ErrorCode::Canceled => 7,
            ErrorCode::NotFound => 8,
            ErrorCode::Busy => 9,
            ErrorCode::MountUnavailable => 10,
            ErrorCode::MountFailed => 11,
            ErrorCode::NotMounted => 12,
            ErrorCode::Internal => 1,
        }
    }

    /// 调用方是否可以「等一下重试」来期待不同结果
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::Network
                | ErrorCode::Timeout
                | ErrorCode::AuthExpired
                | ErrorCode::Busy
                | ErrorCode::Conflict
                // 验证码是「补一个参数再试一次」就能过的，必须可重试
                | ErrorCode::CaptchaRequired
        )
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CLI 层错误
#[derive(Debug, Clone)]
pub struct CliError {
    /// 稳定错误码
    pub code: ErrorCode,
    /// 人类可读文案（不含任何凭据）
    pub message: String,
    /// 结构化细节（可选，例如验证码图片路径、冲突的文件名）
    pub details: Option<serde_json::Value>,
}

impl CliError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        CliError {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// 参数错误
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Usage, message)
    }

    /// 附上结构化细节
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 是否可重试
    pub fn retryable(&self) -> bool {
        self.code.retryable()
    }

    /// 进程退出码
    pub fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub type CliResult<T> = Result<T, CliError>;

impl From<tianyi_core::Error> for CliError {
    fn from(e: tianyi_core::Error) -> Self {
        use tianyi_core::Error as E;
        // 判断顺序很重要：会话失效必须先于「通用 API 错误」被识别，
        // 否则用户看到的是 "remote api error" 而不是"请重新登录"。
        let (code, retryable_hint) = match &e {
            _ if e.is_session_invalid() => (ErrorCode::AuthExpired, true),
            E::NotAuthenticated(_) => (ErrorCode::AuthRequired, false),
            E::Login(m) => {
                if m.contains("NEED_CAPTCHA") {
                    (ErrorCode::CaptchaRequired, true)
                } else {
                    (ErrorCode::LoginFailed, false)
                }
            }
            E::Session(_) => (ErrorCode::AuthExpired, true),
            E::NotFound(_) => (ErrorCode::NotFound, false),
            E::InvalidRequest(_) => (ErrorCode::Usage, false),
            E::Unsupported(_) => (ErrorCode::Unsupported, false),
            E::Config(_) => (ErrorCode::Config, false),
            E::Io(_) => (ErrorCode::LocalIo, false),
            E::Http(_) => (ErrorCode::Network, true),
            E::HttpStatus { status, .. } => {
                let s = *status;
                if s == 429 || s >= 500 {
                    (ErrorCode::Network, true)
                } else if s == 401 || s == 403 {
                    (ErrorCode::PermissionDenied, false)
                } else if s == 404 {
                    (ErrorCode::NotFound, false)
                } else {
                    (ErrorCode::RemoteApi, false)
                }
            }
            E::ApiCoded { .. } | E::Api(_) => (ErrorCode::RemoteApi, false),
            E::Crypto(_) | E::Serde(_) | E::Other(_) => (ErrorCode::Internal, false),
            E::Transfer(_) => (ErrorCode::RemoteApi, true),
            E::Paused | E::Canceled => (ErrorCode::Canceled, false),
        };
        let mut err = CliError::new(code, e.to_string());
        if retryable_hint && !code.retryable() {
            // 明确失效的会话是「重试也没用，得重新登录」，这里不额外标记
        }
        if let Some(status) = e.http_status() {
            err.details = Some(serde_json::json!({ "http_status": status }));
        }
        err
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::new(ErrorCode::LocalIo, format!("本地 IO 错误：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable_and_distinct() {
        // 这些数字是对外契约：改了必须同步改文档，并在 PR 里说明
        assert_eq!(ErrorCode::Usage.exit_code(), 2);
        assert_eq!(ErrorCode::AuthRequired.exit_code(), 3);
        assert_eq!(ErrorCode::Network.exit_code(), 4);
        assert_eq!(ErrorCode::RemoteApi.exit_code(), 5);
        assert_eq!(ErrorCode::LocalIo.exit_code(), 6);
        assert_eq!(ErrorCode::Canceled.exit_code(), 7);
        assert_eq!(ErrorCode::NotFound.exit_code(), 8);
        assert_eq!(ErrorCode::Busy.exit_code(), 9);
        assert_eq!(ErrorCode::MountUnavailable.exit_code(), 10);
        assert_eq!(ErrorCode::MountFailed.exit_code(), 11);
        assert_eq!(ErrorCode::NotMounted.exit_code(), 12);
        assert_eq!(ErrorCode::Internal.exit_code(), 1);
        // 成功态不在这里，但所有码都必须非零
        for code in [
            ErrorCode::Usage,
            ErrorCode::Config,
            ErrorCode::AuthRequired,
            ErrorCode::AuthExpired,
            ErrorCode::LoginFailed,
            ErrorCode::CaptchaRequired,
            ErrorCode::Network,
            ErrorCode::RemoteApi,
            ErrorCode::NotFound,
            ErrorCode::Conflict,
            ErrorCode::PermissionDenied,
            ErrorCode::LocalIo,
            ErrorCode::Busy,
            ErrorCode::Canceled,
            ErrorCode::Timeout,
            ErrorCode::Unsupported,
            ErrorCode::MountUnavailable,
            ErrorCode::MountFailed,
            ErrorCode::NotMounted,
            ErrorCode::Internal,
        ] {
            assert_ne!(code.exit_code(), 0, "{code} 不能是 0");
            assert!(!code.as_str().is_empty());
        }
    }

    #[test]
    fn mount_codes_are_distinct_and_not_retryable() {
        // 缺环境 / 执行失败 / 本来没挂：三件事三种退出码，脚本才能正确分支
        let codes = [
            ErrorCode::MountUnavailable,
            ErrorCode::MountFailed,
            ErrorCode::NotMounted,
        ];
        let exits: Vec<u8> = codes.iter().map(|c| c.exit_code()).collect();
        assert_eq!(exits, vec![10, 11, 12]);
        let names: Vec<&str> = codes.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            vec!["MOUNT_UNAVAILABLE", "MOUNT_FAILED", "NOT_MOUNTED"]
        );
        for code in codes {
            assert!(
                !code.retryable(),
                "{code} 不该标记为可重试：重试是同一个结果"
            );
        }
    }

    #[test]
    fn session_invalid_maps_to_auth_expired_and_is_retryable() {
        let e = tianyi_core::Error::Session("expired".into());
        let ce: CliError = e.into();
        assert_eq!(ce.code, ErrorCode::AuthExpired);
        assert!(ce.retryable(), "刷新后重试是可行的，必须标记为可重试");
    }

    #[test]
    fn not_authenticated_maps_to_auth_required() {
        let ce: CliError = tianyi_core::Error::NotAuthenticated("login first".into()).into();
        assert_eq!(ce.code, ErrorCode::AuthRequired);
        assert!(!ce.retryable());
    }

    #[test]
    fn http_status_classification() {
        let cases = [
            (503u16, ErrorCode::Network),
            (429, ErrorCode::Network),
            (401, ErrorCode::PermissionDenied),
            (404, ErrorCode::NotFound),
            (400, ErrorCode::RemoteApi),
        ];
        for (status, want) in cases {
            let ce: CliError = tianyi_core::Error::HttpStatus {
                status,
                body: String::new(),
            }
            .into();
            assert_eq!(ce.code, want, "HTTP {status}");
        }
    }

    #[test]
    fn captcha_requirement_is_its_own_code() {
        let ce: CliError = tianyi_core::Error::Login("NEED_CAPTCHA".into()).into();
        assert_eq!(ce.code, ErrorCode::CaptchaRequired);
        assert!(ce.retryable());
    }

    #[test]
    fn io_and_config_errors_are_local() {
        let ce: CliError = tianyi_core::Error::Io(std::io::Error::other("boom")).into();
        assert_eq!(ce.code, ErrorCode::LocalIo);
        let ce: CliError = tianyi_core::Error::Config("x".into()).into();
        assert_eq!(ce.code, ErrorCode::Config);
        assert_eq!(ce.exit_code(), 6);
    }
}
