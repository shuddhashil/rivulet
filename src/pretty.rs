//! Pretty mode: smooth corners and soft shadows, for people happy to spend 60-90 MB on them.
//! Everything is drawn with GDI+ into memory and shown through DirectComposition, the one route
//! that allows per-pixel transparency behind the icons. The graphics libraries are loaded by hand
//! so that with this mode off they are never in memory at all.

use crate::layers::{self, Canvas, Style};
use crate::rules::{blend, neck_outline};
use crate::wallpaper::wide;
use std::ffi::c_void;
use std::result::Result::Ok; // GdiPlus also defines a constant called Ok
use std::sync::Mutex;
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::HWND as ComHwnd;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{DT_CENTER, DT_END_ELLIPSIS, DT_RIGHT};
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const PARGB: i32 = 0xE200B;
const UNIT_PIXEL: Unit = 2;
const FAR_AWAY: f32 = -20000.0;

fn argb(bgr: u32, alpha: u8) -> u32 {
    ((alpha as u32) << 24) | ((bgr & 0xFF) << 16) | (bgr & 0xFF00) | ((bgr >> 16) & 0xFF)
}

/// Memory to draw into, with GDI+ set up over it.
struct Page {
    w: i32,
    h: i32,
    pixels: Vec<u32>,
    bitmap: *mut GpBitmap,
    g: *mut GpGraphics,
}

impl Page {
    unsafe fn new(w: i32, h: i32) -> Page {
        let mut pixels = vec![0u32; (w * h) as usize];
        let (mut bitmap, mut g) = (std::ptr::null_mut(), std::ptr::null_mut());
        GdipCreateBitmapFromScan0(w, h, w * 4, PARGB, pixels.as_mut_ptr() as *mut u8, &mut bitmap);
        GdipGetImageGraphicsContext(bitmap as _, &mut g);
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        GdipSetTextRenderingHint(g, TextRenderingHintAntiAlias);
        Page { w, h, pixels, bitmap, g }
    }

    unsafe fn clear(&mut self) {
        GdipResetClip(self.g);
        GdipResetWorldTransform(self.g);
        self.pixels.fill(0);
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteGraphics(self.g);
            GdipDisposeImage(self.bitmap as _);
        }
    }
}

unsafe fn round_path(x: f32, y: f32, w: f32, h: f32, diameter: f32) -> *mut GpPath {
    let mut path = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    let d = diameter.min(w).min(h).max(1.0);
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    path
}

/// Lets the rail and Settings draw here with the same code as the plain look.
struct PlusCanvas {
    g: *mut GpGraphics,
    dpi: f32,
    alpha: u8,
}

impl Canvas for PlusCanvas {
    fn fill(&mut self, r: RECT, radius: i32, colour: u32) {
        unsafe {
            let mut brush = std::ptr::null_mut();
            GdipCreateSolidFill(argb(colour, self.alpha), &mut brush);
            let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
            if radius <= 0 {
                GdipFillRectangle(self.g, brush as _, r.left as f32, r.top as f32, w, h);
            } else {
                let path = round_path(r.left as f32, r.top as f32, w, h, radius as f32);
                GdipFillPath(self.g, brush as _, path);
                GdipDeletePath(path);
            }
            GdipDeleteBrush(brush as _);
        }
    }

    fn text(&mut self, r: RECT, s: &str, colour: u32, style: Style, flags: u32) {
        unsafe {
            let (name, size, bold) = match style {
                Style::Label => ("Segoe UI", 13.0, 1),
                Style::Small => ("Segoe UI", 11.0, 0),
                Style::Heading => ("Segoe UI", 14.0, 1),
                Style::Glyph => ("Segoe MDL2 Assets", 14.0, 0),
            };
            let (mut family, mut font, mut brush, mut format) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            GdipCreateFontFamilyFromName(wide(name).as_ptr(), std::ptr::null_mut(), &mut family);
            GdipCreateFont(family, size * self.dpi, bold, UNIT_PIXEL, &mut font);
            GdipCreateSolidFill(argb(colour, 255), &mut brush);
            GdipCreateStringFormat(StringFormatFlagsNoWrap as i32, 0, &mut format);
            let align = if flags & DT_CENTER != 0 { StringAlignmentCenter } else if flags & DT_RIGHT != 0 { StringAlignmentFar } else { StringAlignmentNear };
            GdipSetStringFormatAlign(format, align);
            GdipSetStringFormatLineAlign(format, StringAlignmentCenter);
            if flags & DT_END_ELLIPSIS != 0 {
                GdipSetStringFormatTrimming(format, StringTrimmingEllipsisCharacter);
            }
            let area = RectF { X: r.left as f32, Y: r.top as f32, Width: (r.right - r.left) as f32, Height: (r.bottom - r.top) as f32 };
            GdipDrawString(self.g, wide(s).as_ptr(), -1, font, &area, format, brush as _);
            GdipDeleteStringFormat(format);
            GdipDeleteBrush(brush as _);
            GdipDeleteFont(font);
            GdipDeleteFontFamily(family);
        }
    }
}

/// One thing on screen: a picture the compositor can move around for free.
struct Sprite {
    visual: IDCompositionVisual,
    swap: Option<IDXGISwapChain1>,
    page: Option<Page>,
}

struct Engine {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    rail: Sprite,
    panel: Sprite,
    neck: Sprite,
    rail_rect: (i32, i32, i32, i32),
    panel_size: (i32, i32),
    dpi: f32,
    _gdiplus: usize,
}

/// Only ever touched behind the mutex below, one thread at a time.
unsafe impl Send for Engine {}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

pub fn is_on() -> bool {
    ENGINE.lock().map(|e| e.is_some()).unwrap_or(false)
}

pub fn stop() {
    if let Ok(mut e) = ENGINE.lock() {
        *e = None;
    }
}

unsafe fn system_function(dll: &str, name: &[u8]) -> Result<*const c_void, String> {
    let module = LoadLibraryW(wide(dll).as_ptr());
    if module.is_null() {
        return Err(format!("{dll} is missing"));
    }
    GetProcAddress(module, name.as_ptr()).map(|f| f as *const c_void).ok_or_else(|| format!("{dll} is missing a function"))
}

/// Start the graphics libraries on this window. On any failure the caller keeps the plain look.
pub unsafe fn start(window: HWND, dpi: f32) -> Result<(), String> {
    type CreateDevice = unsafe extern "system" fn(*mut c_void, i32, *mut c_void, u32, *const c_void, u32, u32, *mut *mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;
    type CreateFactory = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
    type CreateComposition = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
    const HARDWARE: i32 = 1;
    const SDK_VERSION: u32 = 7;
    const BGRA_SUPPORT: u32 = 0x20;

    let create_device: CreateDevice = std::mem::transmute(system_function("d3d11.dll", b"D3D11CreateDevice\0")?);
    let create_factory: CreateFactory = std::mem::transmute(system_function("dxgi.dll", b"CreateDXGIFactory2\0")?);
    let create_composition: CreateComposition = std::mem::transmute(system_function("dcomp.dll", b"DCompositionCreateDevice\0")?);
    let text = |e: windows::core::Error| e.to_string();

    let (mut raw_device, mut raw_context) = (std::ptr::null_mut(), std::ptr::null_mut());
    create_device(std::ptr::null_mut(), HARDWARE, std::ptr::null_mut(), BGRA_SUPPORT, std::ptr::null(), 0, SDK_VERSION, &mut raw_device, std::ptr::null_mut(), &mut raw_context).ok().map_err(text)?;
    let device = ID3D11Device::from_raw(raw_device);
    let context = ID3D11DeviceContext::from_raw(raw_context);
    let dxgi: IDXGIDevice = device.cast().map_err(text)?;
    let mut raw_factory = std::ptr::null_mut();
    create_factory(0, &IDXGIFactory2::IID, &mut raw_factory).ok().map_err(text)?;
    let factory = IDXGIFactory2::from_raw(raw_factory);
    let mut raw_dcomp = std::ptr::null_mut();
    create_composition(dxgi.as_raw(), &IDCompositionDevice::IID, &mut raw_dcomp).ok().map_err(text)?;
    let dcomp = IDCompositionDevice::from_raw(raw_dcomp);

    let target = dcomp.CreateTargetForHwnd(ComHwnd(window), true).map_err(text)?;
    let root = dcomp.CreateVisual().map_err(text)?;
    let sprite = |dcomp: &IDCompositionDevice| -> Result<Sprite, String> {
        let visual = dcomp.CreateVisual().map_err(text)?;
        root.AddVisual(&visual, true, None).map_err(text)?;
        visual.SetOffsetX2(FAR_AWAY).map_err(text)?;
        Ok(Sprite { visual, swap: None, page: None })
    };
    // each is added on top, so the order here is the stacking order
    let panel = sprite(&dcomp)?;
    let neck = sprite(&dcomp)?;
    let rail = sprite(&dcomp)?;
    target.SetRoot(&root).map_err(text)?;
    dcomp.Commit().map_err(text)?;

    let (mut token, input) = (0usize, GdiplusStartupInput { GdiplusVersion: 1, ..std::mem::zeroed() });
    if GdiplusStartup(&mut token, &input, std::ptr::null_mut()) != 0 {
        return Err("GDI+ would not start".into());
    }
    *ENGINE.lock().unwrap() = Some(Engine { device, context, factory, dcomp, _target: target, rail, panel, neck, rail_rect: (0, 0, 1, 1), panel_size: (0, 0), dpi, _gdiplus: token });
    Ok(())
}

impl Engine {
    /// Make sure a sprite can show a picture this big, growing with room to spare.
    unsafe fn ensure(&self, s: &mut Sprite, w: i32, h: i32) -> Result<(), String> {
        if s.page.as_ref().is_some_and(|p| p.w >= w && p.h >= h) {
            return Ok(());
        }
        let (w, h) = (w + w / 4 + 8, h + h / 4 + 8);
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w as u32,
            Height: h as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swap = self.factory.CreateSwapChainForComposition(&self.device, &desc, None).map_err(|e| e.to_string())?;
        s.visual.SetContent(&swap).map_err(|e| e.to_string())?;
        s.swap = Some(swap);
        s.page = Some(Page::new(w, h));
        Ok(())
    }

    /// Hand a finished picture to the screen.
    unsafe fn show(&self, s: &Sprite, x: f32, y: f32) {
        let (Some(swap), Some(page)) = (&s.swap, &s.page) else { return };
        if let Ok(back) = swap.GetBuffer::<ID3D11Texture2D>(0) {
            self.context.UpdateSubresource(&back, 0, None, page.pixels.as_ptr() as *const c_void, (page.w * 4) as u32, 0);
            let _ = swap.Present(0, DXGI_PRESENT(0));
        }
        let _ = s.visual.SetOffsetX2(x);
        let _ = s.visual.SetOffsetY2(y);
        let _ = self.dcomp.Commit();
    }

    unsafe fn draw_rail(&mut self) {
        let (x, y, w, h) = self.rail_rect;
        let mut rail = Sprite { visual: self.rail.visual.clone(), swap: self.rail.swap.take(), page: self.rail.page.take() };
        if self.ensure(&mut rail, w, h).is_ok() {
            if let Some(page) = rail.page.as_mut() {
                page.clear();
                let mut c = PlusCanvas { g: page.g, dpi: self.dpi, alpha: 225 };
                layers::paint_rail_on(&mut c, self.dpi, layers::pill_tint());
            }
            self.show(&rail, x as f32, y as f32);
        }
        self.rail = rail;
    }
}

pub unsafe fn show_rail(x: i32, y: i32, w: i32, h: i32) {
    if let Ok(mut guard) = ENGINE.lock() {
        if let Some(e) = guard.as_mut() {
            e.rail_rect = (x, y, w, h);
            e.draw_rail();
        }
    }
}

pub unsafe fn repaint_rail() {
    if let Ok(mut guard) = ENGINE.lock() {
        if let Some(e) = guard.as_mut() {
            e.draw_rail();
        }
    }
}

pub unsafe fn hide_rail() {
    if let Ok(guard) = ENGINE.lock() {
        if let Some(e) = guard.as_ref() {
            let _ = e.rail.visual.SetOffsetX2(FAR_AWAY);
            let _ = e.dcomp.Commit();
        }
    }
}

pub unsafe fn hide_panel() {
    if let Ok(guard) = ENGINE.lock() {
        if let Some(e) = guard.as_ref() {
            let _ = e.panel.visual.SetOffsetX2(FAR_AWAY);
            let _ = e.neck.visual.SetOffsetX2(FAR_AWAY);
            let _ = e.dcomp.Commit();
        }
    }
}

pub unsafe fn set_panel(x: i32, y: i32, w: i32, h: i32, radius: i32) {
    let Ok(mut guard) = ENGINE.lock() else { return };
    let Some(e) = guard.as_mut() else { return };
    let dpi = e.dpi;
    let shadow = (14.0 * dpi) as i32;
    let (w, h) = (w.max(2), h.max(2));
    let mut panel = Sprite { visual: e.panel.visual.clone(), swap: e.panel.swap.take(), page: e.panel.page.take() };
    if e.ensure(&mut panel, w + 2 * shadow, h + 2 * shadow).is_ok() {
        if let Some(page) = panel.page.as_mut() {
            page.clear();
            let g = page.g;
            let (fx, fy, fw, fh, d) = (shadow as f32, shadow as f32, w as f32, h as f32, (radius * 2) as f32);
            // a soft shadow: stacked, nearly clear shapes, widest first. It is costly to draw, so
            // only once the panel has stopped changing size; animation must never wait for it.
            if e.panel_size == (w, h) {
                let mut brush = std::ptr::null_mut();
                GdipCreateSolidFill(0x0A000000, &mut brush);
                for grow in (1..=shadow).rev().step_by(2) {
                    let out = grow as f32;
                    let path = round_path(fx - out, fy - out + out / 3.0, fw + 2.0 * out, fh + 2.0 * out, d + 2.0 * out);
                    GdipFillPath(g, brush as _, path);
                    GdipDeletePath(path);
                }
                GdipDeleteBrush(brush as _);
            }

            let settings = layers::settings_showing();
            let (shade, accent, pill) = (layers::panel_tint(), layers::accent(), layers::pill_tint());
            let alpha = if settings { 244 } else { 190 };
            let (top, bottom) = (blend(blend(shade, accent, 0.10), 0x00FFFFFF, 0.05), blend(shade, accent, 0.04));
            let body = round_path(fx, fy, fw, fh, d);
            let area = RectF { X: fx, Y: fy, Width: fw, Height: fh + 1.0 };
            let mut glass = std::ptr::null_mut();
            GdipCreateLineBrushFromRect(&area, argb(top, alpha), argb(bottom, alpha), LinearGradientModeVertical, WrapModeTile, &mut glass);
            GdipFillPath(g, glass as _, body);
            GdipDeleteBrush(glass as _);

            let mut pen = std::ptr::null_mut();
            GdipCreatePen1(argb(blend(shade, accent, 0.8), 220), dpi, UNIT_PIXEL, &mut pen);
            GdipDrawPath(g, pen, body);
            GdipDeletePen(pen);

            if settings {
                GdipSetClipPath(g, body, CombineModeReplace);
                GdipTranslateWorldTransform(g, fx, fy, MatrixOrderPrepend);
                let mut c = PlusCanvas { g, dpi, alpha: 255 };
                layers::paint_settings_on(&mut c, dpi, pill, accent);
            }
            GdipDeletePath(body);
        }
        e.show(&panel, (x - shadow) as f32, (y - shadow) as f32);
    }
    e.panel = panel;
    e.panel_size = (w, h);
}

pub unsafe fn set_neck(from: (i32, i32), to: (i32, i32), reach: f32, end_width: f32, mid_width: f32) {
    let Ok(mut guard) = ENGINE.lock() else { return };
    let Some(e) = guard.as_mut() else { return };
    let margin = end_width as i32 + 2;
    let (left, top) = (from.0.min(to.0) - margin, from.1.min(to.1) - margin);
    let (w, h) = ((to.0 - from.0).abs() + 2 * margin, (to.1 - from.1).abs() + 2 * margin);
    let outline = neck_outline((from.0 as f32, from.1 as f32), (to.0 as f32, to.1 as f32), reach, end_width, mid_width);
    let colour = blend(layers::pill_tint(), layers::accent(), 0.6);
    let mut neck = Sprite { visual: e.neck.visual.clone(), swap: e.neck.swap.take(), page: e.neck.page.take() };
    if e.ensure(&mut neck, w, h).is_ok() {
        if let Some(page) = neck.page.as_mut() {
            page.clear();
            let points: Vec<PointF> = outline.iter().map(|&(x, y)| PointF { X: x - left as f32, Y: y - top as f32 }).collect();
            let mut brush = std::ptr::null_mut();
            GdipCreateSolidFill(argb(colour, 235), &mut brush);
            GdipFillPolygon(page.g, brush as _, points.as_ptr(), points.len() as i32, FillModeWinding);
            GdipDeleteBrush(brush as _);
        }
        e.show(&neck, left as f32, top as f32);
    }
    e.neck = neck;
}
