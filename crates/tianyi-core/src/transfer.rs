//! 上传/下载传输实现
//!
//! 上传：initMultiUpload → getMultiUploadUrls → 分片 PUT → commitMultiUploadFile
//! 下载：getDownloadUrl → 并发 Range 分片下载 → 断点续传

use crate::client::TianyiClient;
use crate::crypto;
use crate::error::{Error, Result};
use crate::models::*;
use futures::stream::{FuturesUnordered, StreamExt};
use log::{info, warn};
use std::io::SeekFrom;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// 进度回调类型：接收已传输字节数
pub type ProgressCallback = Arc<dyn Fn(u64) + Send + Sync>;

/// 上传选项
#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub overwrite: bool,
    pub rapid_upload: bool,
    pub thread_count: u32,
    pub part_size: i64,
    pub generate_torrent: bool,
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
        }
    }
}

/// 下载选项
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub thread_count: u32,
    pub resume: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        DownloadOptions {
            thread_count: 4,
            resume: true,
        }
    }
}

/// 上传入口
pub async fn upload(
    client: &TianyiClient,
    parent_folder_id: &str,
    local_path: &Path,
    opts: &UploadOptions,
    progress: ProgressCallback,
) -> Result<CommitFile> {
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
    let (file_md5, slice_md5s, piece_sha1s) = compute_md5s(&local_path, part_size).await?;
    info!(
        "upload start: name={} size={} parts={} part_size={}",
        file_name,
        file_size,
        slice_md5s.len(),
        part_size
    );

    // 秒传尝试
    if opts.rapid_upload {
        match try_rapid_upload(
            client,
            parent_folder_id,
            &file_name,
            file_size,
            &file_md5,
            &slice_md5s,
            part_size,
            opts.overwrite,
        )
        .await
        {
            Ok(Some(f)) => {
                info!("rapid upload success: {}", f.file_name);
                progress(file_size);
                return Ok(f);
            }
            Ok(None) => {}
            Err(e) => {
                warn!("rapid upload failed: {e}, fallback to multipart");
            }
        }
    }

    // 初始化多分片上传
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
        // 服务端已有，直接提交
        let commit = commit_multipart_upload(client, &init.upload_file_id, opts.overwrite).await?;
        progress(file_size);
        return Ok(commit);
    }

    // 计算分片数
    let part_size = part_size as u64;
    let count = ((file_size + part_size - 1) / part_size).max(1) as usize;
    let last_size = if file_size % part_size == 0 {
        part_size
    } else {
        file_size % part_size
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
    let part_size_u = part_size;
    let file_size_u = file_size;

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
        let part_size_u = part_size_u;
        let file_size_u = file_size_u;
        let last_size = last_size;
        let count = count;
        let local_path = local_path.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.map_err(|_| Error::Transfer("sem".into()))?;
            // 获取分片上传 URL
            let part_no = part_info.split('-').next().unwrap_or("?").to_string();
            let urls = get_multi_upload_urls(&client, &upload_file_id, &[part_info]).await?;
            let url_info = &urls[0];
            let part_number = url_info.0;
            let req_url = url_info.1.request_url.clone();
            let headers = parse_http_headers(&url_info.1.request_header);

            // 读取该分片数据
            let offset = (part_number as u64 - 1) * part_size_u;
            let len = if part_number as usize == count {
                last_size
            } else {
                part_size_u
            };

            let mut f = tokio::fs::File::open(&local_path).await?;
            f.seek(SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf).await?;

            // PUT 上传（带重试）
            let mut attempt = 0u32;
            loop {
                match put_part(&client, &req_url, &headers, &buf).await {
                    Ok(()) => break,
                    Err(e) => {
                        attempt += 1;
                        if attempt >= 3 {
                            warn!("part {part_no} put failed after {attempt} tries: {e} (url={req_url})");
                            return Err(e);
                        }
                        warn!("part {part_no} put failed (try {attempt}): {e}, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
            info!("part {part_no} uploaded, {} bytes", len);
            uploaded.fetch_add(len, Ordering::SeqCst);
            progress(uploaded.load(Ordering::SeqCst));
            let _ = file_size_u;
            Ok::<(), Error>(())
        }));
    }

    let mut futs = FuturesUnordered::new();
    for h in handles {
        futs.push(h);
    }
    while let Some(res) = futs.next().await {
        res.map_err(|e| Error::Transfer(format!("task join: {e}")))??;
    }

    // 提交
    info!("all {} parts uploaded, committing", count);
    let commit = commit_multipart_upload(client, &init.upload_file_id, opts.overwrite).await?;
    info!("commit ok: file_id={} name={}", commit.user_file_id, commit.file_name);

    // 生成 CAS torrent（可选，异步不影响结果）
    if opts.generate_torrent && !piece_sha1s.is_empty() && file_size > 0 {
        let torrent_name = format!("{}.cas.torrent", file_name);
        match crate::torrent::generate_torrent(
            &file_name,
            file_size as i64,
            &file_md5,
            &slice_md5s,
            part_size as i64,
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
    Ok(commit)
}

/// 计算整文件 MD5、各分片 MD5、各分片 SHA-1 piece hash
async fn compute_md5s(
    path: &Path,
    part_size: i64,
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
    let flush_part = |part_buf: &[u8], whole: &mut Md5, parts: &mut Vec<String>, part_sha1s: &mut Vec<u8>| {
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
        }
    }
    if !part_buf.is_empty() {
        flush_part(&part_buf, &mut whole, &mut parts, &mut part_sha1s);
    }
    if parts.is_empty() {
        // 空文件：一个分片，MD5 为空内容 MD5；无需 SHA-1 piece hash
        let h = Md5::digest(&[]);
        parts.push(hex::encode_upper(h.as_slice()));
    }

    let whole_hex = hex::encode_upper(whole.finalize().as_slice());
    Ok((whole_hex, parts, part_sha1s))
}

/// 秒传
async fn try_rapid_upload(
    client: &TianyiClient,
    parent_id: &str,
    file_name: &str,
    file_size: u64,
    file_md5: &str,
    slice_md5s: &[String],
    slice_size: i64,
    overwrite: bool,
) -> Result<Option<CommitFile>> {
    let init = init_multipart_upload(
        client,
        parent_id,
        file_name,
        file_size,
        file_md5,
        slice_md5s,
        slice_size,
    )
    .await?;

    if init.file_data_exists == 1 {
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
    params.insert(
        "partInfo".to_string(),
        part_infos.join(","),
    );

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
            String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
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
        if overwrite { "3".to_string() } else { "1".to_string() },
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
// 下载
// ---------------------------------------------------------------------------

/// 下载文件（支持断点续传）
pub async fn download(
    client: &TianyiClient,
    file: &FileObject,
    dest_path: &Path,
    opts: &DownloadOptions,
    progress: ProgressCallback,
) -> Result<()> {
    let file_id = file.id.clone();
    let file_size = file.size.max(0) as u64;

    // 获取下载链接
    let dl_url = client.get_download_url(&file_id).await?;

    // 检查续传：.part 文件是否存在且大小正确
    let part_path = dest_path.with_extension(format!("{}.part", "download"));
    let resume = opts.resume && part_path.exists();
    let start_byte = if resume {
        std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    if start_byte >= file_size {
        // 已完成
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&part_path, dest_path)?;
        progress(file_size);
        return Ok(());
    }

    let mut part_file = if resume {
        tokio::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(&part_path)
            .await?
    } else {
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&part_path)
            .await?
    };

    let thread_count = opts.thread_count.max(1) as usize;
    let chunk_size = ((file_size - start_byte) / thread_count as u64).max(1);
    let mut handles = Vec::new();

    for i in 0..thread_count {
        let start = start_byte + i as u64 * chunk_size;
        if start >= file_size {
            break;
        }
        let end = if i == thread_count - 1 {
            file_size
        } else {
            start + chunk_size
        };
        let dl_url = dl_url.clone();
        let client = client.clone_for_transfer();
        let part_path = part_path.clone();
        let progress = progress.clone();
        let file_size = file_size;

        handles.push(tokio::spawn(async move {
            download_chunk(client, &dl_url, start, end, &part_path).await?;
            progress(end);
            let _ = file_size;
            Ok::<(), Error>(())
        }));
    }

    let mut futs = FuturesUnordered::new();
    for h in handles {
        futs.push(h);
    }
    while let Some(res) = futs.next().await {
        let inner = res.map_err(|e| Error::Transfer(format!("task join: {e}")))?;
        inner?;
    }

    part_file.flush().await?;
    drop(part_file);
    std::fs::rename(&part_path, dest_path)?;
    progress(file_size);
    Ok(())
}

/// 下载一个字节区间，写入 .part 文件对应位置
async fn download_chunk(
    client: TianyiClient,
    dl_url: &str,
    start: u64,
    end: u64,
    part_path: &Path,
) -> Result<()> {
    let range = format!("bytes={}-{}", start, end.saturating_sub(1));
    let resp = client
        .http_client()
        .get(dl_url)
        .header(reqwest::header::RANGE, &range)
        .send()
        .await?;
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT && status != reqwest::StatusCode::OK {
        return Err(Error::Api(format!(
            "download chunk http {status}: {}",
            String::from_utf8_lossy(&resp.bytes().await.unwrap_or_default().as_ref())
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(part_path)
        .await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
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
        let data: Vec<u8> = (0..10 * 1024 * 1024 + 123).map(|i| (i % 251) as u8).collect();
        f.write_all(&data).unwrap();
        drop(f);

        let (whole_hex, parts, _) = compute_md5s(&path, 10 * 1024 * 1024).await.unwrap();

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
        let cnt_4g = ((size_4g as u64 + ps_4g - 1) / ps_4g) as i64;
        assert_eq!(ps_4g, 10 * MB as u64, "4GB should still use 10MiB slices");
        assert!(cnt_4g <= 999, "4GB part count {cnt_4g} exceeds 999");

        // 10GB 边界（此前固定 10MiB 分片会达到 1024 片，超出服务端限制）
        let size_10g: i64 = 10 * 1024 * MB;
        let ps_10g = crypto::part_size(size_10g) as u64;
        let cnt_10g = ((size_10g as u64 + ps_10g - 1) / ps_10g) as i64;
        assert_eq!(ps_10g, 20 * MB as u64, ">9.5GB should use 20MiB slices");
        assert!(cnt_10g <= 999, "10GB part count {cnt_10g} exceeds 999");

        // 100GB：按 1999 片封顶分配更大分片
        let size_100g: i64 = 100 * 1024 * MB;
        let ps_100g = crypto::part_size(size_100g) as u64;
        let cnt_100g = ((size_100g as u64 + ps_100g - 1) / ps_100g) as i64;
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
                    let path = req_line.split_whitespace().nth(1).unwrap_or("/").to_string();
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

                    let mut headers = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ".to_string();
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
        let data: Vec<u8> = (0..(10 * 1024 * 1024 + 7)).map(|i| (i % 253) as u8).collect();
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

        let commit = upload(&client, "FOLDER", &file_path, &opts, progress)
            .await
            .expect("upload should complete without hang");

        assert_eq!(commit.user_file_id, "F1");
        assert_eq!(done.load(Ordering::SeqCst), data.len() as u64);
        assert_eq!(put_count.load(Ordering::SeqCst), 2, "both parts should be PUT");
        assert!(
            !commit_lazy.load(Ordering::SeqCst),
            "commit must NOT carry lazyCheck without fileMd5/sliceMd5"
        );

        let _ = std::fs::remove_file(&file_path);
        server.abort();
    }
}
