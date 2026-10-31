//! Sorting, layout and motion maths. No Windows calls, so `cargo test` covers all of it.

pub const RECENT: &str = "Recent";

#[derive(Clone)]
pub struct Config {
    pub types: Vec<(String, Vec<String>)>,
    pub names: Vec<(String, String)>,
    /// Section order, top to bottom on the rail.
    pub order: Vec<String>,
    /// Colours as Windows wants them: blue, green, red.
    pub colours: Vec<(String, u32)>,
    pub recent_days: u64,
    pub recent_limit: usize,
    /// 0 is the main screen; 1, 2... count screens from the left.
    pub monitor: usize,
    /// Empty turns the shortcut off.
    pub hotkey: String,
    pub right_side: bool,
    pub theme: String,
    pub speed: String,
    pub dots: bool,
    pub click_away: bool,
    pub double_click_hide: bool,
    /// plain, smooth, glass, liquid, aurora or neon. Anything but plain needs the graphics libraries.
    pub style: String,
}

fn on_off(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("sections.ini: {key} should be on or off")),
    }
}

/// A theme's label and panel colours (blue, green, red). None means follow the wallpaper.
pub fn theme_tints(theme: &str) -> Option<(u32, u32)> {
    match theme {
        "midnight" => Some((0x00301C14, 0x001E100A)),
        "graphite" => Some((0x002A2826, 0x00181716)),
        "forest" => Some((0x001C2C14, 0x0010190A)),
        "rose" => Some((0x0028183A, 0x00180E24)),
        _ => None,
    }
}

/// How long opening and closing take, in seconds.
pub fn speed_times(speed: &str) -> (f32, f32) {
    match speed {
        "fast" => (0.15, 0.08),
        "relaxed" => (0.38, 0.18),
        "off" => (0.0, 0.0),
        _ => (0.24, 0.11),
    }
}

/// The rules file with [part] key = value set, keeping everything else exactly as it was.
pub fn set_setting(text: &str, part: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let header = format!("[{part}]");
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut inside = false;
    let mut at = None;
    for (i, l) in lines.iter_mut().enumerate() {
        let trimmed = l.trim().to_string();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
            if inside {
                at = Some(i);
            }
        } else if inside && !trimmed.starts_with('#') && trimmed.rsplit_once('=').is_some_and(|(k, _)| k.trim() == key) {
            *l = line;
            return lines.join("\n") + "\n";
        }
    }
    match at {
        Some(i) => lines.insert(i + 1, line),
        None => lines.extend(["".to_string(), header, line]),
    }
    lines.join("\n") + "\n"
}

pub fn parse_config(text: &str) -> Result<Config, String> {
    let mut cfg = Config {
        types: vec![],
        names: vec![],
        order: vec![],
        colours: vec![],
        recent_days: 3,
        recent_limit: 12,
        monitor: 0,
        hotkey: "ctrl+alt+d".to_string(),
        right_side: false,
        theme: "wallpaper".to_string(),
        speed: "normal".to_string(),
        dots: true,
        click_away: true,
        double_click_hide: true,
        style: "plain".to_string(),
    };
    let mut old_pretty = false;
    let mut style_given = false;
    let mut part = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            part = line[1..line.len() - 1].to_string();
            continue;
        }
        // from the right, because a pinned file's name may itself contain =
        let Some((key, value)) = line.rsplit_once('=') else {
            return Err(format!("sections.ini: cannot read this line: {line}"));
        };
        let (key, value) = (key.trim().to_string(), value.trim());
        match part.as_str() {
            "types" => cfg.types.push((key, value.to_lowercase().split_whitespace().map(String::from).collect())),
            "names" => cfg.names.push((key, value.to_string())),
            "layout" => cfg.order.extend(value.split(',').map(|s| s.trim().to_string())),
            "colours" => {
                let rgb = u32::from_str_radix(value, 16).map_err(|_| format!("sections.ini: not a colour: {line}"))?;
                cfg.colours.push((key, ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)));
            }
            "recent" => {
                let n: u64 = value.parse().map_err(|_| format!("sections.ini: not a number: {line}"))?;
                match key.as_str() {
                    "days" => cfg.recent_days = n,
                    "limit" => cfg.recent_limit = n as usize,
                    _ => return Err(format!("sections.ini: unknown setting: {line}")),
                }
            }
            "general" => match key.as_str() {
                "monitor" => cfg.monitor = value.parse().map_err(|_| format!("sections.ini: not a number: {line}"))?,
                "hotkey" => {
                    if !value.is_empty() && parse_hotkey(value).is_none() {
                        return Err(format!("sections.ini: not a shortcut, try ctrl+alt+d: {line}"));
                    }
                    cfg.hotkey = value.to_string();
                }
                "side" => match value {
                    "left" => cfg.right_side = false,
                    "right" => cfg.right_side = true,
                    _ => return Err(format!("sections.ini: side should be left or right: {line}")),
                },
                "theme" => {
                    if value != "wallpaper" && theme_tints(value).is_none() {
                        return Err(format!("sections.ini: unknown theme: {line}"));
                    }
                    cfg.theme = value.to_string();
                }
                "speed" => {
                    if !matches!(value, "fast" | "normal" | "relaxed" | "off") {
                        return Err(format!("sections.ini: speed should be fast, normal, relaxed or off: {line}"));
                    }
                    cfg.speed = value.to_string();
                }
                "dots" => cfg.dots = on_off(&key, value)?,
                "click_away" => cfg.click_away = on_off(&key, value)?,
                "double_click_hide" => cfg.double_click_hide = on_off(&key, value)?,
                "pretty" => old_pretty = on_off(&key, value)?, // from before there were styles
                "style" => {
                    if !matches!(value, "plain" | "smooth" | "glass" | "liquid" | "aurora" | "neon") {
                        return Err(format!("sections.ini: style should be plain, smooth, glass, liquid, aurora or neon: {line}"));
                    }
                    cfg.style = value.to_string();
                    style_given = true;
                }
                _ => return Err(format!("sections.ini: unknown setting: {line}")),
            },
            _ => return Err(format!("sections.ini: line outside a known section: {line}")),
        }
    }
    if old_pretty && !style_given {
        cfg.style = "smooth".to_string();
    }
    let known = |s: &String| cfg.types.iter().any(|(t, _)| t == s);
    let used = cfg.order.iter().chain(cfg.names.iter().map(|(_, s)| s));
    if let Some(bad) = used.into_iter().find(|s| !known(s)) {
        return Err(format!("sections.ini names a section with no [types] line: {bad}"));
    }
    if let Some((bad, _)) = cfg.types.iter().find(|(t, _)| !cfg.order.contains(t)) {
        return Err(format!("sections.ini section missing from [layout]: {bad}"));
    }
    if !cfg.types.iter().any(|(t, _)| t == "Other") {
        return Err("sections.ini needs an Other section.".into());
    }
    if known(&RECENT.to_string()) {
        return Err("sections.ini: Recent is built in, so it cannot be a [types] section.".into());
    }
    Ok(cfg)
}

/// "ctrl+alt+d" as Windows' modifier flags and key code. It needs at least one modifier, so
/// it can't swallow ordinary typing.
pub fn parse_hotkey(text: &str) -> Option<(u32, u32)> {
    let (mut mods, mut key) = (0, None);
    for part in text.to_lowercase().split('+').map(str::trim) {
        match part {
            "alt" => mods |= 1,
            "ctrl" | "control" => mods |= 2,
            "shift" => mods |= 4,
            "win" => mods |= 8,
            p if p.len() == 1 && p.chars().all(|c| c.is_ascii_alphanumeric()) => key = Some(p.to_uppercase().chars().next()? as u32),
            p if p.starts_with('f') => {
                let n: u32 = p[1..].parse().ok()?;
                key = (1..=12).contains(&n).then_some(0x6F + n);
            }
            _ => return None,
        }
    }
    (mods != 0).then_some((mods, key?))
}

/// The screen to use: 0 is the main one, 1 and up count from the left, too high falls back to main.
/// `screens` holds (left edge, is main) in any order; the answer is an index into it.
pub fn pick_screen(screens: &[(i32, bool)], wanted: usize) -> Option<usize> {
    let main = screens.iter().position(|s| s.1).or(if screens.is_empty() { None } else { Some(0) });
    if wanted == 0 {
        return main;
    }
    let mut by_left: Vec<usize> = (0..screens.len()).collect();
    by_left.sort_by_key(|&i| screens[i].0);
    by_left.get(wanted - 1).copied().or(main)
}

/// The rules file with `name` pinned to `section`, replacing any earlier pin of that name.
pub fn add_name_rule(text: &str, name: &str, section: &str) -> String {
    let rule = format!("{name} = {section}");
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut inside = false;
    let mut header = None;
    for (i, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim().to_string();
        if trimmed.starts_with('[') {
            inside = trimmed == "[names]";
            header = if inside { Some(i) } else { header };
        } else if inside {
            if let Some((key, _)) = trimmed.rsplit_once('=') {
                if key.trim().eq_ignore_ascii_case(name) {
                    *line = rule;
                    return lines.join("\n") + "\n";
                }
            }
        }
    }
    match header {
        Some(i) => lines.insert(i + 1, rule),
        None => lines.extend(["".to_string(), "[names]".to_string(), rule]),
    }
    lines.join("\n") + "\n"
}

/// Case-insensitive match where * stands for any run of characters.
pub fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.to_lowercase().chars().collect(), name.to_lowercase().chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ni;
        } else if pi < p.len() && p[pi] == n[ni] {
            pi += 1;
            ni += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// `kind` is "folder", "system", "unknown", or a lower-case extension such as ".docx".
pub fn classify<'a>(name: &str, kind: &str, cfg: &'a Config) -> &'a str {
    if let Some((_, section)) = cfg.names.iter().find(|(pattern, _)| glob(pattern, name)) {
        return section;
    }
    cfg.types.iter().find(|(_, tokens)| tokens.iter().any(|t| t == kind)).map_or("Other", |(s, _)| s)
}

/// Which items belong in Recent: newest first, shortcuts and system icons left out.
/// `items` holds (kind, stamp, index).
pub fn pick_recent(items: &[(&str, u64, usize)], now: u64, days: u64, limit: usize) -> Vec<usize> {
    if days == 0 {
        return vec![];
    }
    let cutoff = now.saturating_sub(days * 86_400);
    let mut fresh: Vec<_> = items
        .iter()
        .filter(|(kind, stamp, _)| *stamp >= cutoff && !matches!(*kind, "system" | "unknown" | ".lnk" | ".url" | ".exe"))
        .collect();
    fresh.sort_by_key(|(_, stamp, _)| std::cmp::Reverse(*stamp));
    fresh.into_iter().take(limit).map(|x| x.2).collect()
}

/// Icons across and rows for one open panel, sized to its contents: the fewest rows that hold them
/// at a comfortable width, shared out evenly so no row sits half empty. It widens past that
/// only when the rows would not fit. None means it cannot fit in the space.
pub fn panel_grid(icons: i32, cell_w: i32, cell_h: i32, room_w: i32, room_h: i32) -> Option<(i32, i32)> {
    const COMFORTABLE: i32 = 6;
    let most_across = room_w / cell_w;
    let most_rows = room_h / cell_h;
    if most_across < 1 || most_rows < 1 {
        return None;
    }
    let icons = icons.max(1);
    let rows_at = |across: i32| (icons + across - 1) / across;
    let mut across = COMFORTABLE.min(most_across);
    while rows_at(across) > most_rows {
        across += 1;
        if across > most_across {
            return None;
        }
    }
    let rows = rows_at(across);
    Some(((icons + rows - 1) / rows, rows))
}

/// The outline of a liquid neck between two points: wide where it meets each end, narrow in the
/// middle, along an S-curve. `reach` (0 to 1) is how far along it has grown.
pub fn neck_outline(from: (f32, f32), to: (f32, f32), reach: f32, end_width: f32, mid_width: f32) -> Vec<(f32, f32)> {
    const STEPS: usize = 32;
    let (x0, y0, x1, y1) = (from.0, from.1, to.0, to.1);
    let mid = (x0 + x1) / 2.0;
    let at = |t: f32| {
        let u = 1.0 - t;
        let x = u * u * u * x0 + 3.0 * u * u * t * mid + 3.0 * u * t * t * mid + t * t * t * x1;
        let y = u * u * u * y0 + 3.0 * u * u * t * y0 + 3.0 * u * t * t * y1 + t * t * t * y1;
        (x, y)
    };
    let (mut upper, mut lower) = (vec![], vec![]);
    for i in 0..=STEPS {
        let t = reach.clamp(0.02, 1.0) * i as f32 / STEPS as f32;
        let (x, y) = at(t);
        let (ax, ay) = at((t + 0.01).min(1.0));
        let (bx, by) = at((t - 0.01).max(0.0));
        let (dx, dy) = (ax - bx, ay - by);
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        let off_centre = (2.0 * t - 1.0).powi(4);
        let half = (mid_width + (end_width - mid_width) * off_centre) / 2.0;
        upper.push((x - dy / len * half, y + dx / len * half));
        lower.push((x + dy / len * half, y - dx / len * half));
    }
    lower.reverse();
    upper.extend(lower);
    upper
}

/// A damped spring: rushes out, swells a little past the end, settles back.
pub fn spring(t: f32) -> f32 {
    1.0 - (-7.0 * t).exp() * (9.0 * t).cos()
}
/// Liquid wobble: width and height settle out of step, so the panel squashes and stretches.
pub fn jelly(t: f32) -> (f32, f32) {
    let wobble = |t: f32, speed: f32| 1.0 - (-6.0 * t).exp() * (speed * t).cos();
    (wobble(t, 11.0), wobble((t - 0.07).max(0.0), 13.0))
}

pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}
pub fn mix(a: i32, b: i32, t: f32) -> i32 {
    a + ((b - a) as f32 * t) as i32
}

/// Colours are blue, green, red; `t` runs from all of a to all of b.
pub fn blend(a: u32, b: u32, t: f32) -> u32 {
    let one = |shift: u32| {
        let (x, y) = (((a >> shift) & 0xFF) as f32, ((b >> shift) & 0xFF) as f32);
        ((x + (y - x) * t).round().clamp(0.0, 255.0) as u32) << shift
    };
    one(0) | one(8) | one(16)
}

/// A wallpaper's average colour brought down to a dark shade with the same hue.
pub fn dark_shade(colour: u32, level: f32) -> u32 {
    let parts = [(colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF];
    let top = parts.iter().copied().max().unwrap_or(0).max(1) as f32;
    let down = |p: u32| ((p as f32 / top * level) as u32).min(255);
    (down(parts[0]) << 16) | (down(parts[1]) << 8) | down(parts[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "[types]\nFolders = folder\nWord = .DOCX\nApps = system .lnk\nColab = .ipynb\nOther =\n\
        [names]\n*colab* = Colab\nKeep.docx = Other\n[layout]\ncolumn1 = Folders\ncolumn2 = Word, Apps\ncolumn3 = Colab, Other\n\
        [colours]\nWord = 3F8CFF\n[recent]\ndays = 5\nlimit = 2\n";

    #[test]
    fn sorting() {
        let c = parse_config(INI).unwrap();
        assert_eq!(c.order, ["Folders", "Word", "Apps", "Colab", "Other"]);
        assert_eq!(classify("Thing", "folder", &c), "Folders");
        assert_eq!(classify("Refit_COLAB_Kit", "folder", &c), "Colab"); // name beats type
        assert_eq!(classify("a.docx", ".docx", &c), "Word");
        assert_eq!(classify("keep.docx", ".docx", &c), "Other"); // pin
        assert_eq!(classify("Recycle Bin", "system", &c), "Apps");
        assert_eq!(classify("x.weird", ".weird", &c), "Other");
        assert_eq!(c.colours[0], ("Word".to_string(), 0xFF8C3F)); // stored blue first
        assert_eq!((c.recent_days, c.recent_limit), (5, 2));
    }

    #[test]
    fn settings_change_only_their_own_line() {
        let start = "# keep me\n[general]\n# a note\nhotkey = ctrl+alt+d\n\n[names]\n*x* = Word\n";
        let out = set_setting(start, "general", "hotkey", "ctrl+alt+k");
        assert_eq!(out, "# keep me\n[general]\n# a note\nhotkey = ctrl+alt+k\n\n[names]\n*x* = Word\n");
        // a new key joins its section, and a same-named key elsewhere is left alone
        let added = set_setting(start, "general", "dots", "off");
        assert!(added.contains("[general]\ndots = off\n"));
        let elsewhere = set_setting("[a]\nk = 1\n[b]\nk = 2\n", "b", "k", "3");
        assert_eq!(elsewhere, "[a]\nk = 1\n[b]\nk = 3\n");
        assert!(set_setting("[a]\n", "b", "k", "3").ends_with("[b]\nk = 3\n"));
    }

    #[test]
    fn styles() {
        let with = |general: &str| parse_config(&format!("[types]\nOther =\n[layout]\ncolumn1 = Other\n[general]\n{general}\n")).unwrap().style;
        assert_eq!(with(""), "plain");
        assert_eq!(with("pretty = on"), "smooth"); // the old setting still means what it did
        assert_eq!(with("pretty = on\nstyle = glass"), "glass"); // a chosen style wins, either order
        assert_eq!(with("style = plain\npretty = on"), "plain");
        assert_eq!(with("style = liquid"), "liquid");
        assert_eq!(with("style = aurora"), "aurora");
        assert_eq!(with("style = neon"), "neon");
    }

    #[test]
    fn bad_settings_are_refused() {
        for bad in ["side = up", "theme = neon", "speed = warp", "dots = maybe", "style = wood"] {
            assert!(parse_config(&format!("[types]\nOther =\n[layout]\ncolumn1 = Other\n[general]\n{bad}\n")).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_readme_custom_section_example_works() {
        let mut text = include_str!("../sections.ini").to_string();
        text.push_str("\n[types]\nThesis =\n[names]\n*thesis* = Thesis\n[layout]\ncolumn5 = Thesis\n[colours]\nThesis = 38C6D9\n");
        let c = parse_config(&text).unwrap();
        assert_eq!(classify("My THESIS draft.docx", ".docx", &c), "Thesis");
        assert!(c.order.contains(&"Thesis".to_string()));
    }

    #[test]
    fn shipped_rules_load() {
        let c = parse_config(include_str!("../sections.ini")).unwrap();
        assert_eq!(classify("a.docx", ".docx", &c), "Word");
        assert_eq!(c.hotkey, "ctrl+alt+d");
        assert_eq!(c.monitor, 0);
    }

    #[test]
    fn shortcuts() {
        assert_eq!(parse_hotkey("ctrl+alt+d"), Some((3, 0x44)));
        assert_eq!(parse_hotkey("Win + Shift + F2"), Some((12, 0x71)));
        assert_eq!(parse_hotkey("d"), None); // no modifier
        assert_eq!(parse_hotkey("ctrl+alt"), None); // no key
        assert_eq!(parse_hotkey("ctrl+banana"), None);
        assert!(parse_config("[types]\nOther =\n[layout]\ncolumn1 = Other\n[general]\nhotkey = banana\n").is_err());
        assert_eq!(parse_config("[types]\nOther =\n[layout]\ncolumn1 = Other\n[general]\nhotkey =\n").unwrap().hotkey, "");
    }

    #[test]
    fn screens() {
        let two = [(1920, false), (0, true)];
        assert_eq!(pick_screen(&two, 0), Some(1)); // the main one
        assert_eq!(pick_screen(&two, 1), Some(1)); // leftmost
        assert_eq!(pick_screen(&two, 2), Some(0));
        assert_eq!(pick_screen(&two, 9), Some(1)); // unplugged: back to main
        let left_of_main = [(0, true), (-1920, false)];
        assert_eq!(pick_screen(&left_of_main, 1), Some(1));
        assert_eq!(pick_screen(&[], 0), None);
    }

    #[test]
    fn matching() {
        assert!(glob("*colab*", "My COLAB kit"));
        assert!(glob("a*c", "abbbc"));
        assert!(!glob("a*c", "abd"));
        assert!(!glob("keep", "keep2"));
        assert!(glob("*", ""));
    }

    #[test]
    fn bad_config_is_refused() {
        assert!(parse_config("[types]\nOther =\n[layout]\ncolumn1 = Nope, Other\n").is_err());
        assert!(parse_config("[types]\nA = .a\nOther =\n[layout]\ncolumn1 = Other\n").is_err());
        assert!(parse_config("[types]\nA = .a\n[layout]\ncolumn1 = A\n").is_err());
        assert!(parse_config("[types]\nRecent = .a\nOther =\n[layout]\ncolumn1 = Recent, Other\n").is_err());
        assert!(parse_config("[types]\nOther =\n[layout]\ncolumn1 = Other\n[colours]\nOther = nothex\n").is_err());
    }

    #[test]
    fn pinning_writes_the_rule() {
        let added = add_name_rule(INI, "sketch.jpeg", "Word");
        let c = parse_config(&added).unwrap();
        assert_eq!(classify("sketch.jpeg", ".jpeg", &c), "Word");
        // pinning again replaces rather than piling up
        let again = add_name_rule(&added, "SKETCH.jpeg", "Apps");
        let c = parse_config(&again).unwrap();
        assert_eq!(classify("sketch.jpeg", ".jpeg", &c), "Apps");
        assert_eq!(again.matches("sketch").count() + again.matches("SKETCH").count(), 1);
        // a name with = still parses
        let odd = add_name_rule(INI, "a=b.txt", "Word");
        assert_eq!(classify("a=b.txt", ".txt", &parse_config(&odd).unwrap()), "Word");
        // and a file with no [names] gains one
        let bare = "[types]\nOther =\n[layout]\ncolumn1 = Other\n";
        assert_eq!(classify("x", ".y", &parse_config(&add_name_rule(bare, "x", "Other")).unwrap()), "Other");
    }

    #[test]
    fn recent_picks_newest_and_skips_shortcuts() {
        let items = [(".pdf", 1000, 0), (".lnk", 1000, 1), ("system", 1000, 2), ("folder", 900, 3), (".txt", 100, 4), (".md", 950, 5)];
        assert_eq!(pick_recent(&items, 1000, 1, 2), vec![0, 5]);
        assert_eq!(pick_recent(&items, 1000, 1, 12), vec![0, 5, 3, 4]);
        assert!(pick_recent(&items, 1000, 0, 12).is_empty()); // turned off
        assert_eq!(pick_recent(&items, 87_000, 1, 12), vec![0, 5, 3]); // the old one drops out
    }

    #[test]
    fn panel_sizes() {
        assert_eq!(panel_grid(1, 95, 112, 1500, 900), Some((1, 1)));
        assert_eq!(panel_grid(3, 95, 112, 1500, 900), Some((3, 1)));
        assert_eq!(panel_grid(6, 95, 112, 1500, 900), Some((6, 1)));
        assert_eq!(panel_grid(7, 95, 112, 1500, 900), Some((4, 2)));
        assert_eq!(panel_grid(10, 95, 112, 1500, 900), Some((5, 2)));
        assert_eq!(panel_grid(13, 95, 112, 1500, 900), Some((5, 3)));
        assert_eq!(panel_grid(21, 95, 112, 1500, 900), Some((6, 4)));
        assert_eq!(panel_grid(100, 95, 112, 1500, 900), Some((13, 8))); // widens rather than overflow
        assert_eq!(panel_grid(500, 95, 112, 1500, 900), None);
        assert_eq!(panel_grid(5, 95, 112, 200, 900), Some((2, 3))); // narrow room
    }

    #[test]
    fn neck_is_wide_at_the_ends_and_narrow_in_the_middle() {
        let pts = neck_outline((0.0, 100.0), (80.0, 20.0), 1.0, 20.0, 4.0);
        assert_eq!(pts.len(), 66);
        let width = |i: usize| {
            let (a, b) = (pts[i], pts[pts.len() - 1 - i]);
            ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
        };
        assert!(width(0) > 15.0 && width(32) > 15.0);
        assert!(width(16) < 6.0);
        let half = neck_outline((0.0, 100.0), (80.0, 20.0), 0.5, 20.0, 4.0);
        assert!(half.iter().all(|(x, _)| *x < 60.0)); // a partial neck stops short
    }

    #[test]
    fn jelly_starts_small_wobbles_and_ends_near_full_size() {
        assert!(jelly(0.0).0.abs() < 0.001);
        let (w, h) = jelly(1.0);
        assert!((w - 1.0).abs() < 0.02 && (h - 1.0).abs() < 0.03, "{w} {h}");
        let peak = (0..=100).map(|i| jelly(i as f32 / 100.0).0).fold(0.0, f32::max);
        assert!(peak > 1.02 && peak < 1.3, "{peak}");
        assert!((1..30).any(|i| { let (a, b) = jelly(i as f32 / 100.0); (a - b).abs() > 0.05 })); // out of step
    }

    #[test]
    fn motion_ends_where_it_should() {
        assert!(spring(0.0).abs() < 0.001 && (spring(1.0) - 1.0).abs() < 0.01);
        let peak = (0..=100).map(|i| spring(i as f32 / 100.0)).fold(0.0, f32::max);
        assert!(peak > 1.0 && peak < 1.2, "{peak}"); // swells, but not wildly
        assert!(ease_out(0.0).abs() < 0.001 && (ease_out(1.0) - 1.0).abs() < 0.001);
        assert_eq!(mix(10, 20, 0.5), 15);
    }

    #[test]
    fn colours_mix() {
        assert_eq!(blend(0x000000, 0xFFFFFF, 0.0), 0x000000);
        assert_eq!(blend(0x000000, 0xFFFFFF, 1.0), 0xFFFFFF);
        assert_eq!(dark_shade(0x804020, 40.0), 0x28140A);
        assert_eq!(dark_shade(0, 40.0), 0);
    }
}
