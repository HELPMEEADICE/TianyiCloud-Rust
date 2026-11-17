//! 清理工具：把本机残留的 WinFsp 网络驱动器痕迹全部删掉
//!
//! 用途：进程被强杀 / 调试中途退出之后，「此电脑 → 网络位置」里会留下一排红叉的
//! 僵尸网络盘。正常卸载时程序会自己清理（见 win::netuse），本工具用于事后收尾：
//!
//!   cargo run -p tianyi-mount --example netdrive_purge          # 只清本程序留下的挂载
//!   cargo run -p tianyi-mount --example netdrive_purge -- --all # 同时清空 Shell 的网络位置缓存
//!
//! 每清一个都会打印结果；单项失败不影响其它条目。

use tianyi_mount::win::netuse;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let purge_shell = std::env::args().any(|a| a == "--all");

    let mut cleaned = 0usize;
    for letter in b'D'..=b'Z' {
        let local = format!("{}:", letter as char);
        let Some(target) = netuse::dos_device_target(&local) else {
            continue;
        };
        if !netuse::target_is_cloud_volume(&target) {
            continue;
        }
        let unc = netuse::unc_from_dos_target(&target);
        println!("清理 {local}（目标 {target}，UNC {unc}）");
        netuse::cleanup(&local, &unc);
        cleaned += 1;
    }

    if purge_shell {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2";
        if let Ok(key) = hkcu.open_subkey_with_flags(path, KEY_ALL_ACCESS) {
            let names: Vec<String> = key
                .enum_keys()
                .filter_map(|k| k.ok())
                .filter(|n| n.starts_with("##"))
                .collect();
            for name in names {
                if hkcu.delete_subkey_all(format!("{path}\\{name}")).is_ok() {
                    println!("删除网络位置缓存 {name}");
                    cleaned += 1;
                }
            }
        }
    }

    println!("完成：清理 {cleaned} 项");
}
