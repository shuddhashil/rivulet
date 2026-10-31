//! The three windows that sit between the wallpaper and the icons: the label rail, the panel
//! behind an open section's icons, and the liquid neck joining them. Windows ignores per-pixel
//! transparency in this slot, so each is shaped with a region and see-through as a whole.

use crate::rules::{blend, neck_outline};
use crate::wallpaper::wide;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "DesktopSectionsLayer";
const SPAWN_LAYERS: u32 = 0x052C;
const LWA_ALPHA_FLAG: u32 = 2;
const WHITE: u32 = 0x00FFFFFF;

static RAIL: AtomicUsize = AtomicUsize::new(0);
static PANEL_WINDOW: AtomicUsize = AtomicUsize::new(0);
static NECK: AtomicUsize = AtomicUsize::new(0);
/// Colours are written blue, green, red. These follow the wallpaper and the open section.
static PILL_TINT: AtomicU32 = AtomicU32::new(0x00261A12);
static PANEL_TINT: AtomicU32 = AtomicU32::new(0x00160E0A);
static ACCENT: AtomicU32 = AtomicU32::new(0x00F0A040);

pub struct Pill {
    pub name: String,
    pub count: usize,
    pub active: bool,
    pub hover: bool,
    pub fresh: bool,
    pub colour: u32,
    pub glyph: char,
    /// Inside the rail window.
    pub rect: RECT,
}

pub static PILLS: Mutex<Vec<Pill>> = Mutex::new(Vec::new());

fn rail() -> HWND {
    RAIL.load(Ordering::Relaxed) as HWND
}
fn panel() -> HWND {
    PANEL_WINDOW.load(Ordering::Relaxed) as HWND
}
fn neck() -> HWND {
    NECK.load(Ordering::Relaxed) as HWND
}

/// Where our windows go: inside which parent, and which sibling to sit behind.
/// Recent Windows 11 keeps the icons directly inside Progman. Older versions keep them in one
/// WorkerW window, with the wallpaper in the next WorkerW down, so there we join that one.
pub fn parent_view() -> Option<(HWND, Option<HWND>)> {
    unsafe {
        let (view_class, worker_class) = (wide("SHELLDLL_DefView"), wide("WorkerW"));
        let progman = FindWindowW(wide("Progman").as_ptr(), null());
        if progman.is_null() {
            return None;
        }
        let view = FindWindowExW(progman, null_mut(), view_class.as_ptr(), null());
        if !view.is_null() {
            return Some((progman, Some(view)));
        }
        let mut worker = null_mut();
        loop {
            worker = FindWindowExW(null_mut(), worker, worker_class.as_ptr(), null());
            if worker.is_null() {
                return None;
            }
            if !FindWindowExW(worker, null_mut(), view_class.as_ptr(), null()).is_null() {
                let behind = FindWindowExW(null_mut(), worker, worker_class.as_ptr(), null());
                return Some((if behind.is_null() { progman } else { behind }, None));
            }
        }
    }
}

pub fn layout_name() -> &'static str {
    match parent_view() {
        Some((_, Some(_))) => "icons inside Progman (recent Windows 11)",
        Some((_, None)) => "icons inside WorkerW (older Windows; less tested)",
        None => "not recognised",
    }
}

pub fn alive() -> bool {
    unsafe { IsWindow(rail()) != 0 && IsWindow(panel()) != 0 && IsWindow(neck()) != 0 }
}

/// A point on screen, in the coordinates of the window that hosts the layers.
pub fn to_parent(x: i32, y: i32) -> (i32, i32) {
    let mut p = POINT { x, y };
    if let Some((parent, _)) = parent_view() {
        unsafe { ScreenToClient(parent, &mut p) };
    }
    (p.x, p.y)
}

pub fn set_tints(pill: u32, panel_shade: u32) {
    PILL_TINT.store(pill, Ordering::Relaxed);
    PANEL_TINT.store(panel_shade, Ordering::Relaxed);
}

pub fn set_accent(colour: u32) {
    ACCENT.store(colour, Ordering::Relaxed);
}

unsafe fn fill(dc: HDC, r: &RECT, colour: u32) {
    let brush = CreateSolidBrush(colour);
    FillRect(dc, r, brush);
    DeleteObject(brush as _);
}

unsafe fn font(name: &str, size: f32, weight: i32) -> HFONT {
    CreateFontW(-size as i32, 0, 0, 0, weight, 0, 0, 0, 0, 0, 0, CLEARTYPE_QUALITY as u32, 0, wide(name).as_ptr())
}

unsafe fn text(dc: HDC, s: &str, area: &mut RECT, colour: u32, flags: u32) {
    SetTextColor(dc, colour);
    let w: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(dc, w.as_ptr() as _, w.len() as i32, area, flags | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
}

unsafe fn paint_rail(dc: HDC) {
    let dpi = GetDpiForWindow(rail()) as f32 / 96.0;
    let (label_font, glyph_font) = (font("Segoe UI", 13.0 * dpi, 600), font("Segoe MDL2 Assets", 14.0 * dpi, 400));
    let old = SelectObject(dc, label_font as _);
    SetBkMode(dc, TRANSPARENT as _);
    let scaled = |v: f32| (v * dpi) as i32;
    let base = PILL_TINT.load(Ordering::Relaxed);
    if let Ok(pills) = PILLS.lock() {
        for p in pills.iter() {
            let (r, background) = (p.rect, if p.active { blend(base, p.colour, 0.55) } else if p.hover { blend(base, p.colour, 0.28) } else { base });
            fill(dc, &r, background);

            SelectObject(dc, glyph_font as _);
            let mut icon = RECT { left: r.left + scaled(10.0), top: r.top, right: r.left + scaled(32.0), bottom: r.bottom };
            text(dc, &p.glyph.to_string(), &mut icon, if p.active { WHITE } else { p.colour }, DT_LEFT);

            SelectObject(dc, label_font as _);
            let mut area = RECT { left: r.left + scaled(34.0), top: r.top, right: r.right - scaled(10.0), bottom: r.bottom };
            let shade = if p.active { 0x00F0F0F0 } else { 0x00B8B0A8 };
            text(dc, &p.count.to_string(), &mut area, shade, DT_RIGHT);
            area.right -= scaled(24.0);
            text(dc, &p.name, &mut area, WHITE, DT_LEFT | DT_END_ELLIPSIS);

            if p.fresh {
                let (size, x, y) = (scaled(7.0), r.left + scaled(25.0), r.top + scaled(6.0));
                let (brush, pen) = (CreateSolidBrush(p.colour), CreatePen(PS_SOLID, 1, WHITE));
                let (old_brush, old_pen) = (SelectObject(dc, brush as _), SelectObject(dc, pen as _));
                Ellipse(dc, x, y, x + size, y + size);
                SelectObject(dc, old_brush);
                SelectObject(dc, old_pen);
                DeleteObject(brush as _);
                DeleteObject(pen as _);
            }
        }
    }
    SelectObject(dc, old);
    DeleteObject(label_font as _);
    DeleteObject(glyph_font as _);
}

unsafe extern "system" fn layer_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut whole = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(hwnd, &mut whole);
            let accent = ACCENT.load(Ordering::Relaxed);
            if hwnd == rail() {
                paint_rail(dc);
            } else if hwnd == panel() {
                fill(dc, &whole, blend(PANEL_TINT.load(Ordering::Relaxed), accent, 0.06));
                let shape = CreateRectRgn(0, 0, 0, 0);
                if GetWindowRgn(hwnd, shape) != 0 {
                    let brush = CreateSolidBrush(blend(PANEL_TINT.load(Ordering::Relaxed), accent, 0.75));
                    FrameRgn(dc, shape, brush, 1, 1);
                    DeleteObject(brush as _);
                }
                DeleteObject(shape as _);
            } else {
                fill(dc, &whole, blend(PILL_TINT.load(Ordering::Relaxed), accent, 0.55));
            }
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Runs on the main thread, which owns these windows and keeps their messages moving.
pub unsafe fn create() -> Result<(), String> {
    // asks Explorer to split off the layer the wallpaper shows through
    let progman = FindWindowW(wide("Progman").as_ptr(), null());
    SendMessageTimeoutW(progman, SPAWN_LAYERS, 0xD, 1, SMTO_NORMAL, 1000, null_mut());
    let Some((parent, view)) = parent_view() else {
        return Err("This version of Windows arranges the desktop in a way this program does not recognise.\nNothing has been changed.".into());
    };

    let instance = GetModuleHandleW(null());
    let class = wide(CLASS);
    let wc = WNDCLASSW { lpfnWndProc: Some(layer_proc), hInstance: instance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
    RegisterClassW(&wc); // already registered after an Explorer restart, which is fine
    let make = |alpha: u8| {
        let h = CreateWindowExW(WS_EX_LAYERED | WS_EX_NOACTIVATE, class.as_ptr(), class.as_ptr(), WS_CHILD, 0, 0, 1, 1, parent, null_mut(), instance, null());
        if !h.is_null() {
            SetLayeredWindowAttributes(h, 0, alpha, LWA_ALPHA_FLAG);
            let behind = view.unwrap_or(HWND_TOP); // just behind the icons, or simply on top of the wallpaper
            SetWindowPos(h, behind, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        h
    };
    // each new window goes straight behind the icons, so the rail, made last, ends up on top
    let (p, n, r) = (make(170), make(225), make(225));
    if p.is_null() || n.is_null() || r.is_null() {
        return Err("Windows refused to create the section windows.".into());
    }
    PANEL_WINDOW.store(p as usize, Ordering::Relaxed);
    NECK.store(n as usize, Ordering::Relaxed);
    RAIL.store(r as usize, Ordering::Relaxed);
    Ok(())
}

/// Lay the rail out from the current pills.
pub unsafe fn show_rail(x: i32, y: i32, w: i32, h: i32, radius: i32) {
    let shape = CreateRectRgn(0, 0, 0, 0);
    if let Ok(pills) = PILLS.lock() {
        for p in pills.iter() {
            let one = CreateRoundRectRgn(p.rect.left, p.rect.top, p.rect.right + 1, p.rect.bottom + 1, radius, radius);
            CombineRgn(shape, shape, one, RGN_OR);
            DeleteObject(one as _);
        }
    }
    SetWindowPos(rail(), null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    SetWindowRgn(rail(), shape, 1); // Windows now owns the region
    repaint_rail();
}

pub unsafe fn repaint_rail() {
    InvalidateRect(rail(), null(), 0);
    UpdateWindow(rail());
}

pub unsafe fn hide_panel() {
    ShowWindow(panel(), SW_HIDE);
    ShowWindow(neck(), SW_HIDE);
}

pub unsafe fn set_panel(x: i32, y: i32, w: i32, h: i32, radius: i32) {
    let (w, h) = (w.max(2), h.max(2));
    SetWindowPos(panel(), null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    SetWindowRgn(panel(), CreateRoundRectRgn(0, 0, w + 1, h + 1, radius, radius), 1);
    InvalidateRect(panel(), null(), 0);
    UpdateWindow(panel());
}

/// The liquid neck between two points, grown as far as `reach` (0 to 1).
pub unsafe fn set_neck(from: (i32, i32), to: (i32, i32), reach: f32, end_width: f32, mid_width: f32) {
    let f = (from.0 as f32, from.1 as f32);
    let t = (to.0 as f32, to.1 as f32);
    let outline = neck_outline(f, t, reach, end_width, mid_width);
    let margin = end_width as i32 + 2;
    let (left, top) = (from.0.min(to.0) - margin, from.1.min(to.1) - margin);
    let (w, h) = ((to.0 - from.0).abs() + 2 * margin, (to.1 - from.1).abs() + 2 * margin);
    let points: Vec<POINT> = outline.iter().map(|&(x, y)| POINT { x: x.round() as i32 - left, y: y.round() as i32 - top }).collect();
    SetWindowPos(neck(), null_mut(), left, top, w, h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    SetWindowRgn(neck(), CreatePolygonRgn(points.as_ptr(), points.len() as i32, WINDING), 1);
    InvalidateRect(neck(), null(), 0);
    UpdateWindow(neck());
}
