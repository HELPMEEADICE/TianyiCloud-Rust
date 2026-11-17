//! 传输作业：GUI 与 CLI 共用的「一次上传 / 一次下载」执行体
//!
//! 抽出来的理由：GUI 的控制器里原本有一段 60 行的分支逻辑——按方向构造
//! DownloadOptions / UploadOptions，再分别处理「单文件上传 / 目录递归上传 / 下载」三种
//! 调用形状与它们各自的暂停/取消返回值。CLI 需要一模一样的行为（同样分片、同样秒传、
//! 同样 .part 断点续传语义），照抄一遍就会出现「命令行传的和图形端传的不一样」这种
//! 最难查的分叉。
//!
//! 这一层只负责「一次作业怎么跑」，不负责排队、不负责落盘任务表、不负责 UI 提示：
//! 那些是 [crate::task] 与各前端的事。

use crate::client::TianyiClient;
use crate::config::AppConfig;
use crate::error::Error;
use crate::ratelimit::RateLimiter;
use crate::task::{TaskDirection, TransferControl};
use crate::transfer::{
    self, DownloadOptions, DownloadOutcome, ProgressCallback, UploadOptions, UploadOutcome,
};
use std::path::PathBuf;
use std::sync::Arc;

/// 一次作业的输入
#[derive(Debug, Clone)]
pub struct JobRequest {
    /// 上传还是下载
    pub direction: TaskDirection,
    /// 下载时为文件 id；上传时为父目录 id
    pub remote_id: String,
    /// 文件名（用于错误提示与下载命名）
    pub remote_name: String,
    /// 文件大小（0 表示未知）
    pub remote_size: u64,
    /// 本地路径：下载时为落盘目标文件，上传时为源文件/源目录
    pub local_path: PathBuf,
}

impl JobRequest {
    /// 下载一个云端文件
    pub fn download(file_id: &str, file_name: &str, file_size: u64, dest: PathBuf) -> Self {
        JobRequest {
            direction: TaskDirection::Download,
            remote_id: file_id.to_string(),
            remote_name: file_name.to_string(),
            remote_size: file_size,
            local_path: dest,
        }
    }

    /// 上传一个本地路径到云端目录（目录会自动递归）
    pub fn upload(parent_folder_id: &str, local_path: PathBuf) -> Self {
        let name = local_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let size = std::fs::metadata(&local_path)
            .map(|m| if m.is_dir() { 0 } else { m.len() })
            .unwrap_or(0);
        JobRequest {
            direction: TaskDirection::Upload,
            remote_id: parent_folder_id.to_string(),
            remote_name: name,
            remote_size: size,
            local_path,
        }
    }
}

/// 一次作业的运行参数（两端都由配置算出来，保证同一份配置得到同一套行为）
#[derive(Debug, Clone)]
pub struct JobOptions {
    pub download: DownloadOptions,
    pub upload: UploadOptions,
}

/// 由配置推导运行参数
///
/// rate 由调用方按「整个进程共享一个限速桶」的规则取好传进来（见
/// [crate::ratelimit::bucket_for]）：每个任务各造一个桶等于把用户设的额度复制 N 份。
pub fn options_for(cfg: &AppConfig, rate: Option<Arc<RateLimiter>>) -> JobOptions {
    JobOptions {
        download: DownloadOptions {
            thread_count: cfg.download_thread.max(1),
            resume: true,
            chunk_size: transfer::DEFAULT_CHUNK_SIZE,
            rate: rate.clone(),
        },
        upload: UploadOptions {
            overwrite: true,
            rapid_upload: cfg.rapid_upload,
            thread_count: cfg.upload_thread.max(1),
            part_size: 0,
            generate_torrent: cfg.generate_torrent,
            rate,
        },
    }
}

/// 作业结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// 全部完成
    Completed,
    /// 用户暂停：下载的 .part 是合法前缀，上传会话已释放但可重来
    Paused,
    /// 用户取消
    Canceled,
    /// 失败（含网络、业务错误，文案直接可展示）
    Failed(String),
}

impl JobOutcome {
    /// 是否为「可恢复的中断」（暂停/取消），不是真正的失败
    pub fn is_interrupted(&self) -> bool {
        matches!(self, JobOutcome::Paused | JobOutcome::Canceled)
    }
}

/// 执行一次作业
///
/// 入口处若控制句柄已是暂停/取消态，会立即返回对应结果（不打开文件、不联网）。
pub async fn run(
    client: &TianyiClient,
    req: &JobRequest,
    opts: &JobOptions,
    progress: ProgressCallback,
    control: &TransferControl,
) -> JobOutcome {
    match req.direction {
        TaskDirection::Download => {
            let file = crate::models::FileObject {
                id: req.remote_id.clone(),
                name: req.remote_name.clone(),
                size: req.remote_size as i64,
                ..Default::default()
            };
            match transfer::download(
                client,
                &file,
                &req.local_path,
                &opts.download,
                progress,
                control,
            )
            .await
            {
                Ok(DownloadOutcome::Completed) => JobOutcome::Completed,
                Ok(DownloadOutcome::Paused) => JobOutcome::Paused,
                Ok(DownloadOutcome::Canceled) => JobOutcome::Canceled,
                Err(e) if e.is_interrupted() => from_interrupted(&e),
                Err(e) => JobOutcome::Failed(e.to_string()),
            }
        }
        TaskDirection::Upload => {
            if req.local_path.is_dir() {
                match transfer::upload_folder_recursive(
                    client,
                    &req.remote_id,
                    &req.local_path,
                    &opts.upload,
                    progress,
                    control,
                )
                .await
                {
                    Ok(_) => JobOutcome::Completed,
                    Err(e) if e.is_interrupted() => from_interrupted(&e),
                    Err(e) => JobOutcome::Failed(e.to_string()),
                }
            } else {
                match transfer::upload(
                    client,
                    &req.remote_id,
                    &req.local_path,
                    &opts.upload,
                    progress,
                    control,
                )
                .await
                {
                    Ok(UploadOutcome::Completed(_)) => JobOutcome::Completed,
                    Ok(UploadOutcome::Paused) => JobOutcome::Paused,
                    Ok(UploadOutcome::Canceled) => JobOutcome::Canceled,
                    Err(e) if e.is_interrupted() => from_interrupted(&e),
                    Err(e) => JobOutcome::Failed(e.to_string()),
                }
            }
        }
    }
}

/// 把「用户主动中断」的错误映射成暂停/取消
fn from_interrupted(e: &Error) -> JobOutcome {
    match e {
        Error::Paused => JobOutcome::Paused,
        Error::Canceled => JobOutcome::Canceled,
        other => JobOutcome::Failed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn options_follow_config_and_keep_rate_optional() {
        let mut cfg = AppConfig::default_config();
        cfg.download_thread = 0; // 0 会被钳到 1，不能出现 0 路并发
        cfg.upload_thread = 8;
        cfg.rapid_upload = false;
        cfg.generate_torrent = true;
        let opts = options_for(&cfg, None);
        assert_eq!(opts.download.thread_count, 1);
        assert!(opts.download.resume, "下载永远允许断点续传");
        assert_eq!(opts.upload.thread_count, 8);
        assert!(!opts.upload.rapid_upload);
        assert!(opts.upload.generate_torrent);
        assert!(opts.download.rate.is_none());
        assert!(opts.upload.rate.is_none());
    }

    #[test]
    fn rate_bucket_is_shared_by_both_directions() {
        let cfg = AppConfig::default_config();
        let bucket = crate::ratelimit::bucket_for(1024 * 1024).expect("有限速时应造桶");
        let opts = options_for(&cfg, Some(bucket));
        let up = opts.upload.rate.clone().expect("上传应带桶");
        let down = opts.download.rate.clone().expect("下载应带桶");
        assert!(Arc::ptr_eq(&up, &down), "上传下载必须共用同一个桶");
    }

    #[test]
    fn interrupted_errors_map_to_paused_or_canceled() {
        assert_eq!(from_interrupted(&Error::Paused), JobOutcome::Paused);
        assert_eq!(from_interrupted(&Error::Canceled), JobOutcome::Canceled);
        assert!(matches!(
            from_interrupted(&Error::Api("x".into())),
            JobOutcome::Failed(_)
        ));
        assert!(JobOutcome::Paused.is_interrupted());
        assert!(!JobOutcome::Completed.is_interrupted());
    }
}
