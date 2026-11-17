//! 静态审计：挂载实现里不允许出现「把文件内容写本地磁盘」的代码路径
//!
//! 这是零磁盘文件缓存的回归护栏。它不能证明运行时绝对不落盘（操作系统、杀软、
//! 第三方程序的行为不受我们控制），但能保证**本 crate 自己的代码**里不存在
//! 这类调用：写文件、临时文件、SQLite 内容缓存、.part 之类。
//!
//! 配置持久化（config.json / accounts.json / mounts.json）**不在**审计范围内：
//! 那是账号与挂载状态，不是云端文件内容，而且由 tianyi-core 的 ConfigStore 统一处理。

/// 被禁止的源码片段：出现即视为「可能在写文件内容」
const FORBIDDEN: &[(&str, &str)] = &[
    ("File::create", "创建/截断本地文件"),
    ("OpenOptions", "打开可写文件句柄"),
    ("std::fs::write", "写文件内容"),
    (
        "std::fs::File",
        "直接操作本地文件（应改用不落盘的内存路径）",
    ),
    ("tempfile", "临时文件缓存"),
    ("NamedTempFile", "临时文件缓存"),
    ("TemporaryDirectory", "临时目录缓存"),
    (".part", "断点续传分片文件"),
    ("rusqlite", "SQLite 内容缓存"),
    ("write_all(", "可能的落盘写入"),
    ("create_dir_all(", "创建缓存目录"),
];

/// 被审计的源文件（全部来自本 crate 的实现代码，不含示例与测试）
const SOURCES: &[(&str, &str)] = &[
    ("src/lib.rs", include_str!("../src/lib.rs")),
    ("src/error.rs", include_str!("../src/error.rs")),
    ("src/fs.rs", include_str!("../src/fs.rs")),
    ("src/meta.rs", include_str!("../src/meta.rs")),
    ("src/mount.rs", include_str!("../src/mount.rs")),
    ("src/paths.rs", include_str!("../src/paths.rs")),
    ("src/reader.rs", include_str!("../src/reader.rs")),
    ("src/runtime.rs", include_str!("../src/runtime.rs")),
    ("src/session.rs", include_str!("../src/session.rs")),
    ("src/stats.rs", include_str!("../src/stats.rs")),
];

#[cfg(windows)]
const WINDOWS_SOURCES: &[(&str, &str)] = &[
    ("src/win/mod.rs", include_str!("../src/win/mod.rs")),
    ("src/win/sys.rs", include_str!("../src/win/sys.rs")),
];

#[test]
fn crate_sources_never_write_file_contents_to_disk() {
    let mut hits: Vec<String> = Vec::new();
    // Windows 上多审计 src/win/*。这里刻意写成两段 cfg 而不是
    // `let mut all` + 条件 extend：后者在 Linux 上会撞 unused_mut，
    // 而 clippy 是按 -D warnings 跑的。
    #[cfg(windows)]
    let all = {
        let mut all: Vec<(&str, &str)> = SOURCES.to_vec();
        all.extend_from_slice(WINDOWS_SOURCES);
        all
    };
    #[cfg(not(windows))]
    let all: Vec<(&str, &str)> = SOURCES.to_vec();

    for (name, text) in all {
        let code = production_code(text);
        for (needle, why) in FORBIDDEN {
            if code.contains(needle) {
                hits.push(format!("{name}: 出现 {needle:?}（{why}）"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "零磁盘文件缓存的审计失败：\n{}",
        hits.join("\n")
    );
}

/// 去掉注释与测试模块后再审计
///
/// 两个必要动作：
/// * 文档注释里会**描述**被禁止的东西（例如"没有 .part 缓存"），不该算命中；
/// * 测试模块里会往 mock TCP 连接 write_all、读临时目录，那不是产品路径。
///
/// 这个函数只做"缩小扫描面"，不做任何放行名单——真正危险的调用都在产品代码里，
/// 一旦出现必然被扫到。
fn production_code(src: &str) -> String {
    let cut = match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    };
    cut.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn audit_list_is_not_empty() {
    // 护栏自身也要有护栏：如果哪天审计清单被删空，这个测试会失败
    assert!(SOURCES.len() >= 8, "审计源文件清单过短");
    assert!(!FORBIDDEN.is_empty());
}
