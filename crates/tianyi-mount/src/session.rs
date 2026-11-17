//! 独立的云盘会话：挂载自己的一份客户端，互不干扰
//!
//! 为什么不直接共用 GUI/CLI 的 [tianyi_core::CloudService]：
//!
//! * GUI 里切换「个人云 / 家庭云」会改同一个 service 的空间状态，挂载中的盘会被
//!   "就地换掉内容"，而资源管理器还缓存着旧目录——这是数据错乱级的 bug；
//! * 挂载是长生命周期的东西，需要一个稳定的账号与空间，直到用户显式卸载。
//!
//! 因此这里用 [tianyi_core::ConfigStore] 里的账号直接构造一个 [TianyiClient]，
//! 把空间写进**内存里**的 AccountConfig（不写回 accounts.json，不动用户保存的偏好），
//! 同时保留 [tianyi_core::CloudService] 的关键语义：会话失效时刷新 token 并回写。

use std::future::Future;
use std::sync::Arc;
use tianyi_core::{AccountConfig, ConfigStore, Error, FileObject, Space, TianyiClient};

pub struct CloudSession {
    client: Arc<TianyiClient>,
    store: ConfigStore,
    username: String,
    space: Space,
    family_id: String,
}

impl std::fmt::Debug for CloudSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudSession")
            .field("username", &self.username)
            .field("space", &self.space)
            .finish()
    }
}

impl CloudSession {
    /// 从已保存的登录态建立一个独立会话
    ///
    /// `account` 为空时用配置里的 last_account（与 CLI/GUI 的选择保持一致）。
    /// `space` 只影响本次会话，不写回账号文件。
    pub fn open(store: ConfigStore, account: Option<&str>, space: Space) -> Result<Self, Error> {
        let cfg = store.load_config()?;
        let accounts = store.load_accounts()?;
        let wanted = account
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| Some(cfg.last_account.trim().to_string()).filter(|s| !s.is_empty()))
            // 只有一个账号时不必强求用户指定：CLI 的「tianyi-app -c mount X:」应当是开箱能用的
            .or_else(|| match accounts.accounts.len() {
                1 => accounts.accounts.first().map(|a| a.username.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                Error::NotAuthenticated(
                    "还没有登录过任何账号，请先在图形界面或 CLI 登录（多账号时用 --account 指定）"
                        .into(),
                )
            })?;
        let mut acc: AccountConfig = accounts
            .find(&wanted)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("账号不存在：{wanted}")))?;
        if acc.token.access_token.is_empty() && acc.token.session_key.is_empty() {
            return Err(Error::NotAuthenticated(format!(
                "账号 {wanted} 没有可用的登录态，请重新登录"
            )));
        }
        acc.space_type = space.as_str().to_string();
        let family_id = acc.family_id.clone();
        let client = Arc::new(TianyiClient::new(cfg, acc));
        Ok(Self {
            client,
            store,
            username: wanted,
            space,
            family_id,
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn space(&self) -> Space {
        self.space
    }

    pub fn family_id(&self) -> &str {
        &self.family_id
    }

    /// 带「会话失效 → 刷新 → 回写 → 重试一次」的调用
    ///
    /// 与 [tianyi_core::CloudService::call] 同一语义，只覆盖本模块用到的幂等只读操作。
    pub async fn call<T, F, Fut>(&self, f: F) -> Result<T, Error>
    where
        F: Fn(Arc<TianyiClient>) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        match f(self.client.clone()).await {
            Err(e) if e.is_session_invalid() => {
                log::info!("挂载会话失效，刷新后重试");
                self.client
                    .refresh_session()
                    .await
                    .map_err(|e| Error::Session(format!("会话已失效，请重新登录：{e}")))?;
                // 回写：CLI 的挂载是一个独立进程，刷新后的 token 必须落盘给下一次用
                let token = self.client.token();
                if let Err(e) = self.store.save_token(&self.username, &token) {
                    log::warn!("回写刷新后的 token 失败（不影响本次挂载）：{e}");
                }
                f(self.client.clone()).await
            }
            other => other,
        }
    }

    /// 列目录（单层，不递归）
    pub async fn list_raw(&self, folder_id: &str) -> Result<Vec<FileObject>, Error> {
        let id = folder_id.to_string();
        self.call(move |client| {
            let id = id.clone();
            async move { client.list_files(&id).await }
        })
        .await
    }

    /// 取文件下载直链
    pub async fn download_url(&self, file_id: &str) -> Result<String, Error> {
        let id = file_id.to_string();
        self.call(move |client| {
            let id = id.clone();
            async move { client.get_download_url(&id).await }
        })
        .await
    }

    /// 容量（失败返回 None：容量显示不出来不该让挂载失败）
    pub async fn capacity(&self) -> Option<(u64, u64)> {
        match self
            .call(|client| async move { client.get_capacity().await })
            .await
        {
            Ok(cap) => {
                let info = if self.space == Space::Family {
                    cap.family_capacity_info
                } else {
                    cap.cloud_capacity_info
                };
                Some((info.total_size.max(0) as u64, info.free_size.max(0) as u64))
            }
            Err(e) => {
                log::debug!("取容量失败（不影响挂载）：{e}");
                None
            }
        }
    }
}
