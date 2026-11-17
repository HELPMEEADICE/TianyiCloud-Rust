//! 共享配置存储：GUI 与 CLI 走同一条「主目录 → 临时目录 → 内存态」降级链
//!
//! 为什么放在 core 而不是留在 tianyi-app 的控制器里：命令行进程和图形进程必须读写
//! **同一份** config.json / accounts.json / tasks.json。原实现把降级链、原子写和
//! 账号表读写都长在 GUI 控制器内部，CLI 一旦另写一份，就会出现「图形端说配置在 A、
//! 命令行按 B 里的登录态干活」这种错位，而且两份实现迟早会在迁移逻辑上分叉。
//!
//! 跨进程一致性：
//! * 落盘一律走 crate::fsutil::write_atomic（临时文件 → fsync → rename），
//!   任何时刻磁盘上只有「完整旧版」或「完整新版」，不存在半截 JSON；
//! * 读-改-写（[ConfigStore::update_config] / [ConfigStore::update_accounts]）
//!   全程持有 <文件>.lock 的排他锁后再**重新读盘**，因此图形端写主题、命令行写
//!   last_account 不会互相覆盖对方改的字段；
//! * 拿不到锁（另一个进程正在写，最多等 [LOCK_TIMEOUT]）时按「跳过本次写入」处理并
//!   打 warn，绝不为了写进去而破坏对方的文件。

use crate::config::{Accounts, AppConfig, AppStore};
use crate::error::{Error, Result};
use crate::flock;
use crate::lock_or_recover;
use crate::models::TokenInfo;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// 系统默认配置目录名（XDG_CONFIG_HOME/tianyi-cloud-rust 等）
pub const APP_DIR_NAME: &str = "tianyi-cloud-rust";
/// 主配置目录不可用时的降级目录名（位于系统临时目录下）
pub const FALLBACK_CONFIG_DIR: &str = "tianyi-cloud-fallback";
/// 传输任务表文件名（GUI / CLI 共用，格式为 Vec<Task>）
pub const TASKS_FILE: &str = "tasks.json";
/// CLI 交互式历史文件名（与配置文件同目录）
pub const HISTORY_FILE: &str = "cli-history.txt";

/// 配置/账号文件的排他锁名
const CONFIG_LOCK: &str = "config.lock";
const ACCOUNTS_LOCK: &str = "accounts.lock";

/// 抢锁的最长等待时间：锁只覆盖一次读-改-写，正常耗时是毫秒级
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);

/// 配置根目录的选择结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreRoot {
    /// 主目录可用
    Primary(PathBuf),
    /// 主目录不可用，已退到回退目录
    Fallback(PathBuf),
    /// 两处都不可用：本次运行使用内存态配置，不落盘
    Memory,
}

/// 降级原因（由各前端自己翻译成用户可读文案）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreDegrade {
    /// 已退到临时目录
    Fallback(PathBuf),
    /// 完全不可写，本次运行只在内存里改
    Memory,
}

/// 决定本次运行用哪个配置根目录
///
/// primary 传 None 表示「由 [AppStore] 自己解析系统默认配置目录」（生产路径），
/// 只有测试才需要显式指定一个必然失败的路径。任何一步失败都不 panic：最差落到
/// [StoreRoot::Memory]。
pub fn pick_store_root(primary: Option<&Path>, fallback: &Path) -> StoreRoot {
    match AppStore::new(primary.map(Path::to_path_buf)) {
        Ok(store) => return StoreRoot::Primary(store.root().clone()),
        Err(e) => log::warn!(
            "配置目录不可用（{}）：{e}",
            primary
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "系统默认".to_string())
        ),
    }
    match AppStore::new(Some(fallback.to_path_buf())) {
        Ok(store) => StoreRoot::Fallback(store.root().clone()),
        Err(e) => {
            log::warn!("回退配置目录 {} 也不可用：{e}", fallback.display());
            StoreRoot::Memory
        }
    }
}

/// 配置存储：优先磁盘，目录不可用时退化为内存态
///
/// 为什么要再包一层：AppStore 每个接口都直接摸文件系统，目录不可写时连
/// load_config 都会失败。这里把「内存态」显式建模出来，让 GUI / CLI 沿用同一套调用
/// 形状，同时保证内存态下 save_config 真的能改变本进程内的配置——否则设置页改了也白改。
pub struct ConfigStore {
    /// None = 内存态（本次运行不落盘）
    disk: Option<AppStore>,
    /// 内存态下配置的唯一副本；磁盘态时同步跟随，供回读兜底
    mem: Mutex<AppConfig>,
    /// 展示用配置目录
    root: PathBuf,
}

impl ConfigStore {
    /// 按「主目录 → 临时目录 → 内存态」的顺序打开，返回 (存储, 降级原因)
    pub fn open() -> (Self, Option<StoreDegrade>) {
        let fallback = std::env::temp_dir().join(FALLBACK_CONFIG_DIR);
        match pick_store_root(None, &fallback) {
            StoreRoot::Primary(root) => (Self::open_disk(root), None),
            StoreRoot::Fallback(root) => {
                let degrade = StoreDegrade::Fallback(root.clone());
                (Self::open_disk(root), Some(degrade))
            }
            StoreRoot::Memory => (Self::memory(fallback), Some(StoreDegrade::Memory)),
        }
    }

    /// 在用户显式指定的目录上打开（--config-dir、测试用）
    ///
    /// 与 [ConfigStore::open] 的差别是**不降级**：用户既然点名了这个目录，创建失败
    /// 就必须明确报错，而不是悄悄换一个地方写文件。
    pub fn open_at(root: PathBuf) -> Result<Self> {
        let store = AppStore::new(Some(root.clone()))?;
        let cfg = store.load_config()?;
        Ok(ConfigStore {
            disk: Some(store),
            mem: Mutex::new(cfg),
            root,
        })
    }

    /// 打开磁盘态；万一二次打开失败，退化为内存态而不是 panic
    fn open_disk(root: PathBuf) -> Self {
        match AppStore::new(Some(root.clone())) {
            Ok(store) => {
                let cfg = store.load_config().unwrap_or_else(|e| {
                    log::warn!("读取配置失败，使用默认值：{e}");
                    AppConfig::default_config()
                });
                ConfigStore {
                    disk: Some(store),
                    mem: Mutex::new(cfg),
                    root,
                }
            }
            Err(e) => {
                log::warn!(
                    "配置目录 {} 二次打开失败，退化为内存态：{e}",
                    root.display()
                );
                Self::memory(root)
            }
        }
    }

    fn memory(root: PathBuf) -> Self {
        ConfigStore {
            disk: None,
            mem: Mutex::new(AppConfig::default_config()),
            root,
        }
    }

    /// 是否真的会落盘（决定任务表要不要配持久化路径、提示条怎么说）
    pub fn is_persistent(&self) -> bool {
        self.disk.is_some()
    }

    /// 配置根目录
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// 传输任务表路径；内存态返回 None（写不进去，也不该留下半个任务文件）
    pub fn tasks_path(&self) -> Option<PathBuf> {
        self.is_persistent().then(|| self.root.join(TASKS_FILE))
    }

    /// REPL 历史文件路径；内存态返回 None
    pub fn history_path(&self) -> Option<PathBuf> {
        self.is_persistent().then(|| self.root.join(HISTORY_FILE))
    }

    /// 读取配置
    pub fn load_config(&self) -> Result<AppConfig> {
        match &self.disk {
            Some(store) => Ok(store.load_config()?),
            None => Ok(lock_or_recover(&self.mem).clone()),
        }
    }

    /// 整体覆盖写配置
    ///
    /// 调用方给的 cfg 可能是早些时候读出来的快照，直接写会把别的进程在这期间改的
    /// 字段顶掉。**改单个字段请用 [ConfigStore::update_config]**；这个接口只用于
    /// 「整份配置就是我要写的东西」的场景（导入、重置、测试）。
    pub fn save_config(&self, cfg: &AppConfig) -> Result<()> {
        *lock_or_recover(&self.mem) = cfg.clone();
        match &self.disk {
            Some(store) => Ok(store.save_config(cfg)?),
            // 内存态：只更新进程内副本，调用方应提示「不会保存」
            None => Ok(()),
        }
    }

    /// 读-改-写配置：持锁重读磁盘后再应用改动，跨进程不丢别人的字段
    ///
    /// 返回写完的新配置（内存态下就是改完的内存副本）。
    pub fn update_config<F>(&self, f: F) -> Result<AppConfig>
    where
        F: FnOnce(&mut AppConfig),
    {
        let Some(store) = &self.disk else {
            let mut mem = lock_or_recover(&self.mem);
            f(&mut mem);
            return Ok(mem.clone());
        };
        let _guard = flock::lock_exclusive(&self.root.join(CONFIG_LOCK), LOCK_TIMEOUT);
        let mut cfg = store.load_config()?;
        f(&mut cfg);
        store.save_config(&cfg)?;
        *lock_or_recover(&self.mem) = cfg.clone();
        Ok(cfg)
    }

    /// 读取账号表
    pub fn load_accounts(&self) -> Result<Accounts> {
        match &self.disk {
            Some(store) => Ok(store.load_accounts()?),
            None => Ok(Accounts::default()),
        }
    }

    /// 整体覆盖写账号表
    pub fn save_accounts(&self, accounts: &Accounts) -> Result<()> {
        match &self.disk {
            Some(store) => Ok(store.save_accounts(accounts)?),
            None => Ok(()),
        }
    }

    /// 读-改-写账号表：持锁重读后再改，避免两个进程互相覆盖对方刚刷新的 token
    ///
    /// 返回闭包的返回值。
    pub fn update_accounts<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Accounts) -> T,
    {
        let Some(store) = &self.disk else {
            let mut accounts = Accounts::default();
            return Ok(f(&mut accounts));
        };
        let _guard = flock::lock_exclusive(&self.root.join(ACCOUNTS_LOCK), LOCK_TIMEOUT);
        let mut accounts = store.load_accounts()?;
        let out = f(&mut accounts);
        store.save_accounts(&accounts)?;
        Ok(out)
    }

    /// 写入单个账号的 token（会话刷新后的回写）
    pub fn save_token(&self, username: &str, token: &TokenInfo) -> Result<()> {
        self.update_accounts(|accounts| {
            if let Some(acc) = accounts.find_mut(username) {
                acc.token = token.clone();
            }
        })
    }
}

impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigStore")
            .field("root", &self.root)
            .field("persistent", &self.is_persistent())
            .finish()
    }
}

/// 账号首字符（GUI 头像 / CLI 提示符用）
pub fn account_initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// 构造一条「配置不可写」的错误（供需要落盘的操作使用）
pub fn memory_store_error() -> Error {
    Error::Config("配置目录不可写，本次运行使用内存态配置，无法持久化".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tianyi-store-{}-{}-{}",
            name,
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 把一个普通文件当成目录用：create_dir_all 必然失败，从而稳定复现「不可写」
    fn blocked_dir(name: &str) -> (PathBuf, PathBuf) {
        let base = unique_tmp(name);
        std::fs::create_dir_all(&base).expect("base");
        let blocked = base.join("blocked");
        std::fs::write(&blocked, b"x").expect("blocked file");
        let fallback = base.join("fallback");
        (blocked, fallback)
    }

    #[test]
    fn pick_store_root_uses_primary_when_usable() {
        let dir = unique_tmp("primary");
        let fallback = unique_tmp("primary-fb");
        match pick_store_root(Some(&dir), &fallback) {
            StoreRoot::Primary(root) => assert_eq!(root, dir),
            other => panic!("应命主目录，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fallback);
    }

    #[test]
    fn pick_store_root_falls_back_then_degrades_to_memory() {
        let (blocked, fallback) = blocked_dir("fallback");
        match pick_store_root(Some(&blocked), &fallback) {
            StoreRoot::Fallback(root) => assert_eq!(root, fallback),
            other => panic!("应退到回退目录，实际 {other:?}"),
        }
        // 主目录与回退目录都指到同一个「文件当目录」，必然落入内存态
        match pick_store_root(Some(&blocked), &blocked) {
            StoreRoot::Memory => {}
            other => panic!("应降级为内存态，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(blocked.parent().unwrap());
    }

    #[test]
    fn update_config_keeps_fields_written_by_another_writer() {
        let dir = unique_tmp("update");
        let a = ConfigStore::open_at(dir.clone()).expect("store a");
        let b = ConfigStore::open_at(dir.clone()).expect("store b");

        a.update_config(|cfg| cfg.theme = "dark".to_string())
            .expect("a 写主题");
        // b 用的是更早读出来的快照，但 update_config 会先重读再改，
        // 因此 a 改的主题不会被 b 的整份写入顶掉
        b.update_config(|cfg| cfg.last_account = "u1".to_string())
            .expect("b 写账号");

        let cfg = ConfigStore::open_at(dir.clone())
            .expect("reopen")
            .load_config()
            .expect("load");
        assert_eq!(cfg.theme, "dark");
        assert_eq!(cfg.last_account, "u1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_at_on_unwritable_path_reports_error_instead_of_degrading() {
        let (blocked, _) = blocked_dir("explicit");
        let err = ConfigStore::open_at(blocked.clone()).expect_err("显式目录必须报错");
        assert!(matches!(err, Error::Config(_)), "实际：{err:?}");
        let _ = std::fs::remove_dir_all(blocked.parent().unwrap());
    }

    #[test]
    fn tasks_path_follows_persistence() {
        let dir = unique_tmp("tasks");
        let store = ConfigStore::open_at(dir.clone()).expect("store");
        assert!(store.is_persistent());
        assert_eq!(store.tasks_path(), Some(dir.join(TASKS_FILE)));
        assert_eq!(store.history_path(), Some(dir.join(HISTORY_FILE)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
