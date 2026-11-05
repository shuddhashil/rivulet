//! Windows' real right-click menu for a file, and a small box for renaming it. This runs on its own
//! thread: the main thread answers the mouse hook, and a slow shell extension must never stall that.

use crate::wallpaper::wide;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{HWND as ComHwnd, LPARAM as ComLparam, WPARAM as ComWparam};
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::System::Ole::{IDropSource, OleFlushClipboard, OleInitialize, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, SHBindToParent, SHDoDragDrop, SHGetDesktopFolder, SHParseDisplayName, CMINVOKECOMMANDINFO};
use windows::Win32::UI::WindowsAndMessaging::HMENU as ComMenu;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontW, DeleteObject, GetStockObject, DEFAULT_GUI_FONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{mouse_event, SetFocus, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CMF_NORMAL: u32 = 0;
const CMF_CANRENAME: u32 = 0x10;
const GCS_VERBW: u32 = 4;
const FIRST_ID: u32 = 1;
const EM_SETSEL_MSG: u32 = 0x00B1;

/// True while a menu or a drag to another program is under way. The mouse hook then keeps out of the
/// way: if it swallowed a click the menu would never hear of it, and neither would the drag.
pub static SHELL_BUSY: AtomicBool = AtomicBool::new(false);

/// Bring our window to the front. Windows only lets the program the user is in do that, so join that
/// program's input for a moment. Without this a menu gets no keys and may not close on a click away.
pub unsafe fn to_front(window: HWND) {
    let current = GetForegroundWindow();
    let them = GetWindowThreadProcessId(current, std::ptr::null_mut());
    let me = GetCurrentThreadId();
    let joined = them != 0 && them != me && AttachThreadInput(me, them, 1) != 0;
    SetForegroundWindow(window);
    if joined {
        AttachThreadInput(me, them, 0);
    }
}

thread_local! {
    /// The open menu's newer interfaces, which fill "Send to" and "Open with" as they open.
    static MENUS: RefCell<(Option<IContextMenu2>, Option<IContextMenu3>)> = const { RefCell::new((None, None)) };
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        let answer = MENUS.with(|m| {
            let m = m.borrow();
            if let Some(three) = &m.1 {
                let mut result = windows::Win32::Foundation::LRESULT(0);
                let _ = three.HandleMenuMsg2(msg, ComWparam(wparam), ComLparam(lparam), Some(&mut result));
                Some(result.0)
            } else if let Some(two) = &m.0 {
                let _ = two.HandleMenuMsg(msg, ComWparam(wparam), ComLparam(lparam));
                Some(0)
            } else {
                None
            }
        });
        if let Some(a) = answer {
            return a;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn parse(path: &Path) -> Option<*mut ITEMIDLIST> {
    let name = wide(&path.to_string_lossy());
    let mut item = std::ptr::null_mut();
    SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut item, 0, None).ok()?;
    Some(item)
}

const BIN_ICON_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\DefaultIcon";

/// Where Windows says the Recycle Bin's picture is right now: it switches this between the empty and
/// the full one itself, which the shell's cached pictures do not always follow.
pub fn recycle_bin_icon_place() -> Option<String> {
    unsafe { crate::wallpaper::read_setting(BIN_ICON_KEY, "") }
}

/// The Recycle Bin's current picture at exactly this many pixels.
pub unsafe fn recycle_bin_icon(side: i32) -> isize {
    use windows_sys::Win32::UI::Shell::SHDefExtractIconW;
    let Some(place) = recycle_bin_icon_place() else { return 0 };
    let Some((file, index)) = place.rsplit_once(',') else { return 0 };
    let Ok(index) = index.trim().parse::<i32>() else { return 0 };
    let mut icon = std::ptr::null_mut();
    if SHDefExtractIconW(wide(file.trim()).as_ptr(), index, 0, &mut icon, std::ptr::null_mut(), side as u32) != 0 {
        return 0;
    }
    icon as isize
}

/// The icon of something with no ordinary path, such as the Recycle Bin.
pub unsafe fn shell_icon(path: &Path) -> isize {
    let Some(item) = parse(path) else { return 0 };
    let mut info: SHFILEINFOW = std::mem::zeroed();
    SHGetFileInfoW(item as *const u16, 0, &mut info, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON);
    CoTaskMemFree(Some(item as *const _));
    info.hIcon as isize
}

/// Fit a picture from the shell into a square icon: centred, with see-through corners kept. The
/// shell hands back colours already mixed with their transparency, and thumbnails with no
/// transparency at all, so both are made into what an icon expects. `from` is top-down BGRA.
fn square_pixels(from: &[u8], w: usize, h: usize, side: usize) -> Vec<u8> {
    let mut out = vec![0u8; side * side * 4];
    let solid = from.chunks_exact(4).all(|p| p[3] == 0);
    let (dx, dy) = (side.saturating_sub(w) / 2, side.saturating_sub(h) / 2);
    for y in 0..h.min(side) {
        for x in 0..w.min(side) {
            let at = (y * w + x) * 4;
            let to = ((y + dy) * side + x + dx) * 4;
            let a = if solid { 255 } else { from[at + 3] as u32 };
            for c in 0..3 {
                let v = from[at + c] as u32;
                out[to + c] = if a == 0 || solid { v as u8 } else { (v * 255 / a).min(255) as u8 };
            }
            out[to + 3] = a as u8;
        }
    }
    out
}

/// Every item on the desktop by the name it shows, lower case, including Windows' own special items,
/// so one with no file still opens and has its picture.
pub unsafe fn desktop_items() -> std::collections::HashMap<String, PathBuf> {
    use windows::Win32::UI::Shell::Common::STRRET;
    use windows::Win32::UI::Shell::{IEnumIDList, SHGetDesktopFolder, StrRetToStrW, SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, SHGDN_FORPARSING, SHGDN_NORMAL};
    let mut found = std::collections::HashMap::new();
    let Ok(desktop) = SHGetDesktopFolder() else { return found };
    let mut list: Option<IEnumIDList> = None;
    if desktop.EnumObjects(ComHwnd(std::ptr::null_mut()), (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32, &mut list).is_err() {
        return found;
    }
    let Some(list) = list else { return found };
    let name_of = |item: *const ITEMIDLIST, how| -> Option<String> {
        let mut ret = STRRET::default();
        desktop.GetDisplayNameOf(item, how, &mut ret).ok()?;
        let mut text = windows::core::PWSTR::null();
        StrRetToStrW(&mut ret, Some(item), &mut text).ok()?;
        let out = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        out
    };
    loop {
        let mut one = [std::ptr::null_mut::<ITEMIDLIST>()];
        let mut got = 0u32;
        if list.Next(&mut one, Some(&mut got)).is_err() || got == 0 {
            break;
        }
        if let (Some(shown), Some(parsing)) = (name_of(one[0], SHGDN_NORMAL), name_of(one[0], SHGDN_FORPARSING)) {
            // "::{...}" names need this prefix to open and to find their picture
            let path = if parsing.starts_with("::") { format!("shell:{parsing}") } else { parsing };
            found.insert(shown.to_lowercase(), PathBuf::from(path));
        }
        CoTaskMemFree(Some(one[0] as *const _));
    }
    found
}

/// An icon at exactly this many pixels, from the same source as Explorer: real thumbnails and
/// folder previews, sharp at any display scale. Returns 0 if the shell has nothing for it.
pub unsafe fn sharp_icon(path: &Path, side: i32) -> isize {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF};
    use windows_sys::Win32::Graphics::Gdi::{BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, GetDIBits, GetObjectW, HBITMAP, DIB_RGB_COLORS};
    let name = wide(&path.to_string_lossy());
    let Ok(factory) = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(PCWSTR(name.as_ptr()), None) else { return 0 };
    let Ok(picture) = factory.GetImage(SIZE { cx: side, cy: side }, SIIGBF(0)) else { return 0 };
    let picture = picture.0 as HBITMAP;
    let mut facts: BITMAP = std::mem::zeroed();
    let mut made = 0;
    if GetObjectW(picture as _, std::mem::size_of::<BITMAP>() as i32, &mut facts as *mut _ as _) != 0 && facts.bmWidth > 0 && facts.bmHeight > 0 {
        let (w, h) = (facts.bmWidth, facts.bmHeight);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut raw = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        if GetDIBits(dc, picture, 0, h as u32, raw.as_mut_ptr() as _, &mut info, DIB_RGB_COLORS) != 0 {
            let pixels = square_pixels(&raw, w as usize, h as usize, side as usize);
            info.bmiHeader.biWidth = side;
            info.bmiHeader.biHeight = -side;
            let mut bits = std::ptr::null_mut();
            let colour = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if !colour.is_null() {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
                let mask = CreateBitmap(side, side, 1, 1, std::ptr::null());
                let icon = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: colour };
                made = CreateIconIndirect(&icon) as isize;
                DeleteObject(mask as _);
                DeleteObject(colour as _);
            }
        }
        DeleteDC(dc);
    }
    DeleteObject(picture as _);
    made
}

/// Show the menu at the mouse and carry out the choice. Any failure just means no menu.
pub fn show(paths: Vec<PathBuf>) {
    if SHELL_BUSY.swap(true, Ordering::Relaxed) {
        return; // one at a time
    }
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        crate::dark_menus(); // Windows' menus follow this for the whole program, once asked
        run_menu(&paths);
        SHELL_BUSY.store(false, Ordering::Relaxed); // in case it failed before the menu appeared
        crate::send(crate::Command::Retidy);
    });
}

/// Carry out a menu command without showing the menu, as the keyboard shortcuts do.
pub fn verb(paths: Vec<PathBuf>, name: &'static str, shift: bool) {
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        let window = hidden_window();
        if !window.is_null() {
            if let Some(menu) = ui_object::<IContextMenu>(&paths, window) {
                invoke_verb(&menu, window, name, shift);
            }
            DestroyWindow(window);
        }
        crate::send(crate::Command::Retidy);
    });
}

/// Paste whatever was copied onto the desktop.
pub fn paste() {
    std::thread::spawn(|| unsafe {
        let _ = OleInitialize(None);
        let window = hidden_window();
        if !window.is_null() {
            if let Ok(desktop) = SHGetDesktopFolder() {
                if let Ok(menu) = desktop.CreateViewObject::<_, IContextMenu>(ComHwnd(window as _)) {
                    invoke_verb(&menu, window, "paste", false);
                }
            }
            DestroyWindow(window);
        }
        crate::send(crate::Command::Retidy);
    });
}

/// Rename a file, with the box at this spot.
pub fn rename(path: PathBuf, at: POINT) {
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        rename_box(&path, at);
        crate::send(crate::Command::Retidy);
    });
}

unsafe fn invoke_verb(menu: &IContextMenu, window: HWND, name: &str, shift: bool) {
    const SHIFT_DOWN: u32 = 0x1000_0000;
    let popup = CreatePopupMenu();
    if menu.QueryContextMenu(ComMenu(popup as _), 0, FIRST_ID, 0x7FFF, CMF_NORMAL).is_ok() {
        let verb: Vec<u8> = name.bytes().chain([0]).collect();
        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: if shift { SHIFT_DOWN } else { 0 },
            hwnd: ComHwnd(window as _),
            lpVerb: PCSTR(verb.as_ptr()),
            nShow: SW_SHOWNORMAL,
            ..Default::default()
        };
        let _ = menu.InvokeCommand(&info);
        // what was copied has to outlive this thread
        let _ = OleFlushClipboard();
    }
    DestroyMenu(popup);
}

/// Windows can only act on several items that sit in one folder, so keep those in the first one's.
pub fn in_one_folder(paths: &[PathBuf]) -> Vec<PathBuf> {
    let Some(first) = paths.first() else { return vec![] };
    paths.iter().filter(|p| p.parent() == first.parent()).cloned().collect()
}

/// Something Windows offers for these items together: a menu, or data for dragging.
unsafe fn ui_object<T: Interface>(paths: &[PathBuf], window: HWND) -> Option<T> {
    let kept = in_one_folder(paths);
    let items: Vec<*mut ITEMIDLIST> = kept.iter().filter_map(|p| parse(p)).collect();
    let mut result = None;
    let mut children: Vec<*const ITEMIDLIST> = vec![];
    let mut parent = None;
    for &item in &items {
        let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
        if let Ok(folder) = SHBindToParent::<IShellFolder>(item, Some(&mut last)) {
            children.push(last as *const ITEMIDLIST);
            parent.get_or_insert(folder);
        }
    }
    if let Some(parent) = parent {
        result = parent.GetUIObjectOf::<_, T>(ComHwnd(window as _), &children, None).ok();
    }
    for item in items {
        CoTaskMemFree(Some(item as *const _));
    }
    result
}

unsafe fn hidden_window() -> HWND {
    let class = wide("RivuletFileMenu");
    let instance = GetModuleHandleW(std::ptr::null());
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(menu_proc);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    RegisterClassW(&wc); // already registered after the first one, which is fine
    CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null())
}

unsafe fn run_menu(paths: &[PathBuf]) -> Option<()> {
    let hwnd = hidden_window();
    if hwnd.is_null() {
        return None;
    }
    let menu = ui_object::<IContextMenu>(paths, hwnd);
    let chosen = menu.and_then(|menu| use_menu(paths.first()?, hwnd, &menu));
    MENUS.with(|m| *m.borrow_mut() = (None, None));
    DestroyWindow(hwnd);
    chosen
}

unsafe fn use_menu(path: &Path, hwnd: HWND, menu: &IContextMenu) -> Option<()> {
    let popup = CreatePopupMenu();
    menu.QueryContextMenu(ComMenu(popup as _), 0, FIRST_ID, 0x7FFF, CMF_NORMAL | CMF_CANRENAME).ok()?;
    MENUS.with(|m| *m.borrow_mut() = (menu.cast().ok(), menu.cast().ok()));
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    to_front(hwnd); // otherwise the menu will not close when clicking away
    let chosen = TrackPopupMenu(popup, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null()) as u32;
    PostMessageW(hwnd, WM_NULL, 0, 0);
    SHELL_BUSY.store(false, Ordering::Relaxed);
    if chosen >= FIRST_ID {
        let offset = (chosen - FIRST_ID) as usize;
        let mut verb = [0u16; 64];
        let named = menu.GetCommandString(offset, GCS_VERBW, None, PSTR(verb.as_mut_ptr() as *mut u8), 64).is_ok();
        let n = verb.iter().position(|c| *c == 0).unwrap_or(0);
        if named && String::from_utf16_lossy(&verb[..n]).eq_ignore_ascii_case("rename") {
            rename_box(path, pt);
        } else {
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                hwnd: ComHwnd(hwnd as _),
                lpVerb: PCSTR(offset as *const u8), // the menu's way of saying "this item, by number"
                nShow: SW_SHOWNORMAL,
                ..Default::default()
            };
            let _ = menu.InvokeCommand(&info);
            let _ = OleFlushClipboard(); // what was copied has to outlive this thread
        }
    }
    DestroyMenu(popup);
    Some(())
}

// ---------- dragging to other programs

static RELEASED: AtomicBool = AtomicBool::new(false);
static CAUGHT: AtomicBool = AtomicBool::new(false);

/// The mouse hook saw the real button come up while a drag was being set up.
pub fn note_release() {
    RELEASED.store(true, Ordering::Relaxed);
}

unsafe extern "system" fn catcher_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_LBUTTONDOWN {
        CAUGHT.store(true, Ordering::Relaxed);
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Hand a drag over to Windows, so these files can land in any program. The press itself was held
/// back from Windows to keep it off the desktop, and a Windows drag needs the button to be down, so
/// a tiny window under the mouse takes one pressed again in its place.
pub fn begin_drag(paths: Vec<PathBuf>) {
    if SHELL_BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    RELEASED.store(false, Ordering::Relaxed);
    CAUGHT.store(false, Ordering::Relaxed);
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        run_drag(&paths);
        SHELL_BUSY.store(false, Ordering::Relaxed);
        crate::send(crate::Command::Retidy);
    });
}

unsafe fn run_drag(paths: &[PathBuf]) {
    let class = wide("RivuletDragCatcher");
    let instance = GetModuleHandleW(std::ptr::null());
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(catcher_proc);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    RegisterClassW(&wc);
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    const HALF: i32 = 12;
    const LAYERED_ALPHA: u32 = 2;
    let catcher = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class.as_ptr(), class.as_ptr(), WS_POPUP, pt.x - HALF, pt.y - HALF, HALF * 2, HALF * 2, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
    if catcher.is_null() {
        return;
    }
    SetLayeredWindowAttributes(catcher, 0, 1, LAYERED_ALPHA); // all but invisible, and it still takes clicks
    ShowWindow(catcher, SW_SHOWNOACTIVATE);
    if let Some(data) = ui_object::<IDataObject>(paths, catcher) {
        if !RELEASED.load(Ordering::Relaxed) {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
            let begun = std::time::Instant::now();
            let mut msg = std::mem::zeroed();
            while !CAUGHT.load(Ordering::Relaxed) && begun.elapsed().as_millis() < 500 {
                if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            if CAUGHT.load(Ordering::Relaxed) {
                let _ = SHDoDragDrop(ComHwnd(catcher as _), &data, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
            } else {
                mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0); // never leave the button stuck down
            }
        }
    }
    DestroyWindow(catcher);
}

/// Let files go onto a folder, the Recycle Bin or an app, as if dropped there in Explorer: Windows'
/// own drop handler for that item decides (move, delete, open with) and asks about any clash.
pub fn drop_on(paths: Vec<PathBuf>, target: PathBuf) {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Ole::{IDropTarget, DROPEFFECT, DROPEFFECT_NONE};
    use windows::Win32::UI::Shell::IDataObjectAsyncCapability;
    use windows::Win32::System::SystemServices::{MK_CONTROL, MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
    if SHELL_BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        let hwnd = hidden_window();
        let mut keys = MK_LBUTTON.0;
        if GetAsyncKeyState(VK_CONTROL as i32) < 0 {
            keys |= MK_CONTROL.0; // copy, as in Explorer
        }
        if GetAsyncKeyState(VK_SHIFT as i32) < 0 {
            keys |= MK_SHIFT.0; // move, as in Explorer
        }
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        let pt = POINTL { x: at.x, y: at.y };
        let data = ui_object::<IDataObject>(&paths, hwnd);
        if let (Some(data), Some(target)) = (&data, ui_object::<IDropTarget>(&[target], hwnd)) {
            let all = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;
            let mut effect: DROPEFFECT = all;
            let entered = target.DragEnter(data, MODIFIERKEYS_FLAGS(keys), pt, &mut effect).is_ok();
            effect = all;
            if entered && target.DragOver(MODIFIERKEYS_FLAGS(keys), pt, &mut effect).is_ok() && effect != DROPEFFECT_NONE {
                // the button is up by now, as it is when Explorer drops
                let _ = target.Drop(data, MODIFIERKEYS_FLAGS(keys & !MK_LBUTTON.0), pt, &mut effect);
            } else if entered {
                let _ = target.DragLeave();
            }
        }
        SHELL_BUSY.store(false, Ordering::Relaxed);
        // a move or copy may carry on in the background, with its progress and "replace or skip"
        // boxes owned by this window: keep it until Windows says the work is done
        // the dropped files live in this thread, so it stays while Windows' worker still holds them;
        // the async flag alone is not enough, as Explorer's file data does not always report it
        let held_elsewhere = |d: &IDataObject| {
            let raw = d.as_raw();
            let table = *(raw as *const *const windows::core::IUnknown_Vtbl);
            ((*table).AddRef)(raw);
            ((*table).Release)(raw) > 1
        };
        let working = || data.as_ref().is_some_and(|d| held_elsewhere(d) || d.cast::<IDataObjectAsyncCapability>().ok().and_then(|a| a.InOperation().ok()).is_some_and(|b| b.as_bool()));
        let began = std::time::Instant::now();
        let mut msg = std::mem::zeroed();
        while began.elapsed().as_secs() < 2 || working() {
            if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        DestroyWindow(hwnd);
        crate::send(crate::Command::Retidy);
    });
}

/// A file name Windows would accept.
pub fn valid_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && !name.ends_with('.') && !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
}

/// Where the part of a name before its extension ends, in UTF-16 units, to preselect just that.
fn stem_length(name: &str) -> usize {
    match name.rfind('.') {
        Some(at) if at > 0 => name[..at].encode_utf16().count(),
        _ => name.encode_utf16().count(),
    }
}

/// A small box asking for a line of text, at this spot. None if it was closed without Enter.
unsafe fn text_box(prompt: &str, initial: &str, selected: usize, at: POINT) -> Option<String> {
    let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForSystem() as f32 / 96.0;
    let px = |v: f32| (v * dpi) as i32;
    let frame_class = wide("RivuletAsk");
    let instance = GetModuleHandleW(std::ptr::null());
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(DefWindowProcW);
    wc.hInstance = instance;
    wc.lpszClassName = frame_class.as_ptr();
    wc.hbrBackground = (5 + 1) as _; // the window colour
    RegisterClassW(&wc);
    let title = wide(prompt);
    let frame = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, frame_class.as_ptr(), title.as_ptr(), WS_POPUP | WS_CAPTION | WS_SYSMENU, at.x, at.y, px(420.0), px(66.0), std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
    if frame.is_null() {
        return None;
    }
    let (edit_class, text) = (wide("EDIT"), wide(initial));
    const ES_AUTOHSCROLL_STYLE: u32 = 0x0080;
    let edit = CreateWindowExW(0, edit_class.as_ptr(), text.as_ptr(), WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL_STYLE, px(4.0), px(4.0), px(404.0), px(26.0), frame, std::ptr::null_mut(), instance, std::ptr::null());
    let face = wide("Segoe UI");
    let font = CreateFontW(-((12.0 * dpi).round() as i32), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    SendMessageW(edit, WM_SETFONT, if font.is_null() { GetStockObject(DEFAULT_GUI_FONT) as usize } else { font as usize }, 1);
    ShowWindow(frame, SW_SHOW);
    to_front(frame);
    SetFocus(edit);
    SendMessageW(edit, EM_SETSEL_MSG, 0, selected as isize);
    SetTimer(frame, 1, 150, None); // wakes the loop below, to notice a click elsewhere
    let mut commit = false;
    let mut msg = std::mem::zeroed();
    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
        if msg.message == WM_KEYDOWN && msg.wParam == VK_RETURN as usize {
            commit = true;
            break;
        }
        if msg.message == WM_KEYDOWN && msg.wParam == VK_ESCAPE as usize {
            break;
        }
        if GetForegroundWindow() != frame || IsWindow(frame) == 0 {
            break;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let mut answer = None;
    if commit {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(edit, buf.as_mut_ptr(), 512).max(0) as usize;
        answer = Some(String::from_utf16_lossy(&buf[..n]).trim().to_string());
    }
    DestroyWindow(frame);
    if !font.is_null() {
        DeleteObject(font as _);
    }
    answer
}

/// Ask for some text on a thread of its own; the answer goes to the worker.
pub fn ask(question: crate::Ask, prompt: &'static str, initial: String) {
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        if let Some(answer) = text_box(prompt, &initial, initial.encode_utf16().count(), at) {
            crate::send(crate::Command::Answered(question, answer));
        }
    });
}

/// Windows' colour chooser; the choice goes to the worker as red-green-blue hex.
pub fn pick_colour(key: String, start: u32) {
    use windows_sys::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
    std::thread::spawn(move || unsafe {
        let window = hidden_window();
        to_front(window);
        let mut custom = [0u32; 16];
        let mut choice: CHOOSECOLORW = std::mem::zeroed();
        choice.lStructSize = std::mem::size_of::<CHOOSECOLORW>() as u32;
        choice.hwndOwner = window;
        choice.rgbResult = start;
        choice.lpCustColors = custom.as_mut_ptr();
        choice.Flags = CC_FULLOPEN | CC_RGBINIT;
        if ChooseColorW(&mut choice) != 0 {
            let c = choice.rgbResult;
            let hex = format!("{:02X}{:02X}{:02X}", c & 255, (c >> 8) & 255, (c >> 16) & 255);
            crate::send(crate::Command::ColourChosen(key, hex));
        }
        DestroyWindow(window);
    });
}

/// Windows' folder chooser; the choice goes to the worker.
pub fn pick_folder() {
    use windows_sys::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BROWSEINFOW};
    std::thread::spawn(|| unsafe {
        let _ = OleInitialize(None);
        let window = hidden_window();
        to_front(window);
        let title = wide("Choose a folder to show");
        let mut info: BROWSEINFOW = std::mem::zeroed();
        info.hwndOwner = window;
        info.lpszTitle = title.as_ptr();
        const NEW_DIALOG: u32 = 0x0040;
        const ONLY_FOLDERS: u32 = 0x0001;
        info.ulFlags = NEW_DIALOG | ONLY_FOLDERS;
        let item = SHBrowseForFolderW(&info);
        if !item.is_null() {
            let mut buf = [0u16; 1024];
            if SHGetPathFromIDListW(item, buf.as_mut_ptr()) != 0 {
                let n = buf.iter().position(|c| *c == 0).unwrap_or(0);
                crate::send(crate::Command::FolderChosen(String::from_utf16_lossy(&buf[..n])));
            }
            CoTaskMemFree(Some(item as *const _));
        }
        DestroyWindow(window);
    });
}

unsafe fn rename_box(path: &Path, at: POINT) {
    let Some(old) = path.file_name().map(|n| n.to_string_lossy().to_string()) else { return };
    if path.to_string_lossy().starts_with("shell:") {
        return;
    }
    let Some(new) = text_box("Rename", &old, stem_length(&old), at) else { return };
    if new == old {
        return;
    }
    if !valid_name(&new) {
        crate::alert("That name is not allowed. A file name cannot be empty, end in a dot, or contain \\ / : * ? \" < > |");
    } else if let Some(parent) = path.parent() {
        let target = parent.join(&new);
        let same_file = target.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy());
        if target.exists() && !same_file {
            crate::alert("Something with that name already exists.");
        } else if let Err(e) = std::fs::rename(path, &target) {
            crate::alert(&format!("Could not rename: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // needs a real desktop
    fn the_recycle_bin_picture_comes_from_where_windows_points() {
        let place = recycle_bin_icon_place().expect("Windows names the bin's picture");
        assert!(place.contains(','), "{place}");
        assert_ne!(unsafe { recycle_bin_icon(48) }, 0);
    }

    #[test]
    #[ignore] // needs a real desktop
    fn the_recycle_bin_is_found_among_the_desktop_items() {
        unsafe {
            let _ = OleInitialize(None);
            let items = desktop_items();
            println!("{} items", items.len());
            let special: Vec<_> = items.iter().filter(|(_, p)| p.to_string_lossy().starts_with("shell:")).collect();
            println!("{special:?}");
            assert!(!special.is_empty());
            for (_, p) in special {
                assert_ne!(sharp_icon(p, 48), 0, "{p:?}");
            }
        }
    }

    #[test]
    fn pictures_become_square_icons() {
        // a wide, half-see-through picture: colours are un-mixed and it sits in the middle
        let out = square_pixels(&[50, 50, 50, 128, 100, 100, 100, 255], 2, 1, 4);
        let row = 4 * 4;
        assert_eq!(&out[row + 4..row + 8], &[99, 99, 99, 128]);
        assert_eq!(&out[row + 8..row + 12], &[100, 100, 100, 255]);
        assert_eq!(out[3], 0); // the corner stays clear
        // a photo thumbnail with no transparency at all must not vanish
        let photo = square_pixels(&[10, 20, 30, 0], 1, 1, 1);
        assert_eq!(photo, vec![10, 20, 30, 255]);
    }

    #[test]
    fn only_files_from_one_folder_go_together() {
        let (a, b, c) = (PathBuf::from("C:\\d\\a"), PathBuf::from("C:\\d\\b"), PathBuf::from("C:\\p\\c"));
        assert_eq!(in_one_folder(&[a.clone(), c.clone(), b.clone()]), vec![a, b]);
        assert_eq!(in_one_folder(&[]), Vec::<PathBuf>::new());
    }

    #[test]
    fn names_windows_would_refuse_are_refused() {
        assert!(valid_name("report v2.docx"));
        for bad in ["", "   ", "a/b", "a\\b", "a:b", "a*", "a?", "\"a\"", "<a>", "a|b", "ends."] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn rename_preselects_the_name_without_its_extension() {
        assert_eq!(stem_length("report.docx"), 6);
        assert_eq!(stem_length("no extension"), 12);
        assert_eq!(stem_length(".gitignore"), 10);
        assert_eq!(stem_length("a.b.txt"), 3);
    }
}
