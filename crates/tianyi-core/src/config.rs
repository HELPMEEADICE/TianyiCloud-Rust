//! 应用配置与账号持久化

use crate::error::{Error, Result};
use crate::models::{AccountConfig, TokenInfo};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 全局应用配置（不含账号敏感信息）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// 配置结构版本号（字段缺失视为 0，即历史格式）
    #[serde(default)]
    pub version: u32,
    /// 下载目录
    pub download_dir: PathBuf,
    /// 默认并发上传线程数
    pub upload_thread: u32,
    /// 默认并发下载线程数
    pub download_thread: u32,
    /// 上传/下载速度限制（bytes/s，0 为不限）
    pub speed_limit: u64,
    /// 是否开启秒传
    pub rapid_upload: bool,
    /// 是否自动生成 CAS torrent
    pub generate_torrent: bool,
    /// 最近使用的账号（username）
    pub last_account: String,
    /// 主题模式："system"（跟随系统）/ "light" / "dark"
    #[serde(default)]
    pub theme: String,
    /// 是否减少动画（无障碍/低性能设备）
    #[serde(default)]
    pub reduce_motion: bool,
}

impl AppConfig {
    /// 当前配置结构版本（落盘时写入的版本号）
    ///
    /// 迁移链：
    /// - **v0**（无 `version` 字段）：历史格式。字段级兼容由各字段的 `#[serde(default)]`
    ///   与 [`AppConfig::default_config`] 的默认值承担，因此 v0 → v1 只需补版本号。
    /// - **v1**：当前格式（`version` / `theme` / `reduce_motion`）。
    ///
    /// 以后新增字段时：把 `CURRENT_VERSION` +1，并在 [`AppConfig::migrate`] 的
    /// 迁移链里补一个 `match` 分支。
    pub const CURRENT_VERSION: u32 = 1;

    pub fn default_config() -> Self {
        let download_dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
        AppConfig {
            version: Self::CURRENT_VERSION,
            download_dir,
            upload_thread: 3,
            download_thread: 4,
            speed_limit: 0,
            rapid_upload: true,
            generate_torrent: false,
            last_account: String::new(),
            theme: "system".to_string(),
            reduce_motion: false,
        }
    }

    /// 从 JSON 文本加载配置（自动迁移到当前版本）
    pub fn from_json_str(data: &str) -> Result<Self> {
        let raw: serde_json::Value =
            serde_json::from_str(data).map_err(|e| Error::Config(format!("parse config: {e}")))?;
        Self::migrate(raw)
    }

    /// 把任意历史/未来格式的配置对象迁移到当前版本
    ///
    /// - 缺 `version`（= 0）：按历史格式处理，逐级升级到 [`AppConfig::CURRENT_VERSION`]。
    /// - 版本高于当前：记 warn 后按当前结构尽力解析（不丢用户已改的已知字段）。
    /// - 根节点不是 JSON 对象，或字段类型不合法：返回 [`Error::Config`]。
    pub fn migrate(raw: serde_json::Value) -> Result<Self> {
        let serde_json::Value::Object(mut map) = raw else {
            return Err(Error::Config("config root is not a JSON object".into()));
        };

        let raw_version = map
            .get("version")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);

        if raw_version > Self::CURRENT_VERSION {
            log::warn!(
                "config version {raw_version} is newer than supported {}, parsing with current schema",
                Self::CURRENT_VERSION
            );
        }

        // 迁移链：每一步把版本 +1。v0 -> v1 只补版本号（字段兼容交给 serde 默认值），
        // 新增版本时在这里追加分支。
        let mut v = raw_version.min(Self::CURRENT_VERSION);
        while v < Self::CURRENT_VERSION {
            match v {
                0 => {}
                _ => break,
            }
            v += 1;
        }

        // 无论来源版本，最终一律按当前版本落盘
        map.insert(
            "version".to_string(),
            serde_json::Value::from(Self::CURRENT_VERSION),
        );

        serde_json::from_value(serde_json::Value::Object(map))
            .map_err(|e| Error::Config(format!("parse config: {e}")))
    }
}

/// 主题模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    /// 跟随系统
    System,
    /// 浅色
    Light,
    /// 深色
    Dark,
}

impl ThemeMode {
    /// 解析配置字符串
    ///
    /// `""` / `"system"` → `System`；`"light"` → `Light`；`"dark"` → `Dark`；
    /// 其他未知值一律回退为 `System`（配置被人手改坏时不能让界面无法启动）。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => ThemeMode::System,
        }
    }

    /// 序列化为配置字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
}

/// 账号存储（多账号）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Accounts {
    pub accounts: Vec<AccountConfig>,
}

impl Accounts {
    pub fn find(&self, username: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.username == username)
    }

    pub fn find_mut(&mut self, username: &str) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| a.username == username)
    }

    pub fn upsert(&mut self, account: AccountConfig) {
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|a| a.username == account.username)
        {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    pub fn remove(&mut self, username: &str) {
        self.accounts.retain(|a| a.username != username);
    }
}

/// 应用存储（管理配置文件和账号文件）
pub struct AppStore {
    root: PathBuf,
    config_path: PathBuf,
    accounts_path: PathBuf,
}

impl AppStore {
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        let root = root.unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tianyi-cloud-rust")
        });
        std::fs::create_dir_all(&root).map_err(|e| Error::Config(e.to_string()))?;
        Ok(AppStore {
            config_path: root.join("config.json"),
            accounts_path: root.join("accounts.json"),
            root,
        })
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn load_config(&self) -> Result<AppConfig> {
        if !self.config_path.exists() {
            let cfg = AppConfig::default_config();
            self.save_config(&cfg)?;
            return Ok(cfg);
        }
        let data = std::fs::read_to_string(&self.config_path)
            .map_err(|e| Error::Config(format!("read config: {e}")))?;
        AppConfig::from_json_str(&data)
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<()> {
        // 落盘一律带当前版本号：即使调用方给的是 Default/手工构造的配置，
        // 也要保证磁盘上的 version 与结构一致。
        let mut value = serde_json::to_value(cfg)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "version".to_string(),
                serde_json::Value::from(AppConfig::CURRENT_VERSION),
            );
        }
        let data = serde_json::to_string_pretty(&value)?;
        // 直写会在中途失败时把配置文件截成半截（读回即解析失败，等于丢设置），
        // 统一走 fsutil 的「临时文件 → fsync → rename」。
        crate::fsutil::write_atomic(&self.config_path, data.as_bytes())
            .map_err(|e| Error::Config(e.to_string()))
    }

    pub fn load_accounts(&self) -> Result<Accounts> {
        if !self.accounts_path.exists() {
            return Ok(Accounts::default());
        }
        let data = std::fs::read_to_string(&self.accounts_path)
            .map_err(|e| Error::Config(format!("read accounts: {e}")))?;
        serde_json::from_str(&data).map_err(|e| Error::Config(format!("parse accounts: {e}")))
    }

    pub fn save_accounts(&self, accounts: &Accounts) -> Result<()> {
        let data = serde_json::to_string_pretty(accounts)?;
        // `save_token` 也走这里：账号文件一次写坏就等于所有已保存的登录态全丢
        crate::fsutil::write_atomic(&self.accounts_path, data.as_bytes())
            .map_err(|e| Error::Config(e.to_string()))
    }

    /// 保存单个账号的 token 信息
    pub fn save_token(&self, username: &str, token: &TokenInfo) -> Result<()> {
        let mut accounts = self.load_accounts()?;
        if let Some(acc) = accounts.find_mut(username) {
            acc.token = token.clone();
        }
        self.save_accounts(&accounts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(username: &str, token: &str) -> AccountConfig {
        AccountConfig {
            username: username.to_string(),
            token: TokenInfo {
                session_key: token.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_accounts_upsert_roundtrip() {
        let mut accounts = Accounts::default();
        accounts.upsert(account("13800138000", "key-1"));
        accounts.upsert(account("13800138000", "key-2"));
        accounts.upsert(account("user@example.com", "key-3"));

        assert_eq!(accounts.accounts.len(), 2);
        assert_eq!(
            accounts.find("13800138000").unwrap().token.session_key,
            "key-2"
        );
        assert_eq!(
            accounts.find("user@example.com").unwrap().token.session_key,
            "key-3"
        );
        assert!(accounts.find("nobody").is_none());

        accounts.remove("13800138000");
        assert_eq!(accounts.accounts.len(), 1);
        assert!(accounts.find("13800138000").is_none());
    }

    #[test]
    fn test_app_store_save_token() {
        let dir = std::env::temp_dir().join(format!("tianyi-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AppStore::new(Some(dir.clone())).unwrap();

        let mut accounts = Accounts::default();
        accounts.upsert(account("13800138000", "key-1"));
        store.save_accounts(&accounts).unwrap();

        store
            .save_token(
                "13800138000",
                &TokenInfo {
                    session_key: "key-refreshed".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();

        let loaded = store.load_accounts().unwrap();
        assert_eq!(
            loaded.find("13800138000").unwrap().token.session_key,
            "key-refreshed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_theme_mode_parse() {
        assert_eq!(ThemeMode::parse(""), ThemeMode::System);
        assert_eq!(ThemeMode::parse("system"), ThemeMode::System);
        assert_eq!(ThemeMode::parse("light"), ThemeMode::Light);
        assert_eq!(ThemeMode::parse("dark"), ThemeMode::Dark);
        // 大小写与前后空白都要容错
        assert_eq!(ThemeMode::parse(" DARK "), ThemeMode::Dark);
        assert_eq!(ThemeMode::parse("Light"), ThemeMode::Light);
        // 未知值回退为跟随系统
        assert_eq!(ThemeMode::parse("solarized"), ThemeMode::System);
        assert_eq!(ThemeMode::parse("中文主题"), ThemeMode::System);
        // 往返
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(ThemeMode::parse(mode.as_str()), mode);
        }
    }

    /// 新增配置字段必须带 serde 默认，旧版 config.json 才能继续加载
    #[test]
    fn test_config_theme_defaults_and_compat() {
        let cfg = AppConfig::default_config();
        assert_eq!(cfg.theme, "system");
        assert!(!cfg.reduce_motion);

        // 旧版 config.json：没有 theme / reduce_motion 字段
        let old = r#"{"download_dir":"C:\\Downloads","upload_thread":3,"download_thread":4,
            "speed_limit":0,"rapid_upload":true,"generate_torrent":false,"last_account":"u"}"#;
        let parsed: AppConfig = serde_json::from_str(old).expect("旧配置应能加载");
        assert_eq!(
            parsed.theme, "",
            "缺失主题时保持空串，交给 ThemeMode::parse 处理"
        );
        assert!(!parsed.reduce_motion);
        assert_eq!(ThemeMode::parse(&parsed.theme), ThemeMode::System);

        // 新字段往返
        let mut cfg = AppConfig::default_config();
        cfg.theme = "dark".to_string();
        cfg.reduce_motion = true;
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: AppConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.theme, "dark");
        assert!(back.reduce_motion);
        assert_eq!(ThemeMode::parse(&back.theme), ThemeMode::Dark);
    }

    /// 缺 version 的历史配置：迁移到当前版本，其余字段逐一保持
    #[test]
    fn test_migrate_legacy_without_version() {
        let old = r#"{"download_dir":"C:\\Downloads","upload_thread":7,"download_thread":5,
            "speed_limit":1048576,"rapid_upload":false,"generate_torrent":true,
            "last_account":"13800138000","theme":"dark","reduce_motion":true}"#;
        let cfg = AppConfig::from_json_str(old).expect("旧配置应能加载");
        assert_eq!(cfg.version, AppConfig::CURRENT_VERSION);
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.upload_thread, 7);
        assert_eq!(cfg.download_thread, 5);
        assert_eq!(cfg.speed_limit, 1_048_576);
        assert!(!cfg.rapid_upload);
        assert!(cfg.generate_torrent);
        assert_eq!(cfg.last_account, "13800138000");
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.reduce_motion);
    }

    /// 未来版本号：不崩、按当前结构尽力解析，并把版本号归一
    #[test]
    fn test_migrate_future_version_is_tolerated() {
        let future = r#"{"version":99,"download_dir":"D:\\x","upload_thread":2,
            "download_thread":2,"speed_limit":0,"rapid_upload":true,
            "generate_torrent":false,"last_account":"u","theme":"light",
            "reduce_motion":false,"unknown_future_field":{"a":1}}"#;
        let cfg = AppConfig::from_json_str(future).expect("未来版本应尽力解析");
        assert_eq!(cfg.version, AppConfig::CURRENT_VERSION);
        assert_eq!(cfg.upload_thread, 2);
        assert_eq!(cfg.theme, "light");
    }

    /// 非法 JSON / 非对象：返回 Err 而不是 panic
    #[test]
    fn test_invalid_json_returns_err() {
        let err = AppConfig::from_json_str("{not json").expect_err("非法 JSON 必须报错");
        assert!(matches!(err, Error::Config(_)), "实际错误: {err:?}");

        let err = AppConfig::from_json_str("[1,2,3]").expect_err("非对象必须报错");
        assert!(matches!(err, Error::Config(_)), "实际错误: {err:?}");
    }

    /// 落盘永远带当前版本号（即使传入的是 Default 构造的配置）
    #[test]
    fn test_save_config_stamps_current_version() {
        let dir = std::env::temp_dir().join(format!(
            "tianyi-config-ver-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AppStore::new(Some(dir.clone())).expect("store");

        assert_eq!(AppConfig::default().version, 0, "Default 视为历史格式");
        store
            .save_config(&AppConfig::default())
            .expect("save config");

        let raw = std::fs::read_to_string(dir.join("config.json")).expect("read config.json");
        assert!(
            raw.contains("\"version\": 1"),
            "磁盘上必须带版本号，实际: {raw}"
        );
        let loaded = store.load_config().expect("load config");
        assert_eq!(loaded.version, AppConfig::CURRENT_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原子写：覆盖已存在的配置文件成功，读回等价，且不残留 `.tmp`
    #[test]
    fn test_save_config_overwrites_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "tianyi-config-atomic-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AppStore::new(Some(dir.clone())).expect("store");

        let mut cfg = AppConfig::default_config();
        cfg.upload_thread = 2;
        store.save_config(&cfg).expect("首次保存");
        // 覆盖已存在的目标：rename 在 Windows 上必须是替换而不是失败
        cfg.upload_thread = 7;
        store.save_config(&cfg).expect("覆盖保存");

        let loaded = store.load_config().expect("读回配置");
        assert_eq!(loaded.upload_thread, 7, "覆盖后应读到新值");
        assert_eq!(loaded.download_dir, cfg.download_dir);
        assert!(
            !dir.join("config.json.tmp").exists(),
            "rename 之后临时文件不应残留"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
