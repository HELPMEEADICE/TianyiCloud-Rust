//! 天翼云盘 API 数据结构定义
//!
//! 移植自 OpenList `drivers/189pc/types.go`

use serde::{Deserialize, Serialize};

/// 会话刷新 Token 信息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenInfo {
    pub access_token: String,
    pub refresh_token: String,
    pub session_key: String,
    pub session_secret: String,
    pub family_session_key: String,
    pub family_session_secret: String,
    pub login_name: String,
}

/// 账号配置（持久化到磁盘）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountConfig {
    pub login_type: String,
    pub username: String,
    pub password: String,
    pub family_id: String,
    pub space_type: String,
    pub order_by: String,
    pub order_direction: String,
    pub upload_method: String,
    pub upload_thread: u32,
    pub rapid_upload: bool,
    pub token: TokenInfo,
}

impl AccountConfig {
    pub fn is_family(&self) -> bool {
        self.space_type == "family"
    }
}

/// 文件对象（对应 Cloud189File）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileObject {
    pub id: String,
    pub name: String,
    pub size: i64,
    pub md5: String,
    pub parent_id: String,
    pub is_dir: bool,
    pub last_op_time: String,
    pub create_date: String,
    pub thumb: String,
}

impl FileObject {
    pub fn is_folder(&self) -> bool {
        self.is_dir
    }
}

/// 文件夹对象（对应 Cloud189Folder）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FolderObject {
    pub id: String,
    pub name: String,
    pub parent_id: i64,
    pub is_dir: bool,
    pub last_op_time: String,
    pub create_date: String,
}

/// 上传初始化响应（对应 InitMultiUploadResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitMultiUploadResp {
    pub upload_type: i32,
    pub upload_host: String,
    pub upload_file_id: String,
    pub file_data_exists: i32,
}

/// 上传 URL 响应（对应 UploadUrlsResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadUrlsResp {
    pub code: String,
    pub upload_urls: std::collections::HashMap<String, UploadUrlsData>,
}

/// 单个分片上传 URL 信息
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadUrlsData {
    pub request_url: String,
    pub request_header: String,
}

/// 上传进度持久化数据（用于断点续传）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploadProgress {
    pub upload_file_id: String,
    pub upload_type: i32,
    pub upload_host: String,
    pub file_data_exists: i32,
    /// 尚未完成的分片（partInfo 字符串列表）
    pub upload_parts: Vec<String>,
    /// 已上传完成的 part 编号
    pub done_parts: Vec<i32>,
    pub file_md5: String,
    pub parent_id: String,
    pub file_name: String,
}

/// 提交上传响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitMultiUploadResp {
    pub file: CommitFile,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitFile {
    pub user_file_id: String,
    pub file_name: String,
    pub file_size: i64,
    pub file_md5: String,
    pub create_date: String,
}

/// 旧版上传创建响应（对应 CreateUploadFileResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUploadFileResp {
    pub upload_file_id: i64,
    pub file_upload_url: String,
    pub file_commit_url: String,
    pub file_data_exists: i32,
}

/// 批量任务信息（对应 BatchTaskInfo）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchTaskInfo {
    pub file_id: String,
    pub file_name: String,
    pub is_folder: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_parent_id: Option<String>,
}

impl BatchTaskInfo {
    pub fn new(file_id: &str, file_name: &str, is_folder: bool) -> Self {
        BatchTaskInfo {
            file_id: file_id.to_string(),
            file_name: file_name.to_string(),
            is_folder: if is_folder { 1 } else { 0 },
            src_parent_id: None,
        }
    }
}

/// 批量任务创建响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBatchTaskResp {
    pub task_id: String,
}

/// 批量任务状态（对应 BatchTaskStateResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchTaskStateResp {
    pub failed_count: i32,
    pub process: i32,
    pub skip_count: i32,
    pub sub_task_count: i32,
    pub successed_count: i32,
    pub task_id: String,
    /// 1 初始化, 2 冲突, 3 执行中, 4 完成
    pub task_status: i32,
}

/// 容量信息（对应 CapacityResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityResp {
    pub cloud_capacity_info: CapacityInfo,
    pub family_capacity_info: CapacityInfo,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityInfo {
    pub free_size: i64,
    pub total_size: i64,
    pub used_size: i64,
}

/// 家庭云列表响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyListResp {
    pub family_info_resp: Vec<FamilyInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyInfo {
    pub family_id: i64,
    pub remark_name: String,
    pub count: i32,
    pub user_role: i32,
}

/// 下载链接响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadUrlResp {
    pub file_download_url: String,
}

/// 密码登录初始化响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptConfResp {
    pub data: EncryptConfData,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptConfData {
    pub pre: String,
    pub pre_domain: String,
    pub pub_key: String,
    pub up_sms_on: String,
}

/// 登录提交响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResp {
    pub msg: String,
    pub result: i32,
    pub to_url: String,
}

/// 会话刷新响应（对应 UserSessionResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSessionResp {
    #[serde(rename = "res_code")]
    pub res_code: Option<serde_json::Value>,
    #[serde(rename = "res_message")]
    pub res_message: String,
    pub login_name: String,
    pub keep_alive: i32,
    pub session_key: String,
    pub session_secret: String,
    pub family_session_key: String,
    pub family_session_secret: String,
}

/// 登录完整响应（对应 AppSessionResp）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSessionResp {
    #[serde(rename = "res_code")]
    pub res_code: Option<serde_json::Value>,
    #[serde(rename = "res_message")]
    pub res_message: String,
    pub login_name: String,
    pub access_token: String,
    pub refresh_token: String,
    pub session_key: String,
    pub session_secret: String,
    pub family_session_key: String,
    pub family_session_secret: String,
}

impl AppSessionResp {
    /// res_code 是否表示错误（0 或空为成功）
    pub fn has_error(&self) -> bool {
        match &self.res_code {
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
            Some(serde_json::Value::String(s)) => {
                !s.is_empty() && s != "0" && s.to_lowercase() != "success"
            }
            Some(_) | None => false,
        }
    }
}

/// 通用错误响应
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RespErr {
    #[serde(rename = "res_code")]
    pub res_code: Option<serde_json::Value>,
    #[serde(rename = "res_message")]
    pub res_message: String,
    pub code: String,
    pub msg: String,
    pub message: String,
    pub error_code: String,
    pub error_msg: String,
}

impl RespErr {
    pub fn has_error(&self) -> bool {
        if let Some(rc) = &self.res_code {
            match rc {
                serde_json::Value::Number(n) => return n.as_i64().unwrap_or(0) != 0,
                serde_json::Value::String(s) => return !s.is_empty(),
                _ => {}
            }
        }
        (!self.code.is_empty() && self.code != "SUCCESS")
            || !self.error_code.is_empty()
            || !self.msg.is_empty() && !self.msg.contains("SUCCESS")
    }

    pub fn message(&self) -> String {
        if !self.res_message.is_empty() {
            self.res_message.clone()
        } else if !self.msg.is_empty() {
            self.msg.clone()
        } else if !self.message.is_empty() {
            self.message.clone()
        } else if !self.error_msg.is_empty() {
            self.error_msg.clone()
        } else if !self.code.is_empty() {
            self.code.clone()
        } else if let Some(rc) = &self.res_code {
            rc.to_string()
        } else {
            "unknown error".to_string()
        }
    }
}

/// 搜索响应（searchFiles.action）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResp {
    #[serde(rename = "fileListAO")]
    pub file_list_ao: FileListAo,
}

/// 文件列表响应（listFiles.action）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cloud189FilesResp {
    #[serde(rename = "fileListAO")]
    pub file_list_ao: FileListAo,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListAo {
    pub count: i32,
    pub file_list: Vec<Cloud189FileRaw>,
    pub folder_list: Vec<Cloud189FolderRaw>,
}

/// 原始文件 JSON（对应 Cloud189File）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cloud189FileRaw {
    pub id: Cloud189Id,
    pub name: String,
    pub size: i64,
    pub md5: String,
    pub last_op_time: String,
    pub create_date: String,
    #[serde(default)]
    pub icon: Icon,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Icon {
    pub small_url: String,
    pub large_url: String,
    // 仅 iconOption=10 时返回；iconOption=5 的列表请求不含这些字段，故设为可选
    #[serde(rename = "max600")]
    pub max600: Option<String>,
    pub medium_url: Option<String>,
}

/// 原始文件夹 JSON（对应 Cloud189Folder）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cloud189FolderRaw {
    pub id: Cloud189Id,
    pub parent_id: i64,
    pub name: String,
    pub last_op_time: String,
    pub create_date: String,
}

/// 兼容 ID 可能是字符串或数字
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Cloud189Id {
    Str(String),
    Num(i64),
}

impl Default for Cloud189Id {
    fn default() -> Self {
        Cloud189Id::Str(String::new())
    }
}

impl Cloud189Id {
    pub fn as_string(&self) -> String {
        match self {
            Cloud189Id::Str(s) => s.clone(),
            Cloud189Id::Num(n) => n.to_string(),
        }
    }
}
