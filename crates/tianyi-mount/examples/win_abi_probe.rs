//! WinFsp ABI 诊断：逐个组合尝试 FspFileSystemCreate，打印 NTSTATUS 与 GetLastError
//!
//! 只在排查"为什么建卷失败"时使用：cargo run -p tianyi-mount --example win_abi_probe

use tianyi_mount::win::sys as s;

fn main() {
    let api = match s::api() {
        Ok(a) => a,
        Err(e) => {
            println!("加载 WinFsp 失败：{e}");
            return;
        }
    };
    println!("DLL: {}", api.dll_path);

    let cases: Vec<(&str, Option<&str>)> = vec![
        ("WinFsp.Disk", None),
        ("\\\\.\\WinFsp.Disk", None),
        ("WinFsp.Net", None),
        ("WinFsp.Net", Some("\\TianyiProbeA\\Share")),
        ("\\\\.\\WinFsp.Net", Some("\\TianyiProbeB\\Share")),
    ];

    for (device, prefix) in cases {
        let mut params = make_params(prefix);
        let iface = make_iface();
        let device_wide = s::to_wide(device);
        let mut fs: *mut s::FspFileSystem = std::ptr::null_mut();
        let status = unsafe {
            (api.create)(
                device_wide.as_ptr() as s::PWSTR,
                &mut params,
                &iface,
                &mut fs,
            )
        };
        let last = std::io::Error::last_os_error();
        println!(
            "device={device:<22} prefix={:<22} -> NTSTATUS 0x{status:08X} fs_null={} lastError={last}",
            prefix.unwrap_or("<none>"),
            fs.is_null()
        );
        if status == s::STATUS_SUCCESS && !fs.is_null() {
            unsafe { (api.delete)(fs) };
        }
    }
}

fn make_params(prefix: Option<&str>) -> s::VolumeParams {
    let mut p = s::VolumeParams {
        Version: 0,
        SectorSize: 4096,
        SectorsPerAllocationUnit: 1,
        MaxComponentLength: 255,
        VolumeCreationTime: 0,
        VolumeSerialNumber: 0x5459_434C,
        TransactTimeout: 0,
        IrpTimeout: 0,
        IrpCapacity: 0,
        FileInfoTimeout: 1000,
        Flags: s::VP_FLAG_CASE_PRESERVED_NAMES
            | s::VP_FLAG_UNICODE_ON_DISK
            | s::VP_FLAG_READ_ONLY_VOLUME
            | s::VP_FLAG_ALWAYS_USE_DOUBLE_BUFFERING,
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
    if let Some(pfx) = prefix {
        let w: Vec<u16> = pfx.encode_utf16().take(191).collect();
        p.Prefix[..w.len()].copy_from_slice(&w);
    }
    let name: Vec<u16> = "TianyiCloud".encode_utf16().take(15).collect();
    p.FileSystemName[..name.len()].copy_from_slice(&name);
    p
}

fn make_iface() -> s::FileSystemInterface {
    s::FileSystemInterface {
        GetVolumeInfo: None,
        SetVolumeLabel: None,
        GetSecurityByName: None,
        Create: None,
        Open: None,
        Overwrite: None,
        Cleanup: None,
        Close: None,
        Read: None,
        Write: None,
        Flush: None,
        GetFileInfo: None,
        SetBasicInfo: None,
        SetFileSize: None,
        CanDelete: None,
        Rename: None,
        GetSecurity: None,
        SetSecurity: None,
        ReadDirectory: None,
        ResolveReparsePoints: None,
        GetReparsePoint: None,
        SetReparsePoint: None,
        DeleteReparsePoint: None,
        GetStreamInfo: None,
        GetDirInfoByName: None,
        Control: None,
        SetDelete: None,
        CreateEx: None,
        OverwriteEx: None,
        GetEa: None,
        SetEa: None,
        Obsolete0: None,
        DispatcherStopped: None,
        Reserved: [None; 31],
    }
}
