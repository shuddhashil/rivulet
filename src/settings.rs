//! The Settings panel, as plain data: what to draw and what each click does. Drawing and clicks
//! both read this one list, so they can't disagree, and `cargo test` can check it.

use crate::rules::Config;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::RECT;

pub const SETTINGS: &str = "Settings";

const ROW: f32 = 34.0;
const WIDTH: f32 = 520.0;
const MARGIN: f32 = 16.0;
const LABEL_W: f32 = 170.0;
pub const THEMES: [&str; 5] = ["Wallpaper", "Midnight", "Graphite", "Forest", "Rose"];
const SPEEDS: [&str; 4] = ["Fast", "Normal", "Relaxed", "Off"];
const ROWS: i32 = 16;

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
    /// A file in a portal: its name and its icon handle.
    File(String, isize),
}

#[derive(Clone)]
pub struct Item {
    pub rect: RECT,
    pub look: Look,
    pub action: Option<Action>,
}

pub fn size(dpi: f32) -> (i32, i32) {
    ((WIDTH * dpi) as i32, ((ROWS as f32 * ROW + MARGIN) * dpi) as i32)
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
        self.items.push(Item { rect, look, action });
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

    fn stepper(&mut self, text: &str, key: &'static str, value: u64, low: u64, high: u64) {
        self.label(text);
        let less = self.rect(WIDTH - MARGIN - 110.0, 32.0);
        self.add(less, Look::Button("\u{2212}".to_string()), Some(Action::Set("recent", key, value.saturating_sub(1).max(low).to_string())));
        let shown = self.rect(WIDTH - MARGIN - 76.0, 42.0);
        self.add(shown, Look::Value(value.to_string()), None);
        let more = self.rect(WIDTH - MARGIN - 32.0, 32.0);
        self.add(more, Look::Button("+".to_string()), Some(Action::Set("recent", key, (value + 1).min(high).to_string())));
        self.row += 1;
    }
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.to_string()
}

/// `login` says whether Rivulet is set to start at login.
pub fn build(cfg: &Config, login: bool, dpi: f32) -> Vec<Item> {
    let mut b = Builder { dpi, row: 0, items: vec![] };
    b.heading("Settings");
    b.choice("Theme", "theme", &THEMES, &cfg.theme);
    b.choice("Labels on", "side", &["Left", "Right"], if cfg.right_side { "right" } else { "left" });
    b.choice_pairs("Icons", "icons", &[("Explorer", "explorer"), ("Rivulet", "own")], if cfg.own_icons { "own" } else { "explorer" });
    b.choice("Animation", "speed", &SPEEDS, &cfg.speed);
    b.switch("New-file dots", cfg.dots, Action::Set("general", "dots", on_off(!cfg.dots)));
    b.switch("Click empty desktop to close", cfg.click_away, Action::Set("general", "click_away", on_off(!cfg.click_away)));
    b.switch("Double-click desktop hides labels", cfg.double_click_hide, Action::Set("general", "double_click_hide", on_off(!cfg.double_click_hide)));
    b.switch("Start at login", login, Action::Login(!login));
    b.heading("Recent");
    b.stepper("Days to look back (0 is off)", "days", cfg.recent_days, 0, 30);
    b.stepper("Most items to show", "limit", cfg.recent_limit as u64, 1, 24);
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
    debug_assert_eq!(b.row, ROWS);
    b.items
}

/// How big a file's icon is drawn. Icons are made at 32 and 48 pixels; anything between gets
/// shrunk from a larger one and comes out soft, so those sizes are kept wherever they fit.
pub fn icon_pixels(dpi: f32) -> i32 {
    match (32.0 * dpi) as i32 {
        0..=32 => 32,
        33..=48 => 48,
        wanted => wanted,
    }
}

/// A portal shows its newest files only, so it stays quick and always fits on the screen.
pub const PORTAL_SHOWN: usize = 24;

pub fn portal_header(dpi: f32) -> i32 {
    (ROW * dpi) as i32
}

/// The cells of a panel's file grid. `top` is where the first row starts.
fn file_cells(files: &[(Option<PathBuf>, String, isize)], across: i32, cell: (i32, i32), pad: i32, top: i32) -> Vec<Item> {
    let across = across.max(1);
    files
        .iter()
        .enumerate()
        .map(|(n, (path, file, icon))| {
            let (col, row) = (n as i32 % across, n as i32 / across);
            let (left, top) = (pad + col * cell.0, top + row * cell.1);
            Item {
                rect: RECT { left, top, right: left + cell.0, bottom: top + cell.1 },
                look: Look::File(file.clone(), *icon),
                action: path.clone().map(Action::Open),
            }
        })
        .collect()
}

/// A portal's panel, as plain data like Settings. `files` holds (path, name, icon), and `total` is
/// how many the folder really has. `cell` and `pad` match the desktop's own icon grid.
pub fn build_portal(name: &str, folder: &Path, files: &[(PathBuf, String, isize)], total: usize, across: i32, cell: (i32, i32), pad: i32, dpi: f32) -> Vec<Item> {
    let px = |v: f32| (v * dpi) as i32;
    let width = across * cell.0 + 2 * pad;
    let header = portal_header(dpi);
    let button_w = px(100.0);
    let button = RECT { left: width - pad - button_w - px(6.0), top: px(6.0), right: width - pad - px(6.0), bottom: header - px(2.0) };
    let title = if total > files.len() { format!("{name}: newest {} of {total}", files.len()) } else { name.to_string() };
    let mut items = vec![
        Item { rect: RECT { left: pad + px(8.0), top: px(6.0), right: button.left - px(6.0), bottom: header - px(2.0) }, look: Look::Heading(title), action: None },
        Item { rect: button, look: Look::Button("Open folder".to_string()), action: Some(Action::OpenFolder(folder.to_path_buf())) },
    ];
    if files.is_empty() {
        items.push(Item { rect: RECT { left: pad + px(8.0), top: header + pad, right: width - pad, bottom: header + pad + cell.1 }, look: Look::Note("This folder is empty.".to_string()), action: None });
    }
    let cells: Vec<_> = files.iter().map(|(path, file, icon)| (Some(path.clone()), file.clone(), *icon)).collect();
    items.extend(file_cells(&cells, across, cell, pad, header + pad));
    items
}

/// A section drawn by Rivulet itself: the same cells as a portal, without its header.
pub fn build_section(files: &[(Option<PathBuf>, String, isize)], across: i32, cell: (i32, i32), pad: i32) -> Vec<Item> {
    file_cells(files, across, cell, pad, pad)
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
        let (gap_x, gap_y) = if matches!(i.look, Look::File(..)) { ((i.rect.right - i.rect.left) / 10, (i.rect.bottom - i.rect.top) / 14) } else { (0, 0) };
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
            let (w, h) = size(dpi);
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

    #[test]
    fn portal_cells_fit_and_each_opens_its_own_file() {
        let files: Vec<_> = (0..7).map(|n| (PathBuf::from(format!("C:\\d\\{n}.txt")), format!("{n}.txt"), 0)).collect();
        let items = build_portal("Downloads", Path::new("C:\\d"), &files, 7, 3, (100, 90), 8, 1.25);
        let (w, h) = (3 * 100 + 16, portal_header(1.25) + 3 * 90 + 16);
        for a in &items {
            assert!(a.rect.left >= 0 && a.rect.right <= w && a.rect.top >= 0 && a.rect.bottom <= h, "{:?}", a.look);
        }
        let last = items.last().unwrap();
        assert_eq!(action_at(&items, last.rect.left + 50, last.rect.top + 45), Some(Action::Open(PathBuf::from("C:\\d\\6.txt"))));
        assert!(items.iter().any(|i| i.action == Some(Action::OpenFolder(PathBuf::from("C:\\d")))));
    }

    #[test]
    fn a_portal_says_when_it_is_empty_or_cut_short() {
        let empty = build_portal("D", Path::new("C:\\d"), &[], 0, 3, (100, 90), 8, 1.0);
        assert!(empty.iter().any(|i| i.look == Look::Note("This folder is empty.".to_string())));
        let one = [(PathBuf::from("C:\\d\\a"), "a".to_string(), 0)];
        let cut = build_portal("D", Path::new("C:\\d"), &one, 50, 3, (100, 90), 8, 1.0);
        assert_eq!(cut[0].look, Look::Heading("D: newest 1 of 50".to_string()));
    }

    #[test]
    fn the_gap_between_files_belongs_to_no_file() {
        let files = [(Some(PathBuf::from("C:\\d\\a")), "a".to_string(), 0)];
        let items = build_section(&files, 1, (100, 140), 0);
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
        let files: Vec<_> = (0..6).map(|n| (Some(PathBuf::from(format!("C:\\d\\{n}"))), n.to_string(), 0)).collect();
        let items = build_section(&files, 3, (100, 100), 0); // rows of three, each cell 100 square
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
    fn icons_keep_the_sizes_they_are_made_at() {
        assert_eq!(icon_pixels(1.0), 32);
        assert_eq!(icon_pixels(1.25), 48);
        assert_eq!(icon_pixels(1.5), 48);
        assert_eq!(icon_pixels(2.0), 64);
    }

    #[test]
    fn section_cells_start_at_the_pad_and_wrap_in_order() {
        let files: Vec<_> = (0..5)
            .map(|n| (Some(PathBuf::from(format!("C:\\d\\{n}.txt"))), format!("{n}.txt"), 0))
            .collect();
        let items = build_section(&files, 3, (100, 90), 8);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].rect.left, 8);
        assert_eq!(items[0].rect.top, 8);
        assert_eq!(items[1].rect.left, 108);
        assert_eq!(items[2].rect.left, 208);
        assert_eq!(items[3].rect.left, 8);
        assert_eq!(items[3].rect.top, 98);
        assert_eq!(items[4].action, Some(Action::Open(PathBuf::from("C:\\d\\4.txt"))));
        // an item with no path is still drawn, but opens nothing
        let loose = [(None, "mystery".to_string(), 0)];
        assert_eq!(build_section(&loose, 3, (100, 90), 8)[0].action, None);
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
