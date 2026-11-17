//! CLI 端到端测试：直接驱动真实二进制
//!
//! 为什么用子进程而不是在进程内调用：这里要证明的正是「进程级行为」——
//! 退出码、stdout 纯净度、无 TTY 时不会挂住、没有 DISPLAY 也能跑。
//! 进程内测试永远证明不了这些。
//!
//! 每个用例都在自己的临时配置目录里跑（XDG_CONFIG_HOME + HOME 都指过去），
//! 因此不会碰开发机上的真实登录态，也不会读到用户的 tasks.json。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tianyi-app")
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "tianyi-cli-e2e-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Sandbox { root }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config").join("tianyi-cloud-rust")
    }

    /// 跑一次 CLI：配置目录固定指向沙箱，清掉图形环境变量，stdin 默认给空（非 TTY）
    ///
    /// **必须显式传 --config-dir**：Windows 上 dirs::config_dir() 走的是
    /// SHGetKnownFolderPath，改环境变量（APPDATA / XDG_CONFIG_HOME）都不能把它重定向到
    /// 临时目录——只靠环境变量会读到开发机上的真实登录态与真实 tasks.json。
    fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, None)
    }

    /// 不注入配置目录的原始调用（只有「--config-dir 生效」那一条用例需要）
    fn run_raw(&self, args: &[&str]) -> Output {
        self.spawn(args, None)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let dir = self.config_dir();
        let mut full: Vec<String> = vec![
            "--config-dir".to_string(),
            dir.to_string_lossy().to_string(),
        ];
        full.extend(args.iter().map(|s| s.to_string()));
        let borrowed: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
        self.spawn(&borrowed, stdin)
    }

    fn spawn(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env_clear()
            // 只保留运行必需品：PATH 让二进制能找到系统库，其余一概不继承
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("TMPDIR", self.root.join("home"))
            .env("RUST_LOG", "error")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        // 断言「没有图形环境也能跑」：这两个变量绝不设置
        cmd.env_remove("DISPLAY");
        cmd.env_remove("WAYLAND_DISPLAY");
        let mut child = cmd.spawn().expect("启动 tianyi-app 失败");
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("stdin");
            let _ = pipe.write_all(text.as_bytes());
            let _ = pipe.flush();
        }
        child.wait_with_output().expect("等待子进程失败")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn json_of(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout_of(out)).unwrap_or_else(|e| {
        panic!(
            "stdout 必须是合法 JSON（{e}）\nstdout={}\nstderr={}",
            stdout_of(out),
            stderr_of(out)
        )
    })
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

#[test]
fn version_and_help_do_not_need_a_display() {
    let sb = Sandbox::new("version");
    let out = sb.run(&["-c", "--version"]);
    assert_eq!(code(&out), 0, "stderr={}", stderr_of(&out));
    assert!(stdout_of(&out).contains("tianyi-app"));

    let out = sb.run(&["-c", "--help"]);
    assert_eq!(code(&out), 0);
    let text = stdout_of(&out);
    for expected in ["status", "ls", "upload", "download", "login", "task"] {
        assert!(text.contains(expected), "帮助里缺少 {expected}：{text}");
    }
}

#[test]
fn status_json_is_machine_readable_and_reports_logged_out() {
    let sb = Sandbox::new("status");
    let out = sb.run(&["-c", "--json", "--non-interactive", "status"]);
    assert_eq!(code(&out), 0, "stderr={}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["success"], serde_json::json!(true));
    assert_eq!(v["error"], serde_json::Value::Null);
    assert_eq!(v["data"]["authenticated"], serde_json::json!(false));
    assert_eq!(v["data"]["space"], serde_json::json!("personal"));
    assert!(v["data"]["config_dir"]
        .as_str()
        .unwrap()
        .contains("tianyi-cloud-rust"));
}

#[test]
fn stdout_stays_pure_json_even_with_diagnostics() {
    let sb = Sandbox::new("purity");
    let out = sb.run(&["-c", "--json", "account", "list"]);
    assert_eq!(code(&out), 0);
    // 严格解析：多一个字节的非 JSON 前缀都会让这里失败
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout 必须整体是 JSON");
    assert_eq!(v["success"], serde_json::json!(true));
    assert_eq!(v["data"]["accounts"], serde_json::json!([]));
    assert!(
        !stdout_of(&out).contains('\u{1b}'),
        "JSON 输出里不能有 ANSI 转义"
    );
}

#[test]
fn cloud_operations_without_login_exit_with_auth_code() {
    let sb = Sandbox::new("auth");
    for args in [
        vec!["-c", "--json", "ls", "/"],
        vec!["-c", "--json", "quota"],
        vec!["-c", "--json", "info", "/x"],
    ] {
        let out = sb.run(&args);
        assert_eq!(
            code(&out),
            3,
            "命令 {:?} 应以认证错误退出\nstderr={}",
            args,
            stderr_of(&out)
        );
        let v = json_of(&out);
        assert_eq!(v["success"], serde_json::json!(false));
        assert_eq!(v["error"]["code"], serde_json::json!("AUTH_REQUIRED"));
        assert_eq!(v["error"]["retryable"], serde_json::json!(false));
    }
}

#[test]
fn no_tty_without_subcommand_is_a_usage_error_not_a_hang() {
    let sb = Sandbox::new("notty");
    let out = sb.run(&["-c"]);
    assert_eq!(code(&out), 2, "stderr={}", stderr_of(&out));
    let text = stderr_of(&out);
    assert!(
        text.contains("子命令") || text.contains("stdin"),
        "必须说明为什么不能进入交互模式：{text}"
    );
}

#[test]
fn unknown_command_and_bad_flags_exit_two() {
    let sb = Sandbox::new("usage");
    let out = sb.run(&["-c", "definitely-not-a-command"]);
    assert_eq!(code(&out), 2);

    let out = sb.run(&["-c", "--json", "--jsonl", "status"]);
    assert_eq!(code(&out), 2, "两个互斥输出模式必须报错");

    let out = sb.run(&["-c", "task", "info", "not-a-number"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn destructive_commands_need_yes_in_non_interactive_mode() {
    let sb = Sandbox::new("confirm");
    // 未登录时先撞上认证错误；这里验证的是「不会因为等待人工输入而挂住」
    let started = std::time::Instant::now();
    let out = sb.run(&["-c", "--json", "--non-interactive", "rm", "/whatever"]);
    assert!(started.elapsed().as_secs() < 20, "绝不能卡在输入上");
    assert_eq!(code(&out), 3);
}

#[test]
fn tasks_command_reads_the_shared_tasks_file() {
    let sb = Sandbox::new("tasks");
    let out = sb.run(&["-c", "--json", "tasks"]);
    assert_eq!(code(&out), 0, "stderr={}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["data"]["items"], serde_json::json!([]));
    assert_eq!(v["data"]["counts"]["active"], serde_json::json!(0));
}

#[test]
fn account_list_never_leaks_credentials() {
    let sb = Sandbox::new("secrets");
    // 手工写入一份带 token 的账号表，确认输出里不会把 token 带出来
    let dir = sb.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let accounts = serde_json::json!({
        "accounts": [{
            "username": "13800138000",
            "login_type": "password",
            "family_id": "",
            "space_type": "personal",
            "order_by": "",
            "order_direction": "",
            "upload_method": "",
            "upload_thread": 0,
            "rapid_upload": false,
            "token": {
                "access_token": "ACCESS-TOKEN-DO-NOT-LEAK",
                "refresh_token": "REFRESH-TOKEN-DO-NOT-LEAK",
                "session_key": "SESSION-KEY-DO-NOT-LEAK",
                "session_secret": "SESSION-SECRET-DO-NOT-LEAK",
                "family_session_key": "",
                "family_session_secret": "",
                "login_name": "13800138000"
            }
        }]
    });
    std::fs::write(
        dir.join("accounts.json"),
        serde_json::to_vec_pretty(&accounts).unwrap(),
    )
    .unwrap();

    let out = sb.run(&["-c", "--json", "account", "list"]);
    assert_eq!(code(&out), 0, "stderr={}", stderr_of(&out));
    let all = format!("{}{}", stdout_of(&out), stderr_of(&out));
    for secret in [
        "ACCESS-TOKEN-DO-NOT-LEAK",
        "REFRESH-TOKEN-DO-NOT-LEAK",
        "SESSION-KEY-DO-NOT-LEAK",
        "SESSION-SECRET-DO-NOT-LEAK",
    ] {
        assert!(!all.contains(secret), "输出里泄露了凭据：{secret}");
    }
    let v = json_of(&out);
    assert_eq!(
        v["data"]["accounts"][0]["username"],
        serde_json::json!("13800138000")
    );
    assert_eq!(
        v["data"]["accounts"][0]["has_token"],
        serde_json::json!(true)
    );
}

#[test]
fn explicit_config_dir_is_created_and_used() {
    let sb = Sandbox::new("cfgdir");
    let custom = sb.root.join("custom-config");
    let out = sb.run_raw(&[
        "-c",
        "--json",
        "--config-dir",
        custom.to_str().unwrap(),
        "status",
    ]);
    assert_eq!(code(&out), 0, "stderr={}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(
        Path::new(v["data"]["config_dir"].as_str().unwrap()),
        custom.as_path()
    );
    assert!(custom.join("config.json").exists(), "配置必须落在指定目录");

    // 反证：不带 --config-dir、只设 XDG_CONFIG_HOME / APPDATA 时，Windows 上仍然会
    // 读到真实配置目录——所以沙箱一律靠 --config-dir，而不是靠环境变量。
    let out = sb.run_raw(&["-c", "--json", "status"]);
    assert_eq!(code(&out), 0);
    let v = json_of(&out);
    let reported = v["data"]["config_dir"].as_str().unwrap_or_default();
    assert!(
        !reported.is_empty(),
        "无论走哪条路径，都必须报出一个明确的配置目录"
    );
}

#[test]
fn ndjson_mode_pairs_each_request_with_one_response() {
    let sb = Sandbox::new("ndjson");
    let out = sb.run_with_stdin(
        &["-c", "--jsonl"],
        Some("{\"id\":1,\"command\":\"status\"}\n{\"id\":2,\"command\":\"account list\"}\n"),
    );
    assert_eq!(code(&out), 0, "stderr={}", stderr_of(&out));
    let lines: Vec<&str> = stdout_of(&out)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert_eq!(lines.len(), 2, "两条请求必须得到两条响应：{lines:?}");
    for (line, want_id) in lines.iter().zip([1, 2]) {
        let v: serde_json::Value = serde_json::from_str(line).expect("每行必须是 JSON");
        assert_eq!(v["id"], serde_json::json!(want_id));
        assert_eq!(v["success"], serde_json::json!(true));
    }
}
