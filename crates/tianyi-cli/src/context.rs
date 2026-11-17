//! 运行上下文：配置、会话、输出模式、交互策略
//!
//! 一次 CLI 调用的生命周期被收敛在这里：
//! 1. 决定输出模式（人类 / JSON / NDJSON）；
//! 2. 打开配置存储（与 GUI 同一份目录，--config-dir 可覆盖）；
//! 3. 恢复已保存会话（必要时自动刷新 token 并回写）；
//! 4. 把「要不要交互」「超时多久」这类策略集中起来，命令实现只问上下文。
//!
//! 这样命令代码里不再出现 std::env / std::fs / 交互判断，测试可以直接构造上下文。

use crate::args::{Cli, Credentials};
use crate::error::{CliError, CliResult, ErrorCode};
use crate::notify::{default_captcha_path, read_captcha_file};
use crate::output::{OutMode, Printer};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tianyi_core::cloudpath::CloudPath;
use tianyi_core::service::{CloudService, Space};
use tianyi_core::store::{ConfigStore, StoreDegrade};

/// 一次 CLI 调用的上下文
pub struct Context {
    /// 共享云盘服务（含配置存储与当前会话）
    pub service: Arc<CloudService>,
    /// 输出器
    pub printer: Printer,
    /// 是否对破坏性操作直接确认
    pub yes: bool,
    /// 是否禁止任何人工输入
    pub non_interactive: bool,
    /// 单条命令的总超时
    pub timeout: Option<Duration>,
    /// 当前云端工作目录（REPL 会改；一次性命令等于根目录）
    pub cwd: CloudPath,
    /// stdin 是否连着终端
    pub stdin_tty: bool,
}

impl Context {
    /// 由解析好的参数构造上下文（会联网恢复会话）
    pub async fn bootstrap(cli: &Cli) -> CliResult<Context> {
        let mode = out_mode(cli);
        let printer = Printer::new(mode, cli.quiet);
        let store = match &cli.config_dir {
            Some(dir) => ConfigStore::open_at(dir.clone())?,
            None => {
                let (store, degrade) = ConfigStore::open();
                if let Some(d) = degrade {
                    printer.diag(&degrade_text(&d));
                }
                store
            }
        };
        let service = Arc::new(CloudService::new(store));
        // 恢复会话：失败也不在这里终止（status / login 都需要在未登录时可用），
        // 真正的网络错误会在具体命令里以稳定错误码报出来。
        let info = service.restore(cli.account.as_deref()).await?;
        let requested_space = match &cli.space {
            Some(s) => Some(Space::parse(s).ok_or_else(|| {
                CliError::usage(format!("--space 只接受 personal 或 family，收到 {s}"))
            })?),
            None => None,
        };
        if let (Some(space), true) = (requested_space, info.authenticated) {
            if info.space != space {
                service.switch_space(space).await?;
            }
        }
        Ok(Context {
            service,
            printer,
            yes: cli.yes,
            non_interactive: cli.non_interactive,
            timeout: cli.timeout.map(Duration::from_secs),
            cwd: CloudPath::root(),
            stdin_tty: std::io::stdin().is_terminal(),
        })
    }

    /// 配置目录
    pub fn config_root(&self) -> &PathBuf {
        self.service.store().root()
    }

    /// 解析云端路径参数：绝对路径从根开始，相对路径按当前目录
    pub fn resolve_path(&self, input: &str) -> CliResult<CloudPath> {
        CloudPath::resolve(&self.cwd, input)
            .map_err(|e| CliError::new(ErrorCode::Usage, format!("路径无法解析：{e}")))
    }

    /// 命令行里带 --account 但会话没建立起来时，给出明确提示而不是神秘的 AUTH_REQUIRED
    pub fn account_hint(&self) -> String {
        if self.service.session().username.is_empty() {
            "尚未登录：请先执行 tianyi-app -c login".to_string()
        } else {
            "会话已失效：请执行 tianyi-app -c login 重新登录".to_string()
        }
    }

    /// 是否需要人工确认（--yes 直接放行；非交互环境必须显式 --yes）
    pub fn confirm(&self, question: &str) -> CliResult<bool> {
        if self.yes {
            return Ok(true);
        }
        if self.non_interactive || !self.stdin_tty {
            return Err(CliError::new(
                ErrorCode::Usage,
                format!("{question}：非交互模式下需要显式加 --yes 确认"),
            ));
        }
        eprint!("{question} [y/N] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(matches!(
            line.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    /// 读取密码：文件 / 文件描述符 / stdin / 隐藏终端输入
    ///
    /// 绝不把密码写进 argv（调用方只能通过这四种方式提供），也绝不打印它。
    pub fn read_password(&self, creds: &Credentials) -> CliResult<String> {
        if let Some(path) = &creds.password_file {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                CliError::new(
                    ErrorCode::LocalIo,
                    format!("读取密码文件 {} 失败：{e}", path.display()),
                )
            })?;
            return Ok(strip_trailing_newline(&raw));
        }
        if let Some(fd) = creds.password_fd {
            return read_password_fd(fd);
        }
        if creds.password_stdin {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let first = buf.lines().next().unwrap_or_default();
            if first.is_empty() {
                return Err(CliError::usage("--password-stdin 没有读到任何内容"));
            }
            return Ok(first.to_string());
        }
        if self.stdin_tty && !self.non_interactive {
            let pwd = rpassword::prompt_password("密码（输入不回显）：")
                .map_err(|e| CliError::new(ErrorCode::LocalIo, format!("读取密码失败：{e}")))?;
            if pwd.is_empty() {
                return Err(CliError::usage("密码不能为空"));
            }
            return Ok(pwd);
        }
        Err(CliError::usage(
            "非交互模式必须用 --password-stdin、--password-file 或 --password-fd 提供密码",
        ))
    }

    /// 读取验证码文本（--captcha 或 --captcha-file）
    pub fn read_captcha(&self, creds: &Credentials) -> CliResult<Option<String>> {
        if let Some(code) = &creds.captcha {
            let code = code.trim().to_string();
            if code.is_empty() {
                return Err(CliError::usage("--captcha 不能是空串"));
            }
            return Ok(Some(code));
        }
        if let Some(path) = &creds.captcha_file {
            return read_captcha_file(path).map(Some);
        }
        Ok(None)
    }

    /// 验证码图片落盘路径：--captcha-out 优先，否则放在配置目录
    pub fn captcha_out(&self, creds: &Credentials) -> PathBuf {
        creds
            .captcha_out
            .clone()
            .unwrap_or_else(|| default_captcha_path(self.config_root()))
    }
}

/// 套上总超时
///
/// 做成自由函数而不是方法：调用点常常同时需要「读上下文里的超时」和「可变借用
/// runner 去执行命令」，写成 &self 方法会撞上借用冲突。
pub async fn with_timeout<F, T>(timeout: Option<Duration>, fut: F) -> CliResult<T>
where
    F: std::future::Future<Output = CliResult<T>>,
{
    match timeout {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(r) => r,
            Err(_) => Err(CliError::new(
                ErrorCode::Timeout,
                format!("操作超时（{} 秒）", d.as_secs()),
            )),
        },
        None => fut.await,
    }
}

/// 输出模式由 --json / --jsonl 决定
pub fn out_mode(cli: &Cli) -> OutMode {
    if cli.jsonl {
        OutMode::Ndjson
    } else if cli.json {
        OutMode::Json
    } else {
        OutMode::Human
    }
}

/// 配置降级提示文案
pub fn degrade_text(d: &StoreDegrade) -> String {
    match d {
        StoreDegrade::Fallback(dir) => format!(
            "警告：默认配置目录不可用，本次使用 {}（设置不会与 GUI 共享）",
            dir.display()
        ),
        StoreDegrade::Memory => {
            "警告：配置目录不可写，本次运行只在内存里保存设置（退出即丢失）".to_string()
        }
    }
}

fn strip_trailing_newline(s: &str) -> String {
    s.trim_end_matches(['\r', '\n']).to_string()
}

#[cfg(unix)]
fn read_password_fd(fd: i32) -> CliResult<String> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: 调用方通过 --password-fd 显式交给我们一个已打开的可读描述符；
    // 这里用 forget 避免 drop 时把调用方的 fd 关掉（我们只是借用）。
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut buf = String::new();
    let read = file.read_to_string(&mut buf);
    std::mem::forget(file);
    read?;
    let pwd = strip_trailing_newline(&buf);
    if pwd.is_empty() {
        return Err(CliError::usage(format!("文件描述符 {fd} 没有读到密码")));
    }
    Ok(pwd)
}

#[cfg(not(unix))]
fn read_password_fd(_fd: i32) -> CliResult<String> {
    Err(CliError::new(
        ErrorCode::Unsupported,
        "--password-fd 只在 Unix 上可用，Windows 请改用 --password-stdin 或 --password-file",
    ))
}

/// 测试专用构造器：不联网、不读用户配置、输出静默
#[cfg(test)]
impl Context {
    pub fn for_test(yes: bool, non_interactive: bool) -> Context {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tianyi-ctx-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::open_at(dir).expect("临时配置目录必须可用");
        Context {
            service: Arc::new(CloudService::new(store)),
            printer: Printer::new(OutMode::Human, true),
            yes,
            non_interactive,
            timeout: None,
            cwd: CloudPath::root(),
            stdin_tty: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn test_cli() -> Cli {
        Cli::parse_from(["tianyi-app"])
    }

    #[test]
    fn out_mode_maps_flags_to_modes() {
        let mut cli = test_cli();
        assert_eq!(out_mode(&cli), OutMode::Human);
        cli.json = true;
        assert_eq!(out_mode(&cli), OutMode::Json);
        cli.json = false;
        cli.jsonl = true;
        assert_eq!(out_mode(&cli), OutMode::Ndjson);
    }

    #[test]
    fn trailing_newline_is_stripped_but_other_spaces_kept() {
        assert_eq!(strip_trailing_newline("pw  \r\n"), "pw  ");
        assert_eq!(strip_trailing_newline("pw"), "pw");
        assert_eq!(strip_trailing_newline(""), "");
    }

    #[test]
    fn degrade_text_never_claims_success() {
        let fallback = degrade_text(&StoreDegrade::Fallback(PathBuf::from("/tmp/x")));
        assert!(fallback.contains("警告"));
        assert!(fallback.contains("/tmp/x"));
        let memory = degrade_text(&StoreDegrade::Memory);
        assert!(memory.contains("警告"));
        assert!(memory.contains("内存"));
    }

    #[test]
    fn confirm_requires_explicit_yes_in_non_interactive_mode() {
        let ctx = Context::for_test(true, false);
        assert!(ctx.confirm("删除？").unwrap());
        let ctx = Context::for_test(false, true);
        let err = ctx.confirm("删除？").expect_err("非交互必须报错");
        assert_eq!(err.code, ErrorCode::Usage);
        assert!(err.message.contains("--yes"));
    }

    #[test]
    fn read_password_prefers_file_and_strips_newline_only() {
        let dir = std::env::temp_dir().join(format!("tianyi-pw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw.txt");
        std::fs::write(&path, "  secret value  \n").unwrap();
        let ctx = Context::for_test(false, true);
        let creds = Credentials {
            password_file: Some(path.clone()),
            ..Default::default()
        };
        assert_eq!(ctx.read_password(&creds).unwrap(), "  secret value  ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_password_without_source_is_usage_error() {
        let ctx = Context::for_test(false, true);
        let err = ctx
            .read_password(&Credentials::default())
            .expect_err("必须报错");
        assert_eq!(err.code, ErrorCode::Usage);
        assert!(err.message.contains("--password-stdin"));
    }

    #[test]
    fn read_captcha_accepts_value_and_file() {
        let dir = std::env::temp_dir().join(format!("tianyi-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.txt");
        std::fs::write(&file, "abcd\n").unwrap();
        let ctx = Context::for_test(false, true);
        let creds = Credentials {
            captcha: Some("1234".into()),
            ..Default::default()
        };
        assert_eq!(ctx.read_captcha(&creds).unwrap().as_deref(), Some("1234"));
        let creds = Credentials {
            captcha_file: Some(file.clone()),
            ..Default::default()
        };
        assert_eq!(ctx.read_captcha(&creds).unwrap().as_deref(), Some("abcd"));
        assert_eq!(ctx.read_captcha(&Credentials::default()).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_path_handles_relative_against_cwd() {
        let mut ctx = Context::for_test(false, true);
        assert_eq!(ctx.resolve_path("/a/b").unwrap().display(), "/a/b");
        ctx.cwd = CloudPath::parse("/Documents").unwrap();
        assert_eq!(
            ctx.resolve_path("x.zip").unwrap().display(),
            "/Documents/x.zip"
        );
        assert_eq!(ctx.resolve_path("../y").unwrap().display(), "/y");
        assert!(ctx.resolve_path("/..").is_err());
    }
}
