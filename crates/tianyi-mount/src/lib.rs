//! 天翼云盘只读网络挂载
//!
//! 目标：把天翼云盘变成操作系统的原生文件系统，且**绝不把文件内容落到本地磁盘**。
//!
//! # 分层
//!
//! | 层 | 模块 | 职责 |
//! |----|------|------|
//! | 语义 | [fs] | 路径解析、目录缓存、随机读、只读约束 |
//! | 读取 | [reader] | 严格 Range 校验、有界重试、直链刷新 |
//! | 元数据 | [meta] | 目录缓存（TTL + 容量）、时间戳换算 |
//! | 平台 | [win] / [fuse_linux] | 把语义翻译成 WinFsp / FUSE 回调 |
//! | 生命周期 | [mount] | 挂载点校验、状态、卸载 |
//!
//! # 零磁盘文件缓存的边界（必须诚实声明）
//!
//! 本 crate **不会**把云端文件内容写入任何本地文件：没有 `.part`、没有临时目录、
//! 没有块缓存、没有 SQLite 内容缓存。所有读取都是 `bytes=offset-end` 的按需 Range 请求，
//! 数据只在内存里从 HTTP 响应复制进操作系统的缓冲区。
//!
//! 但以下写入**不在**本 crate 的控制范围内，也不做任何承诺：
//!
//! * 操作系统自身的分页文件 / 内存交换；
//! * Windows 搜索索引、缩略图缓存、第三方杀毒软件、备份软件对被读数据的二次落盘；
//! * 用户用资源管理器"复制"文件时产生的目标文件（那是用户自己的写操作）。
//!
//! # 快速上手
//!
//! ```no_run
//! use std::sync::Arc;
//! use tianyi_core::{ConfigStore, Space};
//! use tianyi_mount::{fs::{CloudFs, FsOptions}, mount::{mount, MountOptions}, session::CloudSession};
//!
//! let (store, _degrade) = ConfigStore::open();
//! let session = Arc::new(CloudSession::open(store, None, Space::Personal)?);
//! let fs = Arc::new(CloudFs::new(session, Space::Personal, "user", FsOptions::default())?);
//! let handle = mount(fs, MountOptions { mount_point: "X:".into(), ..Default::default() })?;
//! handle.unmount()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod error;
pub mod fs;
pub mod meta;
pub mod mount;
pub mod paths;
pub mod reader;
pub mod runtime;
pub mod session;
pub mod stats;

#[cfg(windows)]
pub mod win;

#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod fuse_linux;

pub use error::{Error, Result};
pub use fs::{CloudBackend, CloudFs, FsOptions};
pub use mount::{mount, MountHandle, MountOptions, MountStatus, MountTarget};
pub use session::CloudSession;

/// 是否在"零磁盘文件缓存"的意义上安全：编译期常量，供 GUI/CLI 显示真实状态
pub const ZERO_DISK_FILE_CACHE: bool = true;
