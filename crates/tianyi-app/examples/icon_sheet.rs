//! 图标总览渲染器（视觉自检用）
//!
//! 把 `ui/dev/icon-sheet.slint` 里注册表的全部图标（Regular / Filled × 16/20/32）
//! 用 Slint 软件渲染器画成 PNG，浅色与深色各一张，用来逐个确认官方 SVG 资源
//! 都能正确加载、着色、且没有被拉伸或裁切。
//!
//! ```text
//! cargo run -p tianyi-app --example icon_sheet -- <输出目录>
//! ```

// include_modules!() 只包含主界面（main.slint）；总览是第二个被编译的组件，
// 由 build.rs 输出到 OUT_DIR/icon_sheet.rs，这里直接包含。
include!(concat!(env!("OUT_DIR"), "/icon_sheet.rs"));

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, Rgb8Pixel};
use std::rc::Rc;
use std::time::{Duration, Instant};

struct SheetPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
}

impl Platform for SheetPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

fn shoot(
    window: &MinimalSoftwareWindow,
    dir: &std::path::Path,
    name: &str,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    window.set_size(PhysicalSize::new(width, height));
    std::thread::sleep(Duration::from_millis(120));
    slint::platform::update_timers_and_animations();

    let mut buffer = slint::SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), width as usize);
    });

    let mut img = image::RgbaImage::new(width, height);
    for (i, px) in buffer.as_slice().iter().enumerate() {
        img.put_pixel(
            (i % width as usize) as u32,
            (i / width as usize) as u32,
            image::Rgba([px.r, px.g, px.b, 255]),
        );
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
        .unwrap_or_else(|| std::path::PathBuf::from("screenshots/icons"));
    std::fs::create_dir_all(&out)?;

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(SheetPlatform {
        window: window.clone(),
        start: Instant::now(),
    }))?;

    let sheet = IconSheet::new()?;
    sheet.show()?;

    // 与 ui/dev/icon-sheet.slint 的 Window 尺寸一致（56 个图标 × 38px 行高 + 表头）
    let (w, h) = (620u32, 150 + 56 * 38);
    sheet.set_dark(false);
    shoot(&window, &out, "icon-sheet-light", w, h)?;

    sheet.set_dark(true);
    shoot(&window, &out, "icon-sheet-dark", w, h)?;

    Ok(())
}
