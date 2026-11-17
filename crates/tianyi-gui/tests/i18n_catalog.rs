//! 四语 i18n 目录完整性回归测试（测试名统一以 `i18n_catalog_` 开头，便于 `--skip i18n_catalog` 过滤）
//!
//! # 为什么必须有这组测试
//!
//! 界面文案有两套产地、两套目录，任何一处漏项都**不会**在运行时报错，只会静默降级：
//!
//! - `crates/tianyi-app/lang/<lang>/LC_MESSAGES/tianyi-app.po`：给 `.slint` 里的
//!   `@tr("…")` 用，由 `build.rs` 的 `with_bundled_translations("lang")` 在编译期打包。
//!   msgid 就是简体中文原文，查不到时 gettext 回退到翻译表**索引 0**（= msgid 本身），
//!   于是「漏翻」的表现是**英文界面上冒出中文**，而不是编译错误或 panic。
//! - `crates/tianyi-app/i18n/rs/<lang>.json`：给 Rust 侧 `format!` 拼出来的运行时文案用
//!   （状态栏、提示条、任务状态、ETA……这些不经过 Slint 表达式，`@tr` 管不到）。
//!   查词顺序是「当前语种 → 简中 → 键名」，漏项同样静默回退到中文。
//!
//! 于是三类回归全靠人眼：源码新写了 `@tr` 却没补 `.po`、文案删了 `.po` 里还留着、
//! 四份词表键集或占位符漂移。本测试把这三类钉成断言，把「静默回退」变成红灯：
//!
//! - A 组 `i18n_catalog_slint_msgids_are_translated`：源码里每条 `@tr` 在三个语种的
//!   `.po` 里都存在且 `msgstr` 非空。
//! - B 组 `i18n_catalog_po_matches_source_exactly`：三份 `.po` 的 msgid 集合与源码扫描
//!   结果**完全相等**（不多不少），防「文案删了、翻译留着」的僵尸条目。
//! - C 组 `i18n_catalog_runtime_tables_agree`：四份运行时词表键集相同、无空值、
//!   具名占位符集合四语一致；简中键数 ≥ 100；英文值里不含 CJK 字符。
//! - D 组 `i18n_catalog_lang_layout_is_clean`：`lang/zh-Hans` 不存在（简中必须走索引 0），
//!   且每个语言目录都有 `LC_MESSAGES/tianyi-app.po`。
//!
//! # 边界
//!
//! `tianyi-app` 是 binary-only crate（没有 lib 目标），集成测试无法 `use tianyi_app::…`，
//! 所以这里只做**文件系统检查**：自己扫 `.slint`、自己解 `.po`。JSON 用 `serde_json`
//! （已是本 crate 的普通依赖），不引入新依赖、不改 `Cargo.toml`。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// `.slint` 文案必须覆盖的语种（简中 = msgid 原文 = 索引 0，没有自己的 `.po`）
const PO_LANGS: [&str; 3] = ["en", "zh-Hant", "ja"];

/// 运行时词表四份 JSON，顺序即下表索引：0 = 简中（唯一完整的那份）
const JSON_LANGS: [&str; 4] = ["zh-Hans", "en", "zh-Hant", "ja"];

/// 简中词表至少该有的条目数（只增不减的下限，防「静默漏登记一大片」）
const ZH_HANS_MIN_KEYS: usize = 100;

/// 失败信息里最多列出多少条明细
const MAX_LISTED: usize = 20;

fn app_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn po_path(lang: &str) -> PathBuf {
    app_dir()
        .join("lang")
        .join(lang)
        .join("LC_MESSAGES")
        .join("tianyi-app.po")
}

fn json_path(lang: &str) -> PathBuf {
    app_dir()
        .join("i18n")
        .join("rs")
        .join(format!("{lang}.json"))
}

/// 拼一段「前 20 条明细」的错误信息尾部
fn listed(items: &[String]) -> String {
    let mut out = String::new();
    for item in items.iter().take(MAX_LISTED) {
        out.push_str("\n  - ");
        out.push_str(item);
    }
    if items.len() > MAX_LISTED {
        out.push_str(&format!(
            "\n  …（共 {} 条，此处只列前 {MAX_LISTED} 条）",
            items.len()
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// .slint 扫描
// ---------------------------------------------------------------------------

/// 递归收集 `ui/` 下的 `.slint`
///
/// 跳过任何名为 `dev` 的目录：`ui/dev/` 是 `examples/icon_sheet.rs` 的自检素材，
/// 不进产品界面，它的文案也不该进 `.po`（`tools/i18n-extract.ps1` 同一口径）。
fn collect_slint(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("无法读取目录 {}：{e}", dir.display()));
    for entry in entries {
        let path = entry.expect("读取目录项失败").path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("dev") {
                continue;
            }
            collect_slint(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("slint") {
            out.push(path);
        }
    }
}

/// 扫描全部 `.slint`，返回「msgid → 首次出现位置（相对路径:行号）」
fn slint_msgids() -> BTreeMap<String, String> {
    let ui = app_dir().join("ui");
    let mut files = Vec::new();
    collect_slint(&ui, &mut files);
    assert!(
        !files.is_empty(),
        "{} 下没有找到任何 .slint 文件，测试的前置假设已失效",
        ui.display()
    );

    let mut msgids = BTreeMap::new();
    for file in &files {
        let rel = file
            .strip_prefix(&ui)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        scan_slint_file(file, &rel, &mut msgids);
    }
    msgids
}

/// 从一个 `.slint` 文件里抽出所有 `@tr("…")` 的第一个字符串字面量
///
/// - 整行 `//` 注释跳过（注释里的 `@tr` 不是文案）；
/// - `@tr("…", 参数…)` 只取第一个字面量，第二个参数（如 `count`）不参与；
/// - Slint 字符串只有 `\"` 与 `\\` 两种转义，这里手工还原，避免在 `\"` 处截断；
/// - 同一行可以有多个 `@tr`（如 `@tr("松开上传到 {0}", … ? @tr("当前目录") : …)`），
///   所以命中一条后从字面量结尾继续扫，而不是跳过整行。
fn scan_slint_file(path: &Path, rel: &str, out: &mut BTreeMap<String, String>) {
    let src =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("无法读取 {}：{e}", path.display()));
    let chars: Vec<char> = src.chars().collect();
    let mut line = 1usize;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }

        // 整行注释：行首缩进后的第一个非空白字符就是 `/`，整行跳过
        if i == 0 || chars[i - 1] == '\n' {
            let mut j = i;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            if j + 1 < chars.len() && chars[j] == '/' && chars[j + 1] == '/' {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
        }

        if c == '@'
            && i + 3 < chars.len()
            && chars[i + 1] == 't'
            && chars[i + 2] == 'r'
            && chars[i + 3] == '('
        {
            let mut k = i + 4;
            while k < chars.len() && (chars[k] == ' ' || chars[k] == '\t') {
                k += 1;
            }
            assert!(
                k < chars.len() && chars[k] == '"',
                "{rel}:{line}: `@tr(` 后面不是字符串字面量，扫描器需要更新"
            );
            k += 1;

            let start_line = line;
            let mut msgid = String::new();
            loop {
                assert!(
                    k < chars.len(),
                    "{rel}:{start_line}: `@tr` 的字符串字面量没有闭合"
                );
                match chars[k] {
                    '\\' if k + 1 < chars.len()
                        && (chars[k + 1] == '"' || chars[k + 1] == '\\') =>
                    {
                        msgid.push(chars[k + 1]);
                        k += 2;
                    }
                    '\\' => {
                        // 其它转义（如 `\n`）原样保留：msgid 必须与 .po 逐字一致
                        msgid.push('\\');
                        k += 1;
                    }
                    '"' => {
                        k += 1;
                        break;
                    }
                    '\n' => panic!(
                        "{rel}:{start_line}: `@tr` 的字符串字面量跨行，本扫描器按行解析，\
                         请把文案写成一行（.po 的 msgid 也是单行）"
                    ),
                    other => {
                        msgid.push(other);
                        k += 1;
                    }
                }
            }
            out.entry(msgid)
                .or_insert_with(|| format!("{rel}:{start_line}"));
            i = k;
            continue;
        }

        i += 1;
    }
}

// ---------------------------------------------------------------------------
// .po 解析
// ---------------------------------------------------------------------------

/// 一条 `.po` 记录
struct PoEntry {
    /// 译文（可能为空串：等于没翻）
    msgstr: String,
    /// `msgstr` 所在行号，报错时能直接定位
    line: usize,
}

/// 解析 `.po`，返回「msgid → 记录」
///
/// 只处理本项目实际用到的形状：`msgid "…"` / `msgstr "…"`、`#` 注释行、空行分隔、
/// 以及 `"…"` 续行（本项目都是单行，但续行一律拼接而不是崩溃）。头部条目
/// （`msgid ""` + `Content-Type` 等元信息）直接丢弃，它不是界面文案。
fn parse_po(path: &Path) -> BTreeMap<String, PoEntry> {
    let src =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("无法读取 {}：{e}", path.display()));
    let mut out = BTreeMap::new();
    let mut cur_id: Option<(String, usize)> = None;
    let mut cur_str: Option<(String, usize)> = None;

    for (idx, raw) in src.lines().enumerate() {
        let line_no = idx + 1;
        let text = raw.trim_end_matches('\r').trim_start();

        if text.is_empty() {
            flush_po(&mut out, cur_id.take(), cur_str.take());
            continue;
        }
        if text.starts_with('#') {
            continue;
        }
        if let Some(rest) = text.strip_prefix("msgid ") {
            // 正常由空行分隔；这里再兜一次，避免缺空行时把两条糊成一条
            flush_po(&mut out, cur_id.take(), cur_str.take());
            cur_id = Some((po_literal(rest, path, line_no), line_no));
            continue;
        }
        if let Some(rest) = text.strip_prefix("msgstr ") {
            cur_str = Some((po_literal(rest, path, line_no), line_no));
            continue;
        }
        if text.starts_with('"') {
            // 续行：优先拼到 msgstr（头部的 Content-Type 等走这条），否则拼到 msgid
            let piece = po_literal(text, path, line_no);
            if let Some((s, _)) = cur_str.as_mut() {
                s.push_str(&piece);
            } else if let Some((s, _)) = cur_id.as_mut() {
                s.push_str(&piece);
            }
            continue;
        }
        panic!("{}:{line_no}: 无法识别的 .po 行：{text}", path.display());
    }
    flush_po(&mut out, cur_id, cur_str);
    out
}

fn flush_po(
    out: &mut BTreeMap<String, PoEntry>,
    id: Option<(String, usize)>,
    s: Option<(String, usize)>,
) {
    let Some((msgid, _)) = id else { return };
    let (msgstr, line) = s.unwrap_or_default();
    if msgid.is_empty() {
        // 头部条目：`msgid ""` + Content-Type / Language / Plural-Forms
        return;
    }
    out.insert(msgid, PoEntry { msgstr, line });
}

/// 解 `"…"` 字面量，只还原 `\"` 与 `\\`（其余转义原样保留，免得改动 msgid 的比较基准）
fn po_literal(rest: &str, path: &Path, line_no: usize) -> String {
    let text = rest.trim_start();
    assert!(
        text.starts_with('"'),
        "{}:{line_no}: .po 前缀后面不是字符串字面量：{text}",
        path.display()
    );
    let mut out = String::new();
    let mut chars = text.chars();
    chars.next();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => panic!("{}:{line_no}: .po 字符串以 `\\` 结尾", path.display()),
            },
            '"' => return out,
            other => out.push(other),
        }
    }
    panic!("{}:{line_no}: .po 字符串字面量没有闭合", path.display())
}

// ---------------------------------------------------------------------------
// 运行时词表
// ---------------------------------------------------------------------------

/// 载入 `i18n/rs/<lang>.json`（键 → 文案）
fn load_table(lang: &str) -> BTreeMap<String, String> {
    let path = json_path(lang);
    let src = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("无法读取词表 {}：{e}", path.display()));
    serde_json::from_str::<BTreeMap<String, String>>(&src).unwrap_or_else(|e| {
        panic!(
            "词表 {} 不是合法的「键 → 字符串」JSON 对象：{e}",
            path.display()
        )
    })
}

/// 抽出具名占位符：`{`/`}` 之间只由 ASCII 小写字母与下划线组成的 token
///
/// 位置参数 `{0}`、数字、带空格或中文的片段都不算——本项目运行时文案只用
/// `{name}` / `{count}` / `{e}` 这类具名占位符。
fn placeholders(text: &str) -> BTreeSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = BTreeSet::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '{' && i + 1 < chars.len() {
            if let Some(offset) = chars[i + 1..].iter().position(|&c| c == '}' || c == '{') {
                let end = i + 1 + offset;
                if chars[end] == '}' {
                    let token: String = chars[i + 1..end].iter().collect();
                    if !token.is_empty()
                        && token.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    {
                        out.insert(token);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// 英文词表里不该出现的 CJK 统一表意文字
fn contains_cjk(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

// ---------------------------------------------------------------------------
// A 组：Slint 侧全覆盖
// ---------------------------------------------------------------------------

/// A. 源码里每条 `@tr` 在 `en` / `zh-Hant` / `ja` 的 `.po` 里都存在且 `msgstr` 非空
///
/// 防的回归：新写一条 `@tr("…")` 忘了补翻译。gettext 找不到就回退到 msgid（简中原文），
/// 于是英文界面上会突然冒出一句中文，而构建、运行、现有测试全绿。
#[test]
fn i18n_catalog_slint_msgids_are_translated() {
    let msgids = slint_msgids();
    assert!(
        !msgids.is_empty(),
        "扫描 ui/**/*.slint 没有抽到任何 @tr 文案，扫描器或目录结构已经变了"
    );

    // 三个语种的缺项 / 空译文合并成一张清单：早先按语种分别断言时，前一个语种的「缺
    // msgid」会先炸，把后面语种的「msgstr 为空」整个遮住，一次红灯只能看见一处问题。
    let mut problems = Vec::new();
    for lang in PO_LANGS {
        let path = po_path(lang);
        let po = parse_po(&path);
        for (msgid, loc) in &msgids {
            match po.get(msgid) {
                None => problems.push(format!("{lang} 缺 msgid：「{msgid}」（源码 {loc}）")),
                Some(entry) if entry.msgstr.trim().is_empty() => problems.push(format!(
                    "{lang} 的 msgstr 为空：「{msgid}」（源码 {loc}，{}:{}）",
                    path.display(),
                    entry.line
                )),
                Some(_) => {}
            }
        }
    }

    assert!(
        problems.is_empty(),
        "共 {} 处界面文案没有对应译文（{} 条 msgid × {} 个语种），这些位置会静默回退成中文：{}",
        problems.len(),
        msgids.len(),
        PO_LANGS.len(),
        listed(&problems)
    );
}

// ---------------------------------------------------------------------------
// B 组：PO 与源码互不多余
// ---------------------------------------------------------------------------

/// B. 三份 `.po` 的 msgid 集合与源码扫描结果**完全相等**（不多不少）
///
/// 防的回归：文案被删或被改写后，`.po` 里那条旧 msgid 成了僵尸条目。它不会再被任何
/// `@tr` 命中，翻译者却会一直维护它；同时也是 A 组扫描器的自检——两边集合不相等，
/// 说明扫描口径和 `.po` 的产出口径已经分叉。
#[test]
fn i18n_catalog_po_matches_source_exactly() {
    let msgids = slint_msgids();
    let source: BTreeSet<String> = msgids.keys().cloned().collect();

    let mut problems = Vec::new();
    for lang in PO_LANGS {
        let path = po_path(lang);
        let po = parse_po(&path);
        let translated: BTreeSet<String> = po.keys().cloned().collect();

        for msgid in source.difference(&translated) {
            let loc = msgids.get(msgid).map(String::as_str).unwrap_or("?");
            problems.push(format!("{lang} 缺：「{msgid}」（源码 {loc}）"));
        }
        for msgid in translated.difference(&source) {
            let entry = &po[msgid];
            problems.push(format!(
                "{lang} 多：「{msgid}」（{}:{}，源码里已经没有这条文案了，\
                 请同步删除或改回原文，别让翻译继续维护僵尸条目）",
                path.display(),
                entry.line
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "三份 .po 的 msgid 集合与源码扫描结果不是完全相等（共 {} 处差集）：{}",
        problems.len(),
        listed(&problems)
    );
}

// ---------------------------------------------------------------------------
// C 组：运行时词表四份一致
// ---------------------------------------------------------------------------

/// C. 四份运行时词表：键集相同、无空值、占位符四语一致、简中够厚、英文无 CJK
///
/// 防的回归：新加/删掉一条 Rust 侧文案时只改了一份 JSON；某条翻译漏填占位符
/// （英文 `Moved {name}` 写成 `Moved file`，`format!` 之后那一截就永远不显示）；
/// 某条英文条目其实是中文原文没翻。
#[test]
fn i18n_catalog_runtime_tables_agree() {
    let tables: Vec<(&str, BTreeMap<String, String>)> = JSON_LANGS
        .iter()
        .map(|lang| (*lang, load_table(lang)))
        .collect();
    let (zh_lang, zh) = &tables[0];

    // 1) 键集与简中完全一致（简中是唯一完整的那份，是基准）
    let zh_keys: BTreeSet<String> = zh.keys().cloned().collect();
    for (lang, table) in &tables[1..] {
        let keys: BTreeSet<String> = table.keys().cloned().collect();
        let missing: Vec<String> = zh_keys.difference(&keys).cloned().collect();
        let extra: Vec<String> = keys.difference(&zh_keys).cloned().collect();
        assert!(
            missing.is_empty(),
            "词表 {lang}（{}）比 {zh_lang} 少 {} 条键（这些文案在该语种下会静默回退成中文）：{}",
            json_path(lang).display(),
            missing.len(),
            listed(&missing)
        );
        assert!(
            extra.is_empty(),
            "词表 {lang}（{}）比 {zh_lang} 多 {} 条键（源码里没有这条文案，或简中漏登记）：{}",
            json_path(lang).display(),
            extra.len(),
            listed(&extra)
        );
    }

    // 2) 四份都不许有空值
    for (lang, table) in &tables {
        let empty: Vec<String> = table
            .iter()
            .filter(|(_, value)| value.trim().is_empty())
            .map(|(key, _)| key.clone())
            .collect();
        assert!(
            empty.is_empty(),
            "词表 {lang}（{}）有 {} 条空值（运行时会回退成中文原文）：{}",
            json_path(lang).display(),
            empty.len(),
            listed(&empty)
        );
    }

    // 3) 同一条键的具名占位符集合必须四语一致，否则有一个语种会丢参数
    for (key, zh_value) in zh {
        let expected = placeholders(zh_value);
        for (lang, table) in &tables[1..] {
            let Some(value) = table.get(key) else {
                // 键集差异已在上面的断言里报过，这里不重复炸
                continue;
            };
            let got = placeholders(value);
            assert_eq!(
                got,
                expected,
                "词表 {lang}（{}）的键 {key} 占位符对不上：{zh_lang} 是 {expected:?}「{zh_value}」，\
                 {lang} 是 {got:?}「{value}」——少一个占位符那条参数就永远不显示",
                json_path(lang).display()
            );
        }
    }

    // 4) 简中词表厚度下限：掉到 100 条以下说明有一大片文案没登记
    assert!(
        zh.len() >= ZH_HANS_MIN_KEYS,
        "简中词表（{}）只有 {} 条，应不少于 {ZH_HANS_MIN_KEYS} 条：有文案没登记",
        json_path(zh_lang).display(),
        zh.len()
    );

    // 5) 英文词表里不该出现中日韩统一表意文字（那就是没翻）
    let (en_lang, en) = &tables[1];
    let cjk: Vec<String> = en
        .iter()
        .filter(|(_, value)| contains_cjk(value))
        .map(|(key, value)| format!("{key} = 「{value}」"))
        .collect();
    assert!(
        cjk.is_empty(),
        "词表 {en_lang}（{}）有 {} 条值里含中日韩字符（等于没翻）：{}",
        json_path(en_lang).display(),
        cjk.len(),
        listed(&cjk)
    );
}

// ---------------------------------------------------------------------------
// D 组：目录卫生
// ---------------------------------------------------------------------------

/// D. `lang/zh-Hans` 不存在 + 每个语言目录都有 `LC_MESSAGES/tianyi-app.po`
///
/// 防的回归：有人「顺手」给简中建一个 `lang/zh-Hans/` 目录。简中就是 msgid 原文、
/// 就是翻译表的索引 0，多出这个目录会让回退语言被一份空 `.po` 顶掉（界面直接掉成
/// 空串或原文混杂）；而少一个 `LC_MESSAGES/tianyi-app.po` 会让运行时切语报
/// `LanguageNotFound`，且只有切到那门语言时才会发现。
#[test]
fn i18n_catalog_lang_layout_is_clean() {
    let lang_dir = app_dir().join("lang");

    let zh_hans_dir = lang_dir.join("zh-Hans");
    assert!(
        !zh_hans_dir.exists(),
        "{} 不该存在：简体中文 = msgid 原文 = 翻译表索引 0，没有独立 .po；\
         多出这个目录会盖掉回退语言（界面文案会变成空串或原文混杂）",
        zh_hans_dir.display()
    );

    let mut lang_dirs = Vec::new();
    let entries = fs::read_dir(&lang_dir)
        .unwrap_or_else(|e| panic!("无法读取目录 {}：{e}", lang_dir.display()));
    for entry in entries {
        let path = entry.expect("读取目录项失败").path();
        if path.is_dir() {
            lang_dirs.push(path);
        }
    }
    lang_dirs.sort();
    assert!(
        !lang_dirs.is_empty(),
        "{} 下一个语言目录都没有，翻译包无从打包",
        lang_dir.display()
    );

    for dir in &lang_dirs {
        let po = dir.join("LC_MESSAGES").join("tianyi-app.po");
        assert!(
            po.is_file(),
            "语言目录 {} 缺少 LC_MESSAGES/tianyi-app.po：\
             build.rs 的 with_bundled_translations(\"lang\") 只认这个固定路径，\
             运行时切到该语言会报 LanguageNotFound",
            dir.display()
        );
    }
}
