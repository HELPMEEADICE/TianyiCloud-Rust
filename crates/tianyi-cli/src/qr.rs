//! 终端与文件二维码
//!
//! Headless 环境里没有窗口、也不能弹图片：二维码必须能「在 SSH 终端里直接扫」。这里用
//! 半块字符（U+2580 / U+2584 / U+2588）把 QR 矩阵压成两行一个字符，任何带等宽字体的
//! 终端都能正确显示，且是纯文本、不依赖图形库。
//!
//! 另外支持把二维码写成文件：.svg 是矢量图（手机相册/浏览器都能扫），.txt 是同一份
//! 文本画法，便于在没有终端彩排的环境里转存。

use crate::error::{CliError, CliResult, ErrorCode};
use qrcode::QrCode;
use std::path::Path;

/// 生成二维码矩阵（true = 深色模块）
fn matrix(text: &str) -> CliResult<(usize, Vec<bool>)> {
    let code = QrCode::new(text.as_bytes())
        .map_err(|e| CliError::new(ErrorCode::Internal, format!("生成二维码失败：{e}")))?;
    let width = code.width();
    let colors = code.to_colors();
    Ok((
        width,
        colors
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect(),
    ))
}

/// 渲染成终端可直接打印的文本（每行以换行结束）
///
/// 留白 2 个字符（约等于 4 个模块）是扫码器的标准静默区需求；没有静默区时很多手机
/// 相机识别率会明显下降。
pub fn render_terminal(text: &str) -> CliResult<String> {
    const QUIET: usize = 2;
    let (width, dark) = matrix(text)?;
    let at = |x: usize, y: usize| dark.get(y * width + x).copied().unwrap_or(false);
    let mut out = String::new();
    let blank = " ".repeat(width + QUIET * 2);
    for _ in 0..QUIET {
        out.push_str(&blank);
        out.push('\n');
    }
    let mut y = 0;
    while y < width {
        out.push_str(&" ".repeat(QUIET));
        for x in 0..width {
            let top = at(x, y);
            let bottom = if y + 1 < width { at(x, y + 1) } else { false };
            out.push(match (top, bottom) {
                (true, true) => '\u{2588}',  // 全块
                (true, false) => '\u{2580}', // 上半块
                (false, true) => '\u{2584}', // 下半块
                (false, false) => ' ',
            });
        }
        out.push_str(&" ".repeat(QUIET));
        out.push('\n');
        y += 2;
    }
    for _ in 0..QUIET {
        out.push_str(&blank);
        out.push('\n');
    }
    Ok(out)
}

/// 渲染成 SVG（每个深色模块一个 rect，体积小且任何浏览器都能打开）
pub fn render_svg(text: &str) -> CliResult<String> {
    const BORDER: usize = 4;
    let (width, dark) = matrix(text)?;
    let size = width + BORDER * 2;
    let mut rects = String::new();
    for y in 0..width {
        for x in 0..width {
            if dark.get(y * width + x).copied().unwrap_or(false) {
                rects.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"1\" height=\"1\"/>",
                    x + BORDER,
                    y + BORDER
                ));
            }
        }
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\" viewBox=\"0 0 {size} {size}\" shape-rendering=\"crispEdges\"><rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/><g fill=\"#000\">{rects}</g></svg>\n"
    ))
}

/// 按扩展名写文件：.svg → 矢量图；其它 → 文本画法
pub fn write_file(path: &Path, text: &str) -> CliResult<()> {
    let is_svg = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);
    let content = if is_svg {
        render_svg(text)?
    } else {
        render_terminal(text)?
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_art_is_square_and_uses_half_blocks() {
        let art = render_terminal("https://example.com/x").expect("渲染");
        let lines: Vec<&str> = art.lines().collect();
        assert!(lines.len() > 10, "行数太少：{}", lines.len());
        // 去掉静默行后每行长度一致（等宽矩阵）
        let widths: std::collections::HashSet<usize> =
            lines.iter().map(|l| l.chars().count()).collect();
        assert_eq!(widths.len(), 1, "每行宽度必须一致：{widths:?}");
        assert!(
            art.contains('\u{2588}') || art.contains('\u{2580}') || art.contains('\u{2584}'),
            "必须用半块字符绘制"
        );
        assert!(
            !art.contains('0') && !art.contains('1'),
            "输出不能是 0/1 矩阵"
        );
    }

    #[test]
    fn svg_is_well_formed_and_has_a_viewbox() {
        let svg = render_svg("abc").expect("渲染");
        assert!(svg.starts_with("<?xml"));
        assert!(svg.contains("<svg"));
        assert!(svg.contains("viewBox="));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("<rect"), "必须有模块矩形");
    }

    #[test]
    fn write_file_picks_format_by_extension() {
        let dir = std::env::temp_dir().join(format!("tianyi-qr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let svg = dir.join("q.svg");
        write_file(&svg, "hello").expect("写 svg");
        assert!(std::fs::read_to_string(&svg).unwrap().contains("<svg"));
        let txt = dir.join("q.txt");
        write_file(&txt, "hello").expect("写 txt");
        assert!(
            std::fs::read_to_string(&txt).unwrap().contains('\u{2588}')
                || std::fs::read_to_string(&txt).unwrap().contains('\u{2580}')
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
