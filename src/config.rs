//! Config file (TOML) and themes. Everything lives in one folder: see `config_dir`.

use crate::term::Color;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: u32,
    pub fg: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ui_bg: u32,
    pub head_bg: u32, // tinted band behind each terminal panel's header
    pub ui_fg: u32,
    pub border: u32,
    pub accent: u32,
    pub on_accent: u32, // text on top of accent
    pub sub: u32,       // muted text
    pub green: u32,     // status dot
    pub ansi: [u32; 16],
    pub snap: bool, // exact RGB colours (Claude Code's) are drawn as the nearest theme colour
}

// Palette: one surface colour for the terminal and the header (ui_bg == bg); hairlines separate things instead of tints.
impl Theme {
    pub fn dark() -> Self {
        Theme {
            bg: 0x121212,
            fg: 0xe8e8e8,
            cursor: 0xa3ac84,
            selection: 0x3d4034,
            ui_bg: 0x121212,
            head_bg: 0x1a1a1a,
            ui_fg: 0xf0f0f0,
            border: 0x2e2e2e,
            accent: 0xa3ac84,
            on_accent: 0x121212,
            sub: 0x9b9b9b,
            green: 0x34d399,
            ansi: [
                0x2e2e2e, 0xe5736b, 0x8fbf7f, 0xe0c070, 0x7aa2d6, 0xc492c4, 0x6fb8b0, 0xd0d0d0,
                0x6b6b6b, 0xf08a82, 0xa3ac84, 0xefd58a, 0x93b7e6, 0xd8a9d8, 0x8fcfc7, 0xf0f0f0,
            ],
            snap: false,
        }
    }

    pub fn light() -> Self {
        Theme {
            bg: 0xfafafa,
            fg: 0x404040,
            cursor: 0x737b59,
            selection: 0xdadada,
            ui_bg: 0xfafafa,
            head_bg: 0xf0f0f0,
            ui_fg: 0x121212,
            border: 0xdcdcdc,
            accent: 0x737b59,
            on_accent: 0xffffff,
            sub: 0x60636a,
            green: 0x047857,
            ansi: [
                0x404040, 0xb3261e, 0x047857, 0x8a6a00, 0x2b5db0, 0x8f3f8f, 0x0e7490, 0x8a8d94,
                0x60636a, 0xd1453b, 0x0a9a6b, 0xa67c00, 0x3b73d1, 0xa855a8, 0x1593b0, 0xb0b0b0,
            ],
            snap: false,
        }
    }

    /// Claude's dark mode: warm charcoal surface, off-white text, terracotta accent.
    pub fn friendly() -> Self {
        Theme {
            bg: 0x262624,
            fg: 0xe8e6dc,
            cursor: 0xd97757,
            selection: 0x4a4944,
            ui_bg: 0x262624,
            head_bg: 0x30302e,
            ui_fg: 0xfaf9f5,
            border: 0x3d3d3a,
            accent: 0xd97757,
            on_accent: 0x1f1e1d,
            sub: 0xb0aea5,
            green: 0x3fd29a,
            ansi: [
                0x2b2c2d, 0xee8078, 0x9bcb8b, 0xe8c978, 0x86addf, 0xcf9fcf, 0x78c4bb, 0xdcdcd8,
                0x8e8f90, 0xf79990, 0xb6bf98, 0xf2dc96, 0xa2c2ee, 0xdeb4de, 0x98d6ce, 0xf4f4f0,
            ],
            snap: false,
        }
    }

    pub fn rgb(&self, c: Color) -> u32 {
        match c {
            Color::Fg => self.fg,
            Color::Bg => self.bg,
            Color::Ansi(n) => self.ansi[n as usize & 15],
            Color::Rgb(v) if self.snap => self.nearest(v),
            Color::Rgb(v) => v,
        }
    }

    /// The theme colour (background, text or one of the 16) closest to `v`.
    /// ponytail: a linear scan of 18 colours per cell drawn; cache it if it ever shows up in a profile.
    fn nearest(&self, v: u32) -> u32 {
        let dist = |a: u32, b: u32| {
            let d = |s: u32| ((a >> s & 0xff) as i32 - (b >> s & 0xff) as i32).pow(2);
            2 * d(16) + 4 * d(8) + 3 * d(0) // the eye is most sensitive to green
        };
        [self.bg, self.fg].into_iter().chain(self.ansi).min_by_key(|&c| dist(c, v)).unwrap_or(v)
    }

    fn apply(&mut self, c: &ThemeCfg) {
        fn set(dst: &mut u32, v: &Option<String>) {
            if let Some(x) = v.as_deref().and_then(hex) {
                *dst = x;
            }
        }
        set(&mut self.bg, &c.background);
        set(&mut self.fg, &c.foreground);
        set(&mut self.cursor, &c.cursor);
        set(&mut self.selection, &c.selection);
        set(&mut self.ui_bg, &c.ui_background);
        set(&mut self.head_bg, &c.header_background);
        set(&mut self.ui_fg, &c.ui_foreground);
        set(&mut self.border, &c.border);
        set(&mut self.accent, &c.accent);
        set(&mut self.on_accent, &c.on_accent);
        set(&mut self.sub, &c.muted);
        set(&mut self.green, &c.green);
        for (i, s) in c.ansi.iter().take(16).enumerate() {
            if let Some(x) = hex(s) {
                self.ansi[i] = x;
            }
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ThemeCfg {
    background: Option<String>,
    foreground: Option<String>,
    cursor: Option<String>,
    selection: Option<String>,
    ui_background: Option<String>,
    header_background: Option<String>,
    ui_foreground: Option<String>,
    border: Option<String>,
    accent: Option<String>,
    on_accent: Option<String>,
    muted: Option<String>,
    green: Option<String>,
    ansi: Vec<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct PanelCfg {
    pub name: String,
    pub dir: Option<String>,
    pub cmd: Option<String>,
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub accent: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub view: String,
    pub shell: Option<String>,
    pub editor: Option<String>,
    pub theme_colors: bool,
    pub font: String,
    pub font_size: Option<f32>,
    pub line_height: Option<f32>,
    pub letter_spacing: Option<f32>,
    pub accent: String,
    pub maximize: String,
    pub light: ThemeCfg,
    pub dark: ThemeCfg,
    pub friendly: ThemeCfg,
    pub panel: Vec<PanelCfg>,
}

/// Menu labels, in the order of `Config::themes`.
pub const THEME_NAMES: [&str; 3] = ["Light", "Dark", "Friendly"];

impl Config {
    /// [light, dark, friendly], with the config's overrides applied.
    pub fn themes(&self) -> [Theme; 3] {
        let (mut l, mut d, mut f) = (Theme::light(), Theme::dark(), Theme::friendly());
        l.apply(&self.light);
        d.apply(&self.dark);
        f.apply(&self.friendly);
        for t in [&mut l, &mut d, &mut f] {
            t.snap = self.theme_colors;
        }
        [l, d, f]
    }

    /// The configured theme's index into `themes()`; light when unset or unknown.
    pub fn theme_index(&self) -> usize {
        THEME_NAMES.iter().position(|n| n.eq_ignore_ascii_case(&self.theme)).unwrap_or(0)
    }
}

pub fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches('#'), 16).ok().filter(|v| *v <= 0xffffff)
}

/// A space saved from a tab's menu: its name and the folders of its terminals.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct SavedSpace {
    pub name: String,
    pub dirs: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct SavedFile {
    space: Vec<SavedSpace>,
}

fn spaces_path() -> PathBuf {
    config_dir().join("spaces.toml")
}

pub fn load_spaces() -> Vec<SavedSpace> {
    std::fs::read_to_string(spaces_path()).ok().and_then(|s| toml::from_str::<SavedFile>(&s).ok()).map(|f| f.space).unwrap_or_default()
}

/// `s` into `list`, replacing the one with the same name.
fn upsert(list: &mut Vec<SavedSpace>, s: SavedSpace) {
    match list.iter_mut().find(|x| x.name == s.name) {
        Some(x) => *x = s,
        None => list.push(s),
    }
}

/// Saves `s` (see `upsert`) and writes the file.
pub fn save_space(list: &mut Vec<SavedSpace>, s: SavedSpace) {
    upsert(list, s);
    let _ = std::fs::create_dir_all(config_dir());
    match toml::to_string(&SavedFile { space: list.clone() }) {
        Ok(text) => {
            if let Err(e) = std::fs::write(spaces_path(), text) {
                eprintln!("maula: cannot save {}: {e}", spaces_path().display());
            }
        }
        Err(e) => eprintln!("maula: cannot save spaces: {e}"),
    }
}

pub fn config_dir() -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("maula")
}

const TEMPLATE: &str = r##"# maula config. Restart after editing.
theme = "light"  # "light" (default), "dark" or "friendly" (soft grey in between)
view = "space"   # "space" (default: tabs of two terminals), "grid" (2x2 pages) or "carousel"
# shell = "pwsh.exe"   # or Git Bash: "C:/Program Files/Git/bin/bash.exe"
# editor = "code -g"   # Ctrl+click a file path to open it here (given path:line:col); unset: shown in Explorer
# theme_colors = true  # draw programs' exact colours (Claude Code uses them) in the nearest colour of the theme
font = "JetBrains Mono"  # the only font for now
font_size = 100          # percent: 85, 90, 100, 110, 125 or 150
line_height = 0          # terminal rows, times the font size: 0 (the font's own), 1.2, 1.4, 1.6 or 1.8
letter_spacing = 0       # px: 0, 0.5, 1, 1.5 or 2
accent = "default"       # "default" (the theme's own), "terracotta", "blue", "teal", "violet" or "rose"
maximize = "fullscreen"  # the maximize button: "fullscreen" (borderless, covers the taskbar) or "window"

# Optional color overrides, per theme ([light], [dark] or [friendly]; hex). ansi takes up to 16 entries.
# [dark]
# background = "#1e1e1e"
# foreground = "#cccccc"
# cursor = "#ffffff"
# selection = "#264f78"
# accent = "#0e639c"
# ui_background = "#252526"
# header_background = "#1a1a1a"   # band behind each panel's header
# border = "#2e2e2e"
# muted = "#9b9b9b"    # secondary text
# green = "#34d399"    # status dot
# ansi = ["#000000", "#cd3131"]

# Panels opened at startup. Without any, one plain shell is opened.
# [[panel]]
# name = "api"
# dir = "D:/work/api"
# cmd = "claude"
# bg = "#102030"       # this panel's terminal background
# accent = "#ff8800"   # this panel's tab/border color
"##;

/// `text` with each top-level `key = value` of `keys` replaced (keeping its comment), or added before the first
/// table when missing. Everything else in the file is left as written.
fn set_keys(text: &str, keys: &[(&str, String)]) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    for (key, value) in keys {
        let is_key = |l: &String| l.split('=').next().is_some_and(|k| k.trim() == *key) && l.contains('=');
        let top = lines.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(lines.len());
        match lines[..top].iter().position(is_key) {
            Some(i) => {
                let comment = lines[i].find('#').map(|c| format!("  {}", &lines[i][c..])).unwrap_or_default();
                lines[i] = format!("{key} = {value}{comment}");
            }
            None => lines.insert(top, format!("{key} = {value}")),
        }
    }
    lines.join("\n") + "\n"
}

/// Writes the dropdown's choices (theme, view, Text) into config.toml; values are TOML, e.g. `"dark"` or `1.4`.
pub fn save(keys: &[(&str, String)]) {
    let path = config_dir().join("config.toml");
    let old = std::fs::read_to_string(&path).unwrap_or_else(|_| TEMPLATE.into());
    let _ = std::fs::create_dir_all(config_dir());
    if let Err(e) = std::fs::write(&path, set_keys(&old, keys)) {
        eprintln!("maula: cannot save {}: {e}", path.display());
    }
}

pub fn load() -> Config {
    let path = config_dir().join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            eprintln!("maula: bad config {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => {
            let _ = std::fs::create_dir_all(config_dir());
            let _ = std::fs::write(&path, TEMPLATE);
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_spaces_round_trip_and_replace_by_name() {
        let space = |name: &str, dirs: &[&str]| SavedSpace { name: name.into(), dirs: dirs.iter().map(|d| d.to_string()).collect() };
        let mut list = vec![];
        upsert(&mut list, space("api", &["D:/a", "D:/b"]));
        upsert(&mut list, space("web", &["D:/w"]));
        upsert(&mut list, space("api", &["D:/c"]));
        assert_eq!(list, [space("api", &["D:/c"]), space("web", &["D:/w"])], "same name: replaced in place");
        let text = toml::to_string(&SavedFile { space: list.clone() }).unwrap();
        assert_eq!(toml::from_str::<SavedFile>(&text).unwrap().space, list);
        assert!(toml::from_str::<SavedFile>("").unwrap().space.is_empty(), "no file content: no spaces");
    }

    #[test]
    fn parse_and_override() {
        assert_eq!(hex("#1e1e1e"), Some(0x1e1e1e));
        assert_eq!(hex("zzz"), None);
        let c: Config = toml::from_str("theme=\"Friendly\"\n[dark]\nbackground=\"#101010\"\nansi=[\"#ff0000\"]\n[[panel]]\nname=\"a\"\ncmd=\"claude\"").unwrap();
        let [l, d, f] = c.themes();
        assert_eq!((d.bg, d.ansi[0], d.ansi[1]), (0x101010, 0xff0000, Theme::dark().ansi[1]));
        assert_eq!((l.bg, f.bg), (Theme::light().bg, Theme::friendly().bg));
        assert_eq!(c.theme_index(), 2, "theme names are case-insensitive");
        assert_eq!(Config::default().theme_index(), 0, "unset: light");
        assert_eq!(c.panel[0].cmd.as_deref(), Some("claude"));
        let mut snap = Theme::dark();
        assert_eq!(snap.rgb(Color::Rgb(0xd77757)), 0xd77757, "off: exact colours pass through");
        snap.snap = true;
        assert_eq!(snap.rgb(Color::Rgb(0xd77757)), snap.ansi[1], "on: Claude's orange becomes the theme's red");
        assert_eq!(snap.rgb(Color::Rgb(0x141414)), snap.bg, "a near-black tint becomes the background");
        let t: Config = toml::from_str(TEMPLATE).unwrap(); // the shipped template must parse
        assert_eq!(t.theme, "light");
        assert_eq!((t.font_size, t.line_height, t.letter_spacing), (Some(100.0), Some(0.0), Some(0.0)));

        // saving the Text choices: replaced in place with comments kept, or added above the first table
        let keys = [("font_size", "110".to_string()), ("letter_spacing", "0.5".to_string())];
        let out = set_keys("theme = \"dark\"\nfont_size = 100  # percent\n# letter_spacing = 9\n[[panel]]\nname = \"a\"\n", &keys);
        assert_eq!(out, "theme = \"dark\"\nfont_size = 110  # percent\n# letter_spacing = 9\nletter_spacing = 0.5\n[[panel]]\nname = \"a\"\n");
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!((c.font_size, c.letter_spacing, c.panel.len()), (Some(110.0), Some(0.5), 1));

        // legible: text on the surface keeps a comfortable contrast in every theme
        let lum = |c: u32| {
            let ch = |s: u32| ((c >> s & 0xff) as f32 / 255.0).powf(2.2);
            0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
        };
        for t in [l, Theme::dark(), f] {
            let ratio = |a: u32, b: u32| (lum(a).max(lum(b)) + 0.05) / (lum(a).min(lum(b)) + 0.05);
            assert!(ratio(t.fg, t.bg) >= 7.0 && ratio(t.sub, t.bg) >= 4.5, "text {} muted {}", ratio(t.fg, t.bg), ratio(t.sub, t.bg));
            assert!(ratio(t.ansi[8], t.bg) >= 3.0, "dim text (bright black) stays readable");
        }
    }
}
