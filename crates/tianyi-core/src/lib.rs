//! 天翼云盘核心库：登录、API、上传、下载、任务管理
//!
//! 移植自 OpenList 项目的 `drivers/189pc`（天翼云盘 PC 驱动）。

pub mod api;
pub mod client;
pub mod config;
pub mod crypto;
pub mod error;
pub mod file;
pub mod models;
pub mod task;
pub mod torrent;
pub mod transfer;

pub use client::TianyiClient;
pub use error::{Error, Result};
pub use models::*;
