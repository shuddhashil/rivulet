//! The program never draws on the wallpaper. This reads its average colour for tinting, and hands
//! back the user's own picture if an earlier version left its boxed copy in place.

use std::path::{Path, PathBuf};
use std::result::Result::Ok; // GdiPlus also defines a constant called Ok
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const PIXEL_24BPP_RGB: i32 = 0x21808;
const SAMPLES: i32 = 16;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn current() -> String {
    let mut buf = [0u16; 1024];
    unsafe { SystemParametersInfoW(SPI_GETDESKWALLPAPER, 1024, buf.as_mut_ptr() as _, 0) };
    let n = buf.iter().position(|c| *c == 0).unwrap_or(0);
    String::from_utf16_lossy(&buf[..n])
}

/// The picture on the desktop, even if its original file has since gone.
pub fn picture_path() -> Option<PathBuf> {
    let now = current();
    if Path::new(&now).exists() {
        return Some(PathBuf::from(now));
    }
    let copy = Path::new(&std::env::var("APPDATA").ok()?).join(r"Microsoft\Windows\Themes\TranscodedWallpaper");
    copy.exists().then_some(copy)
}

unsafe fn read_setting(key: &str, name: &str) -> Option<String> {
    let mut buffer = [0u16; 64];
    let mut size = 128u32;
    let done = RegGetValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buffer.as_mut_ptr() as _, &mut size);
    if done != 0 {
        return None;
    }
    let n = buffer.iter().position(|c| *c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buffer[..n]))
}

pub fn placement_name(style: &str, tile: &str) -> &'static str {
    if tile.trim() == "1" {
        return "tile";
    }
    match style.trim() {
        "0" => "center",
        "2" => "stretch",
        "6" => "fit",
        _ => "fill", // Fill and Span both cover a single screen
    }
}

/// How Windows is told to place the wallpaper: fill, fit, stretch, center or tile.
pub fn placement_setting() -> String {
    unsafe {
        let style = read_setting(r"Control Panel\Desktop", "WallpaperStyle").unwrap_or_default();
        let tile = read_setting(r"Control Panel\Desktop", "TileWallpaper").unwrap_or_default();
        placement_name(&style, &tile).to_string()
    }
}

/// Windows' desktop background colour, as 0xRRGGBB. Shows around a Fit or Center wallpaper.
pub fn background_colour() -> u32 {
    let text = unsafe { read_setting(r"Control Panel\Colors", "Background") }.unwrap_or_default();
    let parts: Vec<u32> = text.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    match parts[..] {
        [r, g, b] => (r.min(255) << 16) | (g.min(255) << 8) | b.min(255),
        _ => 0,
    }
}

/// The wallpaper's average colour as blue, green, red. GDI+ runs only for this call, so it holds
/// no memory the rest of the time.
pub fn average_colour() -> Option<u32> {
    let path = picture_path()?;
    unsafe {
        let (mut token, input) = (0, GdiplusStartupInput { GdiplusVersion: 1, ..std::mem::zeroed() });
        if GdiplusStartup(&mut token, &input, std::ptr::null_mut()) != 0 {
            return None;
        }
        let mut image = std::ptr::null_mut();
        let mut answer = None;
        if GdipLoadImageFromFile(wide(&path.to_string_lossy()).as_ptr(), &mut image) == 0 {
            let (mut small, mut g) = (std::ptr::null_mut(), std::ptr::null_mut());
            GdipCreateBitmapFromScan0(SAMPLES, SAMPLES, 0, PIXEL_24BPP_RGB, std::ptr::null_mut(), &mut small);
            GdipGetImageGraphicsContext(small as _, &mut g);
            GdipSetInterpolationMode(g, InterpolationModeHighQualityBilinear);
            GdipDrawImageRectI(g, image, 0, 0, SAMPLES, SAMPLES);
            let (mut r, mut gr, mut b) = (0u32, 0u32, 0u32);
            for y in 0..SAMPLES {
                for x in 0..SAMPLES {
                    let mut argb = 0u32;
                    GdipBitmapGetPixel(small, x, y, &mut argb);
                    r += (argb >> 16) & 0xFF;
                    gr += (argb >> 8) & 0xFF;
                    b += argb & 0xFF;
                }
            }
            let n = (SAMPLES * SAMPLES) as u32;
            answer = Some(((b / n) << 16) | ((gr / n) << 8) | (r / n));
            GdipDeleteGraphics(g);
            GdipDisposeImage(small as _);
            GdipDisposeImage(image);
        }
        GdiplusShutdown(token);
        answer
    }
}

fn is_ours(path: &str) -> bool {
    let name = Path::new(path).file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
    let parent = Path::new(path).parent().map(|p| p.to_string_lossy().to_lowercase());
    name == "sections_wallpaper" && parent == Some(crate::state_dir().to_string_lossy().to_lowercase())
}

unsafe fn write_setting(name: &str, text: &str) {
    let (key, value, data) = (wide(r"Control Panel\Desktop"), wide(name), wide(text));
    RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), REG_SZ, data.as_ptr() as _, data.len() as u32 * 2);
}

pub fn restore() -> Result<(), String> {
    if !is_ours(&current()) {
        return Ok(());
    }
    let info = crate::state_dir().join("wallpaper.txt");
    let text = std::fs::read_to_string(&info).map_err(|_| "Could not find your original wallpaper's details.".to_string())?;
    let f: Vec<&str> = text.lines().collect();
    let [path, base, style, tile, ..] = f[..] else {
        return Err("Could not read your original wallpaper's details.".into());
    };
    let target = if Path::new(path).exists() { path } else { base };
    unsafe {
        write_setting("WallpaperStyle", style);
        write_setting("TileWallpaper", tile);
        if SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, wide(target).as_ptr() as _, SPIF_UPDATEINIFILE | SPIF_SENDCHANGE) == 0 {
            return Err("Windows refused to restore your wallpaper.".into());
        }
    }
    for old in ["sections_wallpaper.bmp", "sections_wallpaper.png"] {
        let _ = std::fs::remove_file(crate::state_dir().join(old));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_follows_the_windows_settings() {
        assert_eq!(placement_name("10", "0"), "fill");
        assert_eq!(placement_name("6", "0"), "fit");
        assert_eq!(placement_name("2", "0"), "stretch");
        assert_eq!(placement_name("0", "0"), "center");
        assert_eq!(placement_name("0", "1"), "tile");
        assert_eq!(placement_name("22", "0"), "fill");
    }
}
