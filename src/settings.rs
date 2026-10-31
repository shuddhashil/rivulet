//! The Settings panel, as plain data: what to draw and what each click does. Drawing and clicks
//! both read this one list, so they can't disagree, and `cargo test` can check it.

use crate::rules::Config;
use windows_sys::Win32::Foundation::RECT;

pub const SETTINGS: &str = "Settings";

const ROW: f32 = 34.0;
const WIDTH: f32 = 520.0;
const MARGIN: f32 = 16.0;
const LABEL_W: f32 = 170.0;
pub const THEMES: [&str; 5] = ["Wallpaper", "Midnight", "Graphite", "Forest", "Rose"];
const SPEEDS: [&str; 4] = ["Fast", "Normal", "Relaxed", "Off"];
const ROWS: i32 = 15;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Write this value to sections.ini: [part] key = value.
    Set(&'static str, &'static str, String),
    Login(bool),
    EditRules,
    Resort,
    Undo,
    Stop,
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
        self.label(text);
        let room = WIDTH - MARGIN * 2.0 - LABEL_W;
        let w = (room - 4.0 * (options.len() as f32 - 1.0)) / options.len() as f32;
        for (n, option) in options.iter().enumerate() {
            let r = self.rect(MARGIN + LABEL_W + n as f32 * (w + 4.0), w);
            let on = option.eq_ignore_ascii_case(current);
            self.add(r, Look::Chip(option.to_string(), on), Some(Action::Set("general", key, option.to_lowercase())));
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

/// What a click at this spot inside the panel does.
pub fn action_at(items: &[Item], x: i32, y: i32) -> Option<Action> {
    items.iter().find(|i| x >= i.rect.left && x < i.rect.right && y >= i.rect.top && y < i.rect.bottom).and_then(|i| i.action.clone())
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
                    "style" => assert_eq!(changed.style, *value),
                    "days" => assert_eq!(changed.recent_days.to_string(), *value),
                    "limit" => assert_eq!(changed.recent_limit.to_string(), *value),
                    other => panic!("untested setting {other}"),
                }
                tried += 1;
            }
        }
        assert!(tried >= 15, "{tried}");
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
    fn clicks_find_the_right_control() {
        let cfg = parse_config(INI).unwrap();
        let items = build(&cfg, false, 1.25);
        let stop = items.iter().find(|i| i.action == Some(Action::Stop)).unwrap();
        assert_eq!(action_at(&items, stop.rect.left + 2, stop.rect.top + 2), Some(Action::Stop));
        assert_eq!(action_at(&items, 0, 0), None);
    }
}
