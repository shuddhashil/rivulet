//! The faint icon that follows the mouse while dragging a file to a label. It all runs on the mouse
//! hook's thread, so it stays quick: the picture is made once and after that the window only moves.

use crate::wallpaper::wide;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

struct Drag {
    from: POINT,
    icon: isize,
    side: i32,
    window: usize,
}

static DRAG: Mutex<Option<Drag>> = Mutex::new(None);

/// A slip of the hand during a click is not a drag.
fn far_enough(from: POINT, to: POINT) -> bool {
    (from.x - to.x).abs() > 5 || (from.y - to.y).abs() > 5
}

pub fn press(from: POINT, icon: isize, side: i32) {
    if let Ok(mut drag) = DRAG.try_lock() {
        *drag = Some(Drag { from, icon, side, window: 0 });
    }
}

pub unsafe fn moved(to: POINT) {
    let Ok(mut guard) = DRAG.try_lock() else { return };
    let Some(drag) = guard.as_mut() else { return };
    let half = drag.side / 2;
    if drag.window != 0 {
        SetWindowPos(drag.window as HWND, std::ptr::null_mut(), to.x - half, to.y - half, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    } else if far_enough(drag.from, to) {
        drag.window = show(drag.icon, drag.side, POINT { x: to.x - half, y: to.y - half }) as usize;
    }
}

/// Ends it, and says if there really was a drag rather than a click.
pub unsafe fn release() -> bool {
    let Ok(mut guard) = DRAG.try_lock() else { return false };
    if let Some(drag) = guard.take() {
        if drag.window != 0 {
            DestroyWindow(drag.window as HWND);
            return true;
        }
    }
    false
}

pub fn dragging() -> bool {
    DRAG.try_lock().is_ok_and(|d| d.as_ref().is_some_and(|d| d.window != 0))
}

unsafe fn show(icon: isize, side: i32, at: POINT) -> HWND {
    let class = wide("RivuletGhost");
    let instance = GetModuleHandleW(std::ptr::null());
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(DefWindowProcW);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    RegisterClassW(&wc); // already registered after the first drag, which is fine
    // see-through to the mouse, so the label under it still counts as being under the mouse
    let style = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let window = CreateWindowExW(style, class.as_ptr(), class.as_ptr(), WS_POPUP, at.x, at.y, side, side, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
    if window.is_null() {
        return window;
    }
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = side;
    info.bmiHeader.biHeight = -side;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let dc = CreateCompatibleDC(std::ptr::null_mut());
    let mut bits = std::ptr::null_mut();
    let picture = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if !picture.is_null() {
        let old = SelectObject(dc, picture as _);
        DrawIconEx(dc, 0, 0, icon as _, side, side, 0, std::ptr::null_mut(), DI_NORMAL);
        let (size, origin) = (SIZE { cx: side, cy: side }, POINT { x: 0, y: 0 });
        let faint = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 200, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(window, std::ptr::null_mut(), &at, &size, dc, &origin, 0, &faint, ULW_ALPHA);
        SelectObject(dc, old);
        DeleteObject(picture as _);
    }
    DeleteDC(dc);
    ShowWindow(window, SW_SHOWNOACTIVATE);
    window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_move_starts_a_drag() {
        let from = POINT { x: 100, y: 100 };
        assert!(!far_enough(from, POINT { x: 104, y: 96 }));
        assert!(far_enough(from, POINT { x: 106, y: 100 }));
        assert!(far_enough(from, POINT { x: 100, y: 94 }));
    }
}
