//! 文件操作命令：ls / info / search / mkdir / rename / mv / cp / rm / link / quota / space
//!
//! 所有命令都只做三件事：解析路径 → 调共享服务 → 把结果拼成 (JSON, 人类可读文本)。
//! 业务语义（分页、递归、家庭云限制、错误分类）都在 tianyi_core::service 里，这里不重复。

use crate::args::{LsArgs, MoveArgs, PathArg, RenameArgs, RmArgs, SearchArgs, SpaceArgs};
use crate::cmd::Runner;
use crate::error::{CliError, CliResult, ErrorCode};
use crate::output::Output;
use crate::table;
use serde_json::json;
use tianyi_core::models::FileObject;
use tianyi_core::service::Space;

/// 默认每页条数（人类模式看一屏足够；机器模式建议显式 --all 或 --limit）
const DEFAULT_LIMIT: usize = 200;

/// 计算分页切片
fn page_slice(all: bool, page: u64, limit: Option<usize>, total: usize) -> (usize, usize, usize) {
    if all {
        return (0, total, total.max(1));
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let page = page.max(1);
    let start = ((page - 1) as usize).saturating_mul(limit).min(total);
    let end = (start + limit).min(total);
    (start, end, limit)
}

/// ls
pub async fn ls(r: &mut Runner, args: LsArgs) -> CliResult<Output> {
    let path = r.ctx.resolve_path(args.path.as_deref().unwrap_or("."))?;
    let list = r.ctx.service.list(&path).await?;
    let total = list.items.len();
    let (start, end, limit) = page_slice(args.all, args.page, args.limit, total);
    let items: Vec<FileObject> = list.items[start..end].to_vec();
    let page = table::Page {
        page: args.page.max(1),
        limit,
        total,
        returned: items.len(),
    };
    let data = table::list_value(
        &path.display(),
        &list.folder_id,
        r.ctx.service.space().as_str(),
        &items,
        page,
    );
    let mut human = table::render_items(&path.display(), &items, args.long);
    if !args.all && total > items.len() {
        human.push_str(&format!(
            "（第 {} 页，每页 {} 条，共 {} 条；用 --all 看全部）\n",
            page.page, limit, total
        ));
    }
    Ok(Output::new(data, human))
}

/// info
pub async fn info(r: &mut Runner, args: PathArg) -> CliResult<Output> {
    let path = r.ctx.resolve_path(&args.path)?;
    let obj = r.ctx.service.info(&path).await?;
    let data = table::item_value(&obj);
    let mut human = String::new();
    human.push_str(&format!("名称：{}\n", obj.name));
    human.push_str(&format!("路径：{}\n", path.display()));
    human.push_str(&format!(
        "类型：{}\n",
        if obj.is_dir { "目录" } else { "文件" }
    ));
    human.push_str(&format!("ID：{}\n", obj.id));
    if !obj.is_dir {
        human.push_str(&format!(
            "大小：{}（{} 字节）\n",
            tianyi_core::file::format_size(obj.size.max(0) as u64),
            obj.size.max(0)
        ));
        if !obj.md5.is_empty() {
            human.push_str(&format!("MD5：{}\n", obj.md5));
        }
    }
    if let Some(t) = table::iso_time(&obj.last_op_time) {
        human.push_str(&format!("修改时间：{t}\n"));
    }
    Ok(Output::new(data, human))
}

/// search
pub async fn search(r: &mut Runner, args: SearchArgs) -> CliResult<Output> {
    let items = r.ctx.service.search(&args.keyword).await?;
    let total = items.len();
    let (start, end, limit) = page_slice(args.all, args.page, args.limit, total);
    let page_items: Vec<FileObject> = items[start..end].to_vec();
    let page = table::Page {
        page: args.page.max(1),
        limit,
        total,
        returned: page_items.len(),
    };
    let data = json!({
        "keyword": args.keyword,
        "space": r.ctx.service.space().as_str(),
        "items": page_items.iter().map(table::item_value).collect::<Vec<_>>(),
        "pagination": page.to_value(),
    });
    let mut human = format!("搜索「{}」：\n", args.keyword);
    if page_items.is_empty() {
        human.push_str("（没有匹配项）\n");
    } else {
        let rows: Vec<Vec<String>> = page_items
            .iter()
            .map(|f| {
                vec![
                    if f.is_dir {
                        "d".to_string()
                    } else {
                        "-".to_string()
                    },
                    tianyi_core::file::format_size(f.size.max(0) as u64),
                    f.name.clone(),
                ]
            })
            .collect();
        human.push_str(&table::render_table(&["T", "大小", "名称"], &rows));
        human.push_str(&format!("共 {} 条匹配\n", total));
    }
    Ok(Output::new(data, human))
}

/// mkdir
pub async fn mkdir(r: &mut Runner, args: PathArg) -> CliResult<Output> {
    let path = r.ctx.resolve_path(&args.path)?;
    let folder = r.ctx.service.mkdir(&path).await?;
    let data = table::folder_value(&folder, &path.display());
    Ok(Output::new(
        data,
        format!("已创建目录 {}\n", path.display()),
    ))
}

/// rename
pub async fn rename(r: &mut Runner, args: RenameArgs) -> CliResult<Output> {
    let path = r.ctx.resolve_path(&args.path)?;
    r.ctx.service.rename(&path, &args.new_name).await?;
    let mut after = path.clone();
    if let Some(parent) = path.parent() {
        after = parent.child(args.new_name.trim());
    }
    let data = json!({
        "path": path.display(),
        "new_name": args.new_name.trim(),
        "new_path": after.display(),
    });
    Ok(Output::new(
        data,
        format!("已重命名：{} → {}\n", path.display(), after.display()),
    ))
}

/// mv / cp 的共同实现
async fn transfer_between(r: &mut Runner, args: &MoveArgs, copy: bool) -> CliResult<Output> {
    let (srcs, dst) = args.split();
    if srcs.is_empty() {
        return Err(CliError::usage("至少需要一个源路径"));
    }
    let dst_path = r.ctx.resolve_path(dst)?;
    let mut done: Vec<serde_json::Value> = Vec::new();
    for src in &srcs {
        let src_path = r.ctx.resolve_path(src)?;
        let obj = if copy {
            r.ctx.service.copy_to(&src_path, &dst_path).await?
        } else {
            r.ctx.service.move_to(&src_path, &dst_path).await?
        };
        let target = dst_path.child(&obj.name);
        done.push(json!({
            "from": src_path.display(),
            "to": target.display(),
            "name": obj.name,
            "type": if obj.is_dir { "dir" } else { "file" },
        }));
    }
    let verb = if copy { "复制" } else { "移动" };
    let human = format!(
        "已{verb} {} 项到 {}\n{}",
        done.len(),
        dst_path.display(),
        done.iter()
            .map(|v| format!(
                "  {} → {}\n",
                v["from"].as_str().unwrap_or_default(),
                v["to"].as_str().unwrap_or_default()
            ))
            .collect::<String>()
    );
    Ok(Output::new(
        json!({ "destination": dst_path.display(), "items": done }),
        human,
    ))
}

/// mv
pub async fn mv(r: &mut Runner, args: MoveArgs) -> CliResult<Output> {
    transfer_between(r, &args, false).await
}

/// cp
pub async fn cp(r: &mut Runner, args: MoveArgs) -> CliResult<Output> {
    transfer_between(r, &args, true).await
}

/// rm
///
/// 安全要点（对应「不允许在解析失败或路径歧义时误删」）：
/// 1. 先把**所有**路径解析成对象，任何一个解析失败就整体放弃，一个字节都不删；
/// 2. 目录必须显式 --recursive；
/// 3. 批量删除只确认一次，且确认发生在第一次删除之前。
pub async fn rm(r: &mut Runner, args: RmArgs) -> CliResult<Output> {
    let mut targets = Vec::new();
    for raw in &args.paths {
        let path = r.ctx.resolve_path(raw)?;
        let obj = r.ctx.service.resolve_object(&path).await?;
        if obj.is_dir && !args.recursive {
            return Err(CliError::new(
                ErrorCode::Usage,
                format!("{} 是目录，删除目录需要显式 --recursive", path.display()),
            ));
        }
        targets.push((path, obj));
    }
    let question = format!(
        "确认删除以下 {} 项？（目录会连同内容一起删除，删除后可回收站找回）",
        targets.len()
    );
    if !r.ctx.confirm(&question)? {
        return Err(CliError::new(ErrorCode::Canceled, "已取消删除"));
    }
    let mut deleted = Vec::new();
    for (path, obj) in &targets {
        match r.ctx.service.remove(path, args.recursive).await {
            Ok(removed) => deleted.push(json!({
                "path": path.display(),
                "id": removed.id,
                "type": if removed.is_dir { "dir" } else { "file" },
            })),
            Err(e) => {
                // 已经删掉的那些如实回报，别让调用方以为「一个都没动」
                return Err(CliError::from(e).with_details(json!({
                    "deleted": deleted,
                    "failed": path.display(),
                })));
            }
        }
        let _ = obj;
    }
    let human = format!(
        "已删除 {} 项：\n{}",
        deleted.len(),
        deleted
            .iter()
            .map(|v| format!("  {}\n", v["path"].as_str().unwrap_or_default()))
            .collect::<String>()
    );
    Ok(Output::new(json!({ "deleted": deleted }), human))
}

/// link
pub async fn link(r: &mut Runner, args: PathArg) -> CliResult<Output> {
    let path = r.ctx.resolve_path(&args.path)?;
    let url = r.ctx.service.download_url(&path).await?;
    Ok(Output::new(
        json!({ "path": path.display(), "url": url.clone() }),
        format!("{url}\n"),
    ))
}

/// quota
pub async fn quota(r: &mut Runner, _args: ()) -> CliResult<Output> {
    let cap = r.ctx.service.capacity().await?;
    Ok(Output::new(
        table::capacity_value(&cap),
        table::render_capacity(&cap),
    ))
}

/// space：不带参数只显示，带参数则切换
pub async fn space(r: &mut Runner, args: SpaceArgs) -> CliResult<Output> {
    let current = r.ctx.service.space();
    let Some(want) = args.space else {
        return Ok(Output::new(
            json!({ "space": current.as_str() }),
            format!("当前云空间：{}\n", current.as_str()),
        ));
    };
    let want =
        Space::parse(&want).ok_or_else(|| CliError::usage("space 只接受 personal 或 family"))?;
    r.ctx.service.switch_space(want).await?;
    let info = r.ctx.service.session();
    Ok(Output::new(
        json!({
            "space": want.as_str(),
            "family_id": info.family_id,
        }),
        format!("已切换到 {}\n", want.as_str()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_defaults_to_first_page() {
        let (start, end, limit) = page_slice(false, 1, None, 500);
        assert_eq!((start, end, limit), (0, DEFAULT_LIMIT, DEFAULT_LIMIT));
    }

    #[test]
    fn paging_clamps_out_of_range_pages() {
        let (start, end, _) = page_slice(false, 99, Some(10), 25);
        assert_eq!((start, end), (25, 25), "越界页应返回空切片而不是 panic");
    }

    #[test]
    fn all_ignores_page_and_limit() {
        let (start, end, _) = page_slice(true, 5, Some(10), 7);
        assert_eq!((start, end), (0, 7));
    }

    #[test]
    fn zero_page_is_treated_as_first() {
        let (start, end, _) = page_slice(false, 0, Some(10), 25);
        assert_eq!((start, end), (0, 10));
    }
}
