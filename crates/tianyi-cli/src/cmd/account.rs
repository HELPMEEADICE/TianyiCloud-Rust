//! 账号命令：login / logout / account / status
//!
//! 凭据卫生是本模块的第一约束：
//! * 密码只能来自 stdin、文件、文件描述符或隐藏终端输入——绝不从 argv 读；
//! * 任何输出（含 JSON 与错误文案）都不包含密码、access token、session key；
//! * 验证码图片原样落盘（用户/Agent 自己看），文本再由 --captcha 传回。

use crate::args::{AccountCmd, Credentials, LoginArgs, LoginMode};
use crate::cmd::Runner;
use crate::error::{CliError, CliResult, ErrorCode};
use crate::notify::CliNotifier;
use crate::output::Output;
use crate::table;
use serde_json::json;
use std::time::Duration;
use tianyi_core::models::AccountConfig;

/// 登录
pub async fn login(r: &mut Runner, args: LoginArgs) -> CliResult<Output> {
    let (creds, qr) = match args.mode {
        Some(LoginMode::Password(c)) => (Credentials::merge(c, args.creds.clone()), None),
        Some(LoginMode::Qr(q)) => (args.creds.clone(), Some(q)),
        None => (args.creds.clone(), None),
    };
    match qr {
        None => login_password(r, &creds).await,
        Some(qr) => {
            let file = qr.file.clone().or(args.qr_file.clone());
            let no_art = qr.no_art || args.no_qr_art;
            let timeout = qr.timeout.or(args.qr_timeout).unwrap_or(180);
            login_qr(r, file, no_art, Duration::from_secs(timeout)).await
        }
    }
}

async fn login_password(r: &mut Runner, creds: &Credentials) -> CliResult<Output> {
    let username = match &creds.username {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ if r.ctx.stdin_tty && !r.ctx.non_interactive => {
            eprint!("账号：");
            use std::io::Write;
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(CliError::from)?;
            let u = line.trim().to_string();
            if u.is_empty() {
                return Err(CliError::usage("账号不能为空"));
            }
            u
        }
        _ => return Err(CliError::usage("非交互模式必须用 --username 指定账号")),
    };
    if !creds.has_password_source() && (!r.ctx.stdin_tty || r.ctx.non_interactive) {
        // 提前给出可操作的错误，而不是等到 read_password 里才发现没有来源
        return Err(CliError::usage(
            "非交互模式必须用 --password-stdin、--password-file 或 --password-fd 提供密码",
        ));
    }
    let password = r.ctx.read_password(creds)?;
    let captcha = r.ctx.read_captcha(creds)?;
    let captcha_out = r.ctx.captcha_out(creds);
    let notifier = CliNotifier::new(captcha_out, None, false, r.ctx.printer.is_quiet());

    let result = r
        .ctx
        .service
        .login_password(&username, &password, captcha.as_deref(), &notifier)
        .await;
    match result {
        Ok(info) => Ok(login_output(&info)),
        Err(e) => {
            let mut err = CliError::from(e);
            if err.code == ErrorCode::CaptchaRequired {
                // 把验证码图片位置告诉调用方：Agent 需要它才能继续
                let mut details = json!({});
                if let Some(p) = notifier.captcha_path() {
                    details["captcha_path"] = json!(p.display().to_string());
                }
                details["hint"] = json!("查看图片后，用 --captcha <code> 重新执行 login");
                err = err.with_details(details);
            }
            Err(err)
        }
    }
}

async fn login_qr(
    r: &mut Runner,
    file: Option<std::path::PathBuf>,
    no_art: bool,
    timeout: Duration,
) -> CliResult<Output> {
    // 机器模式下不画终端二维码：stdout 只放结果，stderr 也只留必要的一行 UUID
    let draw = !no_art && !r.ctx.printer.mode().is_machine();
    let notifier = CliNotifier::new(
        r.ctx.config_root().join("captcha-unused.png"),
        file.clone(),
        draw,
        r.ctx.printer.is_quiet(),
    );
    r.ctx
        .printer
        .diag("正在获取二维码，请使用天翼云盘 App 扫码…");
    let info = r.ctx.service.login_qr(&notifier, Some(timeout)).await?;
    let mut out = login_output(&info);
    if let Some(map) = out.data.as_object_mut() {
        map.insert(
            "qr_file".to_string(),
            match &file {
                Some(p) => json!(p.display().to_string()),
                None => serde_json::Value::Null,
            },
        );
        // 二维码内容本身也给出来：Agent 可以把它转发到别处渲染
        map.insert(
            "qr_text".to_string(),
            match notifier.qr_text() {
                Some(t) => json!(t),
                None => serde_json::Value::Null,
            },
        );
    }
    out.human
        .push_str(&format!("扫码登录成功：{}\n", info.username));
    Ok(out)
}

/// 登录成功后的统一输出
fn login_output(info: &tianyi_core::service::SessionInfo) -> Output {
    let data = json!({
        "authenticated": info.authenticated,
        "expired": info.expired,
        "account": info.username,
        "space": info.space.as_str(),
    });
    let human = format!("登录成功：{}（{}）\n", info.username, info.space.as_str());
    Output::new(data, human)
}

/// 退出登录
pub async fn logout(r: &mut Runner) -> CliResult<Output> {
    let info = r.ctx.service.logout()?;
    Ok(Output::new(
        json!({
            "authenticated": info.authenticated,
            "account": info.username,
        }),
        "已退出登录（已保存账号仍可用 account use 切回）\n",
    ))
}

/// account 子命令
pub async fn account(r: &mut Runner, cmd: AccountCmd) -> CliResult<Output> {
    match cmd {
        AccountCmd::List => account_list(r),
        AccountCmd::Use { username } => account_use(r, &username).await,
        AccountCmd::Current => account_current(r),
    }
}

fn account_list(r: &Runner) -> CliResult<Output> {
    let accounts = r.ctx.service.store().load_accounts()?;
    let current = r.ctx.service.session().username;
    let items: Vec<serde_json::Value> = accounts
        .accounts
        .iter()
        .map(|a: &AccountConfig| {
            json!({
                "username": a.username,
                "space": if a.is_family() { "family" } else { "personal" },
                "has_token": !(a.token.access_token.is_empty() && a.token.session_key.is_empty()),
                "current": a.username == current,
            })
        })
        .collect();
    let data = json!({ "accounts": items, "current": current });
    let human = if items.is_empty() {
        "还没有保存的账号，请先执行 login\n".to_string()
    } else {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|v| {
                vec![
                    if v["current"] == json!(true) {
                        "*".to_string()
                    } else {
                        String::new()
                    },
                    v["username"].as_str().unwrap_or_default().to_string(),
                    v["space"].as_str().unwrap_or_default().to_string(),
                    if v["has_token"] == json!(true) {
                        "已登录".to_string()
                    } else {
                        "需要重新登录".to_string()
                    },
                ]
            })
            .collect();
        table::render_table(&["", "账号", "空间", "登录态"], &rows)
    };
    Ok(Output::new(data, human))
}

async fn account_use(r: &mut Runner, username: &str) -> CliResult<Output> {
    let info = r.ctx.service.use_account(username).await?;
    Ok(Output::new(
        json!({
            "authenticated": info.authenticated,
            "account": info.username,
            "space": info.space.as_str(),
        }),
        format!("已切换账号：{}\n", info.username),
    ))
}

fn account_current(r: &Runner) -> CliResult<Output> {
    let info = r.ctx.service.session();
    Ok(Output::new(
        json!({
            "authenticated": info.authenticated,
            "expired": info.expired,
            "account": info.username,
            "space": info.space.as_str(),
            "family_id": info.family_id,
        }),
        format!(
            "{}{}\n",
            if info.username.is_empty() {
                "未登录"
            } else {
                &info.username
            },
            if info.authenticated {
                ""
            } else if info.expired {
                "（登录态已失效）"
            } else {
                ""
            }
        ),
    ))
}

/// status
pub async fn status(r: &mut Runner) -> CliResult<Output> {
    let info = r.ctx.service.session();
    let tasks = r.tasks.list();
    let active = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                tianyi_core::task::TaskStatus::Pending
                    | tianyi_core::task::TaskStatus::Running
                    | tianyi_core::task::TaskStatus::Paused
            )
        })
        .count();
    let finished = tasks.len() - active;
    let config_dir = r.ctx.config_root().display().to_string();
    let version = env!("CARGO_PKG_VERSION");
    let data = table::status_value(&info, version, &config_dir, active, finished);
    let human = table::render_status(&info, version, &config_dir, active);
    Ok(Output::new(data, human))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_output_never_contains_secrets() {
        let info = tianyi_core::service::SessionInfo {
            authenticated: true,
            expired: false,
            username: "13800138000".to_string(),
            space: tianyi_core::service::Space::Personal,
            family_id: String::new(),
        };
        let out = login_output(&info);
        assert_eq!(out.data["authenticated"], json!(true));
        assert_eq!(out.data["account"], json!("13800138000"));
        let text = crate::output::compact(&out.data);
        for secret in ["token", "secret", "password", "session"] {
            assert!(
                !text.to_ascii_lowercase().contains(secret),
                "JSON 里不允许出现 {secret}"
            );
        }
    }
}
