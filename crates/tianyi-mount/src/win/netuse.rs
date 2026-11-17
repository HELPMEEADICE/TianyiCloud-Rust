//! 退出清理：把挂载期间在系统里留下的「网络驱动器」痕迹全部抹掉
//!
//! WinFsp 的网络文件系统挂到盘符上之后，系统里有**三处**痕迹，缺一个就会在
//! 资源管理器「此电脑 → 网络位置」里留下一个红叉的僵尸图标：
//!
//! 1. **DOS 设备映射**（QueryDosDeviceW / DefineDosDeviceW）：盘符 X: →
//!    \Device\Volume{...}\Server\Share。进程被强杀、或 RemoveMountPoint 没跑到时，
//!    这个映射会留下来，Explorer 于是显示一个打不开的网络盘。
//! 2. **网络连接表**（WNetCancelConnection2W）：「net use X: /delete」对 WinFsp.Np 的连接
//!    不起作用（它不是 LanmanWorkstation 的连接，会返回 2250），必须走 WNet 或直接删
//!    DOS 设备映射。
//! 3. **Shell 的网络位置缓存**（HKCU\...\Explorer\MountPoints2\##Server#Share）：
//!    浏览过 UNC 之后 Explorer 会记住它，即使卷已经没了也照样显示。
//!
//! 本模块只做 best-effort 清理：每一步独立成败、只记日志，绝不让清理失败影响
//! 卸载本身的返回值（用户已经要求卸载了，清理只是收尾）。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// 让一段字符串变成以 0 结尾的宽字符
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 盘符当前的 DOS 设备目标（例如 \Device\Volume{...}\TianyiCloud-x\Personal）
pub fn dos_device_target(local: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let name = wide(local);
    let mut buf = vec![0u16; 2048];
    let n = unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(n as usize);
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// 删除某盘符的全部 DOS 设备定义（只删盘符映射，不动卷本身）
pub fn remove_dos_device(local: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{
        DefineDosDeviceW, DDD_NO_BROADCAST_SYSTEM, DDD_REMOVE_DEFINITION,
    };
    let name = wide(local);
    let ok = unsafe {
        DefineDosDeviceW(
            DDD_REMOVE_DEFINITION | DDD_NO_BROADCAST_SYSTEM,
            name.as_ptr(),
            std::ptr::null(),
        )
    };
    ok != 0
}

/// 取消一条网络连接（盘符或 UNC 都可以），返回 Win32 错误码（0 = 成功）
pub fn cancel_network_connection(path: &str) -> u32 {
    use windows_sys::Win32::NetworkManagement::WNet::WNetCancelConnection2W;
    let p = wide(path);
    unsafe { WNetCancelConnection2W(p.as_ptr(), 0, 1) }
}

/// UNC 路径 → Shell 网络位置缓存键名（\\Server\Share → ##Server#Share）
pub fn shell_location_key(unc: &str) -> Option<String> {
    let trimmed = unc.trim_start_matches('\\');
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, '\\');
    let server = parts.next().unwrap_or_default();
    let share = parts.next().unwrap_or_default();
    if server.is_empty() {
        return None;
    }
    if share.is_empty() {
        Some(format!("##{server}"))
    } else {
        Some(format!("##{server}#{share}"))
    }
}

/// 删掉 Explorer 记住的那个网络位置（只删我们自己创建的那一个）
pub fn forget_shell_network_location(unc: &str) -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let Some(key) = shell_location_key(unc) else {
        return false;
    };
    let path = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\{key}");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.delete_subkey_all(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            log::debug!("删除网络位置缓存 {key} 失败：{e}");
            false
        }
    }
}

/// 一次性清掉一个挂载留下的全部痕迹（幂等，可在卸载后、退出前、下次挂载前反复调用）
///
/// * local：盘符形式，例如 "X:"
/// * unc：卷前缀，例如 "\\TianyiCloud-user\\Personal"（反斜杠开头、不带尾部斜杠）
pub fn cleanup(local: &str, unc: &str) {
    // 1. 先取消网络连接（盘符 + UNC 两种形式都试；不在连接表里会返回 2250，忽略）
    let r1 = cancel_network_connection(local);
    let unc_path = format!("\\{}", unc.trim_start_matches('\\'));
    let r2 = cancel_network_connection(&unc_path);
    if r1 == 0 || r2 == 0 {
        log::debug!("已取消网络连接 {local} / {unc_path}");
    }

    // 2. 再删 DOS 设备映射：上面若因进程已被强杀而没生效，这一步保证盘符消失
    if dos_device_target(local).is_some() && remove_dos_device(local) {
        log::info!("已清除盘符映射 {local}");
    }

    // 3. 最后删 Shell 的网络位置缓存，避免「此电脑」里留一个红叉图标
    forget_shell_network_location(unc);
}

/// DOS 设备目标是否像「本程序挂出来的网络卷」
///
/// WinFsp 网络卷的目标形如 \Device\Volume{guid}\TianyiCloud-<账号>\Personal；
/// 本地盘是 \Device\HarddiskVolumeN，SMB 映射是 \Device\LanmanRedirector\...。
/// 因此判定条件：必须以 \Device\Volume{ 开头，且服务器名段以 Tianyi 开头。
pub fn target_is_cloud_volume(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    if !lower.starts_with("\\device\\volume{") {
        return false;
    }
    let mut parts = target.trim_start_matches('\\').split('\\');
    let _device = parts.next();
    let _volume = parts.next();
    matches!(parts.next(), Some(server) if server.to_ascii_lowercase().starts_with("tianyi"))
}

/// 从 DOS 设备目标里取出 UNC 前缀（\Device\Volume{guid}\Server\Share → \Server\Share）
pub fn unc_from_dos_target(target: &str) -> String {
    let mut parts = target.trim_start_matches('\\').splitn(3, '\\');
    let _device = parts.next();
    let _volume = parts.next();
    match parts.next() {
        Some(rest) if !rest.is_empty() => format!("\\{}", rest),
        _ => String::new(),
    }
}

/// 清理注册表/快捷方式里其它可能残留「网络驱动器」的位置（best-effort）
///
/// 不同 Windows 版本与挂载方式会把映射记在不同地方，逐个清比猜一个可靠：
/// * HKCU\Network\<盘符>：持久化映射（net use /persistent 写这里）；
/// * %APPDATA%\Microsoft\Windows\Network Shortcuts\*.lnk：「此电脑 → 网络位置」的快捷方式；
/// * Explorer\Map Network Drive MRU：映射对话框的历史记录。
pub fn purge_shell_traces() -> usize {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
    use winreg::RegKey;
    let mut removed = 0usize;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    if let Ok(net) = hkcu.open_subkey_with_flags("Network", KEY_ALL_ACCESS) {
        let names: Vec<String> = net.enum_keys().filter_map(|k| k.ok()).collect();
        for name in names {
            if hkcu.delete_subkey_all(format!("Network\\{name}")).is_ok() {
                log::info!("删除持久化映射 HKCU\\Network\\{name}");
                removed += 1;
            }
        }
    }

    if let Some(appdata) = dirs::data_dir() {
        let dir = appdata
            .join("Microsoft")
            .join("Windows")
            .join("Network Shortcuts");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let path = e.path();
                let is_lnk = path
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("lnk"))
                    == Some(true);
                if is_lnk {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
                    if std::fs::remove_file(&path).is_ok() {
                        log::info!("删除网络位置快捷方式 {:?}", name);
                        removed += 1;
                    }
                }
            }
        }
    }

    if let Ok(mru) = hkcu.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\Map Network Drive MRU",
        KEY_ALL_ACCESS,
    ) {
        let names: Vec<String> = mru
            .enum_values()
            .filter_map(|v| v.ok())
            .map(|(n, _)| n)
            .collect();
        for name in names {
            if name.eq_ignore_ascii_case("MRUList") {
                continue;
            }
            if mru.delete_value(&name).is_ok() {
                removed += 1;
            }
        }
    }

    removed
}

/// 挂载前清扫上一次同名残留（进程被强杀后的典型情况）
///
/// 不清的后果很具体：同名 UNC 卷已存在 → FspFileSystemCreate 返回 ERROR_FILE_EXISTS
/// （0x80070050），用户只看到「挂载失败」却不知道为什么。
pub fn cleanup_before_mount(local: &str, unc: &str) {
    if let Some(target) = dos_device_target(local) {
        if target_is_cloud_volume(&target) {
            log::warn!("盘符 {local} 上有上次遗留的挂载（{target}），先清理");
            cleanup(local, unc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_location_key_matches_explorer_convention() {
        assert_eq!(
            shell_location_key(r"\\TianyiCloud-alice\Personal").as_deref(),
            Some("##TianyiCloud-alice#Personal")
        );
        assert_eq!(
            shell_location_key(r"\TianyiCloud-alice\Personal").as_deref(),
            Some("##TianyiCloud-alice#Personal"),
            "单个前导反斜杠也要能解析"
        );
        assert_eq!(
            shell_location_key(r"\\OnlyServer").as_deref(),
            Some("##OnlyServer")
        );
        assert_eq!(shell_location_key(""), None);
    }

    #[test]
    fn c_drive_always_has_a_dos_device_mapping() {
        let c = dos_device_target("C:");
        assert!(c.is_some(), "C: 必须有 DOS 设备映射");
        assert!(c.unwrap().to_ascii_uppercase().contains("DEVICE"));
        // 不存在的盘符返回 None 而不是 panic
        assert!(dos_device_target("Q:").is_none());
    }

    #[test]
    fn cancelling_a_non_connection_is_harmless() {
        let rc = cancel_network_connection(r"\\NoSuchServer-zzz\NoSuchShare");
        assert_ne!(rc, 0);
    }
}
