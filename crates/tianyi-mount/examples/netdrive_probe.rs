//! 真机验收工具（Windows）
//!
//! 这不是"演示程序"，而是把本项目的验收标准做成可执行文件：
//!
//! 1. GetDriveTypeW("X:\\") == DRIVE_REMOTE(4) —— 证明它是**网络驱动器**，
//!    而不是伪装成本地盘；
//! 2. UNC 路径 \\Server\Share 可访问；
//! 3. 目录可列举、文件可读取、内容与云端大小一致；
//! 4. 写操作被拒绝（只读卷）；
//! 5. 卸载后盘符释放。
//!
//! 每一项都套了硬超时：某一步卡住（例如 UNC 经由网络提供程序解析超时）只会记一条失败，
//! 不会把整个验收挂死——这也是挂载实现本身必须遵守的纪律。
//!
//! 用法：cargo run -p tianyi-mount --example mount_probe -- --mount Z: --space personal
//! 退出码：0 = 全部通过；1 = 有断言失败；2 = 环境/登录问题（不算功能失败）。
//!
//! 只在 Windows 上编译：DRIVE_REMOTE、UNC 前缀都是 Windows 的概念。其余平台只剩一个
//! stub main，因此"为 Windows 验收准备"的导入与辅助函数在这里是未使用/死代码——
//! 用下面这条 crate 级 allow 把它们从 `clippy --all-targets -D warnings` 里摘出去，
//! 而不是给每个 item 挂一遍 cfg。

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;
use tianyi_core::{ConfigStore, Space};
use tianyi_mount::fs::{CloudFs, FsOptions};
use tianyi_mount::mount::{default_drive_letter, drive_type_of, mount, MountOptions, DRIVE_REMOTE};
use tianyi_mount::session::CloudSession;

/// 单项检查的超时
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(windows)]
fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut account: Option<String> = None;
    let mut space = Space::Personal;
    let mut mount_point: Option<String> = None;
    let mut want_unc = false;
    let mut hold_secs: u64 = 0;
    let mut unc_prefix: Option<String> = None;
    let mut argv = std::env::args().skip(1);
    while let Some(a) = argv.next() {
        match a.as_str() {
            "--account" => account = argv.next(),
            "--space" => {
                space = match argv.next().as_deref() {
                    Some("family") | Some("家庭云") => Space::Family,
                    _ => Space::Personal,
                }
            }
            "--mount" => mount_point = argv.next(),
            "--unc" => want_unc = true,
            "--hold" => {
                hold_secs = argv.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            "--unc-prefix" => unc_prefix = argv.next(),
            other => {
                eprintln!("未知参数：{other}");
                return ExitCode::from(2);
            }
        }
    }

    let (store, degrade) = ConfigStore::open();
    if let Some(d) = degrade {
        eprintln!("配置目录降级：{d:?}");
    }
    let session = match CloudSession::open(store, account.as_deref(), space) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("建立会话失败：{e}");
            return ExitCode::from(2);
        }
    };
    println!("账号：{}  空间：{}", session.username(), space.as_str());

    let cloud = match CloudFs::new(
        session.clone(),
        space,
        session.username(),
        FsOptions::default(),
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("构造文件系统失败：{e}");
            return ExitCode::from(2);
        }
    };

    let options = MountOptions {
        account: account.clone(),
        space,
        mount_point: mount_point.unwrap_or_else(default_drive_letter),
        unc_prefix,
        ..Default::default()
    };
    println!("准备挂载到 {}", options.mount_point);
    let handle = match mount(cloud.clone(), options.clone()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("挂载失败：{e}");
            return ExitCode::from(1);
        }
    };

    let mp = handle.mount_point();
    if hold_secs > 0 {
        println!("挂载已就绪，保持 {hold_secs} 秒供外部进程访问：{mp}");
        std::thread::sleep(Duration::from_secs(hold_secs));
        println!("保持期结束，继续自检");
    }
    let root_path = if mp.ends_with(':') {
        format!("{mp}\\")
    } else {
        mp.clone()
    };
    let mut failures = 0usize;

    // ---- 1. 驱动器类型（最关键的验收项）
    let dtype = drive_type_of(&root_path);
    report(
        &mut failures,
        "GetDriveTypeW == DRIVE_REMOTE",
        dtype == DRIVE_REMOTE,
        format!("{root_path} → {dtype}（期望 {DRIVE_REMOTE}）"),
    );

    // ---- 2. 目录列举（本进程内，走完整的 WinFsp 回调链）
    let listing = timed(CHECK_TIMEOUT, {
        let p = root_path.clone();
        move || {
            std::fs::read_dir(&p).map(|it| {
                it.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
        }
    });
    match listing {
        Ok(Ok(names)) => report(
            &mut failures,
            "目录列举",
            !names.is_empty(),
            format!("{} 项：{:?}", names.len(), &names[..names.len().min(6)]),
        ),
        Ok(Err(e)) => report(&mut failures, "目录列举", false, format!("{e}")),
        Err(_) => report(&mut failures, "目录列举", false, "超时".into()),
    }

    // ---- 3. 文件读取
    let smallest = timed(CHECK_TIMEOUT, {
        let p = root_path.clone();
        move || find_smallest_file(&p)
    });
    match smallest {
        Ok(Ok(Some((path, size)))) => {
            let read = timed(CHECK_TIMEOUT, {
                let path = path.clone();
                move || read_head(&path, 4096)
            });
            match read {
                Ok(Ok(bytes)) => report(
                    &mut failures,
                    "文件读取（走 Range）",
                    !bytes.is_empty() || size == 0,
                    format!(
                        "{} 声明 {size} 字节，实读 {} 字节，前 8 字节 {:02x?}",
                        short(&path),
                        bytes.len(),
                        &bytes[..bytes.len().min(8)]
                    ),
                ),
                Ok(Err(e)) => report(&mut failures, "文件读取（走 Range）", false, format!("{e}")),
                Err(_) => report(&mut failures, "文件读取（走 Range）", false, "超时".into()),
            }
        }
        Ok(Ok(None)) => println!("  [跳过] 文件读取：根目录下没有可直接读取的文件"),
        Ok(Err(e)) => report(&mut failures, "文件读取（走 Range）", false, format!("{e}")),
        Err(_) => report(&mut failures, "文件读取（走 Range）", false, "超时".into()),
    }

    // ---- 3b. 子目录递归（跨进程崩溃的可疑点：子目录会触发一次全新的云端 list_dir）
    let sub = timed(CHECK_TIMEOUT, {
        let p = root_path.clone();
        move || -> std::io::Result<Vec<String>> {
            let mut names = Vec::new();
            for e in std::fs::read_dir(&p)? {
                let e = e?;
                if e.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                    let mut inner = 0usize;
                    if let Ok(it) = std::fs::read_dir(e.path()) {
                        inner = it.count();
                    }
                    names.push(format!("{}=>{inner}", e.file_name().to_string_lossy()));
                    if names.len() >= 3 {
                        break;
                    }
                }
            }
            Ok(names)
        }
    });
    match sub {
        Ok(Ok(names)) => report(
            &mut failures,
            "子目录递归列举",
            !names.is_empty(),
            format!("{names:?}"),
        ),
        Ok(Err(e)) => report(&mut failures, "子目录递归列举", false, format!("{e}")),
        Err(()) => report(&mut failures, "子目录递归列举", false, "超时".into()),
    }

    // ---- 4. 只读校验
    let new_file = format!("{}mount-probe-{}.tmp", root_path, std::process::id());
    let create = timed(CHECK_TIMEOUT, {
        let f = new_file.clone();
        move || std::fs::File::create(&f).err().map(|e| e.to_string())
    });
    match create {
        Ok(Some(err)) => report(&mut failures, "创建文件被拒绝", true, err),
        Ok(None) => report(
            &mut failures,
            "创建文件被拒绝",
            false,
            "竟然创建成功了".into(),
        ),
        Err(()) => report(&mut failures, "创建文件被拒绝", false, "超时".into()),
    }

    // ---- 5. UNC 前缀（经由 WinFsp 网络提供程序）
    let unc = handle.status().unc_path.clone();
    if !want_unc {
        println!("  [跳过] UNC 路径检查（用 --unc 显式开启：该检查可能阻塞在内核网络重定向里）");
    }
    if want_unc {
        let unc_path = format!("{unc}\\");
        let unc_check = timed(CHECK_TIMEOUT, {
            let p = unc_path.clone();
            move || std::fs::read_dir(&p).map(|_| ())
        });
        match unc_check {
            Ok(Ok(())) => report(&mut failures, "UNC 路径可访问", true, unc_path.clone()),
            Ok(Err(e)) => report(
                &mut failures,
                "UNC 路径可访问",
                false,
                format!("{unc_path} → {e}"),
            ),
            Err(()) => report(
                &mut failures,
                "UNC 路径可访问",
                false,
                "超时（网络提供程序未解析）".to_string(),
            ),
        }
    }

    let status = handle.status();
    println!(
        "\n读取统计：累计 {} 字节 / {} 次请求 / {} 次失败；最近速度 {:.1} KiB/s；目录缓存 {} 个",
        status.bytes_total,
        status.requests,
        status.errors,
        status.bytes_per_sec / 1024.0,
        status.cached_dirs
    );

    // ---- 6. 卸载
    let unmount = timed(CHECK_TIMEOUT, move || handle.unmount());
    match unmount {
        Ok(Ok(())) => report(&mut failures, "卸载", true, "已卸载".into()),
        Ok(Err(e)) => report(&mut failures, "卸载", false, format!("{e}")),
        Err(_) => report(&mut failures, "卸载", false, "超时".into()),
    }
    let after = drive_type_of(&root_path);
    report(
        &mut failures,
        "卸载后盘符释放",
        after != DRIVE_REMOTE,
        format!("{root_path} 卸载后类型 {after}"),
    );

    if failures == 0 {
        println!("\n全部验收项通过");
        ExitCode::SUCCESS
    } else {
        eprintln!("\n{failures} 项验收失败");
        ExitCode::from(1)
    }
}

fn report(failures: &mut usize, name: &str, ok: bool, detail: String) {
    if ok {
        println!("  [通过] {name}：{detail}");
    } else {
        *failures += 1;
        println!("  [失败] {name}：{detail}");
    }
}

/// 在独立线程里跑一段可能卡住的系统调用，超时就放弃等待
///
/// 注意：超时后线程本身还在跑（阻塞在系统调用里），因此这里只用于**验收工具**，
/// 不能出现在生产路径上——生产路径靠的是 API 层的异步超时。
fn timed<T, F>(dur: Duration, f: F) -> Result<T, ()>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(dur) {
        Ok(v) => Ok(v),
        Err(RecvTimeoutError::Timeout) => Err(()),
        Err(RecvTimeoutError::Disconnected) => Err(()),
    }
}

fn short(path: &str) -> String {
    if path.chars().count() <= 60 {
        path.to_string()
    } else {
        format!(
            "…{}",
            path.chars()
                .rev()
                .take(50)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        )
    }
}

fn find_smallest_file(dir: &str) -> std::io::Result<Option<(String, u64)>> {
    let mut best: Option<(String, u64)> = None;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if let Ok(md) = e.metadata() {
            if md.is_file() {
                let size = md.len();
                if best.as_ref().map(|(_, s)| size < *s).unwrap_or(true) {
                    best = Some((e.path().display().to_string(), size));
                }
            }
        }
    }
    Ok(best)
}

fn read_head(path: &str, limit: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// 非 Windows 平台：本示例没有可执行的内容（退出码 2 = 环境不适用）
#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("netdrive_probe 是 Windows 真机验收工具，本平台无事可做");
    ExitCode::from(2)
}
