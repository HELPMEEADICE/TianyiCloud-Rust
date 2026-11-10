//! 天翼云盘核心库：登录、API、上传、下载、任务管理
//!
//! 移植自 OpenList 项目的 `drivers/189pc`（天翼云盘 PC 驱动）。

pub mod api;
pub mod client;
pub mod config;
pub mod crypto;
pub mod error;
pub mod file;
mod fsutil;
pub mod models;
pub(crate) mod retry;
pub mod task;
pub mod theme;
pub mod torrent;
pub mod transfer;

pub use client::TianyiClient;
pub use error::{Error, Result};
pub use models::*;

/// 取锁；若锁已被 panic 毒化，记录 warn 并继续用内部数据（不连锁 panic）
///
/// 客户端里每个 `Mutex` 装的都是「可重建的账号/会话状态」：中毒只说明之前某次持锁期间
/// panic 过一次，数据本身仍然可用。直接 unwrap 取锁会让之后每一次取锁都 panic，把
/// 一次局部故障放大成整个进程不可用——尤其是多线程传输时，一次 panic 就能带走所有
/// 后续请求。语义与 `tianyi-app` 控制器里的同名函数保持一致。
pub fn lock_or_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        log::warn!("互斥锁已中毒，继续使用其中的数据（避免连锁 panic）");
        e.into_inner()
    })
}
