//! JSON 字段与人类可读表格
//!
//! 一个原则：**同一个事实只有一种表示**。JSON 里的 size 永远是整数字节，时间永远是
//! ISO-8601 字符串（原始字符串另存一份 raw 字段），进度永远是千分比整数——绝不出现
//! 浮点数、绝不出现 "1.2 MB" 这种只能给人看的字符串。表格才是负责好看的层。
//!
//! 表格宽度按东亚字符宽度计算（中文/全角算 2 列），否则中英混排会错位。

use serde_json::{json, Value};
use tianyi_core::file;
use tianyi_core::models::{FileObject, FolderObject};
use tianyi_core::service::{CapacityView, SessionInfo};
use tianyi_core::task::{Task, TaskDirection, TaskStatus};

/// 东亚宽字符宽度（够用即可：CJK、假名、全角标点、常见 emoji）
pub fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// 字符串显示宽度
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 左侧补空格到指定显示宽度
fn pad_right(s: &str, width: usize) -> String {
    let w = display_width(s);
    let mut out = String::from(s);
    for _ in w..width {
        out.push(' ');
    }
    out
}

/// 右侧补空格到指定显示宽度（数字列用）
fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    let mut out = String::new();
    for _ in w..width {
        out.push(' ');
    }
    out.push_str(s);
    out
}

/// 渲染一个简单表格（最后一列不补齐，避免行尾空格）
pub fn render_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let mut widths = vec![0usize; cols];
    for (i, h) in header.iter().enumerate() {
        widths[i] = display_width(h);
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
    }
    let mut out = String::new();
    if !header.is_empty() {
        let line: Vec<String> = header
            .iter()
            .enumerate()
            .map(|(i, h)| pad_right(h, widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                // 数字列右对齐更易读：这里按「全是数字/空格/小数点」判断
                if i + 1 == row.len() {
                    cell.clone()
                } else if cell
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ' ' || c == '-')
                {
                    pad_left(cell, widths[i])
                } else {
                    pad_right(cell, widths[i])
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// 服务端返回的时间（"2024-01-02 03:04:05"）转 ISO-8601
///
/// 天翼云给的是**没有时区**的服务器本地时间，因此这里只做格式归一（不臆造偏移量），
/// 原始字符串同时以 *_raw 字段输出，调用方需要精确比对时可自行解析。
pub fn iso_time(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"));
    match parsed {
        Ok(dt) => Some(dt.format("%Y-%m-%dT%H:%M:%S").to_string()),
        Err(_) => None,
    }
}

/// unix 秒转 ISO-8601（UTC）
pub fn iso_from_unix(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// 单个文件/目录的 JSON 表示
pub fn item_value(obj: &FileObject) -> Value {
    json!({
        "id": obj.id,
        "name": obj.name,
        "type": if obj.is_dir { "dir" } else { "file" },
        "size": obj.size,
        "modified": iso_time(&obj.last_op_time),
        "modified_raw": obj.last_op_time,
        "created": iso_time(&obj.create_date),
        "created_raw": obj.create_date,
        "md5": if obj.md5.is_empty() { Value::Null } else { Value::String(obj.md5.clone()) },
        "parent_id": obj.parent_id,
    })
}

/// 文件夹对象的 JSON 表示（新建目录的返回）
pub fn folder_value(folder: &FolderObject, path: &str) -> Value {
    json!({
        "id": folder.id,
        "name": folder.name,
        "path": path,
        "type": "dir",
    })
}

/// 目录列表的 JSON 表示（含分页信息）
pub fn list_value(
    path: &str,
    folder_id: &str,
    space: &str,
    items: &[FileObject],
    page: Page,
) -> Value {
    json!({
        "path": path,
        "folder_id": folder_id,
        "space": space,
        "items": items.iter().map(item_value).collect::<Vec<_>>(),
        "pagination": page.to_value(),
    })
}

/// 分页切片信息
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub page: u64,
    pub limit: usize,
    pub total: usize,
    pub returned: usize,
}

impl Page {
    /// 是否还有下一页
    pub fn has_more(&self) -> bool {
        self.page as usize * self.limit < self.total
    }

    pub fn to_value(self) -> Value {
        json!({
            "page": self.page,
            "limit": self.limit,
            "total": self.total,
            "returned": self.returned,
            "has_more": self.has_more(),
        })
    }
}

/// 人类可读的目录列表
pub fn render_items(path: &str, items: &[FileObject], long: bool) -> String {
    let mut out = format!("{path}\n");
    if items.is_empty() {
        out.push_str("（空目录）\n");
        return out;
    }
    if long {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|f| {
                vec![
                    if f.is_dir {
                        "d".to_string()
                    } else {
                        "-".to_string()
                    },
                    iso_time(&f.last_op_time)
                        .unwrap_or_else(|| "未知".to_string())
                        .replace('T', " "),
                    if f.is_dir {
                        "-".to_string()
                    } else {
                        file::format_size(f.size.max(0) as u64)
                    },
                    f.name.clone(),
                ]
            })
            .collect();
        out.push_str(&render_table(&["T", "修改时间", "大小", "名称"], &rows));
    } else {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|f| {
                vec![
                    if f.is_dir {
                        "d".to_string()
                    } else {
                        "-".to_string()
                    },
                    f.name.clone(),
                ]
            })
            .collect();
        out.push_str(&render_table(&["T", "名称"], &rows));
    }
    let dirs = items.iter().filter(|i| i.is_dir).count();
    out.push_str(&format!(
        "共 {} 项（{} 个目录，{} 个文件）\n",
        items.len(),
        dirs,
        items.len() - dirs
    ));
    out
}

/// 单个任务的 JSON 表示
pub fn task_value(t: &Task) -> Value {
    let total = t.file_size;
    let permille = (t.bytes_done.min(total) * 1000)
        .checked_div(total)
        .unwrap_or(0);
    json!({
        "id": t.id,
        "direction": match t.direction { TaskDirection::Upload => "upload", TaskDirection::Download => "download" },
        "name": t.file_name,
        "status": t.status.as_key(),
        "status_text": t.status.label_zh(),
        "size": t.file_size,
        "bytes_done": t.bytes_done,
        "progress_permille": permille,
        "speed": t.speed,
        "remote_id": t.remote_folder_id,
        "local_path": t.local_path.to_string_lossy(),
        "error": t.error,
        "created_at": iso_from_unix(t.created_at),
        "created_at_unix": t.created_at,
    })
}

/// 人类可读的任务列表
pub fn render_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "没有传输任务\n".to_string();
    }
    let rows: Vec<Vec<String>> = tasks
        .iter()
        .map(|t| {
            let total = t.file_size;
            let pct = (t.bytes_done.min(total) * 100)
                .checked_div(total)
                .map(|v| format!("{v}%"))
                .unwrap_or_else(|| "-".to_string());
            vec![
                t.id.to_string(),
                match t.direction {
                    TaskDirection::Upload => "上传".to_string(),
                    TaskDirection::Download => "下载".to_string(),
                },
                t.status.label_zh().to_string(),
                pct,
                if t.speed > 0 {
                    format!("{}/s", file::format_speed(t.speed))
                } else {
                    "-".to_string()
                },
                t.file_name.clone(),
            ]
        })
        .collect();
    render_table(&["ID", "方向", "状态", "进度", "速度", "名称"], &rows)
}

/// 容量的 JSON 表示
pub fn capacity_value(cap: &CapacityView) -> Value {
    let permille = ((cap.used.max(0) * 1000) as u64)
        .checked_div(cap.total.max(0) as u64)
        .unwrap_or(0);
    json!({
        "space": cap.space.as_str(),
        "total": cap.total,
        "used": cap.used,
        "free": cap.free,
        "used_permille": permille,
    })
}

/// 容量的人类可读文本
pub fn render_capacity(cap: &CapacityView) -> String {
    let total = cap.total.max(0) as u64;
    let used = cap.used.max(0) as u64;
    let free = if cap.free > 0 {
        cap.free as u64
    } else {
        total.saturating_sub(used)
    };
    let pct = (used * 100).checked_div(total).unwrap_or(0);
    format!(
        "{}：已用 {} / 共 {}（{}%），剩余 {}\n",
        if cap.space == tianyi_core::service::Space::Family {
            "家庭云"
        } else {
            "个人云"
        },
        file::format_size(used),
        file::format_size(total),
        pct,
        file::format_size(free)
    )
}

/// 登录状态 JSON
pub fn status_value(
    info: &SessionInfo,
    version: &str,
    config_dir: &str,
    tasks_active: usize,
    tasks_finished: usize,
) -> Value {
    json!({
        "authenticated": info.authenticated,
        "expired": info.expired,
        "account": info.username,
        "space": info.space.as_str(),
        "family_id": info.family_id,
        "config_dir": config_dir,
        "version": version,
        "tasks": { "active": tasks_active, "finished": tasks_finished },
    })
}

/// 登录状态的人类可读文本
pub fn render_status(
    info: &SessionInfo,
    version: &str,
    config_dir: &str,
    tasks_active: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("版本：{version}\n"));
    if info.authenticated {
        out.push_str(&format!("账号：{}\n", info.username));
    } else if info.expired {
        out.push_str(&format!(
            "账号：{}（登录态已失效，请重新登录）\n",
            info.username
        ));
    } else if info.username.is_empty() {
        out.push_str("账号：未登录\n");
    } else {
        out.push_str(&format!("账号：{}（没有可用的登录态）\n", info.username));
    }
    out.push_str(&format!(
        "云空间：{}\n",
        if info.space == tianyi_core::service::Space::Family {
            "家庭云"
        } else {
            "个人云"
        }
    ));
    if !info.family_id.is_empty() {
        out.push_str(&format!("家庭云 ID：{}\n", info.family_id));
    }
    out.push_str(&format!("配置目录：{config_dir}\n"));
    out.push_str(&format!("进行中的任务：{tasks_active}\n"));
    out
}

/// 任务状态筛选
pub fn filter_tasks<'a>(tasks: &'a [Task], filter: &str) -> Vec<&'a Task> {
    match filter.trim().to_ascii_lowercase().as_str() {
        "active" | "running" => tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
                )
            })
            .collect(),
        "done" | "completed" => tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .collect(),
        "failed" | "error" => tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Canceled))
            .collect(),
        _ => tasks.iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, size: i64, is_dir: bool) -> FileObject {
        FileObject {
            id: format!("id-{name}"),
            name: name.to_string(),
            size,
            is_dir,
            last_op_time: "2024-05-06 07:08:09".to_string(),
            create_date: "2024-05-01 00:00:00".to_string(),
            md5: if is_dir {
                String::new()
            } else {
                "abc".to_string()
            },
            parent_id: "-11".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn item_json_has_stable_types() {
        let v = item_value(&item("a.zip", 1234, false));
        assert_eq!(v["type"], json!("file"));
        assert_eq!(v["size"], json!(1234));
        assert!(v["size"].is_i64(), "大小必须是整数字节，不能是字符串");
        assert_eq!(v["modified"], json!("2024-05-06T07:08:09"));
        assert_eq!(v["modified_raw"], json!("2024-05-06 07:08:09"));
        assert_eq!(v["md5"], json!("abc"));

        let d = item_value(&item("docs", 0, true));
        assert_eq!(d["type"], json!("dir"));
        assert!(d["md5"].is_null(), "目录没有 md5，必须是 null 而不是空串");
    }

    #[test]
    fn iso_time_never_invents_a_timezone() {
        assert_eq!(
            iso_time("2024-05-06 07:08:09").as_deref(),
            Some("2024-05-06T07:08:09")
        );
        assert_eq!(iso_time(""), None);
        assert_eq!(iso_time("not a time"), None);
    }

    #[test]
    fn pagination_reports_has_more_consistently() {
        let page = Page {
            page: 1,
            limit: 10,
            total: 25,
            returned: 10,
        };
        assert!(page.has_more());
        let v = page.to_value();
        assert_eq!(v["has_more"], json!(true));
        let last = Page {
            page: 3,
            limit: 10,
            total: 25,
            returned: 5,
        };
        assert!(!last.has_more());
    }

    #[test]
    fn table_pads_cjk_by_display_width() {
        // 「中文」占 4 列，「ab」占 2 列：补齐后两行必须等宽
        // 最后一列不补齐（避免行尾空格），所以用三列：前两列必须对齐到同样的位置
        let rows = vec![
            vec!["中文".to_string(), "1".to_string(), "same".to_string()],
            vec!["ab".to_string(), "2".to_string(), "same".to_string()],
        ];
        let out = render_table(&["名称", "值", "标记"], &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        let widths: Vec<usize> = lines.iter().map(|l| display_width(l)).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "中英混排必须等宽：{widths:?}
{out}"
        );
        assert!(out.contains("中文"));
    }

    #[test]
    fn long_listing_shows_size_and_time() {
        let items = vec![item("a.zip", 2048, false), item("docs", 0, true)];
        let out = render_items("/", &items, true);
        assert!(out.contains("a.zip"));
        assert!(out.contains("共 2 项（1 个目录，1 个文件）"));
        assert!(out.lines().count() >= 4);
    }

    #[test]
    fn capacity_json_has_no_floats() {
        let cap = CapacityView {
            total: 1000,
            used: 250,
            free: 750,
            space: tianyi_core::service::Space::Personal,
        };
        let v = capacity_value(&cap);
        assert_eq!(v["used_permille"], json!(250));
        assert!(v["total"].is_i64());
        let text = render_capacity(&cap);
        assert!(text.contains("25%"));
    }

    #[test]
    fn task_json_exposes_progress_as_integer_permille() {
        let t = Task {
            id: 7,
            direction: TaskDirection::Download,
            file_name: "a.bin".to_string(),
            file_size: 1000,
            local_path: std::path::PathBuf::from("/tmp/a.bin"),
            remote_folder_id: "fid".to_string(),
            status: TaskStatus::Running,
            progress: 25.0,
            bytes_done: 250,
            speed: 1024,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: 1_700_000_000,
        };
        let v = task_value(&t);
        assert_eq!(v["progress_permille"], json!(250));
        assert_eq!(v["status"], json!("running"));
        assert_eq!(v["direction"], json!("download"));
        assert_eq!(v["created_at"], json!("2023-11-14T22:13:20Z"));
        let text = render_tasks(&[t]);
        assert!(text.contains("下载"));
        assert!(text.contains("25%"));
    }

    #[test]
    fn status_text_distinguishes_logged_out_from_expired() {
        let anonymous = SessionInfo::default();
        let out = render_status(&anonymous, "0.1.0", "/tmp/cfg", 0);
        assert!(out.contains("未登录"));

        let expired = SessionInfo {
            authenticated: false,
            expired: true,
            username: "u".to_string(),
            ..Default::default()
        };
        let out = render_status(&expired, "0.1.0", "/tmp/cfg", 0);
        assert!(out.contains("失效"), "过期与未登录必须是两种文案");
    }
}
