//! 无窗口 UI 渲染器（视觉回归用）
//!
//! 用 Slint 的软件渲染器把真实组件树渲染成 PNG，不需要窗口系统，
//! 因此可以在 CI / 无桌面环境里核对布局：
//!
//! ```text
//! cargo run -p tianyi-app --example ui_preview -- <输出目录>
//! ```
//!
//! 渲染的是 `ui/` 下的真实界面与真实组件，只是数据来自本文件里的演示数据。
//! 演示数据只在本进程内存在，不读写配置、不发起任何网络请求。

#[path = "../src/app.rs"]
mod app;

use app::{CrumbEntry, DirectLink, FileEntry, MainWindow, Theme, TransferEntry};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, ModelRc, PhysicalSize, Rgb8Pixel, SharedString, VecModel};
use std::rc::Rc;
use std::time::{Duration, Instant};

struct PreviewPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
}

impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

/// 构造一条演示文件条目
///
/// 只给"数据源里真实存在"的字段：大小文案和类型文案与正式实现一样由 size/kind 推导，
/// 避免演示数据和真实渲染路径不一致。
fn entry(id: &str, name: &str, size: i64, is_dir: bool, modified: &str, kind: &str) -> FileEntry {
    let kind_text = match kind {
        "folder" => "文件夹",
        "office" => "文档",
        "image" => "图片",
        "video" => "视频",
        "archive" => "压缩包",
        "text" => "文本",
        _ => "其他",
    };
    let size_text = if is_dir {
        "—".to_string()
    } else if size >= 1024 * 1024 {
        format!("{:.2} MB", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", size as f64 / 1024.0)
    };
    FileEntry {
        id: SharedString::from(id),
        name: SharedString::from(name),
        size: size as i32,
        size_text: SharedString::from(size_text),
        is_dir,
        thumb: SharedString::default(),
        modified: SharedString::from(modified),
        kind: SharedString::from(kind),
        kind_text: SharedString::from(kind_text),
        selected: false,
    }
}

fn demo_files() -> Vec<FileEntry> {
    vec![
        entry("d1", "工作文档", 0, true, "2024/05/06 12:03", "folder"),
        entry("d2", "照片备份", 0, true, "2024/04/18 09:12", "folder"),
        entry(
            "f1",
            "2024年度产品规划与季度目标拆解_v3_final.docx",
            2_527_232,
            false,
            "2024/05/06 11:40",
            "office",
        ),
        entry(
            "f2",
            "屏幕截图 2024-05-01 152233.png",
            1_132_026,
            false,
            "2024/05/01 15:22",
            "image",
        ),
        entry(
            "f3",
            "产品演示视频.mp4",
            134_637_158,
            false,
            "2024/04/29 20:05",
            "video",
        ),
        entry(
            "f4",
            "客户名单.xlsx",
            37_069,
            false,
            "2024/04/27 10:31",
            "office",
        ),
        entry(
            "f5",
            "日志归档.zip",
            524_288,
            false,
            "2024/04/20 08:00",
            "archive",
        ),
        entry("f6", "README.md", 4_301, false, "2024/04/11 17:45", "text"),
    ]
}

fn demo_transfers() -> Vec<TransferEntry> {
    vec![
        TransferEntry {
            id: 1,
            name: "产品演示视频.mp4".into(),
            direction: "download".into(),
            size_text: "128.4 MB".into(),
            progress: 42.0,
            status: "running".into(),
            status_text: "传输中".into(),
            speed: "3.42 MB/s".into(),
            detail: "54.2 MB / 128.4 MB · 剩余 22 秒".into(),
            error: SharedString::default(),
        },
        TransferEntry {
            id: 2,
            name: "季度汇报素材".into(),
            direction: "upload".into(),
            size_text: "18.6 MB".into(),
            progress: 100.0,
            status: "completed".into(),
            status_text: "已完成".into(),
            speed: SharedString::default(),
            detail: "18.6 MB / 18.6 MB".into(),
            error: SharedString::default(),
        },
        TransferEntry {
            id: 3,
            name: "客户名单.xlsx".into(),
            direction: "upload".into(),
            size_text: "36.2 KB".into(),
            progress: 61.0,
            status: "paused".into(),
            status_text: "已暂停".into(),
            speed: SharedString::default(),
            detail: "22.1 KB / 36.2 KB".into(),
            error: SharedString::default(),
        },
        TransferEntry {
            id: 4,
            name: "离线安装包.exe".into(),
            direction: "download".into(),
            size_text: "1.21 GB".into(),
            progress: 12.0,
            status: "failed".into(),
            status_text: "失败".into(),
            speed: SharedString::default(),
            detail: "148 MB / 1.21 GB".into(),
            error: "网络中断：连接被对端重置".into(),
        },
    ]
}

fn crumb(id: &str, name: &str) -> CrumbEntry {
    CrumbEntry {
        id: id.into(),
        name: name.into(),
    }
}

/// 渲染一张 PNG
fn shoot(
    ui: &MainWindow,
    window: &MinimalSoftwareWindow,
    dir: &std::path::Path,
    name: &str,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    window.set_size(PhysicalSize::new(width, height));
    // 让 90~180ms 的过渡动画跑完，避免截到动画中间态
    std::thread::sleep(Duration::from_millis(260));
    slint::platform::update_timers_and_animations();

    let mut buffer = slint::SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    let stride = width as usize;
    let drawn = window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), stride);
    });
    if !drawn {
        // 没有脏区域时也要出一张完整的图
        let renderer = slint::platform::software_renderer::SoftwareRenderer::new();
        let _ = &renderer;
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(buffer.make_mut_slice(), stride);
        });
    }
    let _ = ui;

    let mut img = image::RgbaImage::new(width, height);
    for (i, px) in buffer.as_slice().iter().enumerate() {
        let x = (i % stride) as u32;
        let y = (i / stride) as u32;
        img.put_pixel(x, y, image::Rgba([px.r, px.g, px.b, 255]));
    }
    let path = dir.join(format!("{name}.png"));
    img.save(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("screenshots/after"));
    std::fs::create_dir_all(&out)?;

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform {
        window: window.clone(),
        start: Instant::now(),
    }))?;

    let ui = MainWindow::new()?;
    ui.show()?;

    // ---------------- 静态数据 ----------------
    ui.set_logged_in(true);
    ui.set_account_name(SharedString::from("19051035392@189.cn"));
    ui.set_account_initial(SharedString::from("1"));
    ui.set_version_text(SharedString::from("0.1.0"));
    ui.set_config_dir(SharedString::from(
        r"C:\Users\Administrator\AppData\Roaming\tianyi-cloud\config",
    ));
    ui.set_download_dir(SharedString::from(r"D:\Downloads\天翼云盘\2024 年度归档"));
    ui.set_upload_thread(SharedString::from("3"));
    ui.set_download_thread(SharedString::from("4"));
    ui.set_total_space(SharedString::from("4.00 TB"));
    ui.set_used_space(SharedString::from("1.62 TB"));
    ui.set_capacity_ratio(0.405);
    ui.set_crumbs(ModelRc::new(VecModel::from(vec![
        crumb("-11", "个人云"),
        crumb("d9", "工作文档"),
        crumb("d1", "2024 年度"),
    ])));
    ui.set_status_text(SharedString::from("8 个项目"));
    ui.set_sort_key(0);
    ui.set_sort_asc(true);
    ui.set_transfer_list(ModelRc::new(VecModel::from(demo_transfers())));
    ui.set_active_transfer_list(ModelRc::new(VecModel::from(demo_transfers())));
    ui.set_active_transfers(3);
    ui.set_finished_transfers(1);

    // ---------------- 1. 个人云（浅色，含数据） ----------------
    ui.set_current_page(0);
    ui.set_list_state(SharedString::from("ready"));
    ui.set_file_list(ModelRc::new(VecModel::from(demo_files())));
    shoot(&ui, &window, &out, "01-cloud-light", 1180, 760)?;

    // ---------------- 2. 个人云（深色） ----------------
    ui.global::<Theme>().set_dark(true);
    shoot(&ui, &window, &out, "02-cloud-dark", 1180, 760)?;
    ui.global::<Theme>().set_dark(false);

    // ---------------- 3. 多选态 ----------------
    let mut files = demo_files();
    files[2].selected = true;
    files[3].selected = true;
    files[4].selected = true;
    ui.set_file_list(ModelRc::new(VecModel::from(files)));
    ui.set_selected_count(3);
    ui.set_status_text(SharedString::from("已选择 3 项"));
    shoot(&ui, &window, &out, "03-cloud-multiselect", 1180, 760)?;

    // ---------------- 4. 空目录 ----------------
    ui.set_file_list(ModelRc::new(VecModel::from(Vec::<FileEntry>::new())));
    ui.set_selected_count(0);
    ui.set_list_state(SharedString::from("empty"));
    ui.set_status_text(SharedString::from("0 个项目"));
    shoot(&ui, &window, &out, "04-cloud-empty", 1180, 760)?;

    // ---------------- 5. 空目录（最窄窗口） ----------------
    shoot(&ui, &window, &out, "05-cloud-empty-900x620", 900, 620)?;

    // ---------------- 6. 加载态 ----------------
    ui.set_list_state(SharedString::from("loading"));
    ui.set_status_text(SharedString::from("正在加载目录…"));
    shoot(&ui, &window, &out, "06-cloud-loading", 1180, 760)?;

    // ---------------- 7. 错误态 ----------------
    ui.set_list_state(SharedString::from("error"));
    ui.set_file_error(SharedString::from(
        "加载目录失败：网络连接超时，请检查网络后重试",
    ));
    ui.set_status_text(SharedString::from("加载目录失败：网络连接超时"));
    shoot(&ui, &window, &out, "07-cloud-error", 1180, 760)?;

    // ---------------- 8. 无搜索结果 ----------------
    ui.set_list_state(SharedString::from("empty-search"));
    ui.set_searching(true);
    ui.set_search_keyword(SharedString::from("合同扫描件"));
    ui.set_status_text(SharedString::from("“合同扫描件”的搜索结果：0 项"));
    shoot(&ui, &window, &out, "08-cloud-search-empty", 1180, 760)?;

    // ---------------- 9. 传输任务 ----------------
    ui.set_searching(false);
    ui.set_current_page(2);
    ui.set_status_text(SharedString::from("1 个任务进行中"));
    shoot(&ui, &window, &out, "09-transfers", 1180, 760)?;

    // ---------------- 10. 传输任务（深色） ----------------
    ui.global::<Theme>().set_dark(true);
    shoot(&ui, &window, &out, "10-transfers-dark", 1180, 760)?;
    ui.global::<Theme>().set_dark(false);

    // ---------------- 11. 下载直链（空） ----------------
    ui.set_current_page(3);
    shoot(&ui, &window, &out, "11-links-empty", 1180, 760)?;

    // ---------------- 12. 下载直链（有数据） ----------------
    ui.set_links(ModelRc::new(VecModel::from(vec![
        DirectLink {
            name: "产品演示视频.mp4".into(),
            url: "https://download.cloud.189.cn/file/downloadFile.action?ct=...".into(),
        },
        DirectLink {
            name: "客户名单.xlsx".into(),
            url: "https://download.cloud.189.cn/file/downloadFile.action?ct=...".into(),
        },
    ])));
    shoot(&ui, &window, &out, "12-links", 1180, 760)?;

    // ---------------- 13. 设置 ----------------
    ui.set_current_page(4);
    ui.set_theme_index(0);
    ui.set_reduce_motion(false);
    shoot(&ui, &window, &out, "13-settings", 1180, 760)?;

    // ---------------- 14. 设置（超宽窗口） ----------------
    shoot(&ui, &window, &out, "14-settings-1920x1080", 1920, 1080)?;

    // ---------------- 15. 设置（深色 + 窄窗口） ----------------
    ui.global::<Theme>().set_dark(true);
    shoot(&ui, &window, &out, "15-settings-dark-1024x768", 1024, 768)?;
    ui.global::<Theme>().set_dark(false);

    // ---------------- 16. 删除确认对话框 ----------------
    ui.set_current_page(0);
    ui.set_list_state(SharedString::from("ready"));
    ui.set_file_list(ModelRc::new(VecModel::from(demo_files())));
    ui.set_dialog_title(SharedString::from("确认删除 3 个项目"));
    ui.set_dialog_message(SharedString::from(
        "确定要删除选中的 3 个项目吗？此操作无法恢复。",
    ));
    ui.set_dialog_confirm_text(SharedString::from("删除"));
    ui.set_dialog_danger(true);
    ui.set_dialog_open(true);
    shoot(&ui, &window, &out, "16-dialog", 1180, 760)?;
    ui.set_dialog_open(false);

    // ---------------- 17. Toast 提示 ----------------
    ui.set_notice_text(SharedString::from("已删除 3 项"));
    ui.set_notice_kind(SharedString::from("success"));
    shoot(&ui, &window, &out, "17-toast", 1180, 760)?;
    ui.set_notice_text(SharedString::default());

    // ---------------- 18. 传输抽屉展开 ----------------
    ui.set_transfer_panel_open(true);
    shoot(&ui, &window, &out, "18-transfer-drawer", 1180, 760)?;
    ui.set_transfer_panel_open(false);

    // ---------------- 19. 登录页 ----------------
    ui.set_logged_in(false);
    ui.set_status_text(SharedString::default());
    shoot(&ui, &window, &out, "19-login", 1180, 760)?;

    // ---------------- 20. 登录页（最窄窗口 + 验证码 + 错误） ----------------
    ui.set_show_captcha(true);
    ui.set_login_error(SharedString::from(
        "账号或密码不正确，请检查后重试（剩余尝试次数 4）",
    ));
    ui.set_saved_accounts(ModelRc::new(VecModel::from(vec![
        SharedString::from("19051035392@189.cn"),
        SharedString::from("demo.user@189.cn"),
    ])));
    shoot(&ui, &window, &out, "20-login-narrow-captcha", 900, 620)?;

    // ---------------- 21. 扫码登录 ----------------
    ui.set_login_error(SharedString::default());
    ui.set_show_captcha(false);
    ui.set_qr_mode(true);
    ui.set_qr_state(SharedString::from("waiting"));
    ui.set_qr_status(SharedString::from("等待扫码…"));
    shoot(&ui, &window, &out, "21-login-qr", 1180, 760)?;

    // ---------------- 22. 文件右键菜单（菜单项图标覆盖） ----------------
    // 上下文菜单里的图标只有在右键后才出现，普通页面截图覆盖不到，
    // 这里直接把菜单状态置起来渲染一张。
    ui.set_qr_mode(false);
    ui.set_logged_in(true);
    ui.set_current_page(0);
    ui.set_list_state(SharedString::from("ready"));
    ui.set_file_list(ModelRc::new(VecModel::from(demo_files())));
    ui.set_status_text(SharedString::from("8 个项目"));
    ui.set_context_item_id(SharedString::from("f1"));
    ui.set_context_item_name(SharedString::from(
        "2024年度产品规划与季度目标拆解_v3_final.docx",
    ));
    ui.set_context_is_dir(false);
    ui.set_context_x(520.0);
    ui.set_context_y(240.0);
    ui.set_context_open(true);
    shoot(&ui, &window, &out, "22-context-menu", 1180, 760)?;
    ui.set_context_open(false);

    Ok(())
}
