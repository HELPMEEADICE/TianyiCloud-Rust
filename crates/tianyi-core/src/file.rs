//! 文件类型识别与预览辅助

use std::path::Path;

/// 根据扩展名判断文件类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Image,
    Video,
    Audio,
    Pdf,
    Text,
    Archive,
    Torrent,
    Office,
    Other,
}

impl FileKind {
    pub fn label(&self) -> &'static str {
        match self {
            FileKind::Image => "图片",
            FileKind::Video => "视频",
            FileKind::Audio => "音频",
            FileKind::Pdf => "PDF",
            FileKind::Text => "文本",
            FileKind::Archive => "压缩包",
            FileKind::Torrent => "BT种子",
            FileKind::Office => "Office",
            FileKind::Other => "其他",
        }
    }

    /// 是否支持内置预览
    pub fn builtin_preview(&self) -> bool {
        matches!(self, FileKind::Image | FileKind::Audio | FileKind::Text)
    }
}

/// 图片扩展名集合
const IMAGE_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic",
];
/// 视频扩展名集合
const VIDEO_EXT: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "rmvb",
];
/// 音频扩展名集合
const AUDIO_EXT: &[&str] = &[
    "mp3", "wav", "flac", "aac", "ogg", "m4a", "opus", "wma", "ape", "alac",
];
/// 文本扩展名集合
const TEXT_EXT: &[&str] = &[
    "txt", "md", "log", "json", "xml", "yml", "yaml", "toml", "ini", "conf", "cfg", "csv", "tsv",
    "srt", "vtt", "sh", "bat", "py", "rs", "go", "c", "h", "cpp", "java", "js", "ts", "html",
    "css", "sql", "php", "rb", "kt",
];
/// 压缩包扩展名集合
const ARCHIVE_EXT: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "tgz", "zst"];
/// Office 扩展名集合
const OFFICE_EXT: &[&str] = &[
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp",
];

/// 根据文件名判断类别
pub fn classify(name: &str) -> FileKind {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "torrent" {
        return FileKind::Torrent;
    }
    if IMAGE_EXT.contains(&ext.as_str()) {
        return FileKind::Image;
    }
    if VIDEO_EXT.contains(&ext.as_str()) {
        return FileKind::Video;
    }
    if AUDIO_EXT.contains(&ext.as_str()) {
        return FileKind::Audio;
    }
    if ext == "pdf" {
        return FileKind::Pdf;
    }
    if TEXT_EXT.contains(&ext.as_str()) {
        return FileKind::Text;
    }
    if ARCHIVE_EXT.contains(&ext.as_str()) {
        return FileKind::Archive;
    }
    if OFFICE_EXT.contains(&ext.as_str()) {
        return FileKind::Office;
    }
    FileKind::Other
}

/// 获取 MIME 类型
pub fn mime_type(name: &str) -> &'static str {
    match classify(name) {
        FileKind::Image => "image/*",
        FileKind::Video => "video/*",
        FileKind::Audio => "audio/*",
        FileKind::Pdf => "application/pdf",
        FileKind::Text => "text/plain",
        FileKind::Torrent => "application/x-bittorrent",
        _ => "application/octet-stream",
    }
}

/// 使用默认播放器以链接方式流式播放媒体文件
///
/// 优先在 Windows 上解析注册表中该扩展名关联的播放器命令并直接启动，
/// 将下载直链作为参数传入（避免整文件下载后再播放）。
/// 找不到播放器（或非 Windows 平台）时回退为把链接写入临时 `.m3u`
/// 播放列表，再交给系统按文件关联打开，PotPlayer / VLC / MPC-HC 等
/// 均支持流式播放 m3u 中的远程链接。
pub fn open_media_url_with_player(name: &str, url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if let Some((prog, args)) = windows_player_command(name) {
            let args: Vec<String> = args
                .into_iter()
                .map(|a| a.replace("%1", url).replace("%L", url))
                .collect();
            let mut cmd = std::process::Command::new(&prog);
            cmd.args(&args);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            return cmd
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("launch player: {e}"));
        }
        open_media_via_m3u(name, url)
    }

    #[cfg(not(target_os = "windows"))]
    {
        open_media_via_m3u(name, url)
    }
}

/// 通过临时 .m3u 播放列表把远程链接交给默认播放器
fn open_media_via_m3u(name: &str, url: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join("tianyi-playlist");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create playlist dir: {e}"))?;
    let title = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    let m3u = dir.join(format!("{title}.m3u"));
    let content = format!("#EXTM3U\n#EXTINF:-1,{title}\n{url}\n");
    std::fs::write(&m3u, content).map_err(|e| format!("write m3u: {e}"))?;
    open_with_system(&m3u)
}

/// 在 Windows 注册表中查找扩展名关联的播放器命令，返回（可执行文件, 参数列表）
#[cfg(target_os = "windows")]
fn windows_player_command(name: &str) -> Option<(String, Vec<String>)> {
    use winreg::enums::*;
    use winreg::RegKey;

    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext.is_empty() {
        return None;
    }
    let ext_key = format!(".{ext}");

    // 1. 用户选择的应用（UserChoice，优先级最高）
    let prog_id = {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext_key}\UserChoice"
        );
        hkcu.open_subkey(path)
            .ok()
            .and_then(|k| k.get_value::<String, _>("ProgId").ok())
    };
    // 2. 回退到 HKCR 扩展名默认值
    let prog_id = prog_id.or_else(|| {
        let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
        hkcr.open_subkey(&ext_key)
            .ok()
            .and_then(|k| k.get_value::<String, _>("").ok())
    });
    let prog_id = prog_id?;

    // 3. ProgId -> shell/open/command（优先 HKCR，回退 HKCU）
    let mut command = {
        let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
        hkcr.open_subkey(&prog_id)
            .and_then(|k| k.open_subkey("shell"))
            .and_then(|k| k.open_subkey("open"))
            .and_then(|k| k.open_subkey("command"))
            .and_then(|k| k.get_value::<String, _>(""))
            .ok()
    };
    if command.is_none() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        command = hkcu
            .open_subkey(&prog_id)
            .and_then(|k| k.open_subkey("shell"))
            .and_then(|k| k.open_subkey("open"))
            .and_then(|k| k.open_subkey("command"))
            .and_then(|k| k.get_value::<String, _>(""))
            .ok();
    }
    let command = command?;

    // 4. 按 Windows 命令行规则拆分（与 CommandLineToArgvW 一致）
    let mut args = split_command_line(&command);
    if args.is_empty() {
        return None;
    }

    // 5. 首个参数为可执行文件；若含 %1/%L 参数则已替换为 URL
    let prog = args.remove(0);
    if prog.is_empty() {
        return None;
    }
    // 展开环境变量（Program Files 等）
    let prog = expand_env(&prog);
    let args = args.into_iter().map(|a| expand_env(&a)).collect();
    Some((prog, args))
}

/// 按 CommandLineToArgvW 规则拆分 Windows 命令行字符串
#[cfg(any(target_os = "windows", test))]
fn split_command_line(command: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            // 统计引号前的连续反斜杠
            let mut n = 0;
            while cur.ends_with('\\') {
                cur.pop();
                n += 1;
            }
            if n % 2 == 1 {
                // 奇数个反斜杠：n/2 个反斜杠 + 1 个字面引号
                for _ in 0..n / 2 {
                    cur.push('\\');
                }
                cur.push('"');
            } else {
                // 偶数个反斜杠：n/2 个反斜杠，引号切换成对与否
                for _ in 0..n / 2 {
                    cur.push('\\');
                }
                if in_quotes && chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
        } else if (c == ' ' || c == '\t') && !in_quotes {
            if !cur.is_empty() {
                args.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

/// 展开路径中的 %VAR% 环境变量
#[cfg(target_os = "windows")]
fn expand_env(s: &str) -> String {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let len = windows_sys_expand(wide.as_ptr());
        if len == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; len as usize];
        windows_sys_expand_buf(wide.as_ptr(), buf.as_mut_ptr());
        String::from_utf16_lossy(&buf[..len as usize - 1])
    }
}

/// 调用 ExpandEnvironmentStringsW（避免 cmd.exe 的 % 转义问题）
#[cfg(target_os = "windows")]
unsafe fn windows_sys_expand(src: *const u16) -> u32 {
    extern "system" {
        fn ExpandEnvironmentStringsW(lpSrc: *const u16, lpDst: *mut u16, nSize: u32) -> u32;
    }
    ExpandEnvironmentStringsW(src, std::ptr::null_mut(), 0)
}

#[cfg(target_os = "windows")]
unsafe fn windows_sys_expand_buf(src: *const u16, dst: *mut u16) {
    extern "system" {
        fn ExpandEnvironmentStringsW(lpSrc: *const u16, lpDst: *mut u16, nSize: u32) -> u32;
    }
    ExpandEnvironmentStringsW(src, dst, 0xFFFF);
}

/// 使用系统默认程序打开文件
pub fn open_with_system(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let path = path.to_string_lossy().replace('/', "\\");
        let cmd = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path])
            .spawn();
        cmd.map(|_| ()).map_err(|e| format!("open file: {e}"))
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open file: {e}"))
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open file: {e}"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        Err("unsupported platform".to_string())
    }
}

/// 格式化字节数为可读字符串
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < 4 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.2} {}", size, UNITS[unit])
    }
}

/// 格式化速度为可读字符串
pub fn format_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", format_size(bytes_per_sec))
}

/// 格式化剩余时间（ETA）
///
/// 速度为 0 或已无剩余字节时返回占位符 `—`（避免出现 “剩余 0 秒” 或除零）。
/// - `< 60s`  → `剩余 42 秒`
/// - `< 3600s` → `剩余 3 分 20 秒`
/// - 其余      → `剩余 1 时 05 分`
pub fn format_eta(remaining_bytes: u64, bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 || remaining_bytes == 0 {
        return "—".to_string();
    }
    // 向上取整：还剩 1 字节但限速 1 B/s 时也应显示 1 秒而不是 0 秒
    let secs = remaining_bytes.div_ceil(bytes_per_sec);
    if secs < 60 {
        format!("剩余 {secs} 秒")
    } else if secs < 3600 {
        format!("剩余 {} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("剩余 {} 时 {:02} 分", secs / 3600, (secs % 3600) / 60)
    }
}

/// 解析 `YYYY-MM-DD` 形式的日期前缀，返回 (年, 月, 日)
fn parse_date_prefix(s: &str) -> Option<(&str, &str, &str)> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (&s[0..4], &s[5..7], &s[8..10]);
    if y.bytes().all(|c| c.is_ascii_digit())
        && m.bytes().all(|c| c.is_ascii_digit())
        && d.bytes().all(|c| c.is_ascii_digit())
    {
        Some((y, m, d))
    } else {
        None
    }
}

/// 格式化接口返回的时间字符串
///
/// 天翼云返回的时间格式为 `2024-05-06 12:03:44` / `2024-05-06T12:03:44` / `2024-05-06`，
/// 统一显示为 `2024/05/06 12:03`（只保留到分钟）或 `2024/05/06`。
/// 空串或无法解析时原样返回，绝不丢字段。
pub fn format_datetime(raw: &str) -> String {
    let s = raw.trim();
    // 只有 ASCII 才走字节切片，避免在非 ASCII 边界上 panic
    if s.is_empty() || !s.is_ascii() {
        return raw.to_string();
    }
    let Some((y, m, d)) = parse_date_prefix(s) else {
        return raw.to_string();
    };
    let date = format!("{y}/{m}/{d}");
    if s.len() == 10 {
        return date;
    }
    let sep = s.as_bytes()[10];
    if sep != b' ' && sep != b'T' {
        return raw.to_string();
    }
    let time = &s[11..];
    if time.len() < 5 || time.as_bytes()[2] != b':' {
        return raw.to_string();
    }
    let (hh, mm) = (&time[0..2], &time[3..5]);
    if !hh.bytes().all(|c| c.is_ascii_digit()) || !mm.bytes().all(|c| c.is_ascii_digit()) {
        return raw.to_string();
    }
    format!("{date} {hh}:{mm}")
}

/// 计算百分比并限制在 `0.0..=100.0`
///
/// `total == 0`（大小未知或空文件）时返回 0.0。
pub fn percent(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let p = (done as f64) / (total as f64) * 100.0;
    p.clamp(0.0, 100.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify() {
        assert_eq!(classify("a.jpg"), FileKind::Image);
        assert_eq!(classify("b.mp4"), FileKind::Video);
        assert_eq!(classify("c.mp3"), FileKind::Audio);
        assert_eq!(classify("d.pdf"), FileKind::Pdf);
        assert_eq!(classify("e.txt"), FileKind::Text);
        assert_eq!(classify("f.torrent"), FileKind::Torrent);
        assert_eq!(classify("g.zip"), FileKind::Archive);
        assert_eq!(classify("h.docx"), FileKind::Office);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(500), "500 B");
    }

    #[test]
    fn test_format_eta() {
        // 速率为 0 或已经传完：占位符
        assert_eq!(format_eta(0, 0), "—");
        assert_eq!(format_eta(1000, 0), "—");
        assert_eq!(format_eta(0, 100), "—");
        // < 60 秒
        assert_eq!(format_eta(1, 1), "剩余 1 秒");
        assert_eq!(format_eta(42, 1), "剩余 42 秒");
        assert_eq!(format_eta(59_000, 1000), "剩余 59 秒");
        // 向上取整，不会把 60.1 秒显示成 60 秒
        assert_eq!(format_eta(600, 10), "剩余 1 分 0 秒");
        assert_eq!(format_eta(605, 10), "剩余 1 分 1 秒");
        // < 3600 秒
        assert_eq!(format_eta(200, 1), "剩余 3 分 20 秒");
        assert_eq!(format_eta(3599, 1), "剩余 59 分 59 秒");
        // >= 3600 秒
        assert_eq!(format_eta(3600, 1), "剩余 1 时 00 分");
        assert_eq!(format_eta(3900, 1), "剩余 1 时 05 分");
        assert_eq!(format_eta(86_400, 1), "剩余 24 时 00 分");
        // 巨大值不溢出、不 panic
        assert!(format_eta(u64::MAX, 1).starts_with("剩余 "));
        assert!(format_eta(u64::MAX, 1024).ends_with(" 分"));
    }

    #[test]
    fn test_format_datetime() {
        assert_eq!(format_datetime("2024-05-06 12:03:44"), "2024/05/06 12:03");
        assert_eq!(format_datetime("2024-05-06T12:03:44"), "2024/05/06 12:03");
        assert_eq!(format_datetime("2024-05-06 12:03"), "2024/05/06 12:03");
        assert_eq!(format_datetime("2024-05-06"), "2024/05/06");
        // 空串 / 无法解析：原样返回
        assert_eq!(format_datetime(""), "");
        assert_eq!(format_datetime("abc"), "abc");
        assert_eq!(format_datetime("2024-05-06 12"), "2024-05-06 12");
        assert_eq!(format_datetime("2024/05/06"), "2024/05/06");
        assert_eq!(format_datetime("2024-05-06 1x:03"), "2024-05-06 1x:03");
        assert_eq!(
            format_datetime("2024-05-06x12:03:44"),
            "2024-05-06x12:03:44"
        );
        // 非 ASCII 不能 panic
        assert_eq!(format_datetime("2024-05-06 中文"), "2024-05-06 中文");
    }

    #[test]
    fn test_percent() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(123, 0), 0.0);
        assert_eq!(percent(0, 100), 0.0);
        assert_eq!(percent(1, 100), 1.0);
        assert_eq!(percent(50, 100), 50.0);
        assert_eq!(percent(100, 100), 100.0);
        // 超出范围时被裁剪
        assert_eq!(percent(150, 100), 100.0);
        assert_eq!(percent(u64::MAX, u64::MAX), 100.0);
        assert!(percent(1, u64::MAX) < 0.001);
        // 单调且有界
        let mut last = 0.0f32;
        for done in [0u64, 1, 250, 999, 1000, 1001] {
            let p = percent(done, 1000);
            assert!((0.0..=100.0).contains(&p));
            assert!(p >= last);
            last = p;
        }
    }

    #[test]
    fn test_split_command_line() {
        // 普通：程序路径含空格，参数含 %1
        assert_eq!(
            split_command_line(r#""C:\Program Files\DAUM\PotPlayer\PotPlayerMini64.exe" "%1""#),
            vec![
                r#"C:\Program Files\DAUM\PotPlayer\PotPlayerMini64.exe"#,
                "%1"
            ]
        );
        // 无引号路径
        assert_eq!(
            split_command_line(r"C:\Tools\mpv.exe %1"),
            vec![r"C:\Tools\mpv.exe", "%1"]
        );
        // 参数后追加开关
        assert_eq!(
            split_command_line(r#""C:\vlc\vlc.exe" --play-and-exit "%1""#),
            vec![r"C:\vlc\vlc.exe", "--play-and-exit", "%1"]
        );
        // 反斜杠转义：奇数个 \ + 引号 => 字面引号
        assert_eq!(split_command_line(r#"foo\".exe"#), vec!["foo\".exe"]);
        // 引号内连续的 "" 表示字面引号
        assert_eq!(split_command_line(r#""a""b""#), vec!["a\"b".to_string()]);
    }
}
