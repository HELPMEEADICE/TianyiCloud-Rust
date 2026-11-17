//! 命令行参数（clap 类型化定义）
//!
//! 参数解析在**任何图形/网络初始化之前**完成：tianyi-app 只看 argv 就决定走 CLI 还是
//! GUI，然后才把 argv 交给这里。因此本模块保持纯函数性质——不读配置、不建 runtime、
//! 不碰网络，出错的唯一出口是 clap 的 usage 错误（退出码 2）。

use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

/// 顶层命令：tianyi-app -c <子命令>
#[derive(Parser, Debug)]
#[command(
    name = "tianyi-app",
    bin_name = "tianyi-app",
    version,
    about = "天翼云盘客户端 Headless 命令行模式",
    long_about = "天翼云盘客户端 Headless 命令行模式。\n\n\
不带参数运行 tianyi-app 会打开图形界面；带 -c 则进入本命令行。\n\
单独的 -c 且 stdin 是终端时进入交互式 Shell，否则必须给出子命令。",
    propagate_version = true,
    disable_help_subcommand = false,
    after_help = "示例：\n  tianyi-app -c status\n  tianyi-app -c ls / --long\n  tianyi-app -c --json ls /\n  tianyi-app -c login qr\n  tianyi-app -c upload ./a.zip /Backup\n"
)]
pub struct Cli {
    /// 以命令行模式运行（不初始化图形界面）
    #[arg(short = 'c', long = "cli", global = true)]
    pub cli: bool,

    /// 机器可读的 JSON 输出（stdout 只有结果，不进日志与进度）
    #[arg(long, global = true)]
    pub json: bool,

    /// NDJSON 事件流：一行一条 JSON，带关联 id，适合常驻调用
    #[arg(
        long = "jsonl",
        alias = "ndjson",
        global = true,
        conflicts_with = "json"
    )]
    pub jsonl: bool,

    /// 禁止一切人工输入；需要确认的操作必须显式传 --yes
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// 对破坏性操作直接确认（等价于回答 yes）
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// 单条命令的总超时（秒）
    #[arg(long, global = true, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// 使用指定配置目录（默认取系统配置目录，与 GUI 同源）
    #[arg(long, global = true, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,

    /// 本次调用使用的已保存账号（默认取上次使用的账号）
    #[arg(long, global = true, value_name = "USER")]
    pub account: Option<String>,

    /// 本次调用使用的云空间：[personal|family]
    #[arg(long, global = true, value_name = "SPACE")]
    pub space: Option<String>,

    /// 不打印进度与诊断信息（stderr 静默）
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// 全部子命令
#[derive(Subcommand, Debug)]
pub enum Command {
    /// 登录（默认交互式密码登录）
    ///
    /// 装箱：LoginArgs 里塞着 7 个 Option 的凭据结构与两个路径，比其它变体大 3 倍多，
    /// 直接内联会让每个 Command 值都跟着变大（clippy::large_enum_variant）。
    Login(Box<LoginArgs>),
    /// 退出登录（只清当前会话，保留已保存账号）
    Logout,
    /// 账号管理
    #[command(subcommand)]
    Account(AccountCmd),
    /// 显示登录状态与当前云空间
    Status,
    /// 列出云端目录
    Ls(LsArgs),
    /// 查看文件/目录详情
    Info(PathArg),
    /// 按关键字搜索
    Search(SearchArgs),
    /// 新建文件夹
    Mkdir(PathArg),
    /// 重命名（只改名，不移动）
    Rename(RenameArgs),
    /// 移动文件/目录
    Mv(MoveArgs),
    /// 复制文件/目录
    Cp(MoveArgs),
    /// 删除（进回收站；目录需 --recursive）
    Rm(RmArgs),
    /// 获取下载直链
    Link(PathArg),
    /// 显示容量
    Quota,
    /// 切换云空间
    Space(SpaceArgs),
    /// 上传本地文件/目录
    Upload(UploadArgs),
    /// 下载云端文件/目录
    Download(DownloadArgs),
    /// 列出传输任务
    Tasks(TasksArgs),
    /// 单个传输任务
    #[command(subcommand)]
    Task(TaskCmd),
}

/// 单个路径参数
#[derive(Args, Debug, Clone)]
pub struct PathArg {
    /// 云端路径（绝对或以当前目录为基准的相对路径）
    pub path: String,
}

/// ls
#[derive(Args, Debug)]
pub struct LsArgs {
    /// 目录路径（默认当前目录）
    pub path: Option<String>,
    /// 详细模式：大小、修改时间
    #[arg(long)]
    pub long: bool,
    /// 页码（从 1 开始）
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub page: u64,
    /// 每页条数（默认 200；与 --all 互斥）
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// 不分页，列出全部
    #[arg(long, conflicts_with_all = ["page", "limit"])]
    pub all: bool,
}

/// search
#[derive(Args, Debug)]
pub struct SearchArgs {
    /// 搜索关键字
    pub keyword: String,
    /// 页码（从 1 开始）
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub page: u64,
    /// 每页条数（默认 200）
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// 不分页
    #[arg(long, conflicts_with_all = ["page", "limit"])]
    pub all: bool,
}

/// rename
#[derive(Args, Debug)]
pub struct RenameArgs {
    /// 原路径
    pub path: String,
    /// 新名称（只给名字，不含目录）
    pub new_name: String,
}

/// mv / cp：最后一个参数是目标目录
#[derive(Args, Debug)]
pub struct MoveArgs {
    /// 一个或多个源路径，最后一个参数是目标目录
    #[arg(required = true, num_args = 2.., value_name = "PATH")]
    pub paths: Vec<String>,
}

impl MoveArgs {
    /// 拆成 (源列表, 目标目录)
    pub fn split(&self) -> (Vec<&str>, &str) {
        let (dst, srcs) = match self.paths.split_last() {
            Some((dst, srcs)) => (dst.as_str(), srcs),
            None => ("", &[][..]),
        };
        (srcs.iter().map(String::as_str).collect(), dst)
    }
}

/// rm
#[derive(Args, Debug)]
pub struct RmArgs {
    /// 要删除的路径（可多个）
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    pub paths: Vec<String>,
    /// 允许删除目录（服务端递归删除）
    #[arg(long)]
    pub recursive: bool,
}

/// space
#[derive(Args, Debug)]
pub struct SpaceArgs {
    /// 目标空间：[personal|family]；省略时只显示当前空间
    pub space: Option<String>,
}

/// upload
#[derive(Args, Debug)]
pub struct UploadArgs {
    /// 本地文件/目录；尾部若给出以 / 开头的云端目录，则视为目标目录
    #[arg(required = true, num_args = 1.., value_name = "SOURCE")]
    pub paths: Vec<String>,
    /// 目标云端目录（显式写法，优先于尾部位置参数）
    #[arg(long, value_name = "DIR")]
    pub to: Option<String>,
    /// 递归上传目录（目录上传默认就是递归的，此参数为显式声明）
    #[arg(long)]
    pub recursive: bool,
    /// 只登记任务不下传，等 task resume 再跑
    #[arg(long)]
    pub no_wait: bool,
    /// 覆盖云端同名文件（默认覆盖）
    #[arg(long, default_value_t = true)]
    pub overwrite: bool,
}

impl UploadArgs {
    /// 拆成 (本地源列表, 目标云端目录)
    ///
    /// 规则（可预测、无歧义）：
    /// 1. 给了 --to：全部位置参数都是源；
    /// 2. 否则若至少两个位置参数，且**最后一个**以 / 开头、并且本地磁盘上不存在同路径，
    ///    则最后一个当目标目录；
    /// 3. 其余情况全部按源处理，目标目录用当前目录。
    pub fn split(&self) -> (Vec<PathBuf>, Option<String>) {
        let mut items: Vec<String> = self.paths.clone();
        if self.to.is_some() {
            return (
                items.into_iter().map(PathBuf::from).collect(),
                self.to.clone(),
            );
        }
        if items.len() >= 2 {
            let last = items.last().cloned().unwrap_or_default();
            let looks_remote = last.starts_with('/') && !Path::new(&last).exists();
            if looks_remote {
                items.pop();
                return (items.into_iter().map(PathBuf::from).collect(), Some(last));
            }
        }
        (items.into_iter().map(PathBuf::from).collect(), None)
    }
}

/// download
#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// 云端路径
    pub remote: String,
    /// 本地目标（文件或目录）；默认取配置里的下载目录
    pub local: Option<PathBuf>,
    /// 目标目录（显式写法）
    #[arg(long, value_name = "DIR")]
    pub to: Option<PathBuf>,
    /// 递归下载目录
    #[arg(long)]
    pub recursive: bool,
    /// 只登记任务不下传，等 task resume 再跑
    #[arg(long)]
    pub no_wait: bool,
}

/// tasks
#[derive(Args, Debug)]
pub struct TasksArgs {
    /// 过滤：[all|active|done|failed]
    #[arg(long, value_name = "FILTER", default_value = "all")]
    pub filter: String,
    /// 最多显示多少条
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
}

/// task 子命令
#[derive(Subcommand, Debug)]
pub enum TaskCmd {
    /// 查看单个任务
    Info { id: u64 },
    /// 暂停任务（若任务在别的进程里跑，只记录状态）
    Pause { id: u64 },
    /// 继续任务（本进程会真的去传）
    Resume {
        id: u64,
        /// 只把状态改回「等待中」，不立刻传输
        #[arg(long)]
        no_wait: bool,
    },
    /// 取消任务
    Cancel { id: u64 },
    /// 从任务表移除
    Remove { id: u64 },
}

/// account 子命令
#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// 列出已保存账号
    List,
    /// 切换为指定账号
    Use { username: String },
    /// 显示当前账号
    Current,
}

/// login
#[derive(Args, Debug)]
pub struct LoginArgs {
    #[command(subcommand)]
    pub mode: Option<LoginMode>,
    #[command(flatten)]
    pub creds: Credentials,
    /// 二维码登录：把二维码写入文件（.svg 矢量图 / .txt 文本）
    #[arg(long, value_name = "PATH")]
    pub qr_file: Option<PathBuf>,
    /// 二维码登录：不在终端绘制二维码
    #[arg(long)]
    pub no_qr_art: bool,
    /// 二维码轮询超时（秒）
    #[arg(long, value_name = "SECS")]
    pub qr_timeout: Option<u64>,
}

/// 登录方式
#[derive(Subcommand, Debug)]
pub enum LoginMode {
    /// 密码登录
    Password(Credentials),
    /// 二维码登录
    Qr(QrArgs),
}

/// 二维码登录参数
#[derive(Args, Debug, Default)]
pub struct QrArgs {
    /// 轮询多少秒后放弃
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// 二维码写入文件（.svg / .txt）
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,
    /// 不在终端绘制二维码
    #[arg(long)]
    pub no_art: bool,
}

/// 密码与验证码输入
#[derive(Args, Debug, Default, Clone)]
pub struct Credentials {
    /// 账号（手机号 / 邮箱）
    #[arg(long, value_name = "USER")]
    pub username: Option<String>,
    /// 从 stdin 读取密码（推荐脚本使用）
    #[arg(long)]
    pub password_stdin: bool,
    /// 从文件读取密码
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
    /// 从指定文件描述符读取密码（仅 Unix）
    #[arg(long, value_name = "FD")]
    pub password_fd: Option<i32>,
    /// 验证码文本
    #[arg(long, value_name = "CODE")]
    pub captcha: Option<String>,
    /// 从文件读取验证码
    #[arg(long, value_name = "PATH")]
    pub captcha_file: Option<PathBuf>,
    /// 把服务端下发的验证码图片保存到指定路径
    #[arg(long, value_name = "PATH")]
    pub captcha_out: Option<PathBuf>,
}

impl Credentials {
    /// 合并「子命令里给的」与「顶层给的」两份凭据：前者优先
    pub fn merge(preferred: Self, fallback: Self) -> Self {
        Credentials {
            username: preferred.username.or(fallback.username),
            password_stdin: preferred.password_stdin || fallback.password_stdin,
            password_file: preferred.password_file.or(fallback.password_file),
            password_fd: preferred.password_fd.or(fallback.password_fd),
            captcha: preferred.captcha.or(fallback.captcha),
            captcha_file: preferred.captcha_file.or(fallback.captcha_file),
            captcha_out: preferred.captcha_out.or(fallback.captcha_out),
        }
    }

    /// 是否给了任何一种密码来源
    pub fn has_password_source(&self) -> bool {
        self.password_stdin || self.password_file.is_some() || self.password_fd.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("tianyi-app").chain(argv.iter().copied()))
    }

    #[test]
    fn global_flags_combine_with_subcommands() {
        let cli = parse(&["-c", "--json", "--non-interactive", "ls", "/"]).expect("应能解析");
        assert!(cli.cli);
        assert!(cli.json);
        assert!(cli.non_interactive);
        assert!(matches!(cli.command, Some(Command::Ls(_))));
    }

    #[test]
    fn json_and_jsonl_are_mutually_exclusive() {
        assert!(parse(&["-c", "--json", "--jsonl", "status"]).is_err());
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        let err = parse(&["-c", "definitely-not-a-command"]).expect_err("未知命令必须报错");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn missing_required_positional_is_a_usage_error() {
        let err = parse(&["-c", "info"]).expect_err("缺参数必须报错");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rm_requires_at_least_one_path() {
        assert!(parse(&["-c", "rm"]).is_err());
        assert!(parse(&["-c", "rm", "a", "b"]).is_ok());
    }

    #[test]
    fn mv_needs_two_paths_and_splits_last_as_destination() {
        assert!(parse(&["-c", "mv", "only-one"]).is_err());
        let cli = parse(&["-c", "mv", "a", "b", "/dst"]).expect("解析");
        let Some(Command::Mv(mv)) = cli.command else {
            panic!("应是 mv");
        };
        let (srcs, dst) = mv.split();
        assert_eq!(srcs, vec!["a", "b"]);
        assert_eq!(dst, "/dst");
    }

    #[test]
    fn upload_split_prefers_explicit_to_flag() {
        let cli = parse(&["-c", "upload", "./a.zip", "/Backup"]).expect("解析");
        let Some(Command::Upload(up)) = cli.command else {
            panic!("应是 upload");
        };
        let (srcs, dst) = up.split();
        assert_eq!(srcs, vec![PathBuf::from("./a.zip")]);
        assert_eq!(dst.as_deref(), Some("/Backup"));
    }

    #[test]
    fn upload_split_keeps_trailing_absolute_local_path_as_source() {
        // 最后一个参数是本地真实存在的绝对路径时，它仍然是源，不能被当成云端目录
        let real = std::env::current_exe().expect("current_exe");
        let real_s = real.to_string_lossy().to_string();
        let cli = parse(&["-c", "upload", "./a.zip", &real_s]).expect("解析");
        let Some(Command::Upload(up)) = cli.command else {
            panic!("应是 upload");
        };
        let (srcs, dst) = up.split();
        assert_eq!(srcs.len(), 2, "两个都应是本地源：{srcs:?}");
        assert!(dst.is_none());
    }

    #[test]
    fn upload_split_with_to_flag_treats_everything_as_source() {
        let cli = parse(&["-c", "upload", "a", "b", "--to", "/x"]).expect("解析");
        let Some(Command::Upload(up)) = cli.command else {
            panic!()
        };
        let (srcs, dst) = up.split();
        assert_eq!(srcs.len(), 2);
        assert_eq!(dst.as_deref(), Some("/x"));
    }

    #[test]
    fn login_password_subcommand_shares_credentials_shape() {
        let cli = parse(&[
            "-c",
            "login",
            "password",
            "--username",
            "u",
            "--password-stdin",
        ])
        .expect("解析");
        let Some(Command::Login(login)) = cli.command else {
            panic!("应是 login");
        };
        let creds = match login.mode {
            Some(LoginMode::Password(c)) => c,
            other => panic!("应是 password 子命令，实际 {other:?}"),
        };
        assert_eq!(creds.username.as_deref(), Some("u"));
        assert!(creds.password_stdin);
        let merged = Credentials::merge(creds, Credentials::default());
        assert!(merged.has_password_source());
    }

    #[test]
    fn ls_all_conflicts_with_paging_flags() {
        assert!(parse(&["-c", "ls", "/", "--all", "--page", "2"]).is_err());
        assert!(parse(&["-c", "ls", "/", "--all"]).is_ok());
    }

    #[test]
    fn task_ids_must_be_numbers() {
        assert!(parse(&["-c", "task", "info", "abc"]).is_err());
        let cli = parse(&["-c", "task", "info", "7"]).expect("解析");
        assert!(matches!(
            cli.command,
            Some(Command::Task(TaskCmd::Info { id: 7 }))
        ));
    }

    #[test]
    fn account_subcommands_are_nested() {
        let cli = parse(&["-c", "account", "use", "13800138000"]).expect("解析");
        let Some(Command::Account(AccountCmd::Use { username })) = cli.command else {
            panic!("应是 account use");
        };
        assert_eq!(username, "13800138000");
    }
}
