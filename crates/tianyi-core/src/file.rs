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
const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic"];
/// 视频扩展名集合
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "rmvb"];
/// 音频扩展名集合
const AUDIO_EXT: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a", "opus", "wma", "ape", "alac"];
/// 文本扩展名集合
const TEXT_EXT: &[&str] = &["txt", "md", "log", "json", "xml", "yml", "yaml", "toml", "ini", "conf", "cfg", "csv", "tsv", "srt", "vtt", "sh", "bat", "py", "rs", "go", "c", "h", "cpp", "java", "js", "ts", "html", "css", "sql", "php", "rb", "kt"];
/// 压缩包扩展名集合
const ARCHIVE_EXT: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "tgz", "zst"];
/// Office 扩展名集合
const OFFICE_EXT: &[&str] = &["doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp"];

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

/// 使用系统默认程序打开文件
pub fn open_with_system(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let path = path.to_string_lossy().replace('/', "\\");
        let cmd = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path])
            .spawn();
        return cmd
            .map(|_| ())
            .map_err(|e| format!("open file: {e}"));
    }

    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open file: {e}"));
    }

    #[cfg(target_os = "linux")]
    {
        return std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open file: {e}"));
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
}
