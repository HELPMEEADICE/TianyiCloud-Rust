//! 传输任务管理器：上传/下载任务的队列、进度、暂停/恢复、持久化

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 任务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Running,
    Paused,
    Canceled,
    Completed,
    Failed,
}

/// 任务方向
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskDirection {
    Upload,
    Download,
}

/// 传输任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub direction: TaskDirection,
    pub file_name: String,
    pub file_size: u64,
    pub local_path: PathBuf,
    pub remote_folder_id: String,
    pub status: TaskStatus,
    pub progress: f32,
    pub bytes_done: u64,
    pub error: Option<String>,
    pub created_at: i64,
}

/// 任务管理器（线程安全，供后台 tokio 任务使用）
pub struct TaskManager {
    tasks: Mutex<Vec<Task>>,
    next_id: AtomicU64,
    /// 持久化路径
    persist_path: Option<PathBuf>,
}

impl TaskManager {
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let mut tm = TaskManager {
            tasks: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            persist_path,
        };
        tm.load();
        tm
    }

    /// 添加任务，返回任务 ID
    pub fn add(&self, task: Task) -> u64 {
        let id = task.id;
        let mut guard = self.tasks.lock().unwrap();
        guard.push(task);
        drop(guard);
        self.persist();
        id
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// 获取任务列表副本
    pub fn list(&self) -> Vec<Task> {
        self.tasks.lock().unwrap().clone()
    }

    /// 更新任务
    pub fn update(&self, id: u64, f: impl FnOnce(&mut Task)) {
        let mut guard = self.tasks.lock().unwrap();
        if let Some(t) = guard.iter_mut().find(|t| t.id == id) {
            f(t);
        }
        drop(guard);
        self.persist();
    }

    /// 删除任务
    pub fn remove(&self, id: u64) {
        let mut guard = self.tasks.lock().unwrap();
        guard.retain(|t| t.id != id);
        drop(guard);
        self.persist();
    }

    pub fn clear_completed(&self) {
        let mut guard = self.tasks.lock().unwrap();
        guard.retain(|t| t.status != TaskStatus::Completed && t.status != TaskStatus::Failed);
        drop(guard);
        self.persist();
    }

    fn persist(&self) {
        if let Some(path) = &self.persist_path {
            let tasks = self.list();
            if let Ok(data) = serde_json::to_string(&tasks) {
                let _ = std::fs::write(path, data);
            }
        }
    }

    fn load(&mut self) {
        if let Some(path) = &self.persist_path {
            if let Ok(data) = std::fs::read_to_string(path) {
                if let Ok(tasks) = serde_json::from_str::<Vec<Task>>(&data) {
                    let max_id = tasks.iter().map(|t| t.id).max().unwrap_or(0);
                    self.next_id.store(max_id + 1, Ordering::SeqCst);
                    *self.tasks.lock().unwrap() = tasks;
                }
            }
        }
    }
}

/// 传输会话（供 UI 绑定进度）
#[derive(Debug)]
pub struct TransferSession {
    pub task_id: u64,
    pub bytes_done: Arc<AtomicU64>,
    pub canceled: Arc<AtomicBool>,
    pub started: Instant,
}

impl TransferSession {
    pub fn new(task_id: u64) -> Self {
        TransferSession {
            task_id,
            bytes_done: Arc::new(AtomicU64::new(0)),
            canceled: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
        }
    }

    pub fn progress_callback(&self, total_size: u64) -> crate::transfer::ProgressCallback {
        let bytes_done = self.bytes_done.clone();
        Arc::new(move |n| {
            bytes_done.store(n, Ordering::SeqCst);
            let _ = total_size;
        })
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
    }
}
