//! 交互式 Shell 与 NDJSON 会话
//!
//! 两种持续运行的形态：
//! * **REPL**（stdin 是终端时的 -c）：人类用，带历史、补全、提示符显示当前目录；
//! * **NDJSON 会话**（-c --jsonl）：机器用，一行一条请求、一条响应，带关联 id。
//!
//! 两者共用同一套「一行文本 → 一次性命令」的解析路径：REPL 里敲的命令与
//! tianyi-app -c <命令> 完全同构（都走 clap 解析），因此不存在「交互里能做的事命令行
//! 做不了」这种分叉。cd / pwd / clear / help / exit 是只为交互存在的会话内命令，
//! 它们不改变任何云端状态，只动本进程的工作目录。

use crate::args::{Cli, Command};
use crate::cmd::Runner;
use crate::error::{CliError, CliResult, ErrorCode};
use crate::interrupt::interrupted_error;
use crate::output::Output;
use clap::{CommandFactory, Parser};
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Editor, Helper};
use serde_json::json;
use std::io::{BufRead, Write};
use std::sync::Arc;

/// 会话内命令（不属于云端 API）
const SESSION_COMMANDS: &[&str] = &[
    "cd", "pwd", "clear", "cls", "help", "exit", "quit", "history",
];

/// 把一行文本切成参数（支持单引号、双引号与反斜杠转义）
///
/// 为什么不用 shell-words 库：规则只有三条，自己实现 30 行，省一个依赖；
/// 而且这里的语义要和「文件名里带空格」严格对齐——引号内的空格必须保留。
pub fn shell_split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            '\\' if !in_single => {
                // 反斜杠转义下一个字符（单引号内不生效，与 POSIX shell 一致）
                if let Some(next) = chars.next() {
                    cur.push(next);
                    started = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 补全器：补全会话命令与 CLI 子命令
struct ReplHelper {
    words: Vec<String>,
}

impl Completer for ReplHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let start = line[..pos]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &line[start..pos];
        let mut hits: Vec<String> = self
            .words
            .iter()
            .filter(|w| w.starts_with(word) && w.as_str() != word)
            .cloned()
            .collect();
        hits.sort();
        hits.dedup();
        Ok((start, hits))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}

impl Highlighter for ReplHelper {}

impl Validator for ReplHelper {}

impl Helper for ReplHelper {}

fn helper() -> ReplHelper {
    let mut words: Vec<String> = SESSION_COMMANDS.iter().map(|s| s.to_string()).collect();
    words.extend(
        [
            "login", "logout", "account", "status", "ls", "info", "search", "mkdir", "rename",
            "mv", "cp", "rm", "link", "quota", "space", "upload", "download", "tasks", "task",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    words.extend(
        [
            "list", "use", "current", "password", "qr", "info", "pause", "resume", "cancel",
            "remove",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    ReplHelper { words }
}

/// 提示符：根目录是 tianyi> ，子目录显示当前路径
pub fn prompt_for(runner: &Runner) -> String {
    let cwd = runner.ctx.cwd.display();
    if cwd == "/" {
        "tianyi> ".to_string()
    } else {
        format!("tianyi{cwd}> ")
    }
}

/// 是否退出
fn is_exit(tokens: &[String]) -> bool {
    matches!(
        tokens.first().map(String::as_str),
        Some("exit") | Some("quit")
    )
}

/// 执行一行（会话命令或一次性命令）
async fn execute_tokens(runner: &mut Runner, tokens: &[String]) -> CliResult<Output> {
    if tokens.is_empty() {
        return Err(CliError::usage("空命令"));
    }
    match tokens[0].as_str() {
        "cd" => {
            let target = tokens.get(1).map(String::as_str).unwrap_or("/");
            let path = runner.ctx.resolve_path(target)?;
            // 真的去列一次目录：不存在或不是目录要当场报错，而不是把 cwd 改到一个坏路径
            runner.ctx.service.resolve_dir(&path).await?;
            runner.ctx.cwd = path.clone();
            Ok(Output::new(
                json!({ "cwd": path.display() }),
                format!("{}\n", path.display()),
            ))
        }
        "pwd" => {
            let cwd = runner.ctx.cwd.display();
            Ok(Output::new(
                json!({ "cwd": cwd.clone() }),
                format!("{cwd}\n"),
            ))
        }
        "clear" | "cls" => Ok(Output::text("\u{1b}[2J\u{1b}[H")),
        "help" => {
            let mut cmd = Cli::command();
            let text = cmd.render_long_help().to_string();
            Ok(Output::new(
                json!({ "session_commands": SESSION_COMMANDS }),
                format!("{text}\n会话内命令：cd <目录> / pwd / clear / help / history / exit\n"),
            ))
        }
        other if SESSION_COMMANDS.contains(&other) => {
            Err(CliError::usage(format!("{other} 只能用在交互模式里")))
        }
        _ => {
            // 与一次性命令完全同构：直接把这一行当成 tianyi-app -c <args> 解析
            let argv = std::iter::once("tianyi-app".to_string())
                .chain(tokens.iter().cloned())
                .collect::<Vec<_>>();
            let cli = Cli::try_parse_from(argv).map_err(clap_error)?;
            let Some(cmd) = cli.command else {
                return Err(CliError::usage("空命令"));
            };
            run_command(runner, cmd).await
        }
    }
}

/// 非传输命令在执行期间可被 Ctrl+C 立即放弃；传输命令交给协作式暂停
async fn run_command(runner: &mut Runner, cmd: Command) -> CliResult<Output> {
    if Runner::is_transfer_like(&cmd) {
        return runner.execute(cmd).await;
    }
    let hub = Arc::clone(&runner.hub);
    tokio::select! {
        r = runner.execute(cmd) => r,
        _ = hub.interrupted_signal() => Err(interrupted_error()),
    }
}

/// clap 的错误转成稳定的 CliError
fn clap_error(e: clap::Error) -> CliError {
    let kind = e.kind();
    let text = e.to_string();
    let text = text.trim_end().to_string();
    match kind {
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
            CliError::new(ErrorCode::Usage, text)
        }
        _ => CliError::usage(text),
    }
}

/// 读取历史文件
fn load_history(path: Option<&std::path::PathBuf>) -> Vec<String> {
    let Some(path) = path else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 保存历史文件（只保留最近 2000 条；失败不影响退出）
fn save_history(path: Option<&std::path::PathBuf>, history: &[String]) {
    let Some(path) = path else { return };
    let start = history.len().saturating_sub(2000);
    let body = history[start..].join("\n");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body + "\n");
}

/// 进交互式 Shell
pub async fn run_repl(runner: &mut Runner) -> CliResult<()> {
    let machine = runner.ctx.printer.mode().is_machine();
    if !machine {
        println!("TianyiCloud-Rust CLI {}", env!("CARGO_PKG_VERSION"));
        println!("输入 help 查看命令，exit 退出。");
    }
    let history_path = runner.ctx.service.store().history_path();
    let mut history = load_history(history_path.as_ref());
    let mut editor: Option<Editor<ReplHelper, DefaultHistory>> = if machine {
        None
    } else {
        let cfg = Config::builder().auto_add_history(false).build();
        match Editor::<ReplHelper, DefaultHistory>::with_config(cfg) {
            Ok(mut ed) => {
                ed.set_helper(Some(helper()));
                Some(ed)
            }
            Err(e) => {
                runner
                    .ctx
                    .printer
                    .diag(&format!("行编辑不可用（{e}），退化为逐行读取"));
                None
            }
        }
    };

    loop {
        // 每条命令开始前清一次中断状态：上一轮的 Ctrl+C 不该影响下一条命令
        runner.hub.detach();
        let line = if let Some(ed) = editor.as_mut() {
            let prompt = prompt_for(runner);
            match ed.readline(&prompt) {
                Ok(l) => Some(l),
                Err(ReadlineError::Interrupted) => {
                    println!("^C");
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => {
                    return Err(CliError::new(
                        ErrorCode::LocalIo,
                        format!("读取输入失败：{e}"),
                    ))
                }
            }
        } else {
            let _ = std::io::stderr().write_all(b"");
            let mut buf = String::new();
            match std::io::stdin().lock().read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => Some(buf),
                Err(e) => {
                    return Err(CliError::new(
                        ErrorCode::LocalIo,
                        format!("读取输入失败：{e}"),
                    ))
                }
            }
        };
        let Some(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(ed) = editor.as_mut() {
            let _ = ed.add_history_entry(trimmed.to_string());
        }
        history.push(trimmed.to_string());
        let tokens = shell_split(trimmed);
        if is_exit(&tokens) {
            break;
        }
        // history 命令要看到完整历史，放在分发之前处理
        if tokens.first().map(String::as_str) == Some("history") {
            let body: String = history
                .iter()
                .enumerate()
                .map(|(i, h)| format!("{:>4}  {h}\n", i + 1))
                .collect();
            println!("{body}");
            continue;
        }
        match execute_tokens(runner, &tokens).await {
            Ok(out) => {
                runner.ctx.printer.emit(&out)?;
            }
            Err(e) => runner.ctx.printer.emit_error(&e),
        }
    }
    save_history(history_path.as_ref(), &history);
    if !machine {
        println!("再见");
    }
    Ok(())
}

/// NDJSON 会话：一行一条请求，一条请求一条响应
///
/// 请求格式（三选一，必须带 id 才能和响应对上）：
///   {"id":1,"command":"ls / --long"}
///   {"id":2,"argv":["ls","/"]}
/// 纯文本行也接受（等价于 REPL 的一行，id 记 0）。
pub async fn run_ndjson(runner: &mut Runner) -> CliResult<()> {
    let stdin = std::io::stdin();
    loop {
        runner.hub.detach();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(CliError::new(
                    ErrorCode::LocalIo,
                    format!("读取请求失败：{e}"),
                ))
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (id, tokens) = match parse_request(trimmed) {
            Ok(v) => v,
            Err(e) => {
                runner.ctx.printer.emit_error_with_id(0, &e);
                continue;
            }
        };
        if tokens.is_empty() {
            runner
                .ctx
                .printer
                .emit_error_with_id(id, &CliError::usage("空命令"));
            continue;
        }
        if is_exit(&tokens) {
            runner
                .ctx
                .printer
                .emit_ok_with_id(id, json!({ "bye": true }));
            break;
        }
        match execute_tokens(runner, &tokens).await {
            Ok(out) => runner.ctx.printer.emit_ok_with_id(id, out.data),
            Err(e) => runner.ctx.printer.emit_error_with_id(id, &e),
        }
    }
    Ok(())
}

/// 解析一条 NDJSON 请求
fn parse_request(line: &str) -> CliResult<(u64, Vec<String>)> {
    if !line.starts_with('{') {
        return Ok((0, shell_split(line)));
    }
    let v: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| CliError::usage(format!("请求不是合法 JSON：{e}")))?;
    let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
    let tokens = if let Some(s) = v.get("command").and_then(|c| c.as_str()) {
        shell_split(s)
    } else if let Some(arr) = v
        .get("argv")
        .or_else(|| v.get("cmd"))
        .and_then(|a| a.as_array())
    {
        arr.iter()
            .map(|x| match x.as_str() {
                Some(s) => Ok(s.to_string()),
                None => Err(CliError::usage("argv 里只能放字符串")),
            })
            .collect::<CliResult<Vec<String>>>()?
    } else {
        return Err(CliError::usage("请求里需要 command 字符串或 argv 数组"));
    };
    Ok((id, tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        assert_eq!(shell_split("ls /"), vec!["ls", "/"]);
        assert_eq!(
            shell_split("ls \"/My Documents\""),
            vec!["ls", "/My Documents"]
        );
        assert_eq!(
            shell_split("download '/a b/c.zip' ./"),
            vec!["download", "/a b/c.zip", "./"]
        );
        // 引号内的空格必须保留，引号外的多个空格折叠
        assert_eq!(
            shell_split("upload   './我的 文件.zip'   /Backup"),
            vec!["upload", "./我的 文件.zip", "/Backup"]
        );
        assert_eq!(shell_split("upload a\\ b /x"), vec!["upload", "a b", "/x"]);
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn shell_split_keeps_chinese_and_special_chars() {
        assert_eq!(
            shell_split("mkdir /文档/2024（终稿）"),
            vec!["mkdir", "/文档/2024（终稿）"]
        );
        assert_eq!(shell_split("ls \"a*b?c\""), vec!["ls", "a*b?c"]);
    }

    #[test]
    fn ndjson_request_accepts_command_and_argv() {
        let (id, tokens) = parse_request("{\"id\":9,\"command\":\"ls / --long\"}").unwrap();
        assert_eq!(id, 9);
        assert_eq!(tokens, vec!["ls", "/", "--long"]);

        let (id, tokens) = parse_request("{\"id\":3,\"argv\":[\"status\"]}").unwrap();
        assert_eq!(id, 3);
        assert_eq!(tokens, vec!["status"]);

        // 纯文本行也能用（id 记为 0）
        let (id, tokens) = parse_request("pwd").unwrap();
        assert_eq!(id, 0);
        assert_eq!(tokens, vec!["pwd"]);
    }

    #[test]
    fn ndjson_request_rejects_malformed_lines() {
        assert!(parse_request("{\"id\":1}").is_err());
        assert!(parse_request("{\"id\":1,\"argv\":[1,2]}").is_err());
        assert!(parse_request("{not json").is_err());
    }

    #[test]
    fn exit_tokens_are_recognized() {
        assert!(is_exit(&shell_split("exit")));
        assert!(is_exit(&shell_split("quit")));
        // 只看第一个词：多打的参数不该让 exit 失效
        assert!(is_exit(&shell_split("exit now please")));
        assert!(!is_exit(&shell_split("logout")));
        assert!(!is_exit(&[]));
    }

    #[test]
    fn completion_matches_prefix() {
        let h = helper();
        // 只验证候选集里包含已知命令（运行时不依赖具体顺序）
        assert!(h.words.iter().any(|w| w == "upload"));
        assert!(h.words.iter().any(|w| w == "cd"));
    }
}
