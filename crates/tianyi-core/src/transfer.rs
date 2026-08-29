//! 上传/下载传输实现
//!
//! 上传：initMultiUpload → getMultiUploadUrls → 分片 PUT → commitMultiUploadFile
//! 下载：getDownloadUrl → 并发 Range 分片下载 → 断点续传

use crate::client::consts;
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
            part_size: 10 * 1024 * 1024,
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

            let mut f = tokio::fs::File::open(local_path).await?;
            f.seek(SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf).await?;

            // PUT 上传
            put_part(&client, &req_url, &headers, &buf).await?;
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
    let commit = commit_multipart_upload(client, &init.upload_file_id, opts.overwrite).await?;

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

    let flush_part = |part_buf: &[u8], whole: &mut Md5, parts: &mut Vec<String>, part_sha1s: &mut Vec<u8>| {
        whole.update(part_buf);
        parts.push(hex::encode_upper(part_buf));
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
        parts.push(hex::encode_upper(&[]));
        let h = Sha1::digest(&[]);
        part_sha1s.extend_from_slice(&h);
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
    params.insert("lazyCheck".to_string(), "1".to_string());
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
        format!("{}/family/initMultiUpload", consts::UPLOAD_URL)
    } else {
        format!("{}/person/initMultiUpload", consts::UPLOAD_URL)
    };

    let text = client.get_json_encrypted(&url, params, is_family).await?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse init: {e}")))?;
    let data = &v["data"];
    Ok(InitMultiUploadResp {
        upload_type: data["uploadType"].as_i64().unwrap_or(0) as i32,
        upload_host: data["uploadHost"].as_str().unwrap_or("").to_string(),
        upload_file_id: data["uploadFileId"].as_str().unwrap_or("").to_string(),
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
        format!("{}/family/getMultiUploadUrls", consts::UPLOAD_URL)
    } else {
        format!("{}/person/getMultiUploadUrls", consts::UPLOAD_URL)
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
        .http_client()
        .put(url)
        .body(data.to_vec())
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream");
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
    params.insert("lazyCheck".to_string(), "1".to_string());
    params.insert("isLog".to_string(), "0".to_string());
    params.insert(
        "opertype".to_string(),
        if overwrite { "3".to_string() } else { "1".to_string() },
    );

    let url = if is_family {
        format!("{}/family/commitMultiUploadFile", consts::UPLOAD_URL)
    } else {
        format!("{}/person/commitMultiUploadFile", consts::UPLOAD_URL)
    };

    let text = client.get_json_encrypted(&url, params, is_family).await?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Api(format!("parse commit: {e}")))?;
    let f = &v["file"];
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
