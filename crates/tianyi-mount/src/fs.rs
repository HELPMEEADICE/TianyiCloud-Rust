//! 平台无关的"云盘文件系统"语义
//!
//! 这一层把"路径 → 云端对象 → 字节区间"的流程做完，平台层只负责翻译系统调用：
//!
//! 1. **路径解析**：逐段命中目录缓存，命中即返回，绝不遍历整棵云盘树；
//! 2. **目录缓存**：TTL + 容量上限，只缓存元数据（见 [crate::meta]）；
//! 3. **随机读取**：按 [offset, offset+len) 走严格 Range（见 [crate::reader]）；
//! 4. **直链缓存**：短期复用 + 失效刷新，避免每读一次都要一次鉴权请求；
//! 5. **只读**：任何写语义在这里就没有入口，平台层直接把写操作映射成只读错误。
//!
//! 刻意不缓存文件内容：既没有磁盘缓存，也没有跨请求的内容内存缓存
//! （只有句柄级的单次返回，交给操作系统后即释放）。

use crate::error::{Error, Result};
use crate::meta::{build_listing, DirCache, DirListing, Entry};
use crate::reader::{DirectLink, RangeReader, ReadLimits};
use crate::session::CloudSession;
use crate::stats::{ReadStats, StatsSnapshot};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tianyi_core::{FileObject, Space};

/// 后台（云端）能力的最小接口
///
/// 抽成 trait 的唯一理由是**可测**：单元测试可以塞一个纯内存的假后端，
/// 把路径解析、缓存、Range 校验、只读语义全部跑一遍，而不需要真天翼云账号。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait CloudBackend: Send + Sync {
    fn list<'a>(&'a self, folder_id: &'a str) -> BoxFut<'a, Result<Vec<FileObject>>>;
    fn download_url<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, Result<String>>;
    fn capacity<'a>(&'a self) -> BoxFut<'a, Result<(u64, u64)>>;
}

impl CloudBackend for CloudSession {
    fn list<'a>(&'a self, folder_id: &'a str) -> BoxFut<'a, Result<Vec<FileObject>>> {
        Box::pin(async move { self.list_raw(folder_id).await.map_err(Error::from) })
    }

    fn download_url<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, Result<String>> {
        Box::pin(async move { self.download_url(file_id).await.map_err(Error::from) })
    }

    fn capacity<'a>(&'a self) -> BoxFut<'a, Result<(u64, u64)>> {
        Box::pin(async move {
            self.capacity()
                .await
                .ok_or_else(|| Error::Other("容量不可用".into()))
        })
    }
}

/// 挂载参数（平台无关部分）
#[derive(Debug, Clone)]
pub struct FsOptions {
    pub limits: ReadLimits,
    /// 目录缓存 TTL
    pub dir_ttl: Duration,
    /// 目录缓存容量（目录数）
    pub dir_capacity: usize,
    /// 下载直链缓存时长（天翼云直链通常十几分钟有效，这里保守取 5 分钟）
    pub url_ttl: Duration,
    /// 卷标（资源管理器里显示成「天翼云盘 (X:)」）
    pub volume_label: String,
}

impl Default for FsOptions {
    fn default() -> Self {
        Self {
            limits: ReadLimits::default(),
            dir_ttl: crate::meta::DEFAULT_TTL,
            dir_capacity: crate::meta::DEFAULT_CAPACITY,
            url_ttl: Duration::from_secs(300),
            volume_label: "天翼云盘".to_string(),
        }
    }
}

pub struct CloudFs {
    backend: Arc<dyn CloudBackend>,
    dirs: DirCache,
    reader: RangeReader,
    urls: Mutex<HashMap<String, DirectLink>>,
    stats: Arc<ReadStats>,
    capacity: Mutex<Option<(u64, u64, Instant)>>,
    label: String,
    root_id: String,
    space: Space,
    username: String,
    mounted_at: SystemTime,
    url_ttl: Duration,
}

impl std::fmt::Debug for CloudFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudFs")
            .field("label", &self.label)
            .field("space", &self.space)
            .field("cached_dirs", &self.dirs.len())
            .finish()
    }
}

impl CloudFs {
    pub fn new(
        backend: Arc<dyn CloudBackend>,
        space: Space,
        username: impl Into<String>,
        opts: FsOptions,
    ) -> Result<Self> {
        let stats = Arc::new(ReadStats::new());
        let reader = RangeReader::new(opts.limits.clone(), stats.clone())?;
        Ok(Self {
            backend,
            dirs: DirCache::new(opts.dir_ttl, opts.dir_capacity),
            reader,
            urls: Mutex::new(HashMap::new()),
            stats,
            capacity: Mutex::new(None),
            label: opts.volume_label,
            root_id: tianyi_core::cloudpath::ROOT_FOLDER_ID.to_string(),
            space,
            username: username.into(),
            mounted_at: SystemTime::now(),
            url_ttl: opts.url_ttl,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn space(&self) -> Space {
        self.space
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn mounted_at(&self) -> SystemTime {
        self.mounted_at
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    pub fn memory_budget(&self) -> u64 {
        self.reader.limits().memory_budget()
    }

    pub fn cached_dir_count(&self) -> usize {
        self.dirs.len()
    }

    /// 根目录条目（供 GetFileInfo/根 open 使用）
    pub fn root_entry(&self) -> Entry {
        Entry {
            id: self.root_id.clone(),
            name: String::new(),
            raw_name: String::new(),
            is_dir: true,
            size: 0,
            created: self.mounted_at,
            modified: self.mounted_at,
        }
    }

    /// 清空全部缓存（云端数据可能被其他客户端改过时调用）
    pub fn invalidate_all(&self) {
        self.dirs.clear();
        self.lock_urls().clear();
        *self.lock_capacity() = None;
    }

    /// 路径 → 条目
    pub async fn resolve(&self, segments: &[String]) -> Result<Entry> {
        if segments.is_empty() {
            return Ok(self.root_entry());
        }
        let mut folder_id = self.root_id.clone();
        for (idx, seg) in segments.iter().enumerate() {
            let listing = self.listing(&folder_id).await?;
            let Some(entry) = listing.lookup(seg) else {
                return Err(Error::NotFound(format!(
                    "{} 不存在",
                    crate::paths::display_path(segments)
                )));
            };
            if idx + 1 == segments.len() {
                return Ok(entry.clone());
            }
            if !entry.is_dir {
                return Err(Error::NotFound(format!(
                    "{} 不是目录",
                    crate::paths::display_path(&segments[..=idx])
                )));
            }
            folder_id = entry.id.clone();
        }
        unreachable!("循环里必然返回")
    }

    /// 路径 → 目录列表
    pub async fn list_dir(&self, segments: &[String]) -> Result<Arc<DirListing>> {
        let entry = self.resolve(segments).await?;
        if !entry.is_dir {
            return Err(Error::NotFound(format!(
                "{} 不是目录",
                crate::paths::display_path(segments)
            )));
        }
        self.listing(&entry.id).await
    }

    /// 目录 id → 目录列表（缓存优先）
    pub async fn listing(&self, folder_id: &str) -> Result<Arc<DirListing>> {
        if let Some(hit) = self.dirs.get(folder_id)? {
            return Ok(hit);
        }
        self.listing_refresh(folder_id).await
    }

    /// 强制刷新一个目录
    ///
    /// 刷新失败但手里有旧数据时**回退到旧数据**并记一条 warn：
    /// 网络抖动不该让整个目录在资源管理器里变成"空文件夹"。
    pub async fn listing_refresh(&self, folder_id: &str) -> Result<Arc<DirListing>> {
        match self.backend.list(folder_id).await {
            Ok(items) => {
                let listing = build_listing(folder_id, items);
                self.dirs.put(listing);
                self.dirs
                    .get(folder_id)?
                    .ok_or_else(|| Error::Other("目录写入缓存后立刻读不到".into()))
            }
            Err(e) => match self.dirs.get_stale(folder_id) {
                Some(stale) => {
                    log::warn!("刷新目录 {folder_id} 失败，回退到缓存快照：{}", e.brief());
                    self.stats.record_error(e.brief());
                    Ok(stale)
                }
                None => {
                    self.stats.record_error(e.brief());
                    Err(e)
                }
            },
        }
    }

    /// 读一个文件区间（唯一的内容入口）
    pub async fn read(&self, entry: &Entry, offset: u64, len: u32) -> Result<Vec<u8>> {
        if entry.is_dir {
            return Err(Error::ReadOnly("目录不能按文件读取".into()));
        }
        if len == 0 || offset >= entry.size {
            return Ok(Vec::new());
        }
        let mut link = self.link_for(&entry.id).await?;
        let backend = self.backend.clone();
        let id = entry.id.clone();
        let outcome = self
            .reader
            .read(&mut link, entry.size, offset, len, || {
                let backend = backend.clone();
                let id = id.clone();
                async move { backend.download_url(&id).await }
            })
            .await;
        // 直链被刷新过就存回去，避免每个 Range 都重新申请
        self.put_link(&entry.id, link);

        if let Err(e) = &outcome {
            match e {
                // 文件大小变了 / 文件被删了：缓存里的元数据已经不可信
                Error::RangeMismatch(msg) if msg.contains("文件大小") => self.invalidate_all(),
                Error::Cloud(tianyi_core::Error::NotFound(_)) => self.invalidate_all(),
                _ => {}
            }
        }
        outcome
    }

    /// 容量（缓存 60 秒；失败返回 (0,0) 而不是让挂载失败）
    pub async fn capacity(&self) -> (u64, u64) {
        {
            let guard = self.lock_capacity();
            if let Some((total, free, at)) = *guard {
                if at.elapsed() < Duration::from_secs(60) {
                    return (total, free);
                }
            }
        }
        match self.backend.capacity().await {
            Ok((total, free)) => {
                *self.lock_capacity() = Some((total, free, Instant::now()));
                (total, free)
            }
            Err(e) => {
                log::debug!("容量查询失败：{}", e.brief());
                (0, 0)
            }
        }
    }

    async fn link_for(&self, file_id: &str) -> Result<DirectLink> {
        {
            let guard = self.lock_urls();
            if let Some(link) = guard.get(file_id) {
                if link.is_fresh() {
                    return Ok(link.clone());
                }
            }
        }
        let url = self.backend.download_url(file_id).await?;
        let link = DirectLink::new(url, self.url_ttl);
        self.put_link(file_id, link.clone());
        Ok(link)
    }

    fn put_link(&self, file_id: &str, link: DirectLink) {
        let mut guard = self.lock_urls();
        // 直链表按文件数增长，加上限避免长时间挂载后无限膨胀
        if guard.len() > 4096 && !guard.contains_key(file_id) {
            guard.clear();
        }
        guard.insert(file_id.to_string(), link);
    }

    fn lock_urls(&self) -> std::sync::MutexGuard<'_, HashMap<String, DirectLink>> {
        self.urls.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_capacity(&self) -> std::sync::MutexGuard<'_, Option<(u64, u64, Instant)>> {
        self.capacity.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn obj(id: &str, name: &str, is_dir: bool, size: i64) -> FileObject {
        FileObject {
            id: id.into(),
            name: name.into(),
            size,
            md5: String::new(),
            parent_id: String::new(),
            is_dir,
            last_op_time: "2026-11-08 23:55:23".into(),
            create_date: "2026-11-08 23:55:23".into(),
            thumb: String::new(),
        }
    }

    struct MockBackend {
        dirs: Mutex<HashMap<String, Vec<FileObject>>>,
        url: String,
        list_calls: AtomicUsize,
        url_calls: AtomicUsize,
        cap_fail: AtomicBool,
    }

    impl MockBackend {
        fn new(url: &str) -> Arc<Self> {
            let mut dirs = HashMap::new();
            dirs.insert(
                "-11".to_string(),
                vec![
                    obj("d1", "文档", true, 0),
                    obj("f1", "a.txt", false, 16),
                    obj("f2", "A-copy.TXT", false, 8),
                ],
            );
            dirs.insert("d1".to_string(), vec![obj("f3", "inner.bin", false, 32)]);
            Arc::new(Self {
                dirs: Mutex::new(dirs),
                url: url.to_string(),
                list_calls: AtomicUsize::new(0),
                url_calls: AtomicUsize::new(0),
                cap_fail: AtomicBool::new(false),
            })
        }
        fn list_calls(&self) -> usize {
            self.list_calls.load(Ordering::SeqCst)
        }
        fn url_calls(&self) -> usize {
            self.url_calls.load(Ordering::SeqCst)
        }
    }

    impl CloudBackend for MockBackend {
        fn list<'a>(&'a self, folder_id: &'a str) -> BoxFut<'a, Result<Vec<FileObject>>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            let items = self.dirs.lock().unwrap().get(folder_id).cloned();
            Box::pin(async move {
                items.ok_or_else(|| Error::Cloud(tianyi_core::Error::NotFound("no dir".into())))
            })
        }
        fn download_url<'a>(&'a self, _file_id: &'a str) -> BoxFut<'a, Result<String>> {
            self.url_calls.fetch_add(1, Ordering::SeqCst);
            let url = self.url.clone();
            Box::pin(async move { Ok(url) })
        }
        fn capacity<'a>(&'a self) -> BoxFut<'a, Result<(u64, u64)>> {
            let fail = self.cap_fail.load(Ordering::SeqCst);
            Box::pin(async move {
                if fail {
                    Err(Error::Other("容量接口挂了".into()))
                } else {
                    Ok((100, 40))
                }
            })
        }
    }

    /// 极简 Range 服务：`partial` 为 false 时永远返回 200 整包
    async fn spawn_range_server(data: Vec<u8>, partial: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let data = data.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 512];
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let range = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split_once('='))
                        .map(|(_, v)| v.trim().to_string());
                    let total = data.len();
                    if !partial || range.is_none() {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&data).await;
                        return;
                    }
                    let spec = range.unwrap();
                    let (s, e) = spec.split_once('-').unwrap();
                    let start = s.parse::<usize>().unwrap();
                    let stop = (e.parse::<usize>().unwrap() + 1).min(total);
                    let slice = data[start..stop].to_vec();
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        start + slice.len() - 1,
                        slice.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&slice).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn fs_with(backend: Arc<MockBackend>, opts: FsOptions) -> CloudFs {
        CloudFs::new(backend, Space::Personal, "tester", opts).unwrap()
    }

    #[tokio::test]
    async fn resolve_walks_directories_case_insensitively() {
        let backend = MockBackend::new("http://127.0.0.1:1/none");
        let fs = fs_with(backend.clone(), FsOptions::default());
        let root = fs.resolve(&[]).await.unwrap();
        assert!(root.is_dir);
        let doc = fs.resolve(&["文档".into()]).await.unwrap();
        assert_eq!(doc.id, "d1");
        let inner = fs
            .resolve(&["文档".into(), "INNER.BIN".into()])
            .await
            .unwrap();
        assert_eq!(inner.id, "f3");
        let miss = fs.resolve(&["不存在".into()]).await.unwrap_err();
        assert!(matches!(miss, Error::NotFound(_)));
    }

    #[tokio::test]
    async fn metadata_is_cached_and_ttl_is_respected() {
        let backend = MockBackend::new("http://127.0.0.1:1/none");
        let fs = fs_with(backend.clone(), FsOptions::default());
        fs.resolve(&["文档".into()]).await.unwrap();
        fs.resolve(&["文档".into()]).await.unwrap();
        assert_eq!(backend.list_calls(), 1, "第二次解析必须命中缓存");
    }

    #[tokio::test]
    async fn read_only_touches_the_requested_slice() {
        // a.txt 在 mock 后端里声明为 16 字节，服务端数据必须一致，
        // 否则会命中"文件大小已变化"的严格校验（这正是我们想要的保护）
        let data: Vec<u8> = (0..16u8).collect();
        let base = spawn_range_server(data.clone(), true).await;
        let backend = MockBackend::new(&format!("{base}/file"));
        let fs = fs_with(backend.clone(), FsOptions::default());
        let entry = fs.resolve(&["a.txt".into()]).await.unwrap();
        let out = fs.read(&entry, 8, 4).await.unwrap();
        assert_eq!(out, data[8..12].to_vec());
    }

    #[tokio::test]
    async fn direct_link_is_reused_between_reads() {
        let data = vec![1u8; 16];
        let base = spawn_range_server(data, true).await;
        let backend = MockBackend::new(&format!("{base}/file"));
        let fs = fs_with(backend.clone(), FsOptions::default());
        let entry = fs.resolve(&["a.txt".into()]).await.unwrap();
        fs.read(&entry, 0, 4).await.unwrap();
        fs.read(&entry, 4, 4).await.unwrap();
        assert_eq!(backend.url_calls(), 1, "直链必须复用");
    }

    #[tokio::test]
    async fn server_ignoring_range_is_an_error_not_a_full_download() {
        let data = vec![9u8; 4096];
        let base = spawn_range_server(data, false).await;
        let backend = MockBackend::new(&format!("{base}/file"));
        let fs = fs_with(backend, FsOptions::default());
        let entry = fs.resolve(&["a.txt".into()]).await.unwrap();
        let err = fs.read(&entry, 0, 16).await.unwrap_err();
        assert!(matches!(err, Error::RangeUnsupported { .. }), "{err}");
    }

    #[tokio::test]
    async fn reads_beyond_eof_are_empty_and_zero_length_is_free() {
        let data = vec![2u8; 16];
        let base = spawn_range_server(data, true).await;
        let backend = MockBackend::new(&format!("{base}/file"));
        let fs = fs_with(backend.clone(), FsOptions::default());
        let entry = fs.resolve(&["a.txt".into()]).await.unwrap();
        assert!(fs.read(&entry, 16, 8).await.unwrap().is_empty());
        assert!(fs.read(&entry, 0, 0).await.unwrap().is_empty());
        assert_eq!(backend.url_calls(), 0, "越界与零长度读取不该申请直链");
    }

    #[tokio::test]
    async fn directory_reads_of_a_file_path_fail() {
        let backend = MockBackend::new("http://127.0.0.1:1/none");
        let fs = fs_with(backend, FsOptions::default());
        let err = fs.list_dir(&["a.txt".into()]).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[tokio::test]
    async fn capacity_failure_does_not_break_the_mount() {
        let backend = MockBackend::new("http://127.0.0.1:1/none");
        backend.cap_fail.store(true, Ordering::SeqCst);
        let fs = fs_with(backend, FsOptions::default());
        assert_eq!(fs.capacity().await, (0, 0));
    }

    #[tokio::test]
    async fn stale_snapshot_is_used_when_refresh_fails() {
        let backend = MockBackend::new("http://127.0.0.1:1/none");
        let fs = fs_with(backend.clone(), FsOptions::default());
        fs.resolve(&["文档".into()]).await.unwrap();
        // 让后端"挂掉"
        backend.dirs.lock().unwrap().clear();
        let listing = fs.listing_refresh("-11").await.unwrap();
        assert_eq!(listing.len(), 3, "刷新失败必须回退到旧快照");
    }

    #[tokio::test]
    async fn invalidate_all_drops_every_cache() {
        let backend = MockBackend::new("http://127.0.0.1:1/none");
        let fs = fs_with(backend.clone(), FsOptions::default());
        fs.resolve(&["文档".into()]).await.unwrap();
        assert_eq!(fs.cached_dir_count(), 1);
        fs.invalidate_all();
        assert_eq!(fs.cached_dir_count(), 0);
        fs.resolve(&["文档".into()]).await.unwrap();
        assert_eq!(backend.list_calls(), 2);
    }
}
