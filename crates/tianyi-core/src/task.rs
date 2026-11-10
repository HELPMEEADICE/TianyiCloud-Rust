//! 传输任务管理器：上传/下载任务的队列、进度、暂停/恢复、持久化

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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

impl TaskStatus {
    /// 是否终态（Completed / Canceled / Failed，终态不可再转换）
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Canceled | TaskStatus::Failed
        )
    }

    /// 当前状态是否允许暂停
    pub fn can_pause(self) -> bool {
        matches!(self, TaskStatus::Pending | TaskStatus::Running)
    }

    /// 当前状态是否允许恢复
    pub fn can_resume(self) -> bool {
        matches!(self, TaskStatus::Paused)
    }

    /// 当前状态是否允许取消
    pub fn can_cancel(self) -> bool {
        matches!(
            self,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
        )
    }

    /// UI 面向的 ASCII 键（用于 qproperty 绑定/持久化键名，避免中文枚举名）
    pub fn as_key(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Paused => "paused",
            TaskStatus::Canceled => "canceled",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
        }
    }

    /// 中文显示名
    pub fn label_zh(self) -> &'static str {
        match self {
            TaskStatus::Pending => "等待中",
            TaskStatus::Running => "传输中",
            TaskStatus::Paused => "已暂停",
            TaskStatus::Canceled => "已取消",
            TaskStatus::Completed => "已完成",
            TaskStatus::Failed => "失败",
        }
    }
}

/// 状态机：判断 `from -> to` 是否是合法转换
///
/// 合法边：
/// - Pending  -> Running | Paused | Canceled
/// - Running  -> Paused | Canceled | Completed | Failed
/// - Paused   -> Running | Canceled
/// - Completed / Failed / Canceled 为终态，没有出边
///
/// 同状态之间的“转换”（如 Running -> Running）不属于合法边，会返回 false。
fn transition_allowed(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Pending, Running)
            | (Pending, Paused)
            | (Pending, Canceled)
            | (Running, Paused)
            | (Running, Canceled)
            | (Running, Completed)
            | (Running, Failed)
            | (Paused, Running)
            | (Paused, Canceled)
    )
}

/// 传输控制状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    /// 正常传输
    Running,
    /// 用户暂停：传输在下一个检查点挂起，恢复后从原处继续
    Paused,
    /// 用户取消：传输在下一个检查点退出（终态）
    Canceled,
}

/// 协作式传输控制：UI 线程与 tokio 传输任务之间的暂停/恢复/取消开关
///
/// 内部用 `tokio::sync::watch` 传递状态。`checkpoint()` 先读当前值，
/// 发现 `Paused` 才 `changed().await`，因此不会丢失唤醒，也不会忙等；
/// 该 future 是取消安全的（被 drop 不会消费掉状态）。
///
/// `TransferControl` 可自由 `clone()`（全部字段都是轻量的 watch 句柄），
/// 且同时实现 `Send + Sync`，可以跨线程共享、也可以塞进 `Arc<dyn Fn>` 回调。
#[derive(Clone)]
pub struct TransferControl {
    tx: tokio::sync::watch::Sender<ControlState>,
    rx: tokio::sync::watch::Receiver<ControlState>,
}

impl std::fmt::Debug for TransferControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferControl")
            .field("state", &self.state())
            .finish()
    }
}

impl Default for TransferControl {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferControl {
    /// 新建控制器，初始状态为 `Running`
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(ControlState::Running);
        TransferControl { tx, rx }
    }

    /// 当前状态
    pub fn state(&self) -> ControlState {
        *self.rx.borrow()
    }

    /// 请求暂停；仅当状态由 `Running` 变成 `Paused` 时返回 true
    ///
    /// 已经暂停或已取消时是无副作用的空操作（返回 false）——取消是终态，
    /// 暂停请求不能把它复活。
    pub fn pause(&self) -> bool {
        self.tx.send_if_modified(|s| {
            if *s == ControlState::Running {
                *s = ControlState::Paused;
                true
            } else {
                false
            }
        })
    }

    /// 请求恢复；仅当状态由 `Paused` 变成 `Running` 时返回 true
    ///
    /// 取消是终态：`cancel()` 之后调用本方法不会复活任务。
    pub fn resume(&self) -> bool {
        self.tx.send_if_modified(|s| {
            if *s == ControlState::Paused {
                *s = ControlState::Running;
                true
            } else {
                false
            }
        })
    }

    /// 取消传输；仅当状态真正变成 `Canceled` 时返回 true（幂等、终态）
    pub fn cancel(&self) -> bool {
        self.tx.send_if_modified(|s| {
            if *s == ControlState::Canceled {
                false
            } else {
                *s = ControlState::Canceled;
                true
            }
        })
    }

    /// 是否已取消
    pub fn is_canceled(&self) -> bool {
        self.state() == ControlState::Canceled
    }

    /// 协作式检查点
    ///
    /// - `Running`：立即返回 `Ok(())`，传输继续；
    /// - `Canceled`：返回 `Err(Error::Canceled)`；
    /// - `Paused`：挂起当前 future，直到用户 `resume()`（继续）或 `cancel()`（返回
    ///   `Err(Error::Canceled)`）。挂起期间不消耗 CPU，也不会丢失唤醒。
    pub async fn checkpoint(&self) -> Result<()> {
        // 每次检查点各自持有一个 receiver 副本：被 drop 的 future 不会消费掉状态，
        // 因此该 future 是取消安全的。
        let mut rx = self.rx.clone();
        loop {
            match *rx.borrow() {
                ControlState::Running => return Ok(()),
                ControlState::Canceled => return Err(Error::Canceled),
                // 暂停：等待状态变化后重新判断（不忙等、不丢唤醒）
                ControlState::Paused => {}
            }
            if rx.changed().await.is_err() {
                // 所有发送端都被丢弃：不可能再收到恢复/取消信号，
                // 按取消处理，避免传输永久挂起。
                return Err(Error::Canceled);
            }
        }
    }
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
    /// 上传/下载速度（bytes/s，自适应显示）
    #[serde(default)]
    pub speed: u64,
    /// 上次进度上报的字节数与时间戳（用于计算瞬时速度）
    #[serde(default)]
    pub last_done: u64,
    #[serde(default)]
    pub last_speed_time: u64,
    pub error: Option<String>,
    pub created_at: i64,
}

/// 速度采样窗口（毫秒）：窗口太小会让瞬时速度抖动
const SPEED_SAMPLE_WINDOW_MS: u64 = 200;

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
        // 启动恢复：进程被杀/重启后所有 Running 任务的进度已经丢失，
        // 必须降级为 Paused，不能假装它们还在跑。
        tm.mark_interrupted_as_paused();
        tm
    }

    /// 加锁（忽略互斥锁中毒：任务表是缓存数据，中毒不应让整个进程不可用）
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Task>> {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 添加任务，返回任务 ID
    pub fn add(&self, task: Task) -> u64 {
        let id = task.id;
        {
            let mut guard = self.lock();
            guard.push(task);
        }
        self.persist();
        id
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// 获取任务列表副本
    pub fn list(&self) -> Vec<Task> {
        self.lock().clone()
    }

    /// 按 ID 获取任务副本
    pub fn get(&self, id: u64) -> Option<Task> {
        self.lock().iter().find(|t| t.id == id).cloned()
    }

    /// 状态机校验后推进状态；非法转换返回 false 且不修改任务
    ///
    /// 每次成功转换都会立即落盘，避免进程崩溃后状态回退。
    pub fn transition(&self, id: u64, to: TaskStatus) -> bool {
        {
            let mut guard = self.lock();
            let Some(task) = guard.iter_mut().find(|t| t.id == id) else {
                return false;
            };
            if !transition_allowed(task.status, to) {
                return false;
            }
            task.status = to;
        }
        self.persist();
        true
    }

    /// 写入进度/速度（仅对 Pending / Running 任务有效）
    ///
    /// - `bytes_done`：已传输字节数（超过 `total_size` 时按 `total_size` 截断）；
    /// - `total_size`：文件总字节数，0 表示未知；
    /// - `now_ms`：当前毫秒时间戳，用于 200ms 采样窗口计算瞬时速度
    ///   （与 UI 原有算法保持一致）；
    /// - `progress` 写入 `0.0..=100.0` 的百分比。
    pub fn set_progress(&self, id: u64, bytes_done: u64, total_size: u64, now_ms: u64) {
        {
            let mut guard = self.lock();
            let Some(t) = guard.iter_mut().find(|t| t.id == id) else {
                return;
            };
            if !matches!(t.status, TaskStatus::Pending | TaskStatus::Running) {
                return;
            }
            let done = if total_size > 0 {
                bytes_done.min(total_size)
            } else {
                bytes_done
            };
            let delta_ms = now_ms.saturating_sub(t.last_speed_time);
            let delta_bytes = done.saturating_sub(t.last_done);
            if delta_ms >= SPEED_SAMPLE_WINDOW_MS && delta_bytes > 0 {
                t.speed = delta_bytes.saturating_mul(1000) / delta_ms;
                t.last_speed_time = now_ms;
                t.last_done = done;
            } else if t.last_speed_time == 0 {
                t.last_speed_time = now_ms;
                t.last_done = done;
            }
            t.bytes_done = done;
            t.progress = crate::file::percent(done, total_size);
        }
        self.persist();
    }

    /// 启动恢复：把所有 `Running` 任务降级为 `Paused`
    ///
    /// `TaskManager::new` 会在读取 `tasks.json` 之后自动调用一次。
    pub fn mark_interrupted_as_paused(&self) {
        let changed = {
            let mut guard = self.lock();
            let mut changed = false;
            for t in guard.iter_mut() {
                if t.status == TaskStatus::Running {
                    t.status = TaskStatus::Paused;
                    // 进程都重启了，旧速度一定是假的
                    t.speed = 0;
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.persist();
        }
    }

    /// 未完成任务数（Pending / Running / Paused）
    pub fn active_count(&self) -> usize {
        self.lock()
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                )
            })
            .count()
    }

    /// 已完成任务数（Completed / Canceled / Failed）
    pub fn finished_count(&self) -> usize {
        self.lock()
            .iter()
            .filter(|t| t.status.is_terminal())
            .count()
    }

    /// 清除所有终态任务（Completed / Canceled / Failed）
    pub fn clear_finished(&self) {
        {
            let mut guard = self.lock();
            guard.retain(|t| !t.status.is_terminal());
        }
        self.persist();
    }

    /// 更新任务
    pub fn update(&self, id: u64, f: impl FnOnce(&mut Task)) {
        {
            let mut guard = self.lock();
            if let Some(t) = guard.iter_mut().find(|t| t.id == id) {
                f(t);
            }
        }
        self.persist();
    }

    /// 删除任务
    pub fn remove(&self, id: u64) {
        {
            let mut guard = self.lock();
            guard.retain(|t| t.id != id);
        }
        self.persist();
    }

    pub fn clear_completed(&self) {
        {
            let mut guard = self.lock();
            guard.retain(|t| t.status != TaskStatus::Completed && t.status != TaskStatus::Failed);
        }
        self.persist();
    }

    fn persist(&self) {
        if let Some(path) = &self.persist_path {
            let tasks = self.list();
            if let Ok(data) = serde_json::to_string(&tasks) {
                // 直写会在进程被杀/掉电时留下半截 JSON，下次启动整份历史都会被当成
                // 损坏丢弃；先写临时文件再 rename，"要么全旧要么全新"，没有中间态。
                let _ = write_atomic(path, data.as_bytes());
            }
        }
    }

    fn load(&mut self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let Ok(data) = std::fs::read_to_string(path) else {
            return; // 首次运行 / 文件读不出来：按空表启动，不 panic
        };
        match serde_json::from_str::<Vec<Task>>(&data) {
            Ok(tasks) => {
                let max_id = tasks.iter().map(|t| t.id).max().unwrap_or(0);
                self.next_id.store(max_id + 1, Ordering::SeqCst);
                *self.lock() = tasks;
            }
            Err(e) => {
                // 解析失败不再静默丢历史：改名留档，用户和我们事后都还能把内容捞回来。
                // 之后照旧按空表启动——一个坏文件不该让客户端起不来。
                let archived = corrupt_path(path);
                log::warn!(
                    "任务文件解析失败（{e}），已留档为 {}，本次按空任务表启动",
                    archived.display()
                );
                let _ = std::fs::rename(path, &archived);
            }
        }
    }
}

// 原子写与留档名的实现都在 `fsutil`：配置文件和任务表必须共用同一份代码，
// 否则迟早有一边悄悄退回直写。
use crate::fsutil::{sibling_path, write_atomic};

/// 损坏文件的留档名：`tasks.json.corrupt-<unix 秒>`
///
/// 带时间戳是为了能连续留档——同一份坏文件被反复启动时不会互相覆盖。
fn corrupt_path(path: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    sibling_path(path, &format!("corrupt-{secs}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 全部状态，供状态机穷举测试使用
    const ALL_STATUS: [TaskStatus; 6] = [
        TaskStatus::Pending,
        TaskStatus::Running,
        TaskStatus::Paused,
        TaskStatus::Canceled,
        TaskStatus::Completed,
        TaskStatus::Failed,
    ];

    /// 规格中列出的合法边（与实现无关，直接照抄需求）
    fn spec_allows(from: TaskStatus, to: TaskStatus) -> bool {
        use TaskStatus::*;
        matches!(
            (from, to),
            (Pending, Running)
                | (Pending, Paused)
                | (Pending, Canceled)
                | (Running, Paused)
                | (Running, Canceled)
                | (Running, Completed)
                | (Running, Failed)
                | (Paused, Running)
                | (Paused, Canceled)
        )
    }

    fn sample_task(id: u64, status: TaskStatus) -> Task {
        Task {
            id,
            direction: TaskDirection::Download,
            file_name: "t.bin".to_string(),
            file_size: 100,
            local_path: PathBuf::from("t.bin"),
            remote_folder_id: "0".to_string(),
            status,
            progress: 0.0,
            bytes_done: 0,
            speed: 0,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: 0,
        }
    }

    /// 单任务管理器（id 恒为 1）
    fn manager_with(status: TaskStatus) -> TaskManager {
        let tm = TaskManager::new(None);
        let id = tm.next_id();
        tm.add(sample_task(id, status));
        tm
    }

    fn tmp_json(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tianyi-task-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("tasks.json")
    }

    #[test]
    fn test_status_predicates() {
        for s in ALL_STATUS {
            assert_eq!(
                s.is_terminal(),
                matches!(
                    s,
                    TaskStatus::Completed | TaskStatus::Canceled | TaskStatus::Failed
                )
            );
            assert_eq!(
                s.can_pause(),
                matches!(s, TaskStatus::Pending | TaskStatus::Running)
            );
            assert_eq!(s.can_resume(), s == TaskStatus::Paused);
            assert_eq!(
                s.can_cancel(),
                matches!(
                    s,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                )
            );
        }
        assert_eq!(TaskStatus::Completed.as_key(), "completed");
        assert_eq!(TaskStatus::Running.as_key(), "running");
        assert_eq!(TaskStatus::Pending.label_zh(), "等待中");
        assert_eq!(TaskStatus::Failed.label_zh(), "失败");
        // key 必须是 ASCII（UI 绑定/持久化键名不能用中文）
        for s in ALL_STATUS {
            assert!(s.as_key().is_ascii());
            assert!(!s.label_zh().is_empty());
        }
    }

    /// 状态机矩阵：合法边全部接受，非法边全部拒绝且不修改任务
    #[test]
    fn test_transition_matrix() {
        for from in ALL_STATUS {
            for to in ALL_STATUS {
                let tm = manager_with(from);
                let ok = tm.transition(1, to);
                let after = tm.get(1).expect("task exists").status;
                if spec_allows(from, to) {
                    assert!(ok, "{from:?} -> {to:?} 应当被接受");
                    assert_eq!(after, to, "{from:?} -> {to:?} 后状态应为 {to:?}");
                } else {
                    assert!(!ok, "{from:?} -> {to:?} 应当被拒绝");
                    assert_eq!(after, from, "{from:?} -> {to:?} 被拒绝后状态不应改变");
                }
            }
        }
    }

    /// 需求点名的四条非法边
    #[test]
    fn test_transition_named_illegal_edges() {
        for (from, to) in [
            (TaskStatus::Paused, TaskStatus::Completed),
            (TaskStatus::Completed, TaskStatus::Paused),
            (TaskStatus::Canceled, TaskStatus::Running),
            (TaskStatus::Failed, TaskStatus::Running),
        ] {
            let tm = manager_with(from);
            assert!(!tm.transition(1, to), "{from:?} -> {to:?} 必须被拒绝");
            assert_eq!(tm.get(1).expect("task").status, from);
        }
        // 不存在的任务
        let tm = TaskManager::new(None);
        assert!(!tm.transition(42, TaskStatus::Running));
    }

    #[test]
    fn test_set_progress_and_counts() {
        let tm = manager_with(TaskStatus::Running);
        tm.set_progress(1, 1000, 10_000, 1_000);
        let t = tm.get(1).expect("task");
        assert_eq!(t.bytes_done, 1000);
        assert!((t.progress - 10.0).abs() < 0.01);
        // 200ms 采样窗口内不更新速度
        tm.set_progress(1, 1500, 10_000, 1_100);
        assert_eq!(tm.get(1).expect("task").speed, 1000, "窗口内沿用上次速度");
        // 满 200ms 窗口后按增量计算瞬时速度：1000 字节 / 200ms
        tm.set_progress(1, 2000, 10_000, 1_200);
        let t = tm.get(1).expect("task");
        assert_eq!(t.speed, 5000);
        assert_eq!(t.last_done, 2000);
        assert_eq!(t.last_speed_time, 1200);
        // 超过总大小按总量截断，进度不会 > 100
        tm.set_progress(1, 20_000, 10_000, 1_400);
        let t = tm.get(1).expect("task");
        assert_eq!(t.bytes_done, 10_000);
        assert!((t.progress - 100.0).abs() < 0.01);

        // 终态任务不再接受进度
        tm.transition(1, TaskStatus::Completed);
        tm.set_progress(1, 1, 10_000, 2_000);
        assert_eq!(tm.get(1).expect("task").bytes_done, 10_000);

        // 计数
        let tm2 = TaskManager::new(None);
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Paused,
            TaskStatus::Completed,
            TaskStatus::Canceled,
            TaskStatus::Failed,
        ] {
            let id = tm2.next_id();
            tm2.add(sample_task(id, status));
        }
        assert_eq!(tm2.active_count(), 3);
        assert_eq!(tm2.finished_count(), 3);
        tm2.clear_finished();
        assert_eq!(tm2.list().len(), 3);
        assert_eq!(tm2.finished_count(), 0);
    }

    /// `tasks.json` 里处于 Running 的任务，加载后必须变成 Paused
    #[test]
    fn test_mark_interrupted_as_paused_on_load() {
        let path = tmp_json("interrupted");
        std::fs::write(
            &path,
            r#"[
                {"id":1,"direction":"Upload","file_name":"a.bin","file_size":10,"local_path":"a.bin",
                 "remote_folder_id":"1","status":"Running","progress":30.0,"bytes_done":3,
                 "speed":999,"last_done":3,"last_speed_time":100,"error":null,"created_at":5},
                {"id":2,"direction":"Download","file_name":"b.bin","file_size":10,"local_path":"b.bin",
                 "remote_folder_id":"1","status":"Completed","progress":100.0,"bytes_done":10,
                 "speed":0,"last_done":10,"last_speed_time":100,"error":null,"created_at":5}
            ]"#,
        )
        .expect("write tasks.json");

        let tm = TaskManager::new(Some(path.clone()));
        let running = tm.get(1).expect("task 1");
        assert_eq!(
            running.status,
            TaskStatus::Paused,
            "Running 必须降级为 Paused"
        );
        assert_eq!(running.speed, 0, "恢复后的任务速度必须归零");
        assert_eq!(tm.get(2).expect("task 2").status, TaskStatus::Completed);
        assert_eq!(tm.active_count(), 1);
        assert_eq!(tm.finished_count(), 1);
        // id 计数器跟随已有最大 id
        assert_eq!(tm.next_id(), 3);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    /// 持久化兼容：旧版本（当前仓库版本）写出的 JSON 数组必须能加载
    #[test]
    fn test_load_old_format_tasks_json() {
        let path = tmp_json("oldfmt");
        // 字段与当前仓库版本写出的完全一致（含 speed/last_done/last_speed_time）
        let old = r#"[
            {"id":7,"direction":"Download","file_name":"old.bin","file_size":1048576,
             "local_path":"C:\\Users\\Public\\old.bin","remote_folder_id":"-11",
             "status":"Paused","progress":42.5,"bytes_done":445644,"speed":123456,
             "last_done":445644,"last_speed_time":1717000000000,"error":null,
             "created_at":1717000000}
        ]"#;
        std::fs::write(&path, old).expect("write old tasks.json");
        let tm = TaskManager::new(Some(path.clone()));
        let t = tm.get(7).expect("旧格式任务应能加载");
        assert_eq!(t.id, 7);
        assert_eq!(t.direction, TaskDirection::Download);
        assert_eq!(t.local_path, PathBuf::from("C:\\Users\\Public\\old.bin"));
        assert_eq!(t.status, TaskStatus::Paused);
        assert!((t.progress - 42.5).abs() < 0.01);
        assert_eq!(t.speed, 123456);
        assert_eq!(t.last_speed_time, 1717000000000);
        assert_eq!(t.created_at, 1717000000);

        // 更老的格式（缺少后来才加的速度字段）也必须能加载
        let older = r#"[
            {"id":9,"direction":"Upload","file_name":"x.bin","file_size":1,"local_path":"x.bin",
             "remote_folder_id":"0","status":"Pending","progress":0.0,"bytes_done":0,
             "error":null,"created_at":0}
        ]"#;
        std::fs::write(&path, older).expect("write older tasks.json");
        let tm = TaskManager::new(Some(path.clone()));
        let t = tm.get(9).expect("更老的格式应能加载");
        assert_eq!(t.speed, 0);
        assert_eq!(t.last_done, 0);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    /// 当前版本写出的 JSON 能原样再读回来（往返兼容）
    #[test]
    fn test_persist_roundtrip() {
        let path = tmp_json("roundtrip");
        let tm = TaskManager::new(Some(path.clone()));
        let id = tm.next_id();
        let mut task = sample_task(id, TaskStatus::Pending);
        task.file_name = "往返.bin".to_string();
        tm.add(task);
        assert!(tm.transition(id, TaskStatus::Running));

        let reloaded = TaskManager::new(Some(path.clone()));
        let t = reloaded.get(id).expect("重新加载后任务存在");
        assert_eq!(t.file_name, "往返.bin");
        assert_eq!(t.status, TaskStatus::Paused);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[tokio::test]
    async fn test_control_pause_resume_semantics() {
        let c = TransferControl::new();
        assert_eq!(c.state(), ControlState::Running);
        assert!(c.pause(), "Running -> Paused 应返回 true");
        assert!(!c.pause(), "重复暂停是无副作用的空操作");
        assert_eq!(c.state(), ControlState::Paused);
        assert!(c.resume(), "Paused -> Running 应返回 true");
        assert!(!c.resume(), "未暂停时 resume 返回 false");
        assert_eq!(c.state(), ControlState::Running);
        // Default == new()
        assert_eq!(TransferControl::default().state(), ControlState::Running);
        // Running 下检查点立即通过
        assert!(c.checkpoint().await.is_ok());
    }

    #[tokio::test]
    async fn test_control_cancel_is_terminal() {
        let c = TransferControl::new();
        assert!(c.cancel());
        assert!(!c.cancel(), "取消是幂等的：第二次调用不再改变状态");
        assert!(c.is_canceled());
        assert!(!c.pause(), "取消后不能再暂停");
        assert!(!c.resume(), "取消后 resume 不能复活任务");
        assert_eq!(c.state(), ControlState::Canceled);
        let err = c.checkpoint().await.expect_err("取消后检查点必须失败");
        assert!(matches!(err, Error::Canceled));
        assert!(err.is_interrupted());
        assert_eq!(err.to_string(), "传输已取消");
    }

    /// 暂停后同一个 checkpoint future 挂起，resume 后完成 Ok
    #[tokio::test]
    async fn test_control_pause_parks_same_future() {
        let c = TransferControl::new();
        assert!(c.pause());
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move { c.checkpoint().await })
        };
        let mut waiter = waiter;
        // 给它时间真正进入挂起状态
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "暂停期间 checkpoint 必须保持 pending（不忙等、不返回）"
        );
        assert!(c.resume());
        let joined = tokio::time::timeout(Duration::from_millis(1000), waiter)
            .await
            .expect("resume 后同一个 future 应被唤醒");
        assert!(joined.expect("join ok").is_ok(), "恢复后检查点返回 Ok");
    }

    /// 挂起中的 checkpoint 在取消时立刻返回 Err(Canceled)
    #[tokio::test]
    async fn test_control_park_wakes_on_cancel() {
        let c = TransferControl::new();
        assert!(c.pause());
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move { c.checkpoint().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(c.cancel());
        let joined = tokio::time::timeout(Duration::from_millis(1000), waiter)
            .await
            .expect("取消应唤醒挂起的检查点");
        let err = joined.expect("join ok").expect_err("取消后应返回错误");
        assert!(matches!(err, Error::Canceled));
    }

    /// 多个并发检查点共享同一个控制器
    #[tokio::test]
    async fn test_control_multiple_waiters() {
        let c = TransferControl::new();
        assert!(c.pause());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = c.clone();
            handles.push(tokio::spawn(async move { c.checkpoint().await.is_ok() }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(c.resume());
        for h in handles {
            assert!(h.await.expect("join ok"), "所有等待者都应在恢复后继续");
        }
    }

    // ---------------- 持久化：原子写 + 损坏留档 ----------------

    /// 落盘后重新加载：内容一致，且不留下临时文件
    #[test]
    fn persist_is_atomic_and_survives_reload() {
        let path = tmp_json("atomic-roundtrip");
        {
            let tm = TaskManager::new(Some(path.clone()));
            tm.add(sample_task(1, TaskStatus::Pending));
            tm.add(sample_task(2, TaskStatus::Paused));
        }
        let reloaded = TaskManager::new(Some(path.clone()));
        let names: Vec<String> = reloaded
            .list()
            .iter()
            .map(|t| t.file_name.clone())
            .collect();
        assert_eq!(names, vec!["t.bin", "t.bin"], "两条任务都应读回来");
        assert_eq!(reloaded.list().len(), 2);
        assert!(
            !path.with_file_name("tasks.json.tmp").exists(),
            "rename 之后临时文件不应残留"
        );
    }

    /// 损坏的 tasks.json 不 panic、不留死档：改名留档后按空表启动
    #[test]
    fn corrupt_tasks_json_is_archived_instead_of_dropped() {
        let path = tmp_json("corrupt-archive");
        std::fs::write(&path, b"{ not json at all").expect("写入损坏文件");
        let tm = TaskManager::new(Some(path.clone()));
        assert!(tm.list().is_empty(), "损坏后按空任务表启动");
        assert!(!path.exists(), "坏文件必须被改名，不能留在原位反复触发");
        let archived: Vec<String> = std::fs::read_dir(path.parent().expect("父目录"))
            .expect("读取临时目录")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("tasks.json.corrupt-"))
            .collect();
        assert_eq!(archived.len(), 1, "应留下恰好一份留档，实际：{archived:?}");
    }
}
