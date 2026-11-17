//! Windows 后端：WinFsp 网络文件系统（DRIVE_REMOTE）
//!
//! # 为什么是"网络文件系统"
//!
//! WinFsp 有两条控制设备：`WinFsp.Disk`（本地盘语义）与 `WinFsp.Net`（网络盘语义）。
//! 参考实现的判定逻辑是：**只要 `VolumeParams.Prefix` 非空就选 `WinFsp.Net`**。
//! 于是挂出来的东西：
//!
//! * `GetDriveTypeW("X:\\")` 返回 `DRIVE_REMOTE(4)`，资源管理器把它归到"网络驱动器"；
//! * 同时可以通过 UNC 路径 `\\TianyiCloud-账号\Personal` 访问（WinFsp 自带
//!   `WinFsp.Np` 网络提供程序，注册优先级排在 LanmanWorkstation 之前）。
//!
//! 这不是"改图标/写注册表/做快捷方式"冒充——类型由驱动层决定，改不了。
//!
//! # 回调里的线程模型
//!
//! WinFsp 的 dispatcher 线程是**同步**的，而云端读取是异步的。因此每个回调都：
//!
//! 1. 在 `catch_unwind` 里执行（Rust panic 绝不能穿过 FFI 边界）；
//! 2. 通过 [crate::runtime::block_on_timeout] 把异步请求跑完，**带硬超时**；
//! 3. 绝不长时间阻塞——超时如期返回 `STATUS_IO_TIMEOUT`，资源管理器不会假死。

pub mod sys;

use crate::error::{Error, Result};
use crate::fs::CloudFs;
use crate::meta::{to_filetime, Entry};
use crate::mount::{MountOptions, MountTarget, PlatformMount};
use crate::paths;
use crate::runtime::block_on_timeout;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use sys::*;

/// 单次回调最多等多久（网络请求自身还有更细的超时）
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// 挂载就绪等待上限
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// 每个挂载实例的宿主状态
struct Host {
    cloud: Arc<CloudFs>,
}

static HOSTS: OnceLock<Mutex<HashMap<usize, Arc<Host>>>> = OnceLock::new();

fn hosts() -> &'static Mutex<HashMap<usize, Arc<Host>>> {
    HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_host(fs: *mut FspFileSystem, host: Arc<Host>) {
    let mut g = hosts().lock().unwrap_or_else(|e| e.into_inner());
    g.insert(fs as usize, host);
}

fn unregister_host(fs: *mut FspFileSystem) {
    let mut g = hosts().lock().unwrap_or_else(|e| e.into_inner());
    g.remove(&(fs as usize));
}

/// 从 FSP_FILE_SYSTEM 找回宿主
///
/// 多账号同时挂载时每个挂载有各自的 CloudFs，因此必须按指针区分，不能用一个全局变量。
fn host_of(fs: *mut FspFileSystem) -> Option<Arc<Host>> {
    let g = hosts().lock().unwrap_or_else(|e| e.into_inner());
    g.get(&(fs as usize)).cloned()
}

macro_rules! host {
    ($fs:expr) => {
        match host_of($fs) {
            Some(h) => h,
            None => return STATUS_INVALID_PARAMETER,
        }
    };
}

/// 打开句柄的上下文（由 WinFsp 通过 FileContext 原样保管）
struct OpenContext {
    entry: Entry,
    segments: Vec<String>,
    dir: Mutex<DirBuffer>,
}

/// 目录枚举缓冲（WinFsp 侧的缓冲区句柄 + 是否已填充）
struct DirBuffer {
    ptr: PVOID,
    filled: bool,
}

impl DirBuffer {
    fn new() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            filled: false,
        }
    }
}

impl Drop for DirBuffer {
    fn drop(&mut self) {
        if let Ok(api) = sys::api() {
            unsafe { (api.delete_dir_buffer)(&mut self.ptr) };
        }
    }
}

/// 把内部错误映射成 NTSTATUS
fn ntstatus(err: &Error) -> NTSTATUS {
    match err {
        Error::NotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        Error::Cloud(tianyi_core::Error::NotFound(_)) => STATUS_OBJECT_NAME_NOT_FOUND,
        Error::ReadOnly(_) => STATUS_MEDIA_WRITE_PROTECTED,
        Error::Timeout(_) => STATUS_IO_TIMEOUT,
        Error::RangeUnsupported { .. } => STATUS_NOT_SUPPORTED,
        Error::RangeMismatch(_) => STATUS_INVALID_PARAMETER,
        Error::Network(_) => STATUS_NETWORK_UNREACHABLE,
        Error::Cloud(tianyi_core::Error::NotAuthenticated(_))
        | Error::Cloud(tianyi_core::Error::Session(_)) => STATUS_ACCESS_DENIED,
        _ => STATUS_ACCESS_DENIED,
    }
}

/// 同步执行一段"同步逻辑 + 异步请求 + 硬超时"的回调体
fn run<T, F>(f: F) -> std::result::Result<T, NTSTATUS>
where
    F: FnOnce() -> Result<T>,
{
    match f() {
        Ok(v) => Ok(v),
        Err(e) => {
            // 只记摘要，绝不打印直链
            log::warn!("WinFsp 操作失败：{}", e.brief());
            Err(ntstatus(&e))
        }
    }
}

/// panic 兜底：任何回调 panic 都必须变成一个 NTSTATUS
fn protect<F>(f: F) -> NTSTATUS
where
    F: FnOnce() -> NTSTATUS + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(s) => s,
        Err(_) => {
            log::error!("WinFsp 回调内部 panic（已拦截，未跨越 FFI 边界）");
            STATUS_ACCESS_DENIED
        }
    }
}

fn file_info_of(entry: &Entry) -> FileInfo {
    let attrs = if entry.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_READONLY
    };
    let mtime = to_filetime(entry.modified);
    let ctime = to_filetime(entry.created);
    FileInfo {
        FileAttributes: attrs,
        ReparseTag: 0,
        AllocationSize: if entry.is_dir {
            0
        } else {
            // 4 KiB 对齐的"分配大小"，Explorer 的"占用空间"列用它
            entry.size.div_ceil(4096) * 4096
        },
        FileSize: entry.size,
        CreationTime: ctime,
        LastAccessTime: mtime,
        LastWriteTime: mtime,
        ChangeTime: mtime,
        IndexNumber: entry.index_number(),
        HardLinks: 0,
        EaSize: 0,
    }
}

fn read_only_error() -> NTSTATUS {
    STATUS_MEDIA_WRITE_PROTECTED
}

// ------------------------------------------------------------------ 回调
unsafe extern "system" fn cb_get_volume_info(
    fs: *mut FspFileSystem,
    info: *mut VolumeInfo,
) -> NTSTATUS {
    protect(|| {
        let host = host! { fs };
        let (total, free) =
            block_on_timeout(CALLBACK_TIMEOUT, async { host.cloud.capacity().await })
                .unwrap_or((0, 0));
        let label = host.cloud.label();
        let mut out = VolumeInfo {
            TotalSize: total,
            FreeSize: free,
            VolumeLabelLength: 0,
            VolumeLabel: [0; 32],
        };
        let wide: Vec<u16> = label.encode_utf16().take(31).collect();
        out.VolumeLabel[..wide.len()].copy_from_slice(&wide);
        out.VolumeLabelLength = (wide.len() * 2) as u16;
        unsafe { *info = out };
        STATUS_SUCCESS
    })
}

unsafe extern "system" fn cb_set_volume_label(
    _fs: *mut FspFileSystem,
    _label: PWSTR,
    _info: *mut VolumeInfo,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_open(
    fs: *mut FspFileSystem,
    file_name: PWSTR,
    create_options: u32,
    granted_access: u32,
    file_context: *mut PVOID,
    file_info: *mut FileInfo,
) -> NTSTATUS {
    protect(|| {
        let host = host! { fs };
        let raw = unsafe { from_wide(file_name) };
        let segments = match paths::split_remote_path(&raw) {
            Ok(s) => s,
            Err(e) => return ntstatus(&e),
        };
        // 只读卷：只要请求了写权限就直接拒绝，但不牵连 FILE_READ_DATA 等非破坏性访问
        if granted_access & FILE_WRITE_ACCESS_MASK != 0
            || create_options & FILE_DELETE_ON_CLOSE != 0
        {
            return STATUS_ACCESS_DENIED;
        }
        let entry = match run(|| {
            block_on_timeout(CALLBACK_TIMEOUT, host.cloud.resolve(&segments))
                .map_err(Error::Timeout)?
        }) {
            Ok(e) => e,
            Err(s) => return s,
        };
        if create_options & FILE_DIRECTORY_FILE != 0 && !entry.is_dir {
            return STATUS_NOT_A_DIRECTORY;
        }
        if create_options & FILE_NON_DIRECTORY_FILE != 0 && entry.is_dir {
            return STATUS_FILE_IS_A_DIRECTORY;
        }
        unsafe {
            *file_info = file_info_of(&entry);
            *file_context = Box::into_raw(Box::new(OpenContext {
                entry,
                segments,
                dir: Mutex::new(DirBuffer::new()),
            })) as PVOID;
        }
        STATUS_SUCCESS
    })
}

/// Create：只读卷上"打开已存在的文件"可以成功，"创建/覆盖"一律拒绝
///
/// 关键点：很多程序（含资源管理器）用 `FILE_OPEN_IF` 打开已存在的文件，
/// 所以这里不能一刀切返回写保护，否则读都读不了。
unsafe extern "system" fn cb_create(
    fs: *mut FspFileSystem,
    file_name: PWSTR,
    create_options: u32,
    granted_access: u32,
    _file_attributes: u32,
    _security_descriptor: PVOID,
    _allocation_size: u64,
    file_context: *mut PVOID,
    file_info: *mut FileInfo,
) -> NTSTATUS {
    protect(|| {
        let host = host! { fs };
        let raw = unsafe { from_wide(file_name) };
        let segments = match paths::split_remote_path(&raw) {
            Ok(s) => s,
            Err(e) => return ntstatus(&e),
        };
        if granted_access & FILE_WRITE_ACCESS_MASK != 0
            || create_options & FILE_DELETE_ON_CLOSE != 0
        {
            return STATUS_ACCESS_DENIED;
        }
        let entry = match block_on_timeout(CALLBACK_TIMEOUT, host.cloud.resolve(&segments)) {
            Ok(Ok(e)) => e,
            Ok(Err(Error::NotFound(_))) => return STATUS_OBJECT_NAME_NOT_FOUND,
            Ok(Err(e)) => return ntstatus(&e),
            Err(_) => return STATUS_IO_TIMEOUT,
        };
        if create_options & FILE_DIRECTORY_FILE != 0 && !entry.is_dir {
            return STATUS_NOT_A_DIRECTORY;
        }
        if create_options & FILE_NON_DIRECTORY_FILE != 0 && entry.is_dir {
            return STATUS_FILE_IS_A_DIRECTORY;
        }
        unsafe {
            *file_info = file_info_of(&entry);
            *file_context = Box::into_raw(Box::new(OpenContext {
                entry,
                segments,
                dir: Mutex::new(DirBuffer::new()),
            })) as PVOID;
        }
        STATUS_SUCCESS
    })
}

unsafe extern "system" fn cb_cleanup(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    file_name: PWSTR,
    flags: u32,
) {
    if flags & FSP_CLEANUP_DELETE != 0 {
        let name = unsafe { from_wide(file_name) };
        log::warn!("只读挂载收到删除请求（已忽略）：{name}");
    }
}

unsafe extern "system" fn cb_close(_fs: *mut FspFileSystem, file_context: PVOID) {
    if file_context.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(file_context as *mut OpenContext) });
}

unsafe extern "system" fn cb_read(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    buffer: PVOID,
    offset: u64,
    length: ULONG,
    bytes_transferred: *mut ULONG,
) -> NTSTATUS {
    protect(|| {
        if file_context.is_null() || buffer.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        unsafe { *bytes_transferred = 0 };
        let ctx = unsafe { &*(file_context as *const OpenContext) };
        if ctx.entry.is_dir {
            return STATUS_INVALID_PARAMETER;
        }
        if length == 0 {
            return STATUS_SUCCESS;
        }
        let host = host! { fs };
        let data = match block_on_timeout(
            CALLBACK_TIMEOUT,
            host.cloud.read(&ctx.entry, offset, length),
        ) {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return ntstatus(&e),
            Err(_) => return STATUS_IO_TIMEOUT,
        };
        if data.is_empty() {
            // 到 EOF（offset 已经 >= 文件大小）才算正常结束
            return if offset >= ctx.entry.size {
                STATUS_SUCCESS
            } else {
                STATUS_END_OF_FILE
            };
        }
        let n = data.len().min(length as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), buffer as *mut u8, n);
            *bytes_transferred = n as ULONG;
        }
        STATUS_SUCCESS
    })
}

unsafe extern "system" fn cb_write(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _buffer: PVOID,
    _offset: u64,
    _length: ULONG,
    _write_to_eof: BOOLEAN,
    _constrained: BOOLEAN,
    bytes_transferred: *mut ULONG,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    if !bytes_transferred.is_null() {
        unsafe { *bytes_transferred = 0 };
    }
    read_only_error()
}

unsafe extern "system" fn cb_flush(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    STATUS_SUCCESS
}

unsafe extern "system" fn cb_get_file_info(
    _fs: *mut FspFileSystem,
    file_context: PVOID,
    file_info: *mut FileInfo,
) -> NTSTATUS {
    protect(|| {
        if file_context.is_null() || file_info.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let ctx = unsafe { &*(file_context as *const OpenContext) };
        unsafe { *file_info = file_info_of(&ctx.entry) };
        STATUS_SUCCESS
    })
}

unsafe extern "system" fn cb_set_basic_info(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _file_attributes: u32,
    _creation_time: u64,
    _last_access_time: u64,
    _last_write_time: u64,
    _change_time: u64,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_set_file_size(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _new_size: u64,
    _set_allocation_size: BOOLEAN,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_can_delete(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _file_name: PWSTR,
) -> NTSTATUS {
    STATUS_CANNOT_DELETE
}

unsafe extern "system" fn cb_rename(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _file_name: PWSTR,
    _new_file_name: PWSTR,
    _replace_if_exists: BOOLEAN,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_set_delete(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _file_name: PWSTR,
    _delete_file: BOOLEAN,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_overwrite(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _file_attributes: u32,
    _replace_attributes: BOOLEAN,
    _allocation_size: u64,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_get_security(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _security_descriptor: PVOID,
    security_descriptor_size: *mut SIZE_T,
) -> NTSTATUS {
    // PersistentAcls = 0：由 WinFsp 提供默认安全描述符
    if !security_descriptor_size.is_null() {
        unsafe { *security_descriptor_size = 0 };
    }
    STATUS_NOT_IMPLEMENTED
}

unsafe extern "system" fn cb_set_security(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _security_information: SECURITY_INFORMATION,
    _modification_descriptor: PVOID,
) -> NTSTATUS {
    read_only_error()
}

unsafe extern "system" fn cb_read_directory(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    _pattern: PWSTR,
    marker: PWSTR,
    buffer: PVOID,
    length: ULONG,
    bytes_transferred: *mut ULONG,
) -> NTSTATUS {
    protect(|| {
        if file_context.is_null() || buffer.is_null() || bytes_transferred.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        unsafe { *bytes_transferred = 0 };
        let ctx = unsafe { &*(file_context as *const OpenContext) };
        if !ctx.entry.is_dir {
            return STATUS_INVALID_PARAMETER;
        }
        let host = host! { fs };
        let api = match sys::api() {
            Ok(a) => a,
            Err(e) => {
                log::error!("{e}");
                return STATUS_NOT_IMPLEMENTED;
            }
        };
        let mut state = ctx.dir.lock().unwrap_or_else(|e| e.into_inner());

        // 目录缓冲区只能在【同一侧】配对：
        //   Acquire(Reset=TRUE) → Fill* → Release
        // 其中 Release 放掉的是**独占** SRW 锁；而 ReadDirectoryBuffer 自己会拿/放共享锁。
        // 早先的写法在每次 Read 之后又调一次 Release，等于重复释放独占锁，
        // SRW 锁状态被破坏 → 后续所有目录请求（以及 Create / unmount）全部死锁。
        if !state.filled {
            let listing =
                match block_on_timeout(CALLBACK_TIMEOUT, host.cloud.list_dir(&ctx.segments)) {
                    Ok(Ok(l)) => l,
                    Ok(Err(e)) => return ntstatus(&e),
                    Err(_) => return STATUS_IO_TIMEOUT,
                };
            let mut status: NTSTATUS = STATUS_SUCCESS;
            unsafe { (api.acquire_dir_buffer)(&mut state.ptr, 1, 0, &mut status) };
            if status != STATUS_SUCCESS {
                return status;
            }
            for entry in &listing.entries {
                let info = file_info_of(entry);
                let raw = make_dir_info(&info, &entry.name);
                let mut st: NTSTATUS = STATUS_SUCCESS;
                let ok = unsafe {
                    (api.fill_dir_buffer)(&mut state.ptr, raw.as_ptr() as *mut DirInfo, &mut st)
                };
                if ok == 0 {
                    // 缓冲区满：WinFsp 会带着 marker 再来一次，这里是正常路径
                    break;
                }
            }
            // 排好序并放掉独占锁；之后的 Read 只拿共享锁
            unsafe { (api.release_dir_buffer)(&mut state.ptr) };
            state.filled = true;
        }

        unsafe {
            (api.read_dir_buffer)(&mut state.ptr, marker, buffer, length, bytes_transferred);
        }
        STATUS_SUCCESS
    })
}

unsafe extern "system" fn cb_dispatcher_stopped(_fs: *mut FspFileSystem, normally: BOOLEAN) {
    log::info!("WinFsp dispatcher 已停止（normal={normally}）");
}

fn build_volume_params(label: &str, unc_prefix: &str) -> VolumeParams {
    let mut params = VolumeParams {
        Version: 0,
        SectorSize: 4096,
        SectorsPerAllocationUnit: 1,
        MaxComponentLength: 255,
        VolumeCreationTime: 0,
        VolumeSerialNumber: 0x5459_434C, // "TYCL"
        TransactTimeout: 0,
        IrpTimeout: 0,
        IrpCapacity: 0,
        // 元数据缓存 1 秒：云端被别的客户端改过之后，这里能较快反映出来
        FileInfoTimeout: 1000,
        Flags: VP_FLAG_CASE_PRESERVED_NAMES
            | VP_FLAG_UNICODE_ON_DISK
            | VP_FLAG_READ_ONLY_VOLUME
            | VP_FLAG_ALWAYS_USE_DOUBLE_BUFFERING,
        Prefix: [0; 192],
        FileSystemName: [0; 16],
        FlagsV1: 0,
        VolumeInfoTimeout: 0,
        DirInfoTimeout: 0,
        SecurityTimeout: 0,
        StreamInfoTimeout: 0,
        EaTimeout: 0,
        FsextControlCode: 0,
        Reserved32: 0,
        Reserved64: [0; 2],
    };
    params.VolumeCreationTime = to_filetime(std::time::SystemTime::now());
    let prefix: Vec<u16> = unc_prefix.encode_utf16().take(191).collect();
    params.Prefix[..prefix.len()].copy_from_slice(&prefix);
    let fsname: Vec<u16> = "TianyiCloud".encode_utf16().take(15).collect();
    params.FileSystemName[..fsname.len()].copy_from_slice(&fsname);
    let _ = label;
    params
}

fn build_interface() -> &'static FileSystemInterface {
    Box::leak(Box::new(FileSystemInterface {
        GetVolumeInfo: Some(cb_get_volume_info),
        SetVolumeLabel: Some(cb_set_volume_label),
        GetSecurityByName: None,
        Create: Some(cb_create),
        Open: Some(cb_open),
        Overwrite: Some(cb_overwrite),
        Cleanup: Some(cb_cleanup),
        Close: Some(cb_close),
        Read: Some(cb_read),
        Write: Some(cb_write),
        Flush: Some(cb_flush),
        GetFileInfo: Some(cb_get_file_info),
        SetBasicInfo: Some(cb_set_basic_info),
        SetFileSize: Some(cb_set_file_size),
        CanDelete: Some(cb_can_delete),
        Rename: Some(cb_rename),
        GetSecurity: Some(cb_get_security),
        SetSecurity: Some(cb_set_security),
        ReadDirectory: Some(cb_read_directory),
        ResolveReparsePoints: None,
        GetReparsePoint: None,
        SetReparsePoint: None,
        DeleteReparsePoint: None,
        GetStreamInfo: None,
        GetDirInfoByName: None,
        Control: None,
        SetDelete: Some(cb_set_delete),
        CreateEx: None,
        OverwriteEx: None,
        GetEa: None,
        SetEa: None,
        Obsolete0: None,
        DispatcherStopped: Some(cb_dispatcher_stopped),
        Reserved: [None; 31],
    }))
}

/// 启动 WinFsp 挂载
///
/// 返回前保证：文件系统已创建、挂载点已生效、dispatcher 已在跑。
/// 任何一步失败都会把错误原样抛出（GUI 据此提示安装 WinFsp 或换盘符）。
pub fn start(
    cloud: Arc<CloudFs>,
    options: &MountOptions,
    target: &MountTarget,
) -> Result<(Arc<PlatformMount>, &'static str)> {
    let api = sys::api().map_err(Error::WinFspMissing)?;
    let letter = target
        .drive_letter()
        .ok_or_else(|| Error::InvalidMountPoint("Windows 只支持盘符挂载".into()))?;
    let mount_point = format!("{}:", letter as char);
    let unc = options.effective_unc(cloud.username());
    let params = build_volume_params(cloud.label(), &unc);
    let interface = build_interface();

    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
    let stop_thread = stop.clone();
    let mp = mount_point.clone();

    let thread = std::thread::Builder::new()
        .name("tianyi-mount-winfsp".into())
        .spawn(move || {
            unsafe {
                let mut fs: *mut FspFileSystem = std::ptr::null_mut();
                let status = (api.create)(
                    to_wide("WinFsp.Net").as_ptr() as PWSTR,
                    &params,
                    interface,
                    &mut fs,
                );
                if status != STATUS_SUCCESS || fs.is_null() {
                    let _ = ready_tx.send(Err(format!(
                        "WinFsp 创建文件系统失败：NTSTATUS 0x{status:08X}（盘符 {mp}）"
                    )));
                    return;
                }
                register_host(
                    fs,
                    Arc::new(Host {
                        cloud: cloud.clone(),
                    }),
                );

                let wide_mount = to_wide(&mp);
                let status = (api.set_mount_point)(fs, wide_mount.as_ptr() as PWSTR);
                if status != STATUS_SUCCESS {
                    unregister_host(fs);
                    (api.delete)(fs);
                    let _ = ready_tx.send(Err(format!(
                        "WinFsp 设置挂载点 {mp} 失败：NTSTATUS 0x{status:08X}"
                    )));
                    return;
                }

                let status = (api.start_dispatcher)(fs, 0);
                if status != STATUS_SUCCESS {
                    (api.remove_mount_point)(fs);
                    unregister_host(fs);
                    (api.delete)(fs);
                    let _ = ready_tx.send(Err(format!(
                        "WinFsp 启动 dispatcher 失败：NTSTATUS 0x{status:08X}"
                    )));
                    return;
                }

                log::info!(
                    "WinFsp 网络文件系统已挂载：{mp}（UNC {unc}，卷标 {}）",
                    cloud.label()
                );
                let _ = ready_tx.send(Ok(()));

                // 等待卸载信号：不轮询、不 sleep，纯条件变量
                let (lock, cvar) = &*stop_thread;
                let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*stopped {
                    let (guard, timeout) = cvar
                        .wait_timeout(stopped, Duration::from_millis(500))
                        .unwrap_or_else(|e| e.into_inner());
                    stopped = guard;
                    if timeout.timed_out() && *stopped {
                        break;
                    }
                }
                drop(stopped);

                // 卸载顺序按 WinFsp 文档：先摘挂载点，再停 dispatcher，最后释放对象
                (api.remove_mount_point)(fs);
                (api.stop_dispatcher)(fs);
                unregister_host(fs);
                (api.delete)(fs);
                log::info!("WinFsp 挂载已卸载：{mp}");
            }
        })
        .map_err(|e| Error::WinFsp(format!("创建工作线程失败：{e}")))?;

    match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            let _ = thread.join();
            return Err(Error::WinFsp(msg));
        }
        Err(_) => {
            // 超时：让线程自行退出，避免半挂载状态残留
            let (lock, cvar) = &*stop;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cvar.notify_all();
            let _ = thread.join();
            return Err(Error::WinFsp(format!(
                "WinFsp 在 {STARTUP_TIMEOUT:?} 内没有完成挂载（盘符 {mount_point}）"
            )));
        }
    }

    let stop_for_unmount = stop.clone();
    let unmount = Box::new(move || {
        let (lock, cvar) = &*stop_for_unmount;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
    });

    let platform = PlatformMount::new(unmount, thread, mount_point);
    Ok((Arc::new(platform), "winfsp"))
}

/// 挂载是否真的以"网络驱动器"出现在系统里（验收用）
///
/// 这是唯一能证明"不是本地盘伪装的"手段：类型由内核按设备类型判定。
pub fn verify_network_drive(mount_point: &str) -> bool {
    crate::mount::drive_type_of(mount_point) == crate::mount::DRIVE_REMOTE
}

/// 一个挂载实例是否还活着（探活）：用于 unmount 命令确认真的卸载干净
pub fn is_mounted(mount_point: &str) -> bool {
    crate::mount::drive_type_of(mount_point) != crate::mount::DRIVE_NO_ROOT_DIR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_params_request_a_read_only_unicode_volume() {
        let p = build_volume_params("天翼云盘", "\\TianyiCloud-alice\\Personal");
        assert_ne!(p.Flags & VP_FLAG_READ_ONLY_VOLUME, 0, "必须声明只读卷");
        assert_ne!(
            p.Flags & VP_FLAG_UNICODE_ON_DISK,
            0,
            "必须支持 Unicode 文件名"
        );
        assert_eq!(
            p.Flags & VP_FLAG_CASE_PRESERVED_NAMES,
            VP_FLAG_CASE_PRESERVED_NAMES
        );
        let prefix: String = {
            let end = p.Prefix.iter().position(|c| *c == 0).unwrap_or(192);
            String::from_utf16_lossy(&p.Prefix[..end])
        };
        assert_eq!(
            prefix, "\\TianyiCloud-alice\\Personal",
            "UNC 前缀决定网络盘语义"
        );
        assert!(p.Prefix[191] == 0, "Prefix 必须留结尾 0 的位置");
        let name: String = {
            let end = p.FileSystemName.iter().position(|c| *c == 0).unwrap_or(16);
            String::from_utf16_lossy(&p.FileSystemName[..end])
        };
        assert_eq!(name, "TianyiCloud");
    }

    #[test]
    fn long_unc_prefix_is_truncated_not_overflowed() {
        let long = format!("\\{}", "x".repeat(400));
        let p = build_volume_params("x", &long);
        assert_eq!(p.Prefix[191], 0);
        assert_eq!(p.Prefix[190], 'x' as u16);
    }

    #[test]
    fn file_info_marks_files_read_only_and_directories_not() {
        let mut e = Entry {
            id: "1".into(),
            name: "a.txt".into(),
            raw_name: "a.txt".into(),
            is_dir: false,
            size: 5000,
            created: std::time::UNIX_EPOCH,
            modified: std::time::UNIX_EPOCH,
        };
        let fi = file_info_of(&e);
        assert_ne!(fi.FileAttributes & FILE_ATTRIBUTE_READONLY, 0);
        assert_eq!(fi.FileSize, 5000);
        assert_eq!(fi.AllocationSize, 8192, "分配大小按 4 KiB 向上取整");
        e.is_dir = true;
        e.size = 0;
        let fi = file_info_of(&e);
        assert_ne!(fi.FileAttributes & FILE_ATTRIBUTE_DIRECTORY, 0);
        assert_eq!(fi.FileAttributes & FILE_ATTRIBUTE_READONLY, 0);
    }

    #[test]
    fn errors_map_to_sensible_status_codes() {
        assert_eq!(
            ntstatus(&Error::ReadOnly("x".into())),
            STATUS_MEDIA_WRITE_PROTECTED
        );
        assert_eq!(
            ntstatus(&Error::RangeUnsupported { status: 200 }),
            STATUS_NOT_SUPPORTED
        );
        assert_eq!(
            ntstatus(&Error::NotFound("x".into())),
            STATUS_OBJECT_NAME_NOT_FOUND
        );
        assert_eq!(
            ntstatus(&Error::Timeout(Duration::from_secs(1))),
            STATUS_IO_TIMEOUT
        );
    }
}
