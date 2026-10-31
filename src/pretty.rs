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
/// Frosted glass keeps the wallpaper at one eighth size: heavy blur hides the loss. Liquid glass is
/// nearly clear, so it keeps far more of the picture and blurs it only slightly.
const FROSTED_SHRINK: i32 = 8;
const LIQUID_SHRINK: i32 = 2;
const BLUR_PASSES: usize = 3;
const FROSTED_BLUR: i32 = 3;
const LIQUID_BLUR: i32 = 1;
const FROSTED_BOOST: f32 = 1.45;
const LIQUID_BOOST: f32 = 1.15;

/// A tiny, blurred copy of the wallpaper, lined up with the screen. Glass shows pieces of it.
struct Backdrop {
    liquid: bool,
    shrink: i32,
    /// The tiny copy, kept alive for GDI+, which draws the neck from it.
    _pixels: Vec<u32>,
    /// The same picture enlarged once to screen size, so each frame only has to copy from it.
    full: Vec<u32>,
    full_w: i32,
    full_h: i32,
    bitmap: *mut GpBitmap,
    attributes: *mut GpImageAttributes,
}

/// Only ever touched behind the mutex below, one thread at a time.
unsafe impl Send for Backdrop {}

impl Drop for Backdrop {
    fn drop(&mut self) {
        unsafe {
            GdipDisposeImageAttributes(self.attributes);
            GdipDisposeImage(self.bitmap as _);
        }
    }
}

/// Stays when Explorer restarts and the windows are remade, as the wallpaper has not changed.
static BACKDROP: Mutex<Option<Backdrop>> = Mutex::new(None);

/// A straight blur along rows, wrapping nothing: pixels past the edge repeat the edge.
fn blur_rows(from: &[u32], to: &mut [u32], w: i32, h: i32, radius: i32, across: bool) {
    let (lines, length) = if across { (h, w) } else { (w, h) };
    let span = (2 * radius + 1) as u32;
    let at = |line: i32, n: i32| -> usize {
        let n = n.clamp(0, length - 1);
        (if across { line * w + n } else { n * w + line }) as usize
    };
    for line in 0..lines {
        let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
        for n in -radius..=radius {
            let p = from[at(line, n)];
            r += (p >> 16) & 255;
            g += (p >> 8) & 255;
            b += p & 255;
        }
        for n in 0..length {
            to[at(line, n)] = 0xFF00_0000 | ((r / span) << 16) | ((g / span) << 8) | (b / span);
            let (add, drop) = (from[at(line, n + radius + 1)], from[at(line, n - radius)]);
            r = r + ((add >> 16) & 255) - ((drop >> 16) & 255);
            g = g + ((add >> 8) & 255) - ((drop >> 8) & 255);
            b = b + (add & 255) - (drop & 255);
        }
    }
}

/// Three passes of a box blur look like a true soft blur, at a fraction of the cost.
pub fn blur(pixels: &mut Vec<u32>, w: i32, h: i32, radius: i32) {
    let mut other = pixels.clone();
    for _ in 0..BLUR_PASSES {
        blur_rows(pixels, &mut other, w, h, radius, true);
        blur_rows(&other, pixels, w, h, radius, false);
    }
}

/// Frosted glass looks richer than what is behind it, so colours are pushed away from grey.
pub fn boost_colour(p: u32, amount: f32) -> u32 {
    let (r, g, b) = (((p >> 16) & 255) as f32, ((p >> 8) & 255) as f32, (p & 255) as f32);
    let grey = 0.3 * r + 0.59 * g + 0.11 * b;
    let push = |c: f32| (grey + (c - grey) * amount).clamp(0.0, 255.0) as u32;
    (p & 0xFF00_0000) | (push(r) << 16) | (push(g) << 8) | push(b)
}

/// How far the edge of a rounded box is from a point inside it, and which way is outward.
/// Used to bend the picture through the rim. Returns (distance inside, outward x, outward y).
pub fn edge_info(px: f32, py: f32, w: f32, h: f32, radius: f32) -> (f32, f32, f32) {
    let r = radius.min(w / 2.0).min(h / 2.0);
    let (qx, qy) = ((px - w / 2.0).abs() - (w / 2.0 - r), (py - h / 2.0).abs() - (h / 2.0 - r));
    let (sx, sy) = (if px < w / 2.0 { -1.0 } else { 1.0 }, if py < h / 2.0 { -1.0 } else { 1.0 });
    if qx > 0.0 && qy > 0.0 {
        let len = (qx * qx + qy * qy).sqrt().max(0.001);
        (r - len, sx * qx / len, sy * qy / len)
    } else if qx > qy {
        (r - qx, sx, 0.0)
    } else {
        (r - qy, 0.0, sy)
    }
}

/// Make the tiny blurred wallpaper. Called on start and whenever the wallpaper changes.
pub unsafe fn load_backdrop(screen_w: i32, screen_h: i32, liquid: bool) -> Result<(), String> {
    let (shrink, radius, boost) = if liquid { (LIQUID_SHRINK, LIQUID_BLUR, LIQUID_BOOST) } else { (FROSTED_SHRINK, FROSTED_BLUR, FROSTED_BOOST) };
    let path = crate::wallpaper::picture_path().ok_or("no wallpaper picture found")?;
    let mut image = std::ptr::null_mut();
    if GdipLoadImageFromFile(wide(&path.to_string_lossy()).as_ptr(), &mut image) != 0 {
        return Err("the wallpaper would not load".into());
    }
    let (w, h) = ((screen_w / shrink).max(2), (screen_h / shrink).max(2));
    let mut pixels = vec![0u32; (w * h) as usize];
    let (mut bitmap, mut g) = (std::ptr::null_mut(), std::ptr::null_mut());
    GdipCreateBitmapFromScan0(w, h, w * 4, PARGB, pixels.as_mut_ptr() as *mut u8, &mut bitmap);
    GdipGetImageGraphicsContext(bitmap as _, &mut g);
    GdipSetInterpolationMode(g, InterpolationModeHighQualityBicubic);
    // the same crop as Windows' Fill setting: cover the screen, trim the overflow evenly
    let (mut iw, mut ih) = (0u32, 0u32);
    GdipGetImageWidth(image, &mut iw);
    GdipGetImageHeight(image, &mut ih);
    let scale = (screen_w as f32 / iw as f32).max(screen_h as f32 / ih as f32);
    let (sw, sh) = (screen_w as f32 / scale, screen_h as f32 / scale);
    GdipDrawImageRectRect(g, image, 0.0, 0.0, w as f32, h as f32, (iw as f32 - sw) / 2.0, (ih as f32 - sh) / 2.0, sw, sh, UNIT_PIXEL, std::ptr::null_mut(), 0, std::ptr::null_mut());
    GdipDeleteGraphics(g);
    GdipDisposeImage(image);
    for p in pixels.iter_mut() {
        *p = boost_colour(*p | 0xFF00_0000, boost);
    }
    blur(&mut pixels, w, h, radius);
    let mut attributes = std::ptr::null_mut();
    GdipCreateImageAttributes(&mut attributes);
    GdipSetImageAttributesWrapMode(attributes, WrapModeTileFlipXY, 0, 0); // no dark fringe at the screen's edge
    let full = enlarge(&pixels, w, h, screen_w, screen_h);
    *BACKDROP.lock().unwrap() = Some(Backdrop { liquid, shrink, _pixels: pixels, full, full_w: screen_w, full_h: screen_h, bitmap, attributes });
    Ok(())
}

pub fn drop_backdrop() {
    if let Ok(mut b) = BACKDROP.lock() {
        *b = None;
    }
}

fn has_glass() -> bool {
    BACKDROP.lock().map(|b| b.is_some()).unwrap_or(false)
}

fn has_liquid() -> bool {
    BACKDROP.lock().map(|b| b.as_ref().is_some_and(|b| b.liquid)).unwrap_or(false)
}

/// Show the blurred wallpaper through this shape. x and y place the shape on the page; sx and sy
/// say where that page spot is on the screen.
unsafe fn paint_backdrop(g: *mut GpGraphics, shape: *mut GpPath, x: f32, y: f32, w: f32, h: f32, sx: f32, sy: f32) {
    let Ok(guard) = BACKDROP.lock() else { return };
    let Some(b) = guard.as_ref() else { return };
    GdipSetClipPath(g, shape, CombineModeReplace);
    GdipSetInterpolationMode(g, InterpolationModeBilinear);
    GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
    let s = b.shrink as f32;
    GdipDrawImageRectRect(g, b.bitmap as _, x, y, w, h, sx / s, sy / s, w / s, h / s, UNIT_PIXEL, b.attributes, 0, std::ptr::null_mut());
    GdipResetClip(g);
}

/// Smooth enlargement of the blurred picture to its final size.
pub fn enlarge(small: &[u32], w: i32, h: i32, to_w: i32, to_h: i32) -> Vec<u32> {
    let mut out = Vec::with_capacity((to_w * to_h) as usize);
    let (kx, ky) = (w as f32 / to_w as f32, h as f32 / to_h as f32);
    for y in 0..to_h {
        let fy = ((y as f32 + 0.5) * ky - 0.5).clamp(0.0, (h - 1) as f32);
        let (y0, ty) = (fy as i32, ((fy - fy.floor()) * 256.0) as u32);
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..to_w {
            let fx = ((x as f32 + 0.5) * kx - 0.5).clamp(0.0, (w - 1) as f32);
            let (x0, tx) = (fx as i32, ((fx - fx.floor()) * 256.0) as u32);
            let x1 = (x0 + 1).min(w - 1);
            let (a, b, c, d) = (small[(y0 * w + x0) as usize], small[(y0 * w + x1) as usize], small[(y1 * w + x0) as usize], small[(y1 * w + x1) as usize]);
            let channel = |shift: u32| {
                let (pa, pb, pc, pd) = ((a >> shift) & 255, (b >> shift) & 255, (c >> shift) & 255, (d >> shift) & 255);
                let top = pa * (256 - tx) + pb * tx;
                let low = pc * (256 - tx) + pd * tx;
                ((top * (256 - ty) + low * ty) >> 16) << shift
            };
            out.push(0xFF00_0000 | channel(16) | channel(8) | channel(0));
        }
    }
    out
}

/// One colour as red, green and blue parts, from Windows' blue-green-red order.
fn parts(bgr: u32) -> (u32, u32, u32) {
    (bgr & 255, (bgr >> 8) & 255, (bgr >> 16) & 255)
}

/// Copy a rounded piece of the blurred wallpaper onto the page, tinted from `top` to `bottom`.
/// This is the part that runs every animation frame, so it copies rather than scales, and only
/// works out smooth edges in the corners.
unsafe fn fill_frosted(page: &mut Page, x: i32, y: i32, w: i32, h: i32, radius: f32, sx: i32, sy: i32, top: u32, bottom: u32, alpha: u32) {
    let Ok(guard) = BACKDROP.lock() else { return };
    let Some(b) = guard.as_ref() else { return };
    let corner = radius.ceil() as i32 + 1;
    let keep = 255 - alpha;
    let (tr, tg, tb) = parts(top);
    let (br, bg, bb) = parts(bottom);
    for row in 0..h {
        let page_y = row + y;
        if page_y < 0 || page_y >= page.h {
            continue;
        }
        let along = (row * 256 / h.max(1)) as u32;
        let tint = |t: u32, q: u32| ((t * (256 - along) + q * along) >> 8) * alpha;
        let (ar, ag, ab) = (tint(tr, br), tint(tg, bg), tint(tb, bb));
        let source_row = (sy + row).clamp(0, b.full_h - 1) * b.full_w;
        let in_corner_row = row < corner || row >= h - corner;
        for col in 0..w {
            let page_x = col + x;
            if page_x < 0 || page_x >= page.w {
                continue;
            }
            let s = b.full[(source_row + (sx + col).clamp(0, b.full_w - 1)) as usize];
            let r = (((s >> 16) & 255) * keep + ar) / 255;
            let g = (((s >> 8) & 255) * keep + ag) / 255;
            let bl = ((s & 255) * keep + ab) / 255;
            let mut pixel = 0xFF00_0000 | (r << 16) | (g << 8) | bl;
            if in_corner_row && (col < corner || col >= w - corner) {
                let (inside, _, _) = edge_info(col as f32 + 0.5, row as f32 + 0.5, w as f32, h as f32, radius);
                let cover = ((inside + 0.5).clamp(0.0, 1.0) * 255.0) as u32;
                if cover == 0 {
                    continue;
                }
                pixel = (cover << 24) | ((r * cover / 255) << 16) | ((g * cover / 255) << 8) | (bl * cover / 255);
            }
            page.pixels[(page_y * page.w + page_x) as usize] = pixel;
        }
    }
}

/// Where the light comes from, as a direction: up and to the left.
const LIGHT: (f32, f32) = (-0.55, -0.83);

/// The liquid edge: near the rim, the picture is pulled inward as a thick lens would, and brightens
/// where the curve catches the light. It fades into what is already there, so no seam shows, and
/// it is worked out pixel by pixel, so only once a panel is at rest.
unsafe fn bend_rim(page: &mut Page, x: i32, y: i32, w: i32, h: i32, radius: f32, sx: i32, sy: i32, band: f32) {
    let Ok(guard) = BACKDROP.lock() else { return };
    let Some(b) = guard.as_ref() else { return };
    let reach = band.ceil() as i32 + 1;
    for py in 0..h {
        let middle_row = py >= reach && py < h - reach;
        let mut px = 0;
        while px < w {
            if middle_row && px == reach {
                px = (w - reach).max(reach); // the flat middle of the panel has no rim to bend
            }
            let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
            let this_x = px;
            px += 1;
            let (inside, nx, ny) = edge_info(cx, cy, w as f32, h as f32, radius);
            if inside >= band || inside <= 0.0 {
                continue;
            }
            let index = ((py + y) * page.w + (this_x + x)) as usize;
            let here = page.pixels[index];
            let coverage = here >> 24;
            if coverage == 0 {
                continue;
            }
            let closeness = 1.0 - inside / band;
            let bend = band * 2.2 * closeness * closeness;
            let sample_x = (sx as f32 + cx - nx * bend) as i32;
            let sample_y = (sy as f32 + cy - ny * bend) as i32;
            let s = b.full[(sample_y.clamp(0, b.full_h - 1) * b.full_w + sample_x.clamp(0, b.full_w - 1)) as usize];
            let fade = closeness * closeness * (3.0 - 2.0 * closeness); // nothing at the inner edge, full at the rim
            let cover = coverage as f32 / 255.0;
            if b.liquid {
                // a real lens splits colours a little, red bending least and blue most
                let read = |amount: f32, shift: u32| {
                    let qx = (sx as f32 + cx - nx * bend * amount) as i32;
                    let qy = (sy as f32 + cy - ny * bend * amount) as i32;
                    ((b.full[(qy.clamp(0, b.full_h - 1) * b.full_w + qx.clamp(0, b.full_w - 1)) as usize] >> shift) & 255) as f32
                };
                // light from the top left, with a fainter bounce on the far side
                let facing = nx * LIGHT.0 + ny * LIGHT.1;
                let glint = (facing.max(0.0).powi(3) + (-facing).max(0.0).powi(3) * 0.45) * closeness.powi(5) * 235.0
                    + facing.max(0.0) * closeness * closeness * 26.0;
                let finish = |amount: f32, shift: u32| {
                    let bent = (read(amount, shift) + glint) * cover;
                    let now = ((here >> shift) & 255) as f32;
                    ((now + (bent - now) * fade).clamp(0.0, 255.0) as u32) << shift
                };
                page.pixels[index] = (coverage << 24) | finish(0.94, 16) | finish(1.0, 8) | finish(1.07, 0);
                continue;
            }
            let lift = 1.0 + 0.28 * closeness * closeness;
            let finish = |shift: u32| {
                let bent = ((s >> shift) & 255) as f32 * lift * cover;
                let now = ((here >> shift) & 255) as f32;
                ((now + (bent - now) * fade * 0.8).clamp(0.0, 255.0) as u32) << shift
            };
            page.pixels[index] = (coverage << 24) | finish(16) | finish(8) | finish(0);
        }
    }
}

/// The neck as a glass tube: each column is bent toward its own centre line, strongest at the walls.
/// `outline` is the neck's shape on this page; its first half runs along one wall and its second
/// half back along the other.
unsafe fn bend_neck(page: &mut Page, outline: &[PointF], sx: i32, sy: i32, tint: u32) {
    let Ok(guard) = BACKDROP.lock() else { return };
    let Some(b) = guard.as_ref() else { return };
    let pairs = outline.len() / 2;
    if pairs < 2 {
        return;
    }
    let (tr, tg, tb) = parts(tint);
    let mut centre = vec![(0.0f32, 0.0f32); page.w as usize];
    let mut known = vec![false; page.w as usize];
    for i in 0..pairs {
        let (a, c) = (outline[i], outline[outline.len() - 1 - i]);
        let column = (((a.X + c.X) / 2.0) as i32).clamp(0, page.w - 1) as usize;
        centre[column] = ((a.Y + c.Y) / 2.0, (((a.X - c.X).powi(2) + (a.Y - c.Y).powi(2)).sqrt() / 2.0).max(1.0));
        known[column] = true;
    }
    let mut last = None;
    for column in 0..page.w as usize {
        if known[column] {
            last = Some(centre[column]);
        } else if let Some(v) = last {
            centre[column] = v;
        }
    }
    for y in 0..page.h {
        for x in 0..page.w {
            let index = (y * page.w + x) as usize;
            let coverage = page.pixels[index] >> 24;
            if coverage == 0 {
                continue;
            }
            let (mid, half) = centre[x as usize];
            let across = ((y as f32 + 0.5 - mid) / half).clamp(-1.0, 1.0);
            let bend = across * across * across * half * 2.4;
            let s = b.full[((sy + y - bend as i32).clamp(0, b.full_h - 1) * b.full_w + (sx + x).clamp(0, b.full_w - 1)) as usize];
            let glint = if across < 0.0 { (-across).powi(6) * 150.0 } else { across.powi(6) * 55.0 };
            let cover = coverage as f32 / 255.0;
            let mixed = |shift: u32, t: u32| ((((((s >> shift) & 255) as f32) * 0.76 + t as f32 * 0.24 + glint).min(255.0) * cover) as u32) << shift;
            page.pixels[index] = (coverage << 24) | mixed(16, tr) | mixed(8, tg) | mixed(0, tb);
        }
    }
}

/// The bright line glass catches: strongest at the top left, fading across the shape.
unsafe fn rim_light(g: *mut GpGraphics, shape: *mut GpPath, x: f32, y: f32, w: f32, h: f32, width: f32, strength: u8) {
    let area = RectF { X: x, Y: y, Width: w.max(1.0), Height: h.max(1.0) };
    let mut light = std::ptr::null_mut();
    GdipCreateLineBrushFromRect(&area, ((strength as u32) << 24) | 0xFFFFFF, 0x18FFFFFF, LinearGradientModeForwardDiagonal, WrapModeTile, &mut light);
    let mut pen = std::ptr::null_mut();
    GdipCreatePen2(light as _, width, UNIT_PIXEL, &mut pen);
    GdipDrawPath(g, pen, shape);
    GdipDeletePen(pen);
    GdipDeleteBrush(light as _);
}

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
    /// Where this window's corner is on the screen, to line glass up with the wallpaper.
    origin: (f32, f32),
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

unsafe fn window_origin(window: HWND) -> (f32, f32) {
    let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    windows_sys::Win32::Graphics::Gdi::ClientToScreen(window, &mut p);
    (p.x as f32, p.y as f32)
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
    *ENGINE.lock().unwrap() = Some(Engine { device, context, factory, dcomp, _target: target, rail, panel, neck, rail_rect: (0, 0, 1, 1), panel_size: (0, 0), origin: window_origin(window), dpi, _gdiplus: token });
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
                let glass = has_glass();
                if glass {
                    let d = 14.0 * self.dpi;
                    if let Ok(pills) = layers::PILLS.lock() {
                        for p in pills.iter() {
                            let r = p.rect;
                            let (px, py, pw, ph) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
                            let (sx, sy) = (self.origin.0 + x as f32 + px, self.origin.1 + y as f32 + py);
                            fill_frosted(page, px as i32, py as i32, pw as i32, ph as i32, d / 2.0, sx as i32, sy as i32, 0, 0, 0);
                        }
                    }
                }
                let liquid = has_liquid();
                if liquid {
                    let band = 9.0 * self.dpi;
                    if let Ok(pills) = layers::PILLS.lock() {
                        for p in pills.iter() {
                            let r = p.rect;
                            let (sx, sy) = (self.origin.0 + (x + r.left) as f32, self.origin.1 + (y + r.top) as f32);
                            bend_rim(page, r.left, r.top, r.right - r.left, r.bottom - r.top, 7.0 * self.dpi, sx as i32, sy as i32, band);
                        }
                    }
                }
                let mut c = PlusCanvas { g: page.g, dpi: self.dpi, alpha: if liquid { 112 } else if glass { 150 } else { 225 } };
                layers::paint_rail_on(&mut c, self.dpi, layers::pill_tint());
                if glass {
                    if let Ok(pills) = layers::PILLS.lock() {
                        for p in pills.iter() {
                            let r = p.rect;
                            let (px, py, pw, ph) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
                            let shape = round_path(px + 0.5, py + 0.5, pw - 1.0, ph - 1.0, 14.0 * self.dpi);
                            rim_light(page.g, shape, px, py, pw, ph, 1.0, 130);
                            GdipDeletePath(shape);
                        }
                    }
                }
            }
            self.show(&rail, x as f32, y as f32);
        }
        self.rail = rail;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_keeps_flat_colour_flat_and_softens_a_spike() {
        let mut flat = vec![0xFF40_6080u32; 100];
        blur(&mut flat, 10, 10, 3);
        assert!(flat.iter().all(|p| *p == 0xFF40_6080));
        let mut spike = vec![0xFF00_0000u32; 49 * 49];
        spike[24 * 49 + 24] = 0xFFFF_FFFF;
        blur(&mut spike, 49, 49, 3);
        let centre = spike[24 * 49 + 24] & 255;
        assert!(centre < 255 && spike[24 * 49 + 25] & 255 <= centre);
    }

    #[test]
    fn colour_boost_leaves_grey_alone_and_pushes_colour() {
        let grey = boost_colour(0xFF80_8080, 1.45);
        for shift in [0, 8, 16] {
            assert!((((grey >> shift) & 255) as i32 - 0x80).abs() <= 1, "{grey:x}");
        }
        let pushed = boost_colour(0xFF80_4040, 1.45);
        assert!((pushed >> 16) & 255 > 0x80 && pushed & 255 < 0x40);
    }

    #[test]
    fn enlarging_keeps_a_flat_picture_flat_and_the_right_size() {
        let out = enlarge(&[0xFF10_2030; 16], 4, 4, 40, 20);
        assert_eq!(out.len(), 800);
        assert!(out.iter().all(|p| (*p & 255).abs_diff(0x30) <= 1 && ((*p >> 16) & 255).abs_diff(0x10) <= 1), "{:x}", out[0]);
    }

    #[test]
    fn edge_info_finds_the_nearest_edge_and_the_outward_direction() {
        let (d, nx, ny) = edge_info(50.0, 1.0, 100.0, 100.0, 10.0); // near the top
        assert!((d - 1.0).abs() < 0.01 && nx == 0.0 && ny == -1.0);
        let (d, nx, ny) = edge_info(99.0, 50.0, 100.0, 100.0, 10.0); // near the right
        assert!((d - 1.0).abs() < 0.01 && nx == 1.0 && ny == 0.0);
        let (d, _, _) = edge_info(50.0, 50.0, 100.0, 100.0, 10.0); // the middle is far from any edge
        assert!(d > 40.0);
        let (d, nx, ny) = edge_info(0.5, 0.5, 100.0, 100.0, 10.0); // outside the rounded corner
        assert!(d < 0.0 && nx < 0.0 && ny < 0.0);
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
            let frosted = has_glass();
            let liquid = has_liquid();
            let (shade, accent, pill) = (layers::panel_tint(), layers::accent(), layers::pill_tint());
            // real glass lets more through; text on Settings still has to stay easy to read
            let alpha = match (frosted, settings) {
                (true, true) if liquid => 178,
                (true, false) if liquid => 42,
                (true, true) => 205,
                (true, false) => 120,
                (false, true) => 244,
                (false, false) => 190,
            };
            let (top, bottom) = (blend(blend(shade, accent, 0.10), 0x00FFFFFF, 0.05), blend(shade, accent, 0.04));
            let body = round_path(fx, fy, fw, fh, d);
            if frosted {
                let (sx, sy) = ((e.origin.0 + x as f32) as i32, (e.origin.1 + y as f32) as i32);
                fill_frosted(page, shadow, shadow, w, h, d / 2.0, sx, sy, top, bottom, alpha);
                // liquid glass keeps bending while it moves; frosted glass waits until it rests
                if liquid {
                    bend_rim(page, shadow, shadow, w, h, d / 2.0, sx, sy, (30.0 * dpi).min(h as f32 / 2.0).min(w as f32 / 2.0));
                    rim_light(g, body, fx, fy, fw, fh, dpi, 90);
                } else {
                    if e.panel_size == (w, h) {
                        bend_rim(page, shadow, shadow, w, h, d / 2.0, sx, sy, 20.0 * dpi);
                    }
                    rim_light(g, body, fx, fy, fw, fh, 1.3 * dpi, 215);
                }
            } else {
                let area = RectF { X: fx, Y: fy, Width: fw, Height: fh + 1.0 };
                let mut glass = std::ptr::null_mut();
                GdipCreateLineBrushFromRect(&area, argb(top, alpha as u8), argb(bottom, alpha as u8), LinearGradientModeVertical, WrapModeTile, &mut glass);
                GdipFillPath(g, glass as _, body);
                GdipDeleteBrush(glass as _);

                let mut pen = std::ptr::null_mut();
                GdipCreatePen1(argb(blend(shade, accent, 0.8), 220), dpi, UNIT_PIXEL, &mut pen);
                GdipDrawPath(g, pen, body);
                GdipDeletePen(pen);
            }

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
            let frosted = has_glass();
            if has_liquid() {
                let mut brush = std::ptr::null_mut();
                GdipCreateSolidFill(0xFFFFFFFF, &mut brush);
                GdipFillPolygon(page.g, brush as _, points.as_ptr(), points.len() as i32, FillModeWinding);
                GdipDeleteBrush(brush as _);
                bend_neck(page, &points, (e.origin.0 + left as f32) as i32, (e.origin.1 + top as f32) as i32, colour);
            } else if frosted {
                let mut shape = std::ptr::null_mut();
                GdipCreatePath(FillModeWinding, &mut shape);
                GdipAddPathPolygon(shape, points.as_ptr(), points.len() as i32);
                paint_backdrop(page.g, shape, 0.0, 0.0, w as f32, h as f32, e.origin.0 + left as f32, e.origin.1 + top as f32);
                GdipDeletePath(shape);
            }
            if !has_liquid() {
                let mut brush = std::ptr::null_mut();
                GdipCreateSolidFill(argb(colour, if frosted { 150 } else { 235 }), &mut brush);
                GdipFillPolygon(page.g, brush as _, points.as_ptr(), points.len() as i32, FillModeWinding);
                GdipDeleteBrush(brush as _);
            }
        }
        e.show(&neck, left as f32, top as f32);
    }
    e.neck = neck;
}
