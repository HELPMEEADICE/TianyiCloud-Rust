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

/// 临时目录里所有指定前缀的文件名（按名字排序，方便断言"恰好一份留档"）
fn archived(dir: &Path, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("读取临时目录失败")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect();
    names.sort();
    names
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

/// 损坏的 `config.json`：留档（内容逐字节保留）+ 按默认配置启动
///
/// 以前这里是直接 `Err`：设置丢了、坏文件还留在原位，下次启动继续坏。
#[test]
fn corrupt_config_is_archived_and_defaults_returned() {
    let tmp = TempDir::new("corrupt-config");
    let store = AppStore::new(Some(tmp.path().to_path_buf())).expect("打开存储");
    let broken = "{ 这是半截配置，解析必然失败";
    std::fs::write(tmp.path().join("config.json"), broken).expect("写入损坏配置");

    let cfg = store.load_config().expect("损坏配置不应让客户端起不来");
    assert_eq!(cfg.version, AppConfig::CURRENT_VERSION);
    assert_eq!(cfg.upload_thread, 3, "应按默认配置启动");
    assert_eq!(cfg.theme, "system", "应按默认配置启动");

    let names = archived(tmp.path(), "config.json.corrupt-");
    assert_eq!(names.len(), 1, "必须留下恰好一份留档，实际：{names:?}");
    let kept = std::fs::read_to_string(tmp.path().join(&names[0])).expect("读取留档");
    assert_eq!(kept, broken, "留档内容必须与原坏文件逐字节一致");

    // 留档之后还要补一份默认配置：否则下次启动读到的还是同一份坏文件
    let rewritten =
        std::fs::read_to_string(tmp.path().join("config.json")).expect("默认配置应被重新写出");
    assert!(
        rewritten.contains(&format!("\"version\": {}", AppConfig::CURRENT_VERSION)),
        "重写的默认配置必须带当前版本号，实际：{rewritten}"
    );
}

/// 损坏的 `accounts.json`：留档 + 按空账号表启动
#[test]
fn corrupt_accounts_is_archived_and_empty_table_returned() {
    let tmp = TempDir::new("corrupt-accounts");
    let store = AppStore::new(Some(tmp.path().to_path_buf())).expect("打开存储");
    let broken = r#"{"accounts":[{"username":"13800138000","token":{ 半截"#;
    std::fs::write(tmp.path().join("accounts.json"), broken).expect("写入损坏账号文件");

    let accounts = store
        .load_accounts()
        .expect("损坏账号文件不应让客户端起不来");
    assert!(accounts.accounts.is_empty(), "应按空账号表启动");

    let names = archived(tmp.path(), "accounts.json.corrupt-");
    assert_eq!(names.len(), 1, "必须留下恰好一份留档，实际：{names:?}");
    let kept = std::fs::read_to_string(tmp.path().join(&names[0])).expect("读取留档");
    assert_eq!(kept, broken, "留档内容必须与原坏文件逐字节一致");
    // 空账号表等价于「还没登录」：不在读路径上补写空文件，下次 save_accounts 自然建起来
    assert!(
        !tmp.path().join("accounts.json").exists(),
        "不应补写空账号表"
    );
}

/// 同一秒内二次损坏：留档绝不能覆盖已有的那一份
///
/// 目标名带 unix 秒，所以先按同一算法算出一个"大概率同名"的文件预置进去。若计算与调用
/// 之间恰好跨过了一秒，本次留档会落到不带后缀的基础名上——两条不变量（预置文件内容
/// 不变、本次坏内容另有一份）与秒数无关，因此这个测试不靠时间竞争取胜。
///
/// 覆盖的场景：Windows 上 `rename` 到已存在的目标会失败，而调用点是 `let _ = rename()`
/// ——旧实现会静默丢掉留档，随后写默认值把坏文件覆盖掉。
#[test]
fn corrupt_archive_never_overwrites_existing_archive() {
    let tmp = TempDir::new("corrupt-collision");
    let store = AppStore::new(Some(tmp.path().to_path_buf())).expect("打开存储");

    let squatter_content = "上一份留档，绝对不能被覆盖";
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟早于 UNIX 纪元")
        .as_secs();
    let squatter = format!("config.json.corrupt-{secs}");
    std::fs::write(tmp.path().join(&squatter), squatter_content).expect("预置同名留档");

    let broken = "{ 第二个坏配置";
    std::fs::write(tmp.path().join("config.json"), broken).expect("写入损坏配置");
    store.load_config().expect("损坏配置不应让客户端起不来");

    let names = archived(tmp.path(), "config.json.corrupt-");
    assert_eq!(
        names.len(),
        2,
        "预置留档 + 本次留档应各占一份，实际：{names:?}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(&squatter)).expect("预置留档必须还在"),
        squatter_content,
        "已有留档被覆盖了"
    );
    let other = names
        .iter()
        .find(|n| n.as_str() != squatter.as_str())
        .expect("本次留档缺失");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(other)).expect("读取本次留档"),
        broken,
        "本次坏内容没有完整留档"
    );
}

/// 状态转换必须**当场**落盘（这条路径不吃进度去抖）
///
/// 观测方式：做完 `transition` 不做任何 flush/sleep，直接读磁盘上的 JSON 找新状态。
#[test]
fn transition_persists_immediately() {
    let tmp = TempDir::new("transition-persist");
    let path = tmp.path().join("tasks.json");
    let manager = TaskManager::new(Some(path.clone()));
    let id = manager.next_id();
    manager.add(task(id, "立即落盘.bin"));
    assert!(
        std::fs::read_to_string(&path)
            .expect("add 之后应已落盘")
            .contains("\"status\":\"Pending\""),
        "add 之后磁盘上应是 Pending"
    );

    assert!(manager.transition(id, TaskStatus::Running));
    let raw = std::fs::read_to_string(&path).expect("读回任务表");
    assert!(
        raw.contains("\"status\":\"Running\""),
        "transition 必须立刻落盘，实际磁盘内容：{raw}"
    );
}

/// 旧版 `accounts.json`（含已删除的 `password` 字段、缺后来补齐的字段）仍要能加载
#[test]
fn legacy_accounts_with_password_field_still_loads() {
    let tmp = TempDir::new("legacy-accounts");
    let store = AppStore::new(Some(tmp.path().to_path_buf())).expect("打开存储");
    let legacy = r#"{"accounts":[{"login_type":"password","username":"13800138000",
        "password":"","family_id":"0","space_type":"personal",
        "token":{"session_key":"key-old"}}]}"#;
    std::fs::write(tmp.path().join("accounts.json"), legacy).expect("写入旧版账号文件");

    let accounts = store
        .load_accounts()
        .expect("含 password 的旧文件必须能加载");
    assert_eq!(accounts.accounts.len(), 1);
    let acc = accounts.find("13800138000").expect("账号丢失");
    assert_eq!(acc.login_type, "password");
    assert_eq!(acc.token.session_key, "key-old");
    assert_eq!(acc.upload_thread, 0, "缺失字段应落到 serde 默认值");
    assert!(acc.token.access_token.is_empty(), "缺失的 token 字段按空串");

    // 重新落盘后不再写出 password 键（login_type 的**值**仍是 "password"，不算）
    store.save_accounts(&accounts).expect("保存账号");
    let raw = std::fs::read_to_string(tmp.path().join("accounts.json")).expect("读回账号文件");
    assert!(
        !raw.contains("\"password\":"),
        "新文件不应再有 password 字段，实际：{raw}"
    );
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
