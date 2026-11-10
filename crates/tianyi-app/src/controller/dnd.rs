//! 拖拽：DragDropState 对接、网盘内移动、拖出下载与清理。
use super::*;
use std::sync::Arc;
// 拖出下载只在 Windows 上有实现，下面这些符号全部只在 `cfg(windows)` 的
// 函数体里出现；不带门控时非 Windows 目标会逐个报 unused_imports，
// 而 CI 的 clippy 作业是 `-D warnings`，会直接红。
#[cfg(windows)]
use {
    super::files::sanitize_file_name,
    crate::app::DragOutApi,
    crate::backend::invoke_ui,
    slint::{ComponentHandle, SharedString},
    std::sync::atomic::Ordering,
    tianyi_core::task::TransferControl,
    tianyi_core::transfer::{self, DownloadOptions, DownloadOutcome},
};
impl Controller {
    // ==================================================================
    // 拖拽
    // ==================================================================
    pub fn set_drag_state(&self, state: &'static crate::DragDropState) {
        *lock_or_recover(&self.drag_state) = Some(state);
    }

    /// 判断拖放目标是否可接收
    pub(super) fn check_drop_target(
        &self,
        event: &slint::language::DropEvent,
        folder_id: &str,
    ) -> slint::language::DragAction {
        let Some(src) = Self::dragged_file(event) else {
            return slint::language::DragAction::None;
        };
        if src.id == folder_id {
            return slint::language::DragAction::None;
        }
        slint::language::DragAction::Move
    }

    fn dragged_file(event: &slint::language::DropEvent) -> Option<MovedFile> {
        event
            .data
            .user_data()
            .and_then(|d| d.downcast::<MovedFile>().ok().map(|m| m.as_ref().clone()))
    }

    pub(super) fn handle_drop_move(
        self: &Arc<Self>,
        event: &slint::language::DropEvent,
        folder_id: &str,
    ) {
        let Some(src) = Self::dragged_file(event) else {
            return;
        };
        if src.id == folder_id {
            return;
        }
        let Some(file) = self.find_visible(&src.id) else {
            return;
        };
        let Some(client) = self.client() else { return };
        let folder_id = folder_id.to_string();
        let name = src.name.clone();
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.move_to(&file, &folder_id).await {
                Ok(_) => {
                    this.notice("success", tf("toast.moved", &[("name", &name)]));
                    this.refresh_files();
                }
                Err(e) => this.notice("error", tf("error.move_failed", &[("e", &e)])),
            }
        });
    }

    #[cfg(windows)]
    pub(super) fn handle_drag_out(self: &Arc<Self>, id: &str, name: &str) {
        if self.client().is_none() {
            self.notice("error", t("toast.login_required"));
            return;
        }
        let Some(file) = self.find_visible(id) else {
            return;
        };
        if self.drag_out_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let name = name.to_string();
        let this = Arc::clone(self);
        let ui = self.ui.clone();
        let id = id.to_string();
        self.set_ui_state(move |win| {
            win.set_status_text(SharedString::from(t("status.preparing_drag_out")));
            let api = win.global::<DragOutApi>();
            api.set_active(true);
            api.set_active_id(SharedString::from(id.as_str()));
        });
        self.backend.spawn(move || async move {
            let temp_dir = std::env::temp_dir().join("tianyi-dragout");
            let _ = std::fs::create_dir_all(&temp_dir);
            let temp_path = temp_dir.join(sanitize_file_name(&name));
            let ok = if let Some(client) = this.client() {
                let opts = DownloadOptions::default();
                let ctl = TransferControl::new();
                let prog: transfer::ProgressCallback = Arc::new(|_| {});
                matches!(
                    transfer::download(&client, &file, &temp_path, &opts, prog, &ctl).await,
                    Ok(DownloadOutcome::Completed)
                )
            } else {
                false
            };
            if !ok {
                this.clear_drag_out_active();
                this.drag_out_busy.store(false, Ordering::SeqCst);
                this.notice("error", t("error.drag_out_failed"));
                return;
            }
            let temp_refs = [temp_path.clone()];
            let dropped = crate::dragout::do_drag_drop(&temp_refs).unwrap_or(false);
            crate::dragout::cleanup_temp_paths(&temp_refs);
            this.drag_out_busy.store(false, Ordering::SeqCst);
            this.clear_drag_out_active();
            invoke_ui(&ui, move |win| {
                // 系统拖拽结束后松开左键的消息可能被 OLE 消费，主动派发一次 PointerReleased
                use slint::platform::{PointerEventButton, WindowEvent};
                use slint::LogicalPosition;
                let _ = win
                    .window()
                    .try_dispatch_event(WindowEvent::PointerReleased {
                        position: LogicalPosition::new(-1000.0, -1000.0),
                        button: PointerEventButton::Left,
                    });
                let msg = if dropped {
                    tf("status.dragged_out", &[("name", &name)])
                } else {
                    t("status.drag_canceled").to_string()
                };
                win.set_status_text(SharedString::from(msg));
            });
        });
    }

    #[cfg(not(windows))]
    pub(super) fn handle_drag_out(self: &Arc<Self>, _id: &str, _name: &str) {
        self.notice("info", t("toast.drag_out_unsupported"));
    }

    #[cfg(windows)]
    fn clear_drag_out_active(&self) {
        self.set_ui_state(|win| {
            let api = win.global::<DragOutApi>();
            api.set_active(false);
            api.set_active_id(SharedString::default());
        });
    }
}
/// 网盘内拖拽载荷
#[derive(Debug, Clone)]
pub(super) struct MovedFile {
    pub(super) id: String,
    pub(super) name: String,
}
