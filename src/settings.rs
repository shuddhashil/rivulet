//! The Settings panel, as plain data: what to draw and what each click does. Drawing and clicks
//! both read this one list, so they can't disagree, and `cargo test` can check it.

use crate::rules::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::RECT;

pub const SETTINGS: &str = "Settings";

const ROW: f32 = 34.0;
const WIDTH: f32 = 520.0;
const MARGIN: f32 = 16.0;
const LABEL_W: f32 = 170.0;
pub const THEMES: [&str; 5] = ["Wallpaper", "Midnight", "Graphite", "Forest", "Rose"];
const SPEEDS: [&str; 4] = ["Fast", "Normal", "Relaxed", "Off"];
/// Which page of Settings is showing: 0 general, 1 appearance, 2 sections, 3 rules, 4 layouts, 5 symbols.
pub static PAGE: AtomicU8 = AtomicU8::new(0);
/// 0 small, 1 medium, 2 large.
pub static ICON_SIZE: AtomicU8 = AtomicU8::new(1);
/// The saved layouts, and the section whose symbol is being chosen.
pub static LAYOUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub static SYMBOL_FOR: Mutex<String> = Mutex::new(String::new());
const SYMBOLS: [u32; 48] = [0xE8B7, 0xE8A5, 0xE80A, 0xE7F4, 0xEA90, 0xE70B, 0xE943, 0xE71D, 0xE7B8, 0xE712, 0xE896, 0xE823, 0xE734, 0xE735, 0xEB51, 0xE7C3, 0xE8D6, 0xE8B9, 0xE714, 0xE715, 0xE716, 0xE717, 0xE722, 0xE7BE, 0xE82D, 0xE8F1, 0xE8FD, 0xE774, 0xE753, 0xE787, 0xE80F, 0xE81C, 0xE825, 0xE8A1, 0xE8EF, 0xE90F, 0xE913, 0xE950, 0xEC05, 0xEB3C, 0xE945, 0xE790, 0xE7EE, 0xE8BC, 0xE7FC, 0xE7F8, 0xE8F3, 0xE721];

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Write this value to sections.ini: [part] key = value.
    Set(&'static str, &'static str, String),
    Login(bool),
    EditRules,
    Resort,
    Undo,
    Stop,
    /// A file in a portal: opens on a double-click, like a desktop icon.
    Open(PathBuf),
    /// The portal's own button: opens at once.
    OpenFolder(PathBuf),
    /// Show another page of Settings.
    Page(u8),
    CycleSort,
    ToggleView,
    TogglePin,
    /// Inside a portal: back to where it was, and up a folder.
    Back,
    Up,
    /// Another tab of this panel.
    Show(String),
    /// Choose a colour: a look setting's key, or "section:" and a section.
    PickColour(String),
    /// Take these look settings back to the theme's.
    ClearLook(Vec<&'static str>),
    PickSymbol(String),
    SetSymbol(String, u32),
    ClearSymbol(String),
    RenameSection(String),
    MoveSection(String, i32),
    RemoveSection(String),
    ToggleTab(String),
    AddSection,
    AddPortal,
    AddRule,
    RemoveRule(String),
    SaveLayout,
    LoadLayout(String),
    RemoveLayout(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Look {
    Heading(String),
    Text(String),
    Note(String),
    Chip(String, bool),
    Switch(bool),
    Value(String),
    Button(String),
    /// A file: its name and its icon handle.
    File(String, isize),
    /// The little bar showing how far through a long list the panel is.
    Thumb,
    /// A patch of colour.
    Swatch(u32),
    /// A symbol from the section symbols' font.
    Symbol(char),
}

#[derive(Clone)]
pub struct Item {
    pub rect: RECT,
    pub look: Look,
    pub action: Option<Action>,
    /// A one-line row of a list view rather than a tile, with what shows at its right.
    pub row: bool,
    pub detail: String,
}

/// Settings' size: as tall as this page's contents.
pub fn size(cfg: &Config, dpi: f32) -> (i32, i32) {
    let bottom = build(cfg, false, dpi).iter().map(|i| i.rect.bottom).max().unwrap_or(0);
    ((WIDTH * dpi) as i32, bottom + (MARGIN * dpi) as i32)
}

struct Builder {
    dpi: f32,
    row: i32,
    items: Vec<Item>,
}

impl Builder {
    fn px(&self, v: f32) -> i32 {
        (v * self.dpi) as i32
    }

    fn rect(&self, x: f32, w: f32) -> RECT {
        let top = self.px(MARGIN / 2.0 + self.row as f32 * ROW + 4.0);
        RECT { left: self.px(x), top, right: self.px(x + w), bottom: top + self.px(ROW - 8.0) }
    }

    fn add(&mut self, rect: RECT, look: Look, action: Option<Action>) {
        self.items.push(Item { rect, look, action, row: false, detail: String::new() });
    }

    fn label(&mut self, text: &str) {
        let r = self.rect(MARGIN, LABEL_W);
        self.add(r, Look::Text(text.to_string()), None);
    }

    fn heading(&mut self, text: &str) {
        let r = self.rect(MARGIN, WIDTH - 2.0 * MARGIN);
        self.add(r, Look::Heading(text.to_string()), None);
        self.row += 1;
    }

    fn choice(&mut self, text: &str, key: &'static str, options: &[&str], current: &str) {
        let pairs: Vec<(&str, &str)> = options.iter().map(|o| (*o, *o)).collect();
        self.choice_pairs(text, key, &pairs, current);
    }

    /// Chips whose labels differ from the values written to the rules file.
    fn choice_pairs(&mut self, text: &str, key: &'static str, options: &[(&str, &str)], current: &str) {
        self.label(text);
        let room = WIDTH - MARGIN * 2.0 - LABEL_W;
        let w = (room - 4.0 * (options.len() as f32 - 1.0)) / options.len() as f32;
        for (n, (label, value)) in options.iter().enumerate() {
            let r = self.rect(MARGIN + LABEL_W + n as f32 * (w + 4.0), w);
            let on = value.eq_ignore_ascii_case(current);
            self.add(r, Look::Chip(label.to_string(), on), Some(Action::Set("general", key, value.to_lowercase())));
        }
        self.row += 1;
    }

    fn switch(&mut self, text: &str, on: bool, action: Action) {
        self.label(text);
        let r = self.rect(WIDTH - MARGIN - 46.0, 46.0);
        self.add(r, Look::Switch(on), Some(action));
        self.row += 1;
    }

    fn stepper(&mut self, text: &str, part: &'static str, key: &'static str, value: u64, step: u64, low: u64, high: u64) {
        self.label(text);
        let less = self.rect(WIDTH - MARGIN - 110.0, 32.0);
        self.add(less, Look::Button("\u{2212}".to_string()), Some(Action::Set(part, key, value.saturating_sub(step).max(low).to_string())));
        let shown = self.rect(WIDTH - MARGIN - 76.0, 42.0);
        self.add(shown, Look::Value(value.to_string()), None);
        let more = self.rect(WIDTH - MARGIN - 32.0, 32.0);
        self.add(more, Look::Button("+".to_string()), Some(Action::Set(part, key, (value + step).min(high).to_string())));
        self.row += 1;
    }
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.to_string()
}

/// `login` says whether Rivulet is set to start at login.
pub fn build(cfg: &Config, login: bool, dpi: f32) -> Vec<Item> {
    build_page(cfg, login, dpi, PAGE.load(Ordering::Relaxed))
}

fn build_page(cfg: &Config, login: bool, dpi: f32, page: u8) -> Vec<Item> {
    let mut b = Builder { dpi, row: 0, items: vec![] };
    b.heading("Settings");
    let names = ["General", "Appearance", "Sections", "Rules", "Layouts"];
    let w = (WIDTH - 2.0 * MARGIN - 4.0 * (names.len() as f32 - 1.0)) / names.len() as f32;
    for (n, text) in names.into_iter().enumerate() {
        let r = b.rect(MARGIN + n as f32 * (w + 4.0), w);
        let on = page == n as u8 || (page == 5 && n == 2);
        b.add(r, Look::Chip(text.to_string(), on), Some(Action::Page(n as u8)));
    }
    b.row += 1;
    match page {
        1 => return appearance_page(b, cfg),
        2 => return sections_page(b, cfg),
        3 => return rules_page(b, cfg),
        4 => return layouts_page(b, cfg),
        5 => return symbols_page(b, cfg),
        _ => {}
    }
    b.choice("Theme", "theme", &THEMES, &cfg.theme);
    b.choice("Labels on", "side", &["Left", "Right"], if cfg.right_side { "right" } else { "left" });
    b.choice_pairs("Icons", "icons", &[("Explorer", "explorer"), ("Rivulet", "own")], if cfg.own_icons { "own" } else { "explorer" });
    b.choice("Animation", "speed", &SPEEDS, &cfg.speed);
    b.switch("New-file dots", cfg.dots, Action::Set("general", "dots", on_off(!cfg.dots)));
    b.switch("Click empty desktop to close", cfg.click_away, Action::Set("general", "click_away", on_off(!cfg.click_away)));
    b.switch("Double-click desktop hides labels", cfg.double_click_hide, Action::Set("general", "double_click_hide", on_off(!cfg.double_click_hide)));
    b.switch("Shortcut shows sections over open windows", cfg.peek, Action::Set("general", "peek", on_off(!cfg.peek)));
    b.switch("Start at login", login, Action::Login(!login));
    b.heading("Recent");
    b.stepper("Days to look back (0 is off)", "recent", "days", cfg.recent_days, 1, 0, 30);
    b.stepper("Most items to show", "recent", "limit", cfg.recent_limit as u64, 1, 1, 24);
    b.heading("Look");
    b.choice("Style", "style", &["Plain", "Smooth", "Glass", "Liquid", "Aurora", "Neon"], &cfg.style);
    let note = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(note, Look::Note("Plain uses 2 MB; the others use far more. Leaving Plain applies at next start.".to_string()), None);
    b.row += 1;
    let w = (WIDTH - 2.0 * MARGIN - 12.0) / 4.0;
    for (n, (text, action)) in [("Edit rules", Action::EditRules), ("Re-sort", Action::Resort), ("Undo and stop", Action::Undo), ("Stop", Action::Stop)].into_iter().enumerate() {
        let r = b.rect(MARGIN + n as f32 * (w + 4.0), w);
        b.add(r, Look::Button(text.to_string()), Some(action));
    }
    b.row += 1;
    b.items
}

/// How big a file's icon is drawn. Icons are made at 32 and 48 pixels; anything between gets
/// shrunk from a larger one and comes out soft, so those sizes are kept wherever they fit.
pub fn icon_pixels(dpi: f32) -> i32 {
    let base = [24.0, 32.0, 48.0][ICON_SIZE.load(Ordering::Relaxed).min(2) as usize];
    snap_icon((base * dpi) as i32)
}

/// The small icon of a list view's row.
pub fn list_icon_pixels(dpi: f32) -> i32 {
    snap_icon((20.0 * dpi) as i32)
}

/// How tall a list view's row is.
pub fn list_row(dpi: f32) -> i32 {
    list_icon_pixels(dpi) + (8.0 * dpi) as i32
}

fn snap_icon(wanted: i32) -> i32 {
    match wanted {
        0..=24 => 24,
        25..=32 => 32,
        33..=48 => 48,
        49..=64 => 64,
        65..=96 => 96,
        wanted => wanted,
    }
}

fn appearance_page(mut b: Builder, cfg: &Config) -> Vec<Item> {
    let fonts: Vec<(&str, &str)> = [("Segoe UI", "segoe ui"), ("Calibri", "calibri"), ("Arial", "arial"), ("Verdana", "verdana"), ("Tahoma", "tahoma")].to_vec();
    b.choice_pairs("Font", "font", &fonts, &cfg.font);
    b.choice("Text size", "text_size", &["Small", "Normal", "Large"], &cfg.text_size);
    b.choice("Icon size", "icon_size", &["Small", "Medium", "Large"], &cfg.icon_size);
    b.choice_pairs("Icon colour", "icon_tint", &[("Original", "off"), ("Section", "section"), ("Grey", "mono")], &cfg.icon_tint);
    b.choice("Panel corners", "corners", &["Square", "Soft", "Round"], &cfg.corners);
    b.stepper("How solid panels are (%)", "general", "glass", cfg.glass as u64, 10, 20, 160);
    b.colour_row("Panel colour", "panel_colour", cfg.panel_colour);
    b.colour_row("Text colour", "text_colour", cfg.text_colour);
    b.colour_row("Border colour", "border_colour", cfg.border_colour);
    b.colour_row("Icon tint colour", "tint_colour", cfg.tint_colour);
    b.switch("Open a section when resting on its label", cfg.hover_open, Action::Set("general", "hover_open", on_off(!cfg.hover_open)));
    b.switch("Fade the labels while unused", cfg.fade, Action::Set("general", "fade", on_off(!cfg.fade)));
    let r = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(r, Look::Button("Reset all colours".to_string()), Some(Action::ClearLook(vec!["panel_colour", "text_colour", "border_colour", "tint_colour", "glass", "panel"])));
    b.row += 1;
    let note = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(note, Look::Note("Icon size and colour apply when Icons is set to Rivulet.".to_string()), None);
    b.items
}

fn sections_page(mut b: Builder, cfg: &Config) -> Vec<Item> {
    let colour = |name: &str| cfg.colours.iter().find(|(s, _)| s == name).map_or(0x00C8B090, |(_, c)| *c);
    let in_tabs = |name: &str| cfg.tabs.iter().any(|(_, list)| list.iter().any(|s| s == name));
    let names: Vec<(String, bool)> = cfg.order.iter().map(|n| (n.clone(), false)).chain(cfg.portals.iter().map(|(n, _)| (n.clone(), true))).collect();
    for (name, portal) in &names {
        let name = name.as_str();
        let swatch = b.rect(MARGIN, 26.0);
        b.add(swatch, Look::Swatch(colour(name)), Some(Action::PickColour(format!("section:{name}"))));
        let label = b.rect(MARGIN + 32.0, 138.0);
        b.add(label, Look::Note(name.to_string()), None);
        let x0 = MARGIN + 174.0;
        let button = |b: &mut Builder, slot: f32, w: f32, text: &str, on: bool, action: Action| {
            let r = b.rect(x0 + slot, w);
            b.add(r, Look::Chip(text.to_string(), on), Some(action));
        };
        if !*portal {
            button(&mut b, 0.0, 28.0, "\u{2191}", false, Action::MoveSection(name.to_string(), -1));
            button(&mut b, 32.0, 28.0, "\u{2193}", false, Action::MoveSection(name.to_string(), 1));
            button(&mut b, 64.0, 44.0, "Tab", in_tabs(name), Action::ToggleTab(name.to_string()));
        }
        button(&mut b, 112.0, 64.0, "Rename", false, Action::RenameSection(name.to_string()));
        button(&mut b, 180.0, 62.0, "Symbol", false, Action::PickSymbol(name.to_string()));
        if name != "Other" {
            button(&mut b, 246.0, 28.0, "\u{00D7}", false, Action::RemoveSection(name.to_string()));
        }
        b.row += 1;
    }
    let w = (WIDTH - 2.0 * MARGIN - 4.0) / 2.0;
    let add = b.rect(MARGIN, w);
    b.add(add, Look::Button("New section".to_string()), Some(Action::AddSection));
    let folder = b.rect(MARGIN + w + 4.0, w);
    b.add(folder, Look::Button("New folder portal".to_string()), Some(Action::AddPortal));
    b.row += 1;
    let note = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(note, Look::Note("Click a colour to change it. Tab joins a section to the one above it. Removing moves no files.".to_string()), None);
    b.items
}

fn rules_page(mut b: Builder, cfg: &Config) -> Vec<Item> {
    const SHOWN: usize = 11;
    for (pattern, section) in cfg.names.iter().take(SHOWN) {
        let r = b.rect(MARGIN, WIDTH - 2.0 * MARGIN - 34.0);
        b.add(r, Look::Note(format!("{pattern}  \u{2192}  {section}")), None);
        let x = b.rect(WIDTH - MARGIN - 28.0, 28.0);
        b.add(x, Look::Chip("\u{00D7}".to_string(), false), Some(Action::RemoveRule(pattern.clone())));
        b.row += 1;
    }
    if cfg.names.is_empty() {
        let r = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
        b.add(r, Look::Note("No rules yet. Files are sorted by their type.".to_string()), None);
        b.row += 1;
    }
    if cfg.names.len() > SHOWN {
        let r = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
        b.add(r, Look::Note(format!("and {} more; see Edit rules on the General page.", cfg.names.len() - SHOWN)), None);
        b.row += 1;
    }
    let add = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(add, Look::Button("New rule".to_string()), Some(Action::AddRule));
    b.row += 1;
    for line in ["The first rule that fits wins. Write a rule as: pattern = section", "Patterns: *report*   size>100mb   size<1kb   age<7d   age>2w   age>1y"] {
        let note = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
        b.add(note, Look::Note(line.to_string()), None);
        b.row += 1;
    }
    b.items
}

fn layouts_page(mut b: Builder, cfg: &Config) -> Vec<Item> {
    let layouts = LAYOUTS.lock().map(|l| l.clone()).unwrap_or_default();
    for name in layouts.iter().take(11) {
        let r = b.rect(MARGIN, 300.0);
        b.add(r, Look::Note(name.clone()), None);
        let load = b.rect(WIDTH - MARGIN - 110.0, 76.0);
        b.add(load, Look::Chip("Switch".to_string(), false), Some(Action::LoadLayout(name.clone())));
        let x = b.rect(WIDTH - MARGIN - 28.0, 28.0);
        b.add(x, Look::Chip("\u{00D7}".to_string(), false), Some(Action::RemoveLayout(name.clone())));
        b.row += 1;
    }
    if layouts.is_empty() {
        let r = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
        b.add(r, Look::Note("No saved layouts yet.".to_string()), None);
        b.row += 1;
    }
    let add = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(add, Look::Button("Save this layout".to_string()), Some(Action::SaveLayout));
    b.row += 1;
    b.switch("Keep a layout for each screen setup", cfg.monitor_profiles, Action::Set("general", "monitor_profiles", on_off(!cfg.monitor_profiles)));
    let note = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(note, Look::Note("A layout holds sections, rules and look. Ctrl+Alt+Page Up / Down moves between them.".to_string()), None);
    b.items
}

fn symbols_page(mut b: Builder, _cfg: &Config) -> Vec<Item> {
    let section = SYMBOL_FOR.lock().map(|s| s.clone()).unwrap_or_default();
    let title = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(title, Look::Note(format!("Choose a symbol for {section}")), None);
    b.row += 1;
    const ACROSS: usize = 12;
    let w = (WIDTH - 2.0 * MARGIN) / ACROSS as f32;
    for (n, code) in SYMBOLS.iter().enumerate() {
        let r = b.rect(MARGIN + (n % ACROSS) as f32 * w, w - 4.0);
        let glyph = char::from_u32(*code).unwrap_or(' ');
        b.add(r, Look::Symbol(glyph), Some(Action::SetSymbol(section.clone(), *code)));
        if n % ACROSS == ACROSS - 1 {
            b.row += 1;
        }
    }
    let usual = b.rect(MARGIN, WIDTH - 2.0 * MARGIN);
    b.add(usual, Look::Button("Use the usual symbol".to_string()), Some(Action::ClearSymbol(section)));
    b.row += 1;
    b.items
}

impl Builder {
    fn colour_row(&mut self, text: &str, key: &'static str, current: Option<u32>) {
        self.label(text);
        let chooser = self.rect(WIDTH - MARGIN - 138.0, 58.0);
        match current {
            Some(colour) => {
                self.add(chooser, Look::Swatch(colour), Some(Action::PickColour(key.to_string())));
                let reset = self.rect(WIDTH - MARGIN - 74.0, 74.0);
                self.add(reset, Look::Chip("Reset".to_string(), false), Some(Action::ClearLook(vec![key])));
            }
            None => {
                let wider = self.rect(WIDTH - MARGIN - 138.0, 138.0);
                self.add(wider, Look::Chip("Choose".to_string(), false), Some(Action::PickColour(key.to_string())));
            }
        }
        self.row += 1;
    }
}

const HEADER_ROW: f32 = 34.0;

/// What a panel's top shows.
pub struct Header {
    pub title: String,
    pub sort: Option<String>,
    pub list: bool,
    pub pinned: bool,
    pub tabs: Vec<(String, bool)>,
    /// A portal's folder, and whether Back and Up can be used.
    pub nav: Option<(PathBuf, bool, bool)>,
}

pub fn header_rows(tabs: bool, nav: bool) -> i32 {
    1 + i32::from(tabs) + i32::from(nav)
}

pub fn header_height(rows: i32, dpi: f32) -> i32 {
    (rows as f32 * HEADER_ROW * dpi) as i32
}

/// The rows at a panel's top: its title and controls, then a portal's buttons, then any tabs.
pub fn build_header(h: &Header, width: i32, pad: i32, dpi: f32) -> Vec<Item> {
    let px = |v: f32| (v * dpi) as i32;
    let row_h = px(HEADER_ROW);
    let mut items = vec![];
    let mut push = |r: RECT, look: Look, action: Option<Action>| items.push(Item { rect: r, look, action, row: false, detail: String::new() });
    let (top, bottom) = (px(6.0), row_h - px(2.0));
    let mut right = width - pad - px(6.0);
    let button = |text: String, on: bool, w: f32, action: Action, right: &mut i32, top: i32, bottom: i32, push: &mut dyn FnMut(RECT, Look, Option<Action>)| {
        let left = *right - px(w);
        push(RECT { left, top, right: *right, bottom }, Look::Chip(text, on), Some(action));
        *right = left - px(4.0);
    };
    button(if h.pinned { "Pinned" } else { "Pin" }.to_string(), h.pinned, 58.0, Action::TogglePin, &mut right, top, bottom, &mut push);
    button(if h.list { "List" } else { "Grid" }.to_string(), false, 48.0, Action::ToggleView, &mut right, top, bottom, &mut push);
    if let Some(sort) = &h.sort {
        button(sort.clone(), false, 70.0, Action::CycleSort, &mut right, top, bottom, &mut push);
    }
    push(RECT { left: pad + px(8.0), top, right: right.max(pad + px(8.0)), bottom }, Look::Heading(h.title.clone()), None);
    let mut row = 1;
    if let Some((folder, back, up)) = &h.nav {
        let (t, b) = (row * row_h + px(2.0), (row + 1) * row_h - px(2.0));
        let mut x = pad + px(8.0);
        for (text, on, w, action) in [("Back", *back, 58.0, Action::Back), ("Up", *up, 50.0, Action::Up), ("Open folder", true, 96.0, Action::OpenFolder(folder.clone()))] {
            push(RECT { left: x, top: t, right: x + px(w), bottom: b }, Look::Chip(text.to_string(), false), on.then_some(action));
            x += px(w + 4.0);
        }
        row += 1;
    }
    if !h.tabs.is_empty() {
        let (t, b) = (row * row_h + px(2.0), (row + 1) * row_h - px(2.0));
        let w = ((width - 2 * pad - px(16.0)) / h.tabs.len() as i32).min(px(130.0));
        for (n, (name, on)) in h.tabs.iter().enumerate() {
            let left = pad + px(8.0) + n as i32 * w;
            push(RECT { left, top: t, right: left + w - px(4.0), bottom: b }, Look::Chip(name.clone(), *on), Some(Action::Show(name.clone())));
        }
    }
    items
}

/// How files lie in a panel, and how far it has been scrolled.
#[derive(Clone, Copy)]
pub struct Grid {
    pub across: i32,
    pub cell: (i32, i32),
    pub pad: i32,
    pub top: i32,
    pub scroll: i32,
    pub visible_rows: i32,
    pub list: bool,
}

/// The files' cells. Those scrolled out of view stay in the list with no size, so a selection and
/// its numbering survive scrolling; nothing can be drawn at or click on an empty rectangle.
pub fn file_cells(files: &[(Option<PathBuf>, String, isize, String)], g: Grid) -> Vec<Item> {
    let across = g.across.max(1);
    let mut items: Vec<Item> = files
        .iter()
        .enumerate()
        .map(|(n, (path, file, icon, detail))| {
            let (col, row) = (n as i32 % across, n as i32 / across - g.scroll);
            let shows = row >= 0 && row < g.visible_rows;
            let (left, top) = (g.pad + col * g.cell.0, g.top + row * g.cell.1);
            let rect = if shows { RECT { left, top, right: left + g.cell.0, bottom: top + g.cell.1 } } else { RECT { left: 0, top: 0, right: 0, bottom: 0 } };
            Item { rect, look: Look::File(file.clone(), *icon), action: path.clone().map(Action::Open), row: g.list, detail: detail.clone() }
        })
        .collect();
    let total = (files.len() as i32 + across - 1) / across;
    if total > g.visible_rows && g.visible_rows > 0 {
        let view = g.visible_rows * g.cell.1;
        let thumb = (view * g.visible_rows / total).max(view / 12).max(8);
        let y = g.top + g.scroll * (view - thumb) / (total - g.visible_rows).max(1);
        let right = g.pad + across * g.cell.0 + g.pad - 2;
        let width = (g.pad / 2).max(3);
        items.push(Item { rect: RECT { left: right - width, top: y, right, bottom: y + thumb }, look: Look::Thumb, action: None, row: false, detail: String::new() });
    }
    items
}

/// The row a file is on, and the scroll that shows it with the least movement.
pub fn scroll_to_show(index: i32, across: i32, visible_rows: i32, scroll: i32) -> i32 {
    let row = index / across.max(1);
    if row < scroll {
        row
    } else if row >= scroll + visible_rows {
        row - visible_rows + 1
    } else {
        scroll
    }
}

/// Which files a press selects. Ctrl adds or removes one, Shift takes the run from the last one
/// pressed, and a plain press on one of several keeps them all, as it may be the start of a drag.
pub fn pick(current: &[i32], index: i32, anchor: i32, ctrl: bool, shift: bool) -> Vec<i32> {
    if shift && anchor >= 0 {
        return (anchor.min(index)..=anchor.max(index)).collect();
    }
    if ctrl {
        return if current.contains(&index) { current.iter().copied().filter(|n| *n != index).collect() } else { current.iter().copied().chain([index]).collect() };
    }
    if current.contains(&index) {
        return current.to_vec();
    }
    vec![index]
}

/// The files a drawn box touches.
pub fn in_box(items: &[Item], area: RECT) -> Vec<i32> {
    let touches = |r: &RECT| r.left < area.right && area.left < r.right && r.top < area.bottom && area.top < r.bottom;
    items.iter().enumerate().filter(|(_, i)| matches!(i.action, Some(Action::Open(_))) && touches(&i.rect)).map(|(n, _)| n as i32).collect()
}

/// Where an arrow key goes from this file, among `count` files laid out `across` to a row.
pub fn arrow(index: i32, key: Arrow, across: i32, count: i32) -> i32 {
    let to = match key {
        Arrow::Left => index - 1,
        Arrow::Right => index + 1,
        Arrow::Up => index - across,
        Arrow::Down => index + across,
    };
    if (0..count).contains(&to) { to } else { index }
}

#[derive(Clone, Copy)]
pub enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

/// Which item is under a point inside the panel.
pub fn item_at(items: &[Item], x: i32, y: i32) -> Option<usize> {
    items.iter().position(|i| {
        // cells touch, so a file answers only inside its cell: the gap is where a selection box can start
        let (gap_x, gap_y) = if i.row {
            (4, 0)
        } else if matches!(i.look, Look::File(..)) { ((i.rect.right - i.rect.left) / 10, (i.rect.bottom - i.rect.top) / 14) } else { (0, 0) };
        x >= i.rect.left + gap_x && x < i.rect.right - gap_x && y >= i.rect.top + gap_y && y < i.rect.bottom - gap_y
    })
}

/// What a click at this spot inside the panel does.
pub fn action_at(items: &[Item], x: i32, y: i32) -> Option<Action> {
    item_at(items, x, y).and_then(|i| items[i].action.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{parse_config, set_setting};

    const INI: &str = include_str!("../sections.ini");

    #[test]
    fn everything_fits_and_nothing_overlaps() {
        let cfg = parse_config(INI).unwrap();
        for dpi in [1.0, 1.25, 1.5, 2.0] {
            let items = build(&cfg, false, dpi);
            let (w, h) = size(&cfg, dpi);
            for (n, a) in items.iter().enumerate() {
                assert!(a.rect.left >= 0 && a.rect.right <= w && a.rect.top >= 0 && a.rect.bottom <= h, "{:?} at {dpi}", a.look);
                for b in &items[n + 1..] {
                    let apart = a.rect.right <= b.rect.left || b.rect.right <= a.rect.left || a.rect.bottom <= b.rect.top || b.rect.bottom <= a.rect.top;
                    assert!(apart, "{:?} overlaps {:?}", a.look, b.look);
                }
            }
        }
    }

    #[test]
    fn clicking_each_setting_writes_a_file_that_loads_and_means_it() {
        let cfg = parse_config(INI).unwrap();
        let items = build(&cfg, false, 1.25);
        let mut tried = 0;
        for item in &items {
            if let Some(Action::Set(part, key, value)) = &item.action {
                let text = set_setting(INI, part, key, value);
                let changed = parse_config(&text).unwrap_or_else(|e| panic!("{key}={value}: {e}"));
                match *key {
                    "theme" => assert_eq!(changed.theme, *value),
                    "speed" => assert_eq!(changed.speed, *value),
                    "side" => assert_eq!(changed.right_side, value == "right"),
                    "dots" => assert_eq!(changed.dots, value == "on"),
                    "click_away" => assert_eq!(changed.click_away, value == "on"),
                    "double_click_hide" => assert_eq!(changed.double_click_hide, value == "on"),
                    "icons" => assert_eq!(changed.own_icons, value == "own"),
                    "peek" => assert_eq!(changed.peek, value == "on"),
                    "style" => assert_eq!(changed.style, *value),
                    "days" => assert_eq!(changed.recent_days.to_string(), *value),
                    "limit" => assert_eq!(changed.recent_limit.to_string(), *value),
                    other => panic!("untested setting {other}"),
                }
                tried += 1;
            }
        }
        assert!(tried >= 16, "{tried}");
    }

    #[test]
    fn the_icon_choice_offers_explorer_and_rivulet() {
        let cfg = parse_config(INI).unwrap();
        let items = build(&cfg, false, 1.0);
        let chips: Vec<_> = items.iter().filter(|i| matches!(&i.action, Some(Action::Set("general", "icons", _)))).collect();
        assert_eq!(chips.len(), 2);
        assert_eq!(chips[0].look, Look::Chip("Explorer".to_string(), true)); // the default
        assert_eq!(chips[1].look, Look::Chip("Rivulet".to_string(), false));
        assert_eq!(chips[1].action, Some(Action::Set("general", "icons", "own".to_string())));
    }

    #[test]
    fn switches_show_their_state_and_offer_the_opposite() {
        let mut cfg = parse_config(INI).unwrap();
        cfg.dots = false;
        let items = build(&cfg, true, 1.0);
        let dots = items.iter().position(|i| i.look == Look::Text("New-file dots".to_string())).unwrap();
        assert_eq!(items[dots + 1].look, Look::Switch(false));
        assert_eq!(items[dots + 1].action, Some(Action::Set("general", "dots", "on".to_string())));
        assert!(items.iter().any(|i| i.action == Some(Action::Login(false))));
    }

    #[test]
    fn steppers_stop_at_their_limits() {
        let mut cfg = parse_config(INI).unwrap();
        cfg.recent_days = 0;
        let items = build(&cfg, false, 1.0);
        assert!(items.iter().any(|i| i.action == Some(Action::Set("recent", "days", "0".to_string())))); // can't go below
        assert!(items.iter().any(|i| i.action == Some(Action::Set("recent", "days", "1".to_string()))));
    }

    fn cells(n: i32, across: i32, scroll: i32, visible: i32) -> Vec<Item> {
        let files: Vec<_> = (0..n).map(|k| (Some(PathBuf::from(format!("C:\\d\\{k}.txt"))), format!("{k}.txt"), 0, String::new())).collect();
        file_cells(&files, Grid { across, cell: (100, 90), pad: 8, top: 8, scroll, visible_rows: visible, list: false })
    }

    #[test]
    fn section_cells_start_at_the_pad_and_wrap_in_order() {
        let items = cells(5, 3, 0, 9);
        assert_eq!(items.len(), 5);
        assert_eq!((items[0].rect.left, items[0].rect.top), (8, 8));
        assert_eq!(items[2].rect.left, 208);
        assert_eq!((items[3].rect.left, items[3].rect.top), (8, 98));
        assert_eq!(items[4].action, Some(Action::Open(PathBuf::from("C:\\d\\4.txt"))));
        let loose = [(None, "mystery".to_string(), 0, String::new())];
        let g = Grid { across: 3, cell: (100, 90), pad: 8, top: 8, scroll: 0, visible_rows: 1, list: false };
        assert_eq!(file_cells(&loose, g)[0].action, None);
    }

    #[test]
    fn scrolling_hides_rows_keeps_numbering_and_shows_a_bar() {
        let items = cells(30, 3, 2, 4); // ten rows, four showing, two scrolled past
        assert_eq!(items.len(), 31); // every file, and the bar
        assert!(matches!(items[30].look, Look::Thumb));
        assert_eq!(items[0].rect.right, 0); // scrolled past
        assert_eq!(items[6].rect.top, 8); // the first row showing
        assert_eq!(items[17].rect.top, 8 + 3 * 90); // the last row showing
        assert_eq!(items[18].rect.right, 0);
        assert_eq!(item_at(&items, 0, 0), None); // nothing can be hit where the hidden ones lie
        assert_eq!(cells(9, 3, 0, 4).len(), 9); // no bar when all fits
    }

    #[test]
    fn keyboard_moves_scroll_just_enough() {
        assert_eq!(scroll_to_show(0, 3, 4, 2), 0);
        assert_eq!(scroll_to_show(9, 3, 4, 2), 2);
        assert_eq!(scroll_to_show(15, 3, 4, 0), 2);
    }

    #[test]
    fn the_header_fits_and_each_part_acts() {
        let h = Header { title: "Downloads".into(), sort: Some("Newest".into()), list: false, pinned: false, tabs: vec![("A".into(), true), ("B".into(), false)], nav: Some((PathBuf::from("C:\\d"), false, true)) };
        let (width, height) = (450, header_height(header_rows(true, true), 1.25));
        let items = build_header(&h, width, 8, 1.25);
        for a in &items {
            assert!(a.rect.left >= 0 && a.rect.right <= width && a.rect.bottom <= height, "{:?}", a.look);
        }
        let has = |want: &Action| items.iter().any(|i| i.action.as_ref() == Some(want));
        assert!(has(&Action::CycleSort) && has(&Action::ToggleView) && has(&Action::TogglePin) && has(&Action::Up) && has(&Action::Show("B".into())));
        assert!(!has(&Action::Back)); // nothing to go back to yet
    }

    #[test]
    fn every_settings_page_fits_and_writes_files_that_load() {
        let mut cfg = parse_config(INI).unwrap();
        cfg.names.push(("*x*".into(), "Word".into()));
        *SYMBOL_FOR.lock().unwrap() = "Word".into();
        *LAYOUTS.lock().unwrap() = vec!["Work".into(), "Home".into()];
        for page in 0..=5u8 {
            for dpi in [1.0, 1.25, 1.5, 2.0] {
                let items = build_page(&cfg, false, dpi, page);
                let w = (WIDTH * dpi) as i32;
                for (n, a) in items.iter().enumerate() {
                    assert!(a.rect.right <= w, "page {page} {:?}", a.look);
                    for b in &items[n + 1..] {
                        let apart = a.rect.right <= b.rect.left || b.rect.right <= a.rect.left || a.rect.bottom <= b.rect.top || b.rect.bottom <= a.rect.top;
                        assert!(apart, "page {page}: {:?} overlaps {:?}", a.look, b.look);
                    }
                }
            }
            for item in build_page(&cfg, false, 1.25, page) {
                if let Some(Action::Set(part, key, value)) = &item.action {
                    parse_config(&set_setting(INI, part, key, value)).unwrap_or_else(|e| panic!("{key}={value}: {e}"));
                }
            }
        }
    }

    #[test]
    fn the_gap_between_files_belongs_to_no_file() {
        let g = Grid { across: 1, cell: (100, 140), pad: 0, top: 0, scroll: 0, visible_rows: 1, list: false };
        let items = file_cells(&[(Some(PathBuf::from("C:\\d\\a")), "a".to_string(), 0, String::new())], g);
        assert_eq!(item_at(&items, 50, 70), Some(0));
        assert_eq!(item_at(&items, 3, 70), None);
        assert_eq!(item_at(&items, 50, 3), None);
    }

    #[test]
    fn pressing_selects_as_explorer_does() {
        assert_eq!(pick(&[], 3, -1, false, false), vec![3]);
        assert_eq!(pick(&[1], 3, 1, false, false), vec![3]);
        assert_eq!(pick(&[1, 3], 3, 1, false, false), vec![1, 3]); // may be the start of a drag
        assert_eq!(pick(&[1], 3, 1, true, false), vec![1, 3]);
        assert_eq!(pick(&[1, 3], 3, 3, true, false), vec![1]);
        assert_eq!(pick(&[1], 4, 1, false, true), vec![1, 2, 3, 4]);
        assert_eq!(pick(&[4], 1, 4, false, true), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_box_selects_only_the_files_it_touches() {
        let g = Grid { across: 3, cell: (100, 100), pad: 0, top: 0, scroll: 0, visible_rows: 2, list: false };
        let files: Vec<_> = (0..6).map(|n| (Some(PathBuf::from(format!("C:\\d\\{n}"))), n.to_string(), 0, String::new())).collect();
        let items = file_cells(&files, g);
        assert_eq!(in_box(&items, RECT { left: 50, top: 50, right: 150, bottom: 60 }), vec![0, 1]);
        assert_eq!(in_box(&items, RECT { left: 50, top: 50, right: 60, bottom: 150 }), vec![0, 3]);
        assert_eq!(in_box(&items, RECT { left: 400, top: 0, right: 500, bottom: 500 }), Vec::<i32>::new());
    }

    #[test]
    fn arrows_move_within_the_grid_and_stop_at_its_edges() {
        assert_eq!(arrow(4, Arrow::Right, 3, 6), 5);
        assert_eq!(arrow(4, Arrow::Up, 3, 6), 1);
        assert_eq!(arrow(0, Arrow::Left, 3, 6), 0);
        assert_eq!(arrow(5, Arrow::Right, 3, 6), 5);
        assert_eq!(arrow(4, Arrow::Down, 3, 6), 4);
    }

    #[test]
    fn large_and_small_icons_keep_sizes_they_are_made_at() {
        assert_eq!(snap_icon(30), 32);
        assert_eq!(snap_icon(60), 64);
        assert_eq!(snap_icon(72), 96);
    }

    #[test]
    fn icons_keep_the_sizes_they_are_made_at() {
        assert_eq!(icon_pixels(1.0), 32);
        assert_eq!(icon_pixels(1.25), 48);
        assert_eq!(icon_pixels(1.5), 48);
        assert_eq!(icon_pixels(2.0), 64);
    }

    #[test]
    fn clicks_find_the_right_control() {
        let cfg = parse_config(INI).unwrap();
        let items = build(&cfg, false, 1.25);
        let stop = items.iter().find(|i| i.action == Some(Action::Stop)).unwrap();
        assert_eq!(action_at(&items, stop.rect.left + 2, stop.rect.top + 2), Some(Action::Stop));
        assert_eq!(action_at(&items, 0, 0), None);
    }
}
