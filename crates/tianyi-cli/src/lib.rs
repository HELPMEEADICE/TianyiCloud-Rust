//! 天翼云盘 Headless CLI
//!
//! 入口契约（tianyi-app 在决定走 CLI 之后调用 [run]）：
//! * 参数解析、日志、runtime 都在这里搭起来，然后按「有没有子命令」分成三条路：
//!   一次性命令 / 交互式 Shell / NDJSON 会话；
//! * **不引入任何图形依赖**：本 crate 的依赖树里只有 tianyi-core 与命令行工具库，
//!   因此 --no-default-features --features cli 的产物在无 DISPLAY 的容器里也能跑；
//! * stdout 与 stderr 的分工由 [output::Printer] 统一负责，命令实现不直接 println。
//!
//! 退出码见 [error::ErrorCode::exit_code]（2 参数 / 3 认证 / 4 网络 / 5 远端 /
//! 6 本地 IO / 7 中断 / 8 不存在 / 9 忙碌 / 10 挂载环境不可用 / 11 挂载或卸载失败 /
//! 12 本来就没挂载）。

mod args;
mod cmd;
mod context;
mod error;
mod interrupt;
mod notify;
mod output;
mod qr;
mod repl;
mod table;

pub use error::{CliError, ErrorCode};

use args::Cli;
use clap::Parser;
use cmd::Runner;
use context::{with_timeout, Context};
use error::CliResult;
use interrupt::{interrupted_error, InterruptHub};
use output::{OutMode, Printer};
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;

/// 命令行入口
pub fn run(argv: Vec<OsString>) -> ExitCode {
    let parsed =
        match Cli::try_parse_from(std::iter::once(OsString::from("tianyi-app")).chain(argv)) {
            Ok(cli) => cli,
            // --help / --version 走 stdout 且退出码 0；其余（未知命令、缺值、类型错）走
            // stderr 且退出码 2
            Err(e) => {
                let kind = e.kind();
                let _ = e.print();
                let is_info = matches!(
                    kind,
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayVersion
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                );
                return ExitCode::from(if is_info { 0 } else { 2 });
            }
        };
    init_logging(&parsed);
    let runtime = match build_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("初始化运行时失败：{e}");
            return ExitCode::from(1);
        }
    };
    let code = runtime.block_on(run_async(parsed));
    // 显式关闭 runtime：等后台任务（信号监听、进度 ticker）被 drop，
    // 避免进程退出时残留线程或阻塞在阻塞线程池的回收上。
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    ExitCode::from(code)
}

/// 真正干活的异步主体
async fn run_async(cli: Cli) -> u8 {
    let mode = context::out_mode(&cli);
    let printer = Printer::new(mode, cli.quiet);
    let ctx = match Context::bootstrap(&cli).await {
        Ok(ctx) => ctx,
        Err(e) => {
            printer.emit_error(&e);
            return e.exit_code();
        }
    };
    let hub = InterruptHub::new();
    hub.install();
    let mut runner = Runner::new(ctx, hub);
    match cli.command {
        Some(cmd) => {
            let timeout = runner.ctx.timeout;
            let result = if Runner::is_transfer_like(&cmd) {
                // 传输必须让出收尾机会：不加 select 抢断，靠 TransferControl 协作暂停
                with_timeout(timeout, runner.execute(cmd)).await
            } else {
                let hub = Arc::clone(&runner.hub);
                let fut = with_timeout(timeout, runner.execute(cmd));
                tokio::select! {
                    r = fut => r,
                    _ = hub.interrupted_signal() => Err(interrupted_error()),
                }
            };
            match result {
                Ok(out) => {
                    if let Err(e) = printer.emit(&out) {
                        printer.emit_error(&e);
                        return e.exit_code();
                    }
                    0
                }
                Err(e) => {
                    printer.emit_error(&e);
                    report_auth_hint(&runner, &e);
                    e.exit_code()
                }
            }
        }
        None => run_session(&mut runner, mode).await,
    }
}

/// 没有子命令时：NDJSON 会话 / 交互式 Shell / 明确的用法错误
async fn run_session(runner: &mut Runner, mode: OutMode) -> u8 {
    if matches!(mode, OutMode::Ndjson) {
        let result = repl::run_ndjson(runner).await;
        return finish(runner, result);
    }
    if runner.ctx.non_interactive || !runner.ctx.stdin_tty {
        let err = CliError::new(
            ErrorCode::Usage,
            "-c 后面需要子命令：当前 stdin 不是终端（或指定了 --non-interactive），\
             不会进入交互式 Shell。用法示例：tianyi-app -c status、tianyi-app -c ls /、\
             tianyi-app -c --help",
        );
        runner.ctx.printer.emit_error(&err);
        return err.exit_code();
    }
    let result = repl::run_repl(runner).await;
    finish(runner, result)
}

fn finish(runner: &Runner, result: CliResult<()>) -> u8 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            runner.ctx.printer.emit_error(&e);
            report_auth_hint(runner, &e);
            e.exit_code()
        }
    }
}

/// 认证类错误在 stderr 上再给一条「怎么修」的提示
///
/// 只写 stderr：stdout 的 JSON 协议里已经有稳定的错误码，多塞提示会破坏结构。
fn report_auth_hint(runner: &Runner, err: &CliError) {
    if matches!(err.code, ErrorCode::AuthRequired | ErrorCode::AuthExpired) {
        runner.ctx.printer.diag(&runner.ctx.account_hint());
    }
}

/// 日志：写 stderr，遵循 RUST_LOG，默认 warn
///
/// 绝不写 stdout：那里是结果协议。--quiet 只把默认级别压到 error，用户显式设置的
/// RUST_LOG 依然有效（排错时不必改命令行）。
fn init_logging(cli: &Cli) {
    let default = if cli.quiet { "error" } else { "warn" };
    let env = env_logger::Env::default().default_filter_or(default);
    let mut builder = env_logger::Builder::from_env(env);
    builder.target(env_logger::Target::Stderr);
    let _ = builder.try_init();
}

/// tokio 运行时：4 个 worker
///
/// 与 GUI 的 Backend 保持一致（那里也是 4）。传输的分片并发是异步的，4 个 worker 足够；
/// worker 太多只会抬高 headless 环境的常驻内存。
fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些参数必须能通过 clap 解析（不联网、不建 runtime）
    #[test]
    fn documented_command_shapes_parse() {
        for argv in [
            vec!["-c"],
            vec!["-c", "status"],
            vec!["-c", "ls", "/"],
            vec!["-c", "--json", "ls", "/"],
            vec!["-c", "--jsonl"],
            vec!["-c", "upload", "./a.zip", "/"],
            vec!["-c", "download", "/a.zip", "./"],
            vec!["-c", "task", "resume", "3"],
            vec!["-c", "--json", "--non-interactive", "status"],
            vec!["-c", "mount", "X:", "--read-only", "--no-disk-cache"],
            vec!["-c", "mount", "~/TianyiCloud", "--read-only"],
            vec![
                "-c",
                "--space",
                "family",
                "--account",
                "NAME",
                "mount",
                "--unc-prefix",
                "\\\\TianyiCloud\\Family",
            ],
            vec!["-c", "mount", "--parent-pid", "4242"],
            vec!["-c", "unmount", "X:"],
            vec!["-c", "mounts"],
            vec!["-c", "--json", "mounts"],
        ] {
            let parsed = Cli::try_parse_from(std::iter::once("tianyi-app").chain(argv.clone()));
            assert!(parsed.is_ok(), "应能解析 {:?}：{:?}", argv, parsed.err());
        }
    }

    #[test]
    fn json_and_jsonl_modes_are_reported_correctly() {
        let cli = Cli::try_parse_from(["tianyi-app", "-c", "--json", "status"]).unwrap();
        assert_eq!(context::out_mode(&cli), OutMode::Json);
        let cli = Cli::try_parse_from(["tianyi-app", "-c", "--jsonl"]).unwrap();
        assert_eq!(context::out_mode(&cli), OutMode::Ndjson);
        let cli = Cli::try_parse_from(["tianyi-app", "-c", "status"]).unwrap();
        assert_eq!(context::out_mode(&cli), OutMode::Human);
    }
}
