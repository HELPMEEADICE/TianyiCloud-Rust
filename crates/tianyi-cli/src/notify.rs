//! 登录回调：把验证码与二维码送到终端 / 文件，而不是图形界面
//!
//! tianyi-core 的登录流程通过 LoginNotifier 回调把「需要验证码」「二维码内容」「扫码
//! 状态」交出来。GUI 的实现画图片，CLI 的实现必须能在没有显示器、没有剪贴板、没有
//! 浏览器的环境里工作，因此这里：
//!
//! * 验证码 PNG **原样落盘**（服务端给的就是 base64 PNG，本地不需要任何图像库）；
//! * 二维码画在 stderr（stdout 只留给结果），也可以额外写成 .svg/.txt；
//! * 所有回调状态都记在 state 里，命令结束后能拼进 JSON 的 details。

use crate::error::{CliError, CliResult, ErrorCode};
use crate::qr;
use base64::Engine;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tianyi_core::api::LoginNotifier;

/// 回调产生的中间状态
#[derive(Debug, Default)]
struct NotifierState {
    captcha_path: Option<PathBuf>,
    qr_text: Option<String>,
    last_status: Option<String>,
}

/// 登录通知器
pub struct CliNotifier {
    /// 二维码 UUID 文本（登录成功后仍在，便于用户核对）
    state: Mutex<NotifierState>,
    /// 验证码图片落盘路径
    captcha_out: PathBuf,
    /// 二维码额外写入的文件
    qr_file: Option<PathBuf>,
    /// 是否在终端绘制二维码
    draw_qr: bool,
    /// 是否静默（--quiet 时不打印任何诊断）
    quiet: bool,
}

impl CliNotifier {
    /// captcha_out 由调用方给（默认放在配置目录里）
    pub fn new(captcha_out: PathBuf, qr_file: Option<PathBuf>, draw_qr: bool, quiet: bool) -> Self {
        CliNotifier {
            state: Mutex::new(NotifierState::default()),
            captcha_out,
            qr_file,
            draw_qr,
            quiet,
        }
    }

    /// 验证码图片落盘路径（没有则需要过验证码）
    pub fn captcha_path(&self) -> Option<PathBuf> {
        self.state.lock().ok().and_then(|s| s.captcha_path.clone())
    }

    /// 二维码内容
    pub fn qr_text(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.qr_text.clone())
    }

    fn diag(&self, line: &str) {
        if !self.quiet {
            eprintln!("{line}");
        }
    }
}

impl LoginNotifier for CliNotifier {
    fn need_captcha(&self, image_base64: &str) {
        // 服务端给的就是 base64 PNG：解码后直接落盘，不解码像素、不依赖图像库
        let bytes = match base64::engine::general_purpose::STANDARD.decode(image_base64) {
            Ok(b) => b,
            Err(e) => {
                self.diag(&format!("验证码图片解码失败（{e}），请重试"));
                return;
            }
        };
        if let Some(parent) = self.captcha_out.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        match std::fs::write(&self.captcha_out, &bytes) {
            Ok(()) => {
                if let Ok(mut st) = self.state.lock() {
                    st.captcha_path = Some(self.captcha_out.clone());
                }
                self.diag(&format!(
                    "需要验证码：图片已保存到 {}",
                    self.captcha_out.display()
                ));
                self.diag(
                    "查看图片后，用 --captcha <code> 或 --captcha-file <path> 重新执行登录。",
                );
            }
            Err(e) => self.diag(&format!("验证码图片写入失败（{e}）")),
        }
    }

    fn qr_code(&self, uuid: &str, text: &str) {
        if let Ok(mut st) = self.state.lock() {
            st.qr_text = Some(uuid.to_string());
        }
        if self.draw_qr {
            match qr::render_terminal(uuid) {
                Ok(art) => {
                    // stderr：stdout 只留给结果
                    eprint!("{art}");
                }
                Err(e) => self.diag(&format!("二维码绘制失败：{e}")),
            }
        }
        if let Some(path) = &self.qr_file {
            match qr::write_file(path, uuid) {
                Ok(()) => self.diag(&format!("二维码已写入 {}", path.display())),
                Err(e) => self.diag(&format!("二维码写入失败：{e}")),
            }
        }
        self.diag(&format!("{text}（内容：{uuid}）"));
    }

    fn qr_status(&self, status: &str) {
        if let Ok(mut st) = self.state.lock() {
            st.last_status = Some(status.to_string());
        }
        self.diag(status);
    }
}

/// 验证码图片的默认落盘路径（配置目录下，带进程号避免并发覆盖）
pub fn default_captcha_path(config_root: &Path) -> PathBuf {
    config_root.join(format!("captcha-{}.png", std::process::id()))
}

/// 读取验证码文本文件（去尾部空白）
pub fn read_captcha_file(path: &Path) -> CliResult<String> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(
            ErrorCode::LocalIo,
            format!("读取验证码文件 {} 失败：{e}", path.display()),
        )
    })?;
    let code = raw.trim().to_string();
    if code.is_empty() {
        return Err(CliError::usage(format!(
            "验证码文件 {} 是空的",
            path.display()
        )));
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tianyi-notify-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn captcha_image_is_saved_verbatim() {
        let root = tmp_root("captcha");
        let out = root.join("captcha.png");
        let notifier = CliNotifier::new(out.clone(), None, false, true);
        // 一段真实的 PNG 头 + 数据（内容不重要，重要的是原样落盘）
        let bytes = b"\x89PNG\r\n\x1a\nfake";
        notifier.need_captcha(&STANDARD.encode(bytes));
        assert_eq!(notifier.captcha_path(), Some(out.clone()));
        assert_eq!(std::fs::read(&out).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn qr_callback_records_text_and_optionally_writes_file() {
        let root = tmp_root("qr");
        let file = root.join("q.svg");
        let notifier = CliNotifier::new(root.join("c.png"), Some(file.clone()), false, true);
        notifier.qr_code("some-uuid", "请扫码");
        assert_eq!(notifier.qr_text().as_deref(), Some("some-uuid"));
        let svg = std::fs::read_to_string(&file).expect("二维码文件必须写出来");
        assert!(svg.contains("<svg"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn captcha_file_reader_trims_and_rejects_empty() {
        let root = tmp_root("cap-read");
        let ok = root.join("ok.txt");
        std::fs::write(&ok, " abc \r\n").unwrap();
        assert_eq!(read_captcha_file(&ok).unwrap(), "abc");
        let empty = root.join("empty.txt");
        std::fs::write(&empty, "   \n").unwrap();
        assert!(read_captcha_file(&empty).is_err());
        assert!(read_captcha_file(&root.join("missing.txt")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
