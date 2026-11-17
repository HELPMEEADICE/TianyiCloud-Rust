//! Linux FUSE 后端（只读挂载）
//!
//! 与 Windows 侧共用 [crate::fs::CloudFs]：路径解析、目录缓存、严格 Range 读取全在那边，
//! 这里只把 FUSE 请求翻译成同一次调用。
//!
//! 本文件在 Windows 构建里不参与编译（lib.rs 用 cfg 关掉），因此 Windows 提交可以
//! 完全不依赖 libfuse。

use crate::error::{Error, Result};
use crate::fs::CloudFs;
use crate::mount::{MountOptions, MountTarget, PlatformMount};
use std::sync::Arc;

pub fn start(
    _cloud: Arc<CloudFs>,
    _options: &MountOptions,
    _target: &MountTarget,
) -> Result<(Arc<PlatformMount>, &'static str)> {
    Err(Error::Other(
        "Linux FUSE 后端尚未在此构建中启用（需要 --features fuse 且目标为 Linux）".into(),
    ))
}
