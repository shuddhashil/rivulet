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

/// A label being dragged along the rail: a picture of it, lifted a little, under the mouse.
struct Lifted {
    window: usize,
    /// Where on the picture the mouse holds it.
    grab: POINT,
}

static LABEL: Mutex<Option<Lifted>> = Mutex::new(None);

/// How much bigger a lifted label is drawn, so it reads as picked up.
const LIFT: f32 = 1.06;

/// Pick up the label at `rect` on screen, held at `at`. True once it is showing.
pub unsafe fn lift_label(rect: windows_sys::Win32::Foundation::RECT, at: POINT) -> bool {
    let Ok(mut guard) = LABEL.try_lock() else { return false };
    if guard.is_some() {
        return true;
    }
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let (bw, bh) = (((w as f32) * LIFT) as i32, ((h as f32) * LIFT) as i32);
    let grab = POINT { x: ((at.x - rect.left) as f32 * LIFT) as i32, y: ((at.y - rect.top) as f32 * LIFT) as i32 };
    let place = POINT { x: at.x - grab.x, y: at.y - grab.y };
    let class = wide("RivuletLabelGhost");
    let instance = GetModuleHandleW(std::ptr::null());
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(DefWindowProcW);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    RegisterClassW(&wc);
    let style = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let window = CreateWindowExW(style, class.as_ptr(), class.as_ptr(), WS_POPUP, place.x, place.y, bw, bh, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
    if window.is_null() {
        return false;
    }
    // the label exactly as it looks now, whatever the style, taken from the screen
    let screen = GetDC(std::ptr::null_mut());
    let dc = CreateCompatibleDC(screen);
    let picture = CreateCompatibleBitmap(screen, bw, bh);
    let old = SelectObject(dc, picture as _);
    SetStretchBltMode(dc, HALFTONE);
    StretchBlt(dc, 0, 0, bw, bh, screen, rect.left, rect.top, w, h, SRCCOPY);
    let (size, origin) = (SIZE { cx: bw, cy: bh }, POINT { x: 0, y: 0 });
    let faint = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 235, AlphaFormat: 0 };
    UpdateLayeredWindow(window, screen, &place, &size, dc, &origin, 0, &faint, ULW_ALPHA);
    SelectObject(dc, old);
    DeleteObject(picture as _);
    DeleteDC(dc);
    ReleaseDC(std::ptr::null_mut(), screen);
    // rounded like the label, so the wallpaper behind its corners does not come along
    let round = (h as f32 * LIFT * 0.45) as i32;
    SetWindowRgn(window, CreateRoundRectRgn(0, 0, bw + 1, bh + 1, round, round), 0);
    ShowWindow(window, SW_SHOWNOACTIVATE);
    *guard = Some(Lifted { window: window as usize, grab });
    true
}

pub unsafe fn move_label(to: POINT) {
    let Ok(guard) = LABEL.try_lock() else { return };
    if let Some(l) = guard.as_ref() {
        SetWindowPos(l.window as HWND, std::ptr::null_mut(), to.x - l.grab.x, to.y - l.grab.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Let go: the lifted label glides down into its place at `rect` on screen, then goes.
pub unsafe fn settle_label(rect: windows_sys::Win32::Foundation::RECT) {
    let Some(l) = LABEL.lock().ok().and_then(|mut g| g.take()) else { return };
    let window = l.window;
    std::thread::spawn(move || {
        let mut now = std::mem::zeroed();
        GetWindowRect(window as HWND, &mut now);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let (bw, bh) = (now.right - now.left, now.bottom - now.top);
        // centred on its place, so the last of the lift shrinks away into it
        let to = (rect.left - (bw - w) / 2, rect.top - (bh - h) / 2);
        let began = std::time::Instant::now();
        loop {
            let t = (began.elapsed().as_secs_f32() / 0.14).min(1.0);
            let k = 1.0 - (1.0 - t).powi(3);
            let x = now.left + ((to.0 - now.left) as f32 * k).round() as i32;
            let y = now.top + ((to.1 - now.top) as f32 * k).round() as i32;
            SetWindowPos(window as HWND, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        // its own thread must close it, so ask that thread to
        PostMessageW(window as HWND, WM_CLOSE, 0, 0);
    });
}

pub unsafe fn drop_label() {
    if let Some(l) = LABEL.lock().ok().and_then(|mut g| g.take()) {
        DestroyWindow(l.window as HWND);
    }
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
