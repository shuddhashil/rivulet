//! Rivulet: a rail of labels down the left of the desktop. Click one and its section flows open
//! beside the rail; click it again, or click empty desktop, and it closes. Right-click a label for
//! the menu; drag an icon onto a label to file it there. Files are never moved, and the wallpaper
//! is left alone.
//!
//!   rivulet.exe                  start, or re-sort if already running
//!   rivulet.exe --dry-run        show the sections, change nothing
//!   rivulet.exe --doctor         report this PC's setup, for bug reports
//!   rivulet.exe --quit           lay every icon out in view and stop
//!   rivulet.exe --undo           stop, and put the icons back as saved at first start
//!   rivulet.exe --install        add a Start menu shortcut
//!   rivulet.exe --install-login  start right at every login
//!   rivulet.exe --remove-login   stop starting at login
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod layers;
mod pretty;
mod ghost;
mod rules;
mod settings;
mod shellmenu;
mod wallpaper;

use rules::*;
use settings::{Action, Arrow, Look, SETTINGS};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use wallpaper::wide;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmFlush;
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, InvalidateRect, ScreenToClient, HDC, HMONITOR, MONITORINFO};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, RegisterHotKey};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHGetFolderPathW, ShellExecuteW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const LVM_GETITEMCOUNT: u32 = 0x1004;
const LVM_GETNEXTITEM: u32 = 0x100C;
const LVM_SETITEMPOSITION: u32 = 0x100F;
const LVM_GETITEMPOSITION: u32 = 0x1010;
const LVM_HITTEST: u32 = 0x1012;
const LVM_GETITEMSPACING: u32 = 0x1033;
const LVM_SETICONSPACING: u32 = 0x1035;
const LVM_GETITEMTEXTW: u32 = 0x1073;
const LVM_GETEXTENDEDLISTVIEWSTYLE: u32 = 0x1037;
const LVS_EX_SNAPTOGRID: isize = 0x80000;
const LVS_AUTOARRANGE: i32 = 0x100;
const LVNI_SELECTED: isize = 2;
const CLASS: &str = "DesktopSectionsMessages";
const WM_RETIDY: u32 = WM_APP + 1;
const WM_STOP: u32 = WM_APP + 2;
const WM_REMAKE: u32 = WM_APP + 3;
const WM_MENU: u32 = WM_APP + 4;
const WM_TAKE_KEYS: u32 = WM_APP + 5;
const HOTKEY_FIND: usize = 2;
const HOTKEY_NEXT: usize = 3;
const HOTKEY_PREVIOUS: usize = 4;
/// The section that lists what is being typed to find.
const FIND: &str = "Find";
const WM_GIVE_BACK_KEYS: u32 = WM_APP + 6;
const SPI_SETWORKAREA_CHANGE: usize = 47;
const EVENT_OBJECT_CREATE: u32 = 0x8000;
const EVENT_OBJECT_DESTROY: u32 = 0x8001;
const EVENT_OBJECT_REORDER: u32 = 0x8004;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
/// A backstop only: Windows normally tells the program about changes as they happen.
const CHECK_EVERY: Duration = Duration::from_secs(3);
const NO_WINDOW: u32 = 0x0800_0000;

#[repr(C)]
#[derive(Default)]
struct LvItem {
    mask: u32,
    item: i32,
    sub_item: i32,
    state: u32,
    state_mask: u32,
    text: usize,
    text_max: i32,
    image: i32,
    lparam: isize,
    indent: i32,
    group_id: i32,
    columns: u32,
    pu_columns: usize,
    col_fmt: usize,
    group: i32,
}

fn home() -> PathBuf {
    std::env::current_exe().unwrap().parent().unwrap().to_path_buf()
}
fn state_dir() -> PathBuf {
    home().join("state")
}
fn rules_file() -> PathBuf {
    home().join("sections.ini")
}
fn layouts_dir() -> PathBuf {
    state_dir().join("layouts")
}

fn layout_file(name: &str) -> PathBuf {
    layouts_dir().join(format!("{name}.ini"))
}

fn current_layout_file() -> PathBuf {
    state_dir().join("current_layout.txt")
}

fn profiles_dir() -> PathBuf {
    state_dir().join("screen_profiles")
}

/// The saved layouts, by name.
fn layout_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(layouts_dir()).into_iter().flatten().flatten().filter_map(|e| {
        let p = e.path();
        (p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ini"))).then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned())).flatten()
    }).collect();
    names.sort();
    names
}

/// The arrangement of screens, as a name fit for a file.
unsafe fn screens_name() -> String {
    screens().iter().map(|s| format!("{}x{}", s.rcMonitor.right - s.rcMonitor.left, s.rcMonitor.bottom - s.rcMonitor.top)).collect::<Vec<_>>().join("_")
}

fn open_file() -> PathBuf {
    state_dir().join("open.txt")
}
fn snapshot_file() -> PathBuf {
    state_dir().join("positions.tsv")
}
fn seen_file() -> PathBuf {
    state_dir().join("seen.txt")
}
fn spacing_file() -> PathBuf {
    state_dir().join("spacing.txt")
}
fn null() -> HWND {
    std::ptr::null_mut()
}

// ---------- the desktop icon list, which lives inside Explorer ----------

unsafe fn find_listview() -> HWND {
    let search = |parent: HWND| {
        let view = FindWindowExW(parent, null(), wide("SHELLDLL_DefView").as_ptr(), std::ptr::null());
        if view.is_null() {
            null()
        } else {
            FindWindowExW(view, null(), wide("SysListView32").as_ptr(), std::ptr::null())
        }
    };
    let progman = FindWindowW(wide("Progman").as_ptr(), std::ptr::null());
    let mut found = if progman.is_null() { null() } else { search(progman) };
    let mut worker = null();
    while found.is_null() {
        worker = FindWindowExW(null(), worker, wide("WorkerW").as_ptr(), std::ptr::null());
        if worker.is_null() {
            return null();
        }
        found = search(worker);
    }
    found
}

struct Desktop {
    lv: HWND,
    proc: HANDLE,
    mem: *mut c_void,
}

impl Desktop {
    unsafe fn open() -> Result<Desktop, String> {
        let mut lv = null();
        for _ in 0..30 {
            lv = find_listview(); // Explorer may still be starting at login
            if !lv.is_null() && SendMessageW(lv, LVM_GETITEMCOUNT, 0, 0) > 0 {
                break;
            }
            lv = null();
            std::thread::sleep(Duration::from_secs(1));
        }
        if lv.is_null() {
            return Err("Could not find the desktop icons.".into());
        }
        let mut pid = 0;
        GetWindowThreadProcessId(lv, &mut pid);
        let proc = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE, 0, pid);
        if proc.is_null() {
            return Err("Windows refused access to the desktop icons.".into());
        }
        let mem = VirtualAllocEx(proc, std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if mem.is_null() {
            CloseHandle(proc);
            return Err("Windows refused access to the desktop icons.".into());
        }
        Ok(Desktop { lv, proc, mem })
    }

    unsafe fn alive(&self) -> bool {
        IsWindow(self.lv) != 0
    }

    unsafe fn count(&self) -> usize {
        SendMessageW(self.lv, LVM_GETITEMCOUNT, 0, 0).max(0) as usize
    }

    unsafe fn text(&self, i: usize) -> String {
        let text_addr = self.mem as usize + 512;
        let item = LvItem { mask: 1, item: i as i32, text: text_addr, text_max: 1024, ..Default::default() };
        WriteProcessMemory(self.proc, self.mem, &item as *const _ as _, std::mem::size_of::<LvItem>(), std::ptr::null_mut());
        let n = SendMessageW(self.lv, LVM_GETITEMTEXTW, i, self.mem as isize).clamp(0, 1024) as usize;
        let mut buf = vec![0u16; n];
        ReadProcessMemory(self.proc, text_addr as _, buf.as_mut_ptr() as _, n * 2, std::ptr::null_mut());
        String::from_utf16_lossy(&buf)
    }

    /// The icon under this screen point, if any. Asked by the worker, never by the mouse hook.
    unsafe fn icon_under(&self, mut pt: POINT) -> Option<usize> {
        ScreenToClient(self.lv, &mut pt);
        let mut info = [0i32; 6]; // LVHITTESTINFO: x, y, flags, item, subitem, group
        info[0] = pt.x;
        info[1] = pt.y;
        WriteProcessMemory(self.proc, self.mem, info.as_ptr() as _, 24, std::ptr::null_mut());
        let hit = SendMessageW(self.lv, LVM_HITTEST, 0, self.mem as isize);
        (hit >= 0).then_some(hit as usize)
    }

    unsafe fn icon_at(&self, pt: POINT) -> bool {
        self.icon_under(pt).is_some()
    }

    unsafe fn position(&self, i: usize) -> (i32, i32) {
        SendMessageW(self.lv, LVM_GETITEMPOSITION, i, self.mem as isize);
        let mut p = POINT { x: 0, y: 0 };
        ReadProcessMemory(self.proc, self.mem, &mut p as *mut _ as _, 8, std::ptr::null_mut());
        (p.x, p.y)
    }

    unsafe fn place(&self, i: usize, x: i32, y: i32) {
        SendMessageW(self.lv, LVM_SETITEMPOSITION, i, (((y & 0xFFFF) << 16) | (x & 0xFFFF)) as isize);
    }

    unsafe fn spacing(&self) -> (i32, i32) {
        let v = SendMessageW(self.lv, LVM_GETITEMSPACING, 0, 0);
        ((v & 0xFFFF) as i32, ((v >> 16) & 0xFFFF) as i32)
    }

    unsafe fn set_spacing(&self, w: i32, h: i32) {
        SendMessageW(self.lv, LVM_SETICONSPACING, 0, (((h & 0xFFFF) << 16) | (w & 0xFFFF)) as isize);
    }

    /// Reading this is instant, unlike changing it, so startup only pays when it has to.
    unsafe fn grid_is_on(&self) -> bool {
        SendMessageW(self.lv, LVM_GETEXTENDEDLISTVIEWSTYLE, 0, 0) & LVS_EX_SNAPTOGRID != 0
    }

    unsafe fn width(&self) -> i32 {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(self.lv, &mut r);
        r.right
    }

    unsafe fn selected(&self) -> Vec<usize> {
        let (mut out, mut at) = (vec![], -1isize);
        loop {
            at = SendMessageW(self.lv, LVM_GETNEXTITEM, at as usize, LVNI_SELECTED);
            if at < 0 || out.len() > 1000 {
                return out;
            }
            out.push(at as usize);
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            VirtualFreeEx(self.proc, self.mem, 0, MEM_RELEASE);
            CloseHandle(self.proc);
        }
    }
}

/// Untick "Align icons to grid" in the shell's own settings; a refresh resets the icon list's copy.
fn grid_off() -> Result<(), String> {
    let ps = "$h=0;$v=(New-Object -ComObject Shell.Application).Windows()\
              .FindWindowSW(0,[Type]::Missing,8,[ref]$h,1).Document;\
              $v.FolderFlags=$v.FolderFlags -band -bnot 4;$v.FolderFlags";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .creation_flags(NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    match String::from_utf8_lossy(&out.stdout).trim().parse::<i64>() {
        Ok(flags) if flags & 4 == 0 => Ok(()),
        _ => Err("Could not turn off \"Align icons to grid\", so icons would not stay in place.\n\
                  Right-click the desktop > View > untick Align icons to grid, then run this again."
            .into()),
    }
}

// ---------- which file is each icon? ----------

unsafe fn desktop_dirs() -> Vec<PathBuf> {
    let mut out = vec![];
    for csidl in [0x10, 0x19] {
        // this user's Desktop, then the shared one
        let mut buf = [0u16; 260];
        if SHGetFolderPathW(null(), csidl, std::ptr::null_mut(), 0, buf.as_mut_ptr()) == 0 {
            let n = buf.iter().position(|c| *c == 0).unwrap_or(0);
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
        }
    }
    out
}

/// A copied or downloaded file can keep an old modified date, so the later of the two counts.
fn stamp(path: &Path) -> u64 {
    let Ok(meta) = path.metadata() else { return 0 };
    let secs = |t: std::io::Result<std::time::SystemTime>| {
        t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
    };
    secs(meta.modified()).max(secs(meta.created()))
}

/// Desktop items with no file of their own, by the name Windows shows.
fn system_path(label: &str) -> Option<PathBuf> {
    let target = match label.to_lowercase().as_str() {
        "recycle bin" => "shell:RecycleBinFolder",
        "this pc" => "shell:MyComputerFolder",
        "network" => "shell:NetworkPlacesFolder",
        "control panel" => "shell:ControlPanelFolder",
        _ => {
            let profile = home();
            return profile.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(label)).then_some(profile);
        }
    };
    Some(PathBuf::from(target))
}

/// Match each icon's label to its file: (kind, time). Returns also the unclear labels, and each
/// icon's file, where same-named files are shared out in turn so each still opens something real.
unsafe fn describe(labels: &[String]) -> (Vec<(String, u64)>, Vec<String>, Vec<Option<PathBuf>>) {
    let (mut by_name, mut by_stem): (HashMap<String, PathBuf>, HashMap<String, Vec<PathBuf>>) = Default::default();
    for dir in desktop_dirs() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            by_name.insert(entry.file_name().to_string_lossy().to_lowercase(), path.clone());
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
            by_stem.entry(stem).or_default().push(path);
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for label in labels {
        *seen.entry(label.to_lowercase()).or_default() += 1;
    }
    let (mut out, mut unclear, mut paths) = (vec![], vec![], vec![]);
    let mut shared_out: HashSet<PathBuf> = HashSet::new();
    for label in labels {
        let key = label.to_lowercase();
        let stems = by_stem.get(&key);
        let entry = if seen[&key] > 1 {
            None // "a" could be a.docx or a.pdf when extensions are hidden
        } else if let Some(p) = by_name.get(&key) {
            Some(p)
        } else {
            match stems.map(|s| s.as_slice()) {
                Some([p]) => Some(p),
                Some(_) => None,
                None => {
                    out.push(("system".to_string(), 0));
                    paths.push(system_path(label));
                    continue;
                }
            }
        };
        let Some(path) = entry else {
            unclear.push(label.clone());
            out.push(("unknown".to_string(), 0));
            let next = stems.and_then(|s| s.iter().find(|p| !shared_out.contains(*p))).cloned();
            if let Some(p) = &next {
                shared_out.insert(p.clone());
            }
            paths.push(next);
            continue;
        };
        let kind = if path.is_dir() {
            "folder".to_string()
        } else {
            path.extension().map_or(String::new(), |e| format!(".{}", e.to_string_lossy().to_lowercase()))
        };
        out.push((kind, stamp(path)));
        paths.push(Some(path.clone()));
    }
    if paths.iter().any(|p| p.is_none()) {
        let items = shellmenu::desktop_items();
        for (label, path) in labels.iter().zip(paths.iter_mut()) {
            if path.is_none() {
                *path = items.get(&label.to_lowercase()).cloned();
            }
        }
    }
    (out, unclear, paths)
}

// ---------- layout ----------

/// Measurements in real pixels. Rail and panel positions are inside the work area.
#[derive(Clone, Copy)]
struct Metrics {
    cell_w: i32,
    cell_h: i32,
    wide_spacing: i32,
    pad: i32,
    gap: i32,
    pill_h: i32,
    pill_gap: i32,
    rail_w: i32,
    bridge: i32,
    radius: i32,
    dpi: f32,
}

struct Plan {
    labels: Vec<String>,
    /// Sections with icons, in rail order, each with its icons newest first.
    /// An icon can be in Recent as well as its own section; only one section is ever open.
    groups: Vec<(String, Vec<usize>)>,
    colours: HashMap<String, u32>,
    cfg: Config,
    m: Metrics,
    view_origin: (i32, i32),
    layer_origin: (i32, i32),
    screen_origin: (i32, i32),
    room: (i32, i32),
    tightened: bool,
    unclear: Vec<String>,
    /// The sections that show a folder rather than desktop icons.
    portals: HashMap<String, Portal>,
    /// Each icon's file, where it has one.
    paths: Vec<Option<PathBuf>>,
}

struct Portal {
    folder: PathBuf,
    /// Newest first, and no more than fit.
    files: Vec<(PathBuf, String)>,
    /// How many the folder really has.
    total: usize,
}

/// A folder's visible files, newest first. A folder on a sleeping network drive could hold up this
/// read, which runs on the worker. ponytail: read it on its own thread if anyone hits that.
fn read_portal(folder: PathBuf, sort: &str) -> Option<Portal> {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_OR_SYSTEM: u32 = 2 | 4;
    /// A folder bigger than this is not browsed from the desktop; the Open folder button still works.
    const MOST: usize = 5000;
    let mut found = vec![];
    for entry in std::fs::read_dir(&folder).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.file_attributes() & HIDDEN_OR_SYSTEM != 0 {
            continue;
        }
        let secs = |t: std::io::Result<std::time::SystemTime>| t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let path = entry.path();
        let kind = if meta.is_dir() { String::new() } else { path.extension().map_or(String::new(), |e| e.to_string_lossy().to_lowercase()) };
        found.push((secs(meta.modified()).max(secs(meta.created())), entry.file_name().to_string_lossy().into_owned(), kind, path));
    }
    let total = found.len();
    sort_entries(&mut found, sort);
    found.truncate(MOST);
    Some(Portal { folder, files: found.into_iter().map(|(_, name, _, path)| (path, name)).collect(), total })
}

/// A file's icon, made again if the file has changed since, so a thumbnail is never out of date.
unsafe fn cached_icon(icons: &mut HashMap<PathBuf, (u64, isize)>, path: &Path, side: i32) -> isize {
    let changed = stamp(path) ^ ((side as u64) << 52); // a new icon size is a change too
    if let Some(&(when, icon)) = icons.get(path) {
        if when == changed {
            return icon;
        }
        DestroyIcon(icon as HICON);
    }
    let icon = file_icon(path, side);
    icons.insert(path.to_path_buf(), (changed, icon));
    icon
}

unsafe fn file_icon(path: &Path, side: i32) -> isize {
    let sharp = shellmenu::sharp_icon(path, side); // the size the panel draws it, so nothing is stretched
    if sharp != 0 {
        return sharp;
    }
    if path.to_string_lossy().starts_with("shell:") {
        return shellmenu::shell_icon(path);
    }
    let mut info: SHFILEINFOW = std::mem::zeroed();
    SHGetFileInfoW(wide(&path.to_string_lossy()).as_ptr(), 0, &mut info, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | SHGFI_LARGEICON);
    info.hIcon as isize
}

#[derive(Clone, Copy, PartialEq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn glyph(section: &str) -> char {
    match section {
        RECENT => '\u{E823}',
        "Folders" => '\u{E8B7}',
        "Word" => '\u{E8A5}',
        "Excel" => '\u{E80A}',
        "PowerPoint" => '\u{E7F4}',
        "PDF" => '\u{EA90}',
        "Notes" => '\u{E70B}',
        "Colab & Code" => '\u{E943}',
        "Apps" => '\u{E71D}',
        "Zips" => '\u{E7B8}',
        "Other" => '\u{E712}',
        SETTINGS => '\u{E713}',
        "Downloads" => '\u{E896}',
        _ => '\u{E8A5}',
    }
}

impl Plan {
    fn index(&self, section: &str) -> Option<usize> {
        self.groups.iter().position(|(s, _)| s == section)
    }

    fn colour(&self, section: &str) -> u32 {
        *self.colours.get(section).unwrap_or(&0x00C8B090)
    }

    /// Top of a label inside the rail. Settings sits a little apart from the file sections.
    fn pill_top(&self, at: usize) -> i32 {
        let m = self.m;
        let apart = if self.groups.get(at).is_some_and(|g| g.0 == SETTINGS) { (12.0 * m.dpi) as i32 } else { 0 };
        at as i32 * (m.pill_h + m.pill_gap) + apart
    }

    /// Centre height of a section's label, inside the work area.
    fn pill_mid(&self, at: usize) -> i32 {
        self.m.gap + self.pill_top(at) + self.m.pill_h / 2
    }

    fn rail_x(&self) -> i32 {
        let m = self.m;
        if self.cfg.right_side { self.room.0 - m.gap - m.rail_w } else { m.gap }
    }

    /// Where a panel of this width sits, on the far side of the neck from the rail.
    fn panel_x(&self, w: i32) -> i32 {
        let m = self.m;
        if self.cfg.right_side { self.rail_x() - m.bridge - w } else { self.rail_x() + m.rail_w + m.bridge }
    }

    fn panel_room(&self) -> (i32, i32) {
        let m = self.m;
        (self.room.0 - (m.gap + m.rail_w + m.bridge) - m.gap - 2 * m.pad, self.room.1 - 2 * m.gap - 2 * m.pad)
    }

    /// Whether Rivulet draws this section's files itself, with a header and scrolling.
    fn draws_files(&self, section: &str) -> bool {
        section != SETTINGS && (self.cfg.own_icons || self.portals.contains_key(section))
    }

    fn tabs_of(&self, section: &str) -> Option<&Vec<String>> {
        self.cfg.tabs.iter().map(|(_, list)| list).find(|list| list.iter().any(|s| s == section))
    }

    fn files_in(&self, at: usize) -> i32 {
        let name = &self.groups[at].0;
        self.portals.get(name).map_or(self.groups[at].1.len(), |p| p.files.len()) as i32
    }

    fn is_list(&self, section: &str) -> bool {
        self.cfg.view.iter().any(|(s, v)| s == section && v == "list")
    }

    /// How a panel that draws its own files is laid out: the header's height and the grid.
    fn file_layout(&self, at: usize) -> Option<(i32, settings::Grid)> {
        let m = self.m;
        let name = &self.groups[at].0;
        let header = settings::header_height(settings::header_rows(self.tabs_of(name).is_some(), self.portals.contains_key(name)), m.dpi);
        let (room_w, room_h) = self.panel_room();
        let room_h = room_h - header;
        let n = self.files_in(at).max(1);
        if self.is_list(name) {
            let row = settings::list_row(m.dpi);
            let most = (room_h / row).max(1);
            let w = (m.cell_w * 5).min(room_w).max(1);
            return Some((header, settings::Grid { across: 1, cell: (w, row), pad: m.pad, top: header + m.pad, scroll: 0, visible_rows: n.min(most), list: true }));
        }
        let (most_across, most_rows) = ((room_w / m.cell_w).max(1), (room_h / m.cell_h).max(1));
        let (across, rows) = panel_grid(n, m.cell_w, m.cell_h, room_w, room_h).unwrap_or_else(|| {
            let across = most_across;
            (across, (n + across - 1) / across)
        });
        let across = across.max(3).min(most_across);
        let rows = ((n + across - 1) / across).max(rows.min(1));
        Some((header, settings::Grid { across, cell: (m.cell_w, m.cell_h), pad: m.pad, top: header + m.pad, scroll: 0, visible_rows: rows.min(most_rows), list: false }))
    }

    /// The open panel, level with its label where it fits, and how many icons fit across it.
    fn panel(&self, at: usize) -> Option<(Rect, i32)> {
        let m = self.m;
        let (room_w, room_h) = self.panel_room();
        let name = &self.groups[at].0;
        let (across, w, h) = if name == SETTINGS {
            let (w, h) = settings::size(&self.cfg, m.dpi);
            (1, w, h)
        } else if self.draws_files(name) {
            let (header, g) = self.file_layout(at)?;
            (g.across, g.across * g.cell.0 + 2 * m.pad, header + g.visible_rows * g.cell.1 + 2 * m.pad)
        } else {
            let (across, rows) = panel_grid(self.groups[at].1.len() as i32, m.cell_w, m.cell_h, room_w, room_h)?;
            (across, across * m.cell_w + 2 * m.pad, rows * m.cell_h + 2 * m.pad)
        };
        let lowest = (self.room.1 - m.gap - h).max(m.gap);
        let y = (self.pill_mid(at) - (30.0 * m.dpi) as i32).clamp(m.gap, lowest);
        Some((Rect { x: self.panel_x(w), y, w, h }, across))
    }

    /// Where the nth icon of an open panel sits, in the icon view's coordinates.
    fn cell(&self, r: Rect, across: i32, n: i32) -> (i32, i32) {
        let m = self.m;
        (
            // an icon's position is near its image, so centre that within the cell
            self.view_origin.0 + r.x + m.pad + (n % across) * m.cell_w + m.cell_w / 2 - (20.0 * m.dpi) as i32,
            self.view_origin.1 + r.y + m.pad + (n / across) * m.cell_h,
        )
    }

}

unsafe extern "system" fn add_screen(screen: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(screen, &mut info) != 0 {
        (*(list as *mut Vec<MONITORINFO>)).push(info);
    }
    1
}

unsafe fn screens() -> Vec<MONITORINFO> {
    let mut list: Vec<MONITORINFO> = vec![];
    EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(add_screen), &mut list as *mut _ as LPARAM);
    list
}

/// The chosen screen, minus the taskbar.
unsafe fn chosen_work_area(wanted: usize) -> RECT {
    let list = screens();
    let brief: Vec<(i32, bool)> = list.iter().map(|m| (m.rcMonitor.left, m.dwFlags & MONITORINFOF_PRIMARY != 0)).collect();
    match pick_screen(&brief, wanted) {
        Some(i) => list[i].rcWork,
        None => {
            let mut work = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as _, 0);
            work
        }
    }
}

unsafe fn make_plan(desk: &Desktop, cfg: &Config) -> Result<Plan, String> {
    layers::apply_look(cfg);
    if GetWindowLongPtrW(desk.lv, GWL_STYLE) as i32 & LVS_AUTOARRANGE != 0 {
        return Err("Icons cannot be placed while \"Auto arrange icons\" is on.\n\
                    Right-click the desktop > View > untick Auto arrange icons, then run this again."
            .into());
    }
    let labels: Vec<String> = (0..desk.count()).map(|i| desk.text(i)).collect();
    let (details, unclear, paths) = describe(&labels);
    let mut sortable: HashMap<&str, Vec<(std::cmp::Reverse<u64>, String, usize)>> = HashMap::new();
    let wants_size = cfg.names.iter().any(|(p, _)| p.to_lowercase().starts_with("size"));
    let today = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for (i, (label, (kind, time))) in labels.iter().zip(&details).enumerate() {
        let size = if wants_size { paths.get(i).and_then(|p| p.as_ref()).and_then(|p| p.metadata().ok()).map_or(0, |m| m.len()) } else { 0 };
        let age = today.saturating_sub(*time) / 86_400;
        sortable.entry(classify_with(label, kind, size, age, cfg)).or_default().push((std::cmp::Reverse(*time), label.to_lowercase(), i));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let flat: Vec<(&str, u64, usize)> = details.iter().enumerate().map(|(i, (k, t))| (k.as_str(), *t, i)).collect();
    let recent = pick_recent(&flat, now, cfg.recent_days, cfg.recent_limit);

    let mut groups = vec![];
    if !recent.is_empty() {
        groups.push((RECENT.to_string(), recent));
    }
    for section in &cfg.order {
        if let Some(mut items) = sortable.remove(section.as_str()) {
            let how = cfg.sort.iter().find(|(s, _)| s == section).map_or("newest", |(_, h)| h.as_str());
            let mut entries: Vec<(u64, String, String, usize)> = items.drain(..).map(|(t, name, i)| (t.0, name, details[i].0.clone(), i)).collect();
            sort_entries(&mut entries, how);
            groups.push((section.clone(), entries.into_iter().map(|x| x.3).collect::<Vec<_>>()));
        }
    }
    let mut portals = HashMap::new();
    for (name, folder) in &cfg.portals {
        // a folder that is missing, such as an unplugged drive, just shows no label for now
        let how = cfg.sort.iter().find(|(s, _)| s == name).map_or("newest", |(_, h)| h.as_str());
        if let Some(portal) = read_portal(PathBuf::from(expand_variables(folder, |n| std::env::var(n).ok())), how) {
            groups.push((name.clone(), vec![]));
            portals.insert(name.clone(), portal);
        }
    }
    groups.push((SETTINGS.to_string(), vec![]));

    let dpi = GetDpiForWindow(desk.lv) as f32 / 96.0;
    let work = chosen_work_area(cfg.monitor);
    let mut origin = POINT { x: work.left, y: work.top };
    ScreenToClient(desk.lv, &mut origin);
    let (_, spacing_h) = desk.spacing();
    let scaled = |v: f32| (v * dpi) as i32;
    let room = (work.right - work.left, work.bottom - work.top);
    // a big icon or big text needs a taller cell than Windows' own spacing gives
    let own_cell_h = if cfg.own_icons { settings::icon_pixels(dpi) + scaled(16.0) + 2 * layers::name_line(dpi) } else { 0 };
    let wide_spacing = scaled(120.0); // wider than Windows' default, so more of each name shows
    for tighten in [1.0f32, 0.95, 0.9, 0.85, 0.8] {
        let m = Metrics {
            cell_w: (wide_spacing as f32 * tighten) as i32,
            cell_h: (own_cell_h.max(spacing_h) as f32 * tighten) as i32,
            wide_spacing,
            pad: scaled(6.0),
            gap: scaled(10.0),
            pill_h: scaled(32.0),
            pill_gap: scaled(4.0),
            rail_w: scaled(168.0),
            bridge: scaled(76.0),
            radius: scaled(match cfg.corners.as_str() { "square" => 3.0, "round" => 26.0, _ => 14.0 }),
            dpi,
        };
        let plan = Plan {
            labels: vec![],
            groups: vec![],
            colours: cfg.colours.iter().cloned().collect(),
            cfg: cfg.clone(),
            m,
            view_origin: (origin.x, origin.y),
            layer_origin: layers::to_parent(work.left, work.top),
            screen_origin: (work.left, work.top),
            room,
            tightened: tighten < 1.0,
            unclear: vec![],
            portals: HashMap::new(),
            paths: vec![],
        };
        let (room_w, room_h) = plan.panel_room();
        // Rivulet's own icons scroll; Explorer's have to fit on the screen
        if cfg.own_icons || groups.iter().all(|(_, g)| panel_grid(g.len() as i32, m.cell_w, m.cell_h, room_w, room_h).is_some()) {
            return Ok(Plan { labels, groups, unclear, portals, paths, ..plan });
        }
    }
    Err(format!("{} icons do not fit on the screen, even with the tightest spacing.", labels.len()))
}

// ---------- applying the sections ----------

#[derive(Clone)]
struct Label {
    name: String,
    rect: RECT,
}

static LABELS: Mutex<Vec<Label>> = Mutex::new(Vec::new());
static PANEL_ON_SCREEN: Mutex<Option<RECT>> = Mutex::new(None);
static COMMANDS: OnceLock<Sender<Command>> = OnceLock::new();
static CHANGE_WAITING: AtomicBool = AtomicBool::new(false);
static ICON_LIST: AtomicUsize = AtomicUsize::new(0);

/// What a question put to the user was for.
pub enum Ask {
    RenameSection(String),
    AddSection,
    AddRule,
    SaveLayout,
    NamePortal(String),
}

enum Command {
    Toggle(String),
    Close,
    Hover(Option<String>),
    Filed(String),
    SettingsClick(POINT, bool, bool),
    PanelRepaint,
    FileMenu(POINT),
    FiledItems(Vec<String>, String),
    /// The button came up on a file without a drag.
    FileRelease(POINT),
    HoverOpen(String),
    HoverClose,
    Scroll(i32),
    Answered(Ask, String),
    ColourChosen(String, String),
    FolderChosen(String),
    Find,
    Workspace(i32),
    FadeIn,
    PeekOff,
    SelectBox,
    Key(u32, bool, bool),
    ToggleRail(POINT),
    Shortcut,
    Changed,
    Light,
    Retidy,
    Stop,
}

fn send(c: Command) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(c);
    }
}

struct Sections {
    desk: Desktop,
    open: Option<String>,
    hover: Option<String>,
    hidden: Vec<usize>,
    count: usize,
    seen: Option<HashSet<String>>,
    last_plan: Option<Plan>,
    last_section: Option<String>,
    shortcut_showing: bool,
    rail_hidden: bool,
    login: bool,
    tinted_for: String,
    main_window: usize,
    icons: HashMap<PathBuf, (u64, isize)>,
    last_file_click: Option<(Instant, PathBuf)>,
    /// While a hover-opened section folds: its rail, label and full panel on screen, to see the mouse come back.
    fold_watch: Option<(RECT, RECT, RECT)>,
    /// The fold was cut short because the mouse came back; how far open it still was.
    fold_back: Option<f32>,
    selected: Vec<PathBuf>,
    /// Where a Shift press measures from.
    anchor: i32,
    /// How many rows each section is scrolled.
    scroll: HashMap<String, i32>,
    /// Where each portal has been navigated to, oldest first; empty means its own folder.
    nav: HashMap<String, Vec<PathBuf>>,
    /// What is being typed to find, and the section to go back to.
    query: String,
    find_from: Option<String>,
    screens_seen: String,
    last_near: Instant,
    faded: bool,
}

unsafe fn load_plan(desk: &Desktop) -> Result<Plan, String> {
    let cfg = parse_config(&std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?)?;
    make_plan(desk, &cfg)
}

impl Sections {
    fn far(&self) -> i32 {
        unsafe { self.desk.width() + 2000 }
    }

    /// Everything on the desktop counts as already seen the first time; after that, anything
    /// unknown is new. Opening a section marks its icons as seen.
    fn update_seen(&mut self, plan: &Plan, open_at: Option<usize>) {
        let names = |list: &[usize]| list.iter().map(|&i| plan.labels[i].to_lowercase()).collect::<Vec<_>>();
        let all = names(&(0..plan.labels.len()).collect::<Vec<_>>());
        let seen = self.seen.get_or_insert_with(|| match std::fs::read_to_string(seen_file()) {
            Ok(text) => text.lines().map(String::from).collect(),
            Err(_) => all.iter().cloned().collect(),
        });
        let before = seen.len();
        if let Some(at) = open_at {
            seen.extend(names(&plan.groups[at].1));
        }
        // names are never dropped: the icon list briefly empties during a refresh, and that must
        // not make everything look new afterwards
        if seen.len() != before || !seen_file().exists() {
            let list: Vec<&str> = seen.iter().map(String::as_str).collect();
            let _ = std::fs::write(seen_file(), list.join("\n"));
        }
    }

    /// Draw the rail and remember where each label is for clicks.
    unsafe fn show_rail(&self, plan: &Plan) {
        if self.rail_hidden {
            LABELS.lock().unwrap().clear();
            layers::hide_rail();
            return;
        }
        let m = plan.m;
        let height = plan.pill_top(plan.groups.len().saturating_sub(1)) + m.pill_h + m.pill_gap;
        let (sx, sy) = plan.screen_origin;
        let rail_x = plan.rail_x();
        let (mut hits, mut pills) = (vec![], vec![]);
        for (at, (name, icons)) in plan.groups.iter().enumerate() {
            let top = plan.pill_top(at);
            let rect = RECT { left: 0, top, right: m.rail_w, bottom: top + m.pill_h };
            let fresh = plan.cfg.dots && self.seen.as_ref().is_some_and(|seen| icons.iter().any(|&i| !seen.contains(&plan.labels[i].to_lowercase())));
            pills.push(layers::Pill {
                name: name.clone(),
                count: plan.portals.get(name).map_or(icons.len(), |p| p.total),
                active: self.open.as_deref() == Some(name.as_str()),
                hover: self.hover.as_deref() == Some(name.as_str()),
                fresh,
                colour: plan.colour(name),
                glyph: plan.cfg.symbols.iter().find(|(s, _)| s == name).map_or_else(|| glyph(name), |(_, c)| *c),
                rect,
            });
            let screen = RECT { left: sx + rail_x, top: sy + m.gap + top, right: sx + rail_x + m.rail_w, bottom: sy + m.gap + top + m.pill_h };
            hits.push(Label { name: name.clone(), rect: screen });
        }
        *layers::PILLS.lock().unwrap() = pills;
        *LABELS.lock().unwrap() = hits;
        let (lx, ly) = plan.layer_origin;
        layers::show_rail(lx + rail_x, ly + m.gap, m.rail_w + 1, height.max(1), m.radius);
    }

    unsafe fn draw_open(&self, plan: &Plan, at: usize, r: Rect, reach: f32) {
        self.draw_open_from(plan, plan.pill_mid(at), r, reach);
    }

    /// The panel at `r`, its neck starting from the rail at height `pill_y`.
    unsafe fn draw_open_from(&self, plan: &Plan, pill_y: i32, r: Rect, reach: f32) {
        let m = plan.m;
        let (lx, ly) = plan.layer_origin;
        layers::set_panel(lx + r.x, ly + r.y, r.w, r.h, m.radius.min(r.h / 2).min(r.w / 2));
        // from the rail's inner edge to the facing edge of the panel, on either side of the screen
        let (start_x, end_x) = if plan.cfg.right_side { (plan.rail_x() + 2, r.x + r.w - 2) } else { (plan.rail_x() + m.rail_w - 2, r.x + 2) };
        let start = (lx + start_x, ly + pill_y);
        let end = (lx + end_x, ly + r.y + (r.h / 2).min((46.0 * m.dpi) as i32));
        // liquid pours out thick and thins as it finishes
        let style = plan.cfg.style.as_str();
        let fluid = matches!(style, "liquid" | "aurora" | "neon");
        let swell = if fluid { 1.0 + (1.0 - reach) * 0.9 } else { 1.0 };
        let waist = match style {
            "liquid" | "aurora" => 16.0,
            "neon" => 9.0, // a lit tube looks best slim
            _ => 2.5,
        };
        layers::set_neck(start, end, reach, m.pill_h as f32 * 0.5 * swell, waist * swell * m.dpi);
    }

    /// A fixed theme, or a dark shade of the wallpaper's average colour.
    /// Glass needs a fresh blurred wallpaper; the other styles don't keep one.
    fn apply_style(&self, style: &str) {
        unsafe {
            if matches!(style, "glass" | "liquid" | "aurora" | "neon") && pretty::is_on() {
                let shader_look = match style {
                    "liquid" => Some(0.0),
                    "aurora" => Some(1.0),
                    "neon" => Some(2.0),
                    _ => None,
                };
                if let Err(e) = pretty::load_backdrop(GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN), shader_look) {
                    log(&format!("glass unavailable, using the smooth look: {e}"));
                    pretty::drop_backdrop();
                }
            } else {
                pretty::drop_backdrop();
            }
        }
    }

    fn apply_theme(&mut self, theme: &str, style: &str, tag: &str, panel: Option<u32>) {
        let now = format!("{theme}|{style}|{tag}|{}|{}", wallpaper::current(), wallpaper::placement_setting());
        if now != self.tinted_for {
            self.apply_style(style);
            if let Some((pill, panel)) = theme_tints(theme) {
                layers::set_tints(pill, panel);
            } else if let Some(average) = wallpaper::average_colour() {
                layers::set_tints(dark_shade(average, 46.0), dark_shade(average, 24.0));
            }
            if let Some(chosen) = panel {
                layers::set_tints(layers::pill_tint(), chosen);
            }
            self.tinted_for = now;
        }
    }

    /// A panel of files: its header, and its cells with icons only for what shows, so a big folder
    /// costs nothing for the files never scrolled to.
    unsafe fn file_panel(&mut self, plan: &Plan, at: usize, name: &str, mut grid: settings::Grid) -> Vec<settings::Item> {
        let m = plan.m;
        let side = if grid.list { settings::list_icon_pixels(m.dpi) } else { settings::icon_pixels(m.dpi) };
        let mut entries: Vec<(Option<PathBuf>, String)> = match plan.portals.get(name) {
            Some(portal) => portal.files.iter().map(|(p, f)| (Some(p.clone()), f.clone())).collect(),
            None => plan.groups[at].1.iter().map(|&i| (plan.paths.get(i).cloned().flatten(), plan.labels[i].clone())).collect(),
        };
        let rows = (entries.len() as i32 + grid.across - 1) / grid.across.max(1);
        let most = (rows - grid.visible_rows).max(0);
        let scroll = self.scroll.get(name).copied().unwrap_or(0).clamp(0, most);
        self.scroll.insert(name.to_string(), scroll);
        grid.scroll = scroll;
        let first_shown = (scroll * grid.across) as usize;
        let last_shown = ((scroll + grid.visible_rows) * grid.across) as usize;
        let files: Vec<_> = entries
            .drain(..)
            .enumerate()
            .map(|(n, (path, file))| {
                let shows = n >= first_shown && n < last_shown;
                let icon = match (&path, shows) {
                    (Some(p), true) => cached_icon(&mut self.icons, p, side),
                    _ => 0,
                };
                let detail = match (&path, shows && grid.list) {
                    (Some(p), true) => p.metadata().map_or(String::new(), |meta| {
                        let date = human_date(stamp(p));
                        if meta.is_dir() { date } else { format!("{date}   {}", human_size(meta.len())) }
                    }),
                    _ => String::new(),
                };
                (path, file, icon, detail)
            })
            .collect();
        let stack = self.nav.get(name);
        let title = if name == FIND {
            if self.query.is_empty() { "Type to find a file".to_string() } else { format!("Find: {}  ({})", self.query, files.len()) }
        } else if let Some(portal) = plan.portals.get(name) {
            let folder = portal.folder.file_name().map_or(name.to_string(), |f| f.to_string_lossy().into_owned());
            if stack.is_some_and(|s| !s.is_empty()) { folder } else { name.to_string() }
        } else {
            name.to_string()
        };
        let sort = plan.cfg.sort.iter().find(|(s, _)| s == name).map_or("newest", |(_, h)| h.as_str());
        let header = settings::Header {
            title,
            sort: (name != FIND).then(|| sort_label(sort).to_string()),
            list: grid.list,
            pinned: plan.cfg.pinned.iter().any(|p| p == name),
            tabs: plan.tabs_of(name).map_or(vec![], |list| list.iter().filter(|s| plan.index(s).is_some()).map(|s| (s.clone(), s == name)).collect()),
            nav: plan.portals.get(name).map(|p| (p.folder.clone(), stack.is_some_and(|s| !s.is_empty()), p.folder.parent().is_some())),
        };
        let width = grid.across * grid.cell.0 + 2 * m.pad;
        let mut items = settings::build_header(&header, width, m.pad, m.dpi);
        if name == FIND {
            items.retain(|i| !matches!(i.action, Some(Action::TogglePin | Action::ToggleView))); // a passing list has no settings of its own
        }
        items.extend(settings::file_cells(&files, grid));
        items
    }

    /// What Settings and the rail have changed since the rules file was read: where portals have been
    /// navigated to, and what is being typed to find.
    fn with_current_views(&self, mut plan: Plan) -> Plan {
        for (name, stack) in &self.nav {
            if let (Some(folder), true) = (stack.last(), plan.portals.contains_key(name)) {
                let how = plan.cfg.sort.iter().find(|(s, _)| s == name).map_or("newest", |(_, h)| h.as_str());
                if let Some(portal) = read_portal(folder.clone(), how) {
                    plan.portals.insert(name.clone(), portal);
                }
            }
        }
        if self.find_from.is_some() || self.open.as_deref() == Some(FIND) {
            let q = self.query.to_lowercase();
            let hits: Vec<usize> = if q.is_empty() { vec![] } else { plan.labels.iter().enumerate().filter(|(_, l)| l.to_lowercase().contains(&q)).map(|(i, _)| i).collect() };
            plan.groups.insert(0, (FIND.to_string(), hits));
        }
        plan
    }

    /// Change the rules file, keeping only a result that loads, and put the change on screen.
    unsafe fn edit_file(&mut self, change: impl FnOnce(&str) -> String) -> Result<(), String> {
        let before = std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?;
        let after = change(&before);
        if let Err(e) = parse_config(&after) {
            alert(&e);
            return Ok(());
        }
        std::fs::write(rules_file(), after).map_err(|e| e.to_string())?;
        self.last_plan = None;
        self.settle()
    }

    /// Draw the panel again after its contents changed, without re-reading the desktop.
    unsafe fn refresh_panel(&mut self) {
        if let Some(plan) = self.last_plan.take() {
            self.sync_settings(&plan);
            self.last_plan = Some(plan);
            self.repaint_panel();
        }
    }

    unsafe fn scroll_panel(&mut self, rows: i32) {
        if let Some(name) = self.open.clone() {
            *self.scroll.entry(name).or_insert(0) += rows;
            self.refresh_panel();
        }
    }

    /// Show this portal at another folder.
    unsafe fn navigate(&mut self, section: &str, folder: PathBuf) -> Result<(), String> {
        self.nav.entry(section.to_string()).or_default().push(folder);
        self.scroll.remove(section);
        self.select(vec![]);
        self.last_plan = None;
        self.settle()
    }

    /// Open the panel of everything matching what has been typed.
    unsafe fn find_type(&mut self, letter: Option<char>) -> Result<(), String> {
        match letter {
            Some(c) => self.query.push(c),
            None => {
                self.query.pop();
            }
        }
        if self.find_from.is_none() && self.open.as_deref() != Some(FIND) {
            self.find_from = Some(self.open.clone().unwrap_or_default());
            self.open = Some(FIND.to_string());
        }
        self.selected.clear();
        self.scroll.remove(FIND);
        self.last_plan = None;
        self.settle()
    }

    /// Stop finding, and go back to the section that was open.
    unsafe fn find_end(&mut self) -> Result<(), String> {
        self.query.clear();
        let back = self.find_from.take().filter(|s| !s.is_empty());
        let plan = self.current_plan()?;
        self.close_open(&plan, false);
        if back.is_none() && layers::peeking() {
            layers::set_peek(false); // Find brought the sections up, so it puts them away too
            self.shortcut_showing = false;
        }
        self.open = back;
        self.last_plan = None;
        self.settle()
    }

    /// Save the rules file under the workspace being left, and put another in its place.
    unsafe fn switch_layout(&mut self, name: &str) -> Result<(), String> {
        let current = std::fs::read_to_string(current_layout_file()).unwrap_or_default();
        if !current.trim().is_empty() {
            let _ = std::fs::copy(rules_file(), layout_file(current.trim()));
        }
        let text = std::fs::read_to_string(layout_file(name)).map_err(|e| e.to_string())?;
        if let Err(e) = parse_config(&text) {
            alert(&format!("That layout could not be loaded. {e}"));
            return Ok(());
        }
        std::fs::write(rules_file(), text).map_err(|e| e.to_string())?;
        let _ = std::fs::write(current_layout_file(), name);
        self.open = None;
        self.last_plan = None;
        self.settle()
    }

    /// The screens changed: keep the layout for the old arrangement, and bring back the new one's.
    unsafe fn check_screens(&mut self, cfg: &Config) -> bool {
        let seen = screens_name();
        if seen == self.screens_seen {
            return false;
        }
        let old = std::mem::replace(&mut self.screens_seen, seen.clone());
        if old.is_empty() || !cfg.monitor_profiles {
            return false;
        }
        let _ = std::fs::create_dir_all(profiles_dir());
        let _ = std::fs::copy(rules_file(), profiles_dir().join(format!("{old}.ini")));
        match std::fs::read_to_string(profiles_dir().join(format!("{seen}.ini"))) {
            Ok(text) if parse_config(&text).is_ok() => std::fs::write(rules_file(), text).is_ok(),
            _ => false,
        }
    }

    /// Fade the labels toward this much, a few steps at a time.
    unsafe fn fade_rail(&mut self, target: u32) {
        let from = (layers::rail_fade() * 100.0) as i32;
        for step in 1..=8 {
            layers::set_rail_fade((from + (target as i32 - from) * step / 8) as u32);
            std::thread::sleep(Duration::from_millis(16));
        }
        self.faded = target < 100;
    }

    /// Fill the panel with controls if Settings is the open section, or clear them if not.
    unsafe fn sync_settings(&mut self, plan: &Plan) {
        settings::LAYOUTS.lock().unwrap().clone_from(&layout_names());
        let items = match self.open.as_deref() {
            Some(SETTINGS) => settings::build(&plan.cfg, self.login, plan.m.dpi),
            Some(name) if plan.draws_files(name) => {
                let name = name.to_string();
                match plan.index(&name).and_then(|at| plan.file_layout(at).map(|l| (at, l))) {
                    Some((at, (_, grid))) => self.file_panel(plan, at, &name, grid),
                    None => vec![],
                }
            }
            _ => vec![],
        };
        // the list may have changed, so find the selected file again and let hover be found afresh
        let selected: Vec<i32> = items.iter().enumerate().filter(|(_, i)| matches!(&i.action, Some(Action::Open(p)) if self.selected.contains(p))).map(|(n, _)| n as i32).collect();
        *layers::SELECTED_ITEMS.lock().unwrap() = selected;
        layers::HOVER_ITEM.store(-1, Ordering::Relaxed);
        // a panel of Rivulet's own controls, so clicks in it come here and not to the desktop
        SETTINGS_OPEN.store(!items.is_empty(), Ordering::Relaxed);
        layers::set_settings(items);
    }

    /// Put everything exactly where it belongs, with no animation.
    unsafe fn settle(&mut self) -> Result<(), String> {
        if !self.desk.alive() {
            self.desk = Desktop::open()?; // Explorer restarted
            ICON_LIST.store(self.desk.lv as usize, Ordering::Relaxed);
            if self.desk.grid_is_on() {
                grid_off()?;
            }
        }
        if !layers::alive() {
            SendMessageW(self.main_window as HWND, WM_REMAKE, 0, 0);
        }
        let mut plan = self.with_current_views(load_plan(&self.desk)?);
        if self.check_screens(&plan.cfg) {
            plan = self.with_current_views(load_plan(&self.desk)?);
        }
        let panel_colour = plan.cfg.panel_colour.map_or(String::new(), |c| c.to_string());
        self.apply_theme(&plan.cfg.theme, &plan.cfg.style, &panel_colour, plan.cfg.panel_colour);
        let pinned = self.open.as_ref().is_some_and(|o| plan.cfg.pinned.contains(o));
        CLICK_AWAY.store(plan.cfg.click_away && !pinned, Ordering::Relaxed);
        DOUBLE_CLICK_HIDES.store(plan.cfg.double_click_hide, Ordering::Relaxed);
        HOVER_OPENS.store(plan.cfg.hover_open, Ordering::Relaxed);
        FADE_ON.store(plan.cfg.fade, Ordering::Relaxed);
        PEEK_SHORTCUT.store(plan.cfg.peek, Ordering::Relaxed);
        if let Some(open) = &self.open {
            if plan.index(open).is_none() {
                self.open = None; // that section has emptied
            }
        }
        let open_at = self.open.as_deref().and_then(|s| plan.index(s));
        let m = plan.m;
        if self.desk.spacing().0 != m.wide_spacing {
            remember_spacing(&self.desk);
            self.desk.set_spacing(m.wide_spacing, self.desk.spacing().1);
        }

        let shown_panel = open_at.and_then(|at| plan.panel(at).map(|(r, across)| (at, r, across)));
        let mut wanted: HashMap<usize, (i32, i32)> = HashMap::new();
        if let Some((at, r, across)) = shown_panel.filter(|_| !plan.cfg.own_icons) {
            for (n, &i) in plan.groups[at].1.iter().enumerate() {
                wanted.insert(i, plan.cell(r, across, n as i32));
            }
        }
        let far = self.far();
        self.hidden.clear();
        let mut moved = false;
        for i in 0..plan.labels.len() {
            let want = wanted.get(&i).copied().unwrap_or_else(|| {
                self.hidden.push(i);
                (far, plan.view_origin.1)
            });
            if self.desk.position(i) != want {
                self.desk.place(i, want.0, want.1);
                moved = true;
            }
        }
        if moved {
            InvalidateRect(self.desk.lv, std::ptr::null(), 1); // clears scraps of name text left behind
        }
        self.count = plan.labels.len();
        self.update_seen(&plan, open_at);
        self.show_rail(&plan);
        self.sync_settings(&plan);
        match shown_panel {
            Some((at, r, _)) => {
                layers::set_accent(plan.colour(&plan.groups[at].0));
                self.draw_open(&plan, at, r, 1.0);
                let (sx, sy) = plan.screen_origin;
                *PANEL_ON_SCREEN.lock().unwrap() = Some(RECT { left: sx + r.x, top: sy + r.y, right: sx + r.x + r.w, bottom: sy + r.y + r.h });
            }
            None => {
                layers::hide_panel();
                *PANEL_ON_SCREEN.lock().unwrap() = None;
            }
        }
        self.last_plan = Some(plan);
        Ok(())
    }

    /// Run one open (forward) or close (backward) of a section.
    unsafe fn flow(&mut self, plan: &Plan, at: usize, opening: bool, start: f32) {
        let Some((full, across)) = plan.panel(at) else { return };
        // Rivulet draws its own icons as part of the panel, so Explorer's stay parked
        let icons = if plan.cfg.own_icons { vec![] } else { plan.groups[at].1.clone() };
        let m = plan.m;
        layers::set_accent(plan.colour(&plan.groups[at].0));
        let right = plan.cfg.right_side;
        let liquid = matches!(plan.cfg.style.as_str(), "liquid" | "aurora" | "neon");
        let seed = Rect { x: full.x, y: (plan.pill_mid(at) - m.pill_h / 2).clamp(m.gap, plan.room.1 - m.pill_h), w: m.pill_h, h: m.pill_h };
        let rows = (icons.len() as i32 + across - 1) / across;
        let (open_time, close_time) = speed_times(&plan.cfg.speed);
        let seconds = if opening { open_time } else { close_time };
        let parked = (self.far(), plan.view_origin.1);
        let slide = (22.0 * m.dpi) as f32;
        // only the part of the icon list inside the panel needs repainting
        let dirty = RECT {
            left: plan.view_origin.0 + full.x - slide as i32,
            top: plan.view_origin.1 + full.y,
            right: plan.view_origin.0 + full.x + full.w + slide as i32,
            bottom: plan.view_origin.1 + full.y + full.h,
        };
        if !opening {
            self.park(&icons, parked); // leaving: icons go at once, and the panel folds up after them
        }
        let mut placed = vec![parked; icons.len()];
        let began = Instant::now() - Duration::from_secs_f32(start * seconds);
        loop {
            let t = if seconds > 0.0 { (began.elapsed().as_secs_f32() / seconds).min(1.0) } else { 1.0 };
            let progress = if opening { t } else { 1.0 - t };
            // the last frame lands exactly on full size, which is what tells Pretty mode it has stopped
            let shape = if opening && t >= 1.0 { 1.0 } else if opening { spring(progress) } else { ease_out(progress) };
            // liquid wobbles: height and width settle out of step
            let (shape_w, shape_h) = if liquid && opening && t < 1.0 { jelly(progress) } else { (shape, shape) };
            let w = mix(seed.w, full.w, shape_w);
            // the edge facing the rail stays put; the panel grows away from it
            let x = if right { full.x + full.w - w } else { full.x };
            let r = Rect { x, y: mix(seed.y, full.y, shape_h), w, h: mix(seed.h, full.h, shape_h) };
            self.draw_open(plan, at, r, ease_out((progress * 2.2).min(1.0)));
            if let (false, Some((rail, label, panel))) = (opening, self.fold_watch) {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                if label_at(pt).is_some_and(|n| n != plan.groups[at].0) {
                    break; // on to another section: this one goes now, the next opens without waiting
                }
                if keeps_open(pt, rail, label, panel) {
                    // back again: open from the size it has reached, on the opening curve
                    let size = ease_out(progress);
                    self.fold_back = Some((0..=100).map(|i| i as f32 / 100.0).find(|&t| spring(t) >= size).unwrap_or(1.0));
                    break;
                }
            }
            if opening {
                let wanted: Vec<(i32, i32)> = (0..icons.len() as i32)
                    .map(|n| {
                        // the panel uncovers each icon as it grows past it; it then slides the last short way in
                        let column = if right { across - 1 - n % across } else { n % across };
                        let across_part = column as f32 / across as f32;
                        let down_part = (n / across) as f32 / rows.max(1) as f32;
                        let delay = across_part * 0.5 + down_part * 0.25;
                        let own = ((progress - delay) / 0.3).clamp(0.0, 1.0);
                        if own <= 0.0 {
                            return parked;
                        }
                        let to = plan.cell(full, across, n);
                        let away = (slide * (1.0 - ease_out(own))) as i32;
                        (if right { to.0 + away } else { to.0 - away }, to.1)
                    })
                    .collect();
                let changed: Vec<usize> = (0..icons.len()).filter(|&n| wanted[n] != placed[n]).collect();
                if !changed.is_empty() {
                    // move them together and let Explorer repaint in its own time, never waiting on it
                    SendMessageW(self.desk.lv, WM_SETREDRAW, 0, 0);
                    for n in changed {
                        self.desk.place(icons[n], wanted[n].0, wanted[n].1);
                        placed[n] = wanted[n];
                    }
                    SendMessageW(self.desk.lv, WM_SETREDRAW, 1, 0);
                    InvalidateRect(self.desk.lv, &dirty, 0);
                }
            }
            if t >= 1.0 {
                break;
            }
            if DwmFlush() != 0 {
                std::thread::sleep(Duration::from_millis(8)); // no composition, so pace by hand
            }
        }
    }

    unsafe fn park(&self, icons: &[usize], parked: (i32, i32)) {
        SendMessageW(self.desk.lv, WM_SETREDRAW, 0, 0);
        for &i in icons {
            self.desk.place(i, parked.0, parked.1);
        }
        SendMessageW(self.desk.lv, WM_SETREDRAW, 1, 0);
        InvalidateRect(self.desk.lv, std::ptr::null(), 0);
    }

    /// Fold up the open section; when another is about to open it just vanishes, for a quick switch.
    unsafe fn close_open(&mut self, plan: &Plan, animate: bool) {
        let previous = self.open.take();
        *HOVER_SECTION.lock().unwrap() = None;
        *OPENED_BY_HOVER.lock().unwrap() = None;
        self.selected.clear();
        self.anchor = -1;
        layers::SELECTED_ITEMS.lock().unwrap().clear();
        if !layers::peeking() {
            PostMessageW(self.main_window as HWND, WM_GIVE_BACK_KEYS, 0, 0);
        }
        layers::HOVER_ITEM.store(-1, Ordering::Relaxed);
        if let Some(at) = previous.as_deref().and_then(|s| plan.index(s)) {
            self.show_rail(plan);
            if !animate {
                self.sync_settings(plan);
            }
            if animate {
                self.flow(plan, at, false, 0.0);
            } else {
                self.park(&plan.groups[at].1, (self.far(), plan.view_origin.1));
            }
            if self.fold_back.is_none() {
                layers::hide_panel();
            }
        }
    }

    /// The layout from the last settle, which Windows' change reports keep fresh, so a click
    /// does not have to re-read the whole desktop before anything moves.
    unsafe fn current_plan(&mut self) -> Result<Plan, String> {
        match self.last_plan.take() {
            Some(plan) if plan.labels.len() == self.desk.count() => Ok(plan),
            _ => Ok(self.with_current_views(load_plan(&self.desk)?)),
        }
    }

    /// The keyboard shortcut: bring the desktop forward with the last section open, and on the
    /// second press put everything back.
    unsafe fn shortcut(&mut self) -> Result<(), String> {
        // Peek: the sections come up over the windows, which stay where they are
        if PEEK_SHORTCUT.load(Ordering::Relaxed) && self.last_plan.as_ref().is_some_and(|p| p.cfg.own_icons) {
            if layers::peeking() {
                self.end_peek();
                return Ok(());
            }
            layers::set_peek(true);
            self.shortcut_showing = true;
            self.rail_hidden = false;
            let plan = self.current_plan()?;
            let wanted = self.open.clone().or_else(|| self.last_section.clone()).filter(|s| plan.index(s).is_some()).or_else(|| plan.groups.first().map(|g| g.0.clone()));
            self.last_plan = Some(plan);
            self.settle()?;
            let shown = match wanted {
                Some(name) if self.open.as_deref() != Some(name.as_str()) => self.toggle(name),
                _ => Ok(()),
            };
            self.take_keys();
            return shown;
        }
        // the taskbar's own show-desktop toggle; faking Win+D would fail with Ctrl and Alt still held
        let show_desktop = || {
            const TOGGLE_DESKTOP: usize = 407;
            PostMessageW(FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null()), WM_COMMAND, TOGGLE_DESKTOP, 0);
        };
        if self.shortcut_showing {
            self.shortcut_showing = false;
            self.close()?;
            show_desktop();
            return Ok(());
        }
        self.shortcut_showing = true;
        show_desktop();
        std::thread::sleep(Duration::from_millis(120)); // let the windows clear before anything moves
        if self.open.is_none() {
            let plan = self.current_plan()?;
            self.rail_hidden = false;
            let wanted = self.last_section.clone().filter(|s| plan.index(s).is_some()).or_else(|| plan.groups.first().map(|g| g.0.clone()));
            self.last_plan = Some(plan);
            if let Some(name) = wanted {
                return self.toggle(name);
            }
        }
        Ok(())
    }

    unsafe fn toggle(&mut self, name: String) -> Result<(), String> {
        if self.last_plan.as_ref().is_some_and(|p| p.portals.contains_key(&name)) {
            self.last_plan = None; // a folder may have changed since it was last read
        }
        let plan = self.current_plan()?;
        let previous = self.open.clone();
        // ponytail: slides only when Rivulet draws the icons; Explorer's icons would need moving frame by frame too
        let slide_from = previous.as_deref().filter(|p| *p != name && plan.cfg.own_icons && !layers::peeking()).and_then(|p| plan.index(p)).filter(|_| plan.index(&name).is_some());
        if slide_from.is_some() {
            // as for a close, but the panel stays up to glide to its new place
            *HOVER_SECTION.lock().unwrap() = None;
            *OPENED_BY_HOVER.lock().unwrap() = None;
            self.selected.clear();
            self.anchor = -1;
            layers::SELECTED_ITEMS.lock().unwrap().clear();
            layers::HOVER_ITEM.store(-1, Ordering::Relaxed);
            self.open = None;
        } else {
            self.close_open(&plan, previous.as_deref() == Some(name.as_str()));
        }
        if previous.as_deref() != Some(name.as_str()) {
            if let Some(at) = plan.index(&name) {
                if name == SETTINGS {
                    self.login = login_is_on();
                } else {
                    self.last_section = Some(name.clone());
                }
                self.open = Some(name);
                match slide_from {
                    Some(from) => self.slide(&plan, from, at),
                    None => {
                        self.show_rail(&plan);
                        self.sync_settings(&plan);
                        self.flow(&plan, at, true, 0.0);
                    }
                }
            }
        }
        let _ = std::fs::write(open_file(), self.open.as_deref().unwrap_or(""));
        self.settle()
    }

    /// Move straight from one open section to another: the panel glides and reshapes from the old
    /// place to the new, and its neck slides along the rail between the two labels.
    unsafe fn slide(&mut self, plan: &Plan, from: usize, to: usize) {
        let (Some((old, _)), Some((new, _))) = (plan.panel(from), plan.panel(to)) else { return };
        let (from_y, to_y) = (plan.pill_mid(from), plan.pill_mid(to));
        let (from_colour, to_colour) = (plan.colour(&plan.groups[from].0), plan.colour(&plan.groups[to].0));
        let liquid = matches!(plan.cfg.style.as_str(), "liquid" | "aurora" | "neon");
        let seconds = speed_times(&plan.cfg.speed).0;
        // the new files show at once; the panel carries them to their place
        self.show_rail(plan);
        self.sync_settings(plan);
        let began = Instant::now();
        loop {
            let t = if seconds > 0.0 { (began.elapsed().as_secs_f32() / seconds).min(1.0) } else { 1.0 };
            let glide = t * t * (3.0 - 2.0 * t);
            // liquid arrives with a wobble, width and height settling out of step
            let (shape_w, shape_h) = if liquid && t < 1.0 { jelly(t) } else { (glide, glide) };
            let r = Rect { x: mix(old.x, new.x, glide), y: mix(old.y, new.y, glide), w: mix(old.w, new.w, shape_w), h: mix(old.h, new.h, shape_h) };
            let r = if plan.cfg.right_side { Rect { x: mix(old.x + old.w, new.x + new.w, glide) - r.w, ..r } } else { r };
            layers::set_accent(blend(from_colour, to_colour, glide));
            self.draw_open_from(plan, mix(from_y, to_y, glide), r, 1.0);
            if t >= 1.0 {
                break;
            }
            if DwmFlush() != 0 {
                std::thread::sleep(Duration::from_millis(8)); // no composition, so pace by hand
            }
        }
    }

    /// A file was opened from a section that opened by hover: let it fold when the mouse leaves.
    fn back_to_hover(&self) {
        let by_hover = OPENED_BY_HOVER.lock().unwrap().clone();
        if by_hover.is_some() && by_hover == self.open {
            *HOVER_SECTION.lock().unwrap() = by_hover;
        }
    }

    /// The mouse has left a section that opened by resting on its label: fold it away, and open it
    /// again if the mouse comes back before the fold is done.
    unsafe fn hover_fold(&mut self) -> Result<(), String> {
        let plan = self.current_plan()?;
        let Some(name) = self.open.clone() else { return Ok(()) };
        let (Some(at), Some(panel)) = (plan.index(&name), *PANEL_ON_SCREEN.lock().unwrap()) else { return self.close_at_once() };
        let (rail, label) = {
            let labels = LABELS.lock().unwrap();
            let Some(label) = labels.iter().find(|l| l.name == name).map(|l| l.rect) else { drop(labels); return self.close_at_once() };
            let mut rail = label;
            for l in labels.iter() {
                rail.top = rail.top.min(l.rect.top);
                rail.bottom = rail.bottom.max(l.rect.bottom);
            }
            (rail, label)
        };
        self.fold_watch = Some((rail, label, panel));
        self.close_open(&plan, true);
        self.fold_watch = None;
        if let Some(start) = self.fold_back.take() {
            self.open = Some(name.clone());
            *HOVER_SECTION.lock().unwrap() = Some(name.clone());
            *OPENED_BY_HOVER.lock().unwrap() = Some(name);
            self.show_rail(&plan);
            self.sync_settings(&plan);
            self.flow(&plan, at, true, start);
        }
        let _ = std::fs::write(open_file(), self.open.as_deref().unwrap_or(""));
        self.settle()
    }

    /// Close with no folding away, for when the mouse has simply left.
    unsafe fn close_at_once(&mut self) -> Result<(), String> {
        let plan = self.current_plan()?;
        self.close_open(&plan, false);
        let _ = std::fs::write(open_file(), "");
        self.settle()
    }

    unsafe fn close(&mut self) -> Result<(), String> {
        if self.open.is_none() {
            return Ok(());
        }
        let plan = self.current_plan()?;
        self.close_open(&plan, true);
        let _ = std::fs::write(open_file(), "");
        self.settle()
    }

    unsafe fn hover(&mut self, name: Option<String>) {
        self.hover = name;
        if let Ok(mut pills) = layers::PILLS.lock() {
            for p in pills.iter_mut() {
                p.hover = self.hover.as_deref() == Some(p.name.as_str());
            }
        }
        layers::repaint_rail();
    }

    /// An icon was dragged onto a label: pin the selected icons to that section.
    unsafe fn file_selected(&mut self, section: String) -> Result<(), String> {
        let portal = self.last_plan.as_ref().is_some_and(|p| p.portals.contains_key(&section));
        if section == RECENT || section == SETTINGS || portal {
            return self.settle(); // these are worked out, not filed into
        }
        let mut text = std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?;
        for i in self.desk.selected() {
            text = add_name_rule(&text, &self.desk.text(i), &section);
        }
        parse_config(&text)?; // never write a rules file that would not load
        std::fs::write(rules_file(), text).map_err(|e| e.to_string())?;
        self.settle()
    }

    /// The user answered a question.
    unsafe fn answered(&mut self, question: Ask, answer: String) -> Result<(), String> {
        let answer = answer.trim().to_string();
        let name_taken = |plan: Option<&Plan>, name: &str| plan.is_some_and(|p| p.cfg.order.iter().any(|s| s == name) || p.cfg.portals.iter().any(|(s, _)| s == name));
        let refuse = |name: &str| alert(&format!("\"{name}\" cannot be used as a name. It may be taken, or contain = , [ ] # or ;"));
        match question {
            Ask::RenameSection(old) => {
                if answer == old {
                    return Ok(());
                }
                if !valid_section_name(&answer) || name_taken(self.last_plan.as_ref(), &answer) {
                    refuse(&answer);
                    return Ok(());
                }
                self.edit_file(|t| rename_section(t, &old, &answer))
            }
            Ask::AddSection => {
                if !valid_section_name(&answer) || name_taken(self.last_plan.as_ref(), &answer) {
                    refuse(&answer);
                    return Ok(());
                }
                self.edit_file(|t| add_section(t, &answer))
            }
            Ask::NamePortal(folder) => {
                if !valid_section_name(&answer) || name_taken(self.last_plan.as_ref(), &answer) {
                    refuse(&answer);
                    return Ok(());
                }
                self.edit_file(|t| add_portal(t, &answer, &folder))
            }
            Ask::AddRule => {
                let Some((pattern, section)) = answer.rsplit_once('=').map(|(p, s)| (p.trim().to_string(), s.trim().to_string())) else {
                    alert("Write a rule as: pattern = section. For example: *report* = Word");
                    return Ok(());
                };
                let known = self.last_plan.as_ref().is_some_and(|p| p.cfg.order.contains(&section));
                if !valid_pattern(&pattern) || !known {
                    alert("That rule needs a working pattern and the name of an existing section.");
                    return Ok(());
                }
                self.edit_file(|t| add_name_rule(t, &pattern, &section))
            }
            Ask::SaveLayout => {
                if !shellmenu::valid_name(&answer) {
                    alert("That name is not allowed for a layout.");
                    return Ok(());
                }
                let _ = std::fs::create_dir_all(layouts_dir());
                std::fs::copy(rules_file(), layout_file(&answer)).map_err(|e| e.to_string())?;
                let _ = std::fs::write(current_layout_file(), &answer);
                self.settle()
            }
        }
    }

    /// The find shortcut: with the panel over everything if the sections are not in view.
    unsafe fn begin_find(&mut self) -> Result<(), String> {
        if self.last_plan.as_ref().is_some_and(|p| p.cfg.own_icons) {
            layers::set_peek(true);
        }
        self.rail_hidden = false;
        self.query.clear();
        self.find_from = Some(self.open.clone().unwrap_or_default());
        self.open = Some(FIND.to_string());
        self.take_keys();
        self.last_plan = None;
        self.settle()
    }

    unsafe fn end_peek(&mut self) {
        layers::set_peek(false);
        self.shortcut_showing = false;
        let _ = self.close();
        PostMessageW(self.main_window as HWND, WM_GIVE_BACK_KEYS, 0, 0);
    }

    /// Icons Rivulet drew were dragged onto a label: pin them to that section.
    unsafe fn file_items(&mut self, labels: Vec<String>, section: String) -> Result<(), String> {
        let is_portal = |name: &str| self.last_plan.as_ref().is_some_and(|p| p.portals.contains_key(name));
        if section == RECENT || section == SETTINGS || is_portal(&section) || self.open.as_deref().is_some_and(is_portal) || self.open.as_deref() == Some(section.as_str()) {
            return Ok(());
        }
        let mut text = std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?;
        for label in &labels {
            text = add_name_rule(&text, label, &section);
        }
        parse_config(&text)?; // never write a rules file that would not load
        std::fs::write(rules_file(), text).map_err(|e| e.to_string())?;
        self.selected.clear();
        self.last_plan = None;
        self.settle()
    }

    /// Draw the open panel again, for a change of hover or selection.
    unsafe fn repaint_panel(&self) {
        let (Some(plan), Some(open)) = (&self.last_plan, &self.open) else { return };
        let Some(at) = plan.index(open) else { return };
        if let Some((r, _)) = plan.panel(at) {
            self.draw_open(plan, at, r, 1.0);
        }
    }

    /// Make these the chosen files, drawing the change if there is one.
    unsafe fn select(&mut self, chosen: Vec<i32>) {
        let paths = {
            let items = layers::SETTINGS_ITEMS.lock().unwrap();
            chosen.iter().filter_map(|&n| match items.get(n as usize).map(|i| &i.action) {
                Some(Some(Action::Open(path))) => Some(path.clone()),
                _ => None,
            }).collect()
        };
        self.selected = paths;
        let changed = {
            let mut current = layers::SELECTED_ITEMS.lock().unwrap();
            let changed = *current != chosen;
            *current = chosen;
            changed
        };
        if changed {
            self.repaint_panel();
        }
    }

    fn chosen(&self) -> Vec<i32> {
        layers::SELECTED_ITEMS.lock().unwrap().clone()
    }

    /// The file at this spot in the panel, and its place in the list.
    fn file_at(&self, pt: POINT) -> Option<(i32, PathBuf)> {
        let panel = (*PANEL_ON_SCREEN.lock().unwrap())?;
        let items = layers::SETTINGS_ITEMS.lock().unwrap();
        settings::item_at(&items, pt.x - panel.left, pt.y - panel.top).and_then(|n| match &items[n].action {
            Some(Action::Open(path)) => Some((n as i32, path.clone())),
            _ => None,
        })
    }

    /// The real right-click menu: for all the chosen files if this is one of them, else just this one.
    unsafe fn file_menu(&mut self, pt: POINT) {
        if let Some((n, path)) = self.file_at(pt) {
            if !self.chosen().contains(&n) {
                self.select(vec![n]);
                self.anchor = n;
            }
            let mut paths = vec![path.clone()];
            paths.extend(self.selected.iter().filter(|p| **p != path).cloned());
            self.take_keys();
            shellmenu::show(paths);
        }
    }

    /// Keys go to Rivulet from now until another window is used, rather than to whatever was in front.
    fn take_keys(&self) {
        unsafe { PostMessageW(self.main_window as HWND, WM_TAKE_KEYS, 0, 0) };
    }

    /// The selection box moved: choose what it touches.
    unsafe fn select_box(&mut self) {
        BOX_WAITING.store(false, Ordering::Relaxed);
        let area = *layers::SELECT_BOX.lock().unwrap();
        if let Some(area) = area {
            let chosen = settings::in_box(&layers::SETTINGS_ITEMS.lock().unwrap(), area);
            self.select(chosen);
        }
        self.repaint_panel(); // the box itself moved, or has gone
    }

    /// How many rows show at once.
    fn grid_rows(&self) -> Option<i32> {
        let plan = self.last_plan.as_ref()?;
        let at = plan.index(self.open.as_deref()?)?;
        plan.file_layout(at).map(|(_, g)| g.visible_rows)
    }

    /// How many files there are, and how many fit across, for the arrow keys.
    fn grid(&self) -> Option<(i32, i32)> {
        let plan = self.last_plan.as_ref()?;
        let at = plan.index(self.open.as_deref()?)?;
        let across = plan.panel(at)?.1;
        let count = layers::SETTINGS_ITEMS.lock().unwrap().iter().filter(|i| matches!(i.look, Look::File(..))).count() as i32;
        Some((across, count))
    }

    /// A key, while Rivulet has them. Only files are acted on, and only ones the user chose.
    unsafe fn key(&mut self, key: u32, ctrl: bool, shift: bool) -> Result<(), String> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
        let finding_now = self.open.as_deref() == Some(FIND);
        if self.open.is_none() || (!finding_now && !layers::SETTINGS_ITEMS.lock().unwrap().iter().any(|i| matches!(i.look, Look::File(..)))) {
            return Ok(());
        }
        // portals put a title and a button ahead of their files
        let first = layers::SETTINGS_ITEMS.lock().unwrap().iter().position(|i| matches!(i.look, Look::File(..))).unwrap_or(0) as i32;
        let paths = self.selected.clone();
        let vk = key as u16;
        let finding = self.open.as_deref() == Some(FIND);
        if !ctrl {
            let letter = match vk {
                0x41..=0x5A => Some((vk as u8 as char).to_ascii_lowercase()),
                0x30..=0x39 => Some(vk as u8 as char),
                VK_SPACE => Some(' '),
                VK_OEM_MINUS => Some(if shift { '_' } else { '-' }),
                VK_OEM_PERIOD => Some('.'),
                _ => None,
            };
            if let Some(c) = letter {
                return self.find_type(Some(c));
            }
            if vk == VK_BACK && finding {
                if self.query.is_empty() { return self.find_end() }
                return self.find_type(None);
            }
        }
        if vk == VK_ESCAPE && finding {
            return self.find_end();
        }
        if vk == VK_ESCAPE && layers::peeking() && paths.is_empty() {
            self.end_peek();
            return Ok(());
        }
        match vk {
            VK_PRIOR | VK_NEXT => {
                let page = self.grid_rows().unwrap_or(1).max(1);
                self.scroll_panel(if vk == VK_PRIOR { -page } else { page });
            }
            VK_DELETE if !paths.is_empty() => shellmenu::verb(paths, "delete", shift),
            VK_F2 if paths.len() == 1 => {
                let at = self.chosen().first().copied().unwrap_or(-1);
                let spot = {
                    let items = layers::SETTINGS_ITEMS.lock().unwrap();
                    let panel = *PANEL_ON_SCREEN.lock().unwrap();
                    match (items.get(at as usize), panel) {
                        (Some(i), Some(p)) => POINT { x: p.left + i.rect.left, y: p.top + i.rect.bottom },
                        _ => POINT { x: 0, y: 0 },
                    }
                };
                shellmenu::rename(paths[0].clone(), spot);
            }
            VK_F5 => return self.settle(),
            VK_RETURN => {
                for path in &paths {
                    open_path(path);
                }
                self.back_to_hover();
            }
            VK_ESCAPE => {
                if paths.is_empty() {
                    return self.close();
                }
                self.select(vec![]);
            }
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN => {
                if let Some((across, count)) = self.grid() {
                    let way = match vk {
                        VK_LEFT => Arrow::Left,
                        VK_RIGHT => Arrow::Right,
                        VK_UP => Arrow::Up,
                        _ => Arrow::Down,
                    };
                    let at = self.chosen().last().copied().unwrap_or(first) - first;
                    let to = if self.selected.is_empty() { 0 } else { settings::arrow(at, way, across, count) } + first;
                    // bring the file into view first, as scrolling makes the list again
                    if let (Some(name), Some(rows)) = (self.open.clone(), self.grid_rows()) {
                        let scroll = self.scroll.get(&name).copied().unwrap_or(0);
                        self.scroll.insert(name, settings::scroll_to_show(to - first, across, rows, scroll));
                        self.refresh_panel();
                    }
                    self.select(vec![to]);
                    self.anchor = to;
                }
            }
            0x41 if ctrl => {
                let all: Vec<i32> = layers::SETTINGS_ITEMS.lock().unwrap().iter().enumerate().filter(|(_, i)| matches!(i.action, Some(Action::Open(_))) && matches!(i.look, Look::File(..))).map(|(n, _)| n as i32).collect();
                self.select(all);
            }
            0x43 if ctrl && !paths.is_empty() => shellmenu::verb(paths, "copy", false),
            0x58 if ctrl && !paths.is_empty() => shellmenu::verb(paths, "cut", false),
            0x56 if ctrl => shellmenu::paste(),
            _ => {}
        }
        Ok(())
    }

    /// A click inside the Settings panel.
    unsafe fn settings_click(&mut self, pt: POINT, ctrl: bool, shift: bool) -> Result<(), String> {
        let Some(panel) = *PANEL_ON_SCREEN.lock().unwrap() else { return Ok(()) };
        let (action, index) = {
            let items = layers::SETTINGS_ITEMS.lock().unwrap();
            let (x, y) = (pt.x - panel.left, pt.y - panel.top);
            (settings::action_at(&items, x, y), settings::item_at(&items, x, y).map_or(-1, |n| n as i32))
        };
        match action {
            Some(Action::Set(part, key, value)) => {
                let text = set_setting(&std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?, part, key, &value);
                parse_config(&text)?; // never write a rules file that would not load
                std::fs::write(rules_file(), text).map_err(|e| e.to_string())?;
                self.last_plan = None;
                self.settle()
            }
            Some(Action::Login(on)) => {
                if let Err(e) = if on { install_login() } else { remove_login() } {
                    alert(&e);
                }
                self.login = login_is_on();
                self.settle()
            }
            Some(Action::EditRules) => {
                edit_rules();
                Ok(())
            }
            Some(Action::Resort) => self.settle(),
            Some(Action::Undo) => {
                run_undo();
                Ok(())
            }
            Some(Action::Stop) => {
                send(Command::Stop);
                Ok(())
            }
            Some(Action::Open(path)) => {
                self.take_keys();
                self.select(settings::pick(&self.chosen(), index, self.anchor, ctrl, shift));
                if !shift {
                    self.anchor = index;
                }
                if ctrl || shift {
                    self.last_file_click = None;
                    return Ok(());
                }
                let limit = Duration::from_millis(GetDoubleClickTime() as u64);
                if self.last_file_click.take().is_some_and(|(t, p)| t.elapsed() <= limit && p == path) {
                    let portal = self.open.clone().filter(|o| self.last_plan.as_ref().is_some_and(|p| p.portals.contains_key(o)));
                    return match portal {
                        Some(section) if path.is_dir() => self.navigate(&section, path),
                        _ => {
                            open_path(&path);
                            self.back_to_hover();
                            if layers::peeking() {
                                self.end_peek();
                            }
                            Ok(())
                        }
                    };
                } else {
                    self.last_file_click = Some((Instant::now(), path));
                }
                Ok(())
            }
            Some(Action::OpenFolder(path)) => {
                open_path(&path);
                Ok(())
            }
            Some(Action::Page(page)) => {
                settings::PAGE.store(page, Ordering::Relaxed);
                self.settle()
            }
            Some(Action::CycleSort) => {
                let Some(name) = self.open.clone() else { return Ok(()) };
                let now = self.last_plan.as_ref().and_then(|p| p.cfg.sort.iter().find(|(s, _)| *s == name).map(|(_, h)| h.clone())).unwrap_or("newest".into());
                self.select(vec![]);
                self.edit_file(|t| set_setting(t, "sort", &name, next_sort(&now)))
            }
            Some(Action::ToggleView) => {
                let Some(name) = self.open.clone() else { return Ok(()) };
                let list = self.last_plan.as_ref().is_some_and(|p| p.is_list(&name));
                self.select(vec![]);
                self.scroll.remove(&name);
                self.edit_file(|t| set_setting(t, "view", &name, if list { "grid" } else { "list" }))
            }
            Some(Action::TogglePin) => {
                let Some(name) = self.open.clone() else { return Ok(()) };
                let mut pinned = self.last_plan.as_ref().map_or(vec![], |p| p.cfg.pinned.clone());
                if pinned.contains(&name) { pinned.retain(|p| *p != name) } else { pinned.push(name) }
                self.edit_file(|t| set_setting(t, "general", "pinned", &pinned.join(", ")))
            }
            Some(Action::Back) => {
                let Some(name) = self.open.clone() else { return Ok(()) };
                if let Some(stack) = self.nav.get_mut(&name) {
                    stack.pop();
                }
                self.scroll.remove(&name);
                self.select(vec![]);
                self.last_plan = None;
                self.settle()
            }
            Some(Action::Up) => {
                let Some(name) = self.open.clone() else { return Ok(()) };
                let parent = self.last_plan.as_ref().and_then(|p| p.portals.get(&name)).and_then(|p| p.folder.parent().map(Path::to_path_buf));
                match parent {
                    Some(folder) => self.navigate(&name, folder),
                    None => Ok(()),
                }
            }
            Some(Action::Show(name)) => {
                if self.open.as_deref() == Some(name.as_str()) { Ok(()) } else { self.toggle(name) }
            }
            Some(Action::PickColour(key)) => {
                let cfg = self.last_plan.as_ref().map(|p| &p.cfg);
                let start = cfg.and_then(|c| match key.as_str() {
                    "panel_colour" => c.panel_colour,
                    "text_colour" => c.text_colour,
                    "border_colour" => c.border_colour,
                    "tint_colour" => c.tint_colour,
                    other => other.strip_prefix("section:").and_then(|n| c.colours.iter().find(|(s, _)| s == n).map(|(_, v)| *v)),
                });
                shellmenu::pick_colour(key, start.unwrap_or(0x00C8B090));
                Ok(())
            }
            Some(Action::ClearLook(keys)) => self.edit_file(|t| keys.iter().fold(t.to_string(), |t, k| remove_setting(&t, "general", k))),
            Some(Action::PickSymbol(name)) => {
                *settings::SYMBOL_FOR.lock().unwrap() = name;
                settings::PAGE.store(5, Ordering::Relaxed);
                self.settle()
            }
            Some(Action::SetSymbol(name, code)) => {
                settings::PAGE.store(2, Ordering::Relaxed);
                self.edit_file(|t| set_setting(t, "symbols", &name, &format!("{code:04X}")))
            }
            Some(Action::ClearSymbol(name)) => {
                settings::PAGE.store(2, Ordering::Relaxed);
                self.edit_file(|t| remove_setting(t, "symbols", &name))
            }
            Some(Action::RenameSection(name)) => {
                shellmenu::ask(Ask::RenameSection(name.clone()), "New name for this section", name);
                Ok(())
            }
            Some(Action::MoveSection(name, by)) => self.edit_file(|t| move_section(t, &name, by)),
            Some(Action::RemoveSection(name)) => self.edit_file(|t| remove_section(t, &name)),
            Some(Action::ToggleTab(name)) => self.edit_file(|t| toggle_tab(t, &name)),
            Some(Action::AddSection) => {
                shellmenu::ask(Ask::AddSection, "Name of the new section", String::new());
                Ok(())
            }
            Some(Action::AddPortal) => {
                shellmenu::pick_folder();
                Ok(())
            }
            Some(Action::AddRule) => {
                shellmenu::ask(Ask::AddRule, "New rule: pattern = section", String::new());
                Ok(())
            }
            Some(Action::RemoveRule(pattern)) => self.edit_file(|t| remove_setting(t, "names", &pattern)),
            Some(Action::SaveLayout) => {
                shellmenu::ask(Ask::SaveLayout, "Name for this layout", String::new());
                Ok(())
            }
            Some(Action::LoadLayout(name)) => self.switch_layout(&name),
            Some(Action::RemoveLayout(name)) => {
                let _ = std::fs::remove_file(layout_file(&name));
                self.settle()
            }
            None => {
                self.take_keys();
                if !ctrl {
                    self.select(vec![]);
                }
                Ok(())
            }
        }
    }

    /// Double-click on empty desktop: hide the labels, or bring them back.
    unsafe fn toggle_rail(&mut self) -> Result<(), String> {
        if !self.rail_hidden {
            self.close()?;
        }
        self.rail_hidden = !self.rail_hidden;
        self.settle()
    }

    /// Cheap backstop check, in case Windows did not report a change.
    unsafe fn needs_settle(&self) -> bool {
        if !self.desk.alive() || !layers::alive() || self.desk.count() != self.count || !self.tinted_for.ends_with(&wallpaper::current()) {
            return true;
        }
        let width = self.desk.width();
        self.hidden.iter().any(|&i| self.desk.position(i).0 < width)
    }

    /// Every icon back in view at Windows' usual spacing, so stopping never strands anything.
    unsafe fn lay_all_out(&self) {
        restore_spacing(&self.desk);
        let Ok(plan) = load_plan(&self.desk) else { return };
        let (w, h) = self.desk.spacing();
        let rows = (plan.room.1 / h.max(1)).max(1);
        let (x0, y0) = (plan.view_origin.0 + plan.m.gap, plan.view_origin.1 + plan.m.gap);
        for i in 0..plan.labels.len() {
            let n = i as i32;
            self.desk.place(i, x0 + (n / rows) * w, y0 + (n % rows) * h);
        }
    }
}

/// Open a file or folder the way double-clicking it would.
unsafe fn open_path(path: &Path) {
    ShellExecuteW(std::ptr::null_mut(), wide("open").as_ptr(), wide(&path.to_string_lossy()).as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
}

/// Windows' own icon width, kept so it can be put back.
unsafe fn remember_spacing(desk: &Desktop) {
    if !spacing_file().exists() {
        let (w, h) = desk.spacing();
        let _ = std::fs::write(spacing_file(), format!("{w} {h}"));
    }
}

unsafe fn restore_spacing(desk: &Desktop) {
    let Ok(text) = std::fs::read_to_string(spacing_file()) else { return };
    let parts: Vec<i32> = text.split_whitespace().filter_map(|p| p.parse().ok()).collect();
    if let [w, h] = parts[..] {
        desk.set_spacing(w, h);
    }
    let _ = std::fs::remove_file(spacing_file());
}

fn worker(desk: Desktop, open: Option<String>, rx: std::sync::mpsc::Receiver<Command>, window: usize) {
    unsafe {
        let last_section = open.clone();
        let mut s = Sections { desk, open, hover: None, hidden: vec![], count: 0, seen: None, last_plan: None, last_section, shortcut_showing: false, rail_hidden: false, login: login_is_on(), tinted_for: String::from("\0"), main_window: window, icons: HashMap::new(), last_file_click: None, fold_watch: None, fold_back: None, selected: vec![], anchor: -1, scroll: HashMap::new(), nav: HashMap::new(), query: String::new(), find_from: None, screens_seen: String::new(), last_near: Instant::now(), faded: false };
        CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32); // file icons and opening files ask for it
        if let Err(e) = s.settle() {
            log(&format!("first sort failed: {e}"));
            alert(&e);
            PostMessageW(window as HWND, WM_CLOSE, 0, 0);
            return;
        }
        log("running");
        let mut last_check = Instant::now();
        loop {
            // Aurora's colours drift about 25 times a second, and only while it is worth seeing
            let wait = if pretty::drifting() { Duration::from_millis(33) } else { CHECK_EVERY };
            let result = match rx.recv_timeout(wait) {
                Ok(Command::Toggle(name)) => s.toggle(name),
                Ok(Command::Close) => s.close(),
                Ok(Command::Hover(name)) => {
                    s.hover(name);
                    Ok(())
                }
                Ok(Command::Filed(section)) => s.file_selected(section),
                Ok(Command::SettingsClick(pt, ctrl, shift)) => s.settings_click(pt, ctrl, shift),
                Ok(Command::FileRelease(pt)) => {
                    // a plain click on one of several, once it is clear it was not a drag
                    if let Some((n, _)) = s.file_at(pt) {
                        if s.chosen().len() > 1 && s.chosen().contains(&n) {
                            s.select(vec![n]);
                        }
                    }
                    Ok(())
                }
                Ok(Command::Scroll(rows)) => {
                    s.scroll_panel(rows);
                    Ok(())
                }
                Ok(Command::Answered(question, answer)) => s.answered(question, answer),
                Ok(Command::ColourChosen(key, hex)) => match key.strip_prefix("section:") {
                    Some(name) => {
                        let name = name.to_string();
                        s.edit_file(|t| set_setting(t, "colours", &name, &hex))
                    }
                    None => s.edit_file(|t| set_setting(t, "general", &key, &hex)),
                },
                Ok(Command::FolderChosen(folder)) => {
                    let suggested = Path::new(&folder).file_name().map_or("Folder".to_string(), |n| n.to_string_lossy().into_owned());
                    shellmenu::ask(Ask::NamePortal(folder), "Name for this folder's label", suggested);
                    Ok(())
                }
                Ok(Command::Find) => s.begin_find(),
                Ok(Command::Workspace(by)) => {
                    let names = layout_names();
                    if names.is_empty() {
                        Ok(())
                    } else {
                        let current = std::fs::read_to_string(current_layout_file()).unwrap_or_default();
                        let at = names.iter().position(|n| *n == current.trim()).map_or(0, |a| (a as i32 + by).rem_euclid(names.len() as i32) as usize);
                        s.switch_layout(&names[at])
                    }
                }
                Ok(Command::FadeIn) => {
                    s.last_near = Instant::now();
                    if s.faded {
                        s.fade_rail(100);
                    }
                    Ok(())
                }
                Ok(Command::PeekOff) => {
                    s.end_peek();
                    Ok(())
                }
                Ok(Command::HoverOpen(name)) => {
                    if s.open.as_deref() == Some(name.as_str()) {
                        Ok(())
                    } else {
                        let opened = s.toggle(name.clone());
                        *HOVER_SECTION.lock().unwrap() = Some(name.clone());
                        *OPENED_BY_HOVER.lock().unwrap() = Some(name);
                        opened
                    }
                }
                Ok(Command::HoverClose) => {
                    LEAVING.store(false, Ordering::Relaxed);
                    // a menu or a box asking something belongs to this section, and Peek has its own way out
                    let pinned = s.open.as_ref().is_some_and(|o| s.last_plan.as_ref().is_some_and(|p| p.cfg.pinned.contains(o)));
                    let still = HOVER_SECTION.lock().unwrap().is_some() && HOVER_SECTION.lock().unwrap().as_deref() == s.open.as_deref();
                    if still && !pinned && !shellmenu::SHELL_BUSY.load(Ordering::Relaxed) && !layers::peeking() {
                        s.hover_fold()
                    } else {
                        Ok(())
                    }
                }
                Ok(Command::SelectBox) => {
                    s.select_box();
                    Ok(())
                }
                Ok(Command::Key(key, ctrl, shift)) => s.key(key, ctrl, shift),
                Ok(Command::PanelRepaint) => {
                    s.repaint_panel();
                    Ok(())
                }
                Ok(Command::FileMenu(pt)) => {
                    s.file_menu(pt);
                    Ok(())
                }
                Ok(Command::FiledItems(labels, section)) => s.file_items(labels, section),
                Ok(Command::ToggleRail(pt)) => {
                    if s.desk.icon_at(pt) {
                        Ok(()) // a double-click on an icon opens it; it is not a click on empty desktop
                    } else {
                        s.toggle_rail()
                    }
                }
                Ok(Command::Shortcut) => s.shortcut(),
                Ok(Command::Changed) => {
                    // a refresh arrives as a burst of changes, so let it finish first
                    std::thread::sleep(Duration::from_millis(30));
                    CHANGE_WAITING.store(false, Ordering::Relaxed);
                    s.settle()
                }
                Ok(Command::Retidy) => s.settle(),
                Ok(Command::Light) => {
                    let mut pt = POINT { x: 0, y: 0 };
                    GetCursorPos(&mut pt);
                    pretty::move_light(over_desktop(pt));
                    Ok(())
                }
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    s.lay_all_out();
                    PostMessageW(window as HWND, WM_CLOSE, 0, 0);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    // the labels fade once they have sat unused for a while, with nothing open
                    let fade_on = FADE_ON.load(Ordering::Relaxed);
                    if fade_on && !s.faded && s.open.is_none() && s.last_near.elapsed() > Duration::from_secs(4) {
                        s.fade_rail(30);
                    } else if !fade_on && s.faded {
                        s.fade_rail(100);
                    }
                    if pretty::drifting() {
                        let mut pt = POINT { x: 0, y: 0 };
                        GetCursorPos(&mut pt);
                        pretty::move_light(over_desktop(pt));
                    }
                    if last_check.elapsed() >= CHECK_EVERY {
                        last_check = Instant::now();
                        if s.needs_settle() {
                            s.settle()
                        } else {
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
                }
            };
            if let Err(e) = result {
                eprintln!("{e}"); // a passing problem, such as Explorer restarting; the next check retries
            }
        }
    }
}

// ---------- clicks on labels ----------

static DRAG_FROM_PANEL: AtomicBool = AtomicBool::new(false);
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);
static CLICK_AWAY: AtomicBool = AtomicBool::new(true);
static DOUBLE_CLICK_HIDES: AtomicBool = AtomicBool::new(true);
static SWALLOW_RELEASE: AtomicBool = AtomicBool::new(false);
static LAST_EMPTY_PRESS: Mutex<Option<(Instant, POINT)>> = Mutex::new(None);
static LAST_TOGGLE: Mutex<Option<Instant>> = Mutex::new(None);
static HOVERING: Mutex<Option<String>> = Mutex::new(None);
static RIGHT_ON_LABEL: AtomicBool = AtomicBool::new(false);
static RIGHT_ON_FILE: AtomicBool = AtomicBool::new(false);
/// What a press on a file may drag: the names, for filing under a label, and the files.
static DRAG_ITEMS: Mutex<Option<(Vec<String>, Vec<PathBuf>)>> = Mutex::new(None);
static BOX_FROM: Mutex<Option<POINT>> = Mutex::new(None);
static BOX_WAITING: AtomicBool = AtomicBool::new(false);
static PLAIN_PRESS: AtomicBool = AtomicBool::new(false);
static HOVER_OPENS: AtomicBool = AtomicBool::new(false);
/// The section open only because the mouse rested on its label. It closes when the mouse leaves,
/// and a click on its label or inside it makes it stay.
static HOVER_SECTION: Mutex<Option<String>> = Mutex::new(None);
/// The section that opened by resting on its label and was not since made to stay by a click on
/// its label; opening a file from it hands it back to hover, so it folds once the mouse leaves.
static OPENED_BY_HOVER: Mutex<Option<String>> = Mutex::new(None);
static LEAVING: AtomicBool = AtomicBool::new(false);
static FADE_NUDGED: AtomicBool = AtomicBool::new(false);
static PEEK_SHORTCUT: AtomicBool = AtomicBool::new(false);
static FADE_ON: AtomicBool = AtomicBool::new(false);
static OWNER: AtomicUsize = AtomicUsize::new(0);

unsafe fn over_desktop(pt: POINT) -> bool {
    let mut name = [0u16; 32];
    let n = GetClassNameW(WindowFromPoint(pt), name.as_mut_ptr(), 32).max(0) as usize;
    let class = String::from_utf16_lossy(&name[..n]);
    matches!(class.as_str(), "SysListView32" | "SHELLDLL_DefView" | "WorkerW" | "Progman" | "DesktopSectionsLayer")
}

fn label_at(pt: POINT) -> Option<String> {
    let labels = LABELS.try_lock().ok()?;
    let inside = |r: &RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
    labels.iter().find(|l| inside(&l.rect)).map(|l| l.name.clone())
}

fn in_panel(pt: POINT) -> bool {
    PANEL_ON_SCREEN.try_lock().is_ok_and(|p| p.is_some_and(|r| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom))
}

/// The drawn file under the mouse: its place in the list and its name, or None. The mouse hook calls
/// this, so it never waits and never asks Explorer anything. Outer None means busy, so try next time.
unsafe fn file_under(pt: POINT) -> Option<Option<(i32, String, isize)>> {
    if !SETTINGS_OPEN.load(Ordering::Relaxed) || !in_panel(pt) || !over_desktop(pt) {
        return Some(None);
    }
    let panel = (*PANEL_ON_SCREEN.try_lock().ok()?)?;
    let items = layers::SETTINGS_ITEMS.try_lock().ok()?;
    Some(settings::item_at(&items, pt.x - panel.left, pt.y - panel.top).and_then(|n| match &items[n].look {
        Look::File(name, icon) => Some((n as i32, name.clone(), *icon)),
        _ => None,
    }))
}

fn panel_has_files() -> bool {
    layers::SETTINGS_ITEMS.try_lock().is_ok_and(|items| items.iter().any(|i| matches!(i.look, Look::File(..))))
}

/// A drag from this file takes all the chosen files if it is one of them, and otherwise only itself.
fn what_a_drag_takes(n: i32, name: String) -> Option<(Vec<String>, Vec<PathBuf>)> {
    let items = layers::SETTINGS_ITEMS.try_lock().ok()?;
    let chosen = layers::SELECTED_ITEMS.try_lock().ok()?;
    let take: Vec<usize> = if chosen.contains(&n) { chosen.iter().map(|&c| c as usize).collect() } else { vec![n as usize] };
    let (mut names, mut paths) = (vec![], vec![]);
    // the pressed one first, so its folder decides what can go together
    for i in std::iter::once(n as usize).chain(take.into_iter().filter(|&c| c != n as usize)) {
        if let (Some(Look::File(file, _)), Some(Some(Action::Open(path)))) = (items.get(i).map(|x| &x.look), items.get(i).map(|x| &x.action)) {
            names.push(file.clone());
            paths.push(path.clone());
        }
    }
    let _ = name;
    Some((names, paths))
}

/// The selection box follows the mouse, kept inside the panel.
fn box_moved(pt: POINT) {
    let Ok(from) = BOX_FROM.try_lock() else { return };
    let Some(from) = *from else { return };
    let Some(panel) = PANEL_ON_SCREEN.try_lock().ok().and_then(|p| *p) else { return };
    if (from.x - pt.x).abs() < 4 && (from.y - pt.y).abs() < 4 {
        return;
    }
    let clamp = |v: i32, low: i32, high: i32| v.max(low).min(high);
    let (x, y) = (clamp(pt.x, panel.left, panel.right), clamp(pt.y, panel.top, panel.bottom));
    let area = RECT { left: from.x.min(x) - panel.left, top: from.y.min(y) - panel.top, right: from.x.max(x) - panel.left, bottom: from.y.max(y) - panel.top };
    if let Ok(mut shown) = layers::SELECT_BOX.try_lock() {
        *shown = Some(area);
    }
    if !BOX_WAITING.swap(true, Ordering::Relaxed) {
        send(Command::SelectBox);
    }
}

/// The button came up: end a selection box, if one was started. True if one had been drawn.
fn box_ended() -> bool {
    if let Ok(mut from) = BOX_FROM.try_lock() {
        *from = None;
    }
    let drawn = layers::SELECT_BOX.try_lock().is_ok_and(|mut b| b.take().is_some());
    if drawn {
        send(Command::SelectBox);
    }
    drawn
}

/// Two quick presses in nearly the same spot on empty desktop.
unsafe fn is_double_press(pt: POINT) -> bool {
    let Ok(mut last) = LAST_EMPTY_PRESS.try_lock() else { return false };
    let limit = Duration::from_millis(GetDoubleClickTime() as u64);
    let double = last.is_some_and(|(t, p)| t.elapsed() <= limit && (p.x - pt.x).abs() <= 4 && (p.y - pt.y).abs() <= 4);
    *last = if double { None } else { Some((Instant::now(), pt)) };
    double
}

/// Acts on the press, not the release, so a section starts moving under the finger.
/// Returns true when the desktop should not see this click at all.
unsafe fn left_down(pt: POINT) -> bool {
    DRAG_FROM_PANEL.store(false, Ordering::Relaxed);
    if !over_desktop(pt) {
        return false;
    }
    let hover_section = HOVER_SECTION.try_lock().ok().and_then(|mut h| h.take());
    if let Some(name) = label_at(pt) {
        if hover_section.as_deref() == Some(name.as_str()) {
            if let Ok(mut o) = OPENED_BY_HOVER.try_lock() {
                *o = None;
            }
            return true; // it opened by resting on it; this click keeps it open rather than closing it
        }
        let mut last = LAST_TOGGLE.lock().unwrap();
        // a double-click is one toggle, not two that cancel out
        if last.map_or(true, |t| t.elapsed() > Duration::from_millis(250)) {
            *last = Some(Instant::now());
            send(Command::Toggle(name));
        }
    } else if in_panel(pt) {
        if SETTINGS_OPEN.load(Ordering::Relaxed) {
            // hover_section has been cleared: a click inside means the section is in use
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
            let (ctrl, shift) = (GetAsyncKeyState(VK_CONTROL as i32) < 0, GetAsyncKeyState(VK_SHIFT as i32) < 0);
            PLAIN_PRESS.store(!ctrl && !shift, Ordering::Relaxed);
            match file_under(pt) {
                Some(Some((n, name, icon))) if !ctrl && !shift => {
                    if let (Ok(mut drag), Some(what)) = (DRAG_ITEMS.try_lock(), what_a_drag_takes(n, name)) {
                        *drag = Some(what);
                        ghost::press(pt, icon, settings::icon_pixels(windows_sys::Win32::UI::HiDpi::GetDpiForSystem() as f32 / 96.0));
                    }
                }
                Some(None) if panel_has_files() => {
                    if let Ok(mut from) = BOX_FROM.try_lock() {
                        *from = Some(pt);
                    }
                }
                _ => {}
            }
            send(Command::SettingsClick(pt, ctrl, shift));
            return true; // a control, so no selection box should start on the desktop
        }
        DRAG_FROM_PANEL.store(true, Ordering::Relaxed);
    } else {
        if CLICK_AWAY.load(Ordering::Relaxed) {
            send(Command::Close);
        }
        if DOUBLE_CLICK_HIDES.load(Ordering::Relaxed) && is_double_press(pt) {
            send(Command::ToggleRail(pt));
        }
    }
    false
}

unsafe fn left_up(pt: POINT) {
    if DRAG_FROM_PANEL.swap(false, Ordering::Relaxed) {
        if let Some(section) = label_at(pt) {
            send(Command::Filed(section)); // an icon was dragged onto a label
        }
    }
}

/// Whether the mouse is on the way between a section's parts: the labels, the panel, or the gap
/// between the panel and the label it came from.
fn keeps_open(pt: POINT, rail: RECT, label: RECT, panel: RECT) -> bool {
    let inside = |r: RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
    let (left, right) = if rail.right <= panel.left { (rail.right, panel.left) } else { (panel.right, rail.left) };
    let gap = RECT { left, right, top: label.top.min(panel.top), bottom: label.bottom.max(panel.bottom) };
    inside(rail) || inside(panel) || inside(gap)
}

/// Whether the mouse has left the section that opened by resting on its label. Busy locks count as
/// not having left, as this runs in the mouse hook and another move comes along soon.
fn has_left_hover_section(pt: POINT) -> bool {
    let Some(name) = HOVER_SECTION.try_lock().ok().and_then(|h| h.clone()) else { return false };
    let Some(panel) = PANEL_ON_SCREEN.try_lock().ok().and_then(|p| *p) else { return false };
    let Ok(labels) = LABELS.try_lock() else { return false };
    let Some(label) = labels.iter().find(|l| l.name == name) else { return false };
    let mut rail = label.rect;
    for l in labels.iter() {
        rail.top = rail.top.min(l.rect.top);
        rail.bottom = rail.bottom.max(l.rect.bottom);
    }
    !keeps_open(pt, rail, label.rect, panel)
}

fn mouse_moved(pt: POINT) {
    if has_left_hover_section(pt) && !LEAVING.swap(true, Ordering::Relaxed) {
        // a short grace, so slipping out for a moment or cutting a corner does not close it
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
            let mut now = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut now) };
            if has_left_hover_section(now) {
                send(Command::HoverClose);
            } else {
                LEAVING.store(false, Ordering::Relaxed);
            }
        });
    }
    if FADE_ON.load(Ordering::Relaxed) {
        // near the labels counts as using them; asked only now and then, as this is the mouse hook
        let near = LABELS.try_lock().is_ok_and(|labels| labels.iter().any(|l| pt.x >= l.rect.left - 200 && pt.x <= l.rect.right + 200 && pt.y >= l.rect.top - 60 && pt.y <= l.rect.bottom + 60));
        if near && !FADE_NUDGED.swap(true, Ordering::Relaxed) {
            send(Command::FadeIn);
        } else if !near {
            FADE_NUDGED.store(false, Ordering::Relaxed);
        }
    }
    unsafe {
        ghost::moved(pt);
        // over another program a drag becomes a real Windows drag; on the desktop it is for the labels
        if ghost::dragging() && !over_desktop(pt) {
            if let Some((_, paths)) = DRAG_ITEMS.try_lock().ok().and_then(|mut d| d.take()) {
                ghost::release();
                SWALLOW_RELEASE.store(false, Ordering::Relaxed);
                shellmenu::begin_drag(paths);
                return;
            }
        }
    }
    box_moved(pt);
    if let Some(under) = unsafe { file_under(pt) } {
        let item = under.map_or(-1, |(n, ..)| n);
        if layers::HOVER_ITEM.swap(item, Ordering::Relaxed) != item {
            send(Command::PanelRepaint); // only on moving to another icon, never within one
        }
    }
    let hit = label_at(pt).filter(|_| unsafe { over_desktop(pt) });
    if let Ok(mut hovering) = HOVERING.try_lock() {
        if *hovering != hit {
            *hovering = hit.clone();
            if let Some(name) = hit.clone().filter(|n| HOVER_OPENS.load(Ordering::Relaxed) && n != SETTINGS) {
                // only if the mouse is still resting there, and is not carrying files to file there
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(350));
                    let same = HOVERING.lock().is_ok_and(|h| h.as_deref() == Some(name.as_str()));
                    if same && !ghost::dragging() {
                        send(Command::HoverOpen(name));
                    }
                });
            }
            send(Command::Hover(hit));
        }
    }
}

/// Windows drops a mouse hook that answers slowly, so this only notes the event; the worker does the work.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && shellmenu::SHELL_BUSY.load(Ordering::Relaxed) {
        if wparam as u32 == WM_LBUTTONUP {
            shellmenu::note_release();
        }
    } else if code >= 0 {
        let pt = (*(lparam as *const MSLLHOOKSTRUCT)).pt;
        match wparam as u32 {
            WM_MOUSEMOVE => {
                mouse_moved(pt);
                if pretty::note_mouse(pt.x, pt.y) {
                    send(Command::Light);
                }
            }
            WM_MOUSEWHEEL if SETTINGS_OPEN.load(Ordering::Relaxed) && in_panel(pt) && over_desktop(pt) && panel_has_files() => {
                let turned = ((*(lparam as *const MSLLHOOKSTRUCT)).mouseData >> 16) as i16;
                send(Command::Scroll(if turned > 0 { -1 } else { 1 }));
                return 1;
            }
            WM_LBUTTONDOWN => {
                if layers::peeking() && !over_desktop(pt) {
                    send(Command::PeekOff); // a click elsewhere puts Peek away, and still goes where it was aimed
                }
                if left_down(pt) {
                    SWALLOW_RELEASE.store(true, Ordering::Relaxed);
                    return 1;
                }
            }
            WM_LBUTTONUP => {
                if SWALLOW_RELEASE.swap(false, Ordering::Relaxed) {
                    let dragged = ghost::release();
                    box_ended();
                    if let Some((names, _)) = DRAG_ITEMS.try_lock().ok().and_then(|mut d| d.take()) {
                        if !dragged {
                            if PLAIN_PRESS.load(Ordering::Relaxed) {
                                send(Command::FileRelease(pt));
                            }
                        } else if let Some(section) = label_at(pt) {
                            send(Command::FiledItems(names, section));
                        }
                    }
                    return 1;
                }
                left_up(pt);
            }
            WM_RBUTTONDOWN if over_desktop(pt) && label_at(pt).is_some() => {
                RIGHT_ON_LABEL.store(true, Ordering::Relaxed);
                return 1; // our menu, not the desktop's
            }
            WM_RBUTTONDOWN if matches!(file_under(pt), Some(Some(_))) => {
                if let Ok(mut h) = HOVER_SECTION.try_lock() {
                    *h = None; // a menu means the section is in use
                }
                RIGHT_ON_FILE.store(true, Ordering::Relaxed);
                return 1; // the file's menu, not the desktop's
            }
            WM_RBUTTONUP if RIGHT_ON_FILE.swap(false, Ordering::Relaxed) => {
                send(Command::FileMenu(pt));
                return 1;
            }
            WM_RBUTTONUP if RIGHT_ON_LABEL.swap(false, Ordering::Relaxed) => {
                PostMessageW(OWNER.load(Ordering::Relaxed) as HWND, WM_MENU, 0, 0);
                return 1;
            }
            _ => {}
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Windows reports icons appearing, vanishing or being renamed here, as they happen.
unsafe extern "system" fn icon_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    if hwnd as usize == ICON_LIST.load(Ordering::Relaxed)
        && matches!(event, EVENT_OBJECT_CREATE | EVENT_OBJECT_DESTROY | EVENT_OBJECT_REORDER | EVENT_OBJECT_NAMECHANGE)
        && !CHANGE_WAITING.swap(true, Ordering::Relaxed)
    {
        send(Command::Changed);
    }
}

static EVENT_HOOKS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe fn watch_icons() {
    let mut hooks = EVENT_HOOKS.lock().unwrap();
    for h in hooks.drain(..) {
        UnhookWinEvent(h as HWINEVENTHOOK);
    }
    let lv = find_listview();
    ICON_LIST.store(lv as usize, Ordering::Relaxed);
    let mut pid = 0;
    GetWindowThreadProcessId(lv, &mut pid);
    for (first, last) in [(EVENT_OBJECT_CREATE, EVENT_OBJECT_REORDER), (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE)] {
        hooks.push(SetWinEventHook(first, last, std::ptr::null_mut(), Some(icon_event), pid, 0, 0) as usize);
    }
}

fn edit_rules() {
    let _ = std::process::Command::new("notepad").arg(rules_file()).spawn();
}

fn run_undo() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--undo").spawn();
    }
}

/// Dark menus. Windows has no public switch for this, so it is looked up by number; if a future
/// version drops it, the menu simply stays light.
pub unsafe fn dark_menus() {
    const SET_PREFERRED_APP_MODE: usize = 135;
    const FLUSH_MENU_THEMES: usize = 136;
    const FORCE_DARK: i32 = 2;
    let theme = LoadLibraryW(wide("uxtheme.dll").as_ptr());
    if theme.is_null() {
        return;
    }
    if let (Some(set), Some(flush)) = (GetProcAddress(theme, SET_PREFERRED_APP_MODE as *const u8), GetProcAddress(theme, FLUSH_MENU_THEMES as *const u8)) {
        let set: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set);
        let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
        set(FORCE_DARK);
        flush();
    }
}

unsafe fn show_menu(hwnd: HWND) {
    dark_menus();
    let menu = CreatePopupMenu();
    for (id, text) in [(5, "Settings"), (1, "Re-sort now"), (2, "Edit rules"), (3, "Undo and stop"), (4, "Stop")] {
        AppendMenuW(menu, MF_STRING, id, wide(text).as_ptr());
    }
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // otherwise the menu will not close when clicking away
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match chosen {
        1 => send(Command::Retidy),
        2 => edit_rules(),
        3 => run_undo(),
        4 => send(Command::Stop),
        5 => send(Command::Toggle(SETTINGS.to_string())),
        _ => {}
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_RETIDY | WM_DISPLAYCHANGE | WM_DPICHANGED => send(Command::Retidy),
        WM_SETTINGCHANGE if wparam == SPI_SETWORKAREA_CHANGE => send(Command::Retidy), // the taskbar moved
        WM_STOP => send(Command::Stop),
        WM_HOTKEY => send(match wparam {
            HOTKEY_FIND => Command::Find,
            HOTKEY_NEXT => Command::Workspace(1),
            HOTKEY_PREVIOUS => Command::Workspace(-1),
            _ => Command::Shortcut,
        }),
        WM_MENU => show_menu(hwnd),
        WM_TAKE_KEYS => {
            shellmenu::to_front(hwnd);
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(hwnd);
        }
        WM_GIVE_BACK_KEYS => {
            // back to the desktop, unless the user has already gone to another window
            if GetForegroundWindow() == hwnd {
                SetForegroundWindow(GetShellWindow());
            }
        }
        WM_KEYDOWN => {
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
            send(Command::Key(wparam as u32, GetKeyState(VK_CONTROL as i32) < 0, GetKeyState(VK_SHIFT as i32) < 0));
        }
        WM_REMAKE => {
            if let Err(e) = layers::create() {
                eprintln!("{e}");
            }
            start_pretty_if_wanted();
            watch_icons();
        }
        WM_CLOSE => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    0
}

/// Falls back to the plain look on any failure, so a machine that can't do this still works.
unsafe fn start_pretty_if_wanted() {
    let wanted = std::fs::read_to_string(rules_file()).ok().and_then(|t| parse_config(&t).ok()).is_some_and(|c| c.style != "plain");
    if wanted {
        match layers::start_pretty() {
            Ok(()) => log("pretty mode on"),
            Err(e) => log(&format!("pretty mode unavailable, using the plain look: {e}")),
        }
    }
}

unsafe fn running_instance() -> HWND {
    FindWindowW(wide(CLASS).as_ptr(), std::ptr::null())
}

/// One downloaded exe is all a person needs: the rules file appears next to it on first run.
fn ensure_rules() -> Result<(), String> {
    if !rules_file().exists() {
        std::fs::write(rules_file(), include_str!("../sections.ini"))
            .map_err(|e| format!("Could not write next to the program ({}). Move it to a folder you can write to, such as Documents.", e))?;
    }
    Ok(())
}

unsafe fn registry_text(key: &str, name: &str) -> String {
    use windows_sys::Win32::System::Registry::*;
    let mut buf = [0u16; 128];
    let mut size = 256u32;
    let (k, n) = (wide(key), wide(name));
    RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as _, &mut size);
    let end = buf.iter().position(|c| *c == 0).unwrap_or(0);
    String::from_utf16_lossy(&buf[..end])
}

/// Everything a bug report needs, with no file names in it.
unsafe fn doctor() -> Result<(), String> {
    ensure_rules()?;
    let current = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let mut lines = vec![
        format!("Rivulet {}", env!("CARGO_PKG_VERSION")),
        format!("Windows: version {} build {}", registry_text(current, "DisplayVersion"), registry_text(current, "CurrentBuild")),
        format!("Desktop layout: {}", layers::layout_name()),
        format!("Running now: {}", !running_instance().is_null()),
    ];
    for (i, m) in screens().iter().enumerate() {
        let r = m.rcMonitor;
        let main = if m.dwFlags & MONITORINFOF_PRIMARY != 0 { " (main)" } else { "" };
        lines.push(format!("Screen {i}: {}x{} at {},{}{main}", r.right - r.left, r.bottom - r.top, r.left, r.top));
    }
    match Desktop::open() {
        Ok(desk) => {
            lines.push(format!("Icons: {}", desk.count()));
            lines.push(format!("Scale: {}%", GetDpiForWindow(desk.lv) * 100 / 96));
            lines.push(format!("Auto arrange on: {}", GetWindowLongPtrW(desk.lv, GWL_STYLE) as i32 & LVS_AUTOARRANGE != 0));
            lines.push(match load_plan(&desk) {
                Ok(plan) => format!("Sections: {}", plan.groups.iter().filter(|g| g.0 != SETTINGS).map(|(s, g)| format!("{s} {}", g.len())).collect::<Vec<_>>().join(", ")),
                Err(e) => format!("Problem: {e}"),
            });
        }
        Err(e) => lines.push(format!("Problem: {e}")),
    }
    let report = lines.join("\n");
    println!("{report}");
    std::fs::create_dir_all(state_dir()).map_err(|e| e.to_string())?;
    let path = state_dir().join("doctor.txt");
    std::fs::write(&path, &report).map_err(|e| e.to_string())?;
    println!("Saved to {}", path.display());
    Ok(())
}

/// A short record of starts and problems, with no file names in it, for bug reports.
fn log(line: &str) {
    use std::io::Write;
    const KEEP: u64 = 64 * 1024;
    let path = state_dir().join("log.txt");
    if path.metadata().is_ok_and(|m| m.len() > KEEP) {
        let _ = std::fs::remove_file(&path);
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{now} {line}");
    }
}

/// At login the program starts before the desktop is ready, so a failure here usually only means
/// "not yet". Keep trying quietly, and report a problem only if it never clears.
unsafe fn wait_for_desktop() -> Result<Desktop, String> {
    const PATIENCE: Duration = Duration::from_secs(120);
    let began = Instant::now();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let ready = layers::parent_view()
            .ok_or_else(|| "The desktop is not ready.".to_string())
            .and_then(|_| Desktop::open())
            .and_then(|desk| if desk.grid_is_on() { grid_off().map(|_| desk) } else { Ok(desk) });
        match ready {
            Ok(desk) => {
                log(&format!("desktop ready after {} s, {attempts} attempts", began.elapsed().as_secs()));
                return Ok(desk);
            }
            Err(e) if began.elapsed() >= PATIENCE => {
                log(&format!("gave up: {e}"));
                return Err(e);
            }
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}

unsafe fn run() -> Result<(), String> {
    ensure_rules()?;
    std::fs::create_dir_all(state_dir()).map_err(|e| e.to_string())?;
    // only one copy, even during the wait below, before the message window exists; Windows
    // releases this when the process ends, however it ends
    let lock = CreateMutexW(std::ptr::null(), 0, wide("DesktopSectionsOneCopy").as_ptr());
    if !lock.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
        return Ok(());
    }
    log(&format!("started, version {}", env!("CARGO_PKG_VERSION")));
    let desk = wait_for_desktop()?;
    load_plan(&desk)?; // refuse now, before anything has changed
    wallpaper::restore()?;
    if !snapshot_file().exists() {
        let lines: Vec<String> = (0..desk.count())
            .map(|i| {
                let (x, y) = desk.position(i);
                format!("{}\t{x}\t{y}", desk.text(i))
            })
            .collect();
        std::fs::write(snapshot_file(), lines.join("\n")).map_err(|e| e.to_string())?;
    }
    let open = std::fs::read_to_string(open_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != SETTINGS);

    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(CLASS);
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&wc);
    // a real but never-shown window: it hears about screen changes and can own the menu
    let window = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 0, 0, null(), std::ptr::null_mut(), instance, std::ptr::null());
    if window.is_null() {
        return Err("Could not start.".into());
    }
    OWNER.store(window as usize, Ordering::Relaxed);
    layers::create()?;
    start_pretty_if_wanted();
    let (tx, rx) = channel();
    let _ = COMMANDS.set(tx);
    let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0);
    if hook.is_null() {
        return Err("Windows refused to let this watch for clicks.".into());
    }
    watch_icons();
    // a shortcut another program already holds is not worth stopping for; --doctor reports it
    let shortcut = parse_config(&std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?)?.hotkey;
    if let Some((mods, key)) = parse_hotkey(&shortcut) {
        const NO_REPEAT: u32 = 0x4000;
        RegisterHotKey(window, 1, mods | NO_REPEAT, key);
    }
    {
        // Ctrl+Alt+F to find, and Page Up / Down between saved layouts. If another program holds one it just goes unused.
        const NO_REPEAT: u32 = 0x4000;
        const CTRL_ALT: u32 = 2 | 1;
        RegisterHotKey(window, HOTKEY_FIND as i32, CTRL_ALT | NO_REPEAT, 0x46);
        RegisterHotKey(window, HOTKEY_NEXT as i32, CTRL_ALT | NO_REPEAT, 0x22);
        RegisterHotKey(window, HOTKEY_PREVIOUS as i32, CTRL_ALT | NO_REPEAT, 0x21);
    }
    let handle = window as usize;
    let desk_for_worker = SendDesktop(desk);
    let thread = std::thread::spawn(move || {
        let wrapped = desk_for_worker;
        worker(wrapped.0, open, rx, handle)
    });
    let mut msg = std::mem::zeroed();
    while GetMessageW(&mut msg, null(), 0, 0) > 0 {
        DispatchMessageW(&msg);
    }
    UnhookWindowsHookEx(hook);
    let _ = thread.join();
    Ok(())
}

/// The handles inside Desktop are safe to use from the one worker thread that receives them.
struct SendDesktop(Desktop);
unsafe impl Send for SendDesktop {}

unsafe fn stop_running() {
    let window = running_instance();
    if window.is_null() {
        return;
    }
    PostMessageW(window, WM_STOP, 0, 0);
    for _ in 0..50 {
        if running_instance().is_null() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

unsafe fn undo() -> Result<(), String> {
    stop_running();
    wallpaper::restore()?;
    let saved = std::fs::read_to_string(snapshot_file()).map_err(|_| "There is nothing saved to undo.".to_string())?;
    let desk = Desktop::open()?;
    restore_spacing(&desk);
    let mut spots: HashMap<&str, Vec<(i32, i32)>> = HashMap::new();
    for line in saved.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if let [name, x, y] = f[..] {
            spots.entry(name).or_default().push((x.parse().unwrap_or(0), y.parse().unwrap_or(0)));
        }
    }
    for i in 0..desk.count() {
        let label = desk.text(i);
        if let Some(list) = spots.get_mut(label.as_str()) {
            if !list.is_empty() {
                let (x, y) = list.remove(0);
                desk.place(i, x, y);
            }
        }
    }
    for file in [snapshot_file(), open_file(), seen_file()] {
        let _ = std::fs::remove_file(file);
    }
    println!("Icons restored; the program has stopped.");
    Ok(())
}

unsafe fn dry_run() -> Result<(), String> {
    let desk = Desktop::open()?;
    let plan = load_plan(&desk)?;
    for (s, g) in plan.groups.iter().filter(|g| g.0 != SETTINGS) {
        println!("{s}: {}", g.len());
    }
    if plan.tightened {
        println!("Warning: icon spacing would be tightened to fit everything.");
    }
    for label in plan.unclear {
        println!("Warning: could not tell which file \"{label}\" is, so it would go to Other.");
    }
    Ok(())
}

/// Start at login through Task Scheduler. Windows works through the Startup folder and Run keys
/// slowly, one item every 30 seconds on a busy PC (measured at 4.5 minutes here), and icons sit
/// out of place all that time; a logon task starts straight away.
fn install_login() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let ps = "$a=New-ScheduledTaskAction -Execute $env:DS_EXE -WorkingDirectory $env:DS_DIR;\
              $t=New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME;\
              $p=New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited;\
              $s=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit 0 -Priority 5;\
              Register-ScheduledTask -TaskName Rivulet -Action $a -Trigger $t -Principal $p -Settings $s -Force -ErrorAction Stop | Out-Null";
    let done = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .env("DS_EXE", &exe)
        .env("DS_DIR", home())
        .creation_flags(NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !done.status.success() {
        return Err(format!(
            "Windows would not create the login task. Try again from a terminal opened with Run as administrator.\n\n{}",
            String::from_utf8_lossy(&done.stderr).lines().next().unwrap_or("")
        ));
    }
    // the slow Startup folder entries from earlier versions would only start a pointless second launch
    let startup = Path::new(&std::env::var("APPDATA").map_err(|e| e.to_string())?).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
    for old in ["Rivulet.lnk", "Desktop Sections.lnk"] {
        if std::fs::remove_file(startup.join(old)).is_ok() {
            println!("Removed the old {old} from the Startup folder");
        }
    }
    println!("Rivulet will now start right at login. To turn that off: {} --remove-login", exe.display());
    Ok(())
}

fn login_is_on() -> bool {
    std::process::Command::new("schtasks")
        .args(["/Query", "/TN", "Rivulet"])
        .creation_flags(NO_WINDOW)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn remove_login() -> Result<(), String> {
    let done = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", "Rivulet", "/F"])
        .creation_flags(NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !done.status.success() {
        return Err("There is no login task to remove.".into());
    }
    println!("Rivulet will no longer start at login.");
    Ok(())
}

fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let programs = Path::new(&std::env::var("APPDATA").map_err(|e| e.to_string())?).join(r"Microsoft\Windows\Start Menu\Programs");
    let link = programs.join("Rivulet.lnk");
    let ps = "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:DS_LINK);\
              $s.TargetPath=$env:DS_EXE;$s.WorkingDirectory=$env:DS_DIR;$s.Save()";
    let done = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .env("DS_LINK", &link)
        .env("DS_EXE", &exe)
        .env("DS_DIR", home())
        .creation_flags(NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;
    if !done.success() {
        return Err(format!("Could not create {}", link.display()));
    }
    println!("Created {}", link.display());
    Ok(())
}

fn alert(text: &str) {
    println!("{text}");
    let (t, c) = (wide(text), wide("Rivulet"));
    unsafe { MessageBoxW(null(), t.as_ptr(), c.as_ptr(), MB_ICONWARNING) };
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let result = unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); // work in real screen pixels
        match arg.as_str() {
            "--dry-run" => ensure_rules().and_then(|_| dry_run()),
            "--doctor" => doctor(),
            "--undo" => undo(),
            "--quit" => {
                stop_running();
                Ok(())
            }
            "--install" => install(),
            "--install-login" => install_login(),
            "--remove-login" => remove_login(),
            _ => {
                let window = running_instance();
                if window.is_null() {
                    run()
                } else {
                    PostMessageW(window, WM_RETIDY, 0, 0); // already running, so just re-sort
                    Ok(())
                }
            }
        }
    };
    if let Err(e) = result {
        log(&format!("stopped with a problem: {e}"));
        alert(&e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod hover_tests {
    use super::*;

    #[test]
    fn the_way_from_label_to_panel_keeps_a_section_open() {
        let r = |left, top, right, bottom| RECT { left, top, right, bottom };
        let (rail, label, panel) = (r(10, 10, 200, 600), r(10, 300, 200, 340), r(300, 100, 900, 500));
        let at = |x, y| POINT { x, y };
        assert!(keeps_open(at(100, 320), rail, label, panel)); // on its label
        assert!(keeps_open(at(100, 50), rail, label, panel)); // on another label
        assert!(keeps_open(at(250, 320), rail, label, panel)); // crossing the gap
        assert!(keeps_open(at(600, 300), rail, label, panel)); // in the panel
        assert!(!keeps_open(at(1000, 300), rail, label, panel)); // past the panel
        assert!(!keeps_open(at(600, 700), rail, label, panel)); // below it
        // with the labels on the right
        let (rail, label, panel) = (r(1700, 10, 1900, 600), r(1700, 300, 1900, 340), r(900, 100, 1600, 500));
        assert!(keeps_open(at(1650, 320), rail, label, panel));
        assert!(!keeps_open(at(500, 320), rail, label, panel));
    }
}
