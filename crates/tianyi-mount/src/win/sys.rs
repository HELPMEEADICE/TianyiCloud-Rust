//! WinFsp 的最小 FFI 绑定（手写 + 运行时动态加载）
//!
//! 为什么不用现成的 winfsp crate：它要求 MSVC + bindgen(libclang) + WinFsp SDK
//! （inc/lib），而现实里很多机器只装了 WinFsp 运行时。这里把需要的东西手写出来，
//! 用 `LoadLibraryW` + `GetProcAddress` 取函数地址，于是：
//!
//! * 不需要 SDK 头文件、不需要 import lib、不需要 clang；
//! * MSVC 与 GNU 两种工具链都能编；
//! * WinFsp 没装时是一个**可诊断的错误**（GUI 给出安装指引），而不是加载期崩溃。
//!
//! 结构体布局逐个对照 WinFsp 2.1 的 `winfsp/inc/winfsp/{winfsp,fsctl}.h` 手译，
//! 并在下方用 `const _: () = assert!(...)` 把 sizeof 钉死——ABI 一旦对不上，
//! 编译期就会失败，而不是在用户机器上蓝屏式崩溃。

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::sync::OnceLock;

pub type NTSTATUS = i32;
pub type PWSTR = *mut u16;
pub type PVOID = *mut c_void;
pub type BOOLEAN = u8;
pub type ULONG = u32;
pub type SIZE_T = usize;
pub type SECURITY_INFORMATION = u32;

// ---------------------------------------------------------------- NTSTATUS
pub const STATUS_SUCCESS: NTSTATUS = 0;
pub const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as NTSTATUS;
pub const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC000_00BBu32 as NTSTATUS;
pub const STATUS_END_OF_FILE: NTSTATUS = 0xC000_0011u32 as NTSTATUS;
pub const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as NTSTATUS;
pub const STATUS_ACCESS_DENIED: NTSTATUS = 0xC000_0022u32 as NTSTATUS;
pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC000_0034u32 as NTSTATUS;
pub const STATUS_OBJECT_PATH_NOT_FOUND: NTSTATUS = 0xC000_003Au32 as NTSTATUS;
pub const STATUS_MEDIA_WRITE_PROTECTED: NTSTATUS = 0xC000_00A2u32 as NTSTATUS;
pub const STATUS_DIRECTORY_NOT_EMPTY: NTSTATUS = 0xC000_0101u32 as NTSTATUS;
pub const STATUS_NOT_A_DIRECTORY: NTSTATUS = 0xC000_0103u32 as NTSTATUS;
pub const STATUS_CANNOT_DELETE: NTSTATUS = 0xC000_0121u32 as NTSTATUS;
pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = 0xC000_00BAu32 as NTSTATUS;
pub const STATUS_IO_TIMEOUT: NTSTATUS = 0xC000_00B5u32 as NTSTATUS;
pub const STATUS_NETWORK_UNREACHABLE: NTSTATUS = 0xC000_023Cu32 as NTSTATUS;
pub const STATUS_BUFFER_OVERFLOW: NTSTATUS = 0x8000_0005u32 as NTSTATUS;

// ------------------------------------------------------- 文件属性 / 打开选项
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;

/// NT 访问掩码里所有"会改变数据/元数据"的位
///
/// 只读挂载必须拒绝这些，但**不能**因此拒绝 FILE_READ_DATA / FILE_READ_ATTRIBUTES /
/// SYNCHRONIZE 之类的非破坏性访问——很多普通程序打开文件时都会顺手要几个写位之外的权限。
pub const FILE_WRITE_ACCESS_MASK: u32 = 0x0000_0002  // FILE_WRITE_DATA
    | 0x0000_0004  // FILE_APPEND_DATA
    | 0x0000_0010  // FILE_WRITE_EA
    | 0x0000_0100  // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000  // DELETE
    | 0x0004_0000  // WRITE_DAC
    | 0x0008_0000; // WRITE_OWNER

/// Cleanup 回调的标志位
pub const FSP_CLEANUP_DELETE: u32 = 0x01;

// ------------------------------------------------------------------ 结构体
/// FSP_FSCTL_VOLUME_PARAMS（504 字节）
///
/// 位域按 MSVC 的 UINT32:1 布局从低位起排，这里用一个 u32 + 常量表达，
/// 既保证 ABI 又避免 Rust 位域（不稳定特性）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VolumeParams {
    pub Version: u16,
    pub SectorSize: u16,
    pub SectorsPerAllocationUnit: u16,
    pub MaxComponentLength: u16,
    pub VolumeCreationTime: u64,
    pub VolumeSerialNumber: u32,
    pub TransactTimeout: u32,
    pub IrpTimeout: u32,
    pub IrpCapacity: u32,
    pub FileInfoTimeout: u32,
    pub Flags: u32,
    pub Prefix: [u16; 192],
    pub FileSystemName: [u16; 16],
    pub FlagsV1: u32,
    pub VolumeInfoTimeout: u32,
    pub DirInfoTimeout: u32,
    pub SecurityTimeout: u32,
    pub StreamInfoTimeout: u32,
    pub EaTimeout: u32,
    pub FsextControlCode: u32,
    pub Reserved32: u32,
    pub Reserved64: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<VolumeParams>() == 504);
const _: () = assert!(std::mem::offset_of!(VolumeParams, Prefix) == 40);
const _: () = assert!(std::mem::offset_of!(VolumeParams, FileSystemName) == 424);

// Flags 位（见 fsctl.h 的 FSP_FSCTL_VOLUME_PARAMS_V0_FIELD_DEFN）
pub const VP_FLAG_CASE_PRESERVED_NAMES: u32 = 1 << 1;
pub const VP_FLAG_UNICODE_ON_DISK: u32 = 1 << 2;
pub const VP_FLAG_READ_ONLY_VOLUME: u32 = 1 << 9;
pub const VP_FLAG_ALWAYS_USE_DOUBLE_BUFFERING: u32 = 1 << 12;

/// FSP_FSCTL_VOLUME_INFO（88 字节）
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VolumeInfo {
    pub TotalSize: u64,
    pub FreeSize: u64,
    pub VolumeLabelLength: u16,
    pub VolumeLabel: [u16; 32],
}

const _: () = assert!(std::mem::size_of::<VolumeInfo>() == 88);

/// FSP_FSCTL_FILE_INFO（72 字节）
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FileInfo {
    pub FileAttributes: u32,
    pub ReparseTag: u32,
    pub AllocationSize: u64,
    pub FileSize: u64,
    pub CreationTime: u64,
    pub LastAccessTime: u64,
    pub LastWriteTime: u64,
    pub ChangeTime: u64,
    pub IndexNumber: u64,
    pub HardLinks: u32,
    pub EaSize: u32,
}

const _: () = assert!(std::mem::size_of::<FileInfo>() == 72);

/// FSP_FSCTL_DIR_INFO 的定长部分（104 字节）
///
/// 后面紧跟 `FileNameBuf[]`：构造时在一块 [u8] 里把定长头和名字连起来，
/// `Size = 104 + 名字字节数`（不含结尾 0，与 WinFsp 官方样例 memfs 一致）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirInfo {
    pub Size: u16,
    pub FileInfo: FileInfo,
    pub NextOffset: u64,
    pub Padding: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<DirInfo>() == 104);

/// 目录条目名字的偏移（= sizeof(DirInfo)）
pub const DIR_INFO_NAME_OFFSET: usize = 104;

// ------------------------------------------------------- 回调函数类型别名
pub type GetVolumeInfoFn =
    unsafe extern "system" fn(*mut FspFileSystem, *mut VolumeInfo) -> NTSTATUS;
pub type SetVolumeLabelFn =
    unsafe extern "system" fn(*mut FspFileSystem, PWSTR, *mut VolumeInfo) -> NTSTATUS;
pub type GetSecurityByNameFn =
    unsafe extern "system" fn(*mut FspFileSystem, PWSTR, *mut u32, PVOID, *mut SIZE_T) -> NTSTATUS;
pub type CreateFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PWSTR,
    u32,
    u32,
    u32,
    PVOID,
    u64,
    *mut PVOID,
    *mut FileInfo,
) -> NTSTATUS;
pub type OpenFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PWSTR,
    u32,
    u32,
    *mut PVOID,
    *mut FileInfo,
) -> NTSTATUS;
pub type OverwriteFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PVOID,
    u32,
    BOOLEAN,
    u64,
    *mut FileInfo,
) -> NTSTATUS;
pub type CleanupFn = unsafe extern "system" fn(*mut FspFileSystem, PVOID, PWSTR, u32);
pub type CloseFn = unsafe extern "system" fn(*mut FspFileSystem, PVOID);
pub type ReadFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, PVOID, u64, ULONG, *mut ULONG) -> NTSTATUS;
pub type WriteFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PVOID,
    PVOID,
    u64,
    ULONG,
    BOOLEAN,
    BOOLEAN,
    *mut ULONG,
    *mut FileInfo,
) -> NTSTATUS;
pub type FlushFn = unsafe extern "system" fn(*mut FspFileSystem, PVOID, *mut FileInfo) -> NTSTATUS;
pub type GetFileInfoFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, *mut FileInfo) -> NTSTATUS;
pub type SetBasicInfoFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PVOID,
    u32,
    u64,
    u64,
    u64,
    u64,
    *mut FileInfo,
) -> NTSTATUS;
pub type SetFileSizeFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, u64, BOOLEAN, *mut FileInfo) -> NTSTATUS;
pub type CanDeleteFn = unsafe extern "system" fn(*mut FspFileSystem, PVOID, PWSTR) -> NTSTATUS;
pub type RenameFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, PWSTR, PWSTR, BOOLEAN) -> NTSTATUS;
pub type GetSecurityFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, PVOID, *mut SIZE_T) -> NTSTATUS;
pub type SetSecurityFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, SECURITY_INFORMATION, PVOID) -> NTSTATUS;
pub type ReadDirectoryFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PVOID,
    PWSTR,
    PWSTR,
    PVOID,
    ULONG,
    *mut ULONG,
) -> NTSTATUS;
pub type GetStreamInfoFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, PVOID, ULONG, *mut ULONG) -> NTSTATUS;
pub type GetDirInfoByNameFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, PWSTR, *mut DirInfo) -> NTSTATUS;
pub type ControlFn = unsafe extern "system" fn(
    *mut FspFileSystem,
    PVOID,
    u32,
    PVOID,
    ULONG,
    PVOID,
    ULONG,
    *mut ULONG,
) -> NTSTATUS;
pub type SetDeleteFn =
    unsafe extern "system" fn(*mut FspFileSystem, PVOID, PWSTR, BOOLEAN) -> NTSTATUS;
pub type DispatcherStoppedFn = unsafe extern "system" fn(*mut FspFileSystem, BOOLEAN);

/// FSP_FILE_SYSTEM_INTERFACE（64 个函数指针，x64 上是 512 字节）
#[repr(C)]
pub struct FileSystemInterface {
    pub GetVolumeInfo: Option<GetVolumeInfoFn>,
    pub SetVolumeLabel: Option<SetVolumeLabelFn>,
    pub GetSecurityByName: Option<GetSecurityByNameFn>,
    pub Create: Option<CreateFn>,
    pub Open: Option<OpenFn>,
    pub Overwrite: Option<OverwriteFn>,
    pub Cleanup: Option<CleanupFn>,
    pub Close: Option<CloseFn>,
    pub Read: Option<ReadFn>,
    pub Write: Option<WriteFn>,
    pub Flush: Option<FlushFn>,
    pub GetFileInfo: Option<GetFileInfoFn>,
    pub SetBasicInfo: Option<SetBasicInfoFn>,
    pub SetFileSize: Option<SetFileSizeFn>,
    pub CanDelete: Option<CanDeleteFn>,
    pub Rename: Option<RenameFn>,
    pub GetSecurity: Option<GetSecurityFn>,
    pub SetSecurity: Option<SetSecurityFn>,
    pub ReadDirectory: Option<ReadDirectoryFn>,
    pub ResolveReparsePoints: Option<unsafe extern "system" fn()>,
    pub GetReparsePoint: Option<unsafe extern "system" fn()>,
    pub SetReparsePoint: Option<unsafe extern "system" fn()>,
    pub DeleteReparsePoint: Option<unsafe extern "system" fn()>,
    pub GetStreamInfo: Option<GetStreamInfoFn>,
    pub GetDirInfoByName: Option<GetDirInfoByNameFn>,
    pub Control: Option<ControlFn>,
    pub SetDelete: Option<SetDeleteFn>,
    pub CreateEx: Option<unsafe extern "system" fn()>,
    pub OverwriteEx: Option<unsafe extern "system" fn()>,
    pub GetEa: Option<unsafe extern "system" fn()>,
    pub SetEa: Option<unsafe extern "system" fn()>,
    pub Obsolete0: Option<unsafe extern "system" fn()>,
    pub DispatcherStopped: Option<DispatcherStoppedFn>,
    pub Reserved: [Option<unsafe extern "system" fn()>; 31],
}

const _: () = assert!(std::mem::size_of::<FileSystemInterface>() == 64 * 8);

/// FSP_FILE_SYSTEM：只当作不透明句柄用
#[repr(C)]
pub struct FspFileSystem {
    _opaque: [u8; 0],
}

// --------------------------------------------------------------- 动态加载
type CreateFsFn = unsafe extern "system" fn(
    PWSTR,
    *const VolumeParams,
    *const FileSystemInterface,
    *mut *mut FspFileSystem,
) -> NTSTATUS;
type SetMountPointFn = unsafe extern "system" fn(*mut FspFileSystem, PWSTR) -> NTSTATUS;
type VoidFsFn = unsafe extern "system" fn(*mut FspFileSystem);
type StartDispatcherFn = unsafe extern "system" fn(*mut FspFileSystem, ULONG) -> NTSTATUS;
type AcquireDirBufferFn = unsafe extern "system" fn(*mut PVOID, BOOLEAN, ULONG, *mut NTSTATUS);
type FillDirBufferFn =
    unsafe extern "system" fn(*mut PVOID, *mut DirInfo, *mut NTSTATUS) -> BOOLEAN;
type ReadDirBufferFn = unsafe extern "system" fn(*mut PVOID, PWSTR, PVOID, ULONG, *mut ULONG);
type DirBufferFn = unsafe extern "system" fn(*mut PVOID);

/// 已解析的函数表
pub struct Api {
    pub create: CreateFsFn,
    pub set_mount_point: SetMountPointFn,
    pub remove_mount_point: VoidFsFn,
    pub start_dispatcher: StartDispatcherFn,
    pub stop_dispatcher: VoidFsFn,
    pub delete: VoidFsFn,
    pub acquire_dir_buffer: AcquireDirBufferFn,
    pub fill_dir_buffer: FillDirBufferFn,
    pub read_dir_buffer: ReadDirBufferFn,
    pub release_dir_buffer: DirBufferFn,
    pub delete_dir_buffer: DirBufferFn,
    /// DLL 路径（诊断信息用）
    pub dll_path: String,
}

// SAFETY: 函数指针指向 DLL 中的代码，DLL 被 Windows 引用计数持有，
// 进程存活期间一直有效；跨线程调用是 WinFsp 设计的正常用法。
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// 取（并缓存）WinFsp 函数表；未安装时返回可诊断的错误信息
pub fn api() -> Result<&'static Api, String> {
    match API.get_or_init(load) {
        Ok(api) => Ok(api),
        Err(e) => Err(e.clone()),
    }
}

/// WinFsp 运行时是否可用（GUI 用它决定是否禁用"挂载"按钮并显示安装指引）
pub fn is_available() -> bool {
    api().is_ok()
}

/// 动态库候选路径
fn dll_candidates() -> Vec<String> {
    let mut out = Vec::new();
    // 1) 注册表里的安装目录最权威
    if let Some(dir) = registry_install_dir() {
        out.push(format!("{dir}\\bin\\winfsp-x64.dll"));
        out.push(format!("{dir}\\bin\\winfsp-a64.dll"));
    }
    // 2) 默认安装位置
    for env in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(env) {
            out.push(format!("{base}\\WinFsp\\bin\\winfsp-x64.dll"));
        }
    }
    // 3) 交给系统搜索（PATH 里可能有）
    out.push("winfsp-x64.dll".to_string());
    out
}

fn registry_install_dir() -> Option<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for sub in [r"SOFTWARE\WOW6432Node\WinFsp", r"SOFTWARE\WinFsp"] {
        if let Ok(key) = hklm.open_subkey_with_flags(sub, KEY_READ) {
            if let Ok(dir) = key.get_value::<String, _>("InstallDir") {
                let dir = dir.trim_end_matches('\\').to_string();
                if !dir.is_empty() {
                    return Some(dir);
                }
            }
        }
    }
    None
}

fn load() -> Result<Api, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    let candidates = dll_candidates();
    let mut last_err = String::new();
    for path in &candidates {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            last_err = format!("无法加载 {path}");
            continue;
        }
        // 句柄永不释放：进程生命周期内一直需要这些函数指针
        let handle = std::mem::ManuallyDrop::new(handle);
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let mut name: Vec<u8> = $name.as_bytes().to_vec();
                name.push(0);
                match unsafe { GetProcAddress(*handle, name.as_ptr()) } {
                    Some(f) => unsafe { std::mem::transmute_copy::<_, $ty>(&f) },
                    None => return Err(format!("WinFsp DLL 缺少导出函数 {}（{path}）", $name)),
                }
            }};
        }
        return Ok(Api {
            create: sym!("FspFileSystemCreate", CreateFsFn),
            set_mount_point: sym!("FspFileSystemSetMountPoint", SetMountPointFn),
            remove_mount_point: sym!("FspFileSystemRemoveMountPoint", VoidFsFn),
            start_dispatcher: sym!("FspFileSystemStartDispatcher", StartDispatcherFn),
            stop_dispatcher: sym!("FspFileSystemStopDispatcher", VoidFsFn),
            delete: sym!("FspFileSystemDelete", VoidFsFn),
            acquire_dir_buffer: sym!("FspFileSystemAcquireDirectoryBufferEx", AcquireDirBufferFn),
            fill_dir_buffer: sym!("FspFileSystemFillDirectoryBuffer", FillDirBufferFn),
            read_dir_buffer: sym!("FspFileSystemReadDirectoryBuffer", ReadDirBufferFn),
            release_dir_buffer: sym!("FspFileSystemReleaseDirectoryBuffer", DirBufferFn),
            delete_dir_buffer: sym!("FspFileSystemDeleteDirectoryBuffer", DirBufferFn),
            dll_path: path.clone(),
        });
    }
    Err(format!(
        "未找到 WinFsp 运行时（尝试过：{}）。最后一次错误：{last_err}",
        candidates.join("、")
    ))
}

// ------------------------------------------------------------------ 工具
/// UTF-8 → 以 0 结尾的 UTF-16
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把 WinFsp 传进来的 PWSTR 读成 String（非法 UTF-16 用替换字符兜底，绝不 panic）
///
/// # Safety
/// 指针必须是 WinFsp 给出的、以 0 结尾的合法 UTF-16 串。
pub unsafe fn from_wide(ptr: PWSTR) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
        if len > 32768 {
            // 防御性的硬上限：MAX_PATH 是 260，WinFsp 的路径上限远小于这个值
            break;
        }
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr as *const u16, len))
}

/// 把一段文件名写进 `DirInfo` 的变长布局里
///
/// 返回的 Vec 前 104 字节是定长头，之后是 UTF-16 名字（不含结尾 0，
/// 与 Size 字段的定义一致）。
pub fn make_dir_info(info: &FileInfo, name: &str) -> Vec<u8> {
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    let mut buf = vec![0u8; DIR_INFO_NAME_OFFSET + name_utf16.len() * 2 + 2];
    let head = DirInfo {
        Size: (DIR_INFO_NAME_OFFSET + name_utf16.len() * 2) as u16,
        FileInfo: *info,
        NextOffset: 0,
        Padding: [0; 16],
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            &head as *const DirInfo as *const u8,
            buf.as_mut_ptr(),
            DIR_INFO_NAME_OFFSET,
        );
    }
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(name_utf16.as_ptr() as *const u8, name_utf16.len() * 2)
    };
    buf[DIR_INFO_NAME_OFFSET..DIR_INFO_NAME_OFFSET + bytes.len()].copy_from_slice(bytes);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_params_layout_matches_winfsp_2_1() {
        assert_eq!(std::mem::size_of::<VolumeParams>(), 504);
        assert_eq!(std::mem::size_of::<VolumeInfo>(), 88);
        assert_eq!(std::mem::size_of::<FileInfo>(), 72);
        assert_eq!(std::mem::size_of::<DirInfo>(), 104);
        assert_eq!(std::mem::size_of::<FileSystemInterface>(), 512);
    }

    #[test]
    fn dir_info_carries_the_name_without_terminator() {
        let info = FileInfo {
            FileAttributes: FILE_ATTRIBUTE_ARCHIVE,
            FileSize: 42,
            ..Default::default()
        };
        let buf = make_dir_info(&info, "测试.txt");
        let head = unsafe { &*(buf.as_ptr() as *const DirInfo) };
        assert_eq!(
            head.Size as usize,
            DIR_INFO_NAME_OFFSET + "测试.txt".encode_utf16().count() * 2
        );
        let name_bytes = &buf[DIR_INFO_NAME_OFFSET
            ..DIR_INFO_NAME_OFFSET + head.Size as usize - DIR_INFO_NAME_OFFSET];
        let name: Vec<u16> = name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(String::from_utf16(&name).unwrap(), "测试.txt");
        assert_eq!(head.FileInfo.FileSize, 42);
        // 尾随的 0 不在 Size 里，但内存上确实留了位置
        assert_eq!(&buf[DIR_INFO_NAME_OFFSET + name_bytes.len()..], &[0u8, 0u8]);
    }

    #[test]
    fn runtime_can_be_probed() {
        // 这台机器装了 WinFsp 就应当能加载；没装则必须给出可读的错误而不是 panic
        match api() {
            Ok(a) => {
                assert!(a.dll_path.contains("winfsp"), "{}", a.dll_path);
                assert!(is_available());
            }
            Err(e) => {
                assert!(e.contains("WinFsp"), "错误信息必须可诊断：{e}");
            }
        }
    }

    #[test]
    fn wide_roundtrip() {
        let s = to_wide("天翼云盘 X:");
        let back = unsafe { from_wide(s.as_ptr() as PWSTR) };
        assert_eq!(back, "天翼云盘 X:");
        assert_eq!(unsafe { from_wide(std::ptr::null_mut()) }, "");
    }
}
