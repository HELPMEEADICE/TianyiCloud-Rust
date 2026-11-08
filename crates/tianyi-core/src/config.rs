//! 应用配置与账号持久化

use crate::error::{Error, Result};
use crate::models::{AccountConfig, TokenInfo};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 全局应用配置（不含账号敏感信息）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
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
    pub fn default_config() -> Self {
        let download_dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
        AppConfig {
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
        serde_json::from_str(&data).map_err(|e| Error::Config(format!("parse config: {e}")))
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<()> {
        let data = serde_json::to_string_pretty(cfg)?;
        std::fs::write(&self.config_path, data).map_err(|e| Error::Config(e.to_string()))
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
        std::fs::write(&self.accounts_path, data).map_err(|e| Error::Config(e.to_string()))
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
}
