//! 命令调度
//!
//! Runner 把「上下文 + 任务表 + 中断枢纽」绑在一起，命令实现只需要一个 &mut Runner。
//! 任务表在构造时就绑定到配置目录里的 tasks.json（与 GUI 同一个文件），因此
//! tianyi-app -c tasks 能看到图形端的历史，反之亦然。

pub mod account;
pub mod files;
pub mod mount;
pub mod transfer;

use crate::args::Command;
use crate::context::Context;
use crate::error::CliResult;
use crate::interrupt::InterruptHub;
use crate::output::Output;
use std::path::PathBuf;
use std::sync::Arc;
use tianyi_core::task::TaskManager;

/// 一条命令的执行环境
pub struct Runner {
    /// 运行上下文（配置、会话、输出、交互策略）
    pub ctx: Context,
    /// 传输任务表（与 GUI 共用 tasks.json）
    pub tasks: Arc<TaskManager>,
    /// Ctrl+C 枢纽
    pub hub: Arc<InterruptHub>,
}

impl Runner {
    /// 构造 Runner：任务表路径取自配置目录
    pub fn new(ctx: Context, hub: Arc<InterruptHub>) -> Self {
        let tasks = Arc::new(TaskManager::new(ctx.service.store().tasks_path()));
        Runner { ctx, tasks, hub }
    }

    /// 任务表文件路径（内存态下给一个人能看懂的占位符）
    pub fn tasks_path(&self) -> PathBuf {
        self.ctx
            .service
            .store()
            .tasks_path()
            .unwrap_or_else(|| PathBuf::from("(内存态，不落盘)"))
    }

    /// 该命令是否是「长任务」：传输，或前台阻塞的挂载
    ///
    /// 这类命令不能被 Ctrl+C 硬中断（那会丢掉收尾机会），因此入口层对它们不加
    /// select 抢断：传输靠 TransferControl 的协作式暂停/取消，挂载靠自己的监视线程
    /// 卸载（见 [mount::mount]）。反过来，把它们丢进 select 通道会让 future 在半途
    /// 被 drop——传输会留下永远 Running 的幽灵任务，挂载会留下孤儿卷。
    pub fn is_transfer_like(cmd: &Command) -> bool {
        matches!(
            cmd,
            Command::Upload(_) | Command::Download(_) | Command::Task(_) | Command::Mount(_)
        )
    }

    /// 执行一条命令
    pub async fn execute(&mut self, cmd: Command) -> CliResult<Output> {
        match cmd {
            Command::Login(a) => account::login(self, *a).await,
            Command::Logout => account::logout(self).await,
            Command::Account(c) => account::account(self, c).await,
            Command::Status => account::status(self).await,
            Command::Ls(a) => files::ls(self, a).await,
            Command::Info(a) => files::info(self, a).await,
            Command::Search(a) => files::search(self, a).await,
            Command::Mkdir(a) => files::mkdir(self, a).await,
            Command::Rename(a) => files::rename(self, a).await,
            Command::Mv(a) => files::mv(self, a).await,
            Command::Cp(a) => files::cp(self, a).await,
            Command::Rm(a) => files::rm(self, a).await,
            Command::Link(a) => files::link(self, a).await,
            Command::Quota => files::quota(self, ()).await,
            Command::Space(a) => files::space(self, a).await,
            Command::Upload(a) => transfer::upload(self, a).await,
            Command::Download(a) => transfer::download(self, a).await,
            Command::Tasks(a) => transfer::tasks(self, a),
            Command::Task(c) => transfer::task(self, c).await,
            Command::Mount(a) => mount::mount(self, a).await,
            Command::Unmount(a) => mount::unmount(self, a).await,
            Command::Mounts => mount::mounts(self),
        }
    }
}
