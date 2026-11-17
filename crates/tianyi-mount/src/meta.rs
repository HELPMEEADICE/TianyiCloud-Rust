//! 目录元数据模型与缓存
//!
//! 缓存的边界在这里划得很清楚：**只缓存元数据**（名字、大小、时间戳），
//! 文件内容一个字节都不缓存到磁盘，也不在这里长留。
//!
//! 设计要点：
//!
//! * 目录条目在构造时一次性算好「显示名 + 大小写不敏感索引」，
//!   之后每次 lookup 都是纯内存哈希查找，不再碰网络；
//! * 有 TTL（默认 30 秒）与容量上限（默认 64 个目录），过期/超限即丢弃；
//! * 目录列表只在「首次访问该目录」时拉取，绝不预先遍历整棵云盘树。

use crate::error::Result;
use crate::paths;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tianyi_core::FileObject;

/// 默认目录缓存 TTL
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);
/// 默认缓存目录数
pub const DEFAULT_CAPACITY: usize = 64;

/// 一个云盘对象在文件系统视角下的样子
#[derive(Debug, Clone)]
pub struct Entry {
    /// 云端 id（文件夹 id 或文件 id）
    pub id: String,
    /// 呈现给操作系统的名字（已清洗、已去重）
    pub name: String,
    /// 云端原名（诊断用，也用于日志）
    pub raw_name: String,
    pub is_dir: bool,
    pub size: u64,
    pub created: SystemTime,
    pub modified: SystemTime,
}

impl Entry {
    /// 稳定的文件索引号
    ///
    /// 用它而不是数组下标：Explorer 会用 IndexNumber 判断「是不是同一个文件」，
    /// 下标会在目录刷新后错位，导致选中态与图标缓存错乱。
    pub fn index_number(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for b in self.id.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        // 0 在 NTFS 语义里是「未分配」，避开它
        if hash == 0 {
            1
        } else {
            hash
        }
    }
}

/// 一个目录的完整条目集合（含大小写不敏感索引）
#[derive(Debug)]
pub struct DirListing {
    pub folder_id: String,
    pub entries: Vec<Entry>,
    by_key: HashMap<String, usize>,
    fetched: Instant,
}

impl DirListing {
    /// 大小写不敏感查找（Windows 上的预期行为）
    pub fn lookup(&self, name: &str) -> Option<&Entry> {
        self.by_key
            .get(&paths::lookup_key(name))
            .and_then(|i| self.entries.get(*i))
    }

    /// 位置查找（ReadDirectory 的 marker 是「上一条的名字」，需要知道从哪继续）
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.by_key.get(&paths::lookup_key(name)).copied()
    }

    pub fn fetched_at(&self) -> Instant {
        self.fetched
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 由云端列表构造目录条目
///
/// 排序与去重必须**确定性**：同一份云端数据每次都要得到同样的显示名，
/// 否则「a (1).txt」这种去重名会在两次刷新之间互换，Explorer 会认为文件变了。
pub fn build_listing(folder_id: &str, mut items: Vec<FileObject>) -> DirListing {
    // 目录在前，然后按小写名字，最后用 id 兜底保证全序
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| paths::lookup_key(&a.name).cmp(&paths::lookup_key(&b.name)))
            .then_with(|| a.id.cmp(&b.id))
    });

    let raw_names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    let display = paths::unique_display_names(&raw_names);

    let mut entries = Vec::with_capacity(items.len());
    let mut by_key = HashMap::with_capacity(items.len());
    for (idx, obj) in items.into_iter().enumerate() {
        let created = parse_cloud_time(&obj.create_date)
            .or_else(|| parse_cloud_time(&obj.last_op_time))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let modified = parse_cloud_time(&obj.last_op_time).unwrap_or(created);
        let name = display[idx].clone();
        by_key.insert(paths::lookup_key(&name), idx);
        entries.push(Entry {
            id: obj.id,
            name,
            raw_name: obj.name,
            is_dir: obj.is_dir,
            size: if obj.is_dir {
                0
            } else {
                obj.size.max(0) as u64
            },
            created,
            modified,
        });
    }

    DirListing {
        folder_id: folder_id.to_string(),
        entries,
        by_key,
        fetched: Instant::now(),
    }
}

/// 目录缓存：TTL + 容量上限
#[derive(Debug)]
pub struct DirCache {
    inner: Mutex<Inner>,
    ttl: Duration,
    capacity: usize,
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<String, (Arc<DirListing>, u64)>,
    seq: u64,
}

impl DirCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// 命中且未过期才返回
    pub fn get(&self, folder_id: &str) -> Result<Option<Arc<DirListing>>> {
        let mut g = self.lock();
        let Some((listing, _seq)) = g.map.get(folder_id) else {
            return Ok(None);
        };
        if listing.fetched_at().elapsed() > self.ttl {
            // 刻意不删除：刷新失败时 [DirCache::get_stale] 还要用它兜底
            return Ok(None);
        }
        let listing = listing.clone();
        g.seq = g.seq.wrapping_add(1);
        let seq_now = g.seq;
        if let Some(slot) = g.map.get_mut(folder_id) {
            slot.1 = seq_now;
        }
        Ok(Some(listing))
    }

    /// 不看 TTL 直接取（刷新失败时回退用，避免一次网络抖动让目录整个消失）
    pub fn get_stale(&self, folder_id: &str) -> Option<Arc<DirListing>> {
        self.lock()
            .map
            .get(folder_id)
            .map(|(listing, _)| listing.clone())
    }

    pub fn put(&self, listing: DirListing) {
        let mut g = self.lock();
        g.seq = g.seq.wrapping_add(1);
        let seq = g.seq;
        g.map
            .insert(listing.folder_id.clone(), (Arc::new(listing), seq));
        while g.map.len() > self.capacity {
            // 淘汰最久未使用的（seq 最小）。容量是 64 量级，线性扫描足够。
            let victim = g
                .map
                .iter()
                .min_by_key(|(_, (_, s))| *s)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    g.map.remove(&k);
                }
                None => break,
            }
        }
    }

    /// 手动刷新（GUI 的「刷新」按钮 / CLI 的 mount refresh）
    pub fn invalidate(&self, folder_id: &str) {
        self.lock().map.remove(folder_id);
    }

    pub fn clear(&self) {
        let mut g = self.lock();
        g.map.clear();
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 解析天翼云返回的时间字符串
///
/// 云端给的是北京时间（UTC+8）的 `YYYY-MM-DD HH:MM:SS`，不带时区。这里按 UTC+8 解释，
/// 否则资源管理器里所有文件都会差 8 小时。
pub fn parse_cloud_time(raw: &str) -> Option<SystemTime> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(SystemTime::UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y/%m/%d %H:%M:%S"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
    let dt = naive.and_local_timezone(offset).single()?;
    let secs = dt.timestamp();
    if secs < 0 {
        return Some(SystemTime::UNIX_EPOCH);
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// 把 SystemTime 转成 Windows FILETIME（1601-01-01 起的 100 纳秒数）
///
/// 只给 WinFsp 的 FSP_FSCTL_FILE_INFO 用；放在平台无关模块里是因为它是纯算术，
/// 方便单测（Linux 上也能替 WinFsp 的换算做回归）。
pub fn to_filetime(t: SystemTime) -> u64 {
    /// 1970-01-01 与 1601-01-01 之间相差的 100ns 计数
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => EPOCH_DIFF_100NS.saturating_add(d.as_nanos() as u64 / 100),
        Err(_) => EPOCH_DIFF_100NS,
    }
}

#[cfg(test)]
fn object(id: &str, name: &str, is_dir: bool, size: i64) -> FileObject {
    FileObject {
        id: id.to_string(),
        name: name.to_string(),
        size,
        md5: String::new(),
        parent_id: String::new(),
        is_dir,
        last_op_time: "2026-11-08 23:55:23".to_string(),
        create_date: "2026-11-08 23:55:23".to_string(),
        thumb: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_puts_directories_first_and_indexes_case_insensitively() {
        let items = vec![
            object("2", "b.txt", false, 10),
            object("1", "A DIR", true, 0),
            object("3", "a.txt", false, 20),
        ];
        let listing = build_listing("-11", items);
        assert!(listing.entries[0].is_dir);
        assert_eq!(listing.entries[0].name, "A DIR");
        assert_eq!(listing.lookup("a dir").unwrap().id, "1");
        assert_eq!(listing.lookup("A.TXT").unwrap().id, "3");
        assert_eq!(listing.index_of("a.txt"), Some(1));
        assert!(listing.lookup("missing").is_none());
    }

    #[test]
    fn index_numbers_are_stable_and_nonzero() {
        let a = Entry {
            id: "12345".into(),
            name: "x".into(),
            raw_name: "x".into(),
            is_dir: false,
            size: 1,
            created: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
        };
        assert_ne!(a.index_number(), 0);
        assert_eq!(a.index_number(), a.index_number());
        let mut b = a.clone();
        b.id = "12346".into();
        assert_ne!(a.index_number(), b.index_number());
    }

    #[test]
    fn zero_byte_files_and_empty_dirs_are_representable() {
        let listing = build_listing(
            "-11",
            vec![object("d", "dir", true, 0), object("f", "empty", false, 0)],
        );
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].size, 0);
        assert_eq!(listing.entries[1].size, 0);
    }

    #[test]
    fn cache_honours_capacity() {
        let cache = DirCache::new(Duration::from_secs(60), 2);
        for i in 0..5 {
            cache.put(build_listing(
                &format!("d{i}"),
                vec![object(&format!("f{i}"), "n", false, 1)],
            ));
        }
        assert!(cache.len() <= 2, "容量上限必须生效");
    }

    #[test]
    fn cache_returns_same_listing_until_invalidated() {
        let cache = DirCache::new(Duration::from_secs(60), 8);
        cache.put(build_listing("-11", vec![object("1", "a", false, 1)]));
        let first = cache.get("-11").unwrap().unwrap();
        let second = cache.get("-11").unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        cache.invalidate("-11");
        assert!(cache.get("-11").unwrap().is_none());
        assert!(cache.get_stale("-11").is_none());
    }

    #[test]
    fn expired_entries_are_not_returned_but_stale_access_works() {
        let cache = DirCache::new(Duration::from_millis(1), 8);
        cache.put(build_listing("-11", vec![object("1", "a", false, 1)]));
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            cache.get("-11").unwrap().is_none(),
            "过期条目不能再被当作新鲜数据"
        );
        assert!(cache.get_stale("-11").is_some(), "刷新失败时可回退到旧数据");
    }

    #[test]
    fn cloud_times_are_interpreted_as_utc8() {
        let t = parse_cloud_time("2026-11-08 23:55:23").unwrap();
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // 2026-11-08T15:55:23Z
        assert_eq!(secs, 1794153323);
        assert!(parse_cloud_time("not a date").is_none());
        assert!(parse_cloud_time("").is_none());
    }

    #[test]
    fn filetime_conversion_matches_known_value() {
        // Unix 纪元在 FILETIME 里是 116444736000000000
        assert_eq!(to_filetime(SystemTime::UNIX_EPOCH), 116_444_736_000_000_000);
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        assert_eq!(to_filetime(t), 116_444_736_000_000_000 + 10_000_000);
    }
}
