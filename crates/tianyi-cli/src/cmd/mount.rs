//! 挂载命令：mount / unmount / mounts
//!
//! 三件事必须**真的**做到，否则这个子命令就只是一层骗人的参数解析：
//!
//! 1. **真的挂载**：mount 走 tianyi_mount::mount::{prepare, mount}，然后前台阻塞；
//! 2. **跨进程卸载**：挂载进程是前台阻塞的，别的进程只能通过配置文件与它通信——
//!    注册表 <配置目录>/mounts.json 记录「谁挂在哪」，控制文件
//!    <配置目录>/mounts/<key>.stop 表示「请卸载」，挂载进程有一个 300ms 轮询的监视线程；
//! 3. **绝不留孤儿**：正常退出 / Ctrl+C / 父进程消失 / 甚至 --timeout 把 future 丢掉，
//!    都会走到同一段收尾：卸载 + 删注册表项 + 删控制文件与状态快照。
//!
//! 关于「读写统计」的老实话：统计是**挂载进程内存里的**计数器，另一个进程读不到。
//! 因此挂载进程每约 2 秒把一份 [MountStatus] 快照写到 mounts/<key>.status.json，
//! mounts 子命令读这份快照——所以它显示的是「最多 2 秒前」的真实数字，而不是现编的 0。
//! 进程已经消失时快照会被标记为未挂载并在下一次 mounts 时清掉。
//!
//! 日志纪律：挂载层的错误消息可能带直链，进日志与进用户文案之前一律先过
//! [tianyi_mount::stats::scrub]（丢掉查询串并截断），因此不会有 token / cookie 泄露。

use crate::args::{MountArgs, UnmountArgs};
use crate::cmd::Runner;
use crate::error::{CliError, CliResult, ErrorCode};
use crate::output::{Output, Printer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tianyi_core::file::{format_size, format_speed};
use tianyi_core::ConfigStore;
use tianyi_mount::mount::{
    self as mnt, backend_available, parse_mount_target, suggested_mount_point, MountHandle,
    MountOptions, MountStatus, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE,
};
use tianyi_mount::stats::scrub;

/// 监视线程的轮询周期（控制文件 / 父进程号）
const WATCH_TICK: Duration = Duration::from_millis(300);
/// 一个检查周期切成几片：每片先看一眼停止标志，上层收尾时不必等满整周期
const STOP_SLICES: u32 = 3;
/// 每几个周期写一次状态快照（约 2 秒）
const SNAPSHOT_EVERY: u32 = 7;
/// 注册表文件名
const REGISTRY_FILE: &str = "mounts.json";
/// 控制文件与快照所在的子目录
const MOUNTS_DIR: &str = "mounts";
/// 抢注册表锁的最长等待
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);
/// 锁文件超过这个时长就认为是崩溃残留
const LOCK_STALE: Duration = Duration::from_secs(10);

// ======================================================================
// 注册表
// ======================================================================

/// 注册表里的一条记录
///
/// 字段名是对外契约（脚本与 mounts/unmount 都按它工作），只能增不能改。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    /// 挂载点（由挂载实现回读的真实值，例如 Z: 或 /home/u/TianyiCloud）
    pub mount_point: String,
    /// UNC 路径
    pub unc: String,
    /// 云空间
    pub space: String,
    /// 账号
    pub account: String,
    /// 挂载进程的 PID
    pub pid: u32,
    /// 挂载开始时间（Unix 秒）
    pub started_at_unix: u64,
}

fn registry_path(root: &Path) -> PathBuf {
    root.join(REGISTRY_FILE)
}

fn mounts_dir(root: &Path) -> PathBuf {
    root.join(MOUNTS_DIR)
}

fn control_path(root: &Path, mount_point: &str) -> PathBuf {
    mounts_dir(root).join(format!("{}.stop", mount_key(mount_point)))
}

fn snapshot_path(root: &Path, mount_point: &str) -> PathBuf {
    mounts_dir(root).join(format!("{}.status.json", mount_key(mount_point)))
}

/// 把挂载点变成安全的文件名：`Z:` → `Z-<hash>`，`/home/u/TianyiCloud` → `home_u_TianyiCloud-<hash>`
///
/// 末尾带上原串的 FNV-1a 短哈希：不同挂载点消毒后撞名（例如 /a-b 与 /a_b）时
/// 不会互相踩控制文件——控制文件踩了就会挂错卸载目标。
fn mount_key(mount_point: &str) -> String {
    let mut name = String::with_capacity(mount_point.len());
    for c in mount_point.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if c == ':' {
            name.push('-');
        } else {
            name.push('_');
        }
    }
    let name: String = name.trim_matches('_').chars().take(48).collect();
    let mut hash: u32 = 0x811c_9dc5;
    for b in mount_point.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let stem = if name.is_empty() { "_" } else { name.as_str() };
    format!("{stem}-{hash:08x}")
}

/// 注册表的跨进程写锁：create_new 的锁文件，Drop 时删除
struct RegistryLock {
    path: PathBuf,
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_registry(root: &Path) -> CliResult<RegistryLock> {
    std::fs::create_dir_all(root)?;
    let path = root.join(format!("{REGISTRY_FILE}.lock"));
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(RegistryLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 上一次持锁的进程被 kill 掉会留下锁文件：超过 10 秒就抢过来
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|at| {
                        SystemTime::now()
                            .duration_since(at)
                            .map(|d| d > LOCK_STALE)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if stale {
                    log::warn!("挂载注册表的锁文件已陈旧（{}），回收", path.display());
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(CliError::new(
                        ErrorCode::LocalIo,
                        format!(
                            "挂载注册表被另一个进程锁住了（{}），请稍后重试",
                            path.display()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// 读注册表：文件不存在 = 空表；文件损坏也不把功能整体打死（记日志，按空表继续）
pub fn read_registry(root: &Path) -> CliResult<Vec<MountRecord>> {
    let path = registry_path(root);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(CliError::new(
                ErrorCode::LocalIo,
                format!("读取挂载注册表 {} 失败：{e}", path.display()),
            ))
        }
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Vec<MountRecord>>(&raw) {
        Ok(list) => Ok(list),
        Err(e) => {
            log::warn!("挂载注册表 {} 解析失败（{e}），按空表处理", path.display());
            Ok(Vec::new())
        }
    }
}

fn write_registry(root: &Path, list: &[MountRecord]) -> CliResult<()> {
    let path = registry_path(root);
    let tmp = root.join(format!("{REGISTRY_FILE}.tmp{}", std::process::id()));
    let body = serde_json::to_vec_pretty(list)
        .map_err(|e| CliError::new(ErrorCode::Internal, format!("序列化挂载注册表失败：{e}")))?;
    std::fs::write(&tmp, body)?;
    // 原子替换：读到的永远是完整 JSON，不会撞上写了一半的注册表
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        CliError::from(e)
    })?;
    Ok(())
}

/// 读-改-写注册表（带跨进程锁与原子替换）
fn with_registry<R>(root: &Path, f: impl FnOnce(&mut Vec<MountRecord>) -> R) -> CliResult<R> {
    let _lock = lock_registry(root)?;
    let mut list = read_registry(root)?;
    let out = f(&mut list);
    write_registry(root, &list)?;
    Ok(out)
}

// ======================================================================
// 进程存活探测
// ======================================================================

/// 目标进程是否还在
///
/// Windows 用 OpenProcess + GetExitCodeProcess，Linux 用 /proc/<pid>。
/// 故意不用 tasklist：一次卸载要探测几十次，起子进程既慢又依赖输出格式。
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // 打不开不一定是死了：拒绝访问说明进程还在（只是权限不够）
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(any(windows, unix)))]
fn pid_alive(_pid: u32) -> bool {
    true
}

// ======================================================================
// 状态快照
// ======================================================================

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// MountStatus 序列化后再补上 pid（以及 Windows 的驱动器类型）
fn status_value(status: &MountStatus, pid: u32, drive_type: Option<u32>) -> Value {
    let mut value = serde_json::to_value(status).unwrap_or(Value::Null);
    if !value.is_object() {
        value = json!({});
    }
    if let Some(map) = value.as_object_mut() {
        map.insert("pid".to_string(), json!(pid));
        if let Some(t) = drive_type {
            map.insert("drive_type".to_string(), json!(t));
            map.insert("drive_type_name".to_string(), json!(drive_type_name(t)));
        }
    }
    value
}

fn write_snapshot(path: &Path, handle: &MountHandle, pid: u32) -> std::io::Result<()> {
    let mut value = status_value(&handle.status(), pid, None);
    if let Some(map) = value.as_object_mut() {
        map.insert("noted_at_unix".to_string(), json!(now_unix()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec(&value).unwrap_or_default();
    std::fs::write(path, body)
}

fn read_snapshot(root: &Path, mount_point: &str) -> Option<MountStatus> {
    let raw = std::fs::read_to_string(snapshot_path(root, mount_point)).ok()?;
    serde_json::from_str::<MountStatus>(&raw).ok()
}

fn drive_type_name(t: u32) -> &'static str {
    match t {
        // GetDriveTypeW 的返回值（0..6），只列真实出现的几个
        DRIVE_NO_ROOT_DIR => "DRIVE_NO_ROOT_DIR",
        2 => "DRIVE_REMOVABLE",
        3 => "DRIVE_FIXED",
        DRIVE_REMOTE => "DRIVE_REMOTE",
        5 => "DRIVE_CDROM",
        6 => "DRIVE_RAMDISK",
        _ => "DRIVE_UNKNOWN",
    }
}

fn yes_no(flag: bool) -> &'static str {
    if flag {
        "是"
    } else {
        "否"
    }
}

// ======================================================================
// mount
// ======================================================================

/// future 被丢弃（例如 --timeout 触发取消）时也必须让挂载停下来
///
/// 只有监视线程会读这个标志，它每 100ms 看一眼，因此丢弃到卸载之间最多 100ms。
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Watcher {
    root: PathBuf,
    mount_point: String,
    pid: u32,
    handle: Arc<MountHandle>,
    stop: Arc<AtomicBool>,
    parent_pid: Option<u32>,
    printer: Printer,
}

fn spawn_watcher(watcher: Watcher) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("tianyi-cli-mount-watch".to_string())
        .spawn(move || watch(watcher))
}

/// 监视线程：控制文件 / 父进程 / 上层停止标志，任何一条命中就卸载
fn watch(w: Watcher) {
    let mut ticks: u32 = 0;
    loop {
        // 300ms 一个检查周期，但按 100ms 切片轮询停止标志：
        // 上层收尾时不必等满一个周期才退出
        let slice = WATCH_TICK / STOP_SLICES;
        for _ in 0..STOP_SLICES {
            if w.stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(slice);
        }
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(SNAPSHOT_EVERY) {
            write_snapshot(&snapshot_path(&w.root, &w.mount_point), &w.handle, w.pid).ok();
        }
        // 已经卸载（Ctrl+C 或上层收尾）：直接退出，收尾由上层负责
        if !w.handle.status().mounted || w.stop.load(Ordering::SeqCst) {
            return;
        }
        if control_path(&w.root, &w.mount_point).exists() {
            w.printer
                .diag(&format!("收到卸载请求（{}），正在卸载…", w.mount_point));
            let _ = w.handle.unmount();
            cleanup_mounted(&w.root, &w.mount_point, w.pid);
            return;
        }
        if let Some(parent) = w.parent_pid {
            if !pid_alive(parent) {
                w.printer.diag(&format!(
                    "父进程 {parent} 已退出：自动卸载 {}",
                    w.mount_point
                ));
                let _ = w.handle.unmount();
                cleanup_mounted(&w.root, &w.mount_point, w.pid);
                return;
            }
        }
    }
}

/// 清掉这一次挂载留下的所有痕迹（幂等，正常退出与监视线程都会调用）
fn cleanup_mounted(root: &Path, mount_point: &str, pid: u32) {
    let _ = std::fs::remove_file(control_path(root, mount_point));
    let _ = std::fs::remove_file(snapshot_path(root, mount_point));
    let wanted = mount_point.to_string();
    if let Err(e) = with_registry(root, |list| {
        list.retain(|x| !(x.pid == pid && x.mount_point == wanted));
    }) {
        // 清不掉也不能崩：留在注册表里的条目下次 mounts / unmount 会按 PID 判死并回收
        log::warn!("清理挂载注册表失败（下次 mounts 会自动回收）：{e}");
    }
}

/// 把挂载层的错误翻成稳定错误码
///
/// 只做分类，不改文案语义；消息一律先 scrub（挂载层的错误可能带上直链）。
fn map_mount_error(e: tianyi_mount::Error) -> CliError {
    use tianyi_mount::Error as E;
    match e {
        // 云端错误沿用 core 的分类：没登录仍然是 AUTH_REQUIRED(3)，网络仍然是 NETWORK(4)
        E::Cloud(core) => CliError::from(core),
        E::InvalidMountPoint(m) => {
            CliError::new(ErrorCode::Usage, format!("挂载点无效：{}", scrub(&m)))
        }
        E::WinFspMissing(m) => CliError::new(
            ErrorCode::MountUnavailable,
            format!(
                "未找到 WinFsp 运行时（{}）。装好 WinFsp 之后重新挂载。",
                scrub(&m)
            ),
        ),
        E::MountPointBusy(m) => CliError::new(
            ErrorCode::MountFailed,
            format!(
                "挂载点已被占用：{}；换一个盘符（tianyi-app -c mount Y:）或用 mounts 看谁在占",
                scrub(&m)
            ),
        ),
        other => {
            let message = scrub(&other.to_string());
            // 0x80070050 = ERROR_FILE_EXISTS：这个盘符或 UNC 前缀已经被一个卷占着。
            // 机器上遗留一个崩溃前的卷时就会撞上它，光说 NTSTATUS 用户没法自救。
            if message.contains("0x80070050") {
                CliError::new(
                    ErrorCode::MountFailed,
                    format!(
                        "{message}；该盘符或 UNC 前缀已被占用（多半是上次崩溃遗留的卷）。\
                         换一个盘符，或用 --unc-prefix 指定一个唯一的 UNC 前缀后重试"
                    ),
                )
            } else {
                CliError::new(ErrorCode::MountFailed, message)
            }
        }
    }
}

#[cfg(windows)]
async fn probe_drive_type(mount_point: &str) -> u32 {
    let root = if mount_point.ends_with(':') {
        format!("{mount_point}\\")
    } else {
        mount_point.to_string()
    };
    let mut kind = tianyi_mount::mount::drive_type_of(&root);
    // WinFsp 建完卷到「网络提供程序接管盘符」之间有一小段窗口，
    // 立刻查可能读到 DRIVE_NO_ROOT_DIR；最多等 2 秒再下结论。
    for _ in 0..10 {
        if kind == DRIVE_REMOTE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        kind = tianyi_mount::mount::drive_type_of(&root);
    }
    kind
}

fn render_mounted(
    status: &MountStatus,
    mount_point: &str,
    drive_type: Option<u32>,
    pid: u32,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("已挂载：{mount_point}\n"));
    out.push_str(&format!("  UNC 路径   ：{}\n", status.unc_path));
    out.push_str(&format!("  只读       ：{}\n", yes_no(status.read_only)));
    out.push_str(&format!(
        "  零磁盘缓存 ：{}\n",
        yes_no(status.zero_disk_cache)
    ));
    out.push_str(&format!("  空间       ：{}\n", status.space));
    out.push_str(&format!("  账号       ：{}\n", status.username));
    out.push_str(&format!("  后端       ：{}\n", status.backend));
    if let Some(t) = drive_type {
        out.push_str(&format!("  驱动器类型 ：{t}（{}）\n", drive_type_name(t)));
    }
    out.push_str(&format!("  进程       ：{pid}（前台运行）\n"));
    out.push_str(&format!(
        "卸载：tianyi-app -c unmount {mount_point}（或 Ctrl+C）"
    ));
    out
}

/// mount：真的挂载，然后前台阻塞到卸载
pub async fn mount(r: &mut Runner, args: MountArgs) -> CliResult<Output> {
    let root = r.ctx.config_root().clone();
    let requested = args
        .mount_point
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(suggested_mount_point);
    // 挂载点先校验：盘符非法 / 相对路径在这里就报 USAGE(2)，不必等到建卷
    let target = parse_mount_target(&requested).map_err(map_mount_error)?;
    // 环境不可用（缺 WinFsp / 无 /dev/fuse）单独一个退出码：它不是「挂载失败」
    backend_available().map_err(|msg| CliError::new(ErrorCode::MountUnavailable, scrub(&msg)))?;

    let session = r.ctx.service.session();
    let options = MountOptions {
        space: r.ctx.service.space(),
        account: if session.username.is_empty() {
            None
        } else {
            Some(session.username.clone())
        },
        mount_point: target.display(),
        unc_prefix: args.unc_prefix.clone(),
        // 只读与零磁盘缓存是当前实现的固有属性：
        // 命令行开关（--read-only / --no-disk-cache）只是让脚本能自证意图
        read_only: true,
        zero_disk_cache: true,
        fs: Default::default(),
    };

    // 挂载会话自己开一份 ConfigStore：挂载进程可能要活很久，
    // 不能和 CLI 的交互状态互相污染（见 tianyi_mount::session 的设计说明）
    let store = ConfigStore::open_at(root.clone())?;
    let (_cloud_session, fs) = mnt::prepare(store, &options).map_err(map_mount_error)?;
    let handle = Arc::new(mnt::mount(fs, options.clone()).map_err(map_mount_error)?);

    let mount_point = handle.mount_point();
    let pid = std::process::id();
    let printer = r.ctx.printer.clone();
    let status = handle.status();
    let record = MountRecord {
        mount_point: mount_point.clone(),
        unc: status.unc_path.clone(),
        space: status.space.clone(),
        account: status.username.clone(),
        pid,
        started_at_unix: status.started_at_unix,
    };
    // 注册失败 = 别的进程卸载时找不到它 = 变成孤儿卷：立刻卸载，不留半吊子挂载
    let registered = with_registry(&root, |list| {
        list.retain(|x| x.mount_point != mount_point);
        list.push(record.clone());
    });
    if let Err(e) = registered {
        let _ = handle.unmount();
        return Err(e);
    }
    // 上一次挂载（崩溃残留）留下的控制文件必须先删掉，否则刚挂上就被自己卸载
    let control = control_path(&root, &mount_point);
    let _ = std::fs::remove_file(&control);
    write_snapshot(&snapshot_path(&root, &mount_point), &handle, pid).ok();

    #[cfg(windows)]
    let drive_type = Some(probe_drive_type(&mount_point).await);
    #[cfg(not(windows))]
    let drive_type: Option<u32> = None;

    printer.diag(&render_mounted(&status, &mount_point, drive_type, pid));
    if let Some(t) = drive_type {
        if t != DRIVE_REMOTE {
            printer.diag(&format!(
                "警告：GetDriveTypeW({mount_point}\\) 实测 {t}（{}），期望 DRIVE_REMOTE({DRIVE_REMOTE})；\
                 资源管理器里可能不会显示成网络驱动器",
                drive_type_name(t)
            ));
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let guard = StopOnDrop(Arc::clone(&stop));
    let watcher = match spawn_watcher(Watcher {
        root: root.clone(),
        mount_point: mount_point.clone(),
        pid,
        handle: Arc::clone(&handle),
        stop: Arc::clone(&stop),
        parent_pid: args.parent_pid,
        printer: printer.clone(),
    }) {
        Ok(w) => w,
        Err(e) => {
            let _ = handle.unmount();
            cleanup_mounted(&root, &mount_point, pid);
            drop(guard);
            return Err(CliError::new(
                ErrorCode::MountFailed,
                format!("启动挂载监视线程失败（已卸载）：{e}"),
            ));
        }
    };

    // 前台阻塞：Tokio 的 worker 不能被 wait() 占住，否则 Ctrl+C 处理也会跟着卡
    let hub = Arc::clone(&r.hub);
    let mut waiter = {
        let handle = Arc::clone(&handle);
        tokio::task::spawn_blocking(move || handle.wait())
    };
    let mut interrupted = false;
    loop {
        if interrupted {
            if let Err(e) = (&mut waiter).await {
                log::debug!("等待挂载线程退出失败：{e}");
            }
            break;
        }
        tokio::select! {
            res = &mut waiter => {
                if let Err(e) = res {
                    log::debug!("等待挂载线程退出失败：{e}");
                }
                break;
            }
            _ = hub.interrupted_signal() => {
                interrupted = true;
                printer.diag("收到中断：正在卸载挂载…");
                if let Err(e) = handle.unmount() {
                    printer.diag(&format!("卸载失败：{}", scrub(&e.to_string())));
                }
            }
        }
    }

    // 收尾：卸载（幂等）+ 让监视线程退出 + 清注册表与控制文件
    let unmount = handle.unmount().map_err(map_mount_error);
    drop(guard);
    let _ = watcher.join();
    cleanup_mounted(&root, &mount_point, pid);
    unmount?;

    let final_status = handle.status();
    let mut data = status_value(&final_status, pid, drive_type);
    if let Some(map) = data.as_object_mut() {
        map.insert("unmounted".to_string(), json!(true));
    }
    let human = format!(
        "挂载已结束：{mount_point}\n  本次读取 {} / {} 次请求 / {} 次失败\n",
        format_size(final_status.bytes_total),
        final_status.requests,
        final_status.errors
    );
    Ok(Output::new(data, human))
}

// ======================================================================
// unmount
// ======================================================================

/// 两个挂载点是否指同一处
///
/// Windows 盘符大小写不敏感（Z: / z:\ / Z:\ 都是同一个），Linux 绝对路径大小写敏感，
/// 因此只对「单字母盘符」做大小写归一。
fn normalize_mount(raw: &str) -> String {
    let text = raw.trim();
    let trimmed = text.strip_suffix(['\\', '/']).unwrap_or(text);
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return trimmed.to_ascii_uppercase();
    }
    trimmed.to_string()
}

fn same_mount(a: &str, b: &str) -> bool {
    normalize_mount(a) == normalize_mount(b)
}

fn not_mounted(requested: &str, root: &Path, dead_pid: Option<u32>) -> CliError {
    let registry = registry_path(root);
    let message = match dead_pid {
        Some(pid) => {
            format!("没有在挂载：{requested} 的挂载进程 {pid} 已经退出（陈旧记录已清理，无事可做）")
        }
        None => format!(
            "没有在挂载：{requested} 不在 {} 里（用 tianyi-app -c mounts 看当前挂载）",
            registry.display()
        ),
    };
    CliError::new(ErrorCode::NotMounted, message).with_details(json!({
        "mount_point": requested,
        "stale_removed": dead_pid.is_some(),
        "registry_file": registry.display().to_string(),
    }))
}

/// unmount：写控制文件，然后等目标进程真的退出
///
/// 绝不「写完文件就说成功」：目标进程没退出就是卸载失败（MOUNT_FAILED, 11），
/// 目标本来就不存在才是 NOT_MOUNTED(12)——这两件事必须能分开。
pub async fn unmount(r: &mut Runner, args: UnmountArgs) -> CliResult<Output> {
    let root = r.ctx.config_root().clone();
    let requested = args.mount_point.trim().to_string();
    if requested.is_empty() {
        return Err(CliError::usage(
            "unmount 需要一个挂载点，例如 tianyi-app -c unmount Z:",
        ));
    }
    let found = read_registry(&root)?
        .into_iter()
        .find(|x| same_mount(&x.mount_point, &requested));
    let Some(record) = found else {
        return Err(not_mounted(&requested, &root, None));
    };
    if !pid_alive(record.pid) {
        // 陈旧记录：清理掉，然后明确报告「本来就没在挂载」
        cleanup_mounted(&root, &record.mount_point, record.pid);
        return Err(not_mounted(&requested, &root, Some(record.pid)));
    }

    let control = control_path(&root, &record.mount_point);
    if let Some(dir) = control.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &control,
        format!(
            "mount_point={}\nmounted_pid={}\nrequested_by_pid={}\nrequested_at_unix={}\n",
            record.mount_point,
            record.pid,
            std::process::id(),
            now_unix()
        ),
    )?;

    let wait = Duration::from_secs(args.wait.max(1));
    let began = Instant::now();
    while pid_alive(record.pid) {
        if began.elapsed() >= wait {
            return Err(CliError::new(
                ErrorCode::MountFailed,
                format!(
                    "卸载超时（{} 秒）：{} 的挂载进程 {} 仍在运行。控制文件已留下，\
                     该进程一旦响应就会卸载；也可以直接结束它（进程退出会释放挂载）",
                    wait.as_secs(),
                    record.mount_point,
                    record.pid
                ),
            )
            .with_details(json!({
                "mount_point": record.mount_point,
                "pid": record.pid,
                "control_file": control.display().to_string(),
            })));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // 目标进程已经退出：把它没来得及清的那部分也补上（幂等）
    cleanup_mounted(&root, &record.mount_point, record.pid);
    let waited_ms = began.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    r.ctx.printer.diag(&format!(
        "{} 已卸载（挂载进程 {} 退出，等待 {waited_ms} 毫秒）",
        record.mount_point, record.pid
    ));
    let data = json!({
        "unmounted": true,
        "mount_point": record.mount_point,
        "unc_path": record.unc,
        "space": record.space,
        "account": record.account,
        "pid": record.pid,
        "stale": false,
        "waited_ms": waited_ms,
    });
    let human = format!(
        "已卸载 {}（挂载进程 {}，等待 {waited_ms} 毫秒）\n",
        record.mount_point, record.pid
    );
    Ok(Output::new(data, human))
}

// ======================================================================
// mounts
// ======================================================================

fn synthetic_status(record: &MountRecord, alive: bool) -> MountStatus {
    MountStatus {
        mounted: alive,
        mount_point: record.mount_point.clone(),
        unc_path: record.unc.clone(),
        label: String::new(),
        space: record.space.clone(),
        username: record.account.clone(),
        read_only: true,
        zero_disk_cache: true,
        backend: String::new(),
        started_at_unix: record.started_at_unix,
        bytes_total: 0,
        requests: 0,
        errors: 0,
        bytes_per_sec: 0.0,
        cached_dirs: 0,
        memory_budget: 0,
        last_error: None,
    }
}

fn render_mounts(items: &[Value]) -> String {
    if items.is_empty() {
        return "当前没有挂载\n".to_string();
    }
    let mut out = String::new();
    for item in items {
        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string()
        };
        let number = |key: &str| item.get(key).and_then(Value::as_u64).unwrap_or(0);
        let flag = |key: &str| yes_no(item.get(key).and_then(Value::as_bool).unwrap_or(false));
        let alive = item.get("alive").and_then(Value::as_bool).unwrap_or(false);
        out.push_str(&format!(
            "{}  pid {}  {}\n",
            text("mount_point"),
            number("pid"),
            if alive {
                "运行中"
            } else {
                "进程已退出"
            }
        ));
        out.push_str(&format!(
            "    空间 {} / 账号 {}  只读 {}  零磁盘缓存 {}  后端 {}\n",
            text("space"),
            text("username"),
            flag("read_only"),
            flag("zero_disk_cache"),
            text("backend")
        ));
        out.push_str(&format!(
            "    读 {} / {} 次请求 / {} 次失败  最近速度 {}/s  目录缓存 {}\n",
            format_size(number("bytes_total")),
            number("requests"),
            number("errors"),
            format_speed(
                item.get("bytes_per_sec")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0) as u64
            ),
            number("cached_dirs")
        ));
    }
    out
}

/// mounts：列出注册表 + 每个挂载的最新状态快照
///
/// 陈旧记录（进程已经不在了）在这里顺手清掉，并且明确写进输出，
/// 而不是让用户对着一张「显示在挂但其实是死的」的表发呆。
pub fn mounts(r: &Runner) -> CliResult<Output> {
    let root = r.ctx.config_root().clone();
    let records = read_registry(&root)?;
    let mut items: Vec<Value> = Vec::with_capacity(records.len());
    let mut stale: Vec<String> = Vec::new();
    for record in &records {
        let alive = pid_alive(record.pid);
        let mut status = read_snapshot(&root, &record.mount_point)
            .unwrap_or_else(|| synthetic_status(record, alive));
        if !alive {
            // 进程都没了还声称「已挂载」就是骗人
            status.mounted = false;
        }
        let mut value = status_value(&status, record.pid, None);
        if let Some(map) = value.as_object_mut() {
            map.insert("alive".to_string(), json!(alive));
        }
        items.push(value);
        if !alive {
            stale.push(record.mount_point.clone());
        }
    }
    if !stale.is_empty() {
        for mount_point in &stale {
            cleanup_mounted(&root, mount_point, 0);
        }
        if let Err(e) = with_registry(&root, |list| list.retain(|x| pid_alive(x.pid))) {
            log::warn!("清理陈旧挂载记录失败：{e}");
        }
    }
    let count = items.len();
    let (backend_ok, backend_error) = match backend_available() {
        Ok(()) => (true, Value::Null),
        Err(message) => (false, json!(scrub(&message))),
    };
    let human = render_mounts(&items);
    let data = json!({
        "mounts": items,
        "count": count,
        "stale_removed": stale,
        "registry_file": registry_path(&root).display().to_string(),
        "backend_available": backend_ok,
        "backend_error": backend_error,
    });
    Ok(Output::new(data, human))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::interrupt::InterruptHub;

    fn runner() -> Runner {
        Runner::new(Context::for_test(false, true), InterruptHub::new())
    }

    fn record(mount_point: &str, pid: u32) -> MountRecord {
        MountRecord {
            mount_point: mount_point.to_string(),
            unc: "\\\\TianyiCloud-tester\\Personal".to_string(),
            space: "personal".to_string(),
            account: "tester".to_string(),
            pid,
            started_at_unix: 1_700_000_000,
        }
    }

    #[test]
    fn mount_key_is_file_name_safe_and_collision_resistant() {
        let drive = mount_key("Z:");
        assert!(
            drive
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "控制文件名必须安全：{drive}"
        );
        assert_ne!(mount_key("Z:"), mount_key("Y:"));
        // 消毒后撞名的两个路径必须仍然不同（否则会卸载错目标）
        assert_ne!(mount_key("/a-b"), mount_key("/a_b"));
        assert_ne!(mount_key("/a/b"), mount_key("/a_b"));
        // 长度有界：Windows 路径长度限制
        assert!(mount_key(&"/x".repeat(200)).len() < 80);
    }

    #[test]
    fn same_mount_normalizes_drive_case_but_keeps_unix_case() {
        assert!(same_mount("Z:", "z:\\"));
        assert!(same_mount("Z:/", "z:"));
        assert!(same_mount("/home/u/TianyiCloud/", "/home/u/TianyiCloud"));
        assert!(
            !same_mount("/home/u/TC", "/home/u/tc"),
            "Linux 路径大小写敏感"
        );
    }

    #[test]
    fn registry_round_trips_and_survives_a_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("tianyi-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_registry(&dir).unwrap().is_empty(), "缺少文件 = 空表");
        with_registry(&dir, |list| list.push(record("Z:", 4242))).unwrap();
        let list = read_registry(&dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].mount_point, "Z:");
        // 损坏的注册表不能被当成致命错误（否则一次崩溃就永久卡死挂载功能）
        std::fs::write(registry_path(&dir), b"{not json").unwrap();
        assert!(read_registry(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_errors_map_to_the_documented_exit_codes() {
        use tianyi_mount::Error as M;
        // 缺 WinFsp：环境不可用(10)，不是「挂载失败」
        let e = map_mount_error(M::WinFspMissing("dll not found".to_string()));
        assert_eq!(e.code, ErrorCode::MountUnavailable);
        assert_eq!(e.exit_code(), 10);
        // 挂载点非法：参数问题(2)
        let e = map_mount_error(M::InvalidMountPoint("C:".to_string()));
        assert_eq!(e.code, ErrorCode::Usage);
        assert_eq!(e.exit_code(), 2);
        // 盘符被占 / 建卷失败：挂载失败(11)
        let e = map_mount_error(M::MountPointBusy("Z:".to_string()));
        assert_eq!(e.exit_code(), 11);
        // 没登录仍然是认证(3)：不能被挂载层吞成 MOUNT_FAILED
        let e = map_mount_error(M::Cloud(tianyi_core::Error::NotAuthenticated(
            "login first".to_string(),
        )));
        assert_eq!(e.code, ErrorCode::AuthRequired);
        assert_eq!(e.exit_code(), 3);
        // 遗留卷占用盘符/UNC 前缀：给出可自救的提示，而不是只丢一个 NTSTATUS
        let e = map_mount_error(M::WinFsp(
            "WinFsp 创建文件系统失败：NTSTATUS 0x80070050（盘符 T:）".to_string(),
        ));
        assert_eq!(e.exit_code(), 11);
        assert!(e.message.contains("--unc-prefix"), "{}", e.message);
        // 直链不许进错误文案：scrub 必须把查询串丢掉
        let e = map_mount_error(M::WinFsp(
            "http 403 https://cdn.example.com/a.bin?sign=SECRET".to_string(),
        ));
        assert!(!e.message.contains("SECRET"), "{}", e.message);
    }

    #[tokio::test]
    async fn unmount_without_any_mount_reports_not_mounted() {
        let mut r = runner();
        let err = unmount(
            &mut r,
            UnmountArgs {
                mount_point: "Z:".to_string(),
                wait: 1,
            },
        )
        .await
        .expect_err("没挂载必须报错，不能假装成功");
        assert_eq!(err.code, ErrorCode::NotMounted);
        assert_eq!(err.exit_code(), 12, "「本来就没挂」是独立的退出码");
        assert!(err.message.contains("没有在挂载"), "{}", err.message);
        let details = err.details.expect("要带 details");
        assert_eq!(details["stale_removed"], json!(false));
    }

    #[tokio::test]
    async fn unmount_cleans_stale_record_instead_of_claiming_success() {
        let mut r = runner();
        let root = r.ctx.config_root().clone();
        // 一个几乎不可能存在的 PID：记录在案但进程早就没了
        let dead = if cfg!(windows) { 999_999 } else { 4_000_000 };
        with_registry(&root, |list| list.push(record("Z:", dead))).unwrap();
        let err = unmount(
            &mut r,
            UnmountArgs {
                mount_point: "z:".to_string(),
                wait: 1,
            },
        )
        .await
        .expect_err("陈旧记录也要报「没有在挂载」");
        assert_eq!(err.code, ErrorCode::NotMounted);
        assert_eq!(err.exit_code(), 12);
        let details = err.details.expect("要带 details");
        assert_eq!(details["stale_removed"], json!(true));
        assert!(
            read_registry(&root).unwrap().is_empty(),
            "陈旧记录必须被清理"
        );
    }

    #[test]
    fn mounts_json_has_the_documented_shape() {
        let r = runner();
        let out = mounts(&r).expect("空注册表也要能列");
        for key in [
            "mounts",
            "count",
            "stale_removed",
            "registry_file",
            "backend_available",
            "backend_error",
        ] {
            assert!(out.data.get(key).is_some(), "缺少字段 {key}");
        }
        assert_eq!(out.data["count"], json!(0));
        assert_eq!(out.data["mounts"], json!([]));
        assert!(out.human.contains("没有挂载"));
    }

    #[test]
    fn mounts_item_is_a_mount_status_plus_pid_and_alive() {
        let r = runner();
        let root = r.ctx.config_root().clone();
        // 用当前进程的 PID 伪造一条「活着」的记录 + 一份真实形状的快照
        let me = std::process::id();
        with_registry(&root, |list| list.push(record("Z:", me))).unwrap();
        let mut status = synthetic_status(&record("Z:", me), true);
        status.backend = "winfsp".to_string();
        status.bytes_total = 4096;
        status.requests = 2;
        status.cached_dirs = 3;
        let mut snapshot = status_value(&status, me, None);
        snapshot
            .as_object_mut()
            .unwrap()
            .insert("noted_at_unix".to_string(), json!(1));
        std::fs::create_dir_all(mounts_dir(&root)).unwrap();
        std::fs::write(
            snapshot_path(&root, "Z:"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        let out = mounts(&r).expect("必须能列");
        let item = &out.data["mounts"][0];
        for key in [
            "mounted",
            "mount_point",
            "unc_path",
            "label",
            "space",
            "username",
            "read_only",
            "zero_disk_cache",
            "backend",
            "started_at_unix",
            "bytes_total",
            "requests",
            "errors",
            "bytes_per_sec",
            "cached_dirs",
            "memory_budget",
            "last_error",
            "pid",
            "alive",
        ] {
            assert!(item.get(key).is_some(), "MountStatus 缺字段 {key}");
        }
        assert_eq!(item["pid"], json!(me));
        assert_eq!(item["alive"], json!(true));
        assert_eq!(item["bytes_total"], json!(4096), "读写统计要来自快照");
        assert_eq!(item["requests"], json!(2));
        assert_eq!(item["cached_dirs"], json!(3));
        assert_eq!(item["backend"], json!("winfsp"));
        assert_eq!(out.data["count"], json!(1));
    }

    #[test]
    fn mounts_reports_dead_processes_as_stale_and_cleans_them() {
        let r = runner();
        let root = r.ctx.config_root().clone();
        let dead = if cfg!(windows) { 999_999 } else { 4_000_000 };
        with_registry(&root, |list| list.push(record("Z:", dead))).unwrap();
        let out = mounts(&r).expect("必须能列");
        assert_eq!(out.data["stale_removed"], json!(["Z:"]));
        assert_eq!(out.data["mounts"][0]["alive"], json!(false));
        assert_eq!(
            out.data["mounts"][0]["mounted"],
            json!(false),
            "进程没了就不能再声称已挂载"
        );
        assert!(read_registry(&root).unwrap().is_empty(), "陈旧记录要被清掉");
    }
}
