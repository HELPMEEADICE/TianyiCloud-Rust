//! 传输任务：入队与执行、暂停 / 继续 / 取消 / 重试 / 移除、面板刷新、关窗收尾。
use super::*;
use crate::app::TransferEntry;
use slint::SharedString;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tianyi_core::config::AppConfig;
use tianyi_core::file;
use tianyi_core::models::FileObject;
use tianyi_core::task::{Task, TaskDirection, TaskStatus, TransferControl};
use tianyi_core::transfer::{self, DownloadOptions, DownloadOutcome, UploadOptions, UploadOutcome};
impl Controller {
    pub(super) fn toggle_transfer_panel(self: &Arc<Self>) {
        let ui = self.ui.clone();
        if let Some(win) = ui.upgrade() {
            let open = !win.get_transfer_panel_open();
            win.set_transfer_panel_open(open);
            if open {
                self.refresh_transfers();
            }
        }
    }

    // ==================================================================
    // 上传
    // ==================================================================
    pub(super) fn upload_files(self: &Arc<Self>) {
        if self.client().is_none() {
            self.notice("error", t("toast.login_required"));
            return;
        }
        let Some(paths) = rfd::FileDialog::new()
            .set_title(t("dialog.choose_upload_files_title"))
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let folder_id = lock_or_recover(&self.folder_id).clone();
        self.upload_paths(paths, &folder_id);
    }

    pub(super) fn create_folder(self: &Arc<Self>) {
        let Some(client) = self.client() else { return };
        let folder_id = lock_or_recover(&self.folder_id).clone();
        let name = t("state.default_folder_name");
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.create_folder(&folder_id, name).await {
                Ok(_) => {
                    this.notice("success", t("toast.folder_created"));
                    this.refresh_files();
                }
                Err(e) => this.notice("error", tf("error.create_folder_failed", &[("e", &e)])),
            }
        });
    }

    /// 批量上传本地路径到指定云端目录
    pub fn upload_paths(self: &Arc<Self>, paths: Vec<PathBuf>, folder_id: &str) {
        if self.client().is_none() {
            self.notice("error", t("toast.login_required"));
            return;
        }
        // 去重：同一批里重复出现的路径只上传一次（避免拖拽多文件时重复入队）。
        //
        // 这里刻意不再用 `std::fs::canonicalize`：它对每个路径都要摸一次文件系统
        // （逐级 stat + 解析符号链接），而这段代码跑在 UI 线程上，拖入几百个文件时
        // 会直接掉帧。改成纯字符串规整（见 `dedupe_key`）：相对路径按当前目录补全、
        // 去掉 `.`、折叠 `..`、Windows 下大小写不敏感。
        // 取舍：不解析符号链接，所以"同一个文件的两个软链"会被当成两个任务；拖拽
        // 场景里几乎不出现，换来的是整批路径只发生一次 `current_dir()` 系统调用。
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        for p in paths {
            if seen.insert(dedupe_key(&p, &base)) {
                unique.push(p);
            }
        }
        if unique.is_empty() {
            return;
        }
        let count = unique.len();
        for path in unique {
            let task_id = self.tasks.next_id();
            let is_dir = path.is_dir();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());
            // 单文件用 metadata（O(1)）；目录的递归统计搬到 blocking 线程池去做，
            // 入队时先记 0，算完再回填——绝不在 UI 线程上跑 `read_dir` 递归。
            let size = if is_dir {
                0
            } else {
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            };
            self.tasks.add(Task {
                id: task_id,
                direction: TaskDirection::Upload,
                file_name: name,
                file_size: size,
                local_path: path.clone(),
                remote_folder_id: folder_id.to_string(),
                // 先 Pending：拿到任务级并发许可、真正开始传输时才推进到 Running
                status: TaskStatus::Pending,
                progress: 0.0,
                bytes_done: 0,
                speed: 0,
                last_done: 0,
                last_speed_time: 0,
                error: None,
                created_at: now_secs(),
            });
            if is_dir {
                self.spawn_dir_size(task_id, path);
            }
            self.spawn_transfer(task_id);
        }
        self.set_ui_state(|ui| ui.set_transfer_panel_open(true));
        self.refresh_transfers();
        self.notice("info", tf("toast.uploads_added", &[("count", &count)]));
    }

    /// 目录大小统计：扔进 tokio 的 blocking 线程池，算完回填并置脏
    ///
    /// 用 `tokio::task::spawn_blocking` 而不起裸线程：复用 runtime 的阻塞线程池，
    /// 进程退出时能被统一回收，也不会因为连续拖入几十个目录就炸出几十个线程。
    fn spawn_dir_size(self: &Arc<Self>, task_id: u64, path: PathBuf) {
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match tokio::task::spawn_blocking(move || dir_size(&path)).await {
                Ok(size) => this.set_task_size(task_id, size),
                Err(e) => log::warn!("目录大小统计失败（任务 {task_id}）：{e}"),
            }
        });
    }

    /// 只更新任务大小
    ///
    /// 直接复用 `TaskManager::update`（已有 API，自带落盘），既不新增 `task.rs` 的
    /// 接口，也不需要维护"大小覆盖层"——任务表仍然是唯一事实来源。
    fn set_task_size(&self, task_id: u64, size: u64) {
        self.tasks.update(task_id, |t| t.file_size = size);
        self.dirty.store(true, Ordering::SeqCst);
    }

    pub(super) fn handle_files_dropped(self: &Arc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.client().is_none() {
            self.notice("error", t("toast.login_required_for_drop"));
            return;
        }
        let folder_id = lock_or_recover(&self.folder_id).clone();
        self.upload_paths(paths, &folder_id);
    }

    // ==================================================================
    // 传输任务
    // ==================================================================
    /// 启动（或恢复）一个任务的实际传输
    ///
    /// 真正开始传输前先抢一个任务级并发许可（`MAX_CONCURRENT_TRANSFERS`）；抢不到就
    /// 停在 `Pending` 状态排队（UI 上能看到"等待中"），而不是所有任务一起 `Running`。
    /// 许可由 `OwnedSemaphorePermit` 持有，成功/失败/取消/panic 都是 drop，出口唯一。
    ///
    /// 注意这是**任务级**并发上限，与 `transfer.rs` 内部按 `thread_count` 做的分片级
    /// 并发是两层控制，互不覆盖。
    pub(super) fn spawn_transfer(self: &Arc<Self>, task_id: u64) {
        let Some(task) = self.tasks.get(task_id) else {
            return;
        };
        if lock_or_recover(&self.controls).contains_key(&task_id) {
            return; // 已有存活的控制句柄，说明任务还在跑（可能是暂停挂起或正在排队）
        }
        let control = TransferControl::new();
        lock_or_recover(&self.controls).insert(task_id, control.clone());
        let permits = Arc::clone(&self.transfer_permits);
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            // acquire_owned：许可的生命周期与这个任务绑定，任务结束（future 被 drop）
            // 时自动归还，不需要手工计数
            let permit = match Arc::clone(&permits).acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // 信号量随 runtime 销毁，正常路径走不到；兜底避免任务永远 Pending
                    lock_or_recover(&this.controls).remove(&task_id);
                    this.finish_task(
                        task_id,
                        TaskStatus::Failed,
                        Some(t("error.scheduler_closed").to_string()),
                    );
                    return;
                }
            };
            let status = this.tasks.get(task_id).map(|t| t.status);
            if control.is_canceled()
                || !matches!(
                    status,
                    Some(TaskStatus::Pending) | Some(TaskStatus::Running)
                )
            {
                if status == Some(TaskStatus::Paused) {
                    // 排队期间被暂停：摘掉控制句柄，用户点"继续"会走"会话已丢失"的分支
                    // 重新排队，而不是对着一个从未启动的任务假 Running
                    lock_or_recover(&this.controls).remove(&task_id);
                }
                drop(permit);
                return;
            }
            let _ = this.tasks.transition(task_id, TaskStatus::Running);
            this.dirty.store(true, Ordering::SeqCst);
            this.run_transfer(task, control).await;
            // 传输结束（完成/暂停/取消/失败）后把名额让给排队中的任务
            drop(permit);
            this.dirty.store(true, Ordering::SeqCst);
        });
    }

    pub(super) async fn run_transfer(self: &Arc<Self>, task: Task, control: TransferControl) {
        let task_id = task.id;
        let Some(client) = self.client() else {
            self.finish_task(
                task_id,
                TaskStatus::Failed,
                Some(t("state.not_logged_in").to_string()),
            );
            lock_or_recover(&self.controls).remove(&task_id);
            return;
        };
        let progress = self.progress_callback(task_id, task.file_size);
        let cfg = self
            .store
            .load_config()
            .unwrap_or_else(|_| AppConfig::default_config());

        // 限速桶只在这里取一次，下载与上传两条分支共用同一个 `Arc`。
        //
        // 绝不能各自写 `bucket_for(cfg.speed_limit)`：桶的额度是「每秒 N 字节」而不是
        // 「总共 N 字节」，每个任务各造一个桶就等于把额度复制了一份——两个任务并行时
        // 用户设的 1 MB/s 实际能跑到 2 MB/s。取桶走 `Controller::rate_limiter`，
        // 同一个限速值在整个进程里只对应一个桶（见 `controller/ratelimit.rs`）。
        let rate = self.rate_limiter(cfg.speed_limit);

        let outcome: Outcome = match task.direction {
            TaskDirection::Download => {
                let file = FileObject {
                    id: task.remote_folder_id.clone(),
                    name: task.file_name.clone(),
                    size: task.file_size as i64,
                    ..Default::default()
                };
                let opts = DownloadOptions {
                    thread_count: cfg.download_thread.max(1),
                    resume: true,
                    chunk_size: 4 * 1024 * 1024,
                    rate: rate.clone(),
                };
                match transfer::download(
                    &client,
                    &file,
                    &task.local_path,
                    &opts,
                    progress,
                    &control,
                )
                .await
                {
                    Ok(DownloadOutcome::Completed) => Outcome::Done,
                    Ok(DownloadOutcome::Paused) => Outcome::Paused,
                    Ok(DownloadOutcome::Canceled) => Outcome::Canceled,
                    Err(e) if e.is_interrupted() => Outcome::from_error(&e),
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            }
            TaskDirection::Upload => {
                let opts = UploadOptions {
                    rapid_upload: cfg.rapid_upload,
                    thread_count: cfg.upload_thread.max(1),
                    generate_torrent: cfg.generate_torrent,
                    // 与下载分支共用同一个桶（上传/下载不会同时发生，但"同一个任务表里的
                    // 两个任务"会——那才是这条共享缓存的真正理由）
                    rate,
                    ..Default::default()
                };
                if task.local_path.is_dir() {
                    match transfer::upload_folder_recursive(
                        &client,
                        &task.remote_folder_id,
                        &task.local_path,
                        &opts,
                        progress,
                        &control,
                    )
                    .await
                    {
                        Ok(_) => Outcome::Done,
                        Err(e) if e.is_interrupted() => Outcome::from_error(&e),
                        Err(e) => Outcome::Failed(e.to_string()),
                    }
                } else {
                    match transfer::upload(
                        &client,
                        &task.remote_folder_id,
                        &task.local_path,
                        &opts,
                        progress,
                        &control,
                    )
                    .await
                    {
                        Ok(UploadOutcome::Completed(_)) => Outcome::Done,
                        Ok(UploadOutcome::Paused) => Outcome::Paused,
                        Ok(UploadOutcome::Canceled) => Outcome::Canceled,
                        Err(e) if e.is_interrupted() => Outcome::from_error(&e),
                        Err(e) => Outcome::Failed(e.to_string()),
                    }
                }
            }
        };

        let shutting_down = self.shutting_down.load(Ordering::SeqCst);
        match outcome {
            Outcome::Done => {
                if close_safe_status(shutting_down, TaskStatus::Completed).is_none() {
                    return; // 关窗中：写「已完成」会把归一化出来的「已暂停」顶掉
                }
                self.tasks.update(task_id, |t| {
                    t.status = TaskStatus::Completed;
                    t.progress = 100.0;
                    t.bytes_done = t.file_size;
                    t.speed = 0;
                    t.error = None;
                });
                lock_or_recover(&self.controls).remove(&task_id);
                lock_or_recover(&self.rate_samples).remove(&task_id);
                let label = if task.direction == TaskDirection::Upload {
                    t("toast.upload_done")
                } else {
                    t("toast.download_done")
                };
                self.notice(
                    "success",
                    tf(
                        "toast.transfer_done",
                        &[("label", &label), ("name", &task.file_name)],
                    ),
                );
                if task.direction == TaskDirection::Upload {
                    self.refresh_files();
                }
            }
            Outcome::Paused => {
                self.tasks.transition(task_id, TaskStatus::Paused);
                self.tasks.update(task_id, |t| t.speed = 0);
            }
            Outcome::Canceled => {
                if close_safe_status(shutting_down, TaskStatus::Canceled).is_none() {
                    return; // 同上：关窗的取消不算用户取消，状态由 begin_close 统一负责
                }
                self.tasks.transition(task_id, TaskStatus::Canceled);
                lock_or_recover(&self.controls).remove(&task_id);
                lock_or_recover(&self.rate_samples).remove(&task_id);
            }
            Outcome::Failed(msg) => {
                self.finish_task(task_id, TaskStatus::Failed, Some(msg));
                lock_or_recover(&self.controls).remove(&task_id);
                lock_or_recover(&self.rate_samples).remove(&task_id);
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn finish_task(&self, task_id: u64, status: TaskStatus, err: Option<String>) {
        let Some(status) = close_safe_status(self.shutting_down.load(Ordering::SeqCst), status)
        else {
            return; // 关窗中：失败也不回写，任务保持 begin_close 归一化后的「已暂停」
        };
        self.tasks.transition(task_id, status);
        self.tasks.update(task_id, |t| {
            t.status = status;
            t.error = err.clone();
            if status == TaskStatus::Failed {
                t.speed = 0;
            }
        });
        if let Some(e) = err {
            let name = self
                .tasks
                .get(task_id)
                .map(|t| t.file_name)
                .unwrap_or_default();
            self.notice(
                "error",
                tf("toast.transfer_failed", &[("name", &name), ("e", &e)]),
            );
        }
    }

    /// 进度回调：只写任务表 + 置脏，UI 刷新交给定时器节流
    fn progress_callback(&self, task_id: u64, total: u64) -> transfer::ProgressCallback {
        let tasks = Arc::clone(&self.tasks);
        let dirty = Arc::clone(&self.dirty);
        let samples = Arc::clone(&self.rate_samples);
        Arc::new(move |done: u64| {
            let now = now_millis();
            let mut guard = lock_or_recover(&samples);
            let last_ms = guard.get(&task_id).map(|(_, ms)| *ms).unwrap_or(0);
            // 200ms 采样窗口：既算瞬时速度，也避免高频写盘与 UI 抖动
            if now.saturating_sub(last_ms) < TICK_MS {
                return;
            }
            guard.insert(task_id, (done, now));
            drop(guard);
            if tasks.get(task_id).map(|t| t.status) == Some(TaskStatus::Running) {
                tasks.set_progress(task_id, done, total, now);
                dirty.store(true, Ordering::SeqCst);
            }
        })
    }

    pub(super) fn pause_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if !task.status.can_pause() {
            return;
        }
        let control = lock_or_recover(&self.controls).get(&id).cloned();
        match control {
            Some(ctl) => {
                // 真正的协作式暂停：传输任务在检查点挂起，连接与会话保留
                ctl.pause();
                self.tasks.transition(id, TaskStatus::Paused);
                self.tasks.update(id, |t| t.speed = 0);
                self.notice("info", tf("toast.paused", &[("name", &task.file_name)]));
            }
            None => {
                // 没有控制句柄（例如上次进程遗留的任务）：标记为暂停即可
                self.tasks.transition(id, TaskStatus::Paused);
                self.notice("info", tf("toast.paused", &[("name", &task.file_name)]));
            }
        }
        self.refresh_transfers();
    }

    pub(super) fn resume_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if !task.status.can_resume() {
            return;
        }
        let control = lock_or_recover(&self.controls).get(&id).cloned();
        match control {
            Some(ctl) => {
                // 会话仍在：唤醒挂起的传输
                self.tasks.transition(id, TaskStatus::Running);
                ctl.resume();
                self.notice("info", tf("toast.resumed", &[("name", &task.file_name)]));
                self.refresh_transfers();
            }
            None => {
                // 会话已丢失（重启/失败后）：从断点重新开始
                self.tasks.update(id, |t| {
                    t.error = None;
                    t.status = TaskStatus::Pending;
                });
                self.spawn_transfer(id);
                self.notice("info", tf("toast.restarted", &[("name", &task.file_name)]));
                self.refresh_transfers();
            }
        }
    }

    pub(super) fn cancel_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if !task.status.can_cancel() {
            // 已完成/失败的“取消”等价于从列表移除
            if task.status.is_terminal() {
                self.remove_task(id);
            }
            return;
        }
        let control = lock_or_recover(&self.controls).get(&id).cloned();
        // 关窗中用户已经没有窗口可点：这条分支只是兜底，同样不许抢先写终态
        let cancel_status = close_safe_status(
            self.shutting_down.load(Ordering::SeqCst),
            TaskStatus::Canceled,
        );
        if let Some(ctl) = control {
            // 取消 → 检查点返回 Canceled → 传输任务自行收尾
            ctl.cancel();
            if let Some(status) = cancel_status {
                self.tasks.transition(id, status);
                self.tasks.update(id, |t| t.speed = 0);
            }
            lock_or_recover(&self.controls).remove(&id);
        } else if let Some(status) = cancel_status {
            self.tasks.transition(id, status);
        }
        lock_or_recover(&self.rate_samples).remove(&id);
        self.notice("info", tf("toast.canceled", &[("name", &task.file_name)]));
        self.refresh_transfers();
    }

    pub(super) fn retry_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        self.tasks.update(id, |t| {
            t.status = TaskStatus::Pending;
            t.progress = 0.0;
            t.bytes_done = 0;
            t.speed = 0;
            t.error = None;
            t.last_done = 0;
            t.last_speed_time = 0;
        });
        self.spawn_transfer(id);
        self.notice("info", tf("toast.retried", &[("name", &task.file_name)]));
        self.refresh_transfers();
    }

    pub(super) fn remove_task(self: &Arc<Self>, id: u64) {
        if let Some(ctl) = lock_or_recover(&self.controls).remove(&id) {
            ctl.cancel();
        }
        lock_or_recover(&self.rate_samples).remove(&id);
        self.tasks.remove(id);
        self.refresh_transfers();
    }

    pub(super) fn reveal_task(self: &Arc<Self>, id: u64) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        let target = if task.direction == TaskDirection::Download {
            task.local_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| task.local_path.clone())
        } else {
            self.store
                .load_config()
                .map(|c| c.download_dir)
                .unwrap_or_else(|_| PathBuf::from("."))
        };
        if let Err(e) = file::open_with_system(&target) {
            self.notice("error", tf("error.open_dir_failed", &[("e", &e)]));
        }
    }

    pub(super) fn clear_finished_tasks(self: &Arc<Self>) {
        let before = self.tasks.list().len();
        self.tasks.clear_finished();
        let removed = before - self.tasks.list().len();
        self.refresh_transfers();
        if removed == 0 {
            self.notice("info", t("toast.no_finished_tasks"));
        } else {
            self.notice("success", tf("toast.cleaned_tasks", &[("count", &removed)]));
        }
    }

    /// UI 定时器调用：把脏数据刷到界面（200ms 节流）
    ///
    /// `dirty` 的语义是"可能变了"：先 `swap(false)` 原子取走，再看快照决定推不推。
    /// 取走不会丢信息——真正的判据是 `refresh_transfers` 里的快照比较，脏标记只负责
    /// 省掉一次全量 `tasks.list()`；反过来，即使这一跳没推，下一次置脏也会重新评估。
    pub fn tick(self: &Arc<Self>) {
        if self.dirty.swap(false, Ordering::SeqCst) {
            self.refresh_transfers();
        }
    }

    /// 推送传输列表（增量）
    ///
    /// 200ms 一跳、任务上百时，每次都全量 `list()` + 排序 + 构造两个模型是白烧 CPU，
    /// 而且 Slint 侧还会跟着重排。这里先算轻量快照（id / 状态 / 进度度量），只有
    /// 状态变化、任务集合变化、或进度跨过 `PROGRESS_PUBLISH_PERMILLE` 才真正重建。
    /// 筛选器切换这类快照里看不出来的变化由 `force_publish` 强制推一次。
    pub(super) fn refresh_transfers(self: &Arc<Self>) {
        let filter = self
            .ui
            .upgrade()
            .map(|ui| ui.get_transfer_filter())
            .unwrap_or(0);
        let mut tasks = self.tasks.list();
        tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        let snapshot = transfer_snapshot(&tasks);
        let force = self.force_publish.swap(false, Ordering::SeqCst);
        {
            let mut prev = lock_or_recover(&self.last_snapshot);
            if !force && !needs_publish(&prev, &snapshot) {
                return;
            }
            *prev = snapshot;
        }
        let active: Vec<TransferEntry> = tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                )
            })
            .take(MAX_TRANSFER_ROWS)
            .map(to_transfer_entry)
            .collect();
        let rows: Vec<TransferEntry> = tasks
            .iter()
            .filter(|t| match filter {
                1 => matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                ),
                2 => t.status == TaskStatus::Completed,
                3 => matches!(t.status, TaskStatus::Failed | TaskStatus::Canceled),
                _ => true,
            })
            .take(MAX_TRANSFER_ROWS)
            .map(to_transfer_entry)
            .collect();
        let active_count = active.len() as i32;
        let finished = self.tasks.finished_count() as i32;
        self.set_ui_state(move |ui| {
            ui.set_transfer_list(transfers_model(rows));
            ui.set_active_transfer_list(transfers_model(active));
            ui.set_active_transfers(active_count);
            ui.set_finished_transfers(finished);
        });
    }

    // ==================================================================
    // 关窗
    // ==================================================================
    /// 关窗收尾：取消在途传输 + 任务状态归一化 + 落盘
    ///
    /// 关窗语义选"已暂停"而不是"已取消"，理由有两条：
    /// * `Canceled` 是终态（`TaskStatus::is_terminal`），既不再出现在"进行中"筛选里，
    ///   也不能 `resume`；用户只是关了窗口，不该因此丧失断点续传能力（下载默认
    ///   `resume: true`，上传也有本地文件可重来）；
    /// * 下次启动 `TaskManager::new` 本来就会把遗留的 `Running` 降级成 `Paused`
    ///   （`mark_interrupted_as_paused`）——这里直接写成 `Paused`，让"退出时看到的状态"
    ///   与"启动后看到的状态"一致，不会出现一闪而过的状态跳变。
    ///
    /// 控制句柄仍然调 `cancel()`：`pause()` 只是把会话挂起，进程退出前它还会占着连接与
    /// socket；关窗要的是"立刻停手"，状态由下面的归一化统一负责。
    pub(super) fn begin_close(self: &Arc<Self>) {
        // 必须先置位再做归一化：此后任何后台传输的收尾写回都只能留在「已暂停」，
        // 否则最后几毫秒走到 Canceled 分支的任务会把归一化结果覆盖掉。
        self.shutting_down.store(true, Ordering::SeqCst);
        {
            let controls = lock_or_recover(&self.controls);
            for ctl in controls.values() {
                ctl.cancel();
            }
        }
        let transitions = close_transitions(&self.tasks.list());
        for (id, status) in &transitions {
            // 状态机拒绝（理论上不会）时兜底强写，绝不留 Running
            if !self.tasks.transition(*id, *status) {
                self.tasks.update(*id, |t| {
                    t.status = *status;
                    t.speed = 0;
                });
            }
        }
        // 最后一次"等价于重启恢复"的归并：兜住上面那一瞬间才转成 Running 的任务。
        self.tasks.mark_interrupted_as_paused();
        // 关窗路径**必须**显式 flush，因为它是唯一一个"不保证由状态转换顺带落盘"的出口：
        // * `TaskManager` 的进度落盘是**去抖**的（每秒最多一次），最后一秒的进度只在内存里；
        // * 关窗时很可能一次转换都不会发生（任务早已全是 Paused / 终态，
        //   `mark_interrupted_as_paused` 也改不动任何东西），那这份进度就没人写下去了；
        // * 生产路径上进度还是丢给 `spawn_blocking` 写的，runtime 关停会直接把它丢掉。
        // `flush_progress` 故意不按 `pending` 门控（理由见 core 侧同名注释）：关窗只发生
        // 一次，多付一次 fsync 远小于丢进度。
        self.tasks.flush_progress();
        log::info!("关窗：{} 个任务已归一化为已暂停", transitions.len());
    }
}
/// 关窗时任务应落成的状态；`None` 表示无需改动
///
/// `Pending` 也要改成 `Paused`：排队中的任务在进程退出后没有任何东西会去唤醒它，
/// 留着 `Pending` 下次启动就是一个不会动、也不会被 `mark_interrupted_as_paused`
/// 处理的幽灵任务。
pub(super) fn close_status_for(status: TaskStatus) -> Option<TaskStatus> {
    match status {
        TaskStatus::Pending | TaskStatus::Running => Some(TaskStatus::Paused),
        _ => None,
    }
}

/// 关窗归一化计划：(任务 id, 目标状态)
pub(super) fn close_transitions(tasks: &[Task]) -> Vec<(u64, TaskStatus)> {
    tasks
        .iter()
        .filter_map(|t| close_status_for(t.status).map(|s| (t.id, s)))
        .collect()
}

/// 关窗期间允许写回的目标状态；`None` 表示跳过写回
///
/// `begin_close` 把在跑任务归一化成 `Paused` 之后，后台传输任务往往还要跑几毫秒才走到
/// 收尾分支——它手里的 `Canceled` / `Completed` / `Failed` 会把「已暂停」覆盖掉，进程
/// 退出后用户看到的就是「已取消」，断点续传能力白丢。所以关窗期间只认 `Paused`。
///
/// 只拦后台收尾这一侧：用户点暂停/取消走的是 UI 线程，那时窗口还在，`shutting_down`
/// 仍是 false，行为与改动前完全一致。
pub(super) fn close_safe_status(shutting_down: bool, target: TaskStatus) -> Option<TaskStatus> {
    if !shutting_down {
        return Some(target);
    }
    (target == TaskStatus::Paused).then_some(TaskStatus::Paused)
}

pub(super) enum Outcome {
    Done,
    Paused,
    Canceled,
    Failed(String),
}

impl Outcome {
    pub(super) fn from_error(e: &tianyi_core::error::Error) -> Self {
        match e {
            tianyi_core::error::Error::Paused => Outcome::Paused,
            tianyi_core::error::Error::Canceled => Outcome::Canceled,
            other => Outcome::Failed(other.to_string()),
        }
    }
}

pub(super) fn to_transfer_entry(t: &Task) -> TransferEntry {
    let direction = if t.direction == TaskDirection::Upload {
        "upload"
    } else {
        "download"
    };
    let remaining = t.file_size.saturating_sub(t.bytes_done);
    let eta = if t.status == TaskStatus::Running && t.speed > 0 {
        crate::i18n::format_eta(remaining, t.speed)
    } else {
        String::new()
    };
    let detail = if t.file_size == 0 {
        file::format_size(t.bytes_done)
    } else {
        format!(
            "{} / {}{}",
            file::format_size(t.bytes_done),
            file::format_size(t.file_size),
            if eta.is_empty() {
                String::new()
            } else {
                format!(" · {eta}")
            }
        )
    };
    TransferEntry {
        id: t.id as i32,
        name: t.file_name.clone().into(),
        direction: direction.into(),
        size_text: file::format_size(t.file_size).into(),
        progress: file::percent(t.bytes_done, t.file_size),
        status: t.status.as_key().into(),
        status_text: crate::i18n::label_status(t.status).into(),
        speed: if t.status == TaskStatus::Running && t.speed > 0 {
            file::format_speed(t.speed).into()
        } else {
            SharedString::default()
        },
        detail: detail.into(),
        error: t.error.clone().unwrap_or_default().into(),
    }
}

fn transfers_model(entries: Vec<TransferEntry>) -> slint::ModelRc<TransferEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

/// 递归统计本地目录大小
pub(super) fn dir_size(root: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => total += dir_size(&p),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// 路径去重键：完全不做文件系统访问的规整字符串
///
/// 规则：相对路径按 `base` 补全 → 丢掉 `.`、折叠 `..`（绝不越过根/盘符）→
/// Windows 下统一小写（NTFS 默认大小写不敏感）。不解析符号链接也不解析短名，
/// 这是 `upload_paths` 里显式接受的取舍：宁可漏掉极端去重，也不在 UI 线程上访问磁盘。
pub(super) fn dedupe_key(path: &std::path::Path, base: &std::path::Path) -> String {
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut parts: Vec<String> = Vec::new();
    for comp in absolute.components() {
        match comp {
            // Windows 盘符/UNC 前缀：作成一个不可折叠的段
            Component::Prefix(p) => parts.push(format!("<{}>", p.as_os_str().to_string_lossy())),
            // 根用空串占位，`..` 折叠时靠它挡住"吃掉根"
            Component::RootDir => parts.push(String::new()),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().map(|s| !s.is_empty()).unwrap_or(false) {
                    parts.pop();
                }
            }
            Component::Normal(n) => parts.push(n.to_string_lossy().to_string()),
        }
    }
    let joined = parts.join("/");
    if cfg!(windows) {
        joined.to_lowercase()
    } else {
        joined
    }
}

/// 传输列表快照元素：(任务 id, 状态, 进度度量)
pub(super) type TransferSnapshot = (u64, TaskStatus, u64);

/// 进度度量：大小已知时用千分比（1‰ = 0.1%），未知时退化成已传字节
///
/// 目录大小还在后台统计时 `file_size == 0`，百分比恒为 0；如果不换成字节数，这类
/// 任务的进度就永远触发不了推送，UI 会卡在 0。
pub(super) fn progress_marker(t: &Task) -> u64 {
    let done = t.bytes_done.min(t.file_size);
    match done
        .checked_mul(1000)
        .and_then(|n| n.checked_div(t.file_size))
    {
        // 除数为 0（大小未知 / 空文件）：退化成已传字节，仍然能反映进度
        None => t.bytes_done,
        Some(permille) => permille,
    }
}

/// 生成快照并按 id 排序（任务表顺序会随新增/删除变化，排序后才能逐位比较）
pub(super) fn transfer_snapshot(tasks: &[Task]) -> Vec<TransferSnapshot> {
    let mut snap: Vec<TransferSnapshot> = tasks
        .iter()
        .map(|t| (t.id, t.status, progress_marker(t)))
        .collect();
    snap.sort_unstable_by_key(|s| s.0);
    snap
}

/// 新快照是否需要推给 UI
///
/// 推送条件：任务集合变化（数量不同 / id 不同 / 顺序按 id 归一后不同）、任一任务状态
/// 变化、进度回退（重试归零），或进度前进超过 `PROGRESS_PUBLISH_PERMILLE`。
pub(super) fn needs_publish(prev: &[TransferSnapshot], next: &[TransferSnapshot]) -> bool {
    if prev.len() != next.len() {
        return true; // 任务新增 / 移除
    }
    prev.iter().zip(next.iter()).any(|(a, b)| {
        a.0 != b.0
            || a.1 != b.1
            // 先判回退（重试把进度清零必须立刻推），再判是否跨过阈值
            || b.2 < a.2
            || b.2 - a.2 > PROGRESS_PUBLISH_PERMILLE
    })
}
