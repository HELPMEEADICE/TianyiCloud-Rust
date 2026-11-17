//! 传输命令：upload / download / tasks / task
//!
//! 关于「后台任务」的诚实说明（也是本模块最重要的设计决定）：
//! 本项目**没有**常驻 worker 进程，也没有跨进程的传输守护进程。因此：
//! * --wait（默认）就是**前台**传输：进程活着 = 传输在跑，进程退出 = 传输停下；
//! * --no-wait 只把任务写进 tasks.json（状态 Pending），**不会**有任何进程去传它，
//!   必须再执行 task resume <id> 才会真的开始；
//! * 绝不在退出前声称「任务仍在后台运行」。
//!
//! 断点续传是真的：下载用 .part 前缀续传，上传靠本地文件重来；Ctrl+C 第一次暂停
//! （保住断点）第二次取消。

use crate::args::{DownloadArgs, TaskCmd, TasksArgs, UploadArgs};
use crate::cmd::Runner;
use crate::error::{CliError, CliResult, ErrorCode};
use crate::output::Output;
use crate::table;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tianyi_core::cloudpath::{local_child, CloudPath};
use tianyi_core::file;
use tianyi_core::job::{self, JobOutcome, JobRequest};
use tianyi_core::models::FileObject;
use tianyi_core::task::{TaskDirection, TaskStatus, TransferControl};
use tianyi_core::transfer::ProgressCallback;

/// 进度刷新间隔
const PROGRESS_TICK: Duration = Duration::from_millis(200);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 上传
pub async fn upload(r: &mut Runner, args: UploadArgs) -> CliResult<Output> {
    let (sources, dst) = args.split();
    if sources.is_empty() {
        return Err(CliError::usage("至少需要一个本地文件或目录"));
    }
    let remote_dir = match dst {
        Some(d) => r.ctx.resolve_path(&d)?,
        None => r.ctx.cwd.clone(),
    };
    // 先解析目标目录：不存在就整体失败，不产生任何半途任务
    let folder_id = r.ctx.service.resolve_dir(&remote_dir).await?;

    let mut items: Vec<Value> = Vec::new();
    for src in sources {
        let meta = std::fs::metadata(&src).map_err(|e| {
            CliError::new(
                ErrorCode::NotFound,
                format!("本地路径不存在或不可读：{}（{e}）", src.display()),
            )
        })?;
        if meta.is_dir() && !args.recursive {
            return Err(CliError::usage(format!(
                "{} 是目录，上传目录需要显式 --recursive",
                src.display()
            )));
        }
        let size = if meta.is_dir() {
            tianyi_core::cloudpath::dir_size(&src)
        } else {
            meta.len()
        };
        let req = JobRequest::upload(&folder_id, src.clone());
        let name = req.remote_name.clone();
        let task_id = add_task(
            r,
            TaskDirection::Upload,
            &name,
            size,
            src.clone(),
            &folder_id,
        );
        if args.no_wait {
            items.push(json!({
                "task_id": task_id,
                "name": name,
                "size": size,
                "status": "pending",
                "queued_only": true,
            }));
            continue;
        }
        let outcome = run_job(r, req, task_id, size).await?;
        items.push(json!({
            "task_id": task_id,
            "name": name,
            "size": size,
            "status": outcome_status(&outcome),
            "queued_only": false,
        }));
        if let JobOutcome::Failed(msg) = outcome {
            return Err(
                CliError::new(ErrorCode::RemoteApi, msg).with_details(json!({
                    "items": items,
                    "task_id": task_id,
                })),
            );
        }
    }
    let human = render_transfer_human("上传", &items, &remote_dir.display());
    Ok(Output::new(
        json!({
            "direction": "upload",
            "remote_dir": remote_dir.display(),
            "items": items,
        }),
        human,
    ))
}

/// 下载
pub async fn download(r: &mut Runner, args: DownloadArgs) -> CliResult<Output> {
    let remote = r.ctx.resolve_path(&args.remote)?;
    let obj = r.ctx.service.resolve_object(&remote).await?;
    let target = args.to.clone().or(args.local.clone());
    let cfg = r.ctx.service.store().load_config()?;
    let default_dir: PathBuf = cfg.download_dir.clone();

    let mut items: Vec<Value> = Vec::new();
    if obj.is_dir {
        if !args.recursive {
            return Err(CliError::usage(format!(
                "{} 是目录，下载目录需要显式 --recursive",
                remote.display()
            )));
        }
        let root: PathBuf = match &target {
            Some(p) => p.clone(),
            None => default_dir.join(tianyi_core::cloudpath::sanitize_file_name(&obj.name)),
        };
        let files = walk_remote(r, &remote).await?;
        if files.is_empty() {
            return Ok(Output::new(
                json!({
                    "direction": "download",
                    "remote": remote.display(),
                    "local_root": root.display().to_string(),
                    "items": [],
                }),
                format!("目录为空，没有下载任何文件：{}\n", remote.display()),
            ));
        }
        for (rel, file_obj) in files {
            let local = join_rel(&root, &rel);
            if let Some(parent) = local.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let size = file_obj.size.max(0) as u64;
            let task_id = add_task(
                r,
                TaskDirection::Download,
                &rel,
                size,
                local.clone(),
                &file_obj.id,
            );
            let req = JobRequest::download(&file_obj.id, &file_obj.name, size, local.clone());
            if args.no_wait {
                items.push(json!({
                    "task_id": task_id,
                    "name": rel,
                    "local_path": local.display().to_string(),
                    "size": size,
                    "status": "pending",
                    "queued_only": true,
                }));
                continue;
            }
            let outcome = run_job(r, req, task_id, size).await?;
            items.push(json!({
                "task_id": task_id,
                "name": rel,
                "local_path": local.display().to_string(),
                "size": size,
                "status": outcome_status(&outcome),
                "queued_only": false,
            }));
            if let JobOutcome::Failed(msg) = outcome {
                return Err(
                    CliError::new(ErrorCode::RemoteApi, msg).with_details(json!({
                        "items": items,
                        "task_id": task_id,
                    })),
                );
            }
        }
        let human = render_transfer_human("下载", &items, &root.display().to_string());
        return Ok(Output::new(
            json!({
                "direction": "download",
                "remote": remote.display(),
                "local_root": root.display().to_string(),
                "items": items,
            }),
            human,
        ));
    }

    // 单文件
    let local = match target {
        Some(p) => {
            let is_dir_hint = p.is_dir()
                || p.to_string_lossy().ends_with('/')
                || p.to_string_lossy().ends_with('\\');
            if is_dir_hint {
                std::fs::create_dir_all(&p)?;
                local_child(&p, &obj.name)
            } else {
                p
            }
        }
        None => {
            std::fs::create_dir_all(&default_dir)?;
            local_child(&default_dir, &obj.name)
        }
    };
    if let Some(parent) = local.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let size = obj.size.max(0) as u64;
    let task_id = add_task(
        r,
        TaskDirection::Download,
        &obj.name,
        size,
        local.clone(),
        &obj.id,
    );
    let req = JobRequest::download(&obj.id, &obj.name, size, local.clone());
    let queued = args.no_wait;
    let outcome = if queued {
        None
    } else {
        Some(run_job(r, req, task_id, size).await?)
    };
    let status = match &outcome {
        Some(o) => outcome_status(o),
        None => "pending",
    };
    let data = json!({
        "direction": "download",
        "remote": remote.display(),
        "local_path": local.display().to_string(),
        "size": size,
        "task_id": task_id,
        "status": status,
        "queued_only": queued,
    });
    if let Some(JobOutcome::Failed(msg)) = outcome {
        return Err(CliError::new(ErrorCode::RemoteApi, msg).with_details(
            json!({ "task_id": task_id, "local_path": local.display().to_string() }),
        ));
    }
    let human = format!(
        "下载完成：{} → {}（{}）\n",
        remote.display(),
        local.display(),
        file::format_size(size)
    );
    Ok(Output::new(data, human))
}

fn render_transfer_human(verb: &str, items: &[Value], target: &str) -> String {
    let ok = items
        .iter()
        .filter(|v| v["status"] == json!("completed"))
        .count();
    let queued = items
        .iter()
        .filter(|v| v["queued_only"] == json!(true))
        .count();
    let mut out = format!(
        "{verb}结束：成功 {ok} 项，共 {} 项（目标 {}）\n",
        items.len(),
        target
    );
    if queued > 0 {
        out.push_str(&format!(
            "注意：{queued} 项只是登记为等待中，没有任何进程在传；用 task resume <id> 启动\n"
        ));
    }
    out
}

/// 把相对路径拼到本地根目录（逐段消毒，杜绝路径穿越）
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        out = local_child(&out, seg);
    }
    out
}

/// 递归列出一个云端目录下的全部文件（广度优先，避免 async 递归）
async fn walk_remote(r: &mut Runner, root: &CloudPath) -> CliResult<Vec<(String, FileObject)>> {
    let mut files = Vec::new();
    let mut queue: VecDeque<(CloudPath, String)> = VecDeque::new();
    queue.push_back((root.clone(), String::new()));
    while let Some((dir, rel)) = queue.pop_front() {
        let list = r.ctx.service.list(&dir).await?;
        for item in list.items {
            let child_rel = if rel.is_empty() {
                item.name.clone()
            } else {
                format!("{rel}/{}", item.name)
            };
            if item.is_dir {
                queue.push_back((dir.child(&item.name), child_rel));
            } else {
                files.push((child_rel, item));
            }
        }
    }
    Ok(files)
}

/// 登记一条任务
fn add_task(
    r: &Runner,
    direction: TaskDirection,
    name: &str,
    size: u64,
    local: PathBuf,
    remote_id: &str,
) -> u64 {
    let id = r.tasks.next_id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    r.tasks.add(tianyi_core::task::Task {
        id,
        direction,
        file_name: name.to_string(),
        file_size: size,
        local_path: local,
        remote_folder_id: remote_id.to_string(),
        status: TaskStatus::Pending,
        progress: 0.0,
        bytes_done: 0,
        speed: 0,
        last_done: 0,
        last_speed_time: 0,
        error: None,
        created_at: now,
    });
    id
}

fn outcome_status(outcome: &JobOutcome) -> &'static str {
    match outcome {
        JobOutcome::Completed => "completed",
        JobOutcome::Paused => "paused",
        JobOutcome::Canceled => "canceled",
        JobOutcome::Failed(_) => "failed",
    }
}

/// 真正跑一次传输，并把结果写回任务表
pub async fn run_job(
    r: &mut Runner,
    req: JobRequest,
    task_id: u64,
    total: u64,
) -> CliResult<JobOutcome> {
    let client = r.ctx.service.require_client()?;
    let cfg = r.ctx.service.store().load_config()?;
    let rate = tianyi_core::ratelimit::bucket_for(cfg.speed_limit);
    let opts = job::options_for(&cfg, rate);

    let control = TransferControl::new();
    r.hub.attach(&control);
    r.tasks.transition(task_id, TaskStatus::Running);

    let done = Arc::new(AtomicU64::new(0));
    let progress: ProgressCallback = {
        let tasks = Arc::clone(&r.tasks);
        let done = Arc::clone(&done);
        Arc::new(move |n: u64| {
            done.store(n, Ordering::Relaxed);
            tasks.set_progress(task_id, n, total, now_ms());
        })
    };

    let printer = r.ctx.printer.clone();
    let ticker_printer = printer.clone();
    let is_upload = req.direction == TaskDirection::Upload;
    let label = format!(
        "{} {}",
        if is_upload { "上传" } else { "下载" },
        req.remote_name
    );
    let ticker = {
        let done = Arc::clone(&done);
        tokio::spawn(async move {
            let mut last = 0u64;
            loop {
                tokio::time::sleep(PROGRESS_TICK).await;
                let now = done.load(Ordering::Relaxed);
                let speed = now.saturating_sub(last) * 5;
                last = now;
                // total 为 0（大小未知）时不能除，按 0% 显示
                let pct = (now.min(total) * 100).checked_div(total).unwrap_or(0);
                ticker_printer.progress(&format!(
                    "{label}  {pct}%  {}/{}  {}/s",
                    file::format_size(now),
                    file::format_size(total),
                    file::format_speed(speed)
                ));
            }
        })
    };

    let outcome = job::run(&client, &req, &opts, progress, &control).await;
    let interrupts = r.hub.hits();
    ticker.abort();
    printer.progress_done();
    r.hub.detach();
    if interrupts > 0 && matches!(outcome, JobOutcome::Paused) {
        printer.diag(&format!(
            "任务 {task_id} 已暂停：断点已保留，用 task resume {task_id} 继续"
        ));
    }

    // 回写任务表：这里是状态机的唯一写入口
    match &outcome {
        JobOutcome::Completed => {
            r.tasks.transition(task_id, TaskStatus::Completed);
            r.tasks.update(task_id, |t| {
                t.status = TaskStatus::Completed;
                t.progress = 100.0;
                t.bytes_done = t.file_size;
                t.speed = 0;
                t.error = None;
            });
        }
        JobOutcome::Paused => {
            r.tasks.transition(task_id, TaskStatus::Paused);
            r.tasks.update(task_id, |t| t.speed = 0);
        }
        JobOutcome::Canceled => {
            r.tasks.transition(task_id, TaskStatus::Canceled);
            r.tasks.update(task_id, |t| t.speed = 0);
        }
        JobOutcome::Failed(msg) => {
            r.tasks.transition(task_id, TaskStatus::Failed);
            r.tasks.update(task_id, |t| {
                t.status = TaskStatus::Failed;
                t.speed = 0;
                t.error = Some(msg.clone());
            });
        }
    }
    // 关窗级别的收尾：把去抖掉的进度立即落盘
    r.tasks.flush_progress();
    Ok(outcome)
}

/// tasks
pub fn tasks(r: &mut Runner, args: TasksArgs) -> CliResult<Output> {
    let filter = args.filter.to_ascii_lowercase();
    if !matches!(
        filter.as_str(),
        "all" | "active" | "running" | "done" | "completed" | "failed" | "error"
    ) {
        return Err(CliError::usage(format!(
            "--filter 只接受 all|active|done|failed，收到 {}",
            args.filter
        )));
    }
    let mut all = r.tasks.list();
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    let filtered = table::filter_tasks(&all, &filter);
    let limit = args.limit.unwrap_or(200).max(1);
    let shown: Vec<tianyi_core::task::Task> =
        filtered.iter().take(limit).map(|t| (*t).clone()).collect();
    let items: Vec<Value> = shown.iter().map(table::task_value).collect();
    let active = all
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
            )
        })
        .count();
    let data = json!({
        "items": items,
        "counts": {
            "all": all.len(),
            "active": active,
            "finished": all.len() - active,
            "matched": filtered.len(),
            "returned": items.len(),
        },
        "tasks_file": r.tasks_path().display().to_string(),
    });
    let human = table::render_tasks(&shown);
    Ok(Output::new(data, human))
}

/// task 子命令
pub async fn task(r: &mut Runner, cmd: TaskCmd) -> CliResult<Output> {
    match cmd {
        TaskCmd::Info { id } => task_info(r, id),
        TaskCmd::Pause { id } => task_pause(r, id),
        TaskCmd::Resume { id, no_wait } => task_resume(r, id, no_wait).await,
        TaskCmd::Cancel { id } => task_cancel(r, id),
        TaskCmd::Remove { id } => task_remove(r, id),
    }
}

fn get_task(r: &Runner, id: u64) -> CliResult<tianyi_core::task::Task> {
    r.tasks
        .get(id)
        .ok_or_else(|| CliError::new(ErrorCode::NotFound, format!("任务 {id} 不存在")))
}

fn task_info(r: &mut Runner, id: u64) -> CliResult<Output> {
    let t = get_task(r, id)?;
    let data = table::task_value(&t);
    let human = table::render_tasks(std::slice::from_ref(&t));
    Ok(Output::new(data, human))
}

fn task_pause(r: &mut Runner, id: u64) -> CliResult<Output> {
    let t = get_task(r, id)?;
    if !t.status.can_pause() {
        return Err(CliError::new(
            ErrorCode::Conflict,
            format!("任务 {} 当前状态是 {}，无法暂停", id, t.status.as_key()),
        ));
    }
    r.tasks.transition(id, TaskStatus::Paused);
    r.tasks.update(id, |t| t.speed = 0);
    r.tasks.flush_progress();
    let after = get_task(r, id)?;
    Ok(Output::new(
        table::task_value(&after),
        format!("任务 {id} 已标记为暂停（若任务在另一个进程里传输，只有该进程能真正停下来）\n"),
    ))
}

async fn task_resume(r: &mut Runner, id: u64, no_wait: bool) -> CliResult<Output> {
    let t = get_task(r, id)?;
    if t.status == TaskStatus::Running {
        return Err(CliError::new(
            ErrorCode::Conflict,
            format!("任务 {id} 正在另一个进程里传输，请先在那里暂停"),
        ));
    }
    if !t.status.can_resume() {
        return Err(CliError::new(
            ErrorCode::Conflict,
            format!("任务 {} 当前状态是 {}，无法继续", id, t.status.as_key()),
        ));
    }
    // Paused → Pending 不在状态机合法边里（它是「重新排队」而不是状态推进），
    // 与 GUI 的 resume_task 走同一条路径：直接改状态再重新入队
    r.tasks.update(id, |t| {
        t.status = TaskStatus::Pending;
        t.error = None;
        t.speed = 0;
    });
    if no_wait {
        let after = get_task(r, id)?;
        return Ok(Output::new(
            table::task_value(&after),
            format!("任务 {id} 已重新排队（Pending）；不带 --no-wait 的 task resume {id} 才会真的传输\n"),
        ));
    }
    let req = match t.direction {
        TaskDirection::Download => JobRequest::download(
            &t.remote_folder_id,
            &t.file_name,
            t.file_size,
            t.local_path.clone(),
        ),
        TaskDirection::Upload => JobRequest::upload(&t.remote_folder_id, t.local_path.clone()),
    };
    let total = t.file_size;
    let outcome = run_job(r, req, id, total).await?;
    let after = get_task(r, id)?;
    if let JobOutcome::Failed(msg) = outcome {
        return Err(CliError::new(ErrorCode::RemoteApi, msg).with_details(json!({ "task_id": id })));
    }
    Ok(Output::new(
        table::task_value(&after),
        format!("任务 {id} 结束：{}\n", outcome_status(&outcome)),
    ))
}

fn task_cancel(r: &mut Runner, id: u64) -> CliResult<Output> {
    let t = get_task(r, id)?;
    if !t.status.can_cancel() {
        return Err(CliError::new(
            ErrorCode::Conflict,
            format!(
                "任务 {} 当前状态是 {}，无法取消（终态任务可以用 task remove 清理）",
                id,
                t.status.as_key()
            ),
        ));
    }
    r.tasks.transition(id, TaskStatus::Canceled);
    r.tasks.update(id, |t| t.speed = 0);
    r.tasks.flush_progress();
    let after = get_task(r, id)?;
    Ok(Output::new(
        table::task_value(&after),
        format!("任务 {id} 已取消\n"),
    ))
}

fn task_remove(r: &mut Runner, id: u64) -> CliResult<Output> {
    let t = get_task(r, id)?;
    if matches!(t.status, TaskStatus::Pending | TaskStatus::Running) {
        return Err(CliError::new(
            ErrorCode::Conflict,
            format!("任务 {id} 还在进行中，请先 pause 或 cancel 再移除"),
        ));
    }
    r.tasks.remove(id);
    r.tasks.flush_progress();
    Ok(Output::new(
        json!({ "removed": id }),
        format!("任务 {id} 已从任务表移除\n"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_rel_sanitizes_every_segment() {
        let root = Path::new("/tmp/dest");
        let p = join_rel(root, "a/../../etc/passwd");
        assert!(p.starts_with(root), "不能逃出目标目录：{}", p.display());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/') && !name.contains('\\'));
    }

    #[test]
    fn join_rel_keeps_chinese_and_spaces() {
        let p = join_rel(Path::new("/tmp/d"), "文档/我的 报告.pdf");
        assert_eq!(p, PathBuf::from("/tmp/d/文档/我的 报告.pdf"));
    }

    #[test]
    fn outcome_status_strings_are_stable() {
        assert_eq!(outcome_status(&JobOutcome::Completed), "completed");
        assert_eq!(outcome_status(&JobOutcome::Paused), "paused");
        assert_eq!(outcome_status(&JobOutcome::Canceled), "canceled");
        assert_eq!(outcome_status(&JobOutcome::Failed("x".into())), "failed");
    }
}
