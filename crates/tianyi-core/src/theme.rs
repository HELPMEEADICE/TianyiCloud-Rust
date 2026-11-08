//! 主题探测：读取系统级“深色外观”偏好
//!
//! 只做 best effort：任何平台上的任何错误都退化为 `false`（浅色），
//! 绝不 panic，也不会长时间阻塞调用线程（macOS 分支最多等待约 1 秒）。

/// 系统是否偏好深色主题（best effort，跨平台）
///
/// - Windows：读注册表 `HKCU\...\Themes\Personalize` 的 `AppsUseLightTheme`
///   （DWORD，0 表示深色）。键不存在时返回 `false`。
/// - macOS：`defaults read -g AppleInterfaceStyle`，输出为 `Dark`（忽略大小写）
///   时为深色；命令超时或失败时返回 `false`。
/// - 其他平台：`GTK_THEME` 含 `dark` 时为深色；否则看 `COLORFGBG` 的最后一段
///   是否 `< 8`（终端背景色索引，0..7 为暗色）。
pub fn system_prefers_dark() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows_prefers_dark()
    }

    #[cfg(target_os = "macos")]
    {
        macos_prefers_dark()
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        prefers_dark_from_env(
            std::env::var("GTK_THEME").ok().as_deref(),
            std::env::var("COLORFGBG").ok().as_deref(),
        )
    }
}

/// 读取注册表里的应用外观偏好（0 = 深色）
#[cfg(target_os = "windows")]
fn windows_prefers_dark() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .ok()
        .and_then(|key| key.get_value::<u32, _>("AppsUseLightTheme").ok())
        .map(|apps_use_light| apps_use_light == 0)
        .unwrap_or(false)
}

/// 通过 `defaults` 读取 macOS 外观设置，最多等待 1 秒
#[cfg(target_os = "macos")]
fn macos_prefers_dark() -> bool {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    // 放到独立线程里执行：`Command::output()` 本身没有超时，
    // 万一 `defaults` 卡住，主线程也只会等 1 秒。
    std::thread::spawn(move || {
        let dark = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .eq_ignore_ascii_case("Dark")
            })
            .unwrap_or(false);
        let _ = tx.send(dark);
    });
    rx.recv_timeout(Duration::from_millis(1000))
        .unwrap_or(false)
}

/// 纯函数：按环境变量判断深色偏好（Linux 等平台的实现，也便于单测）
#[cfg_attr(any(target_os = "windows", target_os = "macos"), allow(dead_code))]
fn prefers_dark_from_env(gtk_theme: Option<&str>, colorfgbg: Option<&str>) -> bool {
    if let Some(gtk) = gtk_theme {
        if gtk.to_ascii_lowercase().contains("dark") {
            return true;
        }
    }
    if let Some(cfg) = colorfgbg {
        // 形如 "15;0"（fg;bg）或 "0;default;15"，取最后一段
        if let Some(last) = cfg.rsplit([';', ':']).next() {
            if let Ok(bg) = last.trim().parse::<u32>() {
                return bg < 8;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn test_prefers_dark_from_env() {
        // GTK_THEME 优先
        assert!(prefers_dark_from_env(Some("Adwaita:dark"), None));
        assert!(prefers_dark_from_env(Some("Arc-Dark"), Some("0;15")));
        // GTK_THEME 不是深色时看 COLORFGBG
        assert!(prefers_dark_from_env(Some("Adwaita"), Some("15;0")));
        assert!(prefers_dark_from_env(None, Some("0;default;7")));
        assert!(!prefers_dark_from_env(Some("Adwaita"), Some("0;15")));
        assert!(!prefers_dark_from_env(None, Some("0;default;8")));
        // 什么都没有 / 无法解析 → 浅色
        assert!(!prefers_dark_from_env(None, None));
        assert!(!prefers_dark_from_env(Some("Adwaita"), Some("garbage")));
        assert!(!prefers_dark_from_env(Some(""), Some("")));
    }

    /// 真实平台探测：只要求不 panic、能在 1 秒量级返回
    #[test]
    fn test_system_prefers_dark_never_panics() {
        let start = Instant::now();
        let dark = system_prefers_dark();
        let elapsed = start.elapsed();
        // 两个分支都只是读注册表/等一个带超时的子进程
        assert!(
            elapsed < Duration::from_secs(3),
            "system_prefers_dark 不应阻塞，实际耗时 {elapsed:?}"
        );
        // 返回值只做类型层面的确认（具体值取决于机器设置）
        let _: bool = dark;
    }
}
