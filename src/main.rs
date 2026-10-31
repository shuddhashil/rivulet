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
mod rules;
mod wallpaper;

use rules::*;
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
use windows_sys::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::SHGetFolderPathW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const LVM_GETITEMCOUNT: u32 = 0x1004;
const LVM_GETNEXTITEM: u32 = 0x100C;
const LVM_SETITEMPOSITION: u32 = 0x100F;
const LVM_GETITEMPOSITION: u32 = 0x1010;
const LVM_GETITEMSPACING: u32 = 0x1033;
const LVM_SETICONSPACING: u32 = 0x1035;
const LVM_GETITEMTEXTW: u32 = 0x1073;
const LVS_AUTOARRANGE: i32 = 0x100;
const LVNI_SELECTED: isize = 2;
const CLASS: &str = "DesktopSectionsMessages";
const WM_RETIDY: u32 = WM_APP + 1;
const WM_STOP: u32 = WM_APP + 2;
const WM_REMAKE: u32 = WM_APP + 3;
const WM_MENU: u32 = WM_APP + 4;
const SPI_SETWORKAREA_CHANGE: usize = 47;
const EVENT_OBJECT_CREATE: u32 = 0x8000;
const EVENT_OBJECT_DESTROY: u32 = 0x8001;
const EVENT_OBJECT_REORDER: u32 = 0x8004;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
/// A backstop only: Windows normally tells the program about changes as they happen.
const CHECK_EVERY: Duration = Duration::from_secs(3);
const NO_WINDOW: u32 = 0x0800_0000;
const OPEN_TIME: f32 = 0.24;
const CLOSE_TIME: f32 = 0.11;

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

/// Match each icon's label to its file: (kind, time). Returns also the unclear labels.
unsafe fn describe(labels: &[String]) -> (Vec<(String, u64)>, Vec<String>) {
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
    let (mut out, mut unclear) = (vec![], vec![]);
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
                    continue;
                }
            }
        };
        let Some(path) = entry else {
            unclear.push(label.clone());
            out.push(("unknown".to_string(), 0));
            continue;
        };
        let kind = if path.is_dir() {
            "folder".to_string()
        } else {
            path.extension().map_or(String::new(), |e| format!(".{}", e.to_string_lossy().to_lowercase()))
        };
        out.push((kind, stamp(path)));
    }
    (out, unclear)
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
    m: Metrics,
    view_origin: (i32, i32),
    layer_origin: (i32, i32),
    screen_origin: (i32, i32),
    room: (i32, i32),
    tightened: bool,
    unclear: Vec<String>,
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

    /// Centre height of a section's label, inside the work area.
    fn pill_mid(&self, at: usize) -> i32 {
        let m = self.m;
        m.gap + at as i32 * (m.pill_h + m.pill_gap) + m.pill_h / 2
    }

    fn panel_x(&self) -> i32 {
        self.m.gap + self.m.rail_w + self.m.bridge
    }

    fn panel_room(&self) -> (i32, i32) {
        let m = self.m;
        (self.room.0 - self.panel_x() - m.gap - 2 * m.pad, self.room.1 - 2 * m.gap - 2 * m.pad)
    }

    /// The open panel, level with its label where it fits, and how many icons fit across it.
    fn panel(&self, at: usize) -> Option<(Rect, i32)> {
        let m = self.m;
        let (room_w, room_h) = self.panel_room();
        let (across, rows) = panel_grid(self.groups[at].1.len() as i32, m.cell_w, m.cell_h, room_w, room_h)?;
        let h = rows * m.cell_h + 2 * m.pad;
        let lowest = (self.room.1 - m.gap - h).max(m.gap);
        let y = (self.pill_mid(at) - (30.0 * m.dpi) as i32).clamp(m.gap, lowest);
        Some((Rect { x: self.panel_x(), y, w: across * m.cell_w + 2 * m.pad, h }, across))
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
    if GetWindowLongPtrW(desk.lv, GWL_STYLE) as i32 & LVS_AUTOARRANGE != 0 {
        return Err("Icons cannot be placed while \"Auto arrange icons\" is on.\n\
                    Right-click the desktop > View > untick Auto arrange icons, then run this again."
            .into());
    }
    let labels: Vec<String> = (0..desk.count()).map(|i| desk.text(i)).collect();
    let (details, unclear) = describe(&labels);
    let mut sortable: HashMap<&str, Vec<(std::cmp::Reverse<u64>, String, usize)>> = HashMap::new();
    for (i, (label, (kind, time))) in labels.iter().zip(&details).enumerate() {
        sortable.entry(classify(label, kind, cfg)).or_default().push((std::cmp::Reverse(*time), label.to_lowercase(), i));
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
            items.sort(); // newest first
            groups.push((section.clone(), items.into_iter().map(|x| x.2).collect::<Vec<_>>()));
        }
    }

    let dpi = GetDpiForWindow(desk.lv) as f32 / 96.0;
    let work = chosen_work_area(cfg.monitor);
    let mut origin = POINT { x: work.left, y: work.top };
    ScreenToClient(desk.lv, &mut origin);
    let (_, spacing_h) = desk.spacing();
    let scaled = |v: f32| (v * dpi) as i32;
    let room = (work.right - work.left, work.bottom - work.top);
    let wide_spacing = scaled(120.0); // wider than Windows' default, so more of each name shows
    for tighten in [1.0f32, 0.95, 0.9, 0.85, 0.8] {
        let m = Metrics {
            cell_w: (wide_spacing as f32 * tighten) as i32,
            cell_h: (spacing_h as f32 * tighten) as i32,
            wide_spacing,
            pad: scaled(6.0),
            gap: scaled(10.0),
            pill_h: scaled(32.0),
            pill_gap: scaled(4.0),
            rail_w: scaled(168.0),
            bridge: scaled(76.0),
            radius: scaled(14.0),
            dpi,
        };
        let plan = Plan {
            labels: vec![],
            groups: vec![],
            colours: cfg.colours.iter().cloned().collect(),
            m,
            view_origin: (origin.x, origin.y),
            layer_origin: layers::to_parent(work.left, work.top),
            screen_origin: (work.left, work.top),
            room,
            tightened: tighten < 1.0,
            unclear: vec![],
        };
        let (room_w, room_h) = plan.panel_room();
        if groups.iter().all(|(_, g)| panel_grid(g.len() as i32, m.cell_w, m.cell_h, room_w, room_h).is_some()) {
            return Ok(Plan { labels, groups, unclear, ..plan });
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

enum Command {
    Toggle(String),
    Close,
    Hover(Option<String>),
    Filed(String),
    Shortcut,
    Changed,
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
    tinted_for: String,
    main_window: usize,
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
        let m = plan.m;
        let height = plan.groups.len() as i32 * (m.pill_h + m.pill_gap);
        let (sx, sy) = plan.screen_origin;
        let (mut hits, mut pills) = (vec![], vec![]);
        for (at, (name, icons)) in plan.groups.iter().enumerate() {
            let top = at as i32 * (m.pill_h + m.pill_gap);
            let rect = RECT { left: 0, top, right: m.rail_w, bottom: top + m.pill_h };
            let fresh = self.seen.as_ref().is_some_and(|seen| icons.iter().any(|&i| !seen.contains(&plan.labels[i].to_lowercase())));
            pills.push(layers::Pill {
                name: name.clone(),
                count: icons.len(),
                active: self.open.as_deref() == Some(name.as_str()),
                hover: self.hover.as_deref() == Some(name.as_str()),
                fresh,
                colour: plan.colour(name),
                glyph: glyph(name),
                rect,
            });
            let screen = RECT { left: sx + m.gap, top: sy + m.gap + top, right: sx + m.gap + m.rail_w, bottom: sy + m.gap + top + m.pill_h };
            hits.push(Label { name: name.clone(), rect: screen });
        }
        *layers::PILLS.lock().unwrap() = pills;
        *LABELS.lock().unwrap() = hits;
        let (lx, ly) = plan.layer_origin;
        layers::show_rail(lx + m.gap, ly + m.gap, m.rail_w + 1, height.max(1), m.radius);
    }

    unsafe fn draw_open(&self, plan: &Plan, at: usize, r: Rect, reach: f32) {
        let m = plan.m;
        let (lx, ly) = plan.layer_origin;
        layers::set_panel(lx + r.x, ly + r.y, r.w, r.h, m.radius.min(r.h / 2).min(r.w / 2));
        let start = (lx + m.gap + m.rail_w - 2, ly + plan.pill_mid(at));
        let end = (lx + r.x + 2, ly + r.y + (r.h / 2).min((46.0 * m.dpi) as i32));
        layers::set_neck(start, end, reach, m.pill_h as f32 * 0.5, 2.5 * m.dpi);
    }

    /// Follow the wallpaper: the rail and panel take a dark shade of its average colour.
    fn follow_wallpaper(&mut self) {
        let now = wallpaper::current();
        if now != self.tinted_for {
            if let Some(average) = wallpaper::average_colour() {
                layers::set_tints(dark_shade(average, 46.0), dark_shade(average, 24.0));
            }
            self.tinted_for = now;
        }
    }

    /// Put everything exactly where it belongs, with no animation.
    unsafe fn settle(&mut self) -> Result<(), String> {
        if !self.desk.alive() {
            self.desk = Desktop::open()?; // Explorer restarted
            ICON_LIST.store(self.desk.lv as usize, Ordering::Relaxed);
            grid_off()?;
        }
        if !layers::alive() {
            SendMessageW(self.main_window as HWND, WM_REMAKE, 0, 0);
        }
        self.follow_wallpaper();
        let plan = load_plan(&self.desk)?;
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
        if let Some((at, r, across)) = shown_panel {
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
    unsafe fn flow(&mut self, plan: &Plan, at: usize, opening: bool) {
        let Some((full, across)) = plan.panel(at) else { return };
        let icons = plan.groups[at].1.clone();
        let m = plan.m;
        layers::set_accent(plan.colour(&plan.groups[at].0));
        let seed = Rect { x: full.x, y: (plan.pill_mid(at) - m.pill_h / 2).clamp(m.gap, plan.room.1 - m.pill_h), w: m.pill_h, h: m.pill_h };
        let rows = (icons.len() as i32 + across - 1) / across;
        let seconds = if opening { OPEN_TIME } else { CLOSE_TIME };
        let parked = (self.far(), plan.view_origin.1);
        let slide = (22.0 * m.dpi) as f32;
        // only the part of the icon list inside the panel needs repainting
        let dirty = RECT {
            left: plan.view_origin.0 + full.x - slide as i32,
            top: plan.view_origin.1 + full.y,
            right: plan.view_origin.0 + full.x + full.w,
            bottom: plan.view_origin.1 + full.y + full.h,
        };
        if !opening {
            self.park(&icons, parked); // leaving: icons go at once, and the panel folds up after them
        }
        let mut placed = vec![parked; icons.len()];
        let began = Instant::now();
        loop {
            let t = (began.elapsed().as_secs_f32() / seconds).min(1.0);
            let progress = if opening { t } else { 1.0 - t };
            let shape = if opening { spring(progress) } else { ease_out(progress) };
            let r = Rect { x: full.x, y: mix(seed.y, full.y, shape), w: mix(seed.w, full.w, shape), h: mix(seed.h, full.h, shape) };
            self.draw_open(plan, at, r, ease_out((progress * 2.2).min(1.0)));
            if opening {
                let wanted: Vec<(i32, i32)> = (0..icons.len() as i32)
                    .map(|n| {
                        // the panel uncovers each icon as it grows past it; it then slides the last short way in
                        let across_part = (n % across) as f32 / across as f32;
                        let down_part = (n / across) as f32 / rows.max(1) as f32;
                        let delay = across_part * 0.5 + down_part * 0.25;
                        let own = ((progress - delay) / 0.3).clamp(0.0, 1.0);
                        if own <= 0.0 {
                            return parked;
                        }
                        let to = plan.cell(full, across, n);
                        (to.0 - (slide * (1.0 - ease_out(own))) as i32, to.1)
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
        if let Some(at) = previous.as_deref().and_then(|s| plan.index(s)) {
            self.show_rail(plan);
            if animate {
                self.flow(plan, at, false);
            } else {
                self.park(&plan.groups[at].1, (self.far(), plan.view_origin.1));
            }
            layers::hide_panel();
        }
    }

    /// The layout from the last settle, which Windows' change reports keep fresh, so a click
    /// does not have to re-read the whole desktop before anything moves.
    unsafe fn current_plan(&mut self) -> Result<Plan, String> {
        match self.last_plan.take() {
            Some(plan) if plan.labels.len() == self.desk.count() => Ok(plan),
            _ => load_plan(&self.desk),
        }
    }

    /// The keyboard shortcut: bring the desktop forward with the last section open, and on the
    /// second press put everything back.
    unsafe fn shortcut(&mut self) -> Result<(), String> {
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
            let wanted = self.last_section.clone().filter(|s| plan.index(s).is_some()).or_else(|| plan.groups.first().map(|g| g.0.clone()));
            self.last_plan = Some(plan);
            if let Some(name) = wanted {
                return self.toggle(name);
            }
        }
        Ok(())
    }

    unsafe fn toggle(&mut self, name: String) -> Result<(), String> {
        let plan = self.current_plan()?;
        let previous = self.open.clone();
        self.close_open(&plan, previous.as_deref() == Some(name.as_str()));
        if previous.as_deref() != Some(name.as_str()) {
            if let Some(at) = plan.index(&name) {
                self.last_section = Some(name.clone());
                self.open = Some(name);
                self.show_rail(&plan);
                self.flow(&plan, at, true);
            }
        }
        let _ = std::fs::write(open_file(), self.open.as_deref().unwrap_or(""));
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
        if section == RECENT {
            return self.settle(); // Recent is worked out, not filed into
        }
        let mut text = std::fs::read_to_string(rules_file()).map_err(|e| e.to_string())?;
        for i in self.desk.selected() {
            text = add_name_rule(&text, &self.desk.text(i), &section);
        }
        parse_config(&text)?; // never write a rules file that would not load
        std::fs::write(rules_file(), text).map_err(|e| e.to_string())?;
        self.settle()
    }

    /// Cheap backstop check, in case Windows did not report a change.
    unsafe fn needs_settle(&self) -> bool {
        if !self.desk.alive() || !layers::alive() || self.desk.count() != self.count || wallpaper::current() != self.tinted_for {
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
        let mut s = Sections { desk, open, hover: None, hidden: vec![], count: 0, seen: None, last_plan: None, last_section, shortcut_showing: false, tinted_for: String::from("\0"), main_window: window };
        if let Err(e) = s.settle() {
            log(&format!("first sort failed: {e}"));
            alert(&e);
            PostMessageW(window as HWND, WM_CLOSE, 0, 0);
            return;
        }
        log("running");
        loop {
            let result = match rx.recv_timeout(CHECK_EVERY) {
                Ok(Command::Toggle(name)) => s.toggle(name),
                Ok(Command::Close) => s.close(),
                Ok(Command::Hover(name)) => {
                    s.hover(name);
                    Ok(())
                }
                Ok(Command::Filed(section)) => s.file_selected(section),
                Ok(Command::Shortcut) => s.shortcut(),
                Ok(Command::Changed) => {
                    // a refresh arrives as a burst of changes, so let it finish first
                    std::thread::sleep(Duration::from_millis(30));
                    CHANGE_WAITING.store(false, Ordering::Relaxed);
                    s.settle()
                }
                Ok(Command::Retidy) => s.settle(),
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    s.lay_all_out();
                    PostMessageW(window as HWND, WM_CLOSE, 0, 0);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if s.needs_settle() {
                        s.settle()
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
static LAST_TOGGLE: Mutex<Option<Instant>> = Mutex::new(None);
static HOVERING: Mutex<Option<String>> = Mutex::new(None);
static RIGHT_ON_LABEL: AtomicBool = AtomicBool::new(false);
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

/// Acts on the press, not the release, so a section starts moving under the finger.
unsafe fn left_down(pt: POINT) {
    DRAG_FROM_PANEL.store(false, Ordering::Relaxed);
    if !over_desktop(pt) {
        return;
    }
    if let Some(name) = label_at(pt) {
        let mut last = LAST_TOGGLE.lock().unwrap();
        // a double-click is one toggle, not two that cancel out
        if last.map_or(true, |t| t.elapsed() > Duration::from_millis(250)) {
            *last = Some(Instant::now());
            send(Command::Toggle(name));
        }
    } else if in_panel(pt) {
        DRAG_FROM_PANEL.store(true, Ordering::Relaxed);
    } else {
        send(Command::Close);
    }
}

unsafe fn left_up(pt: POINT) {
    if DRAG_FROM_PANEL.swap(false, Ordering::Relaxed) {
        if let Some(section) = label_at(pt) {
            send(Command::Filed(section)); // an icon was dragged onto a label
        }
    }
}

fn mouse_moved(pt: POINT) {
    let hit = label_at(pt).filter(|_| unsafe { over_desktop(pt) });
    if let Ok(mut hovering) = HOVERING.try_lock() {
        if *hovering != hit {
            *hovering = hit.clone();
            send(Command::Hover(hit));
        }
    }
}

/// Windows drops a mouse hook that answers slowly, so this only notes the event; the worker does the work.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let pt = (*(lparam as *const MSLLHOOKSTRUCT)).pt;
        match wparam as u32 {
            WM_MOUSEMOVE => mouse_moved(pt),
            WM_LBUTTONDOWN => left_down(pt),
            WM_LBUTTONUP => left_up(pt),
            WM_RBUTTONDOWN if over_desktop(pt) && label_at(pt).is_some() => {
                RIGHT_ON_LABEL.store(true, Ordering::Relaxed);
                return 1; // our menu, not the desktop's
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

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    for (id, text) in [(1, "Re-sort now"), (2, "Edit rules"), (3, "Undo and stop"), (4, "Stop")] {
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
        2 => {
            let _ = std::process::Command::new("notepad").arg(rules_file()).spawn();
        }
        3 => {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).arg("--undo").spawn();
            }
        }
        4 => send(Command::Stop),
        _ => {}
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_RETIDY | WM_DISPLAYCHANGE | WM_DPICHANGED => send(Command::Retidy),
        WM_SETTINGCHANGE if wparam == SPI_SETWORKAREA_CHANGE => send(Command::Retidy), // the taskbar moved
        WM_STOP => send(Command::Stop),
        WM_HOTKEY => send(Command::Shortcut),
        WM_MENU => show_menu(hwnd),
        WM_REMAKE => {
            if let Err(e) = layers::create() {
                eprintln!("{e}");
            }
            watch_icons();
        }
        WM_CLOSE => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    0
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
                Ok(plan) => format!("Sections: {}", plan.groups.iter().map(|(s, g)| format!("{s} {}", g.len())).collect::<Vec<_>>().join(", ")),
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
            .and_then(|desk| grid_off().map(|_| desk));
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
    let open = std::fs::read_to_string(open_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

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
    for (s, g) in &plan.groups {
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
