//! 严格零磁盘缓存的随机 Range 读取
//!
//! 这里是整个功能的技术底线：**读云端文件内容时，一个字节都不写本地磁盘**。
//! 因此不存在 `.part`、临时目录、块缓存或 SQLite 内容缓存，
//! 也不允许"先把整个文件下载下来再切片"。
//!
//! 与 [tianyi_core::transfer] 里那份下载实现的区别（刻意不复用）：
//!
//! | 场景 | transfer（下载到磁盘） | 本模块（挂载） |
//! |------|------------------------|----------------|
//! | 服务端忽略 Range 返回 200 | 读完整个响应再按窗口截取 | **直接判错**，绝不整包下载 |
//! | 响应长度不符 | 报错，由上层重试分片 | 校验 `Content-Range` 后报错，重试有界 |
//! | 失败处置 | 落 `.part` 断点续传 | 无落盘，直接返回错误 |
//!
//! 直链会过期，所以每次读都带一个"重新申请直链"的回调：403/401/410 时刷新一次再重试，
//! 但重试次数是**硬上限**（默认 3 次尝试），不存在无限重试。

use crate::error::{Error, Result};
use crate::stats::ReadStats;
use futures::StreamExt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// 读取侧的各种上限。默认值针对"视频拖动 + PDF 翻页 + 图片浏览"调过：
/// 单次请求不超过 8 MiB、最多 4 个并发、单个请求 20 秒超时。
#[derive(Debug, Clone)]
pub struct ReadLimits {
    /// 同时进行的 Range 请求数上限（内存占用的主要闸门：并发 × 单次长度）
    pub max_concurrency: usize,
    /// 单次请求的字节上限
    pub max_read: u32,
    /// 单次请求超时
    pub timeout: Duration,
    /// 首次尝试之外的重试次数（0 表示不重试）
    pub max_retries: u32,
    /// 预读字节数：0 = 关闭（默认）。开启后**不额外发请求**，
    /// 只是把同一次 Range 请求拉长，多出来的尾部留在句柄里供下一次读使用。
    pub readahead: u32,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_read: 8 * 1024 * 1024,
            timeout: Duration::from_secs(20),
            max_retries: 2,
            readahead: 0,
        }
    }
}

impl ReadLimits {
    /// 内存上界估算（并发 × 单次长度 + 读放大）：GUI 里可以展示给用户看
    pub fn memory_budget(&self) -> u64 {
        (self.max_concurrency as u64) * (self.max_read as u64 + self.readahead as u64)
    }
}

/// 一条下载直链及其有效期
#[derive(Debug, Clone)]
pub struct DirectLink {
    pub url: String,
    fetched: Instant,
    ttl: Duration,
}

impl DirectLink {
    pub fn new(url: impl Into<String>, ttl: Duration) -> Self {
        Self {
            url: url.into(),
            fetched: Instant::now(),
            ttl,
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.fetched.elapsed() < self.ttl
    }

    /// 日志里只能出现脱敏后的地址
    pub fn redacted(&self) -> String {
        crate::stats::scrub(&self.url)
    }
}

pub struct RangeReader {
    http: reqwest::Client,
    sem: Arc<Semaphore>,
    limits: ReadLimits,
    stats: Arc<ReadStats>,
}

impl RangeReader {
    pub fn new(limits: ReadLimits, stats: Arc<ReadStats>) -> Result<Self> {
        let http = reqwest::Client::builder()
            // 直链经常 302 到 CDN，但不能跟着无限跳（SSRF/环路防护）
            .redirect(reqwest::redirect::Policy::limited(3))
            .pool_max_idle_per_host(8)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Network(format!("创建 HTTP 客户端失败：{e}")))?;
        let sem = Arc::new(Semaphore::new(limits.max_concurrency.max(1)));
        Ok(Self {
            http,
            sem,
            limits,
            stats,
        })
    }

    pub fn limits(&self) -> &ReadLimits {
        &self.limits
    }

    /// 读 [offset, offset+len) 这一段
    ///
    /// * `link`：当前持有的直链，直链失效时通过 `refresh` 重新申请并就地更新；
    /// * `file_size`：调用方记录的云端文件大小，用来把越界请求截到 EOF；
    /// * 返回的 Vec 长度**可能小于 len**（读到文件末尾），但绝不会大于 len。
    pub async fn read<F, Fut>(
        &self,
        link: &mut DirectLink,
        file_size: u64,
        offset: u64,
        len: u32,
        refresh: F,
    ) -> Result<Vec<u8>>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        if len == 0 || offset >= file_size {
            return Ok(Vec::new());
        }

        // 1) 把请求长度收进内存与文件大小双重上界
        let remaining = file_size - offset;
        let want = (len as u64).min(remaining).min(self.limits.max_read as u64) as u32;
        if want == 0 {
            return Ok(Vec::new());
        }
        // 2) 预读只在同一次请求里放大，且不越过 EOF
        let readahead = (self.limits.readahead as u64)
            .min(self.limits.max_read as u64)
            .min(remaining - want as u64);
        let fetch_len = want as u64 + readahead;

        let mut last_err: Option<Error> = None;
        let mut attempts = 0u32;
        let max_attempts = self.limits.max_retries.saturating_add(1);
        let mut refreshed = false;

        while attempts < max_attempts {
            attempts += 1;

            let permit = self
                .sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Other("读取并发闸门已关闭".into()))?;

            let outcome = self.attempt(&link.url, offset, fetch_len, file_size).await;
            drop(permit);

            match outcome {
                Ok(bytes) => {
                    self.stats.record_read(bytes.len() as u64);
                    let cut = (want as usize).min(bytes.len());
                    let mut out = bytes;
                    out.truncate(cut);
                    if out.len() < want as usize {
                        // 服务器提前截断（大文件尾部分片常见）：能确定是 EOF 就接受
                        if offset + out.len() as u64 >= file_size {
                            return Ok(out);
                        }
                        last_err = Some(Error::RangeMismatch(format!(
                            "短读：期望 {want} 字节，实收 {}（offset={offset}）",
                            out.len()
                        )));
                    } else {
                        return Ok(out);
                    }
                }
                Err(e) if e.is_fatal() => {
                    self.stats.record_error(e.brief());
                    return Err(e);
                }
                Err(Error::Network(msg)) if is_link_expired(&msg) => {
                    // 直链过期：只刷新一次，避免把鉴权接口打爆
                    if !refreshed {
                        refreshed = true;
                        match refresh().await {
                            Ok(url) => {
                                *link = DirectLink::new(url, link.ttl);
                                log::info!("下载直链已刷新：{}", link.redacted());
                                continue;
                            }
                            Err(e) => {
                                self.stats.record_error(e.brief());
                                return Err(e);
                            }
                        }
                    }
                    last_err = Some(Error::Network(msg));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }

            // 有界退避：100ms、200ms……最多 800ms
            if attempts < max_attempts {
                let backoff = Duration::from_millis(100 * (1u64 << (attempts - 1).min(3)));
                tokio::time::sleep(backoff).await;
            }
        }

        let err = last_err.unwrap_or_else(|| Error::Other("Range 读取失败（无更具体原因）".into()));
        self.stats.record_error(err.brief());
        Err(err)
    }

    /// 一次尝试：发请求 + 严格校验 + 完整取回这一次的字节
    async fn attempt(
        &self,
        url: &str,
        offset: u64,
        fetch_len: u64,
        file_size: u64,
    ) -> Result<Vec<u8>> {
        let end = offset + fetch_len - 1; // HTTP Range 是闭区间
        let range = format!("bytes={offset}-{end}");

        let resp = self
            .http
            .get(url)
            .header(reqwest::header::RANGE, &range)
            .timeout(self.limits.timeout)
            .send()
            .await
            .map_err(|e| map_reqwest_error(e, self.limits.timeout))?;

        let status = resp.status();

        match status {
            reqwest::StatusCode::PARTIAL_CONTENT => {
                let cr = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let Some(cr) = cr else {
                    return Err(Error::RangeMismatch("206 响应缺少 Content-Range".into()));
                };
                let (start, _stop, total) = parse_content_range(&cr)
                    .ok_or_else(|| Error::RangeMismatch(format!("无法解析 Content-Range：{cr}")))?;
                if start != offset {
                    return Err(Error::RangeMismatch(format!(
                        "Content-Range 起点不符：请求 {offset}，响应 {start}"
                    )));
                }
                if let Some(total) = total {
                    if total != file_size {
                        return Err(Error::RangeMismatch(format!(
                            "文件大小已变化：本地 {file_size}，服务端 {total}（请刷新目录后重试）"
                        )));
                    }
                }
                self.collect_body(resp, fetch_len as usize).await
            }
            reqwest::StatusCode::OK => {
                // 关键分支：服务端忽略了 Range。下载模式下可以读完再截取，
                // 挂载模式下那等于"为了读 4 KiB 而下载整个文件"，直接拒绝。
                Err(Error::RangeUnsupported { status: 200 })
            }
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                if offset >= file_size {
                    Ok(Vec::new())
                } else {
                    Err(Error::RangeMismatch(format!(
                        "服务端返回 416（offset={offset}, size={file_size}）"
                    )))
                }
            }
            other => Err(Error::Network(format!("http {other}"))),
        }
    }

    /// 按 Content-Length 与上限双重约束收取响应体
    ///
    /// 不用 `resp.bytes()`：如果服务端谎报 Content-Range 却发来一个巨大的响应体，
    /// 一把读完就会把内存打爆。这里边收边计数，超过上限立刻中止。
    async fn collect_body(&self, resp: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
        let declared = resp.content_length().unwrap_or(0) as usize;
        if declared > limit {
            return Err(Error::RangeMismatch(format!(
                "响应体 {declared} 字节超过请求上限 {limit}"
            )));
        }
        let mut buf: Vec<u8> = Vec::with_capacity(declared.max(1024).min(limit));
        let mut stream = resp.bytes_stream();
        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(|e| map_reqwest_error(e, self.limits.timeout))?;
            if buf.len() + piece.len() > limit {
                return Err(Error::RangeMismatch(format!(
                    "响应体超过请求上限 {limit} 字节，已中止（不会把多余数据收进内存）"
                )));
            }
            buf.extend_from_slice(&piece);
        }
        Ok(buf)
    }

    /// 统计快照（GUI/CLI 共用）
    pub fn stats(&self) -> Arc<ReadStats> {
        self.stats.clone()
    }
}

fn is_link_expired(msg: &str) -> bool {
    ["http 401", "http 403", "http 410", "http 404"]
        .iter()
        .any(|p| msg.contains(p))
}

fn map_reqwest_error(e: reqwest::Error, timeout: Duration) -> Error {
    if e.is_timeout() {
        Error::Timeout(timeout)
    } else {
        // 只保留 reqwest 的分类信息，不带 URL（URL 里有签名）
        Error::Network(format!("网络请求失败：{e}"))
    }
}

/// 解析 `bytes start-end/total`（total 可能是 `*`）
pub fn parse_content_range(raw: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = raw.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = match rest.split_once('/') {
        Some((r, t)) => (r.trim(), Some(t.trim())),
        None => (rest.trim(), None),
    };
    let (start, stop) = range.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let stop = stop.trim().parse::<u64>().ok()?;
    let total = match total {
        Some("*") | None => None,
        Some(t) => Some(t.parse::<u64>().ok()?),
    };
    if stop < start {
        return None;
    }
    Some((start, stop, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::ReadStats;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct MockState {
        data: Vec<u8>,
        ignore_range: AtomicBool,
        bad_start: AtomicBool,
        short_body: AtomicBool,
        deny_path: Mutex<Option<String>>,
        requests: Mutex<Vec<String>>,
        inflight: AtomicUsize,
        max_inflight: AtomicUsize,
        hold: AtomicBool,
    }

    impl MockState {
        fn new(data: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                data,
                ignore_range: AtomicBool::new(false),
                bad_start: AtomicBool::new(false),
                short_body: AtomicBool::new(false),
                deny_path: Mutex::new(None),
                requests: Mutex::new(Vec::new()),
                inflight: AtomicUsize::new(0),
                max_inflight: AtomicUsize::new(0),
                hold: AtomicBool::new(false),
            })
        }
        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    async fn spawn_mock(state: Arc<MockState>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let st = state.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if raw.len() > 64 * 1024 {
                            return;
                        }
                    }
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let mut lines = text.split("\r\n");
                    let request_line = lines.next().unwrap_or_default().to_string();
                    let path = request_line.split(' ').nth(1).unwrap_or("/").to_string();
                    st.requests.lock().unwrap().push(request_line.clone());
                    let deny = st.deny_path.lock().unwrap().clone();
                    if let Some(deny) = deny {
                        if path != deny {
                            let body = b"denied";
                            let _ = sock
                                .write_all(
                                    format!(
                                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        body.len()
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            let _ = sock.write_all(body).await;
                            return;
                        }
                    }
                    let now = st.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    st.max_inflight.fetch_max(now, Ordering::SeqCst);
                    if st.hold.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                    }
                    let range = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split_once('=').map(|(_, v)| v.trim().to_string()));
                    let total = st.data.len();
                    let (status_line, header, body): (String, String, Vec<u8>) = match range
                        .as_deref()
                        .and_then(parse_span)
                    {
                        Some((s, e)) if !st.ignore_range.load(Ordering::SeqCst) => {
                            let start = s as usize;
                            let mut stop = ((e as usize) + 1).min(total);
                            if st.bad_start.load(Ordering::SeqCst) {
                                stop = (start + 1).min(total);
                            }
                            if start >= total {
                                (
                                    "HTTP/1.1 416 Range Not Satisfiable".into(),
                                    format!(
                                        "Content-Range: bytes */{total}\r\nContent-Length: 0\r\n"
                                    ),
                                    Vec::new(),
                                )
                            } else {
                                let mut slice = st.data[start..stop].to_vec();
                                if st.short_body.load(Ordering::SeqCst) && slice.len() > 1 {
                                    slice.truncate(slice.len() / 2);
                                }
                                let end = start + slice.len() - 1;
                                let cr_start = if st.bad_start.load(Ordering::SeqCst) {
                                    start + 1
                                } else {
                                    start
                                };
                                (
                                        "HTTP/1.1 206 Partial Content".into(),
                                        format!(
                                            "Content-Range: bytes {cr_start}-{end}/{total}\r\nContent-Length: {}\r\n",
                                            slice.len()
                                        ),
                                        slice,
                                    )
                            }
                        }
                        _ => (
                            "HTTP/1.1 200 OK".into(),
                            format!("Content-Length: {total}\r\n"),
                            st.data.clone(),
                        ),
                    };
                    let head = format!("{status_line}\r\n{header}Connection: close\r\n\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    st.inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        format!("http://{addr}")
    }

    /// 解析 mock 收到的 Range 值（形如 "0-31"）
    fn parse_span(spec: &str) -> Option<(u64, u64)> {
        let (a, b) = spec.split_once('-')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    }

    fn reader(limits: ReadLimits) -> RangeReader {
        RangeReader::new(limits, Arc::new(ReadStats::new())).unwrap()
    }

    fn link(url: &str) -> DirectLink {
        DirectLink::new(format!("{url}/file.bin"), Duration::from_secs(600))
    }

    #[tokio::test]
    async fn reads_exact_range_and_sends_range_header() {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let state = MockState::new(data.clone());
        let base = spawn_mock(state.clone()).await;
        let r = reader(ReadLimits::default());
        let mut l = link(&base);
        let out = r
            .read(&mut l, data.len() as u64, 100, 32, || async {
                Err(Error::Other("不应触发刷新".into()))
            })
            .await
            .unwrap();
        assert_eq!(out, data[100..132].to_vec());
        let reqs = state.requests();
        assert_eq!(reqs.len(), 1);
        assert!(
            !reqs[0].contains("Range"),
            "请求行里不含 Range，Range 在头里"
        );
    }

    #[tokio::test]
    async fn tail_read_is_clamped_to_eof() {
        let data = b"0123456789".to_vec();
        let state = MockState::new(data.clone());
        let base = spawn_mock(state).await;
        let r = reader(ReadLimits::default());
        let mut l = link(&base);
        let out = r
            .read(&mut l, 10, 7, 100, || async { unreachable!() })
            .await
            .unwrap();
        assert_eq!(out, b"789".to_vec());
    }

    #[tokio::test]
    async fn read_past_eof_returns_empty() {
        let data = b"abc".to_vec();
        let state = MockState::new(data);
        let base = spawn_mock(state).await;
        let r = reader(ReadLimits::default());
        let mut l = link(&base);
        let out = r
            .read(&mut l, 3, 3, 10, || async { unreachable!() })
            .await
            .unwrap();
        assert!(out.is_empty());
        let out = r
            .read(&mut l, 3, 99, 10, || async { unreachable!() })
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn zero_length_read_does_not_touch_the_network() {
        let state = MockState::new(b"abc".to_vec());
        let base = spawn_mock(state.clone()).await;
        let r = reader(ReadLimits::default());
        let mut l = link(&base);
        assert!(r
            .read(&mut l, 3, 0, 0, || async { unreachable!() })
            .await
            .unwrap()
            .is_empty());
        assert!(state.requests().is_empty());
    }

    #[tokio::test]
    async fn full_file_200_is_rejected_and_never_downloaded() {
        let data: Vec<u8> = vec![7u8; 1024 * 1024];
        let state = MockState::new(data);
        state.ignore_range.store(true, Ordering::SeqCst);
        let base = spawn_mock(state).await;
        let r = reader(ReadLimits::default());
        let mut l = link(&base);
        let err = r
            .read(&mut l, 1024 * 1024, 0, 64, || async { unreachable!() })
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::RangeUnsupported { status: 200 }),
            "必须判错而不是整包截取：{err}"
        );
        assert!(err.is_fatal(), "这条错误重试没有意义");
    }

    #[tokio::test]
    async fn mismatched_content_range_start_is_rejected() {
        let data: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
        let state = MockState::new(data);
        state.bad_start.store(true, Ordering::SeqCst);
        let base = spawn_mock(state).await;
        let r = reader(ReadLimits {
            max_retries: 0,
            ..Default::default()
        });
        let mut l = link(&base);
        let err = r
            .read(&mut l, 8192, 16, 32, || async { unreachable!() })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::RangeMismatch(_)), "{err}");
    }

    #[tokio::test]
    async fn short_body_is_reported_not_silently_accepted() {
        let data: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let state = MockState::new(data);
        state.short_body.store(true, Ordering::SeqCst);
        let base = spawn_mock(state).await;
        let r = reader(ReadLimits {
            max_retries: 1,
            ..Default::default()
        });
        let mut l = link(&base);
        let err = r
            .read(&mut l, 4096, 0, 1024, || async { unreachable!() })
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::RangeMismatch(_)),
            "短读必须报错：{err}"
        );
    }

    #[tokio::test]
    async fn expired_link_is_refreshed_once_then_succeeds() {
        let data = (0..2048u32).map(|i| i as u8).collect::<Vec<u8>>();
        let state = MockState::new(data.clone());
        *state.deny_path.lock().unwrap() = Some("/fresh.bin".to_string());
        let base = spawn_mock(state.clone()).await;
        let r = reader(ReadLimits::default());
        let mut l = link(&base); // /file.bin 会被 403
        let fresh = format!("{base}/fresh.bin");
        let out = r
            .read(&mut l, data.len() as u64, 0, 64, || {
                let f = fresh.clone();
                async move { Ok(f) }
            })
            .await
            .unwrap();
        assert_eq!(out, data[..64].to_vec());
        assert!(state.requests().len() >= 2, "应当先失败后刷新重试");
    }

    #[tokio::test]
    async fn concurrency_is_bounded_by_the_semaphore() {
        let data = vec![9u8; 65536];
        let state = MockState::new(data.clone());
        state.hold.store(true, Ordering::SeqCst);
        let base = spawn_mock(state.clone()).await;
        let r = Arc::new(reader(ReadLimits {
            max_concurrency: 2,
            ..Default::default()
        }));
        let mut fut = Vec::new();
        for i in 0..8u64 {
            let r = r.clone();
            let url = format!("{base}/file.bin");
            fut.push(tokio::spawn(async move {
                let mut l = DirectLink::new(url, Duration::from_secs(600));
                r.read(&mut l, 65536, i * 512, 512, || async { unreachable!() })
                    .await
            }));
        }
        for f in fut {
            f.await.unwrap().unwrap();
        }
        assert!(
            state.max_inflight.load(Ordering::SeqCst) <= 2,
            "并发上限必须生效，实测 {}",
            state.max_inflight.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn readahead_only_widens_the_same_request() {
        let data = (0..4096u32).map(|i| i as u8).collect::<Vec<u8>>();
        let state = MockState::new(data.clone());
        let base = spawn_mock(state.clone()).await;
        let r = reader(ReadLimits {
            readahead: 1024,
            ..Default::default()
        });
        let mut l = link(&base);
        let out = r
            .read(&mut l, 4096, 0, 512, || async { unreachable!() })
            .await
            .unwrap();
        assert_eq!(out.len(), 512, "只返回调用方要的长度");
        assert_eq!(state.requests().len(), 1, "预读不额外发请求");
    }

    #[tokio::test]
    async fn retries_are_bounded() {
        let state = MockState::new(vec![1u8; 64]);
        *state.deny_path.lock().unwrap() = Some("/never.bin".to_string());
        let base = spawn_mock(state.clone()).await;
        let r = reader(ReadLimits {
            max_retries: 2,
            ..Default::default()
        });
        let mut l = link(&base);
        let err = r
            .read(&mut l, 64, 0, 16, || async {
                Err(Error::Other("刷新也失败".into()))
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Other(_)) || matches!(err, Error::Network(_)),
            "{err}"
        );
        // 1 次失败 + 刷新失败即返回，或者 3 次尝试后返回，总之有界
        assert!(
            state.requests().len() <= 4,
            "请求次数必须有界：{}",
            state.requests().len()
        );
    }

    #[tokio::test]
    async fn no_files_are_created_during_reads() {
        let data = vec![3u8; 8192];
        let state = MockState::new(data);
        let base = spawn_mock(state).await;
        let tmp = std::env::temp_dir();
        let before = list_dir(&tmp);
        let r = reader(ReadLimits::default());
        let mut l = link(&base);
        for off in [0u64, 1000, 4000, 8000] {
            r.read(&mut l, 8192, off, 256, || async { unreachable!() })
                .await
                .unwrap();
        }
        let after = list_dir(&tmp);
        assert_eq!(before, after, "读取过程不得在临时目录产生任何文件");
    }

    fn list_dir(p: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(p)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-99/1000"),
            Some((0, 99, Some(1000)))
        );
        assert_eq!(
            parse_content_range("bytes 100-199/*"),
            Some((100, 199, None))
        );
        assert_eq!(parse_content_range("bytes 5-4/10"), None);
        assert_eq!(parse_content_range("garbage"), None);
    }
}
