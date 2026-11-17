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

    let mut buffer = slint::SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    let stride = width as usize;

    // 把入场动效推到静止再采图。
    //
    // 顺序有两个硬要求，写反了就会采到"动画没开始"的空白帧：
    //  1. **先真绘一帧**：新建元素（对话框/Toast/菜单/抽屉…）的动画属性是惰性求值的，
    //     只有在绘制时才读到，这一帧把起点（透明度 0 / 高度 0）落到实际像素上；
    //  2. 再"等一帧 → 推进定时器 → 绘制一帧 → 判静止"。配方 A 的 1ms 单次 Timer 只有在
    //     时间真的往前走之后才触发，而 opacity 这类非布局属性也要靠绘制才会被求值、
    //     动画才会注册。只推进时间不绘制，动画永远不会注册。
    // 退出条件用"没有活跃动画"，另加一个墙钟上限兜底：spinner / 不确定进度条这类
    // animation-tick() 驱动的循环动画永远不会"静止"，不设上限就会一直转。
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), stride);
    });

    let deadline = Instant::now() + Duration::from_millis(1000);
    loop {
        std::thread::sleep(Duration::from_millis(8));
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(buffer.make_mut_slice(), stride);
        });
        if !ui.window().has_active_animations() || Instant::now() > deadline {
            break;
        }
    }
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

/// 合成一张验证码位图（120×40，3:1），让登录页的验证码区域在预览里真正可见。
///
/// 真实验证码由服务端下发，预览不联网；这里只提供一张同比例的噪点图。
/// 存在的意义是"让缺陷可见"：此前场景 20 只打开 `show-captcha` 却没给位图，
/// 容器里是空的，于是"验证码被非等比拉伸"这件事在视觉基线里完全隐形——
/// 缺陷能在上一轮 UI 重构中活下来，正是因为这个洞。
fn captcha_demo_image() -> slint::Image {
    const W: u32 = 120;
    const H: u32 = 40;
    let mut img = image::RgbaImage::new(W, H);
    for (x, y, px) in img.enumerate_pixels_mut() {
        // 对角条纹：一旦被横向拉伸，条纹斜率会明显变平，肉眼即可判断宽高比
        let v = (((x * 3 + y * 5) % 180) + 40) as u8;
        *px = image::Rgba([v, 255 - v, 160, 255]);
    }
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&img.into_raw(), W, H);
    slint::Image::from_rgba8(buffer)
}

/// 真绘一帧，不落盘。
///
/// 存在的理由只有一个：让**新建的元素子树**（`if` 分支）、连同它内部的
/// `changed xxx => { … }` 处理器一起实例化。Slint 的元素树是惰性的，`if` 的分支
/// 要等到下一次遍历/绘制才真正建出来；在它建出来之前写某个 `in property`，
/// 新实例看到的初值就是"已经是新值"，于是 `changed` 永远不触发。
///
/// 具体踩到的坑：场景 25 先 `set_logged_in(false)`（销毁并等待重建登录页），
/// 紧接着写 `login-error`。如果没有中间这一帧，重建出来的登录页初值就带着错误文案，
/// `changed login-error` 一声不响，两个输入框的红框（S-03 的 `invalid`）根本不出现。
fn render_frame(window: &MinimalSoftwareWindow, width: u32, height: u32) {
    window.set_size(PhysicalSize::new(width, height));
    let mut buffer = slint::SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), width as usize);
    });
    slint::platform::update_timers_and_animations();
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
    // 打包了语言包之后，Slint 会在首个组件创建时按系统 locale 自动选包（本机 zh-CN 的
    // 主标签 zh 会命中 zh-Hant），预览图会莫名其妙变成繁体。这里显式钉回索引 0（空串
    // = 简中原文）。example 是独立 crate，拿不到 `crate::i18n`，只能直接调 slint 的接口。
    let _ = slint::select_bundled_translation("");
    // 允许采其它语种的截图：TIANYI_PREVIEW_LANG=en|zh-Hant|ja（有值就覆盖上面的简中）
    if let Ok(lang) = std::env::var("TIANYI_PREVIEW_LANG") {
        let _ = slint::select_bundled_translation(&lang);
    }
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
    // 传输任务的三处计数与两张列表，全部由 `demo_transfers()` 推导。
    //
    // 演示数据也必须自洽，否则基线图上的矛盾数字会被当成真 bug：这里曾经写死
    // `active_transfers = 3` / `finished_transfers = 1`，「进行中的传输」抽屉却装的是
    // **全部四条**任务（含已完成与失败），而场景 9 的状态栏文案又另写死一句
    // 「1 个任务进行中」——同一批基线图上并排出现三个互相矛盾的数字，复查的人只能
    // 靠记忆区分"哪个是假的"。
    //
    // 口径与控制器保持一致（`controller/transfers.rs`：`active_count` = Pending |
    // Running | Paused，finished = Completed，Failed 既不算进行中也不算已结束）。
    // 现在改演示数据只需要改 `demo_transfers()` 一处。
    let is_active = |s: &str| matches!(s, "pending" | "running" | "paused");
    let demo_rows = demo_transfers();
    let active_rows: Vec<TransferEntry> = demo_rows
        .iter()
        .filter(|t| is_active(t.status.as_str()))
        .cloned()
        .collect();
    let active_count = active_rows.len() as i32;
    let finished_count = demo_rows
        .iter()
        .filter(|t| t.status.as_str() == "completed")
        .count() as i32;
    ui.set_transfer_list(ModelRc::new(VecModel::from(demo_rows)));
    ui.set_active_transfer_list(ModelRc::new(VecModel::from(active_rows)));
    ui.set_active_transfers(active_count);
    ui.set_finished_transfers(finished_count);

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
    // 文案跟随计数（`active_count` 由演示数据算出来）：写死一句文案就会与导航徽章、
    // 页面页脚「进行中 N」各说一套，而那三处本来就该是同一个数。
    ui.set_status_text(SharedString::from(format!("{active_count} 个任务进行中")));
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
    ui.set_captcha_image(captcha_demo_image());
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

    // ---------------- 23. 最小窗口形态的批量操作栏（图标按钮 + Tooltip 兜底） ----------------
    // 900px 是能触发图标档的窗口区间（900–955px）里最靠边的一档：导航折叠成 56px，
    // 内容区 844px < `LayoutMetrics.min-capability-width`（900px），批量操作按钮把文字
    // 清空只留图标，可读名称与悬停说明只剩 `tooltip:` 一条通道——D-01 的缺陷就死在这里。
    // （旧基线的这一档是 960×760：那是"导航展开吃掉 248px、内容区剩 712px"的旧断点
    // 才会出现的窗口区间，也正是 D-11 里"窗口变宽能力反而变窄"的那一段。）
    let mut narrow_rows = demo_files();
    narrow_rows.truncate(4);
    for row in narrow_rows.iter_mut().skip(1) {
        row.selected = true;
    }
    ui.set_current_page(0);
    ui.set_list_state(SharedString::from("ready"));
    ui.set_file_list(ModelRc::new(VecModel::from(narrow_rows)));
    ui.set_selected_count(3);
    ui.set_status_text(SharedString::from("已选择 3 项"));
    shoot(&ui, &window, &out, "23-cloud-narrow-actions", 900, 760)?;

    // ---------------- 24. 行内错误条（有数据时的加载失败） ----------------
    // `list-state = error` + **非空** file-list 才会走 `inline-error` 分支：列表照常
    // 显示，错误条与「重试」按钮占一行压在表头上方。旧的 07 只覆盖"整页错误"
    // （列表为空、占据整个列表区），这条路径在基线里完全是空白。
    ui.set_file_list(ModelRc::new(VecModel::from(demo_files())));
    ui.set_selected_count(0);
    ui.set_list_state(SharedString::from("error"));
    ui.set_file_error(SharedString::from(
        "刷新目录失败：网络连接超时，已显示上一次成功的结果",
    ));
    ui.set_status_text(SharedString::from("刷新失败：网络连接超时"));
    shoot(&ui, &window, &out, "24-cloud-inline-error", 1180, 760)?;

    // ---------------- 25. 登录页字段红框（S-03 的 invalid） ----------------
    // `login-error` 非空时登录页把两个输入框一起标成 invalid（`pages/login.slint` 的
    // `changed login-error`）。旧的 20 是"最窄窗口 + 验证码 + 已保存账号"的复合场景，
    // 红框在那张图里既小又容易被验证码抢走注意力，这条单独采一档。
    ui.set_logged_in(false);
    ui.set_qr_mode(false);
    ui.set_show_captcha(false);
    ui.set_saved_accounts(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_status_text(SharedString::default());
    // 先真绘一帧把重建出来的登录页实例化，再写错误文案：否则新实例的初值直接就是
    // 错误文案，`changed login-error` 不触发，红框不会出现（见 render_frame 的说明）。
    render_frame(&window, 900, 760);
    ui.set_login_error(SharedString::from(
        "账号或密码不正确，请检查后重试（剩余尝试次数 4）",
    ));
    shoot(&ui, &window, &out, "25-login-invalid", 900, 760)?;

    // ---------------- 26. 紧凑导航 + 完整标签批量栏（D-11 的视觉证据） ----------------
    // 1000px 是 D-11 修好之后才存在的形态：导航栏折叠成 56px 图标栏（1000 < 1148），
    // 内容区却有 944px ≥ 900px，于是批量栏的**文字**全都还在。
    // 修之前这一档是反过来的：整窗 960px 就让导航展开吃掉 248px，内容区只剩 712px，
    // 批量栏退化成图标档——窗口比 959px 宽，能力却比 959px 少。
    // 这张图就是"导航是图标栏 + 批量栏有文字"能同时成立的那一帧。
    ui.set_logged_in(true);
    ui.set_login_error(SharedString::default());
    ui.set_current_page(0);
    ui.set_list_state(SharedString::from("ready"));
    let mut compact_rows = demo_files();
    compact_rows.truncate(4);
    for row in compact_rows.iter_mut().skip(1) {
        row.selected = true;
    }
    ui.set_file_list(ModelRc::new(VecModel::from(compact_rows)));
    ui.set_selected_count(3);
    ui.set_status_text(SharedString::from("已选择 3 项"));
    shoot(&ui, &window, &out, "26-cloud-compact-nav", 1000, 760)?;

    // ---------------- 27. 排序浮层打开态（D-12 的入口） ----------------
    // 默认 1180px 窗口的内容区只有 932px → detail-level=1 →「修改时间」**列头不存在**，
    // 按时间排序的入口只有命令栏这个下拉。这张图把它采下来：按钮在搜索框右侧，
    // 浮层贴按钮下方左对齐，三条（名称 / 大小 / 修改时间）右侧显示当前列的方向箭头。
    ui.set_selected_count(0);
    ui.set_status_text(SharedString::from("8 个项目"));
    ui.set_file_list(ModelRc::new(VecModel::from(demo_files())));
    ui.set_sort_menu_open(true);
    shoot(&ui, &window, &out, "27-cloud-sort-menu", 1180, 760)?;
    ui.set_sort_menu_open(false);

    // ---------------- 28. 云盘（超宽窗口 1920×1080，D-19 的视觉证据） ----------------
    // 修之前：内容区 1672px，文件行被拉成一条线、右侧一大片空白。
    // 修之后：工作区列被 `Theme.workspace-max-width`（1360px）限住并居中，左右各留
    // 156px；同时 `min(1672, 1360) = 1360 ≥ 1300`，四列（名称 / 大小 / 修改时间 / 类型）
    // 全开——这张图同时是"限宽生效"与"限宽没把类型列吃掉"的证据。
    ui.set_selected_count(0);
    ui.set_status_text(SharedString::from("8 个项目"));
    ui.set_file_list(ModelRc::new(VecModel::from(demo_files())));
    shoot(&ui, &window, &out, "28-cloud-wide-1920", 1920, 1080)?;

    // ---------------- 29. 账号菜单打开态（1180×760，D-18 的视觉证据） ----------------
    // 账号菜单是唯一一个"常驻可点、平时没有截图覆盖"的浮层：它只在点左下角头像后出现，
    // 旧的 22 只覆盖了右键菜单、27 只覆盖了排序浮层。D-18 修完之后它周围应该有一圈
    // 柔和的投影（`Theme.shadow` + `shadow-blur` 16px / `shadow-offset-y` 4px）。
    ui.set_account_menu_open(true);
    shoot(&ui, &window, &out, "29-account-menu", 1180, 760)?;
    ui.set_account_menu_open(false);

    Ok(())
}
