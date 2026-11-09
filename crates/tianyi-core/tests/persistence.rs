//! 持久化公共 API 集成测试
//!
//! 与 `src/config.rs` / `src/task.rs` 里的单元测试互补：这里只走 **public API**
//! （`AppStore` / `TaskManager` 对外暴露的方法），验证的是"应用重启后数据还在"
//! 这条真实使用路径，而不是内部实现细节。
//!
//! 所有落盘都发生在 `std::env::temp_dir()` 下的唯一子目录里：不碰用户配置目录，
//! 不依赖当前工作目录，不联网，跑完自动清理。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tianyi_core::config::{Accounts, AppConfig, AppStore};
use tianyi_core::models::{AccountConfig, TokenInfo};
use tianyi_core::task::{Task, TaskDirection, TaskManager, TaskStatus};

/// 临时目录守卫：析构时递归删除，测试提前 panic 也不会留垃圾
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// 唯一临时目录：进程号 + 纳秒时间戳，避免并发测试互相踩
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时钟早于 UNIX 纪元")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tianyi-core-it-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录失败");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 构造一个账号（其余字段交给 `Default`）
fn account(username: &str, session_key: &str) -> AccountConfig {
    AccountConfig {
        username: username.to_string(),
        login_type: "password".to_string(),
        token: TokenInfo {
            session_key: session_key.to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 构造一个下载任务
fn task(id: u64, file_name: &str) -> Task {
    Task {
        id,
        direction: TaskDirection::Download,
        file_name: file_name.to_string(),
        file_size: 2_048,
        local_path: PathBuf::from(file_name),
        remote_folder_id: "-1".to_string(),
        status: TaskStatus::Pending,
        progress: 0.0,
        bytes_done: 0,
        speed: 0,
        last_done: 0,
        last_speed_time: 0,
        error: None,
        created_at: 0,
    }
}

/// 配置往返：`save_config` 写入的值（含 `version`）必须原样读回来
#[test]
fn config_roundtrip_keeps_all_fields_and_version() {
    let tmp = TempDir::new("config");
    let store = AppStore::new(Some(tmp.path().to_path_buf())).expect("打开存储");

    let mut config = AppConfig::default_config();
    config.download_dir = tmp.path().join("downloads");
    config.upload_thread = 5;
    config.download_thread = 9;
    config.speed_limit = 1_048_576;
    config.rapid_upload = false;
    config.generate_torrent = true;
    config.last_account = "13800138000".to_string();
    config.theme = "dark".to_string();
    config.reduce_motion = true;
    store.save_config(&config).expect("保存配置");

    let loaded = store.load_config().expect("读取配置");
    assert_eq!(loaded.version, AppConfig::CURRENT_VERSION, "版本号往返丢失");
    assert_eq!(loaded.version, config.version, "版本号往返被改写");
    assert_eq!(loaded.download_dir, config.download_dir, "下载目录不一致");
    assert_eq!(loaded.upload_thread, 5);
    assert_eq!(loaded.download_thread, 9);
    assert_eq!(loaded.speed_limit, 1_048_576);
    assert!(!loaded.rapid_upload);
    assert!(loaded.generate_torrent);
    assert_eq!(loaded.last_account, "13800138000");
    assert_eq!(loaded.theme, "dark");
    assert!(loaded.reduce_motion);

    // 空存储读到的是默认配置，且同样带当前版本号
    let empty = TempDir::new("config-empty");
    let fresh = AppStore::new(Some(empty.path().to_path_buf()))
        .expect("打开空存储")
        .load_config()
        .expect("空存储应给出默认配置");
    assert_eq!(fresh.version, AppConfig::CURRENT_VERSION);
}

/// 账号往返：两个账号落盘后读回数量与内容一致，`upsert` 覆盖同名账号
#[test]
fn accounts_roundtrip_with_upsert_overwrite() {
    let tmp = TempDir::new("accounts");
    let store = AppStore::new(Some(tmp.path().to_path_buf())).expect("打开存储");

    let mut accounts = Accounts::default();
    accounts.upsert(account("13800138000", "key-old"));
    accounts.upsert(account("user@example.com", "key-mail"));
    // 同名账号覆盖：不能变成三条
    accounts.upsert(account("13800138000", "key-new"));
    assert_eq!(accounts.accounts.len(), 2, "upsert 应覆盖而不是追加");

    store.save_accounts(&accounts).expect("保存账号");

    let loaded = store.load_accounts().expect("读取账号");
    assert_eq!(loaded.accounts.len(), 2);
    let phone = loaded.find("13800138000").expect("手机号账号丢失");
    assert_eq!(phone.token.session_key, "key-new", "覆盖后的 token 未落盘");
    assert_eq!(phone.login_type, "password");
    let mail = loaded.find("user@example.com").expect("邮箱账号丢失");
    assert_eq!(mail.token.session_key, "key-mail");
    assert!(loaded.find("nobody").is_none());
}

/// 任务持久化：新建 `TaskManager` 指向同一文件即为"重启"，任务必须还在
#[test]
fn tasks_survive_manager_restart() {
    let tmp = TempDir::new("tasks");
    let path = tmp.path().join("tasks.json");

    let first = TaskManager::new(Some(path.clone()));
    assert!(first.list().is_empty(), "全新任务表应为空");

    let id = first.next_id();
    first.add(task(id, "季度报告.pdf"));
    assert_eq!(first.list().len(), 1);
    assert!(path.exists(), "add 之后应立即落盘");

    // 重启：新的 TaskManager 指向同一个文件
    let second = TaskManager::new(Some(path.clone()));
    let tasks = second.list();
    assert_eq!(tasks.len(), 1, "重启后任务丢失");
    assert_eq!(tasks[0].id, id);
    assert_eq!(tasks[0].file_name, "季度报告.pdf");
    assert_eq!(tasks[0].status, TaskStatus::Pending);
    assert_eq!(
        second.get(id).map(|t| t.file_name).as_deref(),
        Some("季度报告.pdf")
    );

    // 任务 id 序列接着已有数据往后走，不会复用已落盘的 id
    let next = second.next_id();
    assert!(
        next > id,
        "重启后 next_id 必须接在已有 id 之后，实际 {next}"
    );
}
