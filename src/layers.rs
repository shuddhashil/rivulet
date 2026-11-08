//! The three windows that sit between the wallpaper and the icons: the label rail, the panel
//! behind an open section's icons, and the liquid neck joining them. Windows ignores per-pixel
//! transparency in this slot, so each is shaped with a region and see-through as a whole.

use crate::rules::{blend, neck_outline};
use crate::settings::{Item, Look};
use crate::wallpaper::wide;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
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
const SOFT: u32 = 0x00B8B0A8;
const PANEL_SEE_THROUGH: u8 = 170;
const PANEL_READABLE: u8 = 240;

static RAIL: AtomicUsize = AtomicUsize::new(0);
static PANEL_WINDOW: AtomicUsize = AtomicUsize::new(0);
static NECK: AtomicUsize = AtomicUsize::new(0);
/// Colours are written blue, green, red. These follow the theme and the open section.
static PILL_TINT: AtomicU32 = AtomicU32::new(0x00261A12);
static PANEL_TINT: AtomicU32 = AtomicU32::new(0x00160E0A);
static ACCENT: AtomicU32 = AtomicU32::new(0x00F0A040);

#[derive(Clone)]
pub struct Pill {
    pub name: String,
    /// No number is shown for 0, as on the Settings label.
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
pub static SETTINGS_ITEMS: Mutex<Vec<Item>> = Mutex::new(Vec::new());
/// Indices into SETTINGS_ITEMS, or -1 for none.
pub static HOVER_ITEM: AtomicI32 = AtomicI32::new(-1);
pub static SELECTED_ITEMS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
/// Files that arrived since the section was last opened: they carry the label's dot and a patch of its colour.
pub static FRESH_ITEMS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// A new file's mark: a patch of the section colour behind it, brightest as it arrives, and the
/// label's dot at its corner. `shown` fades it in with the file.
fn fresh_mark(c: &mut dyn Canvas, patch: RECT, radius: i32, dot: (i32, i32), dpi: f32, accent: u32, own: f32, shown: f32) {
    let glow = (std::f32::consts::PI * own.clamp(0.0, 1.0)).sin();
    c.wash(patch, radius, accent, ((60.0 + 90.0 * glow) * shown) as u8);
    let size = (8.0 * dpi) as i32;
    let (x, y) = dot;
    c.wash(RECT { left: x - 1, top: y - 1, right: x + size + 1, bottom: y + size + 1 }, size + 2, WHITE, (255.0 * shown) as u8);
    c.wash(RECT { left: x, top: y, right: x + size, bottom: y + size }, size, accent, (255.0 * shown) as u8);
}
/// The selection box being drawn, inside the panel.
pub static SELECT_BOX: Mutex<Option<RECT>> = Mutex::new(None);

#[derive(Clone, Copy, PartialEq)]
pub enum Style {
    /// A file's name under its icon, the size Explorer uses.
    Name,
    Label,
    Small,
    Heading,
    Glyph,
    /// A sticky note's handwriting, at this size in points.
    Note(u8),
}

impl Style {
    pub fn id(self) -> u8 {
        match self {
            Style::Name => 0,
            Style::Label => 1,
            Style::Small => 2,
            Style::Heading => 3,
            Style::Glyph => 4,
            Style::Note(_) => 5,
        }
    }
}

/// What the rail and Settings need from a drawing surface.
pub trait Canvas {
    /// A label's background, which Liquid draws on the graphics card instead.
    fn pill(&mut self, r: RECT, radius: i32, colour: u32) {
        self.fill(r, radius, colour);
    }
    fn fill(&mut self, r: RECT, radius: i32, colour: u32);
    /// A see-through colour wash; plain Windows has none, so it falls back to a darker fill.
    fn wash(&mut self, r: RECT, radius: i32, colour: u32, alpha: u8) {
        self.fill(r, radius, blend(colour, 0x000000, 1.0 - alpha as f32 / 255.0));
    }
    fn text(&mut self, r: RECT, s: &str, colour: u32, style: Style, flags: u32);
    fn icon(&mut self, x: i32, y: i32, size: i32, icon: isize);
    /// A line round a rounded box.
    fn outline(&mut self, r: RECT, radius: i32, colour: u32, width: i32);
    /// How visible what is drawn next is, 0 to 1; files flying in use it, everything else is 1.
    fn set_fade(&mut self, _visible: f32) {}
    /// An icon out of focus: `sharp` 1 is crisp, lower is softer, as a file flying in sharpens.
    fn icon_soft(&mut self, x: i32, y: i32, size: i32, icon: isize, _wide: f32) {
        self.icon(x, y, size, icon);
    }
}

struct GdiCanvas {
    dc: HDC,
    fonts: [HFONT; 5],
}

impl GdiCanvas {
    unsafe fn new(dc: HDC, dpi: f32) -> GdiCanvas {
        SetBkMode(dc, TRANSPARENT as _);
        GdiCanvas { dc, fonts: [{ let (f, s, w) = style_font(Style::Label); font(f, s * dpi, w) }, { let (f, s, w) = style_font(Style::Small); font(f, s * dpi, w) }, { let (f, s, w) = style_font(Style::Heading); font(f, s * dpi, w) }, font("Segoe MDL2 Assets", 14.0 * dpi, 400), { let (f, s, w) = style_font(Style::Name); font(f, (s * dpi).round(), w) }] }
    }
}

impl Drop for GdiCanvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, GetStockObject(SYSTEM_FONT));
            for f in self.fonts {
                DeleteObject(f as _);
            }
        }
    }
}

impl Canvas for GdiCanvas {
    fn fill(&mut self, r: RECT, radius: i32, colour: u32) {
        unsafe {
            let brush = CreateSolidBrush(colour);
            if radius <= 0 {
                FillRect(self.dc, &r, brush);
            } else {
                let (old_brush, old_pen) = (SelectObject(self.dc, brush as _), SelectObject(self.dc, GetStockObject(NULL_PEN)));
                RoundRect(self.dc, r.left, r.top, r.right + 1, r.bottom + 1, radius, radius);
                SelectObject(self.dc, old_brush);
                SelectObject(self.dc, old_pen);
            }
            DeleteObject(brush as _);
        }
    }

    fn text(&mut self, r: RECT, s: &str, colour: u32, style: Style, flags: u32) {
        unsafe {
            let index = match style {
                Style::Label => 0,
                Style::Small => 1,
                Style::Heading => 2,
                Style::Glyph => 3,
                Style::Name | Style::Note(_) => 4,
            };
            SelectObject(self.dc, self.fonts[index] as _);
            SetTextColor(self.dc, colour);
            let w: Vec<u16> = s.encode_utf16().collect();
            let mut area = r;
            let lines = if flags & DT_WORDBREAK != 0 { DT_EDITCONTROL } else { DT_VCENTER | DT_SINGLELINE };
            DrawTextW(self.dc, w.as_ptr() as _, w.len() as i32, &mut area, flags | lines | DT_NOPREFIX);
        }
    }

    fn icon(&mut self, x: i32, y: i32, size: i32, icon: isize) {
        unsafe {
            let mode = ICON_TINT.load(Ordering::Relaxed);
            if mode == 0 {
                DrawIconEx(self.dc, x, y, icon as HICON, size, size, 0, null_mut(), DI_NORMAL);
                return;
            }
            // tinted: draw into memory, change the pixels, and lay the result down with its transparency
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = size;
            info.bmiHeader.biHeight = -size;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let dc = CreateCompatibleDC(self.dc);
            let mut bits = null_mut();
            let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if !dib.is_null() {
                let old = SelectObject(dc, dib as _);
                DrawIconEx(dc, 0, 0, icon as HICON, size, size, 0, null_mut(), DI_NORMAL);
                for pixel in std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize) {
                    *pixel = tint_pixel(*pixel, mode, tint_colour());
                }
                let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
                AlphaBlend(self.dc, x, y, size, size, dc, 0, 0, size, size, blend);
                SelectObject(dc, old);
                DeleteObject(dib as _);
            }
            DeleteDC(dc);
        }
    }

    fn outline(&mut self, r: RECT, radius: i32, colour: u32, width: i32) {
        unsafe {
            let pen = CreatePen(0, width, colour);
            let (old_pen, old_brush) = (SelectObject(self.dc, pen as _), SelectObject(self.dc, GetStockObject(NULL_BRUSH)));
            RoundRect(self.dc, r.left, r.top, r.right, r.bottom, radius, radius);
            SelectObject(self.dc, old_pen);
            SelectObject(self.dc, old_brush);
            DeleteObject(pen as _);
        }
    }
}

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

pub fn pill_tint() -> u32 {
    PILL_TINT.load(Ordering::Relaxed)
}
pub fn panel_tint() -> u32 {
    PANEL_TINT.load(Ordering::Relaxed)
}
pub fn accent() -> u32 {
    ACCENT.load(Ordering::Relaxed)
}
pub fn settings_showing() -> bool {
    SETTINGS_ITEMS.lock().is_ok_and(|i| !i.is_empty())
}

pub fn set_accent(colour: u32) {
    ACCENT.store(colour, Ordering::Relaxed);
}

/// The Settings panel needs its text readable; icon panels stay more see-through.
pub unsafe fn set_settings(items: Vec<Item>) {
    let empty = items.is_empty();
    *SETTINGS_ITEMS.lock().unwrap() = items;
    let alpha = if empty { PANEL_SEE_THROUGH } else { (PANEL_READABLE as f32 * panel_strength()).min(250.0) as u8 };
    SetLayeredWindowAttributes(panel(), 0, alpha, LWA_ALPHA_FLAG);
    InvalidateRect(panel(), null(), 0);
    let peeked = PEEK_PANEL.load(Ordering::Relaxed);
    if peeked != 0 {
        InvalidateRect(peeked as HWND, null(), 0);
    }
}

/// What the Appearance page chose, read while drawing.
static FACE: Mutex<&'static str> = Mutex::new("Segoe UI");
static TEXT_SCALE: AtomicU32 = AtomicU32::new(100);
static PANEL_PERCENT: AtomicU32 = AtomicU32::new(100);
pub static ICON_TINT: AtomicU32 = AtomicU32::new(0);
const NOT_CHOSEN: u32 = u32::MAX;
static TEXT_COLOUR: AtomicU32 = AtomicU32::new(NOT_CHOSEN);
static BORDER_COLOUR: AtomicU32 = AtomicU32::new(NOT_CHOSEN);
static TINT_COLOUR: AtomicU32 = AtomicU32::new(NOT_CHOSEN);
/// The panel as now drawn: width, height, corner radius.
static PANEL_SHAPE: Mutex<(i32, i32, i32)> = Mutex::new((0, 0, 0));
/// How visible the labels are, percent.
static RAIL_FADE: AtomicU32 = AtomicU32::new(100);

fn text_colour() -> u32 {
    match TEXT_COLOUR.load(Ordering::Relaxed) {
        NOT_CHOSEN => WHITE,
        chosen => chosen,
    }
}

pub fn tint_colour() -> u32 {
    match TINT_COLOUR.load(Ordering::Relaxed) {
        NOT_CHOSEN => accent(),
        chosen => chosen,
    }
}

pub fn rail_fade() -> f32 {
    RAIL_FADE.load(Ordering::Relaxed) as f32 / 100.0
}

pub unsafe fn set_rail_fade(percent: u32) {
    RAIL_FADE.store(percent, Ordering::Relaxed);
    if !crate::pretty::is_on() {
        SetLayeredWindowAttributes(rail(), 0, (225 * percent / 100).max(1) as u8, LWA_ALPHA_FLAG);
    } else {
        crate::pretty::repaint_rail();
    }
}

pub fn apply_look(cfg: &crate::rules::Config) {
    if let Ok(mut face) = FACE.lock() {
        *face = match cfg.font.as_str() {
            "calibri" => "Calibri",
            "arial" => "Arial",
            "verdana" => "Verdana",
            "tahoma" => "Tahoma",
            _ => "Segoe UI",
        };
    }
    TEXT_SCALE.store(match cfg.text_size.as_str() { "small" => 90, "large" => 115, _ => 100 }, Ordering::Relaxed);
    PANEL_PERCENT.store(cfg.glass, Ordering::Relaxed);
    TEXT_COLOUR.store(cfg.text_colour.unwrap_or(NOT_CHOSEN), Ordering::Relaxed);
    BORDER_COLOUR.store(cfg.border_colour.unwrap_or(NOT_CHOSEN), Ordering::Relaxed);
    TINT_COLOUR.store(cfg.tint_colour.unwrap_or(NOT_CHOSEN), Ordering::Relaxed);
    ICON_TINT.store(match cfg.icon_tint.as_str() { "section" => 1, "mono" => 2, _ => 0 }, Ordering::Relaxed);
    crate::settings::ICON_SIZE.store(match cfg.icon_size.as_str() { "small" => 0, "large" => 2, _ => 1 }, Ordering::Relaxed);
}

/// How much a panel of files lets through, against normal being 1.
pub fn panel_strength() -> f32 {
    if files_showing() { PANEL_PERCENT.load(Ordering::Relaxed) as f32 / 100.0 } else { 1.0 }
}

/// True when the panel holds files (own-icons mode) rather than a Settings page.
pub fn files_showing() -> bool {
    SETTINGS_ITEMS.lock().is_ok_and(|i| i.iter().any(|x| matches!(x.look, Look::File(..))))
}

/// How high a line of file name is, in pixels.
pub fn name_line(dpi: f32) -> i32 {
    ((12.0 * dpi * TEXT_SCALE.load(Ordering::Relaxed) as f32 / 100.0).round() * 1.36) as i32
}

/// Each style's font: name, size at 100% scale, weight.
pub fn style_font(style: Style) -> (&'static str, f32, i32) {
    let face = FACE.lock().map_or("Segoe UI", |f| *f);
    let scale = TEXT_SCALE.load(Ordering::Relaxed) as f32 / 100.0;
    match style {
        Style::Name => (face, 12.0 * scale, 400),
        Style::Label => (face, 13.0 * scale, 600),
        Style::Small => (face, 11.0 * scale, 400),
        Style::Heading => (face, 14.0 * scale, 700),
        Style::Glyph => ("Segoe MDL2 Assets", 14.0, 400),
        Style::Note(size) => ("Segoe Print", size as f32, 400),
    }
}

/// An icon's pixel, colours already mixed with transparency, in grey or the section's colour.
pub fn tint_pixel(pixel: u32, mode: u32, colour: u32) -> u32 {
    if mode == 0 {
        return pixel;
    }
    let (a, r, g, b) = (pixel >> 24, (pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255);
    let grey = (r * 30 + g * 59 + b * 11) / 100;
    if mode == 2 {
        return (a << 24) | (grey << 16) | (grey << 8) | grey;
    }
    // colour is red in its low byte; three parts tint to one part of the icon's own shade
    let mix = |own: u32, tint: u32| (own + 3 * (tint * grey / 255) * 2).min(4 * a.max(1)) / 4;
    (a << 24) | (mix(r, colour & 255).min(a) << 16) | (mix(g, (colour >> 8) & 255).min(a) << 8) | mix(b, (colour >> 16) & 255).min(a)
}

unsafe fn font(name: &str, size: f32, weight: i32) -> HFONT {
    CreateFontW(-size as i32, 0, 0, 0, weight, 0, 0, 0, 0, 0, 0, CLEARTYPE_QUALITY as u32, 0, wide(name).as_ptr())
}

pub fn pill_colour(p: &Pill, base: u32) -> u32 {
    if p.active {
        blend(base, p.colour, 0.55)
    } else if p.hover {
        blend(base, p.colour, 0.28)
    } else {
        base
    }
}

// ---------- Peek: the same labels and panel in windows of their own, above everything

static PEEK_ON: AtomicBool = AtomicBool::new(false);
static PEEK_RAIL: AtomicUsize = AtomicUsize::new(0);
static PEEK_PANEL: AtomicUsize = AtomicUsize::new(0);

pub fn peeking() -> bool {
    PEEK_ON.load(Ordering::Relaxed)
}

/// These use the sections' own window class, so the mouse handling treats them as the sections.
pub unsafe fn set_peek(on: bool) {
    if PEEK_ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if on {
        let class = wide(CLASS);
        let make = || {
            let h = CreateWindowExW(WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), GetModuleHandleW(null()), null());
            SetLayeredWindowAttributes(h, 0, 248, LWA_ALPHA_FLAG);
            h as usize
        };
        PEEK_PANEL.store(make(), Ordering::Relaxed);
        PEEK_RAIL.store(make(), Ordering::Relaxed);
    } else {
        for slot in [&PEEK_RAIL, &PEEK_PANEL] {
            let h = slot.swap(0, Ordering::Relaxed);
            if h != 0 {
                DestroyWindow(h as HWND);
            }
        }
    }
}

/// From the sections' parent window to the screen.
unsafe fn parent_to_screen(x: i32, y: i32) -> (i32, i32) {
    let mut p = POINT { x, y };
    ClientToScreen(GetParent(rail()), &mut p);
    (p.x, p.y)
}

unsafe fn peek_place(slot: &AtomicUsize, x: i32, y: i32, w: i32, h: i32, shape: HRGN) {
    let window = slot.load(Ordering::Relaxed) as HWND;
    if window.is_null() {
        DeleteObject(shape as _);
        return;
    }
    let (sx, sy) = parent_to_screen(x, y);
    SetWindowPos(window, HWND_TOPMOST, sx, sy, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    SetWindowRgn(window, shape, 1);
    InvalidateRect(window, null(), 0);
    UpdateWindow(window);
}

pub fn paint_rail_on(c: &mut dyn Canvas, dpi: f32, base: u32) {
    let scaled = |v: f32| (v * dpi) as i32;
    if let Ok(pills) = PILLS.lock() {
        for p in pills.iter() {
            let r = p.rect;
            c.pill(r, scaled(14.0), pill_colour(p, base));
            let icon = RECT { left: r.left + scaled(10.0), top: r.top, right: r.left + scaled(32.0), bottom: r.bottom };
            c.text(icon, &p.glyph.to_string(), if p.active { WHITE } else { p.colour }, Style::Glyph, DT_LEFT);
            let mut area = RECT { left: r.left + scaled(34.0), top: r.top, right: r.right - scaled(10.0), bottom: r.bottom };
            if p.count > 0 {
                c.text(area, &p.count.to_string(), if p.active { 0x00F0F0F0 } else { SOFT }, Style::Label, DT_RIGHT);
                area.right -= scaled(24.0);
            }
            c.text(area, &p.name, text_colour(), Style::Label, DT_LEFT | DT_END_ELLIPSIS);
            if p.fresh {
                let (size, x, y) = (scaled(7.0), r.left + scaled(25.0), r.top + scaled(6.0));
                c.fill(RECT { left: x - 1, top: y - 1, right: x + size + 1, bottom: y + size + 1 }, size + 2, WHITE);
                c.fill(RECT { left: x, top: y, right: x + size, bottom: y + size }, size, p.colour);
            }
        }
    }
}

/// How far the opening has come, 0 to 1 (as f32 bits); 1 when nothing moves.
static REVEAL: AtomicU32 = AtomicU32::new(0x3F80_0000);

pub fn set_reveal(at: f32) {
    REVEAL.store(at.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

/// Where the neck meets the panel, and the panel's size, in the panel's own pixels.
static REVEAL_FROM: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, 1, 1));

pub fn set_reveal_from(x: i32, y: i32, w: i32, h: i32) {
    if let Ok(mut from) = REVEAL_FROM.lock() {
        *from = (x, y, w.max(1), h.max(1));
    }
}

/// The animation styles, each a whole way of opening: the panel's own motion and the files' together.
pub const POUR: u32 = 0;
pub const UNFOLD: u32 = 1;
pub const CURTAIN: u32 = 2;
pub const POP: u32 = 3;
pub const DRIFT: u32 = 4;
pub const TRACE: u32 = 5;
static ENTRANCE: AtomicU32 = AtomicU32::new(POUR);

pub fn set_entrance(kind: u32) {
    ENTRANCE.store(kind, Ordering::Relaxed);
}

/// True while a section closes in its style: files leave by running their entrance backwards,
/// except where leaving has its own way (Drift's leaves fall away downward).
static LEAVING: AtomicBool = AtomicBool::new(false);

pub fn set_leaving(on: bool) {
    LEAVING.store(on, Ordering::Relaxed);
}

pub fn entrance() -> u32 {
    ENTRANCE.load(Ordering::Relaxed)
}

pub fn entrance_named(name: &str) -> u32 {
    match name {
        "unfold" => UNFOLD,
        "curtain" => CURTAIN,
        "pop" => POP,
        "drift" => DRIFT,
        "trace" => TRACE,
        _ => POUR,
    }
}

/// What the glass shows of the panel while it opens: how visible it is, how far the style's own
/// effect has come, and which effect (the shader's fade x, z and w). (1, 0, 0) is simply open.
static STAGE: Mutex<[f32; 3]> = Mutex::new([1.0, 0.0, 0.0]);

pub fn set_stage(show: f32, stage: f32, kind: u32) {
    if let Ok(mut s) = STAGE.lock() {
        *s = [show, stage, kind as f32];
    }
}

pub fn stage() -> [f32; 3] {
    STAGE.lock().map(|s| *s).unwrap_or([1.0, 0.0, 0.0])
}

fn smooth(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn out_cubic(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

/// Ends exactly at 1, running about 10% past on the way.
fn back_out(x: f32) -> f32 {
    let q = x.clamp(0.0, 1.0) - 1.0;
    1.0 + 3.0 * q * q * q + 2.0 * q * q
}

/// Curtain: how far the circle opening from the neck has spread, as a share of the way to the
/// panel's furthest corner (a little past 1, so the soft edge clears it).
pub fn curtain_ring(at: f32) -> f32 {
    out_cubic(at / 0.75) * 1.12
}

/// Trace: 0 to 1 while the outline draws itself round from the neck, then 1 to 2 while the inside fills.
pub fn trace_stage(at: f32) -> f32 {
    smooth(at / 0.45) + smooth((at - 0.45) / 0.2)
}

/// Unfold: how much of the panel's height is open, one fold of `folds` at a time.
pub fn unfold_height(at: f32, folds: u32) -> f32 {
    let n = folds.max(1) as f32;
    let a = (at / 0.75).clamp(0.0, 1.0) * n;
    let done = a.floor().min(n - 1.0);
    ((done + back_out(a - done)) / n).min(1.0 + 0.04 / n)
}

fn centre(r: &RECT) -> (f32, f32) {
    ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0)
}

/// How far along its own entrance each item is, 0 to 1; everything but files is simply there.
pub fn reveal_plan(items: &[Item]) -> Vec<f32> {
    let at = f32::from_bits(REVEAL.load(Ordering::Relaxed));
    if at >= 1.0 {
        return vec![1.0; items.len()];
    }
    let (nx, ny, pw, ph) = REVEAL_FROM.lock().map(|f| (f.0 as f32, f.1 as f32, f.2 as f32, f.3 as f32)).unwrap_or((0.0, 0.0, 1.0, 1.0));
    let shown = |i: &Item| matches!(i.look, Look::File(..)) && i.rect.right > i.rect.left;
    let files: Vec<&Item> = items.iter().filter(|i| shown(i)).collect();
    let count = files.len().max(2) as f32 - 1.0;
    let from_neck = |c: (f32, f32)| ((c.0 - nx).powi(2) + (c.1 - ny).powi(2)).sqrt();
    let furthest = files.iter().map(|i| from_neck(centre(&i.rect))).fold(1.0, f32::max);
    // the middle of all the files, for Pop's centre-out pulse
    let n = files.len().max(1) as f32;
    let mx = files.iter().map(|i| centre(&i.rect).0).sum::<f32>() / n;
    let my = files.iter().map(|i| centre(&i.rect).1).sum::<f32>() / n;
    let from_mid = |c: (f32, f32)| ((c.0 - mx).powi(2) + (c.1 - my).powi(2)).sqrt();
    let widest = files.iter().map(|i| from_mid(centre(&i.rect))).fold(1.0, f32::max);
    let corner = [(0.0, 0.0), (pw, 0.0), (0.0, ph), (pw, ph)].iter().map(|&c| from_neck(c)).fold(1.0, f32::max);
    let kind = entrance();
    let mut rank = 0.0;
    items
        .iter()
        .map(|i| {
            if !matches!(i.look, Look::File(..)) {
                return 1.0;
            }
            let c = centre(&i.rect);
            let order = rank / count;
            let shuffled = (rank * 0.618_034) % 1.0;
            if shown(i) {
                rank += 1.0;
            }
            let (delay, span) = match kind {
                // the fold reaches the item's top at that share of the fold's time
                UNFOLD => (0.75 * (i.rect.top as f32 / ph).clamp(0.0, 1.0), 0.25),
                // each one shows as the spreading circle passes it
                CURTAIN => return ((curtain_ring(at) - from_neck(c) / corner) / 0.18).clamp(0.0, 1.0),
                POP => (0.4 * from_mid(c) / widest, 0.5),
                // leaves fall in no particular order
                DRIFT => (0.45 * shuffled, 0.55),
                // they light up once the glass has filled
                TRACE => (0.6 + 0.2 * order, 0.2),
                _ => (0.35 * from_neck(c) / furthest, 0.5),
            };
            ((at - delay) / span).clamp(0.0, 1.0)
        })
        .collect()
}

/// One file part-way into place: its icon's centre and size, how much of its width shows (a hinge
/// opening), and how visible the icon and the name are.
#[derive(Debug, PartialEq)]
pub struct Flight {
    pub x: i32,
    pub y: i32,
    pub size: i32,
    pub wide: f32,
    pub icon: f32,
    pub name: f32,
}

/// A file `own` of the way into `home`, its icon's resting centre, in the current style. Names never
/// move or scale (their sharp text would shimmer); they fade in as the icon settles.
pub fn flight(own: f32, home: (i32, i32), size: i32) -> Flight {
    if own >= 1.0 {
        return Flight { x: home.0, y: home.1, size, wide: 1.0, icon: 1.0, name: 1.0 };
    }
    let s = size as f32;
    let at = |dx: f32, dy: f32| (home.0 + dx.round() as i32, home.1 + dy.round() as i32);
    let (pos, scale, wide, icon, name) = match entrance() {
        // a hinge at the icon's left edge swings open
        UNFOLD => {
            let wide = (82f32.to_radians() * (1.0 - smooth(own))).cos();
            (home, 1.0, wide, smooth(own * 5.0), smooth((own - 0.7) / 0.3))
        }
        // simply uncovered by the spreading circle
        CURTAIN => (home, 1.0, 1.0, smooth(own * 1.4), smooth((own - 0.3) / 0.7)),
        // a pulse out from the middle: 55% up past full and back
        POP => (home, 0.55 + 0.45 * back_out(own), 1.0, smooth(own * 4.0), smooth((own - 0.45) / 0.35)),
        // falls from above, swaying side to side like a leaf, slowing as it lands
        DRIFT => {
            let left = 1.0 - out_cubic(own);
            let fall = if LEAVING.load(Ordering::Relaxed) { 1.4 } else { -1.4 }; // in from above, away below
            (at((own * std::f32::consts::PI * 2.5).sin() * s * 0.35 * left, s * fall * left), 1.0, 1.0, smooth(own * 2.5), smooth((own - 0.6) / 0.4))
        }
        // lights up where it is
        TRACE => (home, 0.9 + 0.1 * out_cubic(own), 1.0, smooth(own * 1.5), smooth((own - 0.4) / 0.6)),
        // pour: the panel carries the motion from the neck; each file lifts into its slot
        _ => (at(0.0, 24f32.min(0.45 * s) * (1.0 - out_cubic(own))), 1.0, 1.0, smooth(own * 3.0), smooth((own - 0.35) / 0.4)),
    };
    Flight { x: pos.0, y: pos.1, size: ((s * scale).round() as i32).max(2), wide, icon, name }
}

pub fn paint_settings_on(c: &mut dyn Canvas, dpi: f32, base: u32, accent: u32) {
    let radius = (10.0 * dpi) as i32;
    let idle = blend(base, WHITE, 0.10);
    let chosen = SELECTED_ITEMS.lock().map(|s| s.clone()).unwrap_or_default();
    let fresh = FRESH_ITEMS.lock().map(|s| s.clone()).unwrap_or_default();
    if let Ok(items) = SETTINGS_ITEMS.lock() {
        let reveal = reveal_plan(&items);
        for (index, item) in items.iter().enumerate() {
            let r = item.rect;
            let own = reveal[index];
            if r.right <= r.left || own <= 0.0 {
                continue; // scrolled out of view, or not yet out of the neck
            }
            match &item.look {
                Look::Thumb => c.wash(r, r.right - r.left, WHITE, 110),
                Look::Swatch(colour) => {
                    c.fill(r, radius, *colour);
                    c.outline(r, radius, 0x00D0D0D0, 1);
                }
                Look::Symbol(glyph) => {
                    c.fill(r, radius, idle);
                    c.text(r, &glyph.to_string(), WHITE, Style::Glyph, DT_CENTER);
                }
                Look::File(name, icon) if item.row => {
                    let inset = (2.0 * dpi) as i32;
                    let selected = chosen.contains(&(index as i32));
                    if own >= 1.0 && (selected || HOVER_ITEM.load(Ordering::Relaxed) == index as i32) {
                        c.wash(RECT { left: r.left + inset, top: r.top + inset / 2, right: r.right - inset, bottom: r.bottom - inset / 2 }, (6.0 * dpi) as i32, accent, if selected { 140 } else { 90 });
                    }
                    let size = crate::settings::list_icon_pixels(dpi);
                    let side = (8.0 * dpi) as i32;
                    let f = flight(own, (r.left + side + size / 2, r.top + (r.bottom - r.top) / 2), size);
                    if fresh.contains(&(index as i32)) {
                        let patch = RECT { left: r.left + inset, top: r.top + inset / 2, right: r.right - inset, bottom: r.bottom - inset / 2 };
                        fresh_mark(c, patch, (6.0 * dpi) as i32, (r.left + side + size - (2.0 * dpi) as i32, r.top + (2.0 * dpi) as i32), dpi, accent, own, f.icon);
                    }
                    c.set_fade(f.icon);
                    c.icon_soft(f.x - f.size / 2, f.y - f.size / 2, f.size, *icon, f.wide);
                    if f.name > 0.0 {
                        c.set_fade(f.name);
                        let name_left = r.left + side * 2 + size;
                        let detail_w = (170.0 * dpi) as i32;
                        c.text(RECT { left: name_left, top: r.top, right: r.right - detail_w - side, bottom: r.bottom }, name, text_colour(), Style::Name, DT_LEFT | DT_END_ELLIPSIS);
                        c.text(RECT { left: r.right - detail_w, top: r.top, right: r.right - side, bottom: r.bottom }, &item.detail, SOFT, Style::Small, DT_RIGHT);
                    }
                    c.set_fade(1.0);
                }
                Look::Heading(t) => c.text(r, t, text_colour(), Style::Heading, DT_LEFT | DT_END_ELLIPSIS),
                Look::Text(t) => c.text(RECT { right: r.right + (200.0 * dpi) as i32, ..r }, t, text_colour(), Style::Label, DT_LEFT),
                Look::Note(t) => c.text(r, t, if TEXT_COLOUR.load(Ordering::Relaxed) == NOT_CHOSEN { SOFT } else { text_colour() }, Style::Small, DT_LEFT | DT_END_ELLIPSIS),
                Look::Value(t) => c.text(r, t, WHITE, Style::Label, DT_CENTER),
                Look::File(name, icon) => {
                    let selected = chosen.contains(&(index as i32));
                    let hovered = HOVER_ITEM.load(Ordering::Relaxed) == index as i32;
                    if own >= 1.0 && (selected || hovered) {
                        let inset = (3.0 * dpi) as i32;
                        let wash = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
                        c.wash(wash, (10.0 * dpi) as i32, accent, if selected { 140 } else { 90 });
                    }
                    let size = crate::settings::icon_pixels(dpi);
                    let top = r.top + (6.0 * dpi) as i32;
                    let f = flight(own, ((r.left + r.right) / 2, top + size / 2), size);
                    if fresh.contains(&(index as i32)) {
                        let inset = (3.0 * dpi) as i32;
                        let patch = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
                        fresh_mark(c, patch, (10.0 * dpi) as i32, (r.right - (14.0 * dpi) as i32, r.top + (7.0 * dpi) as i32), dpi, accent, own, f.icon);
                    }
                    c.set_fade(f.icon);
                    c.icon_soft(f.x - f.size / 2, f.y - f.size / 2, f.size, *icon, f.wide);
                    if f.name > 0.0 {
                        c.set_fade(f.name);
                        let inset = (5.0 * dpi) as i32;
                        let words = RECT { left: r.left + inset, top: top + size + inset / 2, right: r.right - inset, bottom: top + size + inset / 2 + 2 * name_line(dpi) + 2 };
                        c.text(words, name, text_colour(), Style::Name, DT_CENTER | DT_WORDBREAK | DT_END_ELLIPSIS);
                    }
                    c.set_fade(1.0);
                }
                Look::Button(t) => {
                    c.fill(r, radius, idle);
                    c.text(r, t, WHITE, Style::Label, DT_CENTER);
                }
                Look::Chip(t, on) => {
                    c.fill(r, radius, if *on { blend(base, accent, 0.8) } else { idle });
                    c.text(r, t, WHITE, Style::Small, DT_CENTER);
                }
                Look::Switch(on) => {
                    let h = r.bottom - r.top;
                    c.fill(r, h, if *on { accent } else { idle });
                    let inset = (3.0 * dpi) as i32;
                    let knob = h - 2 * inset;
                    let x = if *on { r.right - inset - knob } else { r.left + inset };
                    c.fill(RECT { left: x, top: r.top + inset, right: x + knob, bottom: r.top + inset + knob }, knob, WHITE);
                }
            }
        }
    }
    let border = BORDER_COLOUR.load(Ordering::Relaxed);
    if border != NOT_CHOSEN {
        if let Ok((w, h, corner)) = PANEL_SHAPE.lock().map(|s| *s) {
            let width = (1.5 * dpi).round() as i32;
            let inset = width / 2;
            c.outline(RECT { left: inset, top: inset, right: w - inset - 1, bottom: h - inset - 1 }, corner * 2, border, width);
        }
    }
    if let Ok(Some(area)) = SELECT_BOX.lock().map(|b| *b) {
        c.wash(area, (4.0 * dpi) as i32, accent, 70);
    }
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
            let (pill, shade) = (PILL_TINT.load(Ordering::Relaxed), PANEL_TINT.load(Ordering::Relaxed));
            let dpi = GetDpiForWindow(hwnd) as f32 / 96.0;
            let mut canvas = GdiCanvas::new(dc, dpi);
            if hwnd == rail() || hwnd as usize == PEEK_RAIL.load(Ordering::Relaxed) {
                paint_rail_on(&mut canvas, dpi, pill);
            } else if hwnd == panel() || hwnd as usize == PEEK_PANEL.load(Ordering::Relaxed) {
                canvas.fill(whole, 0, blend(shade, accent, 0.06));
                let frame = CreateRectRgn(0, 0, 0, 0);
                if GetWindowRgn(hwnd, frame) != 0 {
                    let brush = CreateSolidBrush(blend(shade, accent, 0.75));
                    FrameRgn(dc, frame, brush, 1, 1);
                    DeleteObject(brush as _);
                }
                DeleteObject(frame as _);
                paint_settings_on(&mut canvas, dpi, pill, accent);
            } else if hwnd == neck() {
                canvas.fill(whole, 0, blend(pill, accent, 0.55));
            }
            drop(canvas);
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
    let (p, n, r) = (make(PANEL_SEE_THROUGH), make(225), make(225));
    if p.is_null() || n.is_null() || r.is_null() {
        return Err("Windows refused to create the section windows.".into());
    }
    PANEL_WINDOW.store(p as usize, Ordering::Relaxed);
    NECK.store(n as usize, Ordering::Relaxed);
    RAIL.store(r as usize, Ordering::Relaxed);
    Ok(())
}

/// Pretty mode gets one clear window of its own behind the icons, covering the whole desktop.
/// If anything refuses, the plain look simply carries on.
pub unsafe fn start_pretty() -> Result<(), String> {
    crate::pretty::stop();
    let Some((parent, view)) = parent_view() else { return Err("the desktop is not ready".into()) };
    let instance = GetModuleHandleW(null());
    let class = wide(CLASS);
    let mut whole = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetClientRect(parent, &mut whole);
    let host = CreateWindowExW(WS_EX_NOACTIVATE, class.as_ptr(), class.as_ptr(), WS_CHILD | WS_VISIBLE, 0, 0, whole.right, whole.bottom, parent, null_mut(), instance, null());
    if host.is_null() {
        return Err("the window would not open".into());
    }
    SetWindowPos(host, view.unwrap_or(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    let dpi = GetDpiForWindow(host) as f32 / 96.0;
    let started = crate::pretty::start(host, dpi);
    if started.is_err() {
        DestroyWindow(host);
    } else {
        ShowWindow(rail(), SW_HIDE);
        hide_plain_panel();
    }
    started
}

unsafe fn hide_plain_panel() {
    ShowWindow(panel(), SW_HIDE);
    ShowWindow(neck(), SW_HIDE);
}

/// Lay the rail out from the current pills.
pub unsafe fn show_rail(x: i32, y: i32, w: i32, h: i32, radius: i32) {
    if peeking() {
        let shape = CreateRectRgn(0, 0, 0, 0);
        if let Ok(pills) = PILLS.lock() {
            for p in pills.iter() {
                let one = CreateRoundRectRgn(p.rect.left, p.rect.top, p.rect.right + 1, p.rect.bottom + 1, radius, radius);
                CombineRgn(shape, shape, one, RGN_OR);
                DeleteObject(one as _);
            }
        }
        peek_place(&PEEK_RAIL, x, y, w, h, shape);
    }
    if crate::pretty::is_on() {
        return crate::pretty::show_rail(x, y, w, h);
    }
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

pub unsafe fn hide_rail() {
    if crate::pretty::is_on() {
        return crate::pretty::hide_rail();
    }
    ShowWindow(rail(), SW_HIDE);
}

pub unsafe fn repaint_rail() {
    let peeked = PEEK_RAIL.load(Ordering::Relaxed);
    if peeked != 0 {
        InvalidateRect(peeked as HWND, null(), 0);
    }
    if crate::pretty::is_on() {
        return crate::pretty::repaint_rail();
    }
    InvalidateRect(rail(), null(), 0);
    UpdateWindow(rail());
}

pub unsafe fn hide_panel() {
    let peeked = PEEK_PANEL.load(Ordering::Relaxed);
    if peeked != 0 {
        ShowWindow(peeked as HWND, SW_HIDE);
    }
    if crate::pretty::is_on() {
        return crate::pretty::hide_panel();
    }
    hide_plain_panel();
}

pub unsafe fn set_panel(x: i32, y: i32, w: i32, h: i32, radius: i32) {
    if let Ok(mut shape) = PANEL_SHAPE.lock() {
        *shape = (w, h, radius);
    }
    if peeking() {
        peek_place(&PEEK_PANEL, x, y, w.max(2), h.max(2), CreateRoundRectRgn(0, 0, w + 1, h + 1, radius, radius));
    }
    if crate::pretty::is_on() {
        return crate::pretty::set_panel(x, y, w, h, radius);
    }
    let (w, h) = (w.max(2), h.max(2));
    SetWindowPos(panel(), null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    SetWindowRgn(panel(), CreateRoundRectRgn(0, 0, w + 1, h + 1, radius, radius), 1);
    InvalidateRect(panel(), null(), 0);
    UpdateWindow(panel());
}

/// The liquid neck between two points, grown as far as `reach` (0 to 1).
pub unsafe fn set_neck(from: (i32, i32), to: (i32, i32), reach: f32, end_width: f32, mid_width: f32) {
    if crate::pretty::is_on() {
        return crate::pretty::set_neck(from, to, reach, end_width, mid_width);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_animation_style_brings_files_in_its_own_way_and_lands_home() {
        let file = |x: i32, y: i32| Item { rect: RECT { left: x, top: y, right: x + 80, bottom: y + 90 }, look: Look::File("f".into(), 0), action: None, row: false, detail: String::new() };
        let items = vec![file(0, 0), file(400, 0), file(0, 300), file(400, 300), file(200, 150), Item { look: Look::Heading("h".into()), ..file(0, 0) }];
        set_reveal_from(0, 40, 480, 390);
        let home = (300, 200);
        for kind in [POUR, UNFOLD, CURTAIN, POP, DRIFT, TRACE] {
            set_entrance(kind);
            set_reveal(0.0);
            assert!(reveal_plan(&items)[..5].iter().all(|&k| k == 0.0), "{kind}: nothing yet");
            set_reveal(0.5);
            assert_eq!(reveal_plan(&items)[5], 1.0); // only files move
            set_reveal(1.0);
            assert!(reveal_plan(&items).iter().all(|&k| k == 1.0));
            let start = flight(0.0, home, 48);
            assert!(start.icon == 0.0 && start.name == 0.0, "{kind}: hidden at first");
            assert_eq!(flight(1.0, home, 48), Flight { x: 300, y: 200, size: 48, wide: 1.0, icon: 1.0, name: 1.0 });
        }
        // pour lifts each file up from below its slot
        set_entrance(POUR);
        assert!(flight(0.2, home, 48).y > home.1 && flight(0.2, home, 48).x == home.0);
        // unfold swings each icon open from a sliver
        set_entrance(UNFOLD);
        assert!(flight(0.05, home, 48).wide < 0.3 && flight(0.6, home, 48).wide > 0.5);
        // pop: the middle first, and it grows past full size before settling
        set_entrance(POP);
        set_reveal(0.2);
        let k = reveal_plan(&items);
        assert!(k[4] > k[0] && k[4] > k[3], "{k:?}");
        assert!((1..100).any(|i| flight(i as f32 / 100.0, home, 48).size > 48));
        // drift falls from above, and when leaving falls away below
        set_entrance(DRIFT);
        assert!(flight(0.2, home, 48).y < home.1);
        set_leaving(true);
        assert!(flight(0.2, home, 48).y > home.1);
        set_leaving(false);
        // curtain uncovers the nearest the neck first
        set_entrance(CURTAIN);
        set_reveal(0.3);
        let k = reveal_plan(&items);
        assert!(k[0] > k[3], "{k:?}");
        // trace: files wait for the outline and the fill
        set_entrance(TRACE);
        set_reveal(0.55);
        assert!(reveal_plan(&items)[..5].iter().all(|&k| k == 0.0));
        assert!(trace_stage(0.3) < 1.0 && trace_stage(0.5) > 1.0 && (trace_stage(1.0) - 2.0).abs() < 1e-5);
        assert!(curtain_ring(1.0) > 1.0 && curtain_ring(0.0) == 0.0);
        assert!((unfold_height(1.0, 3) - 1.0).abs() < 1e-5 && unfold_height(0.1, 3) < 0.5);
        set_entrance(POUR);
        set_reveal(1.0);
    }
}
