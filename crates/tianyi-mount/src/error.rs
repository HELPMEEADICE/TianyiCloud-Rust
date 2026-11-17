//! 挂载层错误类型
//!
//! 单独建一套错误而不是直接复用 [tianyi_core::Error]：挂载层有一类核心库不存在的失败，
//! 比如「服务端忽略 Range」「挂载点被占用」「WinFsp 运行时缺失」。这些错误的处置方式与
//! 云端业务错误完全不同——调用方需要据此给出可诊断的提示（GUI）或退出码（CLI）。

use std::time::Duration;
use tianyi_core::Error as CoreError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("云端错误：{0}")]
    Cloud(#[from] CoreError),

    #[error("挂载点无效：{0}")]
    InvalidMountPoint(String),

    #[error("挂载点已被占用：{0}")]
    MountPointBusy(String),

    #[error(
        "未找到 WinFsp 运行时（{0}）。请先安装 WinFsp：https://winfsp.dev/rel/ ，安装后重新挂载。"
    )]
    WinFspMissing(String),

    #[error("WinFsp 调用失败：{0}")]
    WinFsp(String),

    #[error("服务端不支持 Range 分段读取（HTTP {status}）。零磁盘缓存模式下禁止退化为整包下载，读取已中止")]
    RangeUnsupported { status: u16 },

    #[error("Range 响应不合规：{0}")]
    RangeMismatch(String),

    #[error("请求超时（{0:?}）")]
    Timeout(Duration),

    #[error("网络错误：{0}")]
    Network(String),

    #[error("路径非法：{0}")]
    InvalidPath(String),

    #[error("对象不存在：{0}")]
    NotFound(String),

    #[error("只读挂载，拒绝修改：{0}")]
    ReadOnly(String),

    #[error("挂载未就绪：{0}")]
    NotReady(String),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// 该错误是否意味着「继续重试没有意义」
    ///
    /// 只读挂载里最有价值的分类：直链过期要重取直链重试，限流要退避重试，
    /// 而「服务端不支持 Range」「路径非法」这类错误重试多少次都是同一个结果。
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::RangeUnsupported { .. }
                | Error::InvalidMountPoint(_)
                | Error::InvalidPath(_)
                | Error::ReadOnly(_)
                | Error::WinFspMissing(_)
        )
    }

    /// 该错误是否提示「下载直链已失效，需要重新申请」
    pub fn is_url_expired(&self) -> bool {
        matches!(self, Error::Network(msg) if msg.contains("403") || msg.contains("401") || msg.contains("410"))
    }

    /// 给操作系统的稳定一句话描述（不含任何认证信息）
    pub fn brief(&self) -> String {
        match self {
            Error::Cloud(e) => format!("云端：{e}"),
            other => other.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
