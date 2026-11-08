//! Sticky notes on the desktop, like notes on a fridge. Each is a coloured slip sitting with the
//! sections behind the icons, so Show desktop keeps them. Typing goes into an ordinary edit box
//! laid exactly over the note; when it closes, the note is drawn again with the new words.
//! Everything here runs on the main thread: the mouse hook only notes the press and posts.

use crate::wallpaper::wide;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const NOTE_CLASS: &str = "RivuletNote";
const EDIT_CLASS: &str = "RivuletNoteEdit";
/// Posted to the app's message window: wparam is what to do, lparam the note (or edit box).
pub const WM_NOTE: u32 = WM_APP + 20;
const EDIT: usize = 0;
const DELETE: usize = 1;
const MENU: usize = 2;
const FINISH: usize = 3;
const QUIT_TYPING: usize = 4;
const MOVED: usize = 5;
const REFRESH: usize = 6;
const REDRAW: usize = 7;

/// Fridge colours, as Windows writes them (blue, green, red).
const COLOURS: [(&str, u32); 5] = [("Yellow", 0x009DF5FF), ("Pink", 0x00D0BBF8), ("Green", 0x00A5E1C5), ("Blue", 0x00FCE5B3), ("Orange", 0x0080CCFF)];
/// Text sizes, in points.
const SIZES: [(&str, u8); 4] = [("Small text", 14), ("Medium text", 18), ("Large text", 24), ("Huge text", 32)];
const MEDIUM: u8 = 18;
const INK: u32 = 0x00303030;
const EM_SETSEL: u32 = 0xB1;
const EM_SETMARGINS: u32 = 0xD3;
const EM_GETSEL: u32 = 0xB0;
const EM_POSFROMCHAR: u32 = 0xD6;

struct Note {
    window: usize,
    /// Top left on screen.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    colour: u32,
    /// Text size in points.
    size: u8,
    /// The size the owner dragged it to, in pixels; 0 lets it fit its words. It still grows
    /// taller than this when the words need it.
    want_w: i32,
    want_h: i32,
    text: String,
    /// Stepped aside for an open section that would otherwise be drawn underneath it.
    hidden: bool,
    /// Drawn as liquid glass by the sections' graphics engine; its own window then stays hidden.
    liquid: bool,
}

/// In stacking order: the last one is on top.
static NOTES: Mutex<Vec<Note>> = Mutex::new(Vec::new());
static OWNER: AtomicUsize = AtomicUsize::new(0);
/// The note being dragged, where in it the mouse took hold, and how: MOVE, or the edges being
/// pulled (RIGHT_EDGE, BOTTOM_EDGE or both), in which case the grip is measured from those edges.
static DRAG: Mutex<Option<(usize, i32, i32, u8)>> = Mutex::new(None);
const MOVE: u8 = 0;
const RIGHT_EDGE: u8 = 1;
const BOTTOM_EDGE: u8 = 2;
static PRESSED: AtomicBool = AtomicBool::new(false);
static RIGHT: AtomicUsize = AtomicUsize::new(0);
/// The open edit box: (its window, the text box inside, the note, the brush behind the text).
static EDITING: Mutex<Option<(usize, usize, usize, usize)>> = Mutex::new(None);
static FONTS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
/// One redraw waiting for a dragged glass note, however many moves came in meanwhile.
static MOVE_WAITING: AtomicBool = AtomicBool::new(false);
static REFRESH_WAITING: AtomicBool = AtomicBool::new(false);

fn px(v: f32) -> i32 {
    (v * unsafe { GetDpiForSystem() } as f32 / 96.0).round() as i32
}
fn header() -> i32 {
    px(28.0)
}
fn pad() -> i32 {
    px(12.0)
}
/// How close to an edge the mouse must be to pull it.
fn grip() -> i32 {
    px(8.0)
}

/// Which edges a press at (gx, gy) inside a note pulls: near the right edge (below the top strip),
/// near the bottom, or on the grip drawn in the bottom-right corner, both.
fn edges_at(w: i32, h: i32, gx: i32, gy: i32) -> u8 {
    let corner = px(24.0);
    if gx >= w - corner && gy >= h - corner {
        return RIGHT_EDGE | BOTTOM_EDGE;
    }
    (if gx >= w - grip() && gy >= header() { RIGHT_EDGE } else { 0 }) | if gy >= h - grip() { BOTTOM_EDGE } else { 0 }
}

unsafe fn font(size: u8) -> HFONT {
    let i = SIZES.iter().position(|s| s.1 == size).unwrap_or(1);
    if FONTS[i].load(Ordering::Relaxed) == 0 {
        let f = CreateFontW(-px(SIZES[i].1 as f32), 0, 0, 0, 400, 0, 0, 0, 0, 0, 0, CLEARTYPE_QUALITY as u32, 0, wide("Segoe Print").as_ptr());
        FONTS[i].store(f as usize, Ordering::Relaxed);
    }
    FONTS[i].load(Ordering::Relaxed) as HFONT
}

fn notes_file() -> std::path::PathBuf {
    crate::state_dir().join("notes.txt")
}

/// One note per line, so tabs, line breaks and backslashes in the words are written as \t \n \\.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\t', "\\t").replace("\r\n", "\\n").replace('\n', "\\n")
}

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push_str("\r\n"),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The colour field may carry the text size and the dragged width and height after slashes;
/// notes saved before these are medium and fit their words.
fn parse(saved: &str) -> Vec<(i32, i32, u32, u8, i32, i32, String)> {
    saved
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '\t');
            let (x, y, look) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?);
            let mut look = look.split('/');
            let colour = u32::from_str_radix(look.next()?, 16).ok()?;
            let size = look.next().and_then(|s| s.parse().ok()).filter(|s| SIZES.iter().any(|z| z.1 == *s)).unwrap_or(MEDIUM);
            let mut number = || look.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(0).max(0);
            let (w, h) = (number(), number());
            Some((x, y, colour, size, w, h, unescape(f.next()?)))
        })
        .collect()
}

fn save() {
    let Ok(notes) = NOTES.try_lock() else { return };
    let lines: Vec<String> = notes.iter().map(|n| format!("{}\t{}\t{:06X}/{}/{}/{}\t{}", n.x, n.y, n.colour, n.size, n.want_w, n.want_h, escape(&n.text))).collect();
    let _ = std::fs::write(notes_file(), lines.join("\n"));
}

/// Start: the window classes, and the notes saved last time.
pub unsafe fn start(owner: HWND) {
    OWNER.store(owner as usize, Ordering::Relaxed);
    let instance = GetModuleHandleW(null());
    let (note, edit) = (wide(NOTE_CLASS), wide(EDIT_CLASS));
    let arrow = LoadCursorW(null_mut(), IDC_ARROW);
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(note_proc), hInstance: instance, lpszClassName: note.as_ptr(), hCursor: arrow, ..std::mem::zeroed() });
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(edit_proc), hInstance: instance, lpszClassName: edit.as_ptr(), hCursor: arrow, ..std::mem::zeroed() });
    if let Ok(mut notes) = NOTES.lock() {
        let saved = std::fs::read_to_string(notes_file()).unwrap_or_default();
        *notes = parse(&saved).into_iter().filter(|(.., text)| !text.trim().is_empty()).map(|(x, y, colour, size, want_w, want_h, text)| Note { window: 0, x, y, w: 0, h: 0, colour, size, want_w, want_h, text, hidden: false, liquid: false }).collect();
    }
    remake();
}

/// The glass came on or went off, or the wallpaper changed: draw every note again.
pub fn redraw_soon() {
    post(REDRAW, 0);
}

/// On the way out: words still being typed are kept.
pub unsafe fn stop() {
    finish(0);
}

/// Windows for every note; after Explorer restarts, the old ones went with the desktop.
pub unsafe fn remake() {
    let Ok(mut notes) = NOTES.lock() else { return };
    for n in notes.iter_mut() {
        n.window = make_window();
        place(n);
    }
}

unsafe fn make_window() -> usize {
    let Some((parent, _)) = crate::layers::parent_view() else { return 0 };
    let class = wide(NOTE_CLASS);
    let h = CreateWindowExW(WS_EX_LAYERED | WS_EX_NOACTIVATE, class.as_ptr(), class.as_ptr(), WS_CHILD, 0, 0, 1, 1, parent, null_mut(), GetModuleHandleW(null()), null());
    if !h.is_null() {
        SetLayeredWindowAttributes(h, 0, 250, 2);
    }
    h as usize
}

unsafe fn text_height(text: &str, width: i32, size: u8) -> i32 {
    let dc = GetDC(null_mut());
    let old = SelectObject(dc, font(size) as _);
    let mut r = RECT { left: 0, top: 0, right: width, bottom: 0 };
    let mut t = wide(if text.is_empty() { " " } else { text });
    DrawTextW(dc, t.as_mut_ptr(), -1, &mut r, DT_CALCRECT | DT_WORDBREAK | DT_EDITCONTROL | DT_NOPREFIX);
    SelectObject(dc, old);
    ReleaseDC(null_mut(), dc);
    r.bottom
}

/// Size the note to its words and put it on the desktop, on top of the other notes.
unsafe fn place(n: &mut Note) {
    n.w = if n.want_w > 0 { n.want_w.max(px(140.0)) } else { px(230.0 * n.size.max(MEDIUM) as f32 / MEDIUM as f32) }; // bigger letters, a wider note
    n.h = (header() + px(6.0) + text_height(&n.text, n.w - 2 * pad(), n.size).max(px(40.0)) + px(14.0)).max(if n.want_h > 0 { n.want_h } else { header() + px(104.0) });
    if n.window == 0 {
        return;
    }
    let words = (pad(), header() + px(6.0), n.w - pad());
    if crate::pretty::note(n.window, n.x, n.y, n.w, n.h, n.colour, &n.text, n.size, words, header(), caret_for(n.window, n.size)) {
        ShowWindow(n.window as HWND, SW_HIDE);
        n.liquid = true;
        return;
    }
    if n.liquid {
        crate::pretty::forget_note(n.window); // the glass went off: back to a plain slip
        n.liquid = false;
    }
    let (x, y) = crate::layers::to_parent(n.x, n.y);
    let behind = crate::layers::parent_view().and_then(|(_, view)| view).unwrap_or(HWND_TOP);
    SetWindowPos(n.window as HWND, behind, x, y, n.w, n.h, SWP_NOACTIVATE | if n.hidden { 0 } else { SWP_SHOWWINDOW });
    SetWindowRgn(n.window as HWND, CreateRoundRectRgn(0, 0, n.w + 1, n.h + 1, px(10.0), px(10.0)), 1);
    InvalidateRect(n.window as HWND, null(), 0);
}

unsafe fn measure(text: &[u16], size: u8) -> (i32, i32) {
    let dc = GetDC(null_mut());
    let old = SelectObject(dc, font(size) as _);
    let mut size = SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size);
    let mut metrics: TEXTMETRICW = std::mem::zeroed();
    GetTextMetricsW(dc, &mut metrics);
    SelectObject(dc, old);
    ReleaseDC(null_mut(), dc);
    (size.cx, metrics.tmHeight)
}

/// Where the caret is in the note being typed in, from the words' top left: x, y and height. The
/// glass note draws it, since the text box itself is invisible over liquid glass.
unsafe fn caret_for(window: usize, size: u8) -> Option<(i32, i32, i32)> {
    let edit = EDITING.try_lock().ok().and_then(|e| *e).filter(|e| e.2 == window)?.1 as HWND;
    let (mut start, mut end) = (0u32, 0u32);
    SendMessageW(edit, EM_GETSEL, &mut start as *mut u32 as WPARAM, &mut end as *mut u32 as LPARAM);
    let text: Vec<u16> = window_text(edit).encode_utf16().collect();
    let (_, line) = measure(&[], size);
    let at = |i: usize| -> Option<(i32, i32)> {
        let r = SendMessageW(edit, EM_POSFROMCHAR, i, 0);
        (r != -1).then(|| ((r & 0xFFFF) as i16 as i32, ((r >> 16) & 0xFFFF) as i16 as i32))
    };
    let i = end as usize;
    let (x, y) = if i < text.len() {
        at(i)?
    } else if let Some(&last) = text.last() {
        // after the last letter there is no letter to ask about: go from the last one
        let (lx, ly) = at(text.len() - 1)?;
        if last == '\n' as u16 { (0, ly + line) } else { (lx + measure(&[last], size).0, ly) }
    } else {
        (0, 0)
    };
    Some((x, y, line))
}

fn darker(colour: u32) -> u32 {
    let ch = |shift: u32| (((colour >> shift) & 0xFF) * 86 / 100) << shift;
    ch(0) | ch(8) | ch(16)
}

unsafe fn fill(dc: HDC, r: &RECT, colour: u32) {
    let brush = CreateSolidBrush(colour);
    FillRect(dc, r, brush);
    DeleteObject(brush as _);
}

unsafe extern "system" fn note_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            if let Some((w, h, colour, size, text)) = NOTES.try_lock().ok().and_then(|notes| notes.iter().find(|n| n.window == hwnd as usize).map(|n| (n.w, n.h, n.colour, n.size, n.text.clone()))) {
                fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, colour);
                fill(dc, &RECT { left: 0, top: 0, right: w, bottom: header() }, darker(colour));
                SetBkMode(dc, TRANSPARENT as _);
                SetTextColor(dc, INK);
                let old = SelectObject(dc, font(MEDIUM) as _);
                let mut close = RECT { left: w - header(), top: 0, right: w, bottom: header() };
                DrawTextW(dc, wide("\u{00D7}").as_mut_ptr(), -1, &mut close, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                // the grip: three short slanted lines in the bottom-right corner
                let pen = CreatePen(PS_SOLID as _, px(1.0).max(1), darker(darker(colour)));
                let old_pen = SelectObject(dc, pen as _);
                for k in 1..=3 {
                    let d = px(4.0 * k as f32);
                    MoveToEx(dc, w - px(6.0) - d, h - px(6.0), null_mut());
                    LineTo(dc, w - px(6.0), h - px(6.0) - d);
                }
                SelectObject(dc, old_pen);
                DeleteObject(pen as _);
                SelectObject(dc, font(size) as _);
                let mut words = RECT { left: pad(), top: header() + px(6.0), right: w - pad(), bottom: h };
                DrawTextW(dc, wide(&text).as_mut_ptr(), -1, &mut words, DT_WORDBREAK | DT_EDITCONTROL | DT_NOPREFIX);
                SelectObject(dc, old);
            }
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn note_at(notes: &[Note], pt: POINT) -> Option<usize> {
    notes.iter().rposition(|n| n.window != 0 && !n.hidden && pt.x >= n.x && pt.x < n.x + n.w && pt.y >= n.y && pt.y < n.y + n.h)
}

fn post(what: usize, window: usize) {
    unsafe { PostMessageW(OWNER.load(Ordering::Relaxed) as HWND, WM_NOTE, what, window as LPARAM) };
}

/// From the mouse hook: a press on a note. The top strip drags it, the cross deletes it, the
/// rest opens it for typing. True when the press was on a note, so the desktop must not see it.
pub unsafe fn press(pt: POINT) -> bool {
    let Ok(mut notes) = NOTES.try_lock() else { return false };
    let Some(i) = note_at(&notes, pt) else { return false };
    let n = notes.remove(i);
    let (gx, gy) = (pt.x - n.x, pt.y - n.y);
    if !n.liquid {
        let behind = crate::layers::parent_view().and_then(|(_, view)| view).unwrap_or(HWND_TOP);
        SetWindowPos(n.window as HWND, behind, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE); // to the top of the pile
    }
    let edges = edges_at(n.w, n.h, gx, gy);
    if edges != 0 {
        // pulling an edge: the grip is how far the mouse is inside those edges
        if let Ok(mut drag) = DRAG.try_lock() {
            *drag = Some((n.window, n.w - gx, n.h - gy, edges));
        }
        post(FINISH, 0); // a text box being typed in would not fit the new size
    } else if gy < header() && gx >= n.w - header() {
        post(DELETE, n.window);
    } else if gy < header() {
        if let Ok(mut drag) = DRAG.try_lock() {
            *drag = Some((n.window, gx, gy, MOVE));
        }
    } else {
        post(EDIT, n.window);
    }
    notes.push(n);
    PRESSED.store(true, Ordering::Relaxed);
    true
}

/// From the sections' thread: the panel opening at `panel` (on screen) is drawn behind the notes,
/// so any note in its way steps aside until it closes (None).
pub fn make_room(panel: Option<RECT>) {
    let Ok(mut notes) = NOTES.lock() else { return };
    for n in notes.iter_mut() {
        let hide = panel.is_some_and(|r| n.x < r.right && n.x + n.w > r.left && n.y < r.bottom && n.y + n.h > r.top);
        if n.liquid {
            continue; // glass notes lie beneath the sections already
        }
        if hide != n.hidden && n.window != 0 {
            n.hidden = hide;
            if let Some((popup, ..)) = EDITING.try_lock().ok().and_then(|e| *e).filter(|e| hide && e.2 == n.window) {
                post(FINISH, popup); // the words are kept; the box must not float over the section
            }
            unsafe { ShowWindowAsync(n.window as HWND, if hide { SW_HIDE } else { SW_SHOWNOACTIVATE }) };
        }
    }
}

/// From the mouse hook: the note being dragged follows the mouse.
pub unsafe fn moved(pt: POINT) {
    let Some((window, gx, gy, how)) = DRAG.try_lock().ok().and_then(|d| *d) else { return };
    let Ok(mut notes) = NOTES.try_lock() else { return };
    if let Some(n) = notes.iter_mut().find(|n| n.window == window) {
        if how != MOVE {
            if how & RIGHT_EDGE != 0 {
                n.want_w = (pt.x + gx - n.x).max(px(140.0));
            }
            if how & BOTTOM_EDGE != 0 {
                n.want_h = (pt.y + gy - n.y).max(header() + px(40.0));
            }
            if n.liquid {
                if !MOVE_WAITING.swap(true, Ordering::Relaxed) {
                    post(MOVED, window);
                }
            } else {
                place(n);
            }
            return;
        }
        (n.x, n.y) = (pt.x - gx, pt.y - gy);
        if n.liquid {
            // glass bends the wallpaper where it is, so it is drawn again, off the mouse's path
            if !MOVE_WAITING.swap(true, Ordering::Relaxed) {
                post(MOVED, window);
            }
        } else {
            let (x, y) = crate::layers::to_parent(n.x, n.y);
            SetWindowPos(window as HWND, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        // a note still being typed in takes its text box along
        if let Some((popup, ..)) = EDITING.try_lock().ok().and_then(|e| *e).filter(|e| e.2 == window) {
            SetWindowPos(popup as HWND, null_mut(), n.x + pad(), n.y + header() + px(6.0), 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// From the mouse hook: true when this release ends a press that began on a note.
pub fn release() -> bool {
    if DRAG.try_lock().is_ok_and(|mut d| d.take().is_some()) {
        save();
    }
    PRESSED.swap(false, Ordering::Relaxed)
}

pub fn right_press(pt: POINT) -> bool {
    let Some(window) = NOTES.try_lock().ok().and_then(|notes| note_at(&notes, pt).map(|i| notes[i].window)) else { return false };
    RIGHT.store(window, Ordering::Relaxed);
    true
}

pub fn right_release() -> bool {
    let window = RIGHT.swap(0, Ordering::Relaxed);
    if window != 0 {
        post(MENU, window);
    }
    window != 0
}

/// A new blank note in the middle of the screen the mouse is on, open for typing.
pub unsafe fn new_note() {
    finish(0);
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut info);
    let work = info.rcWork;
    let Ok(mut notes) = NOTES.lock() else { return };
    let step = px(26.0) * (notes.len() as i32 % 8); // so a second new note does not hide the first
    let mut n = Note { window: make_window(), x: (work.left + work.right) / 2 - px(115.0) + step, y: (work.top + work.bottom) / 2 - px(120.0) + step, w: 0, h: 0, colour: COLOURS[0].1, size: MEDIUM, want_w: 0, want_h: 0, text: String::new(), hidden: false, liquid: false };
    place(&mut n);
    let window = n.window;
    notes.push(n);
    drop(notes);
    edit(window);
}

/// From the app's message window.
pub unsafe fn handle(owner: HWND, what: usize, window: usize) {
    match what {
        EDIT => edit(window),
        DELETE => delete(window),
        MENU => menu(owner, window),
        FINISH => finish(window),
        QUIT_TYPING => {
            finish(window);
            SetForegroundWindow(GetShellWindow());
        }
        MOVED => {
            MOVE_WAITING.store(false, Ordering::Relaxed);
            if let Ok(mut notes) = NOTES.lock() {
                if let Some(n) = notes.iter_mut().find(|n| n.window == window) {
                    place(n);
                }
            }
        }
        REFRESH => {
            REFRESH_WAITING.store(false, Ordering::Relaxed);
            grow(window as HWND);
        }
        REDRAW => {
            if let Ok(mut notes) = NOTES.lock() {
                notes.iter_mut().for_each(|n| place(n));
            }
        }
        _ => {}
    }
}

unsafe fn delete(window: usize) {
    finish(0);
    let has_words = NOTES.lock().is_ok_and(|notes| notes.iter().any(|n| n.window == window && !n.text.trim().is_empty()));
    if has_words && MessageBoxW(null_mut(), wide("Delete this note?").as_ptr(), wide("Rivulet").as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_TOPMOST) != IDYES {
        return;
    }
    if let Ok(mut notes) = NOTES.lock() {
        notes.retain(|n| n.window != window);
    }
    crate::pretty::forget_note(window);
    DestroyWindow(window as HWND);
    save();
}

unsafe fn menu(owner: HWND, window: usize) {
    finish(0);
    let Some((current, size)) = NOTES.lock().ok().and_then(|notes| notes.iter().find(|n| n.window == window).map(|n| (n.colour, n.size))) else { return };
    crate::dark_menus();
    let menu = CreatePopupMenu();
    for (i, (name, colour)) in COLOURS.iter().enumerate() {
        AppendMenuW(menu, if *colour == current { MF_STRING | MF_CHECKED } else { MF_STRING }, i + 1, wide(name).as_ptr());
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    for (i, (name, s)) in SIZES.iter().enumerate() {
        AppendMenuW(menu, if *s == size { MF_STRING | MF_CHECKED } else { MF_STRING }, 11 + i, wide(name).as_ptr());
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, 99, wide("Delete note").as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(owner);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, owner, null()) as usize;
    PostMessageW(owner, WM_NULL, 0, 0);
    DestroyMenu(menu);
    if chosen == 99 {
        delete(window);
    } else if (1..=COLOURS.len()).contains(&chosen) || (11..11 + SIZES.len()).contains(&chosen) {
        if let Ok(mut notes) = NOTES.lock() {
            if let Some(n) = notes.iter_mut().find(|n| n.window == window) {
                if chosen >= 11 {
                    n.size = SIZES[chosen - 11].1;
                } else {
                    n.colour = COLOURS[chosen - 1].1;
                }
                place(n);
            }
        }
        save();
    }
}

/// Lay a text box over the note's words and hand it the keyboard.
unsafe fn edit(window: usize) {
    finish(0);
    let Some((x, y, w, h, colour, size, text, liquid)) = NOTES.lock().ok().and_then(|notes| notes.iter().find(|n| n.window == window).map(|n| (n.x, n.y, n.w, n.h, n.colour, n.size, n.text.clone(), n.liquid))) else { return };
    let instance = GetModuleHandleW(null());
    let top = header() + px(6.0);
    let (bw, bh) = (w - 2 * pad(), h - top - px(6.0));
    let class = wide(EDIT_CLASS);
    // over glass the box is all but invisible: it only takes the keys and clicks, and the glass
    // note shows the words and the caret
    let popup = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | if liquid { WS_EX_LAYERED } else { 0 }, class.as_ptr(), null(), WS_POPUP, x + pad(), y + top, bw, bh, null_mut(), null_mut(), instance, null());
    if liquid {
        SetLayeredWindowAttributes(popup, 0, 1, 2);
    }
    let style = WS_CHILD | WS_VISIBLE | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32;
    let edit = CreateWindowExW(0, wide("EDIT").as_ptr(), wide(&text).as_ptr(), style, 0, 0, bw, bh, popup, null_mut(), instance, null());
    if popup.is_null() || edit.is_null() {
        DestroyWindow(popup);
        return;
    }
    SendMessageW(edit, WM_SETFONT, font(size) as WPARAM, 0);
    SendMessageW(edit, EM_SETMARGINS, (EC_LEFTMARGIN | EC_RIGHTMARGIN) as WPARAM, 0);
    let end = text.encode_utf16().count();
    SendMessageW(edit, EM_SETSEL, end, end as LPARAM);
    SetWindowSubclass(edit, Some(edit_keys), 1, popup as usize);
    if let Ok(mut e) = EDITING.lock() {
        *e = Some((popup as usize, edit as usize, window, CreateSolidBrush(colour) as usize));
    }
    ShowWindow(popup, SW_SHOW);
    crate::shellmenu::to_front(popup);
    SetFocus(edit);
    post(REFRESH, popup as usize); // the caret appears in the glass
}

unsafe fn window_text(window: HWND) -> String {
    let mut buffer = vec![0u16; GetWindowTextLengthW(window) as usize + 1];
    let n = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32);
    String::from_utf16_lossy(&buffer[..n.max(0) as usize])
}

/// Close the text box and keep its words. `popup` 0 closes whichever is open; otherwise only that
/// one, so a late notice from an old box cannot close a newer one.
unsafe fn finish(popup: usize) {
    let Some((open, edit, window, brush)) = EDITING.lock().ok().and_then(|mut e| e.filter(|e| popup == 0 || e.0 == popup).and_then(|_| e.take())) else { return };
    let text = window_text(edit as HWND);
    DestroyWindow(open as HWND);
    DeleteObject(brush as _);
    if let Ok(mut notes) = NOTES.lock() {
        if let Some(i) = notes.iter().position(|n| n.window == window) {
            if text.trim().is_empty() {
                notes.remove(i); // a note left empty is not wanted
                crate::pretty::forget_note(window);
                DestroyWindow(window as HWND);
            } else {
                notes[i].text = text;
                place(&mut notes[i]);
            }
        }
    }
    save();
}

/// The note grows while words are typed, and the box with it.
unsafe fn grow(popup: HWND) {
    let Some((edit, window)) = EDITING.lock().ok().and_then(|e| e.filter(|e| e.0 == popup as usize).map(|e| (e.1, e.2))) else { return };
    let text = window_text(edit as HWND);
    let Ok(mut notes) = NOTES.lock() else { return };
    let Some(n) = notes.iter_mut().find(|n| n.window == window) else { return };
    let before = n.h;
    n.text = text;
    place(n);
    if n.h != before {
        let top = header() + px(6.0);
        let (bw, bh) = (n.w - 2 * pad(), n.h - top - px(6.0));
        SetWindowPos(popup, null_mut(), 0, 0, bw, bh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        SetWindowPos(edit as HWND, null_mut(), 0, 0, bw, bh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CTLCOLOREDIT => {
            let Some((brush, colour)) = EDITING.lock().ok().and_then(|e| *e).and_then(|e| {
                NOTES.try_lock().ok().and_then(|notes| notes.iter().find(|n| n.window == e.2).map(|n| (e.3, n.colour)))
            }) else {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            };
            SetBkColor(wparam as HDC, colour);
            SetTextColor(wparam as HDC, INK);
            brush as LRESULT
        }
        WM_COMMAND if (wparam >> 16) as u32 == EN_CHANGE => {
            grow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Esc, or the keyboard going anywhere else, closes the box and keeps the words.
unsafe extern "system" fn edit_keys(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, popup: usize) -> LRESULT {
    match msg {
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            post(QUIT_TYPING, popup);
            0
        }
        WM_KILLFOCUS => {
            post(FINISH, popup);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // the caret may have moved: the glass note draws it again
        WM_CHAR | WM_KEYDOWN | WM_KEYUP | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEMOVE => {
            let done = DefSubclassProc(hwnd, msg, wparam, lparam);
            if (msg != WM_MOUSEMOVE || wparam & 1 != 0) && !REFRESH_WAITING.swap(true, Ordering::Relaxed) {
                post(REFRESH, popup);
            }
            done
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_survive_saving() {
        let text = "milk\r\neggs\tx2\r\nC:\\temp\\n is not a break";
        assert_eq!(unescape(&escape(text)), text);
        let saved = format!("10\t-20\t9DF5FF\t{}\nbroken line\n5\t6\tD0BBF8/32\thi\n", escape(text));
        assert_eq!(parse(&saved), vec![(10, -20, 0x9DF5FF, 18, 0, 0, text.to_string()), (5, 6, 0xD0BBF8, 32, 0, 0, "hi".to_string())]);
        assert_eq!(parse("1\t2\tA5E1C5/24/400/300\tx"), vec![(1, 2, 0xA5E1C5, 24, 400, 300, "x".to_string())]);
    }

    #[test]
    fn notes_darken_evenly() {
        assert_eq!(darker(0x00646464), 0x00565656);
    }

    #[test]
    fn notes_pull_the_right_edges() {
        let (w, h) = (px(230.0), px(150.0));
        assert_eq!(edges_at(w, h, px(100.0), px(60.0)), 0); // the words
        assert_eq!(edges_at(w, h, w - 2, px(60.0)), RIGHT_EDGE);
        assert_eq!(edges_at(w, h, w - 2, 2), 0); // the cross's corner stays the cross
        assert_eq!(edges_at(w, h, px(100.0), h - 2), BOTTOM_EDGE);
        assert_eq!(edges_at(w, h, w - px(12.0), h - px(12.0)), RIGHT_EDGE | BOTTOM_EDGE);
    }
}
