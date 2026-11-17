//! 路径与文件名的 Windows 语义适配
//!
//! 云盘与 NTFS 对「合法文件名」的约束不一样，这个模块专门吃掉差异：
//!
//! * 云端允许 `? * : " < > |` 与控制字符，Windows 不允许；
//! * 云端区分大小写，Windows 上我们挂的是大小写不敏感的卷，必须自己建索引；
//! * 云端允许同名（仅大小写不同）的两个文件，Windows 不允许；
//! * 云端文件名可能超过 255 个 UTF-16 单元，超过 NTFS 单组件上限；
//! * Windows 有 CON / PRN / COM1 之类的设备名，任何目录下都不能出现。
//!
//! 输入路径来自操作系统（可能被任意程序构造），所以这里对 `..`、空段、多余分隔符一律
//! 采取「拒绝或丢弃」而不是「尽力猜测」：猜测会带来越权访问的可能。

use crate::error::{Error, Result};

/// 单个文件名组件的 UTF-16 上限
///
/// NTFS 是 255，这里留出余量给去重后缀与截断哈希：200 之后开始截断，
/// 保证「截断 + 后缀」仍远小于 255。
pub const MAX_COMPONENT_UTF16: usize = 200;

/// Windows 保留设备名（不带扩展名、大小写不敏感）
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 把操作系统给的路径拆成一段段原始名字
///
/// WinFsp 传进来的是 `\目录\文件.txt` 这种形式（根目录是空串或 `\${BT}），
/// FUSE 传进来的是 `/目录/文件.txt`。两种分隔符都接受，方便复用同一套上层逻辑。
pub fn split_remote_path(raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for seg in raw.split(['\\', '/']) {
        let seg = seg.trim_matches('\0');
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            // 操作系统理论上已经规范化过，出现 `..` 只可能是被构造出来的请求
            return Err(Error::InvalidPath(format!("路径包含上级引用：{raw}")));
        }
        out.push(seg.to_string());
    }
    Ok(out)
}

/// 把一段云端文件名清洗成 Windows 能接受的文件名
pub fn sanitize_component(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => cleaned.push('_'),
            c if (c as u32) < 0x20 => {
                // 控制字符直接丢弃：替换成 _ 会让 "a\nb" 与 "a_b" 撞名
            }
            c => cleaned.push(c),
        }
    }

    // Windows 无法寻址以点或空格结尾的名字
    while cleaned.ends_with(' ') || cleaned.ends_with('.') {
        cleaned.pop();
    }

    if cleaned.is_empty() {
        cleaned.push('_');
    }

    // 保留设备名：任何一个目录下都保留，加下划线前缀即可
    let stem = stem_of(&cleaned);
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        cleaned.insert(0, '_');
    }

    truncate_component(&cleaned)
}

/// 超长组件截断：保留扩展名 + 追加原始名的短哈希，避免截断后互相撞名
fn truncate_component(name: &str) -> String {
    if utf16_len(name) <= MAX_COMPONENT_UTF16 {
        return name.to_string();
    }
    let hash = format!("~{:08x}", fnv1a(name.as_bytes()) as u32);
    let ext = extension_of(name).unwrap_or("");
    let budget = MAX_COMPONENT_UTF16.saturating_sub(hash.len() + ext.chars().count());
    let mut head = String::new();
    let mut taken = 0usize;
    for ch in name.chars() {
        let w = ch.len_utf16();
        if taken + w > budget {
            break;
        }
        head.push(ch);
        taken += w;
    }
    format!("{head}{hash}{ext}")
}

/// 生成互不冲突的显示名
///
/// 输入顺序即优先级：先到者保留原名（这是 [crate::fs] 里按 id 稳定排序后的顺序），
/// 后来者在扩展名前插入 ` (n)`。调用方拿到的是与输入一一对应的 Vec。
pub fn unique_display_names(raw_names: &[String]) -> Vec<String> {
    let mut used: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(raw_names.len());
    let mut out = Vec::with_capacity(raw_names.len());
    for raw in raw_names {
        let base = sanitize_component(raw);
        let key = lookup_key(&base);
        if used.insert(key) {
            out.push(base);
            continue;
        }
        let ext = extension_of(&base).unwrap_or("").to_string();
        let stem = if ext.is_empty() {
            base.clone()
        } else {
            base[..base.len() - ext.len()].to_string()
        };
        let mut resolved = None;
        for n in 1..10_000u32 {
            let candidate = format!("{stem} ({n}){ext}");
            if used.insert(lookup_key(&candidate)) {
                resolved = Some(candidate);
                break;
            }
        }
        // 一万个同名文件已经超出任何真实场景；仍然给出确定性的兜底而不是死循环
        let candidate = resolved
            .unwrap_or_else(|| format!("{stem}~{:08x}{ext}", fnv1a(base.as_bytes()) as u32));
        out.push(candidate);
    }
    out
}

/// 大小写不敏感查找用的规范化键
pub fn lookup_key(name: &str) -> String {
    name.to_lowercase()
}

/// 诊断用的路径拼接（不受文件名里的反斜杠影响，仅供日志/错误信息使用）
pub fn display_path(segments: &[String]) -> String {
    if segments.is_empty() {
        "\\".to_string()
    } else {
        format!("\\{}", segments.join("\\"))
    }
}

fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// 扩展名（含前导点）；没有扩展名时返回 None
fn extension_of(name: &str) -> Option<&str> {
    match name.rfind('.') {
        Some(i) if i > 0 => Some(&name[i..]),
        _ => None,
    }
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// FNV-1a 64：只用来给超长/重名兜底生成稳定后缀，不涉及任何安全语义
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_windows_and_unix_paths() {
        assert_eq!(split_remote_path("").unwrap(), Vec::<String>::new());
        assert_eq!(split_remote_path("\\").unwrap(), Vec::<String>::new());
        assert_eq!(split_remote_path("/").unwrap(), Vec::<String>::new());
        assert_eq!(split_remote_path("\\a\\b.txt").unwrap(), vec!["a", "b.txt"]);
        assert_eq!(split_remote_path("/a/b.txt").unwrap(), vec!["a", "b.txt"]);
        assert_eq!(split_remote_path("\\\\a\\\\b").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn parent_traversal_is_rejected() {
        assert!(matches!(
            split_remote_path("\\a\\..\\b"),
            Err(Error::InvalidPath(_))
        ));
    }

    #[test]
    fn illegal_characters_never_reach_the_os() {
        let s = sanitize_component("a<b>c:d\"e|f?g*h\\i");
        for bad in ['<', '>', ':', '"', '|', '?', '*', '\\'] {
            assert!(!s.contains(bad), "{s} 仍含非法字符 {bad}");
        }
        assert_eq!(sanitize_component("a\u{1}b"), "ab");
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(sanitize_component("name.. "), "name");
        assert_eq!(sanitize_component("  "), "_");
    }

    #[test]
    fn reserved_device_names_get_a_prefix() {
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("con.txt"), "_con.txt");
        assert_eq!(sanitize_component("COM1"), "_COM1");
        assert_eq!(sanitize_component("CONSOLE"), "CONSOLE");
    }

    #[test]
    fn long_names_are_truncated_deterministically() {
        let long = "长".repeat(300);
        let a = sanitize_component(&long);
        let b = sanitize_component(&long);
        assert_eq!(a, b, "同样输入必须得到同样输出");
        assert!(utf16_len(&a) <= MAX_COMPONENT_UTF16, "截断后仍在预算内");
        assert_ne!(a, sanitize_component(&"长".repeat(299)), "截断后仍要可区分");
    }

    #[test]
    fn case_only_duplicates_are_renamed() {
        let names = vec![
            "A.txt".to_string(),
            "a.txt".to_string(),
            "a.TXT".to_string(),
        ];
        let out = unique_display_names(&names);
        assert_eq!(out[0], "A.txt");
        assert_eq!(out[1], "a (1).txt");
        assert_eq!(out[2], "a (2).TXT");
    }

    #[test]
    fn sanitize_collisions_are_also_renamed() {
        let names = vec!["a?b".to_string(), "a*b".to_string()];
        let out = unique_display_names(&names);
        assert_eq!(out[0], "a_b");
        assert_eq!(out[1], "a_b (1)");
    }

    #[test]
    fn empty_directory_listing_is_fine() {
        assert!(unique_display_names(&[]).is_empty());
    }

    #[test]
    fn display_path_is_for_diagnostics() {
        assert_eq!(display_path(&[]), "\\");
        assert_eq!(display_path(&["a".into(), "b".into()]), "\\a\\b");
    }
}
