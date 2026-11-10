//! 上传/下载传输实现
//!
//! 上传：initMultiUpload → getMultiUploadUrls → 分片 PUT → commitMultiUploadFile
//! 下载：getDownloadUrl → 并发 Range 分片下载 → 断点续传
//!
//! 两条链路都接受一个 [`TransferControl`]，在关键边界做协作式检查点：
//! 暂停会在检查点挂起（future 保活，恢复后从原处继续，不重新初始化会话），
//! 取消则在检查点退出。暂停永远不能变成“失败”，取消也永远不能留下半个文件。

use crate::client::TianyiClient;
use crate::crypto;
use crate::error::{Error, Result};
use crate::models::*;
use crate::ratelimit::RateLimiter;
use crate::task::{ControlState, TransferControl};
use futures::stream::{FuturesUnordered, StreamExt};
use log::{info, warn};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// 进度回调类型：接收已传输字节数
pub type ProgressCallback = Arc<dyn Fn(u64) + Send + Sync>;

/// 默认下载分片大小：4 MiB
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// 暂停挂起超过该时长后重新获取下载直链（直链带签名，可能已过期）
const URL_REFRESH_AFTER: Duration = Duration::from_secs(30);

/// 单个下载分片的最大尝试次数
const CHUNK_MAX_ATTEMPTS: u32 = 3;

/// 上传读取节流的步长：64 KiB
///
/// 取 64 KiB 的理由：它既是 tokio 定时器精度（约 1 ms）下「1 MiB/s 时每步约
/// 64 ms」这种不会空转的粒度，又足够小，让桶的租借误差不超过一步。
/// 再小（4 KiB）会让低速率下的租借次数变成原来的 16 倍，唤醒开销盖过带宽本身。
const UPLOAD_THROTTLE_STEP: usize = 64 * 1024;

/// 上传选项
#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub overwrite: bool,
    pub rapid_upload: bool,
    pub thread_count: u32,
    pub part_size: i64,
    pub generate_torrent: bool,
    /// 上传限速桶（`None` = 不限速，零额外开销）
    ///
    /// 放在选项里而不是做成全局单例：限速是「这一批传输」的属性，
    /// 由调用方按当前配置造一个桶交给整批任务，N 个并发分片才会共享同一份额度
    /// （挂在分片任务上就变成每片一份，聚合速率会翻 N 倍）。
    pub rate: Option<Arc<RateLimiter>>,
}

impl Default for UploadOptions {
    fn default() -> Self {
        UploadOptions {
            overwrite: true,
            rapid_upload: true,
            thread_count: 3,
            // 0 = 按文件大小自动计算分片大小（crypto::part_size），
            // 与 OpenList 189pc 一致，避免大文件分片数超过服务端限制
            part_size: 0,
            generate_torrent: false,
            rate: None,
        }
    }
}

/// 下载选项
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub thread_count: u32,
    pub resume: bool,
    /// 分片大小（字节）。每个分片整体读入内存后再按序写入 `.part`，
    /// 决定暂停的响应粒度（默认 [`DEFAULT_CHUNK_SIZE`]，即 4 MiB）
    pub chunk_size: u64,
    /// 下载限速桶（`None` = 不限速，零额外开销）
    ///
    /// 与上传同理放在选项里：`download_round` 的 N 路并发分片必须共享同一个桶，
    /// 速率才是「整条下载」的上限，而不是「每一路」的上限。
    pub rate: Option<Arc<RateLimiter>>,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        DownloadOptions {
            thread_count: 4,
            resume: true,
            chunk_size: DEFAULT_CHUNK_SIZE,
            rate: None,
        }
    }
}

/// 下载结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// 全部字节已写入并已重命名为最终文件
    Completed,
    /// 用户暂停：`.part` 保留为合法前缀，重新调用即可从该点续传
    Paused,
    /// 用户取消：`.part` 已删除
    Canceled,
}

/// 上传结果
#[derive(Debug)]
pub enum UploadOutcome {
    /// 已完成（服务端已提交）
    Completed(CommitFile),
    /// 用户暂停（未提交任何内容，`upload_file_id` 会话保留在活着的 future 里）
    Paused,
    /// 用户取消（未提交，已上传的分片由服务端自行回收）
    Canceled,
}

/// 把中断错误翻译成上传结果；非中断错误原样返回
fn into_upload_result(e: Error) -> Result<UploadOutcome> {
    match e {
        Error::Paused => Ok(UploadOutcome::Paused),
        Error::Canceled => Ok(UploadOutcome::Canceled),
        other => Err(other),
    }
}

/// 规划下载分片：返回覆盖 `[start, file_size)` 的 `(offset, len)` 列表
///
/// 保证：严格升序、不重叠、无空洞、恰好覆盖一次。
/// 边界：`start >= file_size` 返回空；`chunk_size == 0` 按 4 MiB 处理；
/// `file_size == 0` 返回空。
pub fn plan_chunks(file_size: u64, start: u64, chunk_size: u64) -> Vec<(u64, u64)> {
    let chunk_size = if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        chunk_size
    };
    let mut chunks = Vec::new();
    let mut offset = start;
    while offset < file_size {
        let len = chunk_size.min(file_size - offset);
        chunks.push((offset, len));
        match offset.checked_add(len) {
            Some(next) => offset = next,
            // 只有 offset + len 溢出 u64 才可能走到这里（正常不会发生）
            None => break,
        }
    }
    chunks
}

/// 上传入口
///
/// 暂停语义：入口处若已经是暂停态，立即返回 [`UploadOutcome::Paused`]
/// （不打开文件、不联网、不算 MD5）；传输过程中暂停则在检查点挂起，
/// 整个 future 保活——`upload_file_id` 会话与已算好的 MD5 都还在，
/// 恢复后直接从下一个分片继续，不会重新初始化也不会重新计算哈希。
pub async fn upload(
    client: &TianyiClient,
    parent_folder_id: &str,
    local_path: &Path,
    opts: &UploadOptions,
    progress: ProgressCallback,
    control: &TransferControl,
) -> Result<UploadOutcome> {
    // 入口非阻塞观察：暂停态直接让出，不产生任何副作用
    match control.state() {
        ControlState::Canceled => return Ok(UploadOutcome::Canceled),
        ControlState::Paused => return Ok(UploadOutcome::Paused),
        ControlState::Running => {}
    }

    let local_path = local_path.to_path_buf();
    let file = tokio::fs::File::open(&local_path).await?;
    let meta = file.metadata().await?;
    let file_size = meta.len();
    let file_name = local_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();

    // 计算分片大小
    let part_size = if opts.part_size > 0 {
        opts.part_size
    } else {
        crypto::part_size(file_size as i64)
    };

    // 计算整文件 MD5 + 分片 MD5 + SHA-1 piece hashes（一次性读取计算）
    // 计算期间在每个分片边界响应暂停：哈希上下文保留，恢复后接着算
    let (file_md5, slice_md5s, piece_sha1s) =
        match compute_md5s(&local_path, part_size, control).await {
            Ok(v) => v,
            Err(e) => return into_upload_result(e),
        };
    info!(
        "upload start: name={} size={} parts={} part_size={}",
        file_name,
        file_size,
        slice_md5s.len(),
        part_size
    );

    // 秒传尝试
    if opts.rapid_upload {
        if let Err(e) = control.checkpoint().await {
            return into_upload_result(e);
        }
        match try_rapid_upload(
            client,
            parent_folder_id,
            &file_name,
            file_size,
            &file_md5,
            &slice_md5s,
            part_size,
            opts.overwrite,
            control,
        )
        .await
        {
            Ok(Some(f)) => {
                // 秒传已提交：返回前仍然响应暂停/取消（暂停在此挂起）
                if let Err(e) = control.checkpoint().await {
                    return into_upload_result(e);
                }
                info!("rapid upload success: {}", f.file_name);
                progress(file_size);
                return Ok(UploadOutcome::Completed(f));
            }
            Ok(None) => {}
            Err(e) if e.is_interrupted() => return into_upload_result(e),
            Err(e) => {
                warn!("rapid upload failed: {e}, fallback to multipart");
            }
        }
    }

    // 初始化多分片上传
    if let Err(e) = control.checkpoint().await {
        return into_upload_result(e);
    }
    let init = init_multipart_upload(
        client,
        parent_folder_id,
        &file_name,
        file_size,
        &file_md5,
        &slice_md5s,
        part_size,
    )
    .await?;
    info!(
        "initMultiUpload ok: upload_file_id={} file_data_exists={}",
        init.upload_file_id, init.file_data_exists
    );

    if init.file_data_exists == 1 {
        // 服务端已有，直接提交（提交前仍要响应取消，避免“取消后仍创建文件”）
        if let Err(e) = control.checkpoint().await {
            return into_upload_result(e);
        }
        let commit = commit_multipart_upload(client, &init.upload_file_id, opts.overwrite).await?;
        progress(file_size);
        return Ok(UploadOutcome::Completed(commit));
    }

    // 计算分片数
    let part_size_u = part_size as u64;
    let count = file_size.div_ceil(part_size_u).max(1) as usize;
    let last_size = if file_size % part_size_u == 0 {
        part_size_u
    } else {
        file_size % part_size_u
    };

    // 需要计算每个分片的 partInfo（MD5 base64）
    // 我们已在 compute_md5s 中得到各分片 MD5（大写 hex），转 base64
    let part_infos: Vec<String> = slice_md5s
        .iter()
        .enumerate()
        .map(|(i, md5)| {
            let hex_bytes = hex::decode(md5).unwrap_or_default();
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, hex_bytes);
            format!("{}-{}", i + 1, b64)
        })
        .collect();

    // 断点续传恢复：查询已有进度（如果服务端支持）
    // 这里简化处理：每次都重新上传所有分片；服务端对已上传分片会跳过或覆盖
    let uploaded = Arc::new(AtomicU64::new(0));

    // 并发上传分片
    let upload_file_id = init.upload_file_id.clone();
    let mut handles = Vec::new();
    let thread_count = opts.thread_count.max(1) as usize;
    let sem = Arc::new(tokio::sync::Semaphore::new(thread_count));

    for part_info in part_infos.iter() {
        let part_info = part_info.clone();
        let upload_file_id = upload_file_id.clone();
        let client = client.clone_for_transfer();
        let sem = sem.clone();
        let uploaded = uploaded.clone();
        let progress = progress.clone();
        let local_path = local_path.clone();
        // 限速桶按 Arc 克隆：N 个并发分片拿到的是**同一个**桶，
        // 因此聚合速率是整批上传的上限，而不是每片各一份额度
        let rate = opts.rate.clone();
        // 每个分片任务各持一份控制器副本（watch 句柄很轻）
        let control = control.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|_| Error::Transfer("sem".into()))?;
            let part_no = part_info.split('-').next().unwrap_or("?").to_string();
            // 请求分片上传 URL 前的检查点：暂停时不发起新请求
            control.checkpoint().await?;
            let urls = get_multi_upload_urls(&client, &upload_file_id, &[part_info]).await?;
            let url_info = urls
                .first()
                .ok_or_else(|| Error::Transfer(format!("part {part_no}: empty upload url list")))?;
            let part_number = url_info.0;
            let req_url = url_info.1.request_url.clone();
            let headers = parse_http_headers(&url_info.1.request_header);

            // 读取分片数据前再检查一次：暂停时不留大缓冲
            control.checkpoint().await?;
            let offset = (part_number as u64 - 1) * part_size_u;
            let len = if part_number as usize == count {
                last_size
            } else {
                part_size_u
            };

            // 读分片数据（限速在这里记账：见 read_part_throttled）
            let buf = read_part_throttled(&local_path, offset, len, rate.as_deref()).await?;

            // PUT 上传（带重试）
            let mut attempt = 0u32;
            loop {
                match put_part(&client, &req_url, &headers, &buf).await {
                    Ok(()) => break,
                    Err(e) => {
                        attempt += 1;
                        if attempt >= 3 {
                            // 日志里不带签名 URL
                            warn!("part {part_no} put failed after {attempt} tries: {e}");
                            return Err(e);
                        }
                        warn!("part {part_no} put failed (try {attempt}): {e}, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        // 重试前响应取消，避免取消后还继续重试
                        control.checkpoint().await?;
                    }
                }
            }
            info!("part {part_no} uploaded, {} bytes", len);
            let done = uploaded.fetch_add(len, Ordering::SeqCst) + len;
            progress(done);
            Ok::<(), Error>(())
        }));
    }

    let mut futs = FuturesUnordered::new();
    for h in handles {
        futs.push(h);
    }
    while let Some(res) = futs.next().await {
        let joined = res.map_err(|e| Error::Transfer(format!("task join: {e}")))?;
        if let Err(e) = joined {
            // 中断（暂停/取消）不上报为失败；分片任务会各自在检查点退出
            return into_upload_result(e);
        }
    }

    // 只有全部分片都成功后才提交：暂停/取消永远不会走到这里，
    // 因此不可能产生“提交了一半”的文件，也不可能重复提交。
    if let Err(e) = control.checkpoint().await {
        return into_upload_result(e);
    }
    info!("all {} parts uploaded, committing", count);
    let commit = commit_multipart_upload(client, &init.upload_file_id, opts.overwrite).await?;
    info!(
        "commit ok: file_id={} name={}",
        commit.user_file_id, commit.file_name
    );

    // 生成 CAS torrent（可选，异步不影响结果）
    if opts.generate_torrent && !piece_sha1s.is_empty() && file_size > 0 {
        let torrent_name = format!("{}.cas.torrent", file_name);
        match crate::torrent::generate_torrent(
            &file_name,
            file_size as i64,
            &file_md5,
            &slice_md5s,
            part_size_u as i64,
            &piece_sha1s,
        ) {
            Ok(torrent_data) => {
                let torrent_path = local_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join(&torrent_name);
                if let Err(e) = tokio::fs::write(&torrent_path, &torrent_data).await {
                    warn!("写入 CAS torrent 失败: {e}");
                } else {
                    info!("已生成 CAS torrent: {}", torrent_path.display());
                }
            }
            Err(e) => warn!("生成 CAS torrent 失败: {e}"),
        }
    }

    progress(file_size);
    Ok(UploadOutcome::Completed(commit))
}

/// 读取一个上传分片的数据，按 [`UPLOAD_THROTTLE_STEP`] 步长逐块租借令牌
///
/// 为什么在**读取侧**记账：分片必须先整片读进内存，才能算出与服务端分片一致的
/// `Content-Length` 再一次性 PUT（流式 body 会变成 chunked 传输，天翼云的分片上传
/// 接口不接受）。所以限速只能落在「把这一片读进来」这一段：每读一块就按**实际读到的
/// 字节数**向共享桶租借，N 个并发分片因此共用同一份额度，聚合速率被压在设定值附近。
///
/// 代价：租借发生在发送之前，线上速率会略低于设定值（读取与 PUT 串行叠加），
/// 偏差方向保守——只会更慢，不会超额。粒度是「每 64 KiB 一次租借」，
/// 而不是「每 64 KiB 一次网络写」。
///
/// `rate == None` 走上面那条一次读完的直通分支：不加锁、不 sleep、
/// 连分块循环都不进，与引入限速前的代码逐行等价。
async fn read_part_throttled(
    path: &Path,
    offset: u64,
    len: u64,
    rate: Option<&RateLimiter>,
) -> Result<Vec<u8>> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len as usize];
    let Some(rate) = rate else {
        // 不限速：与引入限速前完全一致（含 EOF 的错误种类）
        f.read_exact(&mut buf).await?;
        return Ok(buf);
    };

    let mut filled = 0usize;
    while filled < buf.len() {
        let step = UPLOAD_THROTTLE_STEP.min(buf.len() - filled);
        let n = f.read(&mut buf[filled..filled + step]).await?;
        if n == 0 {
            // 文件在传输过程中被截断：按分片数据不足报错（原来是 read_exact 的
            // UnexpectedEof，语义等价，都不属于「可中断」错误）
            return Err(Error::Transfer(format!(
                "分片数据不足：期望 {len} 字节，实读 {filled}"
            )));
        }
        // 按**实际读到的字节数**记账：短读不会多租借
        rate.acquire(n as u64).await;
        filled += n;
    }
    Ok(buf)
}

/// 计算整文件 MD5、各分片 MD5、各分片 SHA-1 piece hash
///
/// 每读完一个完整分片做一次检查点：暂停在这里挂起，哈希上下文（`Md5`/`Sha1`
/// 的中间状态）不会被丢弃，恢复后从下一个分片接着算。
async fn compute_md5s(
    path: &Path,
    part_size: i64,
    control: &TransferControl,
) -> Result<(String, Vec<String>, Vec<u8>)> {
    use md5::Digest;
    use md5::Md5;
    use sha1::Sha1;

    let mut file = tokio::fs::File::open(path).await?;
    let mut whole = Md5::new();
    let mut parts = Vec::new();
    let mut part_sha1s = Vec::new();
    let mut part_buf = Vec::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let part_size = part_size as u64;

    // 与 OpenList 189pc FastUpload 保持一致：分片 MD5 是分片内容哈希的 hex，
    // 而非分片原始字节的 hex
    // 与 OpenList 189pc FastUpload 保持一致：
    // 分片 MD5 = MD5(分片内容) 的大写 hex；partInfo 的 base64 由这些 hex 解码而来
    let flush_part =
        |part_buf: &[u8], whole: &mut Md5, parts: &mut Vec<String>, part_sha1s: &mut Vec<u8>| {
            whole.update(part_buf);
            let h = Md5::digest(part_buf);
            parts.push(hex::encode_upper(h.as_slice()));
            let h = Sha1::digest(part_buf);
            part_sha1s.extend_from_slice(&h);
        };

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        part_buf.extend_from_slice(&buf[..n]);
        if part_buf.len() as u64 >= part_size {
            flush_part(&part_buf, &mut whole, &mut parts, &mut part_sha1s);
            part_buf.clear();
            // 分片边界：响应暂停/取消
            control.checkpoint().await?;
        }
    }
    if !part_buf.is_empty() {
        flush_part(&part_buf, &mut whole, &mut parts, &mut part_sha1s);
    }
    if parts.is_empty() {
        // 空文件：一个分片，MD5 为空内容 MD5；无需 SHA-1 piece hash
        let h = Md5::digest([]);
        parts.push(hex::encode_upper(h.as_slice()));
    }

    let whole_hex = hex::encode_upper(whole.finalize().as_slice());
    Ok((whole_hex, parts, part_sha1s))
}

/// 秒传
#[allow(clippy::too_many_arguments)]
async fn try_rapid_upload(
    client: &TianyiClient,
    parent_id: &str,
    file_name: &str,
    file_size: u64,
    file_md5: &str,
    slice_md5s: &[String],
    slice_size: i64,
    overwrite: bool,
    control: &TransferControl,
) -> Result<Option<CommitFile>> {
    let init = init_multipart_upload(
        client, parent_id, file_name, file_size, file_md5, slice_md5s, slice_size,
    )
    .await?;

    if init.file_data_exists == 1 {
        // 提交前响应取消：取消后不应再创建文件
        control.checkpoint().await?;
        let commit = commit_multipart_upload(client, &init.upload_file_id, overwrite).await?;
        return Ok(Some(commit));
    }
    Ok(None)
}

/// 初始化多分片上传
pub async fn init_multipart_upload(
    client: &TianyiClient,
    parent_folder_id: &str,
    file_name: &str,
    file_size: u64,
    file_md5: &str,
    slice_md5s: &[String],
    slice_size: i64,
) -> Result<InitMultiUploadResp> {
    let is_family = client.is_family();
    let mut params = std::collections::HashMap::new();
    params.insert("parentFolderId".to_string(), parent_folder_id.to_string());
    params.insert("fileName".to_string(), urlencoding(file_name));
    params.insert("fileSize".to_string(), file_size.to_string());
    params.insert("sliceSize".to_string(), slice_size.to_string());
    let slice_md5 = if slice_md5s.len() > 1 {
        crypto::md5_of_joined(&slice_md5s.iter().map(|s| s.as_str()).collect::<Vec<_>>())
    } else {
        file_md5.to_string()
    };
    params.insert("fileMd5".to_string(), file_md5.to_string());
    params.insert("sliceMd5".to_string(), slice_md5);
    if is_family {
        params.insert("familyId".to_string(), client.family_id());
    }

    let url = if is_family {
        format!("{}/family/initMultiUpload", client.upload_base_url())
    } else {
        format!("{}/person/initMultiUpload", client.upload_base_url())
    };

    let text = client.get_json_encrypted(&url, params, is_family).await?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse init: {e}")))?;
    let data = &v["data"];
    let upload_file_id = data["uploadFileId"].as_str().unwrap_or("").to_string();
    if upload_file_id.is_empty() {
        return Err(Error::Api(format!(
            "initMultiUpload: no uploadFileId in resp: {}",
            text.chars().take(300).collect::<String>()
        )));
    }
    Ok(InitMultiUploadResp {
        upload_type: data["uploadType"].as_i64().unwrap_or(0) as i32,
        upload_host: data["uploadHost"].as_str().unwrap_or("").to_string(),
        upload_file_id,
        file_data_exists: data["fileDataExists"].as_i64().unwrap_or(0) as i32,
    })
}

/// 获取分片上传 URL
pub async fn get_multi_upload_urls(
    client: &TianyiClient,
    upload_file_id: &str,
    part_infos: &[String],
) -> Result<Vec<(i32, UploadUrlsData)>> {
    let is_family = client.is_family();
    let mut params = std::collections::HashMap::new();
    params.insert("uploadFileId".to_string(), upload_file_id.to_string());
    params.insert("partInfo".to_string(), part_infos.join(","));

    let url = if is_family {
        format!("{}/family/getMultiUploadUrls", client.upload_base_url())
    } else {
        format!("{}/person/getMultiUploadUrls", client.upload_base_url())
    };

    let text = client.get_json_encrypted(&url, params, is_family).await?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse urls: {e}")))?;

    let upload_urls = v["uploadUrls"].as_object().ok_or_else(|| {
        Error::Api(format!(
            "getMultiUploadUrls: no uploadUrls in resp: {}",
            text.chars().take(300).collect::<String>()
        ))
    })?;

    let mut list = Vec::new();
    for (k, item) in upload_urls {
        if let Some(part_num) = k.strip_prefix("partNumber_") {
            if let Ok(num) = part_num.parse::<i32>() {
                list.push((
                    num,
                    UploadUrlsData {
                        request_url: item["requestURL"].as_str().unwrap_or("").to_string(),
                        request_header: item["requestHeader"].as_str().unwrap_or("").to_string(),
                    },
                ));
            }
        }
    }
    list.sort_by_key(|(n, _)| *n);
    if list.is_empty() {
        return Err(Error::Api(format!(
            "getMultiUploadUrls: no usable partNumber entries, resp: {}",
            text.chars().take(300).collect::<String>()
        )));
    }
    Ok(list)
}

/// 解析 requestHeader（& 分隔的 k=v）
pub fn parse_http_headers(header_str: &str) -> Vec<(String, String)> {
    header_str
        .split('&')
        .filter_map(|part| {
            let mut it = part.splitn(2, '=');
            let k = it.next()?.trim().to_string();
            let v = it.next().unwrap_or("").trim().to_string();
            if k.is_empty() {
                None
            } else {
                Some((k, v))
            }
        })
        .collect()
}

/// PUT 分片数据
pub async fn put_part(
    client: &TianyiClient,
    url: &str,
    headers: &[(String, String)],
    data: &[u8],
) -> Result<()> {
    let mut req = client
        .upload_client()
        .put(url)
        .body(data.to_vec())
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        // 与 OpenList 189pc put 一致：追加 clientSuffix 查询参数
        .query(&crate::crypto::client_suffix());
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.bytes().await?;
    if !status.is_success() {
        return Err(Error::Api(format!(
            "put part http {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }
    Ok(())
}

/// 提交上传
pub async fn commit_multipart_upload(
    client: &TianyiClient,
    upload_file_id: &str,
    overwrite: bool,
) -> Result<CommitFile> {
    let is_family = client.is_family();
    let mut params = std::collections::HashMap::new();
    params.insert("uploadFileId".to_string(), upload_file_id.to_string());
    params.insert("isLog".to_string(), "0".to_string());
    params.insert(
        "opertype".to_string(),
        if overwrite {
            "3".to_string()
        } else {
            "1".to_string()
        },
    );

    let url = if is_family {
        format!("{}/family/commitMultiUploadFile", client.upload_base_url())
    } else {
        format!("{}/person/commitMultiUploadFile", client.upload_base_url())
    };

    let text = client.get_json_encrypted(&url, params, is_family).await?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse commit: {e}")))?;
    let f = &v["file"];
    if f["userFileId"].as_str().unwrap_or("").is_empty() {
        return Err(Error::Api(format!(
            "commitMultiUploadFile: no file in resp: {}",
            text.chars().take(300).collect::<String>()
        )));
    }
    Ok(CommitFile {
        user_file_id: f["userFileId"].as_str().unwrap_or("").to_string(),
        file_name: f["fileName"].as_str().unwrap_or("").to_string(),
        file_size: f["fileSize"].as_i64().unwrap_or(0),
        file_md5: f["fileMd5"].as_str().unwrap_or("").to_string(),
        create_date: f["createDate"].as_str().unwrap_or("").to_string(),
    })
}

fn urlencoding(s: &str) -> String {
    // 天翼云盘需要 url.QueryEscape，Rust 的 percent-encoding 结果不同。
    // 使用自定义实现近似 Go 的 QueryEscape
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 文件夹递归上传
// ---------------------------------------------------------------------------

/// 递归上传一个本地文件夹到云端目录。
///
/// 在 `parent_folder_id` 下创建与 `local_dir` 同名的子文件夹，然后递归上传其中的
/// 所有文件（子目录逐层建文件夹）。进度回调累计整个文件夹的字节数。
/// 返回上传的文件总数。
///
/// 暂停/取消会以 `Err(Error::Paused)` / `Err(Error::Canceled)` 向上传播
/// （`Error::is_interrupted()` 可判别）。因为每个文件都由服务端在 commit 时
/// 原子创建，中断不会留下半个已提交的文件。
pub async fn upload_folder_recursive(
    client: &TianyiClient,
    parent_folder_id: &str,
    local_dir: &Path,
    opts: &UploadOptions,
    progress: ProgressCallback,
    control: &TransferControl,
) -> Result<usize> {
    control.checkpoint().await?;
    let dir_name = local_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("folder")
        .to_string();
    let folder = client.create_folder(parent_folder_id, &dir_name).await?;
    upload_folder_contents_recursive(client, &folder.id, local_dir, opts, progress, control).await
}

/// 上传文件夹下的全部内容（不新建顶层文件夹），返回文件总数。
///
/// 中断语义同 [`upload_folder_recursive`]。
pub async fn upload_folder_contents_recursive(
    client: &TianyiClient,
    folder_id: &str,
    local_dir: &Path,
    opts: &UploadOptions,
    progress: ProgressCallback,
    control: &TransferControl,
) -> Result<usize> {
    let mut count = 0usize;
    let mut entries = tokio::fs::read_dir(local_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        // 每个条目开始前响应暂停/取消
        control.checkpoint().await?;
        let path = entry.path();
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            let name = entry.file_name().to_str().unwrap_or("folder").to_string();
            let child = client.create_folder(folder_id, &name).await?;
            let sub = upload_folder_contents_recursive(
                client,
                &child.id,
                &path,
                opts,
                progress.clone(),
                control,
            );
            count += Box::pin(sub).await?;
        } else {
            match upload(client, folder_id, &path, opts, progress.clone(), control).await? {
                UploadOutcome::Completed(_) => count += 1,
                UploadOutcome::Paused => return Err(Error::Paused),
                UploadOutcome::Canceled => return Err(Error::Canceled),
            }
        }
    }
    Ok(count)
}

/// 递归列出本地文件夹下的所有文件路径（用于拖入上传批量注册任务）
pub fn collect_local_files(root: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_local_files(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 下载
// ---------------------------------------------------------------------------

/// `.part` 临时文件路径：`<目标文件名>.download.part`
///
/// 与旧版本保持一致，保证升级后仍能接着续传已有临时文件。
fn part_path_of(dest_path: &Path) -> PathBuf {
    dest_path.with_extension("download.part")
}

/// 单轮分片下载的结束原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundEnd {
    /// 本轮所有分片都写完了
    Drained,
    /// 暂停挂起时间较长，直链可能已过期：需要重取直链后从已写位置继续
    UrlStale,
    /// 用户暂停
    Paused,
    /// 用户取消
    Canceled,
}

/// 下载文件（支持暂停 / 恢复 / 取消 / 断点续传）
///
/// 暂停语义：入口处若已经是暂停态，立即返回 [`DownloadOutcome::Paused`]
/// （不创建任何文件、不联网）；传输过程中暂停则在分片边界挂起，
/// future 与文件句柄都保活，恢复后从同一个偏移继续写。
///
/// `.part` 文件名由 [`part_path_of`] 决定，任何时刻它的内容都是目标文件的
/// 合法前缀（见 [`download_round`] 的前缀不变量），因此它的长度可以直接
/// 作为续传点使用。
pub async fn download(
    client: &TianyiClient,
    file: &FileObject,
    dest_path: &Path,
    opts: &DownloadOptions,
    progress: ProgressCallback,
    control: &TransferControl,
) -> Result<DownloadOutcome> {
    download_impl(
        client,
        &file.id,
        file.size.max(0) as u64,
        dest_path,
        opts,
        progress,
        control,
        None,
    )
    .await
}

/// 下载实现主体
///
/// `forced_url` 用于测试注入直链（生产路径恒为 `None`，此时走
/// `getDownloadUrl` 协商）。
#[allow(clippy::too_many_arguments)]
async fn download_impl(
    client: &TianyiClient,
    file_id: &str,
    file_size: u64,
    dest_path: &Path,
    opts: &DownloadOptions,
    progress: ProgressCallback,
    control: &TransferControl,
    forced_url: Option<&str>,
) -> Result<DownloadOutcome> {
    // 入口非阻塞观察：暂停态直接让出，不产生任何副作用
    match control.state() {
        ControlState::Canceled => return Ok(DownloadOutcome::Canceled),
        ControlState::Paused => return Ok(DownloadOutcome::Paused),
        ControlState::Running => {}
    }

    let part_path = part_path_of(dest_path);
    if let Some(parent) = dest_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    // 续传点 = `.part` 当前长度。前缀不变量保证这个长度就是已落盘的前缀长度。
    // `.part` 比目标文件还大说明它是脏数据（换过文件/曾被外部改写），只能从头开始。
    let part_len = match tokio::fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    let start = if opts.resume && part_len <= file_size {
        part_len
    } else {
        0
    };

    let mut part_file = if start > 0 {
        tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&part_path)
            .await?
    } else {
        tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&part_path)
            .await?
    };

    if start == file_size {
        // 已经完整（同样覆盖 0 字节文件）
        part_file.flush().await?;
        drop(part_file);
        tokio::fs::rename(&part_path, dest_path).await?;
        progress(file_size);
        return Ok(DownloadOutcome::Completed);
    }

    let chunk_size = if opts.chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        opts.chunk_size
    };
    let concurrency = opts.thread_count.max(1) as usize;
    let mut written = start;

    let (outcome, written) = loop {
        // 每轮都取一次直链：长时间暂停后旧直链可能已过期
        let dl_url = match forced_url {
            Some(url) => url.to_string(),
            None => client.get_download_url(file_id).await?,
        };
        let (end, done) = download_round(
            client,
            &dl_url,
            &mut part_file,
            file_size,
            written,
            chunk_size,
            concurrency,
            // 整个 `download()` 只有这一个桶：一轮里的 N 路并发分片共享它
            opts.rate.as_deref(),
            &progress,
            control,
        )
        .await?;
        match end {
            RoundEnd::Drained => break (DownloadOutcome::Completed, done),
            RoundEnd::Paused => break (DownloadOutcome::Paused, done),
            RoundEnd::Canceled => break (DownloadOutcome::Canceled, done),
            RoundEnd::UrlStale => written = done,
        }
    };

    part_file.flush().await?;
    drop(part_file);
    match outcome {
        DownloadOutcome::Completed => {
            tokio::fs::rename(&part_path, dest_path).await?;
            progress(file_size);
            info!("download completed: {} bytes", file_size);
        }
        DownloadOutcome::Paused => {
            // 暂停：保留 `.part`（它就是合法前缀），下次调用从这里续传
            info!("download paused: {written} / {file_size} bytes kept in .part");
        }
        DownloadOutcome::Canceled => {
            // 取消：临时文件没有保留价值，删掉避免留下垃圾
            if let Err(e) = tokio::fs::remove_file(&part_path).await {
                warn!("remove .part after cancel failed: {e}");
            }
            info!("download canceled: {written} / {file_size} bytes discarded");
        }
    }
    Ok(outcome)
}

/// 一轮分片下载：从 `start` 起把 `[start, file_size)` 追加进 `.part`
///
/// **前缀不变量（下载不损坏文件的根本保证）**：分片先整体读进内存，然后由本
/// 函数按 [`plan_chunks`] 给出的**升序**逐块 `write_all` + `flush`，绝不并行写
/// 同一个文件、绝不乱序、绝不跳块。因此 `.part` 在任何时刻、任何一次崩溃之后
/// 都是目标文件的一个合法前缀，`.part` 的长度即安全的续传点。
///
/// 并发只用来“取数据”，不改变“落盘顺序”：`futures::stream::buffered(n)`
/// 保证输出顺序与输入顺序一致（注意不能用 `buffer_unordered`）。
///
/// 暂停/取消都在分片边界生效：已发出的分片最多再完成一个 `chunk_size`
/// 的数据量，但只有被按序取走并写盘的分片才会出现在 `.part` 里。
///
/// 限速：`rate` 只传给取数据的那些 future，N 路并发共享同一个桶
/// （记账点见 [`fetch_chunk_once`]）。它不改变落盘顺序、不移动检查点位置，
/// 因此暂停/取消/续传语义与不限速时完全一致，只是每片回来得慢一些。
#[allow(clippy::too_many_arguments)]
async fn download_round(
    client: &TianyiClient,
    dl_url: &str,
    part_file: &mut tokio::fs::File,
    file_size: u64,
    start: u64,
    chunk_size: u64,
    concurrency: usize,
    rate: Option<&RateLimiter>,
    progress: &ProgressCallback,
    control: &TransferControl,
) -> Result<(RoundEnd, u64)> {
    part_file.seek(SeekFrom::Start(start)).await?;
    let mut written = start;
    let chunks = plan_chunks(file_size, start, chunk_size);
    let mut stream = Box::pin(
        futures::stream::iter(chunks)
            .map(|(offset, len)| {
                let client = client.clone_for_transfer();
                let url = dl_url.to_string();
                async move { fetch_chunk(&client, &url, offset, len, rate).await }
            })
            .buffered(concurrency),
    );

    while let Some(chunk) = stream.next().await {
        let buf = chunk?;
        part_file.write_all(&buf).await?;
        written += buf.len() as u64;
        // 每片都 flush：把已确认的字节真正交给内核，`.part` 才随时都是合法前缀
        part_file.flush().await?;
        // 进度上报“已写入的绝对字节数”，调用方可以直接对时间差求速度
        progress(written);

        let parked_at = Instant::now();
        match control.checkpoint().await {
            Ok(()) => {
                if parked_at.elapsed() >= URL_REFRESH_AFTER && written < file_size {
                    return Ok((RoundEnd::UrlStale, written));
                }
            }
            Err(Error::Canceled) => return Ok((RoundEnd::Canceled, written)),
            Err(Error::Paused) => return Ok((RoundEnd::Paused, written)),
            Err(e) => return Err(e),
        }
    }
    Ok((RoundEnd::Drained, written))
}

/// 取一个分片（失败自动重试，最多 [`CHUNK_MAX_ATTEMPTS`] 次）
///
/// 暂停期间在途请求可能因为连接超时而失败，重试可以把它重新拉起来；
/// 日志只记录偏移/长度，绝不打印带签名的直链。
/// 重试期间已经租借掉的令牌不会退回（见 [`RateLimiter::acquire`]），
/// 所以限速下失败重试会让实际速率略微低于设定值——方向保守，可以接受。
async fn fetch_chunk(
    client: &TianyiClient,
    dl_url: &str,
    offset: u64,
    len: u64,
    rate: Option<&RateLimiter>,
) -> Result<Vec<u8>> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match fetch_chunk_once(client, dl_url, offset, len, rate).await {
            Ok(buf) => return Ok(buf),
            Err(e) if attempt >= CHUNK_MAX_ATTEMPTS => {
                warn!("download chunk offset={offset} len={len} failed after {attempt} tries: {e}");
                return Err(e);
            }
            Err(e) => {
                warn!("download chunk offset={offset} len={len} failed (try {attempt}): {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// 发起一次 Range 请求并取出该分片的字节
///
/// 服务端忽略 `Range` 直接返回整个文件时（200），按窗口截取，绝不把
/// 多余字节写进 `.part`。
///
/// 限速粒度：用 `bytes_stream()` **边收边租借**，每收到一块（reqwest/hyper 给的
/// 大小，典型 8～64 KiB）就按实际字节数 `acquire` 一次，因此限的是真实吞吐，
/// 与分片大小无关；代价是每个分片多了一个 await 点，且 `rate == None` 时
/// 这条路径与原来完全一致——`if let Some(rate)` 直接跳过，不加锁、不 sleep、
/// 不做无意义的判断。
async fn fetch_chunk_once(
    client: &TianyiClient,
    dl_url: &str,
    offset: u64,
    len: u64,
    rate: Option<&RateLimiter>,
) -> Result<Vec<u8>> {
    let end = offset.saturating_add(len); // 不含
    let range = format!("bytes={}-{}", offset, end.saturating_sub(1));
    let resp = client
        .http_client()
        .get(dl_url)
        .header(reqwest::header::RANGE, &range)
        .send()
        .await?;
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT && status != reqwest::StatusCode::OK {
        // 错误响应体很小，不节流：这里必须先读完 body 才能把报错内容带出去
        let body = resp.bytes().await.unwrap_or_default();
        return Err(Error::Api(format!(
            "download chunk http {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }

    // 逐块收取并按块租借令牌。缓冲区仍然整片攒齐后再返回，所以
    // 「服务器忽略 Range 时按窗口截取」的容错逻辑一行都不用改。
    //
    // 不限速时**不走**流式分支：直接 `bytes()` 一把读完，与引入限速前的代码
    // 完全一致——不包 stream、每块也不多一个「要不要租借」的 await 点。
    let body: Vec<u8> = match rate {
        None => resp.bytes().await?.to_vec(),
        Some(rate) => {
            let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
            let mut stream = resp.bytes_stream();
            while let Some(piece) = stream.next().await {
                let piece = piece?;
                rate.acquire(piece.len() as u64).await;
                buf.extend_from_slice(&piece);
            }
            buf
        }
    };

    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        if body.len() as u64 != len {
            return Err(Error::Transfer(format!(
                "分片长度不符：期望 {len}，实收 {}",
                body.len()
            )));
        }
        return Ok(body);
    }

    let begin =
        usize::try_from(offset).map_err(|_| Error::Transfer(format!("分片偏移越界: {offset}")))?;
    let stop = usize::try_from(end).map_err(|_| Error::Transfer(format!("分片结束越界: {end}")))?;
    if body.len() < stop {
        return Err(Error::Transfer(format!(
            "分片数据不足：需要到 {stop}，实收 {}",
            body.len()
        )));
    }
    Ok(body[begin..stop].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::Md5;
    use std::io::Write;

    /// 验证分片 MD5 计算与 Go 端 FastUpload 一致：
    /// 分片 MD5 = MD5(分片内容) 的大写 hex，而非分片原始字节 hex
    #[tokio::test]
    async fn test_compute_md5s_slice_md5() {
        let dir = std::env::temp_dir();
        let path = dir.join("tianyi_md5_test.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        let data: Vec<u8> = (0..10 * 1024 * 1024 + 123)
            .map(|i| (i % 251) as u8)
            .collect();
        f.write_all(&data).unwrap();
        drop(f);

        let (whole_hex, parts, _) = compute_md5s(&path, 10 * 1024 * 1024, &TransferControl::new())
            .await
            .unwrap();

        // 两个分片：满 10MiB + 123 字节
        assert_eq!(parts.len(), 2);

        // 与 Go 端 md5.Sum 一致
        let expected1 = {
            use md5::Digest;
            let mut h = Md5::new();
            Digest::update(&mut h, &data[..10 * 1024 * 1024]);
            hex::encode_upper(h.finalize())
        };
        let expected2 = {
            use md5::Digest;
            let mut h = Md5::new();
            Digest::update(&mut h, &data[10 * 1024 * 1024..]);
            hex::encode_upper(h.finalize())
        };
        assert_eq!(parts[0], expected1);
        assert_eq!(parts[1], expected2);

        // 分片 MD5 是 32 位 hex（16 字节），而非原始字节 hex
        assert_eq!(parts[0].len(), 32);

        // 整文件 MD5 正确
        use md5::Digest;
        let mut whole = Md5::new();
        Digest::update(&mut whole, &data);
        assert_eq!(whole_hex, hex::encode_upper(whole.finalize()));

        let _ = std::fs::remove_file(&path);
    }

    /// 回归：>4GB 乃至 >10GB 文件必须使用动态分片大小，
    /// 确保分片数不超过天翼云服务端限制（10MiB 分片 ≤999 片，更大 ≤1999 片），
    /// 否则会报 InvalidPartSize / 405。
    #[test]
    fn test_upload_part_size_large_file() {
        const MB: i64 = 1024 * 1024;
        // 4GB 边界
        let size_4g: i64 = 4 * 1024 * MB;
        let ps_4g = crypto::part_size(size_4g) as u64;
        let cnt_4g = (size_4g as u64).div_ceil(ps_4g) as i64;
        assert_eq!(ps_4g, 10 * MB as u64, "4GB should still use 10MiB slices");
        assert!(cnt_4g <= 999, "4GB part count {cnt_4g} exceeds 999");

        // 10GB 边界（此前固定 10MiB 分片会达到 1024 片，超出服务端限制）
        let size_10g: i64 = 10 * 1024 * MB;
        let ps_10g = crypto::part_size(size_10g) as u64;
        let cnt_10g = (size_10g as u64).div_ceil(ps_10g) as i64;
        assert_eq!(ps_10g, 20 * MB as u64, ">9.5GB should use 20MiB slices");
        assert!(cnt_10g <= 999, "10GB part count {cnt_10g} exceeds 999");

        // 100GB：按 1999 片封顶分配更大分片
        let size_100g: i64 = 100 * 1024 * MB;
        let ps_100g = crypto::part_size(size_100g) as u64;
        let cnt_100g = (size_100g as u64).div_ceil(ps_100g) as i64;
        assert!(cnt_100g <= 1999, "100GB part count {cnt_100g} exceeds 1999");
        assert!(
            ps_100g >= 50 * MB as u64,
            "100GB slice should be >= 50MiB, got {ps_100g}"
        );
    }

    #[test]
    fn test_parse_http_headers() {
        let h = parse_http_headers("a=1&b=hello%20world&c=");
        assert_eq!(
            h,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "hello%20world".to_string()),
                ("c".to_string(), String::new()),
            ]
        );
    }

    /// 用本地 mock 服务跑通完整 upload() 流程，验证不会挂起/恐慌
    #[tokio::test]
    async fn test_upload_end_to_end_mock() {
        use crate::config::AppConfig;
        use crate::models::{AccountConfig, TokenInfo};
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};

        let put_count = Arc::new(AtomicU32::new(0));
        let put_count_clone = put_count.clone();
        let commit_lazy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commit_lazy_clone = commit_lazy.clone();

        // 简单 HTTP 服务：根据路径返回对应 JSON，PUT 上传分片返回 200
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}", addr);
        let server_base = base.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let put_count = put_count_clone.clone();
                let server_base = server_base.clone();
                let commit_lazy = commit_lazy_clone.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    // 只读请求头（到 \r\n\r\n），避免等待 keep-alive 关闭连接
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    for _ in 0..65536 {
                        match sock.read(&mut byte).await {
                            Ok(0) => break,
                            Ok(_) => {
                                head.extend_from_slice(&byte);
                                if head.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req = String::from_utf8_lossy(&head);
                    let req_line = req.lines().next().unwrap_or("").to_string();
                    let path = req_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    // 读取请求体（PUT 上传分片带有 Content-Length body），避免连接被中止
                    if path.starts_with("/part") {
                        let cl = req
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        let mut remaining = cl;
                        let mut buf = [0u8; 8192];
                        while remaining > 0 {
                            let n = sock.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            remaining = remaining.saturating_sub(n);
                        }
                    }

                    let mut headers =
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: "
                            .to_string();
                    let body = if path.starts_with("/person/initMultiUpload") {
                        r#"{"code":"SUCCESS","data":{"uploadFileId":"U1","fileDataExists":0,"uploadType":0,"uploadHost":""}}"#.to_string()
                    } else if path.starts_with("/person/getMultiUploadUrls") {
                        format!(
                            r#"{{"code":"SUCCESS","uploadUrls":{{"partNumber_1":{{"requestURL":"{server_base}/part1","requestHeader":"a=b&c=d"}},"partNumber_2":{{"requestURL":"{server_base}/part2","requestHeader":"e=f"}}}}}}"#
                        )
                    } else if path.starts_with("/part") {
                        put_count.fetch_add(1, Ordering::SeqCst);
                        r#""#.to_string()
                    } else if path.starts_with("/person/commitMultiUploadFile") {
                        if req.contains("lazyCheck") {
                            commit_lazy.store(true, Ordering::SeqCst);
                        }
                        r#"{"code":"SUCCESS","file":{"userFileId":"F1","fileName":"mock.bin","fileSize":0,"fileMd5":"","createDate":""}}"#.to_string()
                    } else {
                        r#"{"code":"FAIL","msg":"not found"}"#.to_string()
                    };
                    headers.push_str(&body.len().to_string());
                    headers.push_str("\r\n\r\n");
                    let _ = sock.write_all(headers.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        // 构造带会话 token 的客户端
        let config = AppConfig::default();
        let account = AccountConfig {
            token: TokenInfo {
                session_key: "SK".to_string(),
                session_secret: "0123456789abcdef".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let client = TianyiClient::new(config, account);
        client.set_upload_base(&base);

        // 构造一个 2 分片的小文件（>1 分片才走分片路径）
        let dir = std::env::temp_dir();
        let file_path = dir.join("tianyi_mock_upload.bin");
        let mut f = std::fs::File::create(&file_path).unwrap();
        let data: Vec<u8> = (0..(10 * 1024 * 1024 + 7))
            .map(|i| (i % 253) as u8)
            .collect();
        f.write_all(&data).unwrap();
        drop(f);

        let opts = UploadOptions {
            rapid_upload: false,
            thread_count: 2,
            part_size: 10 * 1024 * 1024,
            ..Default::default()
        };
        let done = Arc::new(AtomicU64::new(0));
        let done_cb = done.clone();
        let progress: ProgressCallback = Arc::new(move |n| {
            done_cb.store(n, Ordering::SeqCst);
        });

        let commit = match upload(
            &client,
            "FOLDER",
            &file_path,
            &opts,
            progress,
            &TransferControl::new(),
        )
        .await
        .expect("upload should complete without hang")
        {
            UploadOutcome::Completed(commit) => commit,
            other => panic!("expected completed upload, got {other:?}"),
        };

        assert_eq!(commit.user_file_id, "F1");
        assert_eq!(done.load(Ordering::SeqCst), data.len() as u64);
        assert_eq!(
            put_count.load(Ordering::SeqCst),
            2,
            "both parts should be PUT"
        );
        assert!(
            !commit_lazy.load(Ordering::SeqCst),
            "commit must NOT carry lazyCheck without fileMd5/sliceMd5"
        );

        let _ = std::fs::remove_file(&file_path);
        server.abort();
    }

    // -----------------------------------------------------------------
    // 分片规划 / 下载前缀安全 / 暂停恢复取消
    // -----------------------------------------------------------------

    const MIB: u64 = 1024 * 1024;
    const KIB_U: u64 = 1024;

    #[test]
    fn test_plan_chunks_coverage() {
        // 检查：升序、无重叠、无空洞、恰好覆盖 [start, file_size)
        fn check(file_size: u64, start: u64, chunk: u64) {
            let chunks = plan_chunks(file_size, start, chunk);
            let limit = if chunk == 0 {
                DEFAULT_CHUNK_SIZE
            } else {
                chunk
            };
            let mut next_offset = start;
            for (offset, len) in chunks.iter().copied() {
                assert_eq!(offset, next_offset, "分片必须严格升序且连续");
                assert!(len > 0, "分片长度必须 > 0");
                assert!(len <= limit, "分片长度不能超过 chunk_size");
                next_offset += len;
            }
            assert_eq!(
                next_offset,
                file_size.max(start),
                "必须恰好覆盖到文件末尾，不能有空洞或越界"
            );
        }

        check(10 * MIB + 1, 0, 4 * MIB);
        check(10 * MIB + 1, 4 * MIB, 4 * MIB);
        check(10 * MIB + 1, 10 * MIB, 4 * MIB);
        check(10 * MIB + 1, 10 * MIB + 1, 4 * MIB);
        check(1, 0, 4 * MIB);
        check(4 * MIB, 0, 4 * MIB);
        check(4 * MIB, 0, 0);

        // 具体分割结果
        assert_eq!(
            plan_chunks(10 * MIB + 1, 0, 4 * MIB),
            vec![(0, 4 * MIB), (4 * MIB, 4 * MIB), (8 * MIB, 2 * MIB + 1)]
        );
        assert_eq!(
            plan_chunks(10 * MIB + 1, 4 * MIB, 4 * MIB),
            vec![(4 * MIB, 4 * MIB), (8 * MIB, 2 * MIB + 1)]
        );

        // 边界：空文件 / 起点越界 / chunk_size == 0（按 4 MiB）
        assert!(plan_chunks(0, 0, 4 * MIB).is_empty());
        assert!(plan_chunks(0, 0, 0).is_empty());
        assert!(plan_chunks(100, 100, 4 * MIB).is_empty());
        assert!(plan_chunks(100, 200, 4 * MIB).is_empty());
        assert_eq!(
            plan_chunks(4 * MIB + 1, 0, 0),
            vec![(0, 4 * MIB), (4 * MIB, 1)]
        );
        assert_eq!(plan_chunks(10, 0, 4 * MIB), vec![(0, 10)]);
    }

    /// 测试用客户端（只用到 HTTP 层，不访问真实 API）
    fn test_client() -> TianyiClient {
        use crate::config::AppConfig;
        use crate::models::{AccountConfig, TokenInfo};
        let account = AccountConfig {
            token: TokenInfo {
                session_key: "SK".to_string(),
                session_secret: "0123456789abcdef".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        TianyiClient::new(AppConfig::default(), account)
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tianyi-dl-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn parse_range_header(req: &str) -> Option<(u64, u64)> {
        for line in req.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("range:") {
                let spec = rest.trim().strip_prefix("bytes=")?;
                let (a, b) = spec.split_once('-')?;
                return Some((a.trim().parse().ok()?, b.trim().parse().ok()?));
            }
        }
        None
    }

    /// 极简 HTTP 服务：支持 Range；`ignore_range` 为真时永远返回 200 整包
    async fn spawn_range_server(
        data: Vec<u8>,
        ignore_range: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let data = Arc::new(data);
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let data = data.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    for _ in 0..65536 {
                        match sock.read(&mut byte).await {
                            Ok(0) => break,
                            Ok(_) => {
                                head.push(byte[0]);
                                if head.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req = String::from_utf8_lossy(&head).to_string();
                    let (status, body) = match parse_range_header(&req) {
                        Some((s, e)) if !ignore_range => {
                            let s = (s as usize).min(data.len());
                            let e = ((e as usize) + 1).min(data.len());
                            ("206 Partial Content", data[s..e].to_vec())
                        }
                        _ => ("200 OK", data.as_ref().clone()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn collect_progress() -> (ProgressCallback, Arc<std::sync::Mutex<Vec<u64>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let seen_cb = seen.clone();
        let cb: ProgressCallback = Arc::new(move |n| {
            seen_cb.lock().unwrap_or_else(|e| e.into_inner()).push(n);
        });
        (cb, seen)
    }

    /// 完整下载：分片按序落盘，进度单调递增，结束后 `.part` 被重命名为目标文件
    #[tokio::test]
    async fn test_download_completes_and_reports_monotonic_progress() {
        let size = 10 * MIB + 123;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let (base, server) = spawn_range_server(data.clone(), false).await;
        let dir = tmp_dir("complete");
        let dest = dir.join("out.bin");
        let client = test_client();
        let opts = DownloadOptions {
            thread_count: 4,
            resume: true,
            chunk_size: MIB,
            // 这些既有用例验证的是暂停/续传/前缀不变量，不限速时行为必须与
            // 引入限速前逐字节一致
            rate: None,
        };
        let control = TransferControl::new();
        let (progress, seen) = collect_progress();

        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &dest,
            &opts,
            progress,
            &control,
            Some(&format!("{base}/dl")),
        )
        .await
        .expect("download ok");

        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("read dest"), data);
        assert!(!part_path_of(&dest).exists(), ".part 应已重命名");

        let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "进度必须单调不减: {seen:?}"
        );
        assert_eq!(*seen.last().expect("至少上报一次进度"), size);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 暂停：同一个 future 挂起（不失败），`.part` 仍是合法前缀；恢复后继续跑完
    #[tokio::test]
    async fn test_download_pause_parks_then_resumes() {
        let size = 6 * MIB;
        let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
        let (base, server) = spawn_range_server(data.clone(), false).await;
        let dir = tmp_dir("pause");
        let dest = dir.join("out.bin");
        let client = test_client();
        let opts = DownloadOptions {
            thread_count: 2,
            resume: true,
            chunk_size: MIB,
            // 这些既有用例验证的是暂停/续传/前缀不变量，不限速时行为必须与
            // 引入限速前逐字节一致
            rate: None,
        };
        let control = TransferControl::new();
        let pauser = control.clone();
        // 只暂停一次：否则恢复后的下一次进度回调会把它再次按下
        let paused_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress: ProgressCallback = Arc::new(move |n| {
            if n >= MIB && !paused_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                pauser.pause();
            }
        });

        let url = format!("{base}/dl");
        let task_client = client.clone_for_transfer();
        let task_dest = dest.clone();
        let task_control = control.clone();
        let mut task = tokio::spawn(async move {
            download_impl(
                &task_client,
                "file-id",
                size,
                &task_dest,
                &opts,
                progress,
                &task_control,
                Some(&url),
            )
            .await
        });

        // 等进度回调请求暂停，并确认 future 停在检查点上
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(control.state(), ControlState::Paused, "应已请求暂停");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut task)
                .await
                .is_err(),
            "暂停期间下载 future 必须保持 pending，而不是失败或返回"
        );

        // 暂停期间：没有最终文件，`.part` 必须是目标文件的合法前缀
        assert!(!dest.exists(), "暂停不应生成最终文件");
        let part = part_path_of(&dest);
        let prefix = std::fs::read(&part).expect("暂停时应保留 .part");
        assert!(!prefix.is_empty());
        assert_eq!(
            prefix,
            data[..prefix.len()].to_vec(),
            ".part 在任何时刻都必须是目标文件的合法前缀"
        );

        // 恢复：同一个 future 接着跑完
        assert!(control.resume());
        let outcome = tokio::time::timeout(Duration::from_secs(60), task)
            .await
            .expect("恢复后应能完成")
            .expect("join ok")
            .expect("download ok");
        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("read dest"), data);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取消：返回 Canceled、删除 `.part`、绝不留下最终文件
    #[tokio::test]
    async fn test_download_cancel_discards_part_file() {
        let size = 4 * MIB;
        let data: Vec<u8> = (0..size).map(|i| (i % 249) as u8).collect();
        let (base, server) = spawn_range_server(data, false).await;
        let dir = tmp_dir("cancel");
        let dest = dir.join("out.bin");
        let client = test_client();
        let opts = DownloadOptions {
            thread_count: 2,
            resume: true,
            chunk_size: MIB,
            // 这些既有用例验证的是暂停/续传/前缀不变量，不限速时行为必须与
            // 引入限速前逐字节一致
            rate: None,
        };
        let control = TransferControl::new();
        let canceler = control.clone();
        let progress: ProgressCallback = Arc::new(move |n| {
            if n >= MIB {
                canceler.cancel();
            }
        });

        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &dest,
            &opts,
            progress,
            &control,
            Some(&format!("{base}/dl")),
        )
        .await
        .expect("取消不是错误");
        assert_eq!(outcome, DownloadOutcome::Canceled);
        assert!(!dest.exists(), "取消不应产生最终文件");
        assert!(!part_path_of(&dest).exists(), "取消必须删除 .part");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 断点续传：从 `.part` 现有长度继续（长度不必与分片边界对齐）
    #[tokio::test]
    async fn test_download_resumes_from_existing_part_prefix() {
        let size = 4 * MIB + 7;
        let data: Vec<u8> = (0..size).map(|i| (i % 247) as u8).collect();
        let (base, server) = spawn_range_server(data.clone(), false).await;
        let dir = tmp_dir("resume");
        let dest = dir.join("out.bin");
        // 预置 3_000_000 字节的合法前缀（不是 chunk_size 的整数倍）
        let resume_from = 3_000_000u64;
        std::fs::write(part_path_of(&dest), &data[..resume_from as usize]).expect("写 .part");

        let client = test_client();
        let opts = DownloadOptions {
            thread_count: 3,
            resume: true,
            chunk_size: MIB,
            // 这些既有用例验证的是暂停/续传/前缀不变量，不限速时行为必须与
            // 引入限速前逐字节一致
            rate: None,
        };
        let control = TransferControl::new();
        let (progress, seen) = collect_progress();

        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &dest,
            &opts,
            progress,
            &control,
            Some(&format!("{base}/dl")),
        )
        .await
        .expect("download ok");

        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("read dest"), data);
        let first = seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first()
            .copied()
            .expect("至少一次进度");
        assert!(
            first > resume_from,
            "必须从续传点继续（首次进度 {first} 应大于 {resume_from}）"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resume: false`：丢弃脏 `.part`，从头下载
    #[tokio::test]
    async fn test_download_without_resume_truncates_part() {
        let size = 2 * MIB + 5;
        let data: Vec<u8> = (0..size).map(|i| (i % 241) as u8).collect();
        let (base, server) = spawn_range_server(data.clone(), false).await;
        let dir = tmp_dir("noresume");
        let dest = dir.join("out.bin");
        // 脏临时文件：长度合法但内容是垃圾
        std::fs::write(part_path_of(&dest), vec![0xFFu8; MIB as usize]).expect("写脏 .part");

        let client = test_client();
        let opts = DownloadOptions {
            thread_count: 2,
            resume: false,
            chunk_size: MIB,
            // 这些既有用例验证的是暂停/续传/前缀不变量，不限速时行为必须与
            // 引入限速前逐字节一致
            rate: None,
        };
        let progression = TransferControl::new();
        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &dest,
            &opts,
            Arc::new(|_| {}),
            &progression,
            Some(&format!("{base}/dl")),
        )
        .await
        .expect("download ok");

        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("read dest"), data);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 服务端忽略 Range 直接返回整包（200）时，按窗口截取，不能破坏前缀不变量
    #[tokio::test]
    async fn test_download_tolerates_server_ignoring_range() {
        let size = 3 * MIB + 11;
        let data: Vec<u8> = (0..size).map(|i| (i % 239) as u8).collect();
        let (base, server) = spawn_range_server(data.clone(), true).await;
        let dir = tmp_dir("norange");
        let dest = dir.join("out.bin");
        let client = test_client();
        let opts = DownloadOptions {
            thread_count: 2,
            resume: true,
            chunk_size: MIB,
            // 这些既有用例验证的是暂停/续传/前缀不变量，不限速时行为必须与
            // 引入限速前逐字节一致
            rate: None,
        };

        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &dest,
            &opts,
            Arc::new(|_| {}),
            &TransferControl::new(),
            Some(&format!("{base}/dl")),
        )
        .await
        .expect("download ok");
        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("read dest"), data);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 入口就是暂停态：立即让出，不联网、不创建任何文件
    #[tokio::test]
    async fn test_download_yields_immediately_when_paused_at_entry() {
        let dir = tmp_dir("paused-entry");
        let dest = dir.join("out.bin");
        let client = test_client();
        let control = TransferControl::new();
        assert!(control.pause());

        let outcome = download_impl(
            &client,
            "file-id",
            1024,
            &dest,
            &DownloadOptions::default(),
            Arc::new(|_| {}),
            &control,
            // 故意给一个连不上的地址：入口暂停时根本不会发起请求
            Some("http://127.0.0.1:1/never"),
        )
        .await
        .expect("暂停不是错误");
        assert_eq!(outcome, DownloadOutcome::Paused);
        assert!(!dest.exists());
        assert!(!part_path_of(&dest).exists());

        // 入口就是取消态：同样立即返回，且不留下任何文件
        let canceled = TransferControl::new();
        assert!(canceled.cancel());
        let outcome = tokio::time::timeout(
            Duration::from_millis(500),
            download_impl(
                &client,
                "file-id",
                1024,
                &dest,
                &DownloadOptions::default(),
                Arc::new(|_| {}),
                &canceled,
                Some("http://127.0.0.1:1/never"),
            ),
        )
        .await
        .expect("取消态不应阻塞")
        .expect("取消不是错误");
        assert_eq!(outcome, DownloadOutcome::Canceled);
        assert!(!dest.exists());
        assert!(!part_path_of(&dest).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // 限速
    // -----------------------------------------------------------------

    /// 端到端：mock 服务下对同一份数据做「不限速 vs 限速」两次下载，
    /// 限速那次耗时必须 ≥ 字节数 / 速率。
    ///
    /// 这是**强断言**（下界）：tokio 定时器不会提前到期，所以下界方向永不为假；
    /// 反过来「限速一定比不限速慢」也用实际耗时比较了，避免出现「两边一样快
    /// 但下界恰好蒙混过关」的情况。本用例吃真实时间约 0.5 s。
    #[tokio::test]
    async fn test_download_throttled_slower_than_unthrottled() {
        let size = 2 * MIB;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let (base, server) = spawn_range_server(data.clone(), false).await;
        let url = format!("{base}/dl");
        let client = test_client();
        let dir = tmp_dir("throttle");

        // 对照组：不限速（rate: None 时必须零额外开销）
        let free_dest = dir.join("free.bin");
        let free_opts = DownloadOptions {
            thread_count: 2,
            resume: true,
            chunk_size: MIB,
            rate: None,
        };
        let free_start = std::time::Instant::now();
        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &free_dest,
            &free_opts,
            Arc::new(|_| {}),
            &TransferControl::new(),
            Some(&url),
        )
        .await
        .expect("不限速下载应完成");
        let free_elapsed = free_start.elapsed();
        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&free_dest).expect("read free"), data);

        // 实验组：2 MiB @ 4 MiB/s → 至少 500 ms
        let rate = 4 * MIB;
        let slow_dest = dir.join("slow.bin");
        let slow_opts = DownloadOptions {
            thread_count: 2,
            resume: true,
            chunk_size: MIB,
            rate: crate::ratelimit::bucket_for(rate),
        };
        let slow_start = std::time::Instant::now();
        let outcome = download_impl(
            &client,
            "file-id",
            size,
            &slow_dest,
            &slow_opts,
            Arc::new(|_| {}),
            &TransferControl::new(),
            Some(&url),
        )
        .await
        .expect("限速下载应完成");
        let slow_elapsed = slow_start.elapsed();
        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(
            std::fs::read(&slow_dest).expect("read slow"),
            data,
            "限速不能改变落盘的字节"
        );

        let lower_bound = Duration::from_secs_f64(size as f64 / rate as f64);
        assert!(
            slow_elapsed >= lower_bound,
            "限速下载耗时 {slow_elapsed:?} 必须 ≥ 字节数/速率 {lower_bound:?}"
        );
        assert!(
            slow_elapsed > free_elapsed,
            "限速必须真的比不限速慢：限速 {slow_elapsed:?} vs 不限速 {free_elapsed:?}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 上传侧节流点：分片按 64 KiB 一步读、每步向共享桶租借，
    /// 因此读一个分片的耗时 ≥ 分片字节数 / 速率，且读到的内容不变。
    #[tokio::test]
    async fn test_upload_read_is_throttled() {
        let dir = tmp_dir("upload-throttle");
        let path = dir.join("part.bin");
        let len = 512 * KIB_U;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).expect("写测试文件");

        // 不限速：直通路径
        let free = read_part_throttled(&path, 0, len, None)
            .await
            .expect("不限速读取");
        assert_eq!(free, data);

        // 限速 2 MiB/s：512 KiB 至少 250 ms
        let rate = crate::ratelimit::bucket_for(2 * MIB);
        let start = std::time::Instant::now();
        let slow = read_part_throttled(&path, 0, len, rate.as_deref())
            .await
            .expect("限速读取");
        let elapsed = start.elapsed();
        assert_eq!(slow, data, "限速不能改变读到的数据");

        let lower_bound = Duration::from_millis(250);
        assert!(
            elapsed >= lower_bound,
            "读 512 KiB @ 2 MiB/s 至少 {lower_bound:?}，实测 {elapsed:?}"
        );

        // 非零偏移也要正确：从 64 KiB 处读 64 KiB
        let tail = read_part_throttled(&path, KIB_U, KIB_U, None)
            .await
            .expect("带偏移读取");
        assert_eq!(tail, data[KIB_U as usize..2 * KIB_U as usize].to_vec());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
