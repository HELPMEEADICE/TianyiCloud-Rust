//! 拖出下载：将云端文件作为真实文件拖拽到资源管理器/桌面
//!
//! 实现方式：先把云端文件下载到本地临时目录，再用 COM `DoDragDrop` 启动一次
//! 以 `CF_HDROP`（HGLOBAL）为数据格式的系统拖拽。拖放目标（资源管理器等）
//! 从临时文件执行复制，拖拽结束由系统回调通知，之后清理临时文件。

use windows::core::implement;
use windows::Win32::Foundation::{
    BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, GlobalFree, POINT,
};
use windows::Win32::System::Com::{
    FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Ole::{
    DoDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropSource,
    IDropSource_Impl,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::DROPFILES;

/// 拖拽源数据对象：只提供 CF_HDROP 格式
#[implement(IDataObject)]
struct DropSourceData {
    /// 格式：CF_HDROP / TYMED_HGLOBAL / DVASPECT_CONTENT
    cf: u16,
    /// DROPFILES + 各路径（宽字符，双 NUL 结尾）
    hdrop_bytes: Vec<u8>,
}

impl DropSourceData {
    fn build_dropfiles(paths: &[std::path::PathBuf]) -> Vec<u8> {
        let mut out = Vec::new();
        let dropfiles = DROPFILES {
            pFiles: std::mem::size_of::<DROPFILES>() as u32,
            pt: POINT::default(),
            fNC: BOOL(0),
            fWide: BOOL(1),
        };
        let header = unsafe {
            std::slice::from_raw_parts(
                &dropfiles as *const DROPFILES as *const u8,
                std::mem::size_of::<DROPFILES>(),
            )
        };
        out.extend_from_slice(header);
        for p in paths {
            let lossy = p.to_string_lossy();
            let wide = lossy.encode_utf16().chain(std::iter::once(0));
            for u in wide {
                out.extend_from_slice(&u.to_le_bytes());
            }
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // 结尾双 NUL
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

impl IDataObject_Impl for DropSourceData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        unsafe {
            let fmt = &*pformatetcin;
            if fmt.cfFormat != self.cf {
                return Err(windows::core::Error::from_win32());
            }
            let hglobal = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, self.hdrop_bytes.len())?;
            let dst = GlobalLock(hglobal);
            if dst.is_null() {
                let _ = GlobalFree(hglobal);
                return Err(windows::core::Error::from_win32());
            }
            std::ptr::copy_nonoverlapping(self.hdrop_bytes.as_ptr(), dst as *mut u8, self.hdrop_bytes.len());
            let _ = GlobalUnlock(hglobal);
            let mut medium = STGMEDIUM::default();
            medium.tymed = TYMED_HGLOBAL.0 as u32;
            medium.u = STGMEDIUM_0 { hGlobal: hglobal };
            Ok(medium)
        }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(windows::core::Error::from_win32())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows_core::HRESULT {
        unsafe {
            let fmt = &*pformatetc;
            if fmt.cfFormat == self.cf {
                windows::core::HRESULT(0)
            } else {
                windows::core::HRESULT(0x80040064u32 as i32) // DV_E_FORMATETC
            }
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> windows_core::HRESULT {
        unsafe {
            if pformatetcout.is_null() {
                windows::core::HRESULT(0x80004003u32 as i32) // E_POINTER
            } else {
                *pformatetcout = std::mem::zeroed();
                windows::core::HRESULT(0x00040100) // DATA_S_SAMEFORMATETC
            }
        }
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows_core::Result<()> {
        Err(windows::core::Error::from_win32())
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> windows_core::Result<IEnumFORMATETC> {
        Err(windows::core::Error::from_win32())
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Option<&IAdviseSink>,
    ) -> windows_core::Result<u32> {
        Err(windows::core::Error::from_win32())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows_core::Result<()> {
        Err(windows::core::Error::from_win32())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(windows::core::Error::from_win32())
    }
}

/// 拖拽源：按下左键拖动，Esc 取消
#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> windows_core::HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        // 左键仍按住 → 继续
        if grfkeystate.0 & MK_LBUTTON.0 != 0 {
            windows::core::HRESULT(0) // S_OK
        } else {
            DRAGDROP_S_DROP
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows_core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// 启动一次系统拖拽。`paths` 为已下载到本地的临时文件路径。
/// 阻塞直到拖拽结束，返回是否发生了实际复制。
pub fn do_drag_drop(paths: &[std::path::PathBuf]) -> windows::core::Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let cf = 15u16; // CF_HDROP
    let data = DropSourceData {
        cf,
        hdrop_bytes: DropSourceData::build_dropfiles(paths),
    };
    let data_obj: IDataObject = data.into();
    let source: IDropSource = DropSource.into();

    let mut effect = DROPEFFECT_NONE;
    let hr = unsafe {
        DoDragDrop(
            &data_obj,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        )
    };
    if hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE {
        Ok(true)
    } else {
        // 取消或出错
        Ok(false)
    }
}

/// 下载完成后清理临时文件（最佳努力）
pub fn cleanup_temp_paths(paths: &[std::path::PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}
