//! 日志落盘：stderr + 按天滚动的日志文件
//!
//! 用户报问题时只有 stderr 是拿不到东西的——GUI 启动方式下 stderr 往往没有终点。
//! 所以这里做一个 `log::Log` 的扇出（fan-out）：一条记录同时交给 `env_logger`
//! （保持原有 stderr 行为不变）和“配置目录/logs/tianyi-cloud.YYYY-MM-DD.log”。
//!
//! 三条硬约束：
//! * 任何一步失败都不 panic：日志系统故障不应该让客户端起不来；
//! * 降级后 stderr 照旧工作，调用方感知不到差异，只是没有文件；
//! * 全局 logger 只能安装一次，装不上（例如同一进程里重复初始化）只打印一行。
//!
//! 为什么不直接用 `chrono`：`tianyi-app` 没有声明该依赖（只有 `tianyi-core` 声明了），
//! 而本模块所在的改动不允许碰 `Cargo.toml` / `Cargo.lock`。这里需要的只是“本地年月日
//! 与时分秒”，于是用民用历法公式自实现（Howard Hinnant 的 days_from_civil /
//! civil_from_days），与本用途下的 `chrono::NaiveDate` 等价。

use log::{Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tianyi_core::config::AppStore;

/// 日志文件名前缀
const FILE_PREFIX: &str = "tianyi-cloud.";
/// 日志文件名后缀
const FILE_SUFFIX: &str = ".log";
/// 保留天数：严格超过这个天数的旧日志在启动时删除
const KEEP_DAYS: i64 = 7;
/// 单个日志文件的体积上限
///
/// 只按天滚动挡不住"一天就把磁盘写满"：调试级日志 + 长时间挂机时单日文件可以到几百 MB。
/// 超过这个上限就滚成 `.log.1`，并把历史逐级后移，最多留 [`MAX_HISTORY`] 份。
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// 单日最多保留的历史分段数（`.log.1` … `.log.3`）
const MAX_HISTORY: usize = 3;

/// 已解析好的日志目录（`init` 与设置页必须看到同一个路径）
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

// ======================================================================
// 目录
// ======================================================================

/// 日志目录：`<配置目录>/logs`
///
/// 正常情况下这个值由 [`init`] 在启动时按 `ConfigStore` 解析出的配置根目录写入（见
/// `main.rs`），所以它和设置页展示的「配置目录」永远是同一个来源。
///
/// 直接调用 `log_dir()` 而没先 `init`（例如单测）时，这里退化为 `AppStore::new(None)`
/// 的默认根目录：它是唯一不会和控制器漂移的写法。
pub fn log_dir() -> PathBuf {
    LOG_DIR.get_or_init(resolve_log_dir).clone()
}

/// 解析日志目录；配置目录完全不可用时仍然返回一个可展示的路径（写入会退化为 stderr）
fn resolve_log_dir() -> PathBuf {
    match AppStore::new(None) {
        Ok(store) => store.root().join("logs"),
        Err(e) => {
            // 与 AppStore 的默认解析规则保持一致的兜底路径（Windows 上
            // `dirs::config_dir()` 与 `dirs::data_dir()` 是同一个 %APPDATA%）
            let fallback = dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tianyi-cloud-rust")
                .join("logs");
            degrade(&fallback, &e);
            fallback
        }
    }
}

/// 按显式给定的配置根目录确定日志目录，并把它固化下来
///
/// 路径必须由调用方从 `ConfigStore::open()` 的 `root()` 算出来，而不是这里再猜一遍默认
/// 路径：配置目录不可写时 `ConfigStore` 会退到 `%TEMP%\tianyi-cloud-fallback`，日志如果
/// 还按主路径解析，设置页上「配置目录」与「日志目录」就会指向两个地方（上一轮的缺陷）。
fn log_dir_under(root: &Path) -> PathBuf {
    LOG_DIR.get_or_init(|| root.join("logs")).clone()
}

// ======================================================================
// 日期
// ======================================================================

/// 日志用到的“本地日期”（只有年月日）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogDate {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl LogDate {
    /// 距离 1970-01-01 的天数（更早的日期为负）
    fn days_from_epoch(self) -> i64 {
        let y = if self.month <= 2 {
            self.year - 1
        } else {
            self.year
        };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400; // [0, 399]
        let m = self.month as i64;
        let mp = if m > 2 { m - 3 } else { m + 9 }; // 三月是 0
        let doy = (153 * mp + 2) / 5 + self.day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
        era * 146_097 + doe - 719_468
    }

    /// `days_from_epoch` 的逆运算
    fn from_days(days: i64) -> Self {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        Self {
            year: if month <= 2 { y + 1 } else { y },
            month,
            day,
        }
    }
}

/// 当天的日志文件名：`tianyi-cloud.YYYY-MM-DD.log`
pub fn log_file_name_for(date: LogDate) -> String {
    format!(
        "{FILE_PREFIX}{:04}-{:02}-{:02}{FILE_SUFFIX}",
        date.year, date.month, date.day
    )
}

/// 该日志文件是否应当被清理：**严格**超过 [`KEEP_DAYS`] 天
///
/// 未来日期（系统时钟被往回调过）不清理：删掉反而会丢日志。
pub fn should_prune(file_date: LogDate, today: LogDate) -> bool {
    today.days_from_epoch() - file_date.days_from_epoch() > KEEP_DAYS
}

/// 写入前判断：已有字节 + 本次要写的字节是否超过单文件上限
///
/// 用「大于」而不是「大于等于」：正好写满上限的那一条仍然落盘，否则日志会在整数边界上
/// 反复滚动，每次都白白 rename 三份历史文件。
pub fn should_rotate(current_size: u64, incoming: usize, limit: u64) -> bool {
    current_size.saturating_add(incoming as u64) > limit
}

/// 历史分段的文件名：`tianyi-cloud.2024-05-06.log` + 1 → `tianyi-cloud.2024-05-06.log.1`
fn history_path(base: &Path, n: usize) -> PathBuf {
    let name = base
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.with_file_name(format!("{name}.{n}"))
}

/// 从文件名反解日期；不是本程序的日志文件时返回 `None`
///
/// 滚动的历史分段（`….log.2`）与主文件同属一天，保留期按同一条线算，否则历史分段会在
/// 目录里永远留着。
fn parse_log_date(name: &str) -> Option<LogDate> {
    let main = match name.strip_suffix(FILE_SUFFIX) {
        Some(main) => main,
        None => {
            let (head, tail) = name.rsplit_once('.')?;
            if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            head.strip_suffix(FILE_SUFFIX)?
        }
    };
    let body = main.strip_prefix(FILE_PREFIX)?;
    let mut parts = body.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(LogDate { year, month, day })
}

// ======================================================================
// 本地时间
// ======================================================================

/// 当前的本地日期与时分秒
fn now_local() -> (LogDate, u32, u32, u32) {
    #[cfg(target_os = "windows")]
    if let Some(now) = windows_local_now() {
        return now;
    }
    // 兜底（以及非 Windows 平台）：UTC 时间戳 + 本地时区偏移
    date_time_from_epoch(now_epoch_secs() + local_offset_secs())
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 Unix 时间戳拆成 (日期, 时, 分, 秒)
fn date_time_from_epoch(epoch_secs: i64) -> (LogDate, u32, u32, u32) {
    let days = epoch_secs.div_euclid(86_400);
    let rem = epoch_secs.rem_euclid(86_400);
    (
        LogDate::from_days(days),
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// 本地时区相对 UTC 的秒偏移
///
/// Windows 直接读本地时间（`GetLocalTime`，内核共享数据），不需要偏移量；
/// 其它平台在首次使用时执行一次 `date +%z` 取偏移，失败按 0（UTC）处理。
#[cfg(target_os = "windows")]
fn local_offset_secs() -> i64 {
    0
}

#[cfg(not(target_os = "windows"))]
fn local_offset_secs() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|out| parse_offset(&String::from_utf8_lossy(&out.stdout)))
            .unwrap_or(0)
    })
}

/// 解析 `date +%z` 的输出（`+0800` / `-0530`）为秒偏移
#[cfg(not(target_os = "windows"))]
fn parse_offset(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    if rest.len() != 4 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = rest[0..2].parse().ok()?;
    let minutes: i64 = rest[2..4].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// `SYSTEMTIME`（winbase.h）只用到前 8 个字段
///
/// 名字带 `W` 后缀是为了不与 `std::time::SystemTime` 撞名。
#[cfg(target_os = "windows")]
#[repr(C)]
struct SystemTimeW {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[cfg(target_os = "windows")]
fn windows_local_now() -> Option<(LogDate, u32, u32, u32)> {
    extern "system" {
        fn GetLocalTime(out: *mut SystemTimeW);
    }
    let mut st = SystemTimeW {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    // SAFETY: GetLocalTime 只写入调用方提供的 SYSTEMTIME；指针来自本函数的栈变量，
    // 布局与 winbase.h 的 SYSTEMTIME 一致（8 个 u16）。
    unsafe { GetLocalTime(&mut st) };
    if st.year == 0 || st.month == 0 || st.day == 0 {
        return None;
    }
    Some((
        LogDate {
            year: i64::from(st.year),
            month: u32::from(st.month),
            day: u32::from(st.day),
        },
        u32::from(st.hour),
        u32::from(st.minute),
        u32::from(st.second),
    ))
}

// ======================================================================
// 文件写入器（不依赖全局 logger，可直接单测）
// ======================================================================

/// 按天滚动的日志文件写入器
pub struct FileWriter {
    dir: PathBuf,
    date: LogDate,
    file: File,
}

impl FileWriter {
    /// 打开（必要时创建目录与文件）当天的日志文件，只追加不截断
    pub fn open(dir: &Path, date: LogDate) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(log_file_name_for(date)))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            date,
            file,
        })
    }

    /// 当前文件对应的日期（跨天时据此换文件）
    fn date(&self) -> LogDate {
        self.date
    }

    /// 当前文件路径（失败信息里用得上）
    fn path(&self) -> PathBuf {
        self.dir.join(log_file_name_for(self.date))
    }

    /// 当前文件已写字数（取不到元数据时按 0：宁可少滚一次，也不要因为读不到大小停写）
    fn size(&self) -> u64 {
        self.file.metadata().map(|m| m.len()).unwrap_or(0)
    }

    /// 把当前文件滚成 `.1`，历史逐级后移，最老的一份丢弃
    fn rotate(&mut self) -> std::io::Result<()> {
        let base = self.path();
        let _ = self.file.sync_data();
        // 先删最老的一份，再把 .N 逐级后移，最后让主文件变成 .1：总数因此永远是 ≤ MAX_HISTORY
        let _ = std::fs::remove_file(history_path(&base, MAX_HISTORY));
        for n in (1..MAX_HISTORY).rev() {
            let from = history_path(&base, n);
            if from.exists() {
                let _ = std::fs::rename(from, history_path(&base, n + 1));
            }
        }
        std::fs::rename(&base, history_path(&base, 1))?;
        // 句柄跟着文件名走：不重新打开就会继续写进刚被滚走的那一份
        self.file = OpenOptions::new().create(true).append(true).open(&base)?;
        Ok(())
    }

    /// 写入一条 `log` 记录并立刻刷盘
    ///
    /// 每条都 flush：用户能够报上来的日志，往往就是程序上次崩溃前后那几行，
    /// 留在用户态缓冲区里的内容等同于不存在。
    pub fn write_record(&mut self, record: &Record<'_>) -> std::io::Result<()> {
        let (date, hour, minute, second) = now_local();
        let line = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} [{}] {}: {}\n",
            date.year,
            date.month,
            date.day,
            hour,
            minute,
            second,
            record.level(),
            record.target(),
            record.args()
        );
        self.append(&line, MAX_FILE_BYTES)
    }

    /// 追加一行文本；`limit` 显式传入是为了单测能压到几十字节就验证滚动，
    /// 而不必真的写满 8 MiB
    fn append(&mut self, line: &str, limit: u64) -> std::io::Result<()> {
        if should_rotate(self.size(), line.len(), limit) {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.file.flush()
    }

    /// 把内核缓冲区也落下去（`Log::flush` 用）
    fn sync(&self) {
        let _ = self.file.sync_data();
    }
}

/// 保留期清理：删除 `dir` 下超过保留期的日志文件，返回被删掉的路径
///
/// 只处理本程序命名的文件（`tianyi-cloud.YYYY-MM-DD.log`），目录里的其它东西一律不动。
pub fn prune_old(dir: &Path, today: LogDate) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(date) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_log_date)
        else {
            continue;
        };
        if should_prune(date, today) && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

// ======================================================================
// 扇出：stderr + 文件
// ======================================================================

/// 文件侧状态
#[derive(Default)]
struct FileState {
    writer: Option<FileWriter>,
    /// 已经放弃写文件（打开或写入失败）：不再重试，也不刷屏
    stopped: bool,
}

/// 一条记录同时送 `env_logger`（stderr）与日志文件
struct Fanout {
    stderr: env_logger::Logger,
    dir: PathBuf,
    state: Mutex<FileState>,
}

impl Fanout {
    fn write_file(&self, record: &Record<'_>) {
        let (date, ..) = now_local();
        // 中毒只说明之前有别的线程在持锁期间 panic 过：日志系统不该因此停摆
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(e) => e.into_inner(),
        };
        if state.stopped {
            return;
        }
        // 跨天（或首次写入）：换到当天的文件
        if state.writer.as_ref().map(FileWriter::date) != Some(date) {
            match FileWriter::open(&self.dir, date) {
                Ok(writer) => state.writer = Some(writer),
                Err(e) => {
                    degrade(&self.dir, &e);
                    state.stopped = true;
                    return;
                }
            }
        }
        let Some(writer) = state.writer.as_mut() else {
            return;
        };
        if let Err(e) = writer.write_record(record) {
            let path = writer.path();
            degrade(&path, &e);
            state.stopped = true;
            state.writer = None;
        }
    }
}

impl Log for Fanout {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // 过滤规则（RUST_LOG）完全交给 env_logger，行为与改动前的 `init()` 一致
        self.stderr.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.stderr.enabled(record.metadata()) {
            return;
        }
        self.stderr.log(record);
        self.write_file(record);
    }

    fn flush(&self) {
        self.stderr.flush();
        if let Ok(state) = self.state.lock() {
            if let Some(writer) = state.writer.as_ref() {
                writer.sync();
            }
        }
    }
}

/// 记录一次“落盘不可用”
///
/// 这里用 `eprintln!` 而不是 `log::warn!`：失败可能发生在 logger 安装的过程中
/// （此时全局 logger 还没就位，warn 会被直接丢掉），也可能发生在 logger 自己的
/// `log()` 内部（递归回来没有任何收益）。stderr 一定在工作，所以直接写它。
fn degrade(path: &Path, err: impl std::fmt::Display) {
    eprintln!(
        "[tianyi] 日志文件不可用（{}）：{err}；本次运行只输出到 stderr",
        path.display()
    );
}

/// 初始化日志：stderr（沿用 `RUST_LOG`，默认 `info`）+ 按天滚动、按体积滚动的日志文件
///
/// `root` 是配置根目录（调用方从 `ConfigStore` 拿到），日志落在 `<root>/logs`——日志目录
/// 与设置页展示的配置目录因此同源，配置目录降级到临时目录时日志也跟着走。
///
/// 返回日志目录。即使落盘失败也返回它：设置页那一行与“打开”按钮照常可用。
pub fn init(root: &Path) -> PathBuf {
    let dir = log_dir_under(root);
    let today = now_local().0;
    // 保留期清理：此时 logger 还没装，结果是给人看的调试信息，失败静默
    let pruned = prune_old(&dir, today).len();

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    let stderr = builder.build();
    let max_level = stderr.filter();

    // 先试开一次文件：失败就在安装 logger 之前把话说清楚，然后退化为“仅 stderr”。
    // 这样启动之后不会再因为同一次故障刷出成百上千行。
    let (writer, stopped) = match FileWriter::open(&dir, today) {
        Ok(writer) => (Some(writer), false),
        Err(e) => {
            degrade(&dir, &e);
            (None, true)
        }
    };

    let fanout = Fanout {
        stderr,
        dir: dir.clone(),
        state: Mutex::new(FileState { writer, stopped }),
    };
    match log::set_boxed_logger(Box::new(fanout)) {
        Ok(()) => {
            log::set_max_level(max_level);
            if pruned > 0 {
                log::info!("已清理 {pruned} 个超过 {KEEP_DAYS} 天的日志文件");
            }
        }
        Err(e) => eprintln!("[tianyi] 日志器已安装过（{e}），本次不再重复安装"),
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立的临时目录（同进程内测试并行执行，必须按用例名区分）
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tianyi-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn log_file_name_for_uses_iso_date() {
        assert_eq!(
            log_file_name_for(LogDate {
                year: 2024,
                month: 5,
                day: 6
            }),
            "tianyi-cloud.2024-05-06.log"
        );
        // 单位数月份/日期补零，文件名可以直接按字典序排序
        assert_eq!(
            log_file_name_for(LogDate {
                year: 2024,
                month: 1,
                day: 9
            }),
            "tianyi-cloud.2024-01-09.log"
        );
    }

    #[test]
    fn should_prune_only_after_seven_days() {
        let today = LogDate {
            year: 2024,
            month: 5,
            day: 14,
        };
        let days_ago = |n: i64| LogDate::from_days(today.days_from_epoch() - n);
        // 恰好 7 天：保留（边界是“超过 7 天”）
        assert!(!should_prune(days_ago(7), today));
        assert!(!should_prune(days_ago(6), today));
        assert!(!should_prune(today, today));
        // 8 天及以上：清理
        assert!(should_prune(days_ago(8), today));
        assert!(should_prune(days_ago(365), today));
        // 未来日期（时钟被回拨）：不清理
        assert!(!should_prune(days_ago(-3), today));
    }

    #[test]
    fn log_date_round_trips_and_matches_epoch() {
        // 1970-01-01 是第 0 天，两个方向必须互逆
        assert_eq!(
            LogDate {
                year: 1970,
                month: 1,
                day: 1
            }
            .days_from_epoch(),
            0
        );
        assert_eq!(LogDate::from_days(0).year, 1970);
        assert_eq!(LogDate::from_days(0).month, 1);
        assert_eq!(LogDate::from_days(0).day, 1);
        // 闰日必须被正确表示
        let leap = LogDate {
            year: 2024,
            month: 2,
            day: 29,
        };
        assert_eq!(LogDate::from_days(leap.days_from_epoch()), leap);
        // 纪元时间戳拆分：0 → 1970-01-01 00:00:00
        assert_eq!(
            date_time_from_epoch(0),
            (
                LogDate {
                    year: 1970,
                    month: 1,
                    day: 1
                },
                0,
                0,
                0
            )
        );
        assert_eq!(
            date_time_from_epoch(1_700_000_000),
            (
                LogDate {
                    year: 2023,
                    month: 11,
                    day: 14
                },
                22,
                13,
                20
            )
        );
    }

    #[test]
    fn parse_log_date_ignores_foreign_files() {
        assert_eq!(
            parse_log_date("tianyi-cloud.2024-05-06.log"),
            Some(LogDate {
                year: 2024,
                month: 5,
                day: 6
            })
        );
        assert_eq!(parse_log_date("notes.txt"), None);
        assert_eq!(parse_log_date("tianyi-cloud.2024-13-06.log"), None);
        assert_eq!(parse_log_date("tianyi-cloud.2024-05.log"), None);
        assert_eq!(parse_log_date("tianyi-cloud.2024-05-06.log.bak"), None);
    }

    #[test]
    fn prune_old_removes_only_expired_log_files() {
        let dir = temp_dir("prune");
        let today = LogDate {
            year: 2024,
            month: 5,
            day: 14,
        };
        let expired = LogDate {
            year: 2024,
            month: 5,
            day: 6,
        }; // 8 天前
        let kept = LogDate {
            year: 2024,
            month: 5,
            day: 7,
        }; // 7 天前
        std::fs::create_dir_all(&dir).expect("创建临时日志目录");
        for date in [expired, kept, today] {
            std::fs::write(dir.join(log_file_name_for(date)), b"x").expect("写入日志文件");
        }
        std::fs::write(dir.join("notes.txt"), b"x").expect("写入无关文件");

        let removed = prune_old(&dir, today);
        assert_eq!(removed.len(), 1, "只有 8 天前那一个该被删");
        assert!(!dir.join(log_file_name_for(expired)).exists());
        assert!(dir.join(log_file_name_for(kept)).exists());
        assert!(dir.join(log_file_name_for(today)).exists());
        assert!(dir.join("notes.txt").exists(), "目录里的无关文件不能动");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_appends_message_to_file_in_temp_dir() {
        let dir = temp_dir("writer");
        let date = LogDate {
            year: 2024,
            month: 5,
            day: 6,
        };
        let mut writer = FileWriter::open(&dir, date).expect("临时目录应能创建并打开日志文件");

        // 直接构造 Record：不依赖全局 logger，也不需要任何初始化
        assert!(writer
            .write_record(
                &Record::builder()
                    .args(format_args!("hello log file"))
                    .level(log::Level::Info)
                    .target("tianyi_app::logging::tests")
                    .build()
            )
            .is_ok());
        assert!(writer
            .write_record(
                &Record::builder()
                    .args(format_args!("second line"))
                    .level(log::Level::Warn)
                    .target("tianyi_app::logging::tests")
                    .build()
            )
            .is_ok());

        let path = dir.join("tianyi-cloud.2024-05-06.log");
        let text = std::fs::read_to_string(&path).expect("日志文件应存在且可读");
        assert!(
            text.contains("hello log file"),
            "文件内容应包含 message，实际：{text}"
        );
        // 追加语义：第二条不能覆盖第一条
        assert!(text.contains("second line"), "追加写入，实际：{text}");
        assert!(text.contains("INFO") && text.contains("WARN"));
        assert!(text.contains("tianyi_app::logging::tests"));
        assert_eq!(text.lines().count(), 2, "一行一条记录");

        // 同一天再次打开不得截断既有内容
        drop(writer);
        let mut again = FileWriter::open(&dir, date).expect("重新打开当天文件");
        let _ = again.write_record(
            &Record::builder()
                .args(format_args!("third line"))
                .level(log::Level::Info)
                .target("tianyi_app::logging::tests")
                .build(),
        );
        let text = std::fs::read_to_string(&path).expect("日志文件应存在且可读");
        assert!(text.contains("hello log file") && text.contains("third line"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- 单文件体积滚动 ----------------

    #[test]
    fn should_rotate_only_past_the_limit() {
        let limit = 1024;
        // 远小于上限：不滚
        assert!(!should_rotate(0, 10, limit));
        assert!(!should_rotate(64, 128, limit));
        // 写完这一条正好等于上限：仍然落盘（宁可写满，也不要在边界上反复滚动）
        assert!(!should_rotate(1000, 24, limit));
        // 超过上限：滚
        assert!(should_rotate(1000, 25, limit));
        // 已经满/超了，再来任何一条都得先滚
        assert!(should_rotate(1024, 1, limit));
        assert!(should_rotate(4096, 1, limit));
        // 空写入不触发
        assert!(!should_rotate(1024, 0, limit));
    }

    #[test]
    fn rotation_keeps_at_most_three_history_files() {
        let dir = temp_dir("rotate");
        let date = LogDate {
            year: 2024,
            month: 5,
            day: 6,
        };
        let mut writer = FileWriter::open(&dir, date).expect("打开日志文件");
        // 上限压到 40 字节：每写两条就滚一次，12 条足够滚出 3 份以上历史
        for i in 0..12 {
            writer
                .append(&format!("line-{i}-0123456789\n"), 40)
                .expect("写入一行");
        }
        let base = log_file_name_for(date);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("读取日志目录")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for n in 1..=MAX_HISTORY {
            assert!(
                names.contains(&format!("{base}.{n}")),
                "缺少历史分段 .{n}：{names:?}"
            );
        }
        assert!(
            !names.contains(&format!("{base}.{}", MAX_HISTORY + 1)),
            "最多保留 {MAX_HISTORY} 份历史：{names:?}"
        );
        assert!(names.contains(&base), "主文件必须还在：{names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_today_log_and_its_rotated_segments() {
        let dir = temp_dir("prune-rotated");
        let today = LogDate {
            year: 2024,
            month: 5,
            day: 14,
        };
        let expired = LogDate {
            year: 2024,
            month: 5,
            day: 6,
        }; // 8 天前
        std::fs::create_dir_all(&dir).expect("创建临时日志目录");
        let today_log = log_file_name_for(today);
        let expired_log = log_file_name_for(expired);
        // 当天：主文件 + 两个历史分段（体积滚动的产物）
        for name in [
            today_log.clone(),
            format!("{today_log}.1"),
            format!("{today_log}.2"),
        ] {
            std::fs::write(dir.join(name), b"x").expect("写入当天日志");
        }
        // 过期：主文件 + 一个历史分段
        for name in [expired_log.clone(), format!("{expired_log}.1")] {
            std::fs::write(dir.join(name), b"x").expect("写入过期日志");
        }

        let removed = prune_old(&dir, today);
        assert_eq!(removed.len(), 2, "过期的两份都该删，实际：{removed:?}");
        assert!(
            dir.join(&today_log).exists(),
            "当天主文件不能被历史分段误伤"
        );
        assert!(dir.join(format!("{today_log}.1")).exists());
        assert!(dir.join(format!("{today_log}.2")).exists());
        assert!(!dir.join(&expired_log).exists());
        assert!(!dir.join(format!("{expired_log}.1")).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 日志目录永远挂在调用方给的配置根目录下（设置页那两行因此不会漂）
    #[test]
    fn log_dir_follows_the_given_config_root() {
        let root = temp_dir("root-follow");
        // 主配置目录不可写时 `ConfigStore` 给的是 %TEMP%\tianyi-cloud-fallback，
        // 日志必须跟着落进那个目录的 logs 子目录，而不是留在原处
        let dir = log_dir_under(&root);
        assert_eq!(dir, root.join("logs"));
        // 设置页读的是 log_dir()：必须与 init 固化的那个值完全一致
        assert_eq!(log_dir(), dir);
    }
}
