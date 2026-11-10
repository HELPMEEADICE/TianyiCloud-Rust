//! 界面语言：Slint 语言包切换 + Rust 侧文案表
//!
//! # 为什么有两套目录
//!
//! 界面文案有两个产地，各走各的管线，谁也替不了谁：
//!
//! - `crates/tianyi-app/lang/<lang>/LC_MESSAGES/tianyi-app.po`：给 `.slint` 里的
//!   `@tr("…")` 用。`build.rs` 靠 `with_bundled_translations("lang")` 在**编译期**
//!   把 .po 打进二进制；运行时 [`apply`] 调 `slint::select_bundled_translation`，
//!   置脏之后所有 `@tr` 绑定自动重算，不用重启进程。
//! - `crates/tianyi-app/i18n/rs/<lang>.json`：给 Rust 侧用。列表里的文字（状态栏、
//!   提示条、任务状态、文件类型、ETA）都是控制器 `format!` 拼出来的，压根不经过
//!   Slint 表达式，`@tr` 管不到，只能查表。
//!
//! # 为什么 msgid 是简体中文、为什么索引 0 是回退
//!
//! Slint 把 `@tr("原文")` 的原文放在翻译表的**索引 0**——那是没选中任何语言时的
//! 回退项。本仓库的源码文案本来就是简中，于是「msgid = 简中原文」既是事实上的简中
//! 翻译，又让「简中 / 回退 / 缺词」三种情况自然合一：`zh-Hans` 因此没有自己的语言
//! 目录，[`Lang::code`] 对它返回空串（等于「选索引 0」），需要维护的只有
//! `en` / `zh-Hant` / `ja` 三份 .po。
//!
//! Rust 侧刻意复用同一套思路：[`zh-Hans.json`] 是**唯一完整的词表**（键 → 简中原
//! 文），其它语种的 JSON 只填翻好的项。查词顺序是「当前语种 → 简中 → 键本身」，
//! 所以任何一处漏翻都退化到中文原文，而不是让界面出现空串或 `toast.xxx` 这种键名。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use tianyi_core::config::LanguageMode;

/// 界面语种（`Auto` 不在这里：它必须先经 [`resolve`] 落成具体语种）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 简体中文（同时是回退语种）
    ZhHans,
    /// 繁体中文
    ZhHant,
    /// 英文
    En,
    /// 日文
    Ja,
}

impl Lang {
    /// 语种在进程内的存储序号（[`CURRENT`] 只存 u8，避免锁）
    fn index(self) -> u8 {
        match self {
            Lang::ZhHans => 0,
            Lang::ZhHant => 1,
            Lang::En => 2,
            Lang::Ja => 3,
        }
    }

    fn from_index(i: u8) -> Self {
        match i {
            1 => Lang::ZhHant,
            2 => Lang::En,
            3 => Lang::Ja,
            _ => Lang::ZhHans,
        }
    }

    /// Slint 翻译包里的语言目录名，也是 `select_bundled_translation` 的入参
    ///
    /// 简中返回空串：`.po` 的 msgid 就是简中原文，它由索引 0 承担，没有独立目录。
    /// 其余三个字符串必须与 `lang/` 下的目录名**字面一致**（有单测钉住）。
    pub fn code(self) -> &'static str {
        match self {
            Lang::ZhHans => "",
            Lang::ZhHant => "zh-Hant",
            Lang::En => "en",
            Lang::Ja => "ja",
        }
    }
}

/// 当前语种。初值是简中：那是回退语言，任何语言包都没生效时界面仍是可读的原文。
static CURRENT: AtomicU8 = AtomicU8::new(0);

static ZH_HANS: OnceLock<HashMap<String, String>> = OnceLock::new();
static ZH_HANT: OnceLock<HashMap<String, String>> = OnceLock::new();
static EN: OnceLock<HashMap<String, String>> = OnceLock::new();
static JA: OnceLock<HashMap<String, String>> = OnceLock::new();

/// 当前语种
pub fn current() -> Lang {
    Lang::from_index(CURRENT.load(Ordering::Relaxed))
}

/// 切换语种：写进程内的当前语种，并让 Slint 侧的 `@tr` 绑定全部重算
///
/// 必须在**首个组件创建之后**调用：翻译包是随组件一起注册进运行时的，早于它调用
/// 只会拿到 `NoTranslationsBundled`。失败只记日志不中断——切不了语言不该让界面起不来。
pub fn apply(lang: Lang) {
    CURRENT.store(lang.index(), Ordering::Relaxed);
    if let Err(e) = slint::select_bundled_translation(lang.code()) {
        log::warn!("切换界面语言失败（{}）：{e}", lang.code());
    }
}

/// 跟随系统语言
///
/// 系统语言取不到（容器 / 精简系统里 `sys_locale` 会返回 `None`）时判成英文：
/// 那是除中文外覆盖面最广的一种。
pub fn detect_system() -> Lang {
    match sys_locale::get_locale() {
        Some(locale) => map_locale(&locale),
        None => Lang::En,
    }
}

/// 把配置里的语言模式落成具体语种
pub fn resolve(mode: LanguageMode) -> Lang {
    match mode {
        LanguageMode::Auto => detect_system(),
        LanguageMode::ZhHans => Lang::ZhHans,
        LanguageMode::ZhHant => Lang::ZhHant,
        LanguageMode::En => Lang::En,
        LanguageMode::Ja => Lang::Ja,
    }
}

/// BCP-47 语言标签 → 语种（纯函数，供单测与 [`detect_system`] 共用）
///
/// `sys_locale` 给的是 `zh-CN` / `zh-TW` / `ja-JP` / `de-DE` 这类标签，实际系统上
/// 还会冒出 `zh_Hans_CN` 这种下划线写法或大小写混排，所以先归一化成小写、再按
/// `-` 与 `_` 切子标签。
///
/// 判定顺序是「繁体优先」：脚本子标签 `hant` 与地区子标签 `tw` / `hk` / `mo` 都
/// 指向繁体；剩下的 `hans` / `cn` / `sg` / `my` 以及任何其它 `zh-*` 写法（含裸
/// `zh`）一律归简中。非中文一律回退英文。
pub fn map_locale(locale: &str) -> Lang {
    let normalized = locale.trim().to_ascii_lowercase();
    let mut parts = normalized.split(['-', '_']);
    let primary = parts.next().unwrap_or("");
    let subtags: Vec<&str> = parts.collect();
    match primary {
        "zh" => {
            if subtags
                .iter()
                .any(|s| matches!(*s, "hant" | "tw" | "hk" | "mo"))
            {
                Lang::ZhHant
            } else {
                Lang::ZhHans
            }
        }
        "ja" => Lang::Ja,
        // en 与其它一切未知语言（以及空串）都落到英文
        _ => Lang::En,
    }
}

/// 取词：当前语种 → 简中 → 键本身
pub fn t(key: &'static str) -> &'static str {
    resolve_in(current(), key).unwrap_or(key)
}

/// 取词并替换 `{name}` 形式的占位符
///
/// 表里没有的占位符原样保留：翻译漏填一个变量时，界面上出现 `{count}` 至少能一眼
/// 看出是文案问题，比悄悄吞掉更利于定位。
pub fn tf(key: &'static str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    render_in(current(), key, args)
}

/// 文件类型的中文/本地化显示名
///
/// `FileKind` 自己没有「文件夹」变体（目录是 `FileObject::is_dir` 单独表达的），
/// 所以这条链上没有 `kind.folder`——它只存在于词表里，供 `files_to_entries` 使用。
pub fn label_kind(k: tianyi_core::file::FileKind) -> &'static str {
    use tianyi_core::file::FileKind;
    t(match k {
        FileKind::Image => "kind.image",
        FileKind::Video => "kind.video",
        FileKind::Audio => "kind.audio",
        FileKind::Pdf => "kind.pdf",
        FileKind::Text => "kind.text",
        FileKind::Archive => "kind.archive",
        FileKind::Torrent => "kind.torrent",
        FileKind::Office => "kind.office",
        FileKind::Other => "kind.other",
    })
}

/// 传输任务状态的显示名
pub fn label_status(s: tianyi_core::task::TaskStatus) -> &'static str {
    use tianyi_core::task::TaskStatus;
    t(match s {
        TaskStatus::Pending => "task.pending",
        TaskStatus::Running => "task.running",
        TaskStatus::Paused => "task.paused",
        TaskStatus::Canceled => "task.canceled",
        TaskStatus::Completed => "task.completed",
        TaskStatus::Failed => "task.failed",
    })
}

/// 传输剩余时间的本地化文案（速度未知时返回空串，调用方据此不显示这一段）
///
/// 与 `tianyi_core::file::format_eta` 的分档保持一致（秒 / 分秒 / 时分），只是把
/// 拼好的中文换成查表，否则英文界面上会冒出一句「剩余 1 分 2 秒」。
pub fn format_eta(remaining_bytes: u64, bytes_per_sec: u64) -> String {
    eta_in(current(), remaining_bytes, bytes_per_sec)
}

/// 指定语种下的 ETA 文案
///
/// 分档与取词都是纯函数（语种由参数给定），单测因此不必动进程级语种。
fn eta_in(lang: Lang, remaining_bytes: u64, bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return String::new();
    }
    // 向上取整：还剩 1 字节但限速 1 B/s 时也该显示 1 秒而不是 0 秒
    let secs = remaining_bytes.div_ceil(bytes_per_sec);
    if secs < 60 {
        render_in(lang, "eta.seconds", &[("n", &secs)])
    } else if secs < 3600 {
        render_in(
            lang,
            "eta.minutes",
            &[("m", &(secs / 60)), ("s", &(secs % 60))],
        )
    } else {
        // 分钟补零两位：16:05 读起来比 16:5 稳
        let h = secs / 3600;
        let m = format!("{:02}", (secs % 3600) / 60);
        render_in(lang, "eta.hours", &[("h", &h), ("m", &m)])
    }
}

/// 查词（不兜底）：`None` 表示该语种的词表里没有这一条
///
/// 语种由参数给定而不是读 [`CURRENT`]：查表是纯函数，才能被单测直接驱动，
/// 不会因为并行跑的其它测试改了全局语种而偶发失败。
fn lookup_in(lang: Lang, key: &str) -> Option<&'static str> {
    table(lang).get(key).map(|v| v.as_str())
}

/// 回退链：指定语种 → 简中 → `None`（调用方据此退化成键名本身）
fn resolve_in(lang: Lang, key: &str) -> Option<&'static str> {
    if let Some(v) = lookup_in(lang, key) {
        return Some(v);
    }
    if lang == Lang::ZhHans {
        return None;
    }
    lookup_in(Lang::ZhHans, key)
}

/// 按指定语种取词并替换占位符（[`tf`] 的纯函数内核）
fn render_in(lang: Lang, key: &'static str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = resolve_in(lang, key).unwrap_or(key).to_string();
    for &(name, value) in args {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

/// 语种 → 词表（首次访问时从编译期嵌入的 JSON 解析，之后只读）
fn table(lang: Lang) -> &'static HashMap<String, String> {
    match lang {
        Lang::ZhHans => ZH_HANS.get_or_init(|| parse(include_str!("../i18n/rs/zh-Hans.json"))),
        Lang::ZhHant => ZH_HANT.get_or_init(|| parse(include_str!("../i18n/rs/zh-Hant.json"))),
        Lang::En => EN.get_or_init(|| parse(include_str!("../i18n/rs/en.json"))),
        Lang::Ja => JA.get_or_init(|| parse(include_str!("../i18n/rs/ja.json"))),
    }
}

/// 词表 JSON 解析
///
/// 词表是 `include_str!` 进来的编译期资产，解析失败只可能是仓库里的 JSON 坏了。
/// 这里不 panic：漏翻退化成键名，总比整个界面起不来强；日志里会留下线索。
fn parse(src: &'static str) -> HashMap<String, String> {
    serde_json::from_str(src).unwrap_or_else(|e| {
        log::error!("i18n 词表解析失败：{e}");
        HashMap::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_locale_basics() {
        assert_eq!(map_locale("zh-CN"), Lang::ZhHans);
        assert_eq!(map_locale("zh-Hans"), Lang::ZhHans);
        assert_eq!(map_locale("zh-TW"), Lang::ZhHant);
        assert_eq!(map_locale("zh-HK"), Lang::ZhHant);
        assert_eq!(map_locale("zh-Hant"), Lang::ZhHant);
        assert_eq!(map_locale("ja-JP"), Lang::Ja);
        assert_eq!(map_locale("en-US"), Lang::En);
        // 非中文非日文一律回退英文
        assert_eq!(map_locale("de-DE"), Lang::En);
        assert_eq!(map_locale(""), Lang::En);
    }

    /// 下划线写法、大小写混排、裸语言码都要容错
    #[test]
    fn test_map_locale_tolerates_shapes() {
        assert_eq!(map_locale("zh_CN"), Lang::ZhHans);
        assert_eq!(map_locale("ZH-HANT"), Lang::ZhHant);
        assert_eq!(map_locale("ja"), Lang::Ja);
        assert_eq!(map_locale(" zh-TW "), Lang::ZhHant);
        // 裸 zh / 无地区子标签的 zh-* 都归简中
        assert_eq!(map_locale("zh"), Lang::ZhHans);
        assert_eq!(map_locale("zh-SG"), Lang::ZhHans);
    }

    /// 繁体优先：地区标签比脚本标签更贴近用户所在地区
    #[test]
    fn test_map_locale_traditional_wins() {
        assert_eq!(map_locale("zh-Hans-HK"), Lang::ZhHant);
        assert_eq!(map_locale("zh-MO"), Lang::ZhHant);
    }

    /// `code()` 必须与 `lang/` 下的真实目录名字面一致，否则运行时切语会报 LanguageNotFound
    #[test]
    fn test_lang_code_matches_packaging_dirs() {
        assert_eq!(Lang::ZhHans.code(), "");
        assert_eq!(Lang::ZhHant.code(), "zh-Hant");
        assert_eq!(Lang::En.code(), "en");
        assert_eq!(Lang::Ja.code(), "ja");

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lang");
        for lang in [Lang::ZhHant, Lang::En, Lang::Ja] {
            let po = root
                .join(lang.code())
                .join("LC_MESSAGES")
                .join("tianyi-app.po");
            assert!(po.is_file(), "缺少语言包：{}", po.display());
        }
        // 简中走索引 0，不允许有目录（有了反而会盖掉回退语言）
        assert!(!root.join("zh-Hans").exists(), "zh-Hans 不应有独立目录");
    }

    /// 简中词表是唯一完整的那份：本期已登记全部现有文案，只该增不该减
    #[test]
    fn test_zh_hans_table_is_complete() {
        let table = table(Lang::ZhHans);
        assert!(
            table.len() >= 100,
            "简中词表只有 {} 条，说明有文案没登记",
            table.len()
        );
        for key in ["toast.copied", "status.item_count", "error.move_failed"] {
            assert!(table.contains_key(key), "简中词表缺少 {key}");
        }
    }

    /// 缺词回退链：当前语种 → 简中 → 键名本身
    ///
    /// 这里全部走 `lookup_in` / `resolve_in`，不再 `apply()` 改进程级语种：
    /// 同一个测试二进制里 `ui_tests` / `controller` 的测试在别的线程并行跑，
    /// 它们查 `t()` 时不该看见被别的测试改过的语种。
    #[test]
    fn test_lookup_fallback_chain() {
        // 简中是唯一完整的词表：它的命中结果就是最终回退值
        assert_eq!(
            lookup_in(Lang::ZhHans, "toast.copied"),
            Some("已复制到剪贴板")
        );
        assert_eq!(
            resolve_in(Lang::ZhHans, "toast.copied"),
            Some("已复制到剪贴板")
        );

        // 非简中语种：有译文就用译文，没有就逐字退回简中原文——
        // 无论如何都不能落到 `None`，否则界面会出现 `toast.copied` 这种键名
        let zh = lookup_in(Lang::ZhHans, "toast.copied");
        for lang in [Lang::En, Lang::ZhHant, Lang::Ja] {
            assert_eq!(
                resolve_in(lang, "toast.copied"),
                lookup_in(lang, "toast.copied").or(zh),
                "{lang:?} 缺词时必须回退到简中原文"
            );
        }

        // 任何语种都没有的键 → 解析为 `None`，由 `t()` 退化成键名本身
        for lang in [Lang::ZhHans, Lang::En, Lang::ZhHant, Lang::Ja] {
            assert_eq!(
                lookup_in(lang, "toast.this_key_does_not_exist"),
                None,
                "{lang:?}"
            );
            assert_eq!(
                resolve_in(lang, "toast.this_key_does_not_exist"),
                None,
                "{lang:?}"
            );
        }
        // 公开取词口仍在同一条链上：默认语种下拿到简中原文，缺词时拿到键名
        assert_eq!(t("toast.copied"), "已复制到剪贴板");
        assert_eq!(
            t("toast.this_key_does_not_exist"),
            "toast.this_key_does_not_exist"
        );
    }

    /// `tf` / `render_in` 只替换表里出现过的具名占位符，其余原样保留
    #[test]
    fn test_tf_substitutes_placeholders() {
        let zh = Lang::ZhHans;

        assert_eq!(
            render_in(zh, "toast.moved", &[("name", &"a.txt")]),
            "已移动 a.txt"
        );
        assert_eq!(
            render_in(zh, "status.item_count", &[("count", &12u32)]),
            "12 个项目"
        );
        // 没有传占位符 → 原样保留 `{name}`，一眼能看出漏了参数
        assert_eq!(render_in(zh, "toast.moved", &[]), "已移动 {name}");
        // 传了表里没有的占位符 → 不影响结果
        assert_eq!(
            render_in(zh, "toast.moved", &[("other", &"x"), ("name", &"b.txt")]),
            "已移动 b.txt"
        );
        // 公开入口与按语种渲染必须是同一份结果（默认语种 = 简中）
        assert_eq!(
            tf("toast.moved", &[("name", &"a.txt")]),
            render_in(zh, "toast.moved", &[("name", &"a.txt")])
        );
    }

    /// ETA：三档分界 + 速度为 0 时不显示
    #[test]
    fn test_format_eta_buckets() {
        let zh = Lang::ZhHans;

        assert_eq!(eta_in(zh, 100, 0), "", "速度未知时不该显示剩余时间");
        assert_eq!(eta_in(zh, 59, 1), "剩余 59 秒");
        assert_eq!(eta_in(zh, 90, 1), "剩余 1 分 30 秒");
        assert_eq!(eta_in(zh, 3600, 1), "剩余 1 时 00 分");
        assert_eq!(eta_in(zh, 3900, 1), "剩余 1 时 05 分");
        // 向上取整：1 字节 / 1 B/s 也要显示 1 秒而不是 0 秒
        assert_eq!(eta_in(zh, 1, 1), "剩余 1 秒");
        // 公开入口与按语种渲染逐字一致
        assert_eq!(format_eta(90, 1), eta_in(zh, 90, 1));
    }

    /// 文件类型 / 任务状态的显示名来自词表，而不是硬编码在 `t()` 的 match 臂里
    #[test]
    fn test_label_helpers_use_table() {
        use tianyi_core::file::FileKind;
        use tianyi_core::task::TaskStatus;

        assert_eq!(
            label_kind(FileKind::Other),
            lookup_in(Lang::ZhHans, "kind.other").unwrap()
        );
        assert_eq!(
            label_kind(FileKind::Image),
            lookup_in(Lang::ZhHans, "kind.image").unwrap()
        );
        assert_eq!(
            label_status(TaskStatus::Pending),
            lookup_in(Lang::ZhHans, "task.pending").unwrap()
        );
        assert_eq!(
            label_status(TaskStatus::Completed),
            lookup_in(Lang::ZhHans, "task.completed").unwrap()
        );
    }
}
