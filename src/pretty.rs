//! Pretty mode: smooth corners and soft shadows, for people happy to spend 60-90 MB on them.
//! Everything is drawn with GDI+ into memory and shown through DirectComposition, the one route
//! that allows per-pixel transparency behind the icons. The graphics libraries are loaded by hand
//! so that with this mode off they are never in memory at all.

use crate::layers::{self, Canvas, Style};
use crate::rules::{blend, neck_outline};
use crate::wallpaper::wide;
use std::ffi::c_void;
use std::result::Result::Ok; // GdiPlus also defines a constant called Ok
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{FreeLibrary, HWND as ComHwnd, RECT as ComRect};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const PARGB: i32 = 0xE200B;
const UNIT_PIXEL: Unit = 2;
const FAR_AWAY: f32 = -20000.0;
/// Frosted glass keeps the wallpaper at one eighth size: heavy blur hides the loss. Liquid glass
/// hands the graphics card the full picture and blurs it there.
const FROSTED_SHRINK: i32 = 8;
const BLUR_PASSES: usize = 3;
const FROSTED_BLUR: i32 = 3;
const FROSTED_BOOST: f32 = 1.45;

/// A tiny, blurred copy of the wallpaper, lined up with the screen. Glass shows pieces of it.
struct Backdrop {
    liquid: bool,
    /// What the shader draws: 0 liquid, 1 aurora, 2 neon.
    look: f32,
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

/// Where the wallpaper's picture goes on a screen, under each of Windows' placement settings, as
/// (screen rectangle, picture rectangle) pairs. Tile needs many pieces.
pub fn wallpaper_pieces(setting: &str, iw: f32, ih: f32, sw: f32, sh: f32) -> Vec<((f32, f32, f32, f32), (f32, f32, f32, f32))> {
    let whole = (0.0, 0.0, iw, ih);
    match setting {
        "stretch" => vec![((0.0, 0.0, sw, sh), whole)],
        "fit" => {
            let k = (sw / iw).min(sh / ih);
            vec![(((sw - iw * k) / 2.0, (sh - ih * k) / 2.0, iw * k, ih * k), whole)]
        }
        "center" => vec![(((sw - iw) / 2.0, (sh - ih) / 2.0, iw, ih), whole)],
        "tile" => {
            let mut out = vec![];
            let mut y = 0.0;
            while y < sh {
                let mut x = 0.0;
                while x < sw {
                    out.push(((x, y, iw, ih), whole));
                    x += iw.max(1.0);
                }
                y += ih.max(1.0);
            }
            out
        }
        _ => {
            // Fill: cover the screen, trim the overflow evenly
            let k = (sw / iw).max(sh / ih);
            let (cw, ch) = (sw / k, sh / k);
            vec![((0.0, 0.0, sw, sh), ((iw - cw) / 2.0, (ih - ch) / 2.0, cw, ch))]
        }
    }
}

/// Make the tiny blurred wallpaper. Called on start and whenever the wallpaper changes.
pub unsafe fn load_backdrop(screen_w: i32, screen_h: i32, shader_look: Option<f32>) -> Result<(), String> {
    let shader_look = shader_look.filter(|_| gpu_ready());
    let liquid = shader_look.is_some();
    let shrink = if liquid { 1 } else { FROSTED_SHRINK };
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
    let (mut iw, mut ih) = (0u32, 0u32);
    GdipGetImageWidth(image, &mut iw);
    GdipGetImageHeight(image, &mut ih);
    // the empty part of a Fit or Center wallpaper shows Windows' background colour
    pixels.fill(0xFF00_0000 | crate::wallpaper::background_colour());
    let setting = crate::wallpaper::placement_setting();
    let k = 1.0 / shrink as f32;
    for (to, from) in wallpaper_pieces(&setting, iw as f32, ih as f32, screen_w as f32, screen_h as f32) {
        GdipDrawImageRectRect(g, image, to.0 * k, to.1 * k, to.2 * k, to.3 * k, from.0, from.1, from.2, from.3, UNIT_PIXEL, std::ptr::null_mut(), 0, std::ptr::null_mut());
    }
    GdipDeleteGraphics(g);
    GdipDisposeImage(image);
    if !liquid {
        for p in pixels.iter_mut() {
            *p = boost_colour(*p | 0xFF00_0000, FROSTED_BOOST);
        }
        blur(&mut pixels, w, h, FROSTED_BLUR);
    }
    let mut attributes = std::ptr::null_mut();
    GdipCreateImageAttributes(&mut attributes);
    GdipSetImageAttributesWrapMode(attributes, WrapModeTileFlipXY, 0, 0); // no dark fringe at the screen's edge
    // liquid only reads the picture itself, so it needs no enlarged copy
    let full = if liquid { Vec::new() } else { enlarge(&pixels, w, h, screen_w, screen_h) };
    *BACKDROP.lock().unwrap() = Some(Backdrop { liquid, look: shader_look.unwrap_or(0.0), shrink, _pixels: pixels, full, full_w: screen_w, full_h: screen_h, bitmap, attributes });
    if let Ok(mut engine) = ENGINE.lock() {
        if let Some(g) = engine.as_mut().and_then(|e| e.gpu.as_mut()) {
            g.wall = None; // made again from the new picture when next drawn
        }
    }
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

/// The glass edge: near the rim, the picture is pulled inward as a thick lens would, and brightens
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
        GdipSetTextRenderingHint(g, TextRenderingHintAntiAliasGridFit); // letters lined up with the pixels
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
    /// Liquid: the card draws the labels' glass, and words get a soft shadow to stand out on it.
    liquid: bool,
    /// Where text goes, so it can have ClearType.
    text: TextTarget,
}

/// A page, and how to reach it from the coordinates the rail and panels are laid out in.
#[derive(Clone, Copy)]
struct TextTarget {
    page: *mut u32,
    /// The card's copies, as big as the page. Null: blend straight into the page.
    colour: *mut u32,
    cover: *mut u32,
    w: i32,
    h: i32,
    origin: (i32, i32),
    /// left, top, right, bottom on the page: the panel, however far it has opened.
    clip: (i32, i32, i32, i32),
    drew: *mut bool,
}

/// One colour channel of text over what is under it: `amount` of the text, the rest of the page.
fn mix_channel(under: u32, colour: u32, amount: u32) -> u32 {
    (colour * amount + under * (255 - amount)) / 255
}

/// This colour (as 0xAARRGGBB) at these red, green and blue amounts over a page pixel, with its
/// colours already mixed with its transparency.
fn lcd_over(under: u32, cover: u32, colour: u32) -> u32 {
    let byte = |v: u32, shift: u32| (v >> shift) & 255;
    let part = |shift: u32| mix_channel(byte(under, shift), byte(colour, shift), byte(cover, shift)) << shift;
    let most = byte(cover, 0).max(byte(cover, 8)).max(byte(cover, 16));
    part(0) | part(8) | part(16) | ((byte(under, 24) + (255 - byte(under, 24)) * most / 255) << 24)
}

/// The text's colour times its amounts, per channel, ready for the card.
fn colour_for_card(cover: u32, colour: u32) -> u32 {
    let byte = |v: u32, shift: u32| (v >> shift) & 255;
    let part = |shift: u32| (byte(colour, shift) * byte(cover, shift) / 255) << shift;
    part(0) | part(8) | part(16)
}

/// Black at this strength over a page pixel.
fn shade(under: u32, strength: u32) -> u32 {
    let part = |shift: u32| (under >> shift) & 255;
    let colour = |shift: u32| (part(shift) * (255 - strength) / 255) << shift;
    colour(0) | colour(8) | colour(16) | ((part(24) + (255 - part(24)) * strength / 255) << 24)
}

/// The most of a name that fits. `fits` says if a text does; a name cut short ends in an ellipsis.
fn longest_fit(name: &str, fits: &mut dyn FnMut(&str) -> bool) -> String {
    if fits(name) {
        return name.to_string();
    }
    let letters: Vec<char> = name.chars().collect();
    for keep in (1..letters.len()).rev() {
        let cut: String = letters[..keep].iter().chain(std::iter::once(&'\u{2026}')).collect();
        if fits(&cut) {
            return cut;
        }
    }
    "\u{2026}".to_string()
}

type TileKey = (String, i32, i32, u8, u32, i32);

thread_local! {
    /// Text as ClearType drew it, kept because a redraw hardly ever changes any words.
    static TILES: std::cell::RefCell<std::collections::HashMap<TileKey, std::rc::Rc<Vec<u32>>>> = Default::default();
}

/// Some text in white on black, drawn by Windows with ClearType: each pixel's colours are the amounts.
unsafe fn text_tile(text: &str, w: i32, h: i32, style: Style, flags: u32, dpi: f32) -> std::rc::Rc<Vec<u32>> {
    use windows_sys::Win32::Graphics::Gdi::*;
    let (face, size, weight) = layers::style_font(style);
    let pixels = (size * dpi).round() as i32;
    let key = (text.to_string(), w, h, style as u8, flags, pixels);
    if let Some(tile) = TILES.with(|t| t.borrow().get(&key).cloned()) {
        return tile;
    }
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = w;
    info.bmiHeader.biHeight = -h;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let dc = CreateCompatibleDC(std::ptr::null_mut());
    let mut bits = std::ptr::null_mut();
    let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    let mut out = vec![0u32; (w * h) as usize];
    if !dib.is_null() {
        let old = SelectObject(dc, dib as _);
        const CLEARTYPE: u32 = 5;
        let font = CreateFontW(-pixels, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, CLEARTYPE, 0, wide(face).as_ptr());
        let old_font = SelectObject(dc, font as _);
        SetBkMode(dc, TRANSPARENT as _);
        SetTextColor(dc, 0x00FFFFFF);
        let mut area = RECT { left: 0, top: 0, right: w, bottom: h };
        if flags & DT_WORDBREAK != 0 {
            let lines = (flags & !DT_END_ELLIPSIS) | DT_EDITCONTROL | DT_NOPREFIX;
            let fitted = longest_fit(text, &mut |t| {
                let t: Vec<u16> = t.encode_utf16().collect();
                let mut measured = RECT { left: 0, top: 0, right: w, bottom: h };
                DrawTextW(dc, t.as_ptr() as _, t.len() as i32, &mut measured, lines | DT_CALCRECT);
                measured.bottom <= h && measured.right <= w
            });
            let t: Vec<u16> = fitted.encode_utf16().collect();
            DrawTextW(dc, t.as_ptr() as _, t.len() as i32, &mut area, lines);
        } else {
            let t: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(dc, t.as_ptr() as _, t.len() as i32, &mut area, flags | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }
        GdiFlush();
        std::ptr::copy_nonoverlapping(bits as *const u32, out.as_mut_ptr(), out.len());
        SelectObject(dc, old_font);
        DeleteObject(font as _);
        SelectObject(dc, old);
        DeleteObject(dib as _);
    }
    DeleteDC(dc);
    let tile = std::rc::Rc::new(out);
    TILES.with(|t| {
        let mut t = t.borrow_mut();
        if t.len() > 2000 {
            t.clear(); // a long-running program must not keep every name it ever saw
        }
        t.insert(key, tile.clone());
    });
    tile
}

unsafe fn draw_text(t: &TextTarget, r: RECT, text: &str, colour: u32, style: Style, flags: u32, dpi: f32) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let tile = text_tile(text, w, h, style, flags, dpi);
    let at = |x: i32, y: i32| -> Option<usize> {
        let (l, top, right, bottom) = t.clip;
        (x >= l.max(0) && x < right.min(t.w) && y >= top.max(0) && y < bottom.min(t.h)).then(|| (y * t.w + x) as usize)
    };
    let (left, top) = (t.origin.0 + r.left, t.origin.1 + r.top);
    let colour = argb(colour, 255);
    if !t.colour.is_null() {
        // Liquid: a soft shadow on the words page, under where the card will put the text
        let drop = dpi.round() as i32;
        let small = matches!(style, Style::Small | Style::Name);
        let offsets: &[(i32, i32)] = if small { &[(0, 1)] } else { &[(0, 1), (1, 1)] };
        for &(dx, dy) in offsets {
            for y in 0..h {
                for x in 0..w {
                    let c = tile[(y * w + x) as usize];
                    let most = (c & 255).max((c >> 8) & 255).max((c >> 16) & 255);
                    if most > 0 {
                        if let Some(i) = at(left + x + dx * drop, top + y + dy * drop) {
                            *t.page.add(i) = shade(*t.page.add(i), most * 88 / 255);
                        }
                    }
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            let cover = tile[(y * w + x) as usize] & 0x00FF_FFFF;
            if cover == 0 {
                continue;
            }
            if let Some(i) = at(left + x, top + y) {
                if t.colour.is_null() {
                    *t.page.add(i) = lcd_over(*t.page.add(i), cover, colour);
                } else {
                    *t.colour.add(i) = colour_for_card(cover, colour);
                    *t.cover.add(i) = cover;
                }
                *t.drew = true;
            }
        }
    }
}

/// The text of one piece of glass, for the card to blend over it, the size of that piece's page.
#[derive(Default)]
struct TextLayer {
    colour: Vec<u32>,
    cover: Vec<u32>,
    textures: Option<(ID3D11Texture2D, ID3D11Texture2D, ID3D11ShaderResourceView, ID3D11ShaderResourceView, i32, i32)>,
    showing: bool,
    size: (i32, i32),
}

impl TextLayer {
    /// Start a new drawing: everything clear, and a target that puts text here and shadows on the page.
    fn begin(&mut self, page: &mut Page, origin: (i32, i32), clip: (i32, i32, i32, i32), drew: *mut bool) -> TextTarget {
        let n = (page.w * page.h) as usize;
        self.colour.clear();
        self.colour.resize(n, 0);
        self.cover.clear();
        self.cover.resize(n, 0);
        self.size = (page.w, page.h);
        TextTarget { page: page.pixels.as_mut_ptr(), colour: self.colour.as_mut_ptr(), cover: self.cover.as_mut_ptr(), w: page.w, h: page.h, origin, clip, drew }
    }

    /// Copy the text to the card.
    unsafe fn upload(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, drew: bool) {
        self.showing = drew;
        if !drew {
            return;
        }
        let (w, h) = self.size;
        if !self.textures.as_ref().is_some_and(|t| t.4 == w && t.5 == h) {
            self.textures = None;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w as u32,
                Height: h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let make = || -> Option<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
                let (mut texture, mut view) = (None, None);
                device.CreateTexture2D(&desc, None, Some(&mut texture)).ok()?;
                let texture = texture?;
                device.CreateShaderResourceView(&texture, None, Some(&mut view)).ok()?;
                Some((texture, view?))
            };
            let (Some(a), Some(b)) = (make(), make()) else { self.showing = false; return };
            self.textures = Some((a.0, b.0, a.1, b.1, w, h));
        }
        if let Some(t) = &self.textures {
            context.UpdateSubresource(&t.0, 0, None, self.colour.as_ptr() as *const c_void, (w * 4) as u32, 0);
            context.UpdateSubresource(&t.1, 0, None, self.cover.as_ptr() as *const c_void, (w * 4) as u32, 0);
        }
    }

    fn views(&self) -> Option<(ID3D11ShaderResourceView, ID3D11ShaderResourceView)> {
        self.textures.as_ref().filter(|_| self.showing).map(|t| (t.2.clone(), t.3.clone()))
    }
}

impl Canvas for PlusCanvas {
    fn pill(&mut self, r: RECT, radius: i32, colour: u32) {
        if !self.liquid {
            self.fill(r, radius, colour);
        }
    }

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

    fn wash(&mut self, r: RECT, radius: i32, colour: u32, alpha: u8) {
        unsafe {
            let mut brush = std::ptr::null_mut();
            GdipCreateSolidFill(argb(colour, alpha), &mut brush);
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
        unsafe { draw_text(&self.text, r, s, colour, style, flags, self.dpi) }
    }

    fn icon(&mut self, x: i32, y: i32, size: i32, icon: isize) {
        unsafe {
            // GDI+ loses an icon's transparency when given it directly, so draw it into memory first
            use windows_sys::Win32::Graphics::Gdi::*;
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = size;
            info.bmiHeader.biHeight = -size;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let mut bits = std::ptr::null_mut();
            let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if !dib.is_null() {
                let old = SelectObject(dc, dib as _);
                windows_sys::Win32::UI::WindowsAndMessaging::DrawIconEx(dc, 0, 0, icon as _, size, size, 0, std::ptr::null_mut(), windows_sys::Win32::UI::WindowsAndMessaging::DI_NORMAL);
                let mut picture = std::ptr::null_mut();
                if GdipCreateBitmapFromScan0(size, size, size * 4, PARGB, bits as *mut u8, &mut picture) == 0 {
                    // pixel for pixel: the usual smoothing would soften an icon already the right size
                    GdipSetInterpolationMode(self.g, InterpolationModeNearestNeighbor);
                    GdipSetPixelOffsetMode(self.g, PixelOffsetModeHalf);
                    GdipDrawImageRectI(self.g, picture as _, x, y, size, size);
                    GdipSetPixelOffsetMode(self.g, PixelOffsetModeDefault);
                    GdipSetInterpolationMode(self.g, InterpolationModeDefault);
                    GdipDisposeImage(picture as _);
                }
                SelectObject(dc, old);
                DeleteObject(dib as _);
            }
            DeleteDC(dc);
        }
    }
}

/// One thing on screen: a picture the compositor can move around for free.
struct Sprite {
    visual: IDCompositionVisual,
    swap: Option<IDXGISwapChain1>,
    page: Option<Page>,
    /// Liquid: the page's words, as a picture the graphics card can lay over the glass.
    words: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
}

/// The numbers `liquid.hlsl` reads. Sixteen floats to a line is how the card wants them.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Glass {
    box_: [f32; 4],
    shape: [f32; 4],
    tint: [f32; 4],
    place: [f32; 4],
    mouse: [f32; 4],
    neck0: [f32; 4],
    neck1: [f32; 4],
    neck2: [f32; 4],
    pill: [f32; 4],
    look: [f32; 4],
    glow: [f32; 4],
}

/// What the card needs to draw liquid glass.
struct Gpu {
    vertex: ID3D11VertexShader,
    glass: ID3D11PixelShader,
    words: ID3D11PixelShader,
    names: ID3D11PixelShader,
    names_blend: ID3D11BlendState,
    numbers: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    over: ID3D11BlendState,
    clip: ID3D11RasterizerState,
    wall: Option<ID3D11ShaderResourceView>,
}

#[derive(Clone, Copy)]
struct Panel {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
}

#[derive(Clone, Copy)]
struct Neck {
    from: (i32, i32),
    to: (i32, i32),
    reach: f32,
    end_width: f32,
    mid_width: f32,
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
    gpu: Option<Gpu>,
    panel_text: TextLayer,
    rail_text: TextLayer,
    open_panel: Option<(Panel, Neck)>,
    waiting_panel: Option<Panel>,
    rail_showing: bool,
    /// Where the mouse is, on this window, and how much it leads the light.
    mouse: (f32, f32, f32),
}

/// Only ever touched behind the mutex below, one thread at a time.
unsafe impl Send for Engine {}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

pub fn is_on() -> bool {
    ENGINE.lock().map(|e| e.is_some()).unwrap_or(false)
}

pub fn stop() {
    LIGHT_WANTED.store(false, Ordering::Relaxed);
    DRIFTING.store(false, Ordering::Relaxed);
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
    let sprite = |dcomp: &IDCompositionDevice, below: Option<&IDCompositionVisual>| -> Result<Sprite, String> {
        let visual = dcomp.CreateVisual().map_err(text)?;
        match below {
            Some(under) => root.AddVisual(&visual, true, under).map_err(text)?,
            None => root.AddVisual(&visual, false, None).map_err(text)?,
        }
        visual.SetOffsetX2(FAR_AWAY).map_err(text)?;
        Ok(Sprite { visual, swap: None, page: None, words: None })
    };
    // bottom to top: named outright, as the rail must cover the part of the panel melted into it
    let panel = sprite(&dcomp, None)?;
    let neck = sprite(&dcomp, Some(&panel.visual))?;
    let rail = sprite(&dcomp, Some(&neck.visual))?;
    target.SetRoot(&root).map_err(text)?;
    dcomp.Commit().map_err(text)?;

    let (mut token, input) = (0usize, GdiplusStartupInput { GdiplusVersion: 1, ..std::mem::zeroed() });
    if GdiplusStartup(&mut token, &input, std::ptr::null_mut()) != 0 {
        return Err("GDI+ would not start".into());
    }
    let gpu = match make_gpu(&device) {
        Ok(g) => Some(g),
        Err(why) => {
            crate::log(&format!("liquid unavailable, it will look like Glass: {why}"));
            None
        }
    };
    LIGHT_WANTED.store(false, Ordering::Relaxed);
    *ENGINE.lock().unwrap() = Some(Engine { device, context, factory, dcomp, _target: target, rail, panel, neck, rail_rect: (0, 0, 1, 1), panel_size: (0, 0), origin: window_origin(window), dpi, _gdiplus: token, gpu, panel_text: TextLayer::default(), rail_text: TextLayer::default(), open_panel: None, waiting_panel: None, rail_showing: false, mouse: (0.0, 0.0, 0.0) });
    Ok(())
}

/// Turn the shader's text into code for the card. The compiler is a large library, so it is loaded
/// only for this and let go straight after.
unsafe fn compile(source: &str, entry: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    type Compile = unsafe extern "system" fn(*const c_void, usize, *const u8, *const c_void, *const c_void, *const u8, *const u8, u32, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
    let name = wide("d3dcompiler_47.dll");
    let module = LoadLibraryW(name.as_ptr());
    if module.is_null() {
        return Err("d3dcompiler_47.dll is missing".into());
    }
    let answer = (|| {
        let function = GetProcAddress(module, b"D3DCompile\0".as_ptr()).ok_or("the shader compiler is missing a function")?;
        let function: Compile = std::mem::transmute(function);
        let (mut code, mut errors) = (std::ptr::null_mut(), std::ptr::null_mut());
        let result = function(source.as_ptr() as _, source.len(), std::ptr::null(), std::ptr::null(), std::ptr::null(), entry.as_ptr(), target.as_ptr(), 0, 0, &mut code, &mut errors);
        if result.is_err() {
            if errors.is_null() {
                return Err(format!("shader failed: {result:?}"));
            }
            let errors = ID3DBlob::from_raw(errors);
            let text = std::slice::from_raw_parts(errors.GetBufferPointer() as *const u8, errors.GetBufferSize());
            return Err(String::from_utf8_lossy(text).into_owned());
        }
        if !errors.is_null() {
            drop(ID3DBlob::from_raw(errors));
        }
        // copied out, as the blob's own code goes away with the library below
        let code = ID3DBlob::from_raw(code);
        Ok(std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()).to_vec())
    })();
    let _ = FreeLibrary(windows::Win32::Foundation::HMODULE(module as _));
    answer
}

unsafe fn make_gpu(device: &ID3D11Device) -> Result<Gpu, String> {
    let text = |e: windows::core::Error| e.to_string();
    let source = include_str!("liquid.hlsl");
    let vs = compile(source, b"cover_screen\0", b"vs_5_0\0")?;
    let ps = compile(source, b"glass\0", b"ps_5_0\0")?;
    let ws = compile(source, b"words\0", b"ps_5_0\0")?;
    let ns = compile(source, b"names\0", b"ps_5_0\0")?;
    let mut names = None;
    device.CreatePixelShader(&ns, None, Some(&mut names)).map_err(text)?;
    let (mut vertex, mut glass, mut words) = (None, None, None);
    device.CreateVertexShader(&vs, None, Some(&mut vertex)).map_err(text)?;
    device.CreatePixelShader(&ps, None, Some(&mut glass)).map_err(text)?;
    device.CreatePixelShader(&ws, None, Some(&mut words)).map_err(text)?;
    let mut numbers = None;
    let desc = D3D11_BUFFER_DESC { ByteWidth: std::mem::size_of::<Glass>() as u32, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
    device.CreateBuffer(&desc, None, Some(&mut numbers)).map_err(text)?;
    let mut sampler = None;
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_MIRROR,
        AddressV: D3D11_TEXTURE_ADDRESS_MIRROR,
        AddressW: D3D11_TEXTURE_ADDRESS_MIRROR,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    device.CreateSamplerState(&desc, Some(&mut sampler)).map_err(text)?;
    let mut blend_desc = D3D11_BLEND_DESC::default();
    blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D11_BLEND_ONE,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut over = None;
    device.CreateBlendState(&blend_desc, Some(&mut over)).map_err(text)?;
    blend_desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC1_COLOR; // each colour keeps its own share of the glass
    let mut names_blend = None;
    device.CreateBlendState(&blend_desc, Some(&mut names_blend)).map_err(text)?;
    let mut clip = None;
    let desc = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, ScissorEnable: true.into(), ..Default::default() };
    device.CreateRasterizerState(&desc, Some(&mut clip)).map_err(text)?;
    let missing = || "the graphics card returned nothing".to_string();
    Ok(Gpu {
        vertex: vertex.ok_or_else(missing)?,
        glass: glass.ok_or_else(missing)?,
        words: words.ok_or_else(missing)?,
        names: names.ok_or_else(missing)?,
        names_blend: names_blend.ok_or_else(missing)?,
        numbers: numbers.ok_or_else(missing)?,
        sampler: sampler.ok_or_else(missing)?,
        over: over.ok_or_else(missing)?,
        clip: clip.ok_or_else(missing)?,
        wall: None,
    })
}

fn gpu_ready() -> bool {
    ENGINE.lock().map(|e| e.as_ref().is_some_and(|e| e.gpu.is_some())).unwrap_or(false)
}

/// Blue-green-red colour as the 0-to-1 values the card reads, with a strength.
fn card_colour(bgr: u32, strength: f32) -> [f32; 4] {
    let (r, g, b) = parts(bgr);
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, strength]
}

static MOUSE_X: AtomicI32 = AtomicI32::new(0);
static MOUSE_Y: AtomicI32 = AtomicI32::new(0);
static LIGHT_WANTED: AtomicBool = AtomicBool::new(false);
static LIGHT_WAITING: AtomicBool = AtomicBool::new(false);
/// Aurora's colours are moving, which needs redrawing many times a second. Only true while a
/// section is open or the mouse is near the glass, and the desktop is what's under the mouse.
static DRIFTING: AtomicBool = AtomicBool::new(false);
static CLOCK: OnceLock<Instant> = OnceLock::new();

pub fn drifting() -> bool {
    DRIFTING.load(Ordering::Relaxed)
}

/// Seconds for the shader, kept small enough for its numbers to stay accurate.
fn clock_seconds() -> f32 {
    CLOCK.get_or_init(Instant::now).elapsed().as_secs_f32() % 20000.0
}

/// A colour at full brightness, as neon and aurora need however pale the section's colour is.
fn vivid(bgr: u32) -> [f32; 4] {
    let c = card_colour(bgr, 1.0);
    let top = c[0].max(c[1]).max(c[2]).max(0.01);
    [c[0] / top, c[1] / top, c[2] / top, 1.0]
}

/// From the mouse hook, which must be quick. True when the worker should move the light, at most
/// one request at a time. With Liquid off this is one relaxed read.
pub fn note_mouse(x: i32, y: i32) -> bool {
    if !LIGHT_WANTED.load(Ordering::Relaxed) {
        return false;
    }
    MOUSE_X.store(x, Ordering::Relaxed);
    MOUSE_Y.store(y, Ordering::Relaxed);
    !LIGHT_WAITING.swap(true, Ordering::Relaxed)
}

/// How much the mouse leads the light, given its distance from the glass: all of it when close,
/// none from `reach` away.
pub fn mouse_lead(distance: f32, reach: f32) -> f32 {
    let t = (1.0 - distance / reach).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn distance_to_box(x: f32, y: f32, left: f32, top: f32, w: f32, h: f32) -> f32 {
    let (dx, dy) = ((left - x).max(x - left - w).max(0.0), (top - y).max(y - top - h).max(0.0));
    (dx * dx + dy * dy).sqrt()
}

/// Move the light to the mouse. Redraws only while the mouse is near the glass or just leaving it,
/// so moving it elsewhere costs nothing.
pub unsafe fn move_light(over_desktop: bool) {
    if let Ok(mut guard) = ENGINE.lock() {
        if let Some(e) = guard.as_mut() {
            let (x, y) = (MOUSE_X.load(Ordering::Relaxed) as f32 - e.origin.0, MOUSE_Y.load(Ordering::Relaxed) as f32 - e.origin.1);
            let reach = 260.0 * e.dpi;
            let mut near = f32::MAX;
            if e.rail_showing {
                let r = e.rail_rect;
                near = near.min(distance_to_box(x, y, r.0 as f32, r.1 as f32, r.2 as f32, r.3 as f32));
            }
            if let Some((p, _)) = e.open_panel {
                near = near.min(distance_to_box(x, y, p.x as f32, p.y as f32, p.w as f32, p.h as f32));
            }
            let lead = if over_desktop { mouse_lead(near, reach) } else { 0.0 };
            let aurora = BACKDROP.lock().is_ok_and(|b| b.as_ref().is_some_and(|b| b.liquid && b.look == 1.0));
            let drifting = aurora && over_desktop && (lead > 0.0 || e.open_panel.is_some());
            DRIFTING.store(drifting, Ordering::Relaxed);
            if lead > 0.0 || e.mouse.2 > 0.0 || drifting {
                e.mouse = (x, y, lead);
                if e.rail_showing {
                    e.glass_rail(false);
                }
                e.glass_panel(false);
            }
        }
    }
    let _ = windows_sys::Win32::Graphics::Dwm::DwmFlush(); // no faster than the screen shows
    LIGHT_WAITING.store(false, Ordering::Relaxed);
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
        s.words = None;
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

    /// The wallpaper on the graphics card, with its smaller copies for blurring.
    unsafe fn wall(&mut self) -> Option<ID3D11ShaderResourceView> {
        let device = self.device.clone();
        let context = self.context.clone();
        let gpu = self.gpu.as_mut()?;
        if gpu.wall.is_none() {
            let guard = BACKDROP.lock().ok()?;
            let b = guard.as_ref().filter(|b| b.liquid)?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: b.full_w as u32,
                Height: b.full_h as u32,
                MipLevels: 0, // all of them
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
                ..Default::default()
            };
            let (mut texture, mut view) = (None, None);
            device.CreateTexture2D(&desc, None, Some(&mut texture)).ok()?;
            let texture = texture?;
            device.CreateShaderResourceView(&texture, None, Some(&mut view)).ok()?;
            let view: ID3D11ShaderResourceView = view?;
            context.UpdateSubresource(&texture, 0, None, b._pixels.as_ptr() as *const c_void, (b.full_w * 4) as u32, 0);
            context.GenerateMips(&view);
            gpu.wall = Some(view);
        }
        gpu.wall.clone()
    }

    /// Copy a sprite's page of words to the card.
    unsafe fn upload_words(&self, s: &mut Sprite) {
        let Some(page) = s.page.as_ref() else { return };
        if s.words.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: page.w as u32,
                Height: page.h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let (mut texture, mut view) = (None, None);
            if self.device.CreateTexture2D(&desc, None, Some(&mut texture)).is_err() {
                return;
            }
            let Some(texture) = texture else { return };
            if self.device.CreateShaderResourceView(&texture, None, Some(&mut view)).is_err() {
                return;
            }
            let Some(view) = view else { return };
            s.words = Some((texture, view));
        }
        if let Some((texture, _)) = &s.words {
            self.context.UpdateSubresource(texture, 0, None, page.pixels.as_ptr() as *const c_void, (page.w * 4) as u32, 0);
        }
    }

    /// Draw glass shapes, then the words over them, and show the result.
    unsafe fn render(&mut self, s: &Sprite, shapes: &[(Glass, ComRect)], with_words: bool, text: Option<(ID3D11ShaderResourceView, ID3D11ShaderResourceView)>, x: f32, y: f32) {
        let Some(wall) = self.wall() else { return };
        let (Some(gpu), Some(swap), Some(page)) = (self.gpu.as_ref(), &s.swap, &s.page) else { return };
        let Ok(back) = swap.GetBuffer::<ID3D11Texture2D>(0) else { return };
        let mut target = None;
        if self.device.CreateRenderTargetView(&back, None, Some(&mut target)).is_err() {
            return;
        }
        let Some(target) = target else { return };
        let c = &self.context;
        c.ClearRenderTargetView(&target, &[0.0; 4]);
        c.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: page.w as f32, Height: page.h as f32, MaxDepth: 1.0, ..Default::default() }]));
        c.RSSetState(&gpu.clip);
        c.OMSetBlendState(&gpu.over, Some(&[0.0; 4]), 0xFFFF_FFFF);
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(&gpu.vertex, None);
        c.PSSetShader(&gpu.glass, None);
        c.PSSetSamplers(0, Some(&[Some(gpu.sampler.clone())]));
        c.PSSetConstantBuffers(0, Some(&[Some(gpu.numbers.clone())]));
        c.PSSetShaderResources(0, Some(&[Some(wall)]));
        for (numbers, area) in shapes {
            c.UpdateSubresource(&gpu.numbers, 0, None, numbers as *const Glass as *const c_void, 0, 0);
            c.RSSetScissorRects(Some(&[*area]));
            c.Draw(3, 0);
        }
        if with_words {
            if let Some((_, view)) = &s.words {
                c.RSSetScissorRects(Some(&[ComRect { left: 0, top: 0, right: page.w, bottom: page.h }]));
                c.PSSetShader(&gpu.words, None);
                c.PSSetShaderResources(0, Some(&[Some(view.clone())]));
                c.Draw(3, 0);
            }
        }
        if let Some((colour, cover)) = text {
            c.RSSetScissorRects(Some(&[ComRect { left: 0, top: 0, right: page.w, bottom: page.h }]));
            c.PSSetShader(&gpu.names, None);
            c.OMSetBlendState(&gpu.names_blend, Some(&[0.0; 4]), 0xFFFF_FFFF);
            c.PSSetShaderResources(0, Some(&[Some(colour), Some(cover)]));
            c.Draw(3, 0);
            c.PSSetShaderResources(1, Some(&[None]));
        }
        c.OMSetRenderTargets(None, None);
        let _ = swap.Present(0, DXGI_PRESENT(0));
        let _ = s.visual.SetOffsetX2(x);
        let _ = s.visual.SetOffsetY2(y);
        let _ = self.dcomp.Commit();
    }

    fn base_numbers(&self, left: f32, top: f32) -> Glass {
        let (w, h, look) = BACKDROP.lock().ok().and_then(|b| b.as_ref().map(|b| (b.full_w as f32, b.full_h as f32, b.look))).unwrap_or((1.0, 1.0, 0.0));
        Glass {
            place: [self.origin.0 + left, self.origin.1 + top, w, h],
            mouse: [self.mouse.0 - left, self.mouse.1 - top, self.mouse.2, 0.0],
            look: [look, clock_seconds(), 1.0, 0.0],
            ..Default::default()
        }
    }

    /// The labels: each a small lens with its own colour, with the words drawn once and kept, so
    /// moving the light does not have to draw them again.
    unsafe fn glass_rail(&mut self, repaint: bool) {
        let (x, y, w, h) = self.rail_rect;
        let dpi = self.dpi;
        let margin = (16.0 * dpi) as i32;
        let mut rail = Sprite { visual: self.rail.visual.clone(), swap: self.rail.swap.take(), page: self.rail.page.take(), words: self.rail.words.take() };
        if self.ensure(&mut rail, w + 2 * margin, h + 2 * margin).is_ok() {
            if repaint || rail.words.is_none() {
                let mut drew = false;
                if let Some(page) = rail.page.as_mut() {
                    page.clear();
                    GdipTranslateWorldTransform(page.g, margin as f32, margin as f32, MatrixOrderPrepend);
                    let everywhere = (0, 0, page.w, page.h);
                    let text = self.rail_text.begin(page, (margin, margin), everywhere, &mut drew);
                    let mut c = PlusCanvas { g: page.g, dpi, alpha: 255, liquid: true, text };
                    layers::paint_rail_on(&mut c, dpi, layers::pill_tint());
                }
                self.upload_words(&mut rail);
                self.rail_text.upload(&self.device, &self.context, drew);
            }
            let (left, top) = ((x - margin) as f32, (y - margin) as f32);
            let base = self.base_numbers(left, top);
            let tint = layers::pill_tint();
            let mut shapes = vec![];
            if let Ok(pills) = layers::PILLS.lock() {
                for p in pills.iter() {
                    let r = p.rect;
                    let colour = layers::pill_colour(p, tint);
                    let aurora = base.look[0] == 1.0;
                    let strength = match (p.active, p.hover) {
                        (true, _) => if aurora { 0.35 } else { 0.62 },
                        (_, true) => if aurora { 0.22 } else { 0.42 },
                        _ => if aurora { 0.06 } else { 0.30 },
                    };
                    let numbers = Glass {
                        box_: [(r.left + margin) as f32, (r.top + margin) as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32],
                        shape: [14.0 * dpi, 12.0 * dpi, 2.2, dpi],
                        tint: card_colour(colour, strength),
                        mouse: [base.mouse[0], base.mouse[1], base.mouse[2], 0.55],
                        look: [base.look[0], base.look[1], if p.active { 1.0 } else if p.hover { 0.8 } else { 0.5 }, 0.0],
                        glow: vivid(p.colour),
                        ..base
                    };
                    shapes.push((numbers, ComRect { left: r.left, top: r.top, right: r.right + 2 * margin, bottom: r.bottom + 2 * margin }));
                }
            }
            let text = self.rail_text.views();
            self.render(&rail, &shapes, true, text, left, top);
        }
        self.rail = rail;
    }

    /// The open panel, with the neck melted into it and the label it pours from.
    unsafe fn glass_panel(&mut self, repaint: bool) {
        let Some((p, n)) = self.open_panel else { return };
        let dpi = self.dpi;
        let margin = (26.0 * dpi) as i32;
        let rail = self.rail_rect;
        let (mut left, mut top, mut right, mut bottom) = (p.x, p.y, p.x + p.w, p.y + p.h);
        for (cx, cy) in [n.from, n.to] {
            let half = n.end_width as i32 + 1;
            left = left.min(cx - half);
            top = top.min(cy - half);
            right = right.max(cx + half);
            bottom = bottom.max(cy + half);
        }
        let active = layers::PILLS.lock().ok().and_then(|pills| pills.iter().find(|q| q.active).map(|q| q.rect));
        if let Some(r) = active {
            left = left.min(rail.0 + r.left);
            top = top.min(rail.1 + r.top);
            right = right.max(rail.0 + r.right);
            bottom = bottom.max(rail.1 + r.bottom);
        }
        let (left, top, right, bottom) = (left - margin, top - margin, right + margin, bottom + margin);
        let (w, h) = (right - left, bottom - top);
        let mut panel = Sprite { visual: self.panel.visual.clone(), swap: self.panel.swap.take(), page: self.panel.page.take(), words: self.panel.words.take() };
        if self.ensure(&mut panel, w, h).is_ok() {
            let settings = layers::settings_showing();
            if settings && repaint {
                let mut drew = false;
                if let Some(page) = panel.page.as_mut() {
                    page.clear();
                    let (fx, fy) = ((p.x - left) as f32, (p.y - top) as f32);
                    let body = round_path(fx, fy, p.w as f32, p.h as f32, (p.radius * 2) as f32);
                    GdipSetClipPath(page.g, body, CombineModeReplace);
                    GdipDeletePath(body);
                    GdipTranslateWorldTransform(page.g, fx, fy, MatrixOrderPrepend);
                    let (ox, oy) = (p.x - left, p.y - top);
                    let text = self.panel_text.begin(page, (ox, oy), (ox, oy, ox + p.w, oy + p.h), &mut drew);
                    let mut c = PlusCanvas { g: page.g, dpi, alpha: 255, liquid: true, text };
                    layers::paint_settings_on(&mut c, dpi, layers::pill_tint(), layers::accent());
                }
                self.upload_words(&mut panel);
                self.panel_text.upload(&self.device, &self.context, drew);
            }
            let (lf, tf) = (left as f32, top as f32);
            let base = self.base_numbers(lf, tf);
            let tint = blend(blend(layers::panel_tint(), layers::accent(), 0.16), 0x00FFFFFF, 0.04);
            let band = (34.0 * dpi).min(p.h as f32 / 2.0).min(p.w as f32 / 2.0);
            let pill = active.map(|r| [(rail.0 + r.left) as f32 - lf, (rail.1 + r.top) as f32 - tf, (r.right - r.left) as f32, (r.bottom - r.top) as f32]);
            let (fx, fy, tx, ty) = (n.from.0 as f32 - lf, n.from.1 as f32 - tf, n.to.0 as f32 - lf, n.to.1 as f32 - tf);
            let mid = (fx + tx) / 2.0;
            let numbers = Glass {
                box_: [(p.x - left) as f32, (p.y - top) as f32, p.w as f32, p.h as f32],
                shape: [p.radius as f32, band, if settings { 3.2 } else { 0.9 }, dpi],
                tint: card_colour(tint, if settings { if base.look[0] == 1.0 { 0.42 } else { 0.66 } } else { 0.10 }),
                mouse: [base.mouse[0], base.mouse[1], base.mouse[2], if settings { 0.2 } else { 0.32 }],
                neck0: [fx, fy, mid, fy],
                neck1: [mid, ty, tx, ty],
                neck2: [if pill.is_some() { 1.0 } else { 0.0 }, n.reach.clamp(0.02, 1.0), n.end_width / 2.0, n.mid_width / 2.0],
                pill: pill.unwrap_or([0.0; 4]),
                glow: vivid(layers::accent()),
                ..base
            };
            let names = if settings { self.panel_text.views() } else { None };
            self.render(&panel, &[(numbers, ComRect { left: 0, top: 0, right: w, bottom: h })], settings, names, lf, tf);
        }
        self.panel = panel;
    }

    unsafe fn draw_rail(&mut self) {
        self.rail_showing = true;
        if self.liquid_on() {
            return self.glass_rail(true);
        }
        let (x, y, w, h) = self.rail_rect;
        let mut rail = Sprite { visual: self.rail.visual.clone(), swap: self.rail.swap.take(), page: self.rail.page.take(), words: self.rail.words.take() };
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
                let mut drew = false;
                let text = TextTarget { page: page.pixels.as_mut_ptr(), colour: std::ptr::null_mut(), cover: std::ptr::null_mut(), w: page.w, h: page.h, origin: (0, 0), clip: (0, 0, page.w, page.h), drew: &mut drew };
                let mut c = PlusCanvas { g: page.g, dpi: self.dpi, alpha: if glass { 150 } else { 225 }, liquid: false, text };
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

    fn liquid_on(&self) -> bool {
        let on = self.gpu.is_some() && BACKDROP.lock().map(|b| b.as_ref().is_some_and(|b| b.liquid)).unwrap_or(false);
        LIGHT_WANTED.store(on, Ordering::Relaxed);
        on
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleartype_blends_each_colour_by_its_own_amount() {
        // white at full red, half green, no blue over a solid dark pixel
        let out = lcd_over(0xFF10_1010, 0x00FF_8000, 0xFFFF_FFFF);
        assert_eq!((out >> 16) & 255, 255);
        assert_eq!((out >> 8) & 255, (255 * 128 + 16 * 127) / 255);
        assert_eq!(out & 255, 16);
        assert_eq!(out >> 24, 255);
        // a coloured text keeps its colour where fully covered
        let red = lcd_over(0xFF10_1010, 0x00FF_FFFF, 0xFFC0_2040);
        assert_eq!(red & 0x00FF_FFFF, 0x00C0_2040);
        // no cover leaves the pixel alone, even a clear one
        assert_eq!(lcd_over(0, 0, 0xFFFF_FFFF), 0);
        assert_eq!(lcd_over(0x8040_2010, 0, 0xFFFF_FFFF), 0x8040_2010);
    }

    #[test]
    fn the_card_gets_colour_mixed_with_the_amounts() {
        assert_eq!(colour_for_card(0x00FF_8000, 0xFFFF_FFFF), 0x00FF_8000);
        assert_eq!(colour_for_card(0x00FF_FF00, 0xFFC0_2040), 0x00C0_2000);
    }

    #[test]
    fn shade_darkens_and_never_lightens() {
        let out = shade(0xFF80_8080, 128);
        assert!((out & 255) < 0x80 && out >> 24 == 255);
        assert_eq!(shade(0, 0), 0);
        assert_eq!(shade(0, 255) >> 24, 255);
    }

    #[test]
    fn long_names_are_cut_to_what_fits() {
        let short = longest_fit("abc", &mut |t| t.chars().count() <= 5);
        assert_eq!(short, "abc");
        let cut = longest_fit("a long file name", &mut |t| t.chars().count() <= 5);
        assert_eq!(cut, "a lo\u{2026}");
        assert_eq!(longest_fit("anything", &mut |_| false), "\u{2026}");
    }

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
    fn wallpaper_goes_where_each_windows_setting_puts_it() {
        // a square picture on a wide screen
        let fill = wallpaper_pieces("fill", 100.0, 100.0, 200.0, 100.0);
        assert_eq!(fill[0], ((0.0, 0.0, 200.0, 100.0), (0.0, 25.0, 100.0, 50.0)));
        let fit = wallpaper_pieces("fit", 100.0, 100.0, 200.0, 100.0);
        assert_eq!(fit[0].0, (50.0, 0.0, 100.0, 100.0));
        let center = wallpaper_pieces("center", 100.0, 100.0, 200.0, 200.0);
        assert_eq!(center[0].0, (50.0, 50.0, 100.0, 100.0));
        assert_eq!(wallpaper_pieces("stretch", 100.0, 100.0, 200.0, 100.0)[0].0, (0.0, 0.0, 200.0, 100.0));
        assert_eq!(wallpaper_pieces("tile", 100.0, 100.0, 250.0, 100.0).len(), 3);
    }

    #[test]
    fn the_mouse_leads_the_light_only_when_near() {
        assert_eq!(mouse_lead(0.0, 200.0), 1.0);
        assert_eq!(mouse_lead(200.0, 200.0), 0.0);
        assert_eq!(mouse_lead(900.0, 200.0), 0.0);
        assert!(mouse_lead(50.0, 200.0) > mouse_lead(150.0, 200.0));
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
    if let Ok(mut guard) = ENGINE.lock() {
        if let Some(e) = guard.as_mut() {
            e.rail_showing = false;
            let _ = e.rail.visual.SetOffsetX2(FAR_AWAY);
            let _ = e.dcomp.Commit();
        }
    }
}

pub unsafe fn hide_panel() {
    if let Ok(mut guard) = ENGINE.lock() {
        if let Some(e) = guard.as_mut() {
            e.open_panel = None;
            e.waiting_panel = None;
            let _ = e.panel.visual.SetOffsetX2(FAR_AWAY);
            let _ = e.neck.visual.SetOffsetX2(FAR_AWAY);
            let _ = e.dcomp.Commit();
        }
    }
}

pub unsafe fn set_panel(x: i32, y: i32, w: i32, h: i32, radius: i32) {
    let Ok(mut guard) = ENGINE.lock() else { return };
    let Some(e) = guard.as_mut() else { return };
    if e.liquid_on() {
        e.waiting_panel = Some(Panel { x, y, w: w.max(2), h: h.max(2), radius }); // drawn with its neck, which comes next
        return;
    }
    let dpi = e.dpi;
    let shadow = (14.0 * dpi) as i32;
    let (w, h) = (w.max(2), h.max(2));
    let mut panel = Sprite { visual: e.panel.visual.clone(), swap: e.panel.swap.take(), page: e.panel.page.take(), words: e.panel.words.take() };
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
            let (shade, accent, pill) = (layers::panel_tint(), layers::accent(), layers::pill_tint());
            // real glass lets more through; text on Settings still has to stay easy to read
            let alpha = match (frosted, settings) {
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
                if e.panel_size == (w, h) {
                    bend_rim(page, shadow, shadow, w, h, d / 2.0, sx, sy, 20.0 * dpi);
                }
                rim_light(g, body, fx, fy, fw, fh, 1.3 * dpi, 215);
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
                let mut drew = false;
                let text = TextTarget {
                    page: page.pixels.as_mut_ptr(),
                    colour: std::ptr::null_mut(),
                    cover: std::ptr::null_mut(),
                    w: page.w,
                    h: page.h,
                    origin: (shadow, shadow),
                    clip: (shadow, shadow, shadow + w, shadow + h),
                    drew: &mut drew,
                };
                let mut c = PlusCanvas { g, dpi, alpha: 255, liquid: false, text };
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
    if let Some(panel) = e.waiting_panel.take() {
        e.open_panel = Some((panel, Neck { from, to, reach, end_width, mid_width }));
        if BACKDROP.lock().is_ok_and(|b| b.as_ref().is_some_and(|b| b.look == 1.0)) {
            DRIFTING.store(true, Ordering::Relaxed); // the worker's next look decides if it stays on
        }
        return e.glass_panel(true);
    }
    let margin = end_width as i32 + 2;
    let (left, top) = (from.0.min(to.0) - margin, from.1.min(to.1) - margin);
    let (w, h) = ((to.0 - from.0).abs() + 2 * margin, (to.1 - from.1).abs() + 2 * margin);
    let outline = neck_outline((from.0 as f32, from.1 as f32), (to.0 as f32, to.1 as f32), reach, end_width, mid_width);
    let colour = blend(layers::pill_tint(), layers::accent(), 0.6);
    let mut neck = Sprite { visual: e.neck.visual.clone(), swap: e.neck.swap.take(), page: e.neck.page.take(), words: e.neck.words.take() };
    if e.ensure(&mut neck, w, h).is_ok() {
        if let Some(page) = neck.page.as_mut() {
            page.clear();
            let points: Vec<PointF> = outline.iter().map(|&(x, y)| PointF { X: x - left as f32, Y: y - top as f32 }).collect();
            let frosted = has_glass();
            if frosted {
                let mut shape = std::ptr::null_mut();
                GdipCreatePath(FillModeWinding, &mut shape);
                GdipAddPathPolygon(shape, points.as_ptr(), points.len() as i32);
                paint_backdrop(page.g, shape, 0.0, 0.0, w as f32, h as f32, e.origin.0 + left as f32, e.origin.1 + top as f32);
                GdipDeletePath(shape);
            }
            let mut brush = std::ptr::null_mut();
            GdipCreateSolidFill(argb(colour, if frosted { 150 } else { 235 }), &mut brush);
            GdipFillPolygon(page.g, brush as _, points.as_ptr(), points.len() as i32, FillModeWinding);
            GdipDeleteBrush(brush as _);
        }
        e.show(&neck, left as f32, top as f32);
    }
    e.neck = neck;
}
