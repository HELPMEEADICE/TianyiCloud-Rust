//! 天翼云盘启动器
//!
//! 唯一职责：**在加载任何图形后端之前**决定这次是 GUI 还是 CLI，然后转交。
//! 这条规则有三条硬约束，改动时必须同时满足：
//!
//! 1. 默认（不带参数）行为与历史版本完全一致——直接打开 Slint 主窗口；
//! 2. 一旦判定为 CLI，绝不能构造 MainWindow、不能初始化 winit、不能碰图形后端，
//!    因为那些代码在纯 CLI 构建里**根本不存在**（feature 未启用），
//!    在混合构建里也不该被无谓地拉起来；
//! 3. 纯 CLI 构建（--no-default-features --features cli）在无参数启动时要给出明确的
//!    「本构建没有图形界面」提示并以非零码退出，而不是崩溃或静默成功。

use std::ffi::OsString;
use std::process::ExitCode;

/// 明确的 CLI 子命令名：让 tianyi-app ls / 这种省略 -c 的写法也能进 CLI
const CLI_SUBCOMMANDS: &[&str] = &[
    "login", "logout", "account", "status", "ls", "info", "search", "mkdir", "rename", "mv", "cp",
    "rm", "link", "quota", "space", "upload", "download", "tasks", "task", "mount", "unmount",
    "mounts", "help",
];

/// 只在 CLI 里存在的全局选项（出现即意味着用户要的是命令行）
const CLI_GLOBAL_FLAGS: &[&str] = &[
    "--json",
    "--jsonl",
    "--ndjson",
    "--non-interactive",
    "--yes",
    "-y",
    "--timeout",
    "--config-dir",
    "--account",
    "--space",
    "--quiet",
    "-q",
];

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    if wants_cli(&argv) {
        return run_cli(argv);
    }
    run_gui()
}

/// 判断这次是否要走 CLI
///
/// 判定依据只有「明确的 CLI 信号」，不认识参数一律维持 GUI（历史行为）：
/// macOS 从 Finder 启动时可能带 -psn_... 之类的系统参数，把它们当成 CLI 会让用户
/// 双击图标却看到终端报错。
fn wants_cli(argv: &[OsString]) -> bool {
    for arg in argv {
        let Some(text) = arg.to_str() else {
            // 非 UTF-8 参数：维持历史行为（GUI）
            return false;
        };
        if text == "--" {
            break;
        }
        if !text.starts_with('-') {
            return CLI_SUBCOMMANDS.contains(&text);
        }
        if matches!(text, "-c" | "--cli" | "-h" | "--help" | "-V" | "--version") {
            return true;
        }
        // --timeout=30 这种带值写法按 '=' 前半段比对
        let name = text.split('=').next().unwrap_or(text);
        if CLI_GLOBAL_FLAGS.contains(&name) {
            return true;
        }
    }
    false
}

#[cfg(feature = "cli")]
fn run_cli(argv: Vec<OsString>) -> ExitCode {
    tianyi_cli::run(argv)
}

#[cfg(not(feature = "cli"))]
fn run_cli(_argv: Vec<OsString>) -> ExitCode {
    eprintln!("本构建没有编译命令行支持（缺少 cli feature）。");
    eprintln!("请使用默认构建，或用 --features cli 重新构建。");
    ExitCode::from(2)
}

#[cfg(feature = "gui")]
fn run_gui() -> ExitCode {
    match tianyi_gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("启动失败：{e:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "gui"))]
fn run_gui() -> ExitCode {
    eprintln!("本构建没有编译图形界面（缺少 gui feature）。");
    eprintln!("这个可执行文件只提供命令行，请加 -c 使用，例如：");
    eprintln!("  tianyi-app -c status");
    eprintln!("  tianyi-app -c ls /");
    eprintln!("用法帮助：tianyi-app -c --help");
    ExitCode::from(2)
}

#[cfg(not(any(feature = "gui", feature = "cli")))]
compile_error!("至少要启用一个 feature：gui 或 cli");

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_keeps_the_gui_behaviour() {
        assert!(!wants_cli(&[]), "不带参数必须走 GUI");
    }

    #[test]
    fn dash_c_and_its_companions_select_cli() {
        assert!(wants_cli(&args(&["-c"])));
        assert!(wants_cli(&args(&["--cli"])));
        assert!(wants_cli(&args(&["-c", "status"])));
        assert!(wants_cli(&args(&["-c", "--json", "ls", "/"])));
        assert!(wants_cli(&args(&["--json", "status"])));
        assert!(wants_cli(&args(&["--non-interactive", "ls", "/"])));
        assert!(wants_cli(&args(&["--timeout=30", "ls", "/"])));
        assert!(wants_cli(&args(&["-c", "--help"])));
        assert!(wants_cli(&args(&["--version"])));
    }

    #[test]
    fn bare_subcommand_without_dash_c_also_goes_to_cli() {
        assert!(wants_cli(&args(&["status"])));
        assert!(wants_cli(&args(&["ls", "/"])));
        assert!(wants_cli(&args(&["task", "info", "3"])));
    }

    #[test]
    fn mount_family_is_recognised_as_cli() {
        // 挂载是长时间前台命令：省略 -c 时也必须进 CLI，不能被当成未知参数丢回 GUI
        assert!(wants_cli(&args(&["mount"])));
        assert!(wants_cli(&args(&["mount", "X:"])));
        assert!(wants_cli(&args(&["mount", "--space", "family"])));
        assert!(wants_cli(&args(&["unmount", "X:"])));
        assert!(wants_cli(&args(&["mounts"])));
        assert!(wants_cli(&args(&["--json", "mounts"])));
        assert!(wants_cli(&args(&["-c", "mount", "X:", "--read-only"])));
    }

    #[test]
    fn cli_subcommand_table_covers_every_documented_command() {
        // 这张表是「省略 -c 也能用」的白名单，漏一个就等于该命令在双击场景下静默走 GUI
        for name in ["mount", "unmount", "mounts"] {
            assert!(
                CLI_SUBCOMMANDS.contains(&name),
                "CLI_SUBCOMMANDS 缺少 {name}"
            );
        }
    }

    #[test]
    fn unknown_platform_arguments_keep_the_gui_behaviour() {
        // macOS Finder 启动残留参数、以及任何未知开关：不能被当成 CLI
        assert!(!wants_cli(&args(&["-psn_0_12345"])));
        assert!(!wants_cli(&args(&["--some-random-flag"])));
    }

    #[test]
    fn double_dash_stops_cli_detection() {
        assert!(!wants_cli(&args(&["--", "status"])));
    }
}
