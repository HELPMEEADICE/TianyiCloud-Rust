//! 挂载点、挂载生命周期与对外状态
//!
//! 本模块是 GUI 与 CLI 的**唯一入口**：两个前端都只跟 [MountHandle] 打交道，
//! 平台细节（WinFsp / FUSE）被藏在 cfg 后面。

use crate::error::{Error, Result};
use crate::fs::CloudFs;
use crate::paths;
use crate::stats::StatsSnapshot;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tianyi_core::Space;

/// Windows 上 `GetDriveTypeW` 的返回值：网络驱动器
///
/// 这是本项目 Windows 侧最重要的一条验收标准：挂载必须以这个类型出现，
/// 而不是 DRIVE_FIXED(3)。
pub const DRIVE_REMOTE: u32 = 4;
/// `GetDriveTypeW`：本地固定盘
pub const DRIVE_FIXED: u32 = 3;
/// `GetDriveTypeW`：路径不可用
pub const DRIVE_NO_ROOT_DIR: u32 = 1;

/// 挂载目标
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountTarget {
    /// Windows 盘符，例如 X:
    Drive(u8),
    /// Linux 目录
    Directory(PathBuf),
}

impl MountTarget {
    /// 呈现给用户/操作系统的挂载点字符串
    pub fn display(&self) -> String {
        match self {
            MountTarget::Drive(letter) => format!("{}:", *letter as char),
            MountTarget::Directory(p) => p.display().to_string(),
        }
    }

    /// Windows 盘符形式（非 Windows 目标返回 None）
    pub fn drive_letter(&self) -> Option<u8> {
        match self {
            MountTarget::Drive(l) => Some(*l),
            MountTarget::Directory(_) => None,
        }
    }
}

/// 解析并校验挂载点
///
/// Windows 只接受盘符：WinFsp 的网络文件系统**不能**挂在目录上（Windows 对
/// junction 指向网络设备的限制），强行允许只会在资源管理器里变成一个打不开的目录。
pub fn parse_mount_target(raw: &str) -> Result<MountTarget> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::InvalidMountPoint("挂载点不能为空".into()));
    }

    if cfg!(windows) {
        let bytes = raw.as_bytes();
        let letter = bytes[0].to_ascii_uppercase();
        let looks_like_drive = raw.len() == 2 && bytes[1] == b':' && letter.is_ascii_uppercase();
        let looks_like_drive_path = raw.len() >= 3
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
            && letter.is_ascii_uppercase();
        if !(looks_like_drive || looks_like_drive_path) {
            return Err(Error::InvalidMountPoint(format!(
                "Windows 上只支持盘符挂载（例如 X:），收到：{raw}"
            )));
        }
        if letter < b'D' {
            // A:/B: 是软驱保留位，C: 是系统盘，占用它们会出人命
            return Err(Error::InvalidMountPoint(format!(
                "{}: 是系统保留盘符，请换一个（建议 X:）",
                letter as char
            )));
        }
        return Ok(MountTarget::Drive(letter));
    }

    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => return Err(Error::InvalidMountPoint("无法解析用户主目录".into())),
        }
    } else if raw == "~" {
        dirs::home_dir().ok_or_else(|| Error::InvalidMountPoint("无法解析用户主目录".into()))?
    } else {
        PathBuf::from(raw)
    };
    if !expanded.is_absolute() {
        return Err(Error::InvalidMountPoint(format!(
            "挂载目录必须是绝对路径：{}",
            expanded.display()
        )));
    }
    Ok(MountTarget::Directory(expanded))
}

/// 卸载时等待后台线程的上限
pub const UNMOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 平台挂载后端是否可用（GUI 用它决定是否禁用「挂载」按钮并给出安装指引）
///
/// 返回 Err 时说明里已经是给用户看的中文提示，前端可以直接展示。
pub fn backend_available() -> std::result::Result<(), String> {
    #[cfg(windows)]
    {
        // 前端直接把这段文字展示给用户，所以必须走 [Error::WinFspMissing] 的 Display：
        // sys::api() 的原始错误只说明"哪个候选 DLL 没加载成功"，不含安装指引。
        match crate::win::sys::api() {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::WinFspMissing(e).to_string()),
        }
    }
    #[cfg(unix)]
    {
        if std::path::Path::new("/dev/fuse").exists() {
            Ok(())
        } else {
            Err("没有找到 /dev/fuse：请确认已安装 fuse3 并且当前用户在 fuse 组里".into())
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("本平台没有实现文件系统挂载".to_string())
    }
}

/// 由配置存储 + 挂载选项准备好会话与文件系统
///
/// GUI 与 CLI 都只需要两步：
///
/// ```text
/// let store = ConfigStore::open().0;
/// let options = MountOptions::default();
/// let (_session, fs) = prepare(store, &options)?;
/// let handle = mount(fs, options)?;
/// ```
pub fn prepare(
    store: tianyi_core::ConfigStore,
    options: &MountOptions,
) -> Result<(Arc<crate::session::CloudSession>, Arc<CloudFs>)> {
    let session = Arc::new(crate::session::CloudSession::open(
        store,
        options.account.as_deref(),
        options.space,
    )?);
    let fs = Arc::new(CloudFs::new(
        session.clone(),
        options.space,
        session.username(),
        options.fs.clone(),
    )?);
    Ok((session, fs))
}

/// Windows 上可用的盘符（从 X: 往下找，符合用户直觉）
#[cfg(windows)]
pub fn free_drive_letters() -> Vec<String> {
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    // 优先 X、Y、Z 这些"外挂盘"位置，再往前找
    let order: Vec<u8> = (b'X'..=b'Z').chain((b'D'..b'X').rev()).collect();
    order
        .into_iter()
        .filter(|l| mask & (1u32 << (l - b'A')) == 0)
        .map(|l| format!("{}:", l as char))
        .collect()
}

#[cfg(not(windows))]
pub fn free_drive_letters() -> Vec<String> {
    Vec::new()
}

/// 默认盘符（X: 可用就用 X:，否则按顺序找）
pub fn default_drive_letter() -> String {
    free_drive_letters()
        .into_iter()
        .next()
        .unwrap_or_else(|| "X:".to_string())
}

/// 盘符是否已经被占用（含"路径不存在但盘符被定义"的情况）
#[cfg(windows)]
pub fn drive_in_use(letter: u8) -> bool {
    let root = format!("{}:\\", letter as char);
    let wide = to_wide(&root);
    let t = unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) };
    t != DRIVE_NO_ROOT_DIR
}

#[cfg(not(windows))]
pub fn drive_in_use(_letter: u8) -> bool {
    false
}

/// 查询路径的驱动器类型（Windows 验收用）
#[cfg(windows)]
pub fn drive_type_of(path: &str) -> u32 {
    let wide = to_wide(path);
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) }
}

#[cfg(not(windows))]
pub fn drive_type_of(_path: &str) -> u32 {
    0
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 生成 UNC 前缀（`\\Server\\Share` 形式）
///
/// * 必须唯一：多账号同时挂载时不能互相顶掉，因此把清洗过的账号名放进 Server 段；
/// * 必须是清理过的：服务器名来自用户输入，不能直接拼进路径；
/// * 长度受限：WinFsp 的 Prefix 字段是 192 个 wchar（含结尾 0）。
pub fn unc_prefix(space: Space, account: &str) -> String {
    let cleaned: String = account
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            '.' | '@' => '-',
            _ => '_',
        })
        .take(40)
        .collect();
    let server = if cleaned.is_empty() {
        "TianyiCloud".to_string()
    } else {
        format!("TianyiCloud-{cleaned}")
    };
    let share = match space {
        Space::Personal => "Personal",
        Space::Family => "Family",
    };
    format!("\\{server}\\{share}")
}

/// 一次挂载的选项
#[derive(Debug, Clone)]
pub struct MountOptions {
    /// 云空间
    pub space: Space,
    /// 使用的账号（None = 配置里的 last_account）
    pub account: Option<String>,
    /// 挂载点：Windows 是 `X:`，Linux 是目录
    pub mount_point: String,
    /// 覆盖 UNC 前缀（默认按 [unc_prefix] 生成）
    pub unc_prefix: Option<String>,
    /// 只读（当前实现恒为只读，保留字段是为了 CLI/JSON 的兼容与将来的扩展）
    pub read_only: bool,
    /// 零磁盘文件缓存（当前实现恒为真；把它暴露出来是为了让 GUI 显示真实状态，
    /// 而不是写死一段文案）
    pub zero_disk_cache: bool,
    /// 文件系统层参数
    pub fs: crate::fs::FsOptions,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            space: Space::Personal,
            account: None,
            mount_point: default_drive_letter(),
            unc_prefix: None,
            read_only: true,
            zero_disk_cache: true,
            fs: crate::fs::FsOptions::default(),
        }
    }
}

impl MountOptions {
    /// 解析后的挂载目标
    pub fn target(&self) -> Result<MountTarget> {
        parse_mount_target(&self.mount_point)
    }

    /// 生效的 UNC 前缀
    pub fn effective_unc(&self, account: &str) -> String {
        self.unc_prefix
            .clone()
            .unwrap_or_else(|| unc_prefix(self.space, account))
    }
}

/// 挂载状态快照（GUI 与 CLI 共用）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MountStatus {
    pub mounted: bool,
    pub mount_point: String,
    pub unc_path: String,
    pub label: String,
    pub space: String,
    pub username: String,
    pub read_only: bool,
    pub zero_disk_cache: bool,
    pub backend: String,
    pub started_at_unix: u64,
    /// 累计读取字节
    pub bytes_total: u64,
    /// Range 请求次数
    pub requests: u64,
    /// 失败次数
    pub errors: u64,
    /// 最近 3 秒的平均速度（字节/秒）
    pub bytes_per_sec: f64,
    pub cached_dirs: usize,
    /// 读路径的内存上界（并发 × 单次长度），用于向用户证明"有界"
    pub memory_budget: u64,
    pub last_error: Option<String>,
}

/// 平台挂载实现需要提供的最小能力
pub struct PlatformMount {
    /// 卸载（必须幂等）
    unmount: Box<dyn Fn() + Send + Sync>,
    /// 后台线程句柄（join 之后挂载资源才真正释放）
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    stopped: AtomicBool,
    /// 平台实际发生的挂载点（WinFsp 传 NULL 时由系统分配盘符，这里回读真实值）
    actual_mount_point: Mutex<String>,
}

impl PlatformMount {
    pub fn new(
        unmount: Box<dyn Fn() + Send + Sync>,
        thread: std::thread::JoinHandle<()>,
        actual_mount_point: String,
    ) -> Self {
        Self {
            unmount,
            thread: Mutex::new(Some(thread)),
            stopped: AtomicBool::new(false),
            actual_mount_point: Mutex::new(actual_mount_point),
        }
    }

    /// 幂等卸载
    pub fn stop(&self) -> Result<()> {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        (self.unmount)();
        Ok(())
    }

    /// 等待后台线程结束
    ///
    /// 这里刻意用「监护线程 + 超时」而不是裸 join：卸载路径上一个永远不返回的
    /// 后台线程会让 GUI 关不掉、CLI 退不出。超时后打印 warn 并放行——
    /// 进程退出时操作系统会回收挂载（WinFsp/FUSE 都会跟着句柄释放）。
    pub fn join_timeout(&self, dur: std::time::Duration) -> bool {
        let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return true;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("tianyi-mount-join".into())
            .spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            })
            .ok();
        match rx.recv_timeout(dur) {
            Ok(()) => true,
            Err(_) => {
                log::warn!("挂载后台线程在 {dur:?} 内没有退出，已放行（进程退出时会回收）");
                false
            }
        }
    }

    pub fn actual_mount_point(&self) -> String {
        self.actual_mount_point
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 一次活着的挂载
pub struct MountHandle {
    fs: Arc<CloudFs>,
    platform: Arc<PlatformMount>,
    options: MountOptions,
    started: SystemTime,
    backend: &'static str,
}

impl MountHandle {
    pub(crate) fn new(
        fs: Arc<CloudFs>,
        platform: Arc<PlatformMount>,
        options: MountOptions,
        backend: &'static str,
    ) -> Self {
        Self {
            fs,
            platform,
            options,
            started: SystemTime::now(),
            backend,
        }
    }

    pub fn fs(&self) -> &Arc<CloudFs> {
        &self.fs
    }

    pub fn options(&self) -> &MountOptions {
        &self.options
    }

    /// 平台实际挂载点（可能优于用户输入，例如盘符自动选择）
    pub fn mount_point(&self) -> String {
        self.platform.actual_mount_point()
    }

    pub fn status(&self) -> MountStatus {
        let snap: StatsSnapshot = self.fs.stats();
        let started_at_unix = self
            .started
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        MountStatus {
            mounted: !self.platform.stopped.load(Ordering::SeqCst),
            mount_point: self.mount_point(),
            unc_path: self.options.effective_unc(self.fs.username()),
            label: self.fs.label().to_string(),
            space: self.fs.space().as_str().to_string(),
            username: self.fs.username().to_string(),
            read_only: self.options.read_only,
            zero_disk_cache: self.options.zero_disk_cache,
            backend: self.backend.to_string(),
            started_at_unix,
            bytes_total: snap.bytes_total,
            requests: snap.requests,
            errors: snap.errors,
            bytes_per_sec: snap.bytes_per_sec,
            cached_dirs: self.fs.cached_dir_count(),
            memory_budget: self.fs.memory_budget(),
            last_error: snap.last_error,
        }
    }

    /// 卸载并等待后台线程退出（最长 [UNMOUNT_TIMEOUT]）
    pub fn unmount(&self) -> Result<()> {
        self.platform.stop()?;
        self.platform.join_timeout(UNMOUNT_TIMEOUT);
        Ok(())
    }

    /// 阻塞等待挂载结束（CLI 前台挂载用）
    pub fn wait(&self) {
        loop {
            if self.platform.stopped.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        // 句柄被丢弃时绝不留孤儿挂载：尽力卸载 + 有界等待
        let _ = self.platform.stop();
        self.platform.join_timeout(UNMOUNT_TIMEOUT);
    }
}

/// 启动挂载（平台分发）
pub fn mount(fs: Arc<CloudFs>, options: MountOptions) -> Result<MountHandle> {
    let target = options.target()?;
    if let MountTarget::Drive(letter) = target {
        if drive_in_use(letter) {
            return Err(Error::MountPointBusy(format!(
                "{}: 已被占用，请换一个盘符",
                letter as char
            )));
        }
    }

    let (platform, backend) = start_backend(&fs, &options, &target)?;
    Ok(MountHandle::new(fs, platform, options, backend))
}

/// 按平台启动挂载后端
///
/// 与 [mount] 分开是必要的：**不支持挂载的平台**（macOS、未编 fuse feature 的
/// Linux）在这里返回明确的中文原因，而 [MountHandle::new] 因此成为所有平台上的
/// 活代码——否则 cfg 剥离之后它会变成 dead_code，在 clippy -D warnings 下直接失败。
fn start_backend(
    fs: &Arc<CloudFs>,
    options: &MountOptions,
    target: &MountTarget,
) -> Result<(Arc<PlatformMount>, &'static str)> {
    #[cfg(windows)]
    {
        crate::win::start(fs.clone(), options, target)
    }

    #[cfg(target_os = "linux")]
    {
        #[cfg(feature = "fuse")]
        {
            crate::fuse_linux::start(fs.clone(), options, target)
        }
        #[cfg(not(feature = "fuse"))]
        {
            let _ = (fs, options, target);
            Err(Error::Other(
                "本构建没有编译 Linux FUSE 后端（缺少 fuse feature）".into(),
            ))
        }
    }

    // 既不是 Windows 也不是 Linux（例如 macOS）：明确说明没有实现，
    // 而不是让编译器在 cfg 剥离后报一个看不懂的"缺少返回值"。
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (fs, options, target);
        Err(Error::Other(
            "本平台（macOS 等）没有实现文件系统挂载，目前只支持 Windows 与 Linux".into(),
        ))
    }
}

/// 供诊断使用的默认挂载点提示
pub fn suggested_mount_point() -> String {
    if cfg!(windows) {
        default_drive_letter()
    } else {
        dirs::home_dir()
            .map(|h| h.join("TianyiCloud").display().to_string())
            .unwrap_or_else(|| "~/TianyiCloud".to_string())
    }
}

/// 把路径的一段转成展示名（诊断日志用）
pub fn describe(segments: &[String]) -> String {
    paths::display_path(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_targets_must_be_drive_letters() {
        if !cfg!(windows) {
            return;
        }
        assert_eq!(parse_mount_target("X:").unwrap(), MountTarget::Drive(b'X'));
        assert_eq!(
            parse_mount_target("x:\\").unwrap(),
            MountTarget::Drive(b'X')
        );
        assert!(matches!(
            parse_mount_target("C:"),
            Err(Error::InvalidMountPoint(_))
        ));
        assert!(matches!(
            parse_mount_target("C:\\Users"),
            Err(Error::InvalidMountPoint(_))
        ));
        assert!(matches!(
            parse_mount_target("\\\\.\\X:"),
            Err(Error::InvalidMountPoint(_))
        ));
        assert!(matches!(
            parse_mount_target(""),
            Err(Error::InvalidMountPoint(_))
        ));
    }

    #[test]
    fn unix_targets_must_be_absolute() {
        if cfg!(windows) {
            return;
        }
        assert!(matches!(
            parse_mount_target("/mnt/x"),
            Ok(MountTarget::Directory(_))
        ));
        assert!(matches!(
            parse_mount_target("relative/path"),
            Err(Error::InvalidMountPoint(_))
        ));
        assert!(parse_mount_target("~/TianyiCloud").is_ok());
    }

    #[test]
    fn unc_prefix_is_cleaned_and_unique_per_account() {
        let a = unc_prefix(Space::Personal, "alice");
        let b = unc_prefix(Space::Personal, "bob");
        assert_ne!(a, b, "多账号不能共用 UNC 名字");
        assert!(a.starts_with('\\'), "{a}");
        assert!(!a.contains(".."));
        assert!(!a.contains('/'));
        assert_eq!(a, "\\TianyiCloud-alice\\Personal");
        assert_eq!(
            unc_prefix(Space::Family, "alice"),
            "\\TianyiCloud-alice\\Family"
        );
    }

    #[test]
    fn unc_prefix_never_contains_uncleaned_user_input() {
        let p = unc_prefix(Space::Personal, "a b\\c/d:e@f");
        assert!(!p.contains(' '), "{p}");
        assert!(!p.contains('/'), "{p}");
        assert_eq!(p.matches('\\').count(), 2, "{p}");
        let long = unc_prefix(Space::Personal, &"x".repeat(200));
        assert!(long.len() < 190, "Prefix 字段只有 192 个 wchar：{long}");
    }

    #[test]
    fn empty_account_falls_back_to_a_stable_name() {
        assert_eq!(unc_prefix(Space::Personal, ""), "\\TianyiCloud\\Personal");
    }

    #[test]
    fn default_options_are_read_only_and_zero_cache() {
        let o = MountOptions::default();
        assert!(o.read_only);
        assert!(o.zero_disk_cache);
        assert!(!o.mount_point.is_empty());
    }
}
