//! 云盘路径：解析、规范化与本地文件名消毒
//!
//! GUI 与 CLI 都要回答同一批问题：「这个输入是绝对路径还是相对当前目录」「.. 折叠到根
//! 之后怎么办」「这个名字能不能直接当本地文件名」。原实现散在控制器里（路径栈 + 各种
//! 字符串拼接），CLI 再抄一遍必然分叉，因此统一到 core：
//!
//! * 云端路径本身与 API 无关，纯字符串语义：以 '/' 分隔、'.' 忽略、'..' 弹栈；
//! * 段名允许空格、Unicode、中文与几乎所有非控制字符（天翼云盘的真实文件名就是这样），
//!   只拒绝 NUL——它一旦进到 C 字符串边界会把参数截断；
//! * 折叠 '..' 越过根时**报错**而不是静默停在根：静默会让 "rm ../../x" 变成
//!   "rm /x"，那是真正的误删。
//!
//! 本地文件名消毒单独一条规则：云端合法、本地非法的字符（Windows 的 : * ? " < > | 等）
//! 一律替换为下划线，避免下载到一半才因为 EINVAL 失败。

use std::path::{Path, PathBuf};

/// 根目录 id（个人云与家庭云都用 -11）
pub const ROOT_FOLDER_ID: &str = "-11";

/// 路径解析错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// '..' 已经越过根目录
    AboveRoot,
    /// 段里含 NUL
    Nul,
    /// 路径为空（例如 '' 作为必须给出名字的参数）
    Empty,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::AboveRoot => write!(f, "路径越过了云端根目录"),
            PathError::Nul => write!(f, "路径中包含 NUL 字符"),
            PathError::Empty => write!(f, "路径为空"),
        }
    }
}

impl std::error::Error for PathError {}

/// 规范化后的云端绝对路径
///
/// 内部只存段名，永远是绝对路径（没有「相对路径」这种状态）：相对输入必须在解析时
/// 结合当前目录换成绝对路径，否则「先 cd 再 ls」这类操作会因为保存了一份相对路径而
/// 指向完全不同的位置。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CloudPath {
    segments: Vec<String>,
}

impl CloudPath {
    /// 根目录
    pub fn root() -> Self {
        CloudPath {
            segments: Vec::new(),
        }
    }

    /// 用已经规范化好的段构造
    pub fn from_segments(segments: Vec<String>) -> Self {
        CloudPath { segments }
    }

    /// 是否根目录
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// 段列表
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// 最后一段（根目录返回 None）
    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// 父目录（根目录返回 None）
    pub fn parent(&self) -> Option<CloudPath> {
        if self.is_root() {
            return None;
        }
        let mut segs = self.segments.clone();
        segs.pop();
        Some(CloudPath { segments: segs })
    }

    /// 追加一段（调用方保证 name 不含 '/'）
    pub fn child(&self, name: &str) -> CloudPath {
        let mut segs = self.segments.clone();
        segs.push(name.to_string());
        CloudPath { segments: segs }
    }

    /// 解析绝对路径：前导 '/' 可有可无，'.' 忽略，'..' 弹栈
    ///
    /// 空串按根目录处理（"ls" 不带参数就是列根目录）。
    pub fn parse(input: &str) -> Result<Self, PathError> {
        let mut segments: Vec<String> = Vec::new();
        for raw in input.split('/') {
            if raw.contains('\0') {
                return Err(PathError::Nul);
            }
            match raw {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        return Err(PathError::AboveRoot);
                    }
                }
                name => segments.push(name.to_string()),
            }
        }
        Ok(CloudPath { segments })
    }

    /// 以当前目录为基准解析输入：绝对路径从根开始，其余按 base 拼接
    ///
    /// 这是 CLI 里所有路径参数的统一入口（cd / ls / download 都用它），也是「支持相对
    /// 路径」这条需求的唯一实现点。
    pub fn resolve(base: &CloudPath, input: &str) -> Result<Self, PathError> {
        if input.starts_with('/') {
            return CloudPath::parse(input);
        }
        let mut segments = base.segments.clone();
        for raw in input.split('/') {
            if raw.contains('\0') {
                return Err(PathError::Nul);
            }
            match raw {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        return Err(PathError::AboveRoot);
                    }
                }
                name => segments.push(name.to_string()),
            }
        }
        Ok(CloudPath { segments })
    }

    /// 展示用字符串：根目录是 "/"，其余是 "/a/b"
    pub fn display(&self) -> String {
        if self.segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.segments.join("/"))
        }
    }
}

impl std::fmt::Display for CloudPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// 去掉文件名中不安全的字符（跨平台本地写入用）
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() || cleaned == "." || cleaned == ".." {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// 拼出本地落盘路径：目录 + 消毒后的文件名
///
/// 消毒在拼接**之后**再校验一次：'..' 或分隔符必须已经被替换掉，否则一个恶意文件名
/// 就能把下载写到目标目录之外。
pub fn local_child(dir: &Path, name: &str) -> PathBuf {
    let safe = sanitize_file_name(name);
    debug_assert!(!safe.contains('/') && !safe.contains('\\'));
    dir.join(safe)
}

/// 递归统计本地目录大小（字节）
///
/// GUI 拖入目录时要先算出任务大小，CLI 上传目录也要报总量；两边必须给出一致的数字。
pub fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => total += dir_size(&p),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_absolutizes_and_collapses_dots() {
        assert_eq!(CloudPath::parse("").unwrap(), CloudPath::root());
        assert_eq!(CloudPath::parse("/").unwrap(), CloudPath::root());
        assert_eq!(CloudPath::parse("///").unwrap(), CloudPath::root());
        assert_eq!(CloudPath::parse("/a/b").unwrap().display(), "/a/b");
        assert_eq!(CloudPath::parse("a/b").unwrap().display(), "/a/b");
        assert_eq!(CloudPath::parse("/a/./b").unwrap().display(), "/a/b");
        assert_eq!(CloudPath::parse("/a/b/..").unwrap().display(), "/a");
        assert_eq!(CloudPath::parse("/a//b").unwrap().display(), "/a/b");
        // 折叠到根是合法的（/a/.. = /）
        assert!(CloudPath::parse("/a/..").unwrap().is_root());
        // 越过根必须报错，不能静默停在根
        assert_eq!(CloudPath::parse("/..").unwrap_err(), PathError::AboveRoot);
        assert_eq!(CloudPath::parse("/../a").unwrap_err(), PathError::AboveRoot);
        assert_eq!(CloudPath::parse("a\0b").unwrap_err(), PathError::Nul);
    }

    #[test]
    fn resolve_supports_relative_and_unicode_and_spaces() {
        let docs = CloudPath::parse("/Documents").unwrap();
        assert_eq!(
            CloudPath::resolve(&docs, "file.zip").unwrap().display(),
            "/Documents/file.zip"
        );
        assert_eq!(
            CloudPath::resolve(&docs, "../Backup").unwrap().display(),
            "/Backup"
        );
        assert_eq!(
            CloudPath::resolve(&docs, "/Music").unwrap().display(),
            "/Music"
        );
        // 空格 / 中文 / emoji / 全角标点都是合法段名
        let deep = CloudPath::resolve(&docs, "报  告 2024（终稿）.pdf").unwrap();
        assert_eq!(deep.name(), Some("报  告 2024（终稿）.pdf"));
        assert_eq!(
            CloudPath::resolve(&docs, "🎉/子目录").unwrap().display(),
            "/Documents/🎉/子目录"
        );
    }

    #[test]
    fn parent_and_name_walk_the_tree() {
        let p = CloudPath::parse("/a/b/c").unwrap();
        assert_eq!(p.name(), Some("c"));
        let parent = p.parent().unwrap();
        assert_eq!(parent.display(), "/a/b");
        let root = CloudPath::root();
        assert!(root.parent().is_none());
        assert!(root.name().is_none());
        assert_eq!(root.display(), "/");
        assert_eq!(root.child("x").display(), "/x");
    }

    #[test]
    fn sanitize_removes_reserved_characters_and_traversal() {
        assert_eq!(
            sanitize_file_name("a/b:c*d?e\"f<g>h|i"),
            "a_b_c_d_e_f_g_h_i"
        );
        assert_eq!(sanitize_file_name("tab\there"), "tab_here");
        assert_eq!(sanitize_file_name(""), "unnamed");
        assert_eq!(sanitize_file_name("   "), "unnamed");
        assert_eq!(sanitize_file_name(".."), "unnamed");
        assert_eq!(sanitize_file_name("."), "unnamed");
        // 中文与空格原样保留
        assert_eq!(sanitize_file_name("我的 文件.zip"), "我的 文件.zip");
    }

    #[test]
    fn local_child_never_escapes_the_directory() {
        let dir = Path::new("/tmp/dest");
        let p = local_child(dir, "../../etc/passwd");
        assert_eq!(p.parent().unwrap(), dir);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/') && !name.contains('\\'), "{name}");
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let root = std::env::temp_dir().join(format!("tianyi-dirsize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("a.bin"), vec![0u8; 10]).expect("write");
        std::fs::write(root.join("sub/b.bin"), vec![0u8; 5]).expect("write");
        assert_eq!(dir_size(&root), 15);
        let _ = std::fs::remove_dir_all(&root);
    }
}
