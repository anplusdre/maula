#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod claude;
mod config;
mod gfx;
mod json;
mod link;
mod picker;
mod suggest;
mod term;
mod width;
mod win;

use config::{Config, PanelCfg, SavedSpace, Theme, THEME_NAMES};
use gfx::{blend, Canvas, Font, Rect};
use picker::{Kind, Picker};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use suggest::Suggest;
use term::{Term, BLANK};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalSize},
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::{CursorIcon, Fullscreen, ResizeDirection, Theme as WinTheme, Window, WindowId},
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

const FONT_PX: f32 = 14.0; // terminal text
const UI_PX: f32 = 13.0; // dropdown and popups: the system UI font
const TITLE_PX: f32 = 14.0; // panel titles
const CARD_COLS: i32 = 90; // terminal width of one card
const RADIUS: i32 = 0; // the whole UI is square; raise this to round the dropdown and popups

const MAX_SPACES: usize = 5; // tabs that fit the header
const SPACE_SIZE: usize = 2; // terminals per space

/// Dropdown rows, in order: the themes (`THEME_NAMES` order) and Accent, then view (space, grid, carousel) and
/// Maximize, then the Text submenus (`TEXT_ROWS` order), then Claude. `MENU_GROUPS`: how many rows each group has.
const MENU_ACTIONS: [Action; 13] = [
    Action::SetTheme(0),
    Action::SetTheme(1),
    Action::SetTheme(2),
    Action::SubMenu(4),
    Action::SetView(View::Space),
    Action::SetView(View::Grid),
    Action::SetView(View::Cards),
    Action::SubMenu(5),
    Action::SubMenu(0),
    Action::SubMenu(1),
    Action::SubMenu(2),
    Action::SubMenu(3),
    Action::InstallHooks,
];
const MENU_GROUPS: [(&str, usize); 4] = [("Theme", 4), ("View", 4), ("Text", 4), ("Claude", 1)];

/// The dropdown's submenu rows. The Text group (the first four) applies to the terminal and the header titles;
/// Accent sits in the Theme group, Maximize in the View group.
const TEXT_ROWS: [&str; 6] = ["Font", "Size", "Line height", "Letter spacing", "Accent", "Maximize"];
const SIZES: [f32; 6] = [85.0, 90.0, 100.0, 110.0, 125.0, 150.0]; // percent of FONT_PX / TITLE_PX
const LINES: [f32; 5] = [0.0, 1.2, 1.4, 1.6, 1.8]; // terminal row height, times the font size; 0 = the font's own (~1.32)
const TRACKS: [f32; 5] = [0.0, 0.5, 1.0, 1.5, 2.0]; // letter-spacing, design px
const ACCENTS: [(&str, u32); 6] = [("Default", 0), ("Terracotta", 0xd97757), ("Blue", 0x4f86d9), ("Teal", 0x2a9d8f), ("Violet", 0x8b6fd6), ("Rose", 0xd9577f)];
const MAXIMIZE: [&str; 2] = ["Borderless fullscreen", "Maximized window"]; // saved as "fullscreen" / "window"
const TEXT_DEFAULT: [usize; 6] = [0, 2, 0, 0, 0, 0]; // chosen index per TEXT_ROWS entry

/// Labels of the choices in Text submenu `k`.
fn text_choices(k: usize) -> Vec<String> {
    match k {
        0 => vec!["JetBrains Mono".into()],
        1 => SIZES.iter().map(|s| format!("{s}%")).collect(),
        2 => LINES.iter().map(|&l| if l == 0.0 { "Default".into() } else { format!("{l}") }).collect(),
        4 => ACCENTS.iter().map(|a| a.0.into()).collect(),
        5 => MAXIMIZE.iter().map(|&m| m.into()).collect(),
        _ => TRACKS.iter().map(|t| format!("{t}px")).collect(),
    }
}

/// The Text choices saved in the config; a missing value, or one not on the menu, is the default.
fn text_of(cfg: &Config) -> [usize; 6] {
    let at = |list: &[f32], v: Option<f32>, k: usize| v.and_then(|v| list.iter().position(|&x| x == v)).unwrap_or(TEXT_DEFAULT[k]);
    let accent = ACCENTS.iter().position(|a| a.0.eq_ignore_ascii_case(&cfg.accent)).unwrap_or(0);
    let window = usize::from(cfg.maximize.eq_ignore_ascii_case("window"));
    [0, at(&SIZES, cfg.font_size, 1), at(&LINES, cfg.line_height, 2), at(&TRACKS, cfg.letter_spacing, 3), accent, window]
}

/// `t` with Accent choice `i` as its accent (and cursor); 0 keeps the theme's own.
fn with_accent(mut t: Theme, i: usize) -> Theme {
    if i > 0 {
        let c = ACCENTS[i].1;
        let bright = (c >> 16 & 0xff) * 299 + (c >> 8 & 0xff) * 587 + (c & 0xff) * 114 > 150_000;
        (t.accent, t.cursor, t.on_accent) = (c, c, if bright { 0x121212 } else { 0xffffff });
    }
    t
}

/// Terminal and title fonts for the Text choices `text` (see `TEXT_DEFAULT`).
fn text_fonts(text: [usize; 6], scale: f32) -> (Font, Font) {
    let (k, track) = (SIZES[text[1]] / 100.0 * scale, TRACKS[text[3]] * scale);
    (Font::new(FONT_PX * k).spaced(LINES[text[2]], track), Font::medium(TITLE_PX * k).spaced(0.0, track))
}

/// Replaces the user's PowerShell prompt with three blank cells, where maula draws the prompt mark (the panel header
/// already shows the drive and folder), and makes every prompt report the working directory (OSC 9;9) and where it
/// starts (OSC 133;A).
const PS_HOOK: &str = r#"function global:prompt { $e=[string][char]27; $q=[string][char]34; $e+']9;9;'+$q+$PWD.ProviderPath+$q+$e+'\'+$e+']133;A'+[string][char]7+'   ' }"#;

/// The same prompt for bash, as PS1: the marker, then the three blank cells.
const BASH_PS1: &str = r"PS1='\[\e]133;A\a\]   '";

/// Dev aid: with `MAULA_STATS=1` the app prints, once a second, how many frames it drew and how many shell
/// output chunks it received. A healthy idle app prints zeros.
static DRAWS: AtomicU32 = AtomicU32::new(0);
static OUTPUTS: AtomicU32 = AtomicU32::new(0);

enum Ev {
    Output(usize, Vec<u8>),
    Exit(usize),
    Hook(claude::Hook),
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Space, // tabs of two terminals each; one space on screen
    Grid,
    Cards, // shown as "Carousel"
}

#[derive(Clone, Copy)]
enum Action {
    Focus(usize),
    New,
    Close(usize),
    Space(usize),     // the tab of space id
    SpaceMenu(usize), // the chevron of that tab
    SaveSpace(usize),
    LoadMenu(usize), // open the saved-spaces list beside the tab menu
    CloseSpace(usize),
    SetView(View),
    SetTheme(usize),
    SubMenu(usize),        // open Text submenu k beside the dropdown
    SetText(usize, usize), // choice i of Text submenu k
    InstallHooks,          // add maula's hooks to every Claude account
    Memory(usize),         // switch pane i's memory on or off
    ToggleMenu,
    PickDrive(usize), // open the drive picker of pane i
    PickDir(usize),   // open the folder picker of pane i
    Pick(usize),      // row k of the open picker
    Search,           // click in the picker's search box: place the caret
    Nop, // swallows clicks on a dropdown's background
    Minimize,
    ToggleMaximize,
    CloseWindow,
}

type Pos = (usize, usize); // (absolute line, column)

/// Ctrl+Shift+F: the scrollback search of one pane. The current match is that pane's selection.
struct Search {
    pane: usize,
    field: Picker,                      // its text box
    hits: Vec<(usize, usize, usize)>, // see `Term::find`
    cur: usize,                         // index into `hits`; starts on the newest
}

/// A tab: up to two terminals shown together.
struct Space {
    id: usize,
    name: String, // "Space N" until renamed (not remembered across runs, except through Save)
}

struct Pane {
    id: usize,
    space: usize, // id of the space it belongs to
    name: String,
    custom: bool, // name came from the config; otherwise the tab shows the current folder
    dir: Option<PathBuf>,
    bg: Option<u32>,
    fg: Option<u32>,
    accent: Option<u32>,
    term: Term,
    master: win::Pty,
    writer: std::fs::File,
    scroll_pos: f32,    // displayed scroll position, in lines back; chases scroll_goal
    scroll_goal: f32,   // where scrolling is heading, in fractional lines (term.scroll is this, rounded)
    vis: Tween,         // 0..1 visibility: fades in when created, out when closing
    lit: Tween,         // 0..1: 1 while this is the active panel (eased); card view greys out the rest
    closing: bool,      // fading out; removed once the fade is done
    anchor: Pos,
    sel: Option<(Pos, Pos)>,
    suggest: Option<Suggest>,
    dismissed: Option<String>,
    claude: Option<claude::Session>, // the Claude session running here, once a hook reported it
    memory: bool,                    // the header's MEMORY switch: the session's notes are also saved in the project
    attention: bool,                 // Claude stopped or asked something while you were looking elsewhere
}

impl Pane {
    /// Where the shell is: what it last reported, else where it started.
    fn cwd(&self) -> Option<PathBuf> {
        self.term.cwd.as_ref().map(PathBuf::from).or_else(|| self.dir.clone()).or_else(|| std::env::current_dir().ok())
    }
}

/// The shell to start: program, arguments, and the variables it gets on top of maula's environment.
fn shell_command(shell: Option<&str>) -> (String, Vec<&'static str>, Vec<(&'static str, String)>) {
    let exe = shell.unwrap_or("powershell.exe");
    let l = exe.to_lowercase();
    let mut env = vec![("TERM", "xterm-256color".to_string()), ("COLORTERM", "truecolor".to_string())];
    let mut args = Vec::new();
    if l.contains("powershell") || l.contains("pwsh") {
        args = vec!["-NoLogo", "-NoExit", "-Command", PS_HOOK];
    } else if l.contains("bash") {
        // Git Bash: `pwd -W` is the folder as Windows writes it (D:/x), which the header and pickers understand
        // ponytail: a bashrc that overwrites PROMPT_COMMAND disables the cwd report and the prompt mark
        env.push(("PROMPT_COMMAND", format!(r#"printf '\033]9;9;"%s"\007' "$(pwd -W 2>/dev/null || pwd)"; {BASH_PS1}"#)));
    }
    (exe.to_string(), args, env)
}

/// `ipc`: the hook listener's (port, token), handed to the shell so `maula hook` can report back to this panel.
fn spawn_pty(proxy: &EventLoopProxy<Ev>, id: usize, shell: Option<&str>, dir: Option<&Path>, ipc: Option<&(u16, String)>) -> (win::Pty, std::fs::File) {
    let (exe, args, mut env) = shell_command(shell);
    if let Some((port, token)) = ipc {
        env.extend([("MAULA_PANE", id.to_string()), ("MAULA_PORT", port.to_string()), ("MAULA_TOKEN", token.clone())]);
    }
    let (pty, mut reader, writer, child) = win::spawn(&exe, &args, &env, dir, 80, 24).expect("spawn shell");

    let p = proxy.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || p.send_event(Ev::Output(id, buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });
    // ConPTY never gives the reader EOF when the child exits, so watch the child itself.
    let p = proxy.clone();
    std::thread::spawn(move || {
        child.wait();
        let _ = p.send_event(Ev::Exit(id));
    });
    (pty, writer)
}

/// ponytail: no F-keys, no kitty keyboard protocol. Add when something needs them.
fn key_bytes(ev: &KeyEvent, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = mods.control_key() && !mods.alt_key(); // Ctrl+Alt is AltGr on Windows
    let alt = mods.alt_key() && !mods.control_key();
    let arrow = |c: char| -> String { if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") } };
    let s: String = match &ev.logical_key {
        Key::Named(k) => match k {
            NamedKey::Enter => "\r".into(),
            NamedKey::Backspace => "\x7f".into(),
            NamedKey::Tab => (if mods.shift_key() { "\x1b[Z" } else { "\t" }).into(),
            NamedKey::Escape => "\x1b".into(),
            NamedKey::Space => (if ctrl { "\0" } else { " " }).into(),
            NamedKey::ArrowUp => arrow('A'),
            NamedKey::ArrowDown => arrow('B'),
            NamedKey::ArrowRight => arrow('C'),
            NamedKey::ArrowLeft => arrow('D'),
            NamedKey::Home => "\x1b[H".into(),
            NamedKey::End => "\x1b[F".into(),
            NamedKey::Insert => "\x1b[2~".into(),
            NamedKey::Delete => "\x1b[3~".into(),
            NamedKey::PageUp => "\x1b[5~".into(),
            NamedKey::PageDown => "\x1b[6~".into(),
            _ => return None,
        },
        Key::Character(c) => {
            let up = c.chars().next()?.to_ascii_uppercase();
            if ctrl && ('@'..='_').contains(&up) {
                char::from(up as u8 & 0x1f).to_string()
            } else {
                c.to_string()
            }
        }
        _ => return None,
    };
    Some(if alt { format!("\x1b{s}") } else { s }.into_bytes())
}

/// `s`, shortened with an ellipsis to fit `max_w` pixels.
fn fit(ui: &mut Font, s: &str, max_w: i32, track: f32) -> String {
    if ui.width_tracked(s, track) <= max_w {
        return s.to_string();
    }
    let mut t: String = s.to_string();
    while !t.is_empty() && ui.width_tracked(&format!("{t}…"), track) > max_w {
        t.pop();
    }
    format!("{t}…")
}

/// Tab title: the configured name, else the folder the shell is in, else "Terminal N".
fn title(p: &Pane) -> String {
    let folder = p.term.cwd.as_deref().and_then(|c| Path::new(c).file_name()).map(|n| n.to_string_lossy().into_owned());
    match folder {
        Some(f) if !p.custom && !f.is_empty() => f,
        _ => p.name.clone(),
    }
}

struct Ui {
    window: Arc<Window>,
    hwnd: isize, // the window, for presenting frames and owning the clipboard
    font: Font,    // terminal (monospace)
    ui_font: Font, // dropdown, popups
    title_font: Font, // panel titles
    icon_font: Font, // Lucide, sized to ICON
    scale: f32,      // display scaling (1.0 = 100%); all design sizes go through `px`
    proxy: EventLoopProxy<Ev>,
    shell: Option<String>,
    editor: Option<String>, // what Ctrl+click opens file paths in
    search: Option<Search>,
    ipc: Option<(u16, String)>, // where `maula hook` reports, see `claude::listen`
    hooks: bool,                // every Claude account runs our hooks
    focused: bool,              // the window has the keyboard
    started: Instant,           // the prompt mark's animation clock
    mark: Option<Rect>,         // the animated prompt mark drawn last frame: it gets its own small repaints
    next_mark: Instant,
    themes: [Theme; 3], // THEME_NAMES order
    theme: usize,
    view: View,
    panes: Vec<Pane>,
    focus: usize, // the active space is this pane's
    next_id: usize,
    spaces: Vec<Space>,
    next_space: usize,
    saved: Vec<SavedSpace>,
    smenu: Option<usize>,          // space id whose tab menu (Save / Load / Close) is open
    smenu_sel: Option<usize>,      // highlighted row of it
    rename: Option<(usize, Picker)>, // space id being renamed, and its text box
    last_tab_click: Option<(usize, Instant)>, // for double-click to rename
    card_scroll: i32,
    mods: ModifiersState,
    mouse: (i32, i32),
    hits: Vec<(Rect, Action)>,
    drag: Option<usize>, // pane index being drag-selected
    hover: Option<usize>, // index into `hits` under the mouse
    last_header_click: Option<Instant>, // for double-click to maximize
    menu_open: bool,                    // theme / view dropdown
    menu_sel: Option<usize>,            // highlighted dropdown row (hover or keyboard)
    sub: Option<usize>,                 // open Text submenu
    sub_sel: Option<usize>,             // highlighted row of it
    text: [usize; 6],                   // submenu choices, see TEXT_DEFAULT
    picker: Option<Picker>,             // folder / drive picker of a panel header; modal like the dropdown
    pick_scroll: f32,                   // wheel distance not yet worth a row
    pick_rehover: bool,                 // the wheel scrolled the picker: the highlight must follow the row under the pointer
    fades: HashMap<RectKey, Tween>,     // hover fade (0..1) per button/tab, keyed by its rectangle
    last_tick: Instant,
    animating: bool,                    // something is still easing: keep drawing at ~60 fps
    card_anim: Tween,                   // eases card_scroll (the carousel position) onto the focused card
    repaint: Option<Rect>,              // panels that changed since the last frame (partial redraw)
    frame: Vec<u32>,                    // the last complete frame; partial redraws patch it
    dirty: bool,                        // the next frame must repaint everything
    partial_ok: bool,                   // the next frame may repaint only `damage`
    damage: Option<Rect>,               // where the hover fades changed this frame
    next_frame: Instant,                // frame pacing while animating
    close_font: Font,                   // "CLOSE" in panel headers
    icon: CursorIcon,
    quit: bool,
}

impl Ui {
    fn new(el: &ActiveEventLoop, proxy: EventLoopProxy<Ev>, cfg: Config) -> Ui {
        let attrs = Window::default_attributes()
            .with_title("maula")
            .with_decorations(false) // we draw our own header and caption buttons
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(480.0, 300.0));
        #[cfg(windows)]
        let attrs = {
            // the icon build.rs embedded (resource 1): title bar, taskbar and Alt+Tab
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            let icon = winit::window::Icon::from_resource(1, None).ok();
            attrs.with_window_icon(icon.clone()).with_taskbar_icon(icon)
        };
        let window = Arc::new(el.create_window(attrs).unwrap());
        let scale = window.scale_factor() as f32;
        let text = text_of(&cfg);
        let ((font, title_font), ui_font) = (text_fonts(text, scale), Font::new(UI_PX * scale));
        let (icon_font, close_font) = (Font::icons(ICON * scale), Font::medium(CLOSE_PX * scale));
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => panic!("not a Win32 window"),
        };
        let p = proxy.clone();
        let ipc = claude::listen(move |h| drop(p.send_event(Ev::Hook(h))));
        let mut ui = Ui {
            window,
            hwnd,
            font,
            ui_font,
            title_font,
            icon_font,
            scale,
            proxy,
            shell: cfg.shell.clone(),
            editor: cfg.editor.clone(),
            search: None,
            ipc,
            hooks: claude::hooks_installed(),
            focused: true,
            started: Instant::now(),
            mark: None,
            next_mark: Instant::now(),
            themes: cfg.themes(),
            theme: cfg.theme_index(),
            view: match cfg.view.to_lowercase().as_str() {
                "grid" => View::Grid,
                "cards" | "carousel" => View::Cards,
                _ => View::Space,
            },
            panes: Vec::new(),
            focus: 0,
            next_id: 0,
            spaces: Vec::new(),
            next_space: 0,
            saved: config::load_spaces(),
            smenu: None,
            smenu_sel: None,
            rename: None,
            last_tab_click: None,
            card_scroll: 0,
            mods: ModifiersState::empty(),
            mouse: (0, 0),
            hits: Vec::new(),
            drag: None,
            hover: None,
            last_header_click: None,
            menu_open: false,
            menu_sel: None,
            sub: None,
            sub_sel: None,
            text,
            picker: None,
            pick_scroll: 0.0,
            pick_rehover: false,
            fades: HashMap::new(),
            last_tick: Instant::now(),
            animating: false,
            card_anim: Tween::at(0.0),
            repaint: None,
            frame: Vec::new(),
            dirty: true,
            partial_ok: false,
            damage: None,
            next_frame: Instant::now(),
            close_font,
            icon: CursorIcon::Default,
            quit: false,
        };
        ui.sync_window_theme();
        let panels = if cfg.panel.is_empty() { vec![PanelCfg::default()] } else { cfg.panel };
        for p in panels {
            let Some(space) = ui.room() else { break };
            ui.add_pane(p, space);
        }
        ui.focus = 0;
        ui.relayout();
        ui
    }

    // ---- layout ----

    /// Height of the single header row.
    fn toolbar_h(&self) -> i32 {
        px(HEADER, self.scale)
    }

    /// Which window edge/corner (if any) is under the pointer; the window has no OS frame to resize from.
    fn edge_at(&self, x: i32, y: i32) -> Option<ResizeDirection> {
        if self.maximized() {
            return None;
        }
        let s = self.window.inner_size();
        let (w, h, m) = (s.width as i32, s.height as i32, 5);
        let (l, r, t, b) = (x < m, x >= w - m, y < 3, y >= h - m);
        use ResizeDirection::*;
        match (l, r, t, b) {
            (true, _, true, _) => Some(NorthWest),
            (_, true, true, _) => Some(NorthEast),
            (true, _, _, true) => Some(SouthWest),
            (_, true, _, true) => Some(SouthEast),
            (true, ..) => Some(West),
            (_, true, ..) => Some(East),
            (_, _, true, _) => Some(North),
            (_, _, _, true) => Some(South),
            _ => None,
        }
    }

    /// Maximized, or in borderless fullscreen (what the maximize button does by default, see MAXIMIZE).
    fn maximized(&self) -> bool {
        self.window.is_maximized() || self.window.fullscreen().is_some()
    }

    fn card_w(&self) -> i32 {
        card_width(self.window.inner_size().width as i32, self.font.cw, px(PANE_PAD, self.scale))
    }

    /// Card view with more than two terminals: the focused one in the center, the rest stacked behind it.
    fn carousel(&self) -> bool {
        self.view == View::Cards && self.panes.len() > 2
    }

    /// Terminal area of a pane: PANE_PAD on every side, inside the 1px divider on the right and bottom.
    fn inner(&self, o: Rect) -> Rect {
        pane_inner(o, self.scale)
    }

    /// (pane index, outer rect, terminal rect) for every pane currently on screen, back to front.
    fn slots(&self) -> Vec<(usize, Rect, Rect)> {
        let size = self.window.inner_size();
        let (w, h) = (size.width as i32, size.height as i32);
        let top = self.toolbar_h();
        let area = Rect { x: 0, y: top, w, h: h - top };
        let n = self.panes.len();
        let outers: Vec<(usize, Rect)> = match self.view {
            View::Cards if n > 2 => carousel_rects(area, self.card_w(), self.card_scroll, n),
            View::Space => {
                let space = self.active_space();
                tile(area, (0..n).filter(|&i| self.panes[i].space == space).collect())
            }
            // a grid page holds four; one or two terminals tile the same way in both views
            View::Grid | View::Cards => {
                let page = self.focus / 4;
                tile(area, (page * 4..(page * 4 + 4).min(n)).collect())
            }
        };
        outers.into_iter().map(|(i, o)| (i, o, self.inner(o))).collect()
    }

    /// Id of the active space: the one the focused terminal is in.
    fn active_space(&self) -> usize {
        self.panes.get(self.focus).map_or(0, |p| p.space)
    }

    /// Terminals in space `id` that are not on their way out.
    fn space_count(&self, id: usize) -> usize {
        self.panes.iter().filter(|p| p.space == id && !p.closing).count()
    }

    /// A new space, named "Space N" with the lowest N not in use.
    fn new_space(&mut self) -> usize {
        let names: Vec<&str> = self.spaces.iter().map(|s| s.name.as_str()).collect();
        let (id, name) = (self.next_space, next_space_name(&names));
        self.next_space += 1;
        self.spaces.push(Space { id, name });
        id
    }

    /// Where the next terminal goes: the active space if it has room, else a new space, else (all five full) any
    /// space with room. None: every space is full, nothing can be added.
    fn room(&mut self) -> Option<usize> {
        let active = self.active_space();
        if !self.panes.is_empty() && self.space_count(active) < SPACE_SIZE {
            return Some(active);
        }
        if self.spaces.len() < MAX_SPACES {
            return Some(self.new_space());
        }
        self.spaces.iter().map(|s| s.id).find(|&s| self.space_count(s) < SPACE_SIZE)
    }

    fn can_add(&self) -> bool {
        self.panes.is_empty() || self.spaces.len() < MAX_SPACES || self.spaces.iter().any(|s| self.space_count(s.id) < SPACE_SIZE)
    }

    fn hit_pane(&self, x: i32, y: i32) -> Option<(usize, Rect, Rect)> {
        if y < self.toolbar_h() {
            return None;
        }
        self.slots().into_iter().rev().find(|(i, o, _)| !self.panes[*i].closing && o.contains(x, y)) // frontmost first
    }

    /// Turns (animated) the carousel so the focused card sits in the center, the short way round the ring.
    fn ensure_visible(&mut self) {
        if !self.carousel() {
            (self.card_scroll, self.card_anim) = (0, Tween::at(0.0));
            return;
        }
        let (cw, ring) = (self.card_w(), self.panes.len() as i32 * self.card_w());
        let mut turn = (self.focus as i32 * cw - self.card_scroll).rem_euclid(ring);
        if turn > ring / 2 {
            turn -= ring;
        }
        let target = self.card_scroll + turn;
        if target != self.card_scroll && target as f32 != self.card_anim.to {
            self.card_anim = Tween { from: self.card_scroll as f32, to: target as f32, t: 0.0, dur: ANIM_SECS };
            self.start_animating();
        }
    }

    /// Recompute every visible pane's terminal size and tell its PTY.
    fn relayout(&mut self) {
        if self.view == View::Cards {
            self.ensure_visible();
        }
        for (i, _, r) in self.slots() {
            if r.w < self.font.cw * 2 || r.h < self.font.ch {
                continue; // minimized (Windows reports 0x0): keep the old size, or the shell redraws 2 columns wide
            }
            let (cols, rows) = ((r.w / self.font.cw).max(2) as usize, (r.h / self.font.ch).max(1) as usize);
            let p = &mut self.panes[i];
            if (cols, rows) != (p.term.cols, p.term.rows) {
                p.term.resize(cols, rows);
                p.sel = None; // its line numbers point at the old wrapping
                p.master.resize(cols, rows);
            }
        }
        self.invalidate();
    }

    // ---- panes ----

    /// Makes the OS title bar follow the theme (friendly counts as dark: light text on a grey surface).
    fn sync_window_theme(&self) {
        self.window.set_theme(Some(if self.theme == 0 { WinTheme::Light } else { WinTheme::Dark }));
    }

    /// The picker's rows or highlight changed (typing, keys, wheel). Its row rectangles from the last frame no longer
    /// match what `Pick(k)` means, and a click can arrive before the next paint: drop them, so such a click is
    /// swallowed instead of choosing the wrong folder.
    fn picker_changed(&mut self) {
        self.hits.retain(|h| !matches!(h.1, Action::Pick(_)));
        self.invalidate();
    }

    /// A dropdown or picker is open: it takes every click and key, and hover fades pause.
    fn modal(&self) -> bool {
        self.menu_open || self.picker.is_some() || self.smenu.is_some()
    }

    fn add_pane(&mut self, cfg: PanelCfg, space: usize) {
        let id = self.next_id;
        self.next_id += 1;
        let dir = cfg.dir.as_deref().map(PathBuf::from).filter(|d| d.is_dir());
        let (master, mut writer) = spawn_pty(&self.proxy, id, self.shell.as_deref(), dir.as_deref(), self.ipc.as_ref());
        if let Some(cmd) = cfg.cmd.as_deref().filter(|c| !c.is_empty()) {
            let _ = writer.write_all(format!("{cmd}\r").as_bytes());
            let _ = writer.flush();
        }
        let color = |s: &Option<String>| s.as_deref().and_then(config::hex);
        let (bg, fg, accent) = (color(&cfg.bg), color(&cfg.fg), color(&cfg.accent));
        let mut vis = Tween::at(0.0);
        vis.retarget(1.0, ANIM_SECS);
        self.panes.push(Pane {
            id,
            space,
            custom: !cfg.name.is_empty(),
            name: if cfg.name.is_empty() { format!("Terminal {}", id + 1) } else { cfg.name },
            dir,
            bg,
            fg,
            accent,
            term: Term::new(80, 24),
            master,
            writer,
            scroll_pos: 0.0,
            scroll_goal: 0.0,
            vis,
            lit: Tween::at(0.0),
            closing: false,
            anchor: (0, 0),
            sel: None,
            suggest: None,
            dismissed: None,
            claude: None,
            memory: false,
            attention: false,
        });
        self.focus = self.panes.len() - 1;
        self.relayout();
        self.start_animating();
    }

    /// Something changed: repaint the whole window on the next frame.
    fn invalidate(&mut self) {
        self.dirty = true;
        self.partial_ok = false;
        self.window.request_redraw();
    }

    /// Something inside `r` changed: repaint just that rectangle on the next frame (unless a full repaint is pending).
    fn invalidate_rect(&mut self, r: Rect) {
        self.repaint = Some(self.repaint.map_or(r, |d| d.union(&r)));
        if !self.dirty {
            self.partial_ok = true;
        }
        self.window.request_redraw();
    }

    /// The on-screen rectangle of pane `i`, or None if it is not visible (a card scrolled out, another grid page).
    fn visible_rect(&self, i: usize) -> Option<Rect> {
        let size = self.window.inner_size();
        let body = Rect { x: 0, y: self.toolbar_h(), w: size.width as i32, h: size.height as i32 - self.toolbar_h() };
        self.slots().into_iter().find(|s| s.0 == i).map(|s| s.1).filter(|o| {
            let v = o.intersect(&body);
            v.w > 0 && v.h > 0
        })
    }

    /// Makes sure the frame loop runs: time that passed while idle must not count toward the new animation.
    fn start_animating(&mut self) {
        self.animating = true;
        self.last_tick = Instant::now();
        self.invalidate();
    }

    /// Starts closing a panel: it fades out, then `tick` removes it. Closing the last panel exits the app.
    fn begin_close(&mut self, i: usize) {
        if i >= self.panes.len() || self.panes[i].closing {
            return;
        }
        if self.panes.iter().filter(|p| !p.closing).count() <= 1 {
            self.quit = true;
            return;
        }
        let p = &mut self.panes[i];
        if let (true, Some(s)) = (p.memory, &mut p.claude) {
            s.read();
            s.summarize(); // the shell goes, and Claude with it: no SessionEnd will come
        }
        p.closing = true;
        p.vis.retarget(0.0, ANIM_SECS * p.vis.value()); // closing mid fade-in takes proportionally less
        self.drag = None;
        self.picker = None;
        if self.focus == i {
            self.focus_nearest(i);
        }
        self.relayout();
        self.start_animating();
    }

    /// Moves the focus off pane `i` to the nearest panel that is staying, preferring one in the same space.
    fn focus_nearest(&mut self, i: usize) {
        let space = self.panes[i].space;
        let stay = (0..self.panes.len()).filter(|&j| !self.panes[j].closing);
        if let Some(j) = stay.min_by_key(|&j| (self.panes[j].space != space, j.abs_diff(i))) {
            self.focus = j;
        }
    }

    /// Closes both terminals of space `id` and its tab. The only space left is replaced by a fresh one.
    fn close_space(&mut self, id: usize) {
        let alone = self.panes.iter().all(|p| p.closing || p.space == id);
        let at = self.panes.iter().position(|p| p.space == id && !p.closing).unwrap_or(self.focus);
        for p in self.panes.iter_mut().filter(|p| p.space == id && !p.closing) {
            p.closing = true;
            p.vis.retarget(0.0, ANIM_SECS * p.vis.value());
        }
        self.spaces.retain(|s| s.id != id);
        if alone {
            let fresh = self.new_space();
            self.add_pane(PanelCfg::default(), fresh);
        } else {
            self.focus_nearest(at);
        }
        self.prune();
        self.relayout();
        self.start_animating();
    }

    /// Opens saved space `name` as a new tab, a fresh terminal in each saved folder. Nothing when all five tabs are in use.
    fn load_space(&mut self, name: &str) {
        let Some(saved) = self.saved.iter().find(|s| s.name == name).cloned() else { return };
        if self.spaces.len() >= MAX_SPACES {
            return;
        }
        let id = self.new_space();
        self.spaces.last_mut().unwrap().name = saved.name;
        let first = self.panes.len();
        for dir in saved.dirs.into_iter().take(SPACE_SIZE) {
            self.add_pane(PanelCfg { dir: Some(dir), ..Default::default() }, id);
        }
        if self.panes.len() == first {
            self.add_pane(PanelCfg::default(), id); // saved with no folders: still a usable space
        }
        self.focus = first;
        self.relayout();
    }

    /// Saves space `id` under its current name: the folder of each of its terminals.
    fn save_space(&mut self, id: usize) {
        let Some(name) = self.spaces.iter().find(|s| s.id == id).map(|s| s.name.clone()) else { return };
        let dirs = self.panes.iter().filter(|p| p.space == id && !p.closing).filter_map(Pane::cwd).map(|d| d.display().to_string()).collect();
        config::save_space(&mut self.saved, SavedSpace { name, dirs });
    }

    /// Forgets spaces that have no terminals left, and menus that pointed at them.
    fn prune(&mut self) {
        let panes = &self.panes;
        self.spaces.retain(|s| panes.iter().any(|p| p.space == s.id));
        let exists = |id: usize| self.spaces.iter().any(|s| s.id == id);
        self.smenu = self.smenu.filter(|&id| exists(id));
        if self.rename.as_ref().is_some_and(|(id, _)| !exists(*id)) {
            self.rename = None;
        }
        if self.smenu.is_none() {
            self.picker = self.picker.take().filter(|p| p.kind != Kind::Space);
        }
    }

    /// Removes a panel immediately (after its fade-out); dropping the PTY ends the shell.
    fn remove_pane(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        self.panes.remove(i);
        self.drag = None;
        (self.picker, self.search) = (None, None); // they name a pane by index
        if self.panes.is_empty() {
            self.quit = true;
            return;
        }
        if i < self.focus {
            self.focus -= 1;
        }
        self.focus = self.focus.min(self.panes.len() - 1);
        self.prune();
        self.relayout();
    }

    fn focus_pane(&mut self, i: usize) {
        if i < self.panes.len() {
            self.focus = i;
            self.panes[i].attention = false; // seen
            self.relayout();
        }
    }

    /// A Claude hook from pane `h.pane`: bind its session, read what the transcript gained (saving it while memory is
    /// on), and when Claude stops or asks something you are not looking at, mark the panel and flash the taskbar.
    fn on_hook(&mut self, h: claude::Hook) {
        let Some(i) = self.panes.iter().position(|p| p.id == h.pane) else { return };
        let looking = self.focused && i == self.focus;
        let p = &mut self.panes[i];
        if !h.session.is_empty() && p.claude.as_ref().is_none_or(|s| s.id != h.session) {
            if let Some(old) = &mut p.claude {
                if p.memory {
                    old.summarize(); // a new session replaced it (/clear, or claude restarted)
                }
            }
            p.claude = Some(claude::Session::new(&h));
        }
        let Some(s) = &mut p.claude else { return };
        s.read();
        if p.memory {
            s.flush();
        }
        match h.event.as_str() {
            "Stop" | "Notification" if !looking => {
                p.attention = true;
                if !self.focused {
                    self.window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
            }
            "SessionEnd" => {
                if p.memory {
                    s.summarize();
                }
                p.claude = None;
            }
            _ => {}
        }
        self.invalidate();
    }

    /// Saves and summarizes every panel's memory (maula is closing): the summaries finish on their own.
    fn finish_memory(&mut self) {
        for p in self.panes.iter_mut().filter(|p| p.memory) {
            if let Some(s) = &mut p.claude {
                s.read();
                s.summarize();
            }
        }
    }

    /// Applies the rename box: a non-empty name replaces the space's name.
    fn commit_rename(&mut self) {
        if let Some((id, f)) = self.rename.take() {
            let name = f.query.trim();
            if let (false, Some(s)) = (name.is_empty(), self.spaces.iter_mut().find(|s| s.id == id)) {
                s.name = name.to_string();
            }
            self.invalidate();
        }
    }

    /// Remembers the dropdown's choices (theme, view, Text) in config.toml.
    fn save_config(&self) {
        let view = match self.view {
            View::Space => "space",
            View::Grid => "grid",
            View::Cards => "carousel",
        };
        let [font, size, line, track, accent, window] = self.text;
        config::save(&[
            ("theme", format!("{:?}", THEME_NAMES[self.theme].to_lowercase())),
            ("view", format!("{view:?}")),
            ("font", format!("{:?}", text_choices(0)[font])),
            ("font_size", SIZES[size].to_string()),
            ("line_height", LINES[line].to_string()),
            ("letter_spacing", TRACKS[track].to_string()),
            ("accent", format!("{:?}", ACCENTS[accent].0.to_lowercase())),
            ("maximize", format!("{:?}", ["fullscreen", "window"][window])),
        ]);
    }

    fn run(&mut self, a: Action) {
        if !matches!(a, Action::Pick(_) | Action::Search | Action::Nop) {
            self.picker = None; // anything but a click inside the picker closes it
        }
        let menu_was = self.smenu;
        if !matches!(a, Action::Pick(_) | Action::Search | Action::Nop | Action::LoadMenu(_)) {
            (self.smenu, self.smenu_sel) = (None, None); // a tab menu stays open only for its own submenu
        }
        match a {
            Action::Search => {
                let found = self.hits.iter().find(|h| matches!(h.1, Action::Search)).map(|h| h.0);
                if let (Some(r), Some(pk)) = (found, &mut self.picker) {
                    pk.caret = caret_at(&mut self.ui_font, &pk.query, self.mouse.0 - r.x - px(PICK_INSET, self.scale));
                    self.invalidate();
                }
            }
            Action::Focus(i) => self.focus_pane(i),
            Action::New => {
                let Some(space) = self.room() else { return }; // all five spaces are full
                let dir = self.panes.get(self.focus).and_then(Pane::cwd).map(|d| d.display().to_string());
                self.add_pane(PanelCfg { dir, ..Default::default() }, space);
            }
            Action::Space(id) => {
                let now = Instant::now();
                match self.last_tab_click.take() {
                    Some((s, t)) if s == id && now.duration_since(t) < Duration::from_millis(400) => {
                        let name = self.spaces.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
                        self.rename = Some((id, Picker::text(id, &name)));
                        self.invalidate();
                    }
                    _ => {
                        self.last_tab_click = Some((id, now));
                        if let Some(i) = self.panes.iter().position(|p| p.space == id && !p.closing) {
                            self.focus_pane(i);
                        }
                    }
                }
            }
            Action::SpaceMenu(id) => {
                (self.menu_open, self.menu_sel) = (false, None);
                self.smenu = (menu_was != Some(id)).then_some(id); // the chevron toggles its own menu
                self.invalidate();
            }
            Action::SaveSpace(id) => {
                self.save_space(id);
                self.invalidate();
            }
            Action::LoadMenu(id) => {
                self.pick_scroll = 0.0;
                self.picker = Some(Picker::saved(id, self.saved.iter().map(|s| s.name.clone())));
                self.invalidate();
            }
            Action::CloseSpace(id) => self.close_space(id),
            Action::PickDrive(i) | Action::PickDir(i) => {
                let Some(cwd) = self.panes.get(i).and_then(Pane::cwd) else { return };
                self.focus_pane(i);
                (self.menu_open, self.pick_scroll) = (false, 0.0);
                self.picker = Some(if matches!(a, Action::PickDrive(_)) { Picker::drives(i, picker::drive_of(&cwd)) } else { Picker::dirs(i, cwd) });
                self.invalidate();
            }
            Action::Pick(k) => self.pick(k),
            Action::Close(i) => self.begin_close(i),
            Action::SetView(v) => {
                (self.view, self.menu_open, self.menu_sel) = (v, false, None);
                self.save_config();
                self.relayout();
            }
            Action::SetTheme(t) => {
                (self.theme, self.menu_open, self.menu_sel) = (t, false, None);
                self.save_config();
                self.sync_window_theme();
                self.invalidate();
            }
            Action::SubMenu(k) => {
                (self.sub, self.sub_sel) = (Some(k), Some(self.text[k]));
                self.invalidate();
            }
            Action::SetText(k, i) => {
                self.text[k] = i;
                self.save_config();
                (self.menu_open, self.menu_sel, self.sub) = (false, None, None);
                (self.font, self.title_font) = text_fonts(self.text, self.scale);
                self.relayout();
            }
            Action::ToggleMenu => {
                (self.menu_open, self.menu_sel, self.sub) = (!self.menu_open, None, None);
                self.invalidate();
            }
            Action::InstallHooks => {
                claude::install_hooks();
                (self.hooks, self.menu_open, self.menu_sel) = (claude::hooks_installed(), false, None);
                self.invalidate();
            }
            Action::Memory(i) => {
                let Some(p) = self.panes.get_mut(i) else { return };
                p.memory = !p.memory;
                if let Some(s) = &mut p.claude {
                    if p.memory {
                        s.flush(); // everything so far, then the rest as it comes
                    } else {
                        s.summarize(); // done saving
                    }
                }
                self.invalidate();
            }
            Action::Nop => {}
            Action::Minimize => self.window.set_minimized(true),
            Action::ToggleMaximize => {
                if self.maximized() {
                    self.window.set_fullscreen(None);
                    self.window.set_maximized(false);
                } else if self.text[5] == 0 {
                    self.window.set_fullscreen(Some(Fullscreen::Borderless(None))); // the monitor the window is on
                } else {
                    self.window.set_maximized(true);
                }
            }
            Action::CloseWindow => self.quit = true,
        }
    }

    // ---- input / output ----

    /// Runs row `k` of the open picker in its pane and closes it: types `cd <folder>` (or `D:`) into the shell,
    /// the same thing you would type. ponytail: typed blind, so a busy shell or a full-screen app receives it as text.
    fn pick(&mut self, k: usize) {
        let Some(pk) = self.picker.take() else { return };
        if pk.kind == Kind::Space {
            return match pk.shown().get(k).map(|r| r.to_string()) {
                Some(name) => {
                    (self.smenu, self.smenu_sel) = (None, None);
                    self.load_space(&name);
                }
                None => self.picker = Some(pk),
            };
        }
        let line = pk.shown().get(k).map(|&row| match pk.kind {
            Kind::Drive => picker::drive_line(self.shell.as_deref(), row),
            _ => picker::cd_line(self.shell.as_deref(), &pk.path_of(row)),
        });
        match line {
            Some(l) => self.send(l.as_bytes()),
            None => self.picker = Some(pk),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let p = &mut self.panes[self.focus];
        p.term.scroll = 0;
        p.sel = None;
        if bytes.contains(&b'\r') {
            p.term.prompt = None; // a command was sent: the mark stays away until the shell prompts again
        }
        let _ = p.writer.write_all(bytes);
        let _ = p.writer.flush();
        self.invalidate();
    }

    fn paste_text(&mut self, text: &str) {
        // no ESC: a pasted `\x1b[201~` would end bracketed paste early and run the rest as typed commands
        let text = text.replace("\r\n", "\r").replace('\n', "\r").replace('\x1b', "");
        let data = if self.panes[self.focus].term.bracketed_paste { format!("\x1b[200~{text}\x1b[201~") } else { text };
        self.send(data.as_bytes());
    }

    fn paste(&mut self) {
        if let Some(text) = win::clipboard_get(self.hwnd) {
            self.paste_text(&text);
        }
    }

    fn copy(&mut self) {
        let p = &mut self.panes[self.focus];
        let Some((a, b)) = p.sel.take() else { return };
        let text = p.term.text_range(a, b);
        win::clipboard_set(self.hwnd, &text);
        self.invalidate();
    }

    fn on_output(&mut self, id: usize, bytes: &[u8]) {
        OUTPUTS.fetch_add(1, Ordering::Relaxed);
        let Some(i) = self.panes.iter().position(|p| p.id == id) else { return };
        let p = &mut self.panes[i];
        let before = p.term.scroll;
        p.term.feed(bytes);
        if p.term.scroll > before {
            let moved = (p.term.scroll - before) as f32; // the content kept its place: shift, do not animate
            p.scroll_pos += moved;
            p.scroll_goal += moved;
        }
        if !p.term.reply.is_empty() {
            let _ = p.writer.write_all(&p.term.reply);
            let _ = p.writer.flush();
            p.term.reply.clear();
        }
        self.update_suggest(i);
        if let Some(outer) = self.visible_rect(i) {
            self.invalidate_rect(outer);
        }
    }

    /// Looks at the text left of the cursor; if it ends in `cd <3+ chars>`, list matching sub-directories.
    fn update_suggest(&mut self, i: usize) {
        let p = &mut self.panes[i];
        let t = &p.term;
        let start = t.cy * t.cols;
        let line: String = t.cells[start..start + t.cx].iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect();
        let mut new = None;
        if let Some(arg) = suggest::cd_arg(&line) {
            if p.dismissed.as_deref() != Some(arg) {
                if let Some(mut s) = p.cwd().and_then(|c| suggest::find(&c, arg)) {
                    s.col = t.cx.saturating_sub(s.frag_len);
                    new = Some(s);
                }
            }
        }
        if let (Some(n), Some(o)) = (&mut new, &p.suggest) {
            if n.items == o.items {
                n.sel = o.sel; // same list: keep the user's highlighted row
            }
        }
        p.suggest = new;
    }

    fn on_key(&mut self, ev: &KeyEvent) {
        let (ctrl, shift, alt) = (self.mods.control_key(), self.mods.shift_key(), self.mods.alt_key());
        let key = &ev.logical_key;
        if let Some((id, mut f)) = self.rename.take() {
            // the tab's name box: Enter keeps the text, Esc drops it
            match key {
                Key::Named(NamedKey::Escape) => {}
                Key::Named(NamedKey::Enter) => {
                    self.rename = Some((id, f));
                    self.commit_rename();
                }
                _ => {
                    edit_key(&mut f, key, ctrl, alt);
                    self.rename = Some((id, f));
                }
            }
            return self.invalidate();
        }
        if let Some(mut s) = self.search.take() {
            // type to search; Enter / Up: the older match, Shift+Enter / Down: the newer one; Esc closes (the match stays selected)
            let step = match key {
                Key::Named(NamedKey::Escape) => return self.invalidate(),
                Key::Named(NamedKey::Enter) if shift => 1,
                Key::Named(NamedKey::Enter | NamedKey::ArrowUp) => -1,
                Key::Named(NamedKey::ArrowDown) => 1,
                _ => {
                    let before = s.field.query.clone();
                    edit_key(&mut s.field, key, ctrl, alt);
                    if s.field.query != before {
                        s.hits = self.panes[s.pane].term.find(&s.field.query);
                        s.cur = s.hits.len().saturating_sub(1);
                    }
                    0
                }
            };
            if !s.hits.is_empty() {
                s.cur = (s.cur as isize + step).rem_euclid(s.hits.len() as isize) as usize;
            }
            self.show_match(&s);
            self.search = Some(s);
            return self.invalidate();
        }
        if let Some(pk) = &mut self.picker {
            // type to search (at the caret: Left/Right/Home/End/Delete edit); Up/Down move, Enter goes there,
            // Tab or Right at the end of the text browses into the row, Backspace on an empty box goes up, Esc closes
            match key {
                Key::Named(NamedKey::Escape) => self.picker = None,
                Key::Named(NamedKey::ArrowDown) => pk.step(1),
                Key::Named(NamedKey::ArrowUp) => pk.step(-1),
                Key::Named(NamedKey::Enter) => {
                    let k = pk.sel;
                    return self.pick(k);
                }
                Key::Named(NamedKey::Tab) => {
                    let k = pk.sel;
                    pk.enter(k);
                }
                Key::Named(NamedKey::ArrowRight) if pk.caret >= pk.query.len() => {
                    let k = pk.sel;
                    pk.enter(k);
                }
                _ => edit_key(pk, key, ctrl, alt),
            }
            return self.picker_changed();
        }
        if let Some(id) = self.smenu {
            // the tab menu: arrows move, Enter/Space pick, Esc closes; any other key closes it and is typed as usual
            let rows = space_menu_actions(id);
            self.invalidate();
            match key {
                Key::Named(NamedKey::ArrowDown) => return self.smenu_sel = Some(self.smenu_sel.map_or(0, |i| (i + 1) % rows.len())),
                Key::Named(NamedKey::ArrowUp) => return self.smenu_sel = Some(self.smenu_sel.map_or(rows.len() - 1, |i| (i + rows.len() - 1) % rows.len())),
                Key::Named(NamedKey::Enter | NamedKey::Space) => {
                    return match self.smenu_sel {
                        Some(i) => self.run(rows[i]),
                        None => self.smenu = None,
                    };
                }
                Key::Named(NamedKey::Escape) => return self.smenu = None,
                _ => (self.smenu, self.smenu_sel) = (None, None),
            }
        }
        if self.menu_open {
            // keyboard navigation: arrows move, Enter/Space pick, Esc closes; any other key closes and is typed as usual
            let n = MENU_ACTIONS.len();
            self.invalidate();
            if let (Some(k), Some(i)) = (self.sub, self.sub_sel) {
                // in a submenu: the same keys, and Left goes back to the dropdown
                let m = text_choices(k).len();
                match key {
                    Key::Named(NamedKey::ArrowDown) => return self.sub_sel = Some((i + 1) % m),
                    Key::Named(NamedKey::ArrowUp) => return self.sub_sel = Some((i + m - 1) % m),
                    Key::Named(NamedKey::Enter | NamedKey::Space) => return self.run(Action::SetText(k, i)),
                    Key::Named(NamedKey::ArrowLeft | NamedKey::Escape) => return (self.sub, self.sub_sel) = (None, None),
                    _ => {}
                }
            }
            match key {
                Key::Named(NamedKey::ArrowDown) => return (self.menu_sel, self.sub) = (Some(self.menu_sel.map_or(0, |i| (i + 1) % n)), None),
                Key::Named(NamedKey::ArrowUp) => return (self.menu_sel, self.sub) = (Some(self.menu_sel.map_or(n - 1, |i| (i + n - 1) % n)), None),
                Key::Named(NamedKey::ArrowRight) => {
                    return if let Some(i) = self.menu_sel.filter(|&i| matches!(MENU_ACTIONS[i], Action::SubMenu(_))) {
                        self.run(MENU_ACTIONS[i])
                    }
                }
                Key::Named(NamedKey::Enter | NamedKey::Space) => {
                    return match self.menu_sel {
                        Some(i) => self.run(MENU_ACTIONS[i]),
                        None => self.menu_open = false,
                    };
                }
                Key::Named(NamedKey::Escape) => return self.menu_open = false,
                _ => self.menu_open = false,
            }
        }
        let ch = if let Key::Character(c) = key { c.to_lowercase() } else { String::new() };
        if ctrl && shift && !alt {
            match ch.as_str() {
                "n" => return self.run(Action::New),
                "w" => return self.run(Action::Close(self.focus)),
                "c" => return self.copy(),
                "v" => return self.paste(),
                "f" => {
                    self.search = Some(Search { pane: self.focus, field: Picker::text(self.focus, ""), hits: Vec::new(), cur: 0 });
                    return self.invalidate();
                }
                "g" => {
                    return self.run(Action::SetView(match self.view {
                        View::Space => View::Grid,
                        View::Grid => View::Cards,
                        View::Cards => View::Space,
                    }))
                }
                _ => {}
            }
        }
        if ctrl && !alt && *key == Key::Named(NamedKey::Tab) {
            // next panel that is not on its way out; in Space view, one of this space. With Shift: the next space
            let space = self.active_space();
            if shift {
                let ids: Vec<usize> = self.spaces.iter().map(|s| s.id).collect();
                let at = ids.iter().position(|&id| id == space).unwrap_or(0);
                let live = |id: usize| self.panes.iter().position(|p| p.space == id && !p.closing);
                if let Some(j) = (1..=ids.len()).find_map(|k| live(ids[(at + k) % ids.len()])) {
                    self.focus_pane(j);
                }
                return;
            }
            let n = self.panes.len();
            let shown = |j: usize| !self.panes[j].closing && (self.view != View::Space || self.panes[j].space == space);
            if let Some(j) = (1..=n).map(|k| (self.focus + k) % n).find(|&j| shown(j)) {
                self.focus_pane(j);
            }
            return;
        }
        if ctrl && !shift && !alt {
            if ch == "v" {
                return self.paste();
            }
            if ch == "c" && self.panes[self.focus].sel.is_some() {
                return self.copy(); // Ctrl+C copies only while text is selected, otherwise it is ^C
            }
        }
        let p = &mut self.panes[self.focus];
        if p.term.scroll == 0 {
            if let Some(s) = &mut p.suggest {
                let n = s.items.len();
                match key {
                    Key::Named(NamedKey::ArrowDown) => s.sel = (s.sel + 1) % n,
                    Key::Named(NamedKey::ArrowUp) => s.sel = (s.sel + n - 1) % n,
                    Key::Named(NamedKey::Tab) => {
                        let bytes = s.accept_bytes();
                        p.suggest = None;
                        return self.send(&bytes);
                    }
                    Key::Named(NamedKey::Escape) => {
                        p.dismissed = Some(s.key.clone());
                        p.suggest = None;
                    }
                    _ => {
                        return self.send_key(ev);
                    }
                }
                return self.invalidate(); // popup keys never reach the shell
            }
        }
        self.send_key(ev);
    }

    /// Selects the current match of `s` and scrolls it to mid-screen if it is out of view.
    fn show_match(&mut self, s: &Search) {
        let p = &mut self.panes[s.pane];
        let Some(&(line, a, b)) = s.hits.get(s.cur) else { return p.sel = None };
        p.sel = Some(((line, a), (line, b)));
        let (hist, rows) = (p.term.history.len(), p.term.rows);
        let top = hist - p.term.scroll;
        if line < top || line >= top + rows {
            p.term.scroll = hist - line.saturating_sub(rows / 2).min(hist); // `tick` glides the view there
        }
    }

    fn send_key(&mut self, ev: &KeyEvent) {
        if self.mods.shift_key() && !self.mods.control_key() && ev.logical_key == Key::Named(NamedKey::Enter) {
            return self.send(b"\x1b\r"); // newline without submitting (Alt+Enter, which Claude Code understands)
        }
        if let Some(b) = key_bytes(ev, self.mods, self.panes[self.focus].term.app_cursor) {
            self.send(&b);
        }
    }

    // ---- mouse ----

    fn cell_at(&self, i: usize, inner: Rect, x: i32, y: i32) -> Pos {
        let t = &self.panes[i].term;
        let col = ((x - inner.x) / self.font.cw).clamp(0, t.cols as i32 - 1) as usize;
        let row = ((y - inner.y) / self.font.ch).clamp(0, t.rows as i32 - 1) as usize;
        ((t.history.len() as f32 - self.panes[i].scroll_pos).round() as usize + row, col)
    }

    fn on_mouse_button(&mut self, button: MouseButton, state: ElementState) {
        let (x, y) = self.mouse;
        if state == ElementState::Released {
            self.drag = None;
            return;
        }
        match button {
            MouseButton::Left => {
                if let Some(dir) = self.edge_at(x, y) {
                    let _ = self.window.drag_resize_window(dir);
                    return;
                }
                self.commit_rename(); // a click anywhere else keeps the name typed so far
                if self.modal() {
                    // dropdowns and picker are modal: only their own items (and the menu buttons) respond; any other click just closes them
                    return match self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|h| h.1) {
                        Some(
                            a @ (Action::ToggleMenu
                            | Action::SetTheme(_)
                            | Action::SetView(_)
                            | Action::SubMenu(_)
                            | Action::SetText(..)
                            | Action::InstallHooks
                            | Action::Pick(_)
                            | Action::Search
                            | Action::Nop
                            | Action::SpaceMenu(_)
                            | Action::SaveSpace(_)
                            | Action::LoadMenu(_)
                            | Action::CloseSpace(_)),
                        ) => self.run(a),
                        _ => {
                            (self.menu_open, self.picker, self.smenu) = (false, None, None);
                            self.invalidate();
                        }
                    };
                }
                if let Some(a) = self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|h| h.1) {
                    return self.run(a);
                }
                if y < self.toolbar_h() {
                    // empty header space acts as the title bar: drag to move, double-click to maximize
                    let now = Instant::now();
                    if self.last_header_click.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400)) {
                        self.last_header_click = None;
                        self.run(Action::ToggleMaximize);
                    } else {
                        self.last_header_click = Some(now);
                        let _ = self.window.drag_window();
                    }
                    return;
                }
                if let Some((i, _, inner)) = self.hit_pane(x, y) {
                    let pos = self.cell_at(i, inner, x, y);
                    if self.mods.control_key() {
                        // Ctrl+click: open the link or file path under the pointer
                        let p = &self.panes[i];
                        if let Some(t) = p.term.word_at(pos.0, pos.1).zip(p.cwd()).and_then(|(w, cwd)| link::parse(&w, &cwd)) {
                            return link::open(&t, self.editor.as_deref());
                        }
                    }
                    if i != self.focus {
                        self.focus_pane(i);
                    }
                    let p = &mut self.panes[i];
                    (p.anchor, p.sel) = (pos, None);
                    self.drag = Some(i);
                    self.invalidate();
                }
            }
            MouseButton::Right => {
                if self.panes[self.focus].sel.is_some() { self.copy() } else { self.paste() }
            }
            _ => {}
        }
    }

    fn on_cursor(&mut self, x: i32, y: i32) {
        let moved = self.mouse != (x, y); // repeated events at the same spot must not undo a wheel/keyboard selection
        self.mouse = (x, y);
        let hover = self.hits.iter().position(|(r, _)| r.contains(x, y));
        let over_text = hover.is_none() && self.hit_pane(x, y).is_some_and(|(_, _, inner)| inner.contains(x, y));
        let icon = match self.edge_at(x, y) {
            Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
            Some(ResizeDirection::East | ResizeDirection::West) => CursorIcon::EwResize,
            Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => CursorIcon::NwseResize,
            Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => CursorIcon::NeswResize,
            None if hover.is_some_and(|h| matches!(self.hits[h].1, Action::Search)) => CursorIcon::Text,
            None if hover.is_some() => CursorIcon::Pointer,
            None if over_text => CursorIcon::Text,
            None => CursorIcon::Default,
        };
        if icon != self.icon {
            self.icon = icon;
            self.window.set_cursor(icon);
        }
        if self.menu_open {
            // the dropdown's rows are the first hits, in order, then the open submenu's; hovering one highlights it,
            // and hovering a Text row opens its submenu
            let n = MENU_ACTIONS.len();
            match hover {
                Some(i) if i < n => {
                    self.menu_sel = Some(i);
                    let k = if let Action::SubMenu(k) = MENU_ACTIONS[i] { Some(k) } else { None };
                    if k != self.sub {
                        (self.sub, self.sub_sel) = (k, None);
                    }
                }
                Some(i) if self.sub.is_some_and(|k| i < n + text_choices(k).len()) => self.sub_sel = Some(i - n),
                _ => (self.menu_sel, self.sub_sel) = (None, None),
            }
        } else if self.smenu.is_some() && self.picker.is_none() {
            self.smenu_sel = hover.filter(|&i| i < SPACE_MENU_ROWS);
        }
        let row = hover.and_then(|h| if let Action::Pick(k) = self.hits[h].1 { Some(k) } else { None });
        if let (true, Some(k), Some(pk)) = (moved, row, &mut self.picker) {
            if pk.sel != k {
                pk.sel = k;
                self.invalidate(); // the row under the pointer changed even if the hit index did not (the list scrolled)
            }
        }
        if hover != self.hover {
            self.hover = hover;
            self.partial_ok = !self.modal(); // a hover fade only repaints its own rectangle
            self.window.request_redraw();
        }
        let Some(i) = self.drag else { return };
        let Some((_, outer, inner)) = self.slots().into_iter().find(|s| s.0 == i) else { return };
        let head = self.cell_at(i, inner, x, y);
        let p = &mut self.panes[i];
        p.sel = if head != p.anchor { Some((p.anchor, head)) } else { None };
        self.invalidate_rect(outer);
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        // `fine`: a touchpad (or free-spinning wheel) sends many small deltas and does its own inertia, so it is
        // applied as-is. A notched wheel sends whole steps, which glide.
        let (dy, fine) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (y * 3.0, x.fract() != 0.0 || y.fract() != 0.0), // dy in lines
            MouseScrollDelta::PixelDelta(p) => (p.y as f32 / self.font.ch as f32, true),
        };
        if let Some(pk) = &mut self.picker {
            // the wheel moves the highlight, not the terminal behind. Distance adds up until it covers a row:
            // one notch = one row, and a touchpad's stream of tiny deltas no longer means a step per event
            let row = if fine { px(32.0, self.scale) as f32 / self.font.ch as f32 } else { 3.0 }; // a row, in the units of dy (lines)
            if self.pick_scroll * dy < 0.0 {
                self.pick_scroll = 0.0; // turned around: leftover distance from the other way must not delay the response
            }
            self.pick_scroll += dy;
            while self.pick_scroll.abs() >= row {
                let d = self.pick_scroll.signum();
                pk.scroll(-d as isize);
                self.pick_scroll -= d * row;
                self.pick_rehover = true;
            }
            return self.picker_changed();
        }
        if let Some((i, outer, _)) = self.hit_pane(self.mouse.0, self.mouse.1) {
            let p = &mut self.panes[i];
            // keep the fractional part: small touchpad deltas add up instead of being rounded away
            p.scroll_goal = (p.scroll_goal + dy).clamp(0.0, p.term.history.len() as f32);
            p.term.scroll = p.scroll_goal.round() as usize;
            if fine {
                p.scroll_pos = p.scroll_goal; // follow the fingers exactly
            }
            return self.invalidate_rect(outer);
        }
        self.invalidate();
    }

    // ---- drawing ----

    /// Advances hover fades and smooth scrolling by the time since the last frame.
    fn tick(&mut self) {
        let now = Instant::now();
        // time that passed while nothing was animating is not animation time (else a new tween would start part-way)
        let dt = if self.animating { now.duration_since(self.last_tick).as_secs_f32().min(0.05) } else { 0.0 };
        self.last_tick = now;
        let mut busy = false;

        let (mouse, modal) = (self.mouse, self.modal());
        if !modal {
            for (r, _) in &self.hits {
                if r.contains(mouse.0, mouse.1) {
                    self.fades.entry(rect_key(*r)).or_insert(Tween::at(0.0));
                }
            }
        }
        let mut damage: Option<Rect> = None;
        self.fades.retain(|&(x, y, w, h), tw| {
            let r = Rect { x, y, w, h };
            let target = if !modal && r.contains(mouse.0, mouse.1) { 1.0 } else { 0.0 };
            tw.retarget(target, ANIM_SECS * (target - tw.value()).abs()); // a half-finished fade takes proportionally less
            tw.step(dt);
            busy |= !tw.done();
            damage = Some(damage.map_or(r, |d| d.union(&r))); // this element's pixels changed (even on its last frame)
            !(tw.done() && tw.to == 0.0) // forget finished fade-outs
        });
        self.damage = damage;
        let mut busy_full = false; // animations that move every panel (the carousel): repaint everything
        if !self.card_anim.done() {
            self.card_anim.step(dt);
            self.card_scroll = self.card_anim.value().round() as i32;
            busy_full = true;
        }

        let mut moving: Vec<usize> = Vec::new(); // panels with their own animation running: repaint only those
        let focus = self.focus;
        for (i, p) in self.panes.iter_mut().enumerate() {
            let mut m = false;
            let active = if i == focus && !p.closing { 1.0 } else { 0.0 };
            p.lit.retarget(active, ANIM_SECS * (active - p.lit.value()).abs());
            p.lit.step(dt);
            m |= !p.lit.done();
            let max = p.term.history.len() as f32;
            if p.scroll_goal.round() as usize != p.term.scroll {
                p.scroll_goal = p.term.scroll as f32; // changed elsewhere: typing snapped to the bottom, history was trimmed
            }
            p.scroll_goal = p.scroll_goal.clamp(0.0, max);
            p.scroll_pos = chase(p.scroll_pos, p.scroll_goal, dt, SCROLL_TAU).clamp(0.0, max);
            m |= p.scroll_pos != p.scroll_goal;
            p.vis.step(dt);
            m |= !p.vis.done();
            if m {
                moving.push(i);
            }
        }
        if !moving.is_empty() {
            let slots = self.slots();
            for i in &moving {
                if let Some((_, outer, _)) = slots.iter().find(|s| s.0 == *i) {
                    self.repaint = Some(self.repaint.map_or(*outer, |d| d.union(outer)));
                }
            }
        }
        while let Some(i) = self.panes.iter().position(|p| p.closing && p.vis.done()) {
            self.remove_pane(i);
        }
        self.animating = busy || busy_full || !moving.is_empty();
        if busy_full {
            self.dirty = true;
        }
    }

    fn draw(&mut self) {
        DRAWS.fetch_add(1, Ordering::Relaxed);
        self.tick();
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let (w, h) = (size.width as i32, size.height as i32);
        let slots = self.slots();
        let (hh, maximized, scale) = (self.toolbar_h(), self.maximized(), self.scale);
        // Repaint only where hover fades changed, unless something else changed too (or the size did).
        let resized = self.frame.len() != (w * h) as usize;
        if resized {
            self.frame = vec![0; (w * h) as usize];
        }
        let region = match (self.damage, self.repaint) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        };
        let full = resized || self.dirty || !self.partial_ok || region.is_none();
        let area = if full { Rect { x: 0, y: 0, w, h } } else { region.unwrap().inset(-2) };
        (self.dirty, self.partial_ok, self.repaint) = (false, false, None);
        let (mouse, rehover) = (self.mouse, std::mem::take(&mut self.pick_rehover));
        let (active, can_add) = (self.active_space(), self.can_add());
        let tab_menu = self.smenu.map(|id| (id, self.smenu_sel, self.space_count(id)));
        let (hooks, focused, clock) = (self.hooks, self.focused, self.started.elapsed().as_secs_f32());
        let mut mark = None; // the live prompt mark's cell, if one is on screen
        let Ui { hwnd, frame, font, ui_font: ui, title_font: titlef, close_font: closef, icon_font: icons, panes, hits, focus, view, menu_open, menu_sel, sub, sub_sel, text, picker, fades, themes, theme, spaces, rename, search, .. } = self;
        let th = with_accent(themes[*theme], text[4]);
        let mut cv = Canvas::new(frame.as_mut_slice(), w, h);
        cv.set_bounds(area);
        cv.rect(Rect { x: 0, y: 0, w, h }, th.ui_bg);
        hits.clear();

        let chrome = Chrome { th, scale, w, fades, view: *view, theme: *theme, menu_open: *menu_open, menu_sel: *menu_sel, maximized, can_add, hooks };
        draw_header(&mut cv, icons, &chrome, hits);
        if *view == View::Space {
            let waiting = |id: usize| panes.iter().any(|p| p.space == id && p.attention);
            let tabs: Vec<Tab> = spaces.iter().map(|s| Tab { id: s.id, name: &s.name, active: s.id == active, attention: waiting(s.id) }).collect();
            let editing = rename.as_ref().map(|(id, f)| (*id, f.query.as_str(), f.caret));
            draw_tabs(&mut cv, titlef, icons, &chrome, &tabs, tab_menu.map(|m| m.0), editing, hits);
        }

        // panels: flat, each with its own header, divided by 1px lines on the right and bottom
        let content = Rect { x: 0, y: hh, w, h: h - hh };
        let carousel = *view == View::Cards && panes.len() > 2;
        let panel_hits = hits.len();
        for (idx, outer, inner) in &slots {
            let seen = outer.intersect(&content);
            if seen.w == 0 || seen.h == 0 {
                continue; // a card scrolled out of view: nothing to draw, nothing to click
            }
            let p = &panes[*idx];
            let on = *idx == *focus && !p.closing;
            let pt = Theme { bg: p.bg.unwrap_or(th.bg), fg: p.fg.unwrap_or(th.fg), ..th };
            cv.set_clip(outer.intersect(&content));
            cv.rect(*outer, pt.bg);

            let hr = Rect { x: outer.x, y: outer.y, w: outer.w - 1, h: hh };
            let drive = p.cwd().as_deref().and_then(picker::drive_of);
            let number = if *view == View::Space { slots.iter().position(|s| s.0 == *idx).unwrap_or(0) } else { *idx }; // 01, 02 within a space
            let head = draw_panel_header(&mut cv, titlef, closef, icons, &pt, scale, hr, number, &title(p), drive, on, p.memory, p.attention, fades);
            let first = hits.len();
            if !p.closing {
                // before the header itself, so they win the click
                hits.push((head.close, Action::Close(*idx)));
                hits.push((head.memory, Action::Memory(*idx)));
                hits.extend(head.drive.map(|r| (r, Action::PickDrive(*idx))));
                hits.push((head.dir, Action::PickDir(*idx)));
                hits.push((hr, Action::Focus(*idx)));
            }
            let added = hits.len() - first;
            hits[panel_hits..].rotate_right(added); // panels are drawn back to front: the frontmost one's hits win
            if carousel {
                cv.rect(Rect { x: outer.x, y: outer.y, w: 1, h: outer.h }, th.border); // edge against the panel behind
            }
            cv.rect(Rect { x: outer.x + outer.w - 1, y: outer.y, w: 1, h: outer.h }, th.border);
            cv.rect(Rect { x: outer.x, y: outer.y + outer.h - 1, w: outer.w, h: 1 }, th.border);
            if on {
                // the active panel: accent border along its bottom edge, covering the divider
                cv.rect(Rect { x: outer.x, y: outer.y + outer.h - 2, w: outer.w - 1, h: 2 }, p.accent.unwrap_or(th.accent));
            }

            cv.set_clip(inner.intersect(&content));
            let dim = 1.0 - p.lit.value();
            draw_term(&mut cv, font, &p.term, p.scroll_pos, p.sel, &pt, *inner, on, dim);
            if let (true, Some((line, col))) = (p.term.at_prompt(), p.term.prompt) {
                // the prompt mark, in the two cells the prompt leaves blank; it turns only where you are typing
                let top = p.term.history.len() as f32 - p.scroll_pos;
                let y = inner.y + ((line as f32 - top) * font.ch as f32).round() as i32;
                let cell = Rect { x: inner.x + col as i32 * font.cw, y, w: 2 * font.cw, h: font.ch };
                let live = on && focused && p.term.scroll == 0;
                let color = mix(p.accent.unwrap_or(th.accent), pt.bg, 0.6 * dim);
                draw_prompt_mark(&mut cv, cell, if live { clock } else { 0.0 }, color, pt.bg);
                mark = mark.or(live.then_some(cell));
            }
            cv.set_clip(outer.intersect(&content));
            draw_scrollbar(&mut cv, &p.term, p.scroll_pos, &pt, *outer, hh, scale);
            if on && p.term.scroll == 0 {
                if let Some(s) = &p.suggest {
                    draw_suggest(&mut cv, font, ui, s, &th, p, *inner);
                }
            }
            if let Some(s) = search.as_ref().filter(|s| s.pane == *idx) {
                draw_search(&mut cv, ui, &th, scale, s, *inner);
            }
            let v = p.vis.value(); // creating / closing: the whole panel fades in or out
            if v < 1.0 {
                cv.set_clip(outer.intersect(&content));
                cv.rrect(*outer, 0, pt.bg, ((1.0 - v) * 255.0) as u32);
            }
        }

        cv.set_clip(Rect { x: 0, y: 0, w, h });

        if chrome.menu_open {
            draw_menu(&mut cv, ui, icons, &chrome, *text, sub.map(|k| (k, *sub_sel)), hits);
        }
        if let Some((id, sel, count)) = tab_menu {
            if let Some(anchor) = hits.iter().find(|h| matches!(h.1, Action::SpaceMenu(i) if i == id)).map(|h| h.0) {
                draw_space_menu(&mut cv, ui, &chrome, id, anchor, sel, count, hits);
            }
        }
        let mut again = false; // the highlight moved while drawing: one more frame to show it
        if let Some(pk) = picker.as_mut() {
            // hangs below the chip it was opened from (the Load row of a tab menu: beside it)
            let from = |a: &Action| match (a, pk.kind) {
                (Action::PickDir(i), Kind::Dir) | (Action::PickDrive(i), Kind::Drive) | (Action::LoadMenu(i), Kind::Space) => *i == pk.pane,
                _ => false,
            };
            if let Some(anchor) = hits.iter().find(|h| from(&h.1)).map(|h| h.0) {
                draw_picker(&mut cv, ui, icons, &th, scale, pk, anchor, (w, h), hits);
            }
            if rehover {
                // the wheel slid other rows under a still pointer: the highlight follows what is now under it
                let under = hits.iter().find(|h| h.0.contains(mouse.0, mouse.1)).and_then(|h| if let Action::Pick(k) = h.1 { Some(k) } else { None });
                if let Some(k) = under.filter(|&k| k != pk.sel) {
                    (pk.sel, again) = (k, true);
                }
            }
        }

        // hairline frame, since the window has no OS border
        if !maximized {
            for r in [Rect { x: 0, y: 0, w, h: 1 }, Rect { x: 0, y: h - 1, w, h: 1 }, Rect { x: 0, y: 0, w: 1, h }, Rect { x: w - 1, y: 0, w: 1, h }] {
                cv.rect(r, th.border);
            }
        }
        let a = if full { Rect { x: 0, y: 0, w, h } } else { area.intersect(&Rect { x: 0, y: 0, w, h }) };
        win::present(*hwnd, frame, w, (a.x, a.y, a.w, a.h));
        self.mark = mark;
        if again {
            self.invalidate();
        }
    }
}

/// `dim` (0..1) greys the text toward the background: used for inactive terminals.
#[allow(clippy::too_many_arguments)]
fn draw_term(cv: &mut Canvas, font: &mut Font, t: &Term, scroll_pos: f32, sel: Option<(Pos, Pos)>, th: &Theme, r: Rect, focused: bool, dim: f32) {
    let hist = t.history.len();
    // absolute line at the top of the view; fractional while a scroll animation is running
    let top = hist as f32 - scroll_pos.clamp(0.0, hist as f32);
    let base = top.floor() as usize;
    let shift = ((top - base as f32) * font.ch as f32).round() as i32; // pixels the content sits above its row
    let sel = sel.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    for row in 0..=t.rows {
        let abs = base + row;
        if abs >= t.total_lines() {
            break;
        }
        let line = t.line(abs);
        let y = r.y + row as i32 * font.ch - shift;
        let clip = cv.clip();
        if y + font.ch <= clip.y || y >= clip.y + clip.h {
            continue; // outside the area being repainted
        }
        let (first, last) = (((clip.x - r.x) / font.cw).max(0) as usize, ((clip.x + clip.w - r.x) / font.cw + 1).max(0) as usize);
        for col in first.min(t.cols)..last.min(t.cols) {
            let cell = line.get(col).copied().unwrap_or(BLANK);
            let x = r.x + col as i32 * font.cw;
            let (mut fg, mut bg) = (mix(th.rgb(cell.fg), th.bg, 0.6 * dim), th.rgb(cell.bg));
            if cell.dim {
                fg = mix(fg, bg, 0.5);
            }
            if let Some((a, b)) = sel {
                if (abs, col) >= a && (abs, col) <= b {
                    bg = th.selection;
                }
            }
            if focused && t.cursor_visible && abs == hist + t.cy && col == t.cx {
                (bg, fg) = (th.cursor, th.bg);
            }
            if bg != th.bg {
                cv.rect(Rect { x, y, w: font.cw, h: font.ch }, bg);
            }
            if cell.ch != ' ' && cell.ch != '\0' {
                cv.glyph(font, cell.ch, x, y, fg);
            }
        }
    }
}

/// Thin scrollbar on the panel's outer right edge, shown once there is scrollback. Its track starts below the
/// panel header and stops above the bottom border, so it never overlaps either.
fn draw_scrollbar(cv: &mut Canvas, t: &Term, scroll_pos: f32, th: &Theme, outer: Rect, header_h: i32, scale: f32) {
    if t.history.is_empty() {
        return;
    }
    let hist = t.history.len() as f32;
    let top = hist - scroll_pos.clamp(0.0, hist);
    let (track_y, track) = (outer.y + header_h + 1, outer.h - header_h - 3);
    let thumb = (track * t.rows as i32 / t.total_lines() as i32).max(14);
    let y = track_y + ((track - thumb) as f32 * top / hist) as i32;
    let thick = px(4.0, scale);
    cv.rrect(Rect { x: outer.x + outer.w - px(7.0, scale), y, w: thick, h: thumb }, thick / 2, th.sub, 120);
}

// ---- design sizes ----
// Everything below is in design pixels ("36px" means 36 at 100% display scaling). `px` scales them to
// real pixels in one place, so the layout is identical at 100%, 125%, 150%... only sharpness changes.
const HEADER: f32 = 36.0; // header height; also the width and height of every header button
const ICON: f32 = 16.0; // every icon
const CLOSE_PX: f32 = 11.0; // the "CLOSE" button text
const TITLE_GAP: f32 = 16.0; // text to chevron box in a panel header; the glyph's own margin makes it ~20px to the ink
/// One animation frame (60 fps).
const FRAME: Duration = Duration::from_micros(16_667);
const PANE_PAD: f32 = 24.0; // space between a terminal panel's edge and its text, all four sides

fn px(n: f32, scale: f32) -> i32 {
    (n * scale).round() as i32
}

/// Terminal area inside a pane: below its header, PANE_PAD on every side, plus the 1px divider on the right and bottom.
fn pane_inner(o: Rect, scale: f32) -> Rect {
    let (p, hh) = (px(PANE_PAD, scale), px(HEADER, scale));
    Rect { x: o.x + p, y: o.y + hh + p, w: o.w - 1 - 2 * p, h: o.h - hh - 1 - 2 * p }
}

/// Hover amount of the element at `r`: 0 (idle) .. 1 (hovered), eased over time by `Ui::tick`.
fn fade_of(fades: &HashMap<RectKey, Tween>, r: Rect) -> f32 {
    fades.get(&rect_key(r)).map_or(0.0, Tween::value)
}

/// Where a panel header can be clicked.
struct PanelHits {
    close: Rect,
    memory: Rect,
    drive: Option<Rect>, // the "C:" chip; none when the shell is not on a drive
    dir: Rect,           // the folder-name chip
}

/// Header text with a chevron `gap` design px after it; hover brightens both. Returns the hit rectangle.
/// The chevron glyph has ~4px of empty space on each side, so its ink starts that much beyond `gap`.
#[allow(clippy::too_many_arguments)]
fn chip(cv: &mut Canvas, titlef: &mut Font, icons: &mut Font, th: &Theme, scale: f32, hr: Rect, x: i32, label: &str, gap: f32, fades: &HashMap<RectKey, Tween>) -> Rect {
    let track = titlef.track;
    let (tw, bw) =(titlef.width_tracked(label, track), px(ICON, scale));
    let r = Rect { x, y: hr.y, w: tw + px(gap, scale) + bw, h: hr.h };
    let color = mix(th.sub, th.ui_fg, fade_of(fades, r));
    cv.text_tracked(titlef, label, x, hr.y + (hr.h - titlef.ch) / 2, color, track);
    icon(cv, icons, Icon::Chevron, Rect { x: x + tw + px(gap, scale), y: hr.y, w: bw, h: hr.h }, color);
    r
}

/// A terminal panel's own header, styled like a spec-sheet table row: a tinted band, `01 · C: ⌄ · title ⌄` (ink
/// number, muted letter-spaced labels; the two chevrons open the drive and folder pickers), and small uppercase
/// "MEMORY" (its square green while on) and "CLOSE", each in its own cell behind a hairline. `attention`: Claude is
/// waiting here, the number turns accent. Returns the clickable rectangles.
#[allow(clippy::too_many_arguments)]
fn draw_panel_header(cv: &mut Canvas, titlef: &mut Font, closef: &mut Font, icons: &mut Font, th: &Theme, scale: f32, hr: Rect, index: usize, title: &str, drive: Option<char>, active: bool, memory: bool, attention: bool, fades: &HashMap<RectKey, Tween>) -> PanelHits {
    let (pad, track, dot) = (px(16.0, scale), titlef.track, px(6.0, scale));
    let th = &Theme { head_bg: if active { th.head_bg } else { th.bg }, ..*th }; // inactive: blends into its terminal
    cv.rect(hr, th.head_bg);
    // a cell at the right end of the header, ending at `right`: hover tint, label, hairline on its left
    let mut cell = |label: &str, lead: i32, right: i32| {
        let w = lead + closef.width(label) + 2 * pad;
        let r = Rect { x: right - w, y: hr.y, w, h: hr.h };
        let f = fade_of(fades, r);
        if f > 0.0 {
            cv.rect(r, mix(th.head_bg, th.ui_fg, 0.10 * f));
        }
        cv.text(closef, label, r.x + pad + lead, hr.y + (hr.h - closef.ch) / 2, th.ui_fg);
        cv.rect(Rect { x: r.x - 1, y: hr.y, w: 1, h: hr.h - 1 }, th.border);
        r
    };
    let close = cell("CLOSE", 0, hr.x + hr.w);
    let mem = cell("MEMORY", dot + px(8.0, scale), close.x - 1);
    let sq = Rect { x: mem.x + pad, y: hr.y + (hr.h - dot) / 2, w: dot, h: dot };
    cv.rect(sq, if memory { th.green } else { th.border });
    let y = hr.y + (hr.h - titlef.ch) / 2;
    let mut x = hr.x + pad;
    let number = if attention { th.accent } else if active { th.ui_fg } else { th.sub };
    x += cv.text_tracked(titlef, &format!("{:02}", index + 1), x, y, number, track);
    x += cv.text_tracked(titlef, " · ", x, y, th.sub, track);
    let drive = drive.map(|d| {
        let r = chip(cv, titlef, icons, th, scale, hr, x, &format!("{d}:"), 0.0, fades);
        x += r.w;
        x += cv.text_tracked(titlef, " · ", x, y, th.sub, track);
        r
    });
    let room = mem.x - 1 - x - pad - px(TITLE_GAP, scale) - px(ICON, scale);
    let label = fit(titlef, title, room, track);
    let dir = chip(cv, titlef, icons, th, scale, hr, x, &label, TITLE_GAP, fades);
    cv.rect(Rect { x: hr.x, y: hr.y + hr.h - 1, w: hr.w + 1, h: 1 }, th.border);
    PanelHits { close, memory: mem, drive, dir }
}

/// Width of one card: 90 columns, or exactly half the window when the window is wide enough to show two.
fn card_width(window_w: i32, cw: i32, pad: i32) -> i32 {
    let min = CARD_COLS * cw + 2 * pad + 1; // +1: the divider on the right edge
    if window_w >= 2 * min { window_w / 2 } else { min }
}

/// Splits `area` between the panes `idx` (one to four): full, side by side, two over one, or 2x2.
fn tile(area: Rect, idx: Vec<usize>) -> Vec<(usize, Rect)> {
    let (hw, hh) = (area.w / 2, area.h / 2);
    let tl = Rect { w: hw, h: hh, ..area };
    let tr = Rect { x: area.x + hw, w: area.w - hw, h: hh, ..area };
    let bl = Rect { y: area.y + hh, w: hw, h: area.h - hh, ..area };
    let br = Rect { x: area.x + hw, y: area.y + hh, w: area.w - hw, h: area.h - hh };
    let rects = match idx.len() {
        1 => vec![area],
        2 => vec![Rect { w: hw, ..area }, Rect { x: area.x + hw, w: area.w - hw, ..area }],
        3 => vec![tl, tr, Rect { y: area.y + hh, h: area.h - hh, ..area }],
        _ => vec![tl, tr, bl, br],
    };
    idx.into_iter().zip(rects).collect()
}

/// "Space N" for the lowest N >= 1 that no space is named yet.
fn next_space_name(names: &[&str]) -> String {
    (1..).map(|n| format!("Space {n}")).find(|c| !names.contains(&c.as_str())).unwrap()
}

/// Width of one space tab: the room between the "+" and the window buttons, split `MAX_SPACES` ways, so five tabs
/// fill the header exactly and a tab never changes width as spaces come and go.
fn tab_width(window_w: i32, sq: i32) -> i32 {
    (window_w - 5 * sq) / MAX_SPACES as i32 // 5 squares: menu, "+" and the three window buttons
}

/// Carousel: the cards stand evenly around a ring seen from the front. The card at `scroll` (= its index * `card_w`,
/// fractional mid-slide) is at the front, centered; the others are the same size, shifted sideways by where they sit on
/// the ring, so a dim strip of each peeks out. The ring wraps (the last card is next to the first), and with an even
/// count the one opposite the front is directly behind it, out of sight. Returned back to front, the order to draw them.
fn carousel_rects(area: Rect, card_w: i32, scroll: i32, n: usize) -> Vec<(usize, Rect)> {
    let side = ((area.w - card_w) / 2).max(0); // room on each side of the centered card
    let mut v: Vec<(usize, Rect, f32)> = (0..n)
        .map(|i| {
            let places = ((i as i32 * card_w - scroll) as f32 / card_w as f32).rem_euclid(n as f32); // ahead of the front, 0..n
            let a = places / n as f32 * std::f32::consts::TAU; // angle around the ring: 0 = front, PI = back
            let x = area.x + side + (side as f32 * a.sin()).round() as i32;
            (i, Rect { x, w: card_w, ..area }, a.cos())
        })
        .collect();
    v.sort_by(|a, b| a.2.total_cmp(&b.2));
    v.into_iter().map(|(i, r, _)| (i, r)).collect()
}

/// Every transition lasts this long, like CSS `transition: ... 0.2s ease-in-out`.
const ANIM_SECS: f32 = 0.2;

/// How quickly scrolling glides toward where it is heading (seconds to close ~63% of the gap).
const SCROLL_TAU: f32 = 0.05;

/// Moves `pos` toward `goal`, closing the fraction `1 - e^(-dt/tau)` of the gap per step. Unlike a timed ease, the
/// speed depends only on the distance left, so a goal that keeps moving (a touchpad, a spinning wheel) is followed
/// continuously instead of the motion restarting from zero speed on every new event.
fn chase(pos: f32, goal: f32, dt: f32, tau: f32) -> f32 {
    let gap = goal - pos;
    if gap.abs() < 0.02 { goal } else { pos + gap * (1.0 - (-dt / tau).exp()) }
}

/// CSS `cubic-bezier(x1, y1, x2, y2)`: maps time progress `p` (0..1) to the eased fraction (0..1).
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, p: f32) -> f32 {
    let bez = |a: f32, b: f32, u: f32| {
        let v = 1.0 - u;
        3.0 * v * v * u * a + 3.0 * v * u * u * b + u * u * u
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32); // solve x(u) = p by bisection, then read y(u)
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if bez(x1, x2, mid) < p { lo = mid } else { hi = mid }
    }
    bez(y1, y2, (lo + hi) / 2.0)
}

/// CSS `ease-in-out` = cubic-bezier(0.42, 0, 0.58, 1).
fn ease_in_out(p: f32) -> f32 {
    if p <= 0.0 {
        0.0 // exact endpoints: the bisection above is only accurate to ~1e-7
    } else if p >= 1.0 {
        1.0
    } else {
        cubic_bezier(0.42, 0.0, 0.58, 1.0, p)
    }
}

/// A value moving from `from` to `to` over `dur` seconds along ease-in-out.
#[derive(Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    t: f32,
    dur: f32,
}

impl Tween {
    fn at(v: f32) -> Self {
        Tween { from: v, to: v, t: 0.0, dur: 0.0 }
    }

    fn value(&self) -> f32 {
        if self.done() { self.to } else { self.from + (self.to - self.from) * ease_in_out(self.t / self.dur) }
    }

    fn done(&self) -> bool {
        self.t >= self.dur
    }

    /// Starts a new transition from wherever the value is right now (so reversing mid-way is seamless).
    fn retarget(&mut self, to: f32, dur: f32) {
        if to != self.to {
            *self = Tween { from: self.value(), to, t: 0.0, dur };
        }
    }

    fn step(&mut self, dt: f32) {
        self.t = (self.t + dt).min(self.dur);
    }

    /// Moves the whole animation by `d` (used when the content itself moved, so there is nothing to animate).
    #[cfg(test)]
    fn shift(&mut self, d: f32) {
        self.from += d;
        self.to += d;
    }
}

type RectKey = (i32, i32, i32, i32);

fn rect_key(r: Rect) -> RectKey {
    (r.x, r.y, r.w, r.h)
}

/// `a` blended toward `b` by `f` (0..1).
fn mix(a: u32, b: u32, f: f32) -> u32 {
    blend(a, b, (f.clamp(0.0, 1.0) * 255.0) as u32)
}

impl Chrome<'_> {
    fn fade(&self, r: Rect) -> f32 {
        fade_of(self.fades, r)
    }
}

/// Everything the header and dropdown need to draw, so they can run without a window (see `header_preview`).
struct Chrome<'a> {
    th: Theme,
    scale: f32,
    w: i32,
    fades: &'a HashMap<RectKey, Tween>,
    view: View,
    theme: usize, // index into THEME_NAMES
    menu_open: bool,
    menu_sel: Option<usize>,
    maximized: bool,
    can_add: bool, // false: every space is full, "+" is dimmed and does nothing
    hooks: bool,   // the Claude hooks are installed: their menu row is checked
}

/// One space tab in the header.
struct Tab<'a> {
    id: usize,
    name: &'a str,
    active: bool,
    attention: bool, // a terminal in it has Claude waiting
}

/// Window header: flush to the window edge, no vertical padding, no rounded corners. Menu button and "+" on the
/// left, window buttons on the right, nothing in between (drag it to move the window). Every button is a
/// HEADER x HEADER square; hover fills the whole square.
fn draw_header(cv: &mut Canvas, icons: &mut Font, c: &Chrome, hits: &mut Vec<(Rect, Action)>) {
    let (th, w) = (c.th, c.w);
    let sq = px(HEADER, c.scale);
    cv.rect(Rect { x: 0, y: 0, w, h: sq }, th.ui_bg);

    // right: window buttons  [min][max][close]
    let caps = [
        (Action::Minimize, Icon::Minimize),
        (Action::ToggleMaximize, if c.maximized { Icon::Restore } else { Icon::Maximize }),
        (Action::CloseWindow, Icon::Close),
    ];
    for (i, (act, ic)) in caps.into_iter().enumerate() {
        let r = Rect { x: w - (3 - i as i32) * sq, y: 0, w: sq, h: sq };
        let (f, close) = (c.fade(r), matches!(act, Action::CloseWindow));
        if f > 0.0 {
            cv.rect(r, if close { mix(th.ui_bg, 0xe81123, f) } else { mix(th.ui_bg, th.ui_fg, 0.10 * f) });
        }
        icon(cv, icons, ic, r, mix(th.sub, if close { 0xffffff } else { th.ui_fg }, f));
        hits.push((r, act));
    }

    // left: menu button (filled while its dropdown is open), then "+"
    let btn = Rect { x: 0, y: 0, w: sq, h: sq };
    let f = c.fade(btn);
    if c.menu_open {
        cv.rect(btn, th.ui_fg);
    } else if f > 0.0 {
        cv.rect(btn, mix(th.ui_bg, th.ui_fg, 0.10 * f));
    }
    icon(cv, icons, Icon::Chevron, btn, if c.menu_open { th.ui_bg } else { mix(th.sub, th.ui_fg, f) });
    hits.push((btn, Action::ToggleMenu));

    let plus = Rect { x: sq, y: 0, w: sq, h: sq };
    if c.can_add {
        let f = c.fade(plus);
        if f > 0.0 {
            cv.rect(plus, mix(th.ui_bg, th.ui_fg, 0.10 * f));
        }
        icon(cv, icons, Icon::Plus, plus, mix(th.sub, th.ui_fg, f));
        hits.push((plus, Action::New));
    } else {
        icon(cv, icons, Icon::Plus, plus, mix(th.ui_bg, th.sub, 0.4)); // dimmed, no hit: nothing to add to
    }

    // hairlines last, so hover fills never cover them
    cv.rect(Rect { x: sq - 1, y: 0, w: 1, h: sq }, th.border);
    cv.rect(Rect { x: 2 * sq - 1, y: 0, w: 1, h: sq }, th.border);
    cv.rect(Rect { x: 0, y: sq - 1, w, h: 1 }, th.border);
}

/// The space tabs after the "+": a chevron button (opens the tab's menu) and the name. Every tab is `tab_width` wide;
/// the active one has an accent line under it, like the active panel. `editing` is (space id, text, caret) while
/// a tab is being renamed.
#[allow(clippy::too_many_arguments)]
fn draw_tabs(cv: &mut Canvas, titlef: &mut Font, icons: &mut Font, c: &Chrome, tabs: &[Tab], open: Option<usize>, editing: Option<(usize, &str, usize)>, hits: &mut Vec<(Rect, Action)>) {
    let (th, s) = (c.th, c.scale);
    let sq = px(HEADER, s);
    let (tw, track, pad) = (tab_width(c.w, sq), titlef.track, px(8.0, s));
    for (k, t) in tabs.iter().enumerate() {
        let r = Rect { x: 2 * sq + k as i32 * tw, y: 0, w: tw, h: sq };
        let chev = Rect { w: sq.min(tw), ..r };
        let (fr, fc, menu) = (c.fade(r), c.fade(chev), open == Some(t.id));
        if fr > 0.0 {
            cv.rect(r, mix(th.ui_bg, th.ui_fg, 0.06 * fr));
        }
        if menu {
            cv.rect(chev, th.ui_fg);
        } else if fc > 0.0 {
            cv.rect(chev, mix(th.ui_bg, th.ui_fg, 0.10 * fc));
        }
        icon(cv, icons, Icon::Chevron, chev, if menu { th.ui_bg } else { mix(th.sub, th.ui_fg, fc) });
        let (x, y) = (r.x + chev.w + pad / 2, (sq - titlef.ch) / 2);
        let color = if t.active { th.ui_fg } else { mix(th.sub, th.ui_fg, fr) };
        match editing.filter(|e| e.0 == t.id) {
            Some((_, text, caret)) => {
                let end = cv.text_tracked(titlef, text, x, y, th.ui_fg, track);
                let caret_x = x + titlef.width_tracked(&text[..caret.min(text.len())], track).min(end);
                cv.rect(Rect { x: caret_x, y, w: px(1.0, s), h: titlef.ch }, th.ui_fg);
            }
            None => {
                let dot = px(6.0, s);
                let room = r.x + tw - 1 - x - pad - if t.attention { dot + pad } else { 0 };
                let label = fit(titlef, t.name, room, track);
                let end = x + cv.text_tracked(titlef, &label, x, y, color, track);
                if t.attention {
                    cv.rect(Rect { x: end + pad, y: (sq - dot) / 2, w: dot, h: dot }, th.accent); // Claude is waiting in here
                }
            }
        }
        if t.active {
            cv.rect(Rect { x: r.x, y: sq - 2, w: tw - 1, h: 2 }, th.accent);
        }
        cv.rect(Rect { x: r.x + tw - 1, y: 0, w: 1, h: sq }, th.border);
        hits.push((chev, Action::SpaceMenu(t.id))); // before the tab itself, so it wins the click
        hits.push((r, Action::Space(t.id)));
    }
}

const SPACE_MENU_ROWS: usize = 3;

/// Save, Load (opens the list of saved spaces beside it) and Close, in row order.
fn space_menu_actions(id: usize) -> [Action; SPACE_MENU_ROWS] {
    [Action::SaveSpace(id), Action::LoadMenu(id), Action::CloseSpace(id)]
}

/// A tab's dropdown, hanging below its chevron (`anchor`). Drawn over the panels; its hit regions go first.
#[allow(clippy::too_many_arguments)]
fn draw_space_menu(cv: &mut Canvas, ui: &mut Font, c: &Chrome, id: usize, anchor: Rect, sel: Option<usize>, count: usize, hits: &mut Vec<(Rect, Action)>) {
    let (th, s) = (c.th, c.scale);
    let labels = ["Save", "Load", if count > 1 { "Close both" } else { "Close" }];
    let (item_h, pad, inset) = (px(32.0, s), px(4.0, s), px(12.0, s));
    let w = px(200.0, s);
    let menu = Rect { x: anchor.x.min(c.w - w - 1).max(1), y: px(HEADER, s), w, h: 2 + 2 * pad + SPACE_MENU_ROWS as i32 * item_h };
    cv.card(menu, RADIUS, th.border, th.ui_bg);
    let hover_bg = blend(th.ui_bg, th.ui_fg, 26);
    let mut menu_hits = Vec::new();
    for (i, (label, act)) in labels.iter().zip(space_menu_actions(id)).enumerate() {
        let row = Rect { x: menu.x + 1 + pad, y: menu.y + 1 + pad + i as i32 * item_h, w: menu.w - 2 - 2 * pad, h: item_h };
        if sel == Some(i) {
            cv.rect(row, hover_bg);
        }
        let ty = row.y + (row.h - ui.ch) / 2;
        cv.text(ui, label, row.x + inset, ty, th.ui_fg);
        if matches!(act, Action::LoadMenu(_)) {
            let tx = row.x + row.w - inset - ui.width("›");
            cv.text(ui, "›", tx, ty, th.sub); // a submenu opens from this row
        }
        menu_hits.push((row, act));
    }
    menu_hits.push((menu, Action::Nop));
    drop(hits.splice(0..0, menu_hits));
}

/// Edits the text box `f` with one key: typing at the caret, Left/Right/Home/End/Delete/Backspace.
fn edit_key(f: &mut Picker, key: &Key, ctrl: bool, alt: bool) {
    match key {
        Key::Named(NamedKey::ArrowLeft) => f.move_caret(-1),
        Key::Named(NamedKey::ArrowRight) => f.move_caret(1),
        Key::Named(NamedKey::Home) => f.caret = 0,
        Key::Named(NamedKey::End) => f.caret = f.query.len(),
        Key::Named(NamedKey::Delete) => f.delete(),
        Key::Named(NamedKey::Backspace) => f.pop(),
        Key::Named(NamedKey::Space) => f.push(" "),
        Key::Character(c) if !ctrl && !alt => f.push(c),
        _ => {}
    }
}

/// Theme / view dropdown. Drawn last so it sits on top; its hit regions go first so they win the click.
/// Structure follows the usual menu spec: group labels, a reserved check column so item text aligns,
/// a 1px separator between groups, hover/keyboard highlight. Text rows show their current choice and a "›";
/// `sub` is the open Text submenu (and its highlighted row), drawn beside its row. `text`: see TEXT_DEFAULT.
#[allow(clippy::too_many_arguments)]
fn draw_menu(cv: &mut Canvas, ui: &mut Font, icons: &mut Font, c: &Chrome, text: [usize; 6], sub: Option<(usize, Option<usize>)>, hits: &mut Vec<(Rect, Action)>) {
    let (th, s) = (c.th, c.scale);
    let hover_bg = blend(th.ui_bg, th.ui_fg, 26);
    let items: Vec<(&str, bool)> = MENU_ACTIONS
        .iter()
        .map(|a| match *a {
            Action::SetTheme(i) => (THEME_NAMES[i], c.theme == i),
            Action::SetView(v) => (["Space", "Grid", "Carousel"][v as usize], c.view == v),
            Action::SubMenu(k) => (TEXT_ROWS[k], false),
            _ => ("Install hooks", c.hooks), // checked once every Claude account runs them
        })
        .collect();
    let values: Vec<String> = (0..TEXT_ROWS.len()).map(|k| text_choices(k).swap_remove(text[k])).collect();
    let (item_h, label_h, pad, col) = (px(32.0, s), px(28.0, s), px(4.0, s), px(32.0, s)); // col: the check column
    let (sep, arrow) = (2 * px(4.0, s) + 1, ui.width("›"));
    let widest = items.iter().zip(MENU_ACTIONS).map(|(it, a)| ui.width(it.0) + if let Action::SubMenu(k) = a { px(24.0, s) + ui.width(&values[k]) + arrow } else { 0 }).max().unwrap_or(0);
    let menu_w = (col + widest + px(24.0, s) + 2 * pad + 2).max(px(200.0, s));
    let menu = Rect { x: 0, y: px(HEADER, s), w: menu_w, h: 2 * pad + 2 + 4 * label_h + items.len() as i32 * item_h + 3 * sep };
    cv.card(menu, RADIUS, th.border, th.ui_bg);
    let mut y = menu.y + 1 + pad;
    let mut menu_hits = Vec::new();
    let mut anchor = None; // the row the open submenu hangs from
    let mut start = 0;
    for (gi, (name, n)) in MENU_GROUPS.into_iter().enumerate() {
        let range = start..start + n;
        start += n;
        if gi > 0 {
            cv.rect(Rect { x: menu.x + 1, y: y + px(4.0, s), w: menu.w - 2, h: 1 }, th.border);
            y += sep;
        }
        cv.text(ui, name, menu.x + 1 + pad + px(12.0, s), y + (label_h - ui.ch) / 2, th.sub);
        y += label_h;
        for idx in range {
            let (label, on) = items[idx];
            let row = Rect { x: menu.x + 1 + pad, y, w: menu.w - 2 - 2 * pad, h: item_h };
            if c.menu_sel == Some(idx) {
                cv.rect(row, hover_bg);
            }
            if on {
                icon(cv, icons, Icon::Check, Rect { w: col, ..row }, th.ui_fg);
            }
            let ty = row.y + (row.h - ui.ch) / 2;
            cv.text(ui, label, row.x + col, ty, th.ui_fg);
            if let Action::SubMenu(k) = MENU_ACTIONS[idx] {
                let ax = row.x + row.w - px(12.0, s) - arrow;
                cv.text(ui, "›", ax, ty, th.sub);
                let vw = ui.width(&values[k]);
                cv.text(ui, &values[k], ax - px(8.0, s) - vw, ty, th.sub);
                if sub.is_some_and(|(open, _)| open == k) {
                    anchor = Some(row);
                }
            }
            menu_hits.push((row, MENU_ACTIONS[idx]));
            y += item_h;
        }
    }
    let mut backs = vec![(menu, Action::Nop)];
    if let (Some((k, sel)), Some(row)) = (sub, anchor) {
        // the submenu: its rows come right after the dropdown's, so `on_cursor` can tell them apart
        let choices = text_choices(k);
        let w = (col + choices.iter().map(|c| ui.width(c)).max().unwrap_or(0) + px(24.0, s) + 2 * pad + 2).max(px(160.0, s));
        let r = Rect { x: menu.x + menu.w - 1, y: row.y - pad - 1, w, h: 2 * pad + 2 + choices.len() as i32 * item_h };
        cv.card(r, RADIUS, th.border, th.ui_bg);
        for (i, label) in choices.iter().enumerate() {
            let row = Rect { x: r.x + 1 + pad, y: r.y + 1 + pad + i as i32 * item_h, w: r.w - 2 - 2 * pad, h: item_h };
            if sel == Some(i) {
                cv.rect(row, hover_bg);
            }
            if text[k] == i {
                icon(cv, icons, Icon::Check, Rect { w: col, ..row }, th.ui_fg);
            }
            cv.text(ui, label, row.x + col, row.y + (row.h - ui.ch) / 2, th.ui_fg);
            menu_hits.push((row, Action::SetText(k, i)));
        }
        backs.push((r, Action::Nop));
    }
    menu_hits.extend(backs);
    drop(hits.splice(0..0, menu_hits));
}

const PICK_INSET: f32 = 12.0; // picker text, from the left edge of its row (design px)

/// The caret position (byte offset into `s`) nearest to `x` pixels from the start of the text.
fn caret_at(font: &mut Font, s: &str, x: i32) -> usize {
    s.char_indices()
        .map(|(i, _)| i)
        .chain([s.len()])
        .min_by_key(|&i| (font.width(&s[..i]) - x).abs())
        .unwrap_or(0)
}

/// Folder / drive picker, hanging below `anchor` (the header chip it opened from). Folders get a search box and the
/// folder being browsed above the list; drives are just the list. Drawn last; its hit regions go first, like the dropdown's.
#[allow(clippy::too_many_arguments)]
fn draw_picker(cv: &mut Canvas, ui: &mut Font, icons: &mut Font, th: &Theme, s: f32, pk: &Picker, anchor: Rect, win: (i32, i32), hits: &mut Vec<(Rect, Action)>) {
    let (dirs, spaces) = (pk.kind == Kind::Dir, pk.kind == Kind::Space);
    let search = dirs || spaces; // drives are just the list
    let (item_h, path_h, pad, inset, col) = (px(32.0, s), px(24.0, s), px(4.0, s), px(PICK_INSET, s), px(32.0, s));
    let mut picker_hits = Vec::new();
    let rows = pk.shown();
    let first = pk.top.min(rows.len());
    let list = &rows[first..(first + picker::VISIBLE).min(rows.len())];
    let head = if dirs { item_h + path_h + 2 } else if spaces { item_h + 1 } else { 0 }; // search box, folder line, hairlines
    let w = px(if dirs { 320.0 } else if spaces { 240.0 } else { 160.0 }, s).min(win.0 - 2);
    let h = (2 + 2 * pad + head + list.len().max(1) as i32 * item_h).min(win.1 - 2);
    // below the chip it opened from; a tab menu's Load list opens beside its row instead
    let (px0, py0) = if spaces { (anchor.x + anchor.w + pad + 1, anchor.y - pad - 1) } else { (anchor.x, anchor.y + anchor.h + px(4.0, s)) };
    let r = Rect { x: px0.min(win.0 - w - 1).max(1), y: py0.min(win.1 - h - 1).max(1), w, h };
    cv.card(r, RADIUS, mix(th.border, th.ui_fg, 0.18), th.ui_bg); // outline a notch stronger than the hairlines, to lift it off the terminal
    let (left, inner_w) = (r.x + 1 + pad, r.w - 2 - 2 * pad);
    let mut y = r.y + 1 + pad;
    if search {
        let ty = y + (item_h - ui.ch) / 2;
        if pk.query.is_empty() {
            cv.text(ui, if dirs { "Search folders" } else { "Search spaces" }, left + inset, ty, th.sub);
        }
        cv.text(ui, &pk.query, left + inset, ty, th.ui_fg);
        let caret_x = left + inset + ui.width(&pk.query[..pk.caret]);
        cv.rect(Rect { x: caret_x, y: ty, w: px(1.0, s), h: ui.ch }, th.ui_fg);
        picker_hits.push((Rect { x: left, y, w: inner_w, h: item_h }, Action::Search));
        y += item_h;
        cv.rect(Rect { x: r.x + 1, y, w: r.w - 2, h: 1 }, th.border);
        if spaces {
            y += 1;
        }
    }
    if dirs {
        let mut path = pk.base.display().to_string();
        if ui.width(&path) > inner_w - 2 * inset {
            while !path.is_empty() && ui.width(&format!("…{path}")) > inner_w - 2 * inset {
                path.remove(0); // keep the end: that is the part that tells folders apart
            }
            path.insert(0, '…');
        }
        cv.text(ui, &path, left + inset, y + 1 + (path_h - ui.ch) / 2, th.sub);
        y += path_h + 1;
        cv.rect(Rect { x: r.x + 1, y, w: r.w - 2, h: 1 }, th.border);
        y += 1;
    }
    let hover_bg = blend(th.ui_bg, th.ui_fg, 26);
    let list_y = y;
    for (n, &name) in list.iter().enumerate() {
        let (k, row) = (first + n, Rect { x: left, y, w: inner_w, h: item_h });
        if k == pk.sel {
            cv.rect(row, hover_bg);
        }
        let x = row.x + if search { inset } else { col }; // drives keep a check column, like the dropdown
        if !search && name == pk.current {
            icon(cv, icons, Icon::Check, Rect { w: col, ..row }, th.ui_fg);
        }
        let label = match name {
            "." if dirs => ". (this folder)",
            ".." if dirs => ".. (up)",
            _ => name,
        };
        let label = fit(ui, label, row.x + row.w - x - inset, 0.0);
        cv.text(ui, &label, x, row.y + (item_h - ui.ch) / 2, if dirs && name.starts_with('.') { th.sub } else { th.ui_fg });
        picker_hits.push((row, Action::Pick(k)));
        y += item_h;
    }
    if list.is_empty() {
        cv.text(ui, if spaces { "No saved spaces" } else { "No folders" }, left + inset, y + (item_h - ui.ch) / 2, th.sub);
    }
    if rows.len() > picker::VISIBLE {
        // scrollbar thumb, same look as the terminal's
        let (track, thick) = (y - list_y, px(3.0, s));
        let thumb = (track * picker::VISIBLE as i32 / rows.len() as i32).max(px(14.0, s));
        let thumb_y = list_y + (track - thumb) * pk.top as i32 / (rows.len() - picker::VISIBLE) as i32;
        cv.rrect(Rect { x: r.x + r.w - 2 - pad - thick, y: thumb_y, w: thick, h: thumb }, thick / 2, th.sub, 120);
    }
    picker_hits.push((r, Action::Nop));
    drop(hits.splice(0..0, picker_hits));
}

/// Frames per second of the prompt mark: slow enough to cost next to nothing while you sit at a prompt.
const MARK_FPS: f32 = 20.0;

/// The prompt mark: square sprites, 2 wide and 4 high, forming a vertical rectangle centred in `cell`, shaded in a
/// gradient from `color` at the top to half-way into `bg` at the bottom. `t` > 0 (seconds): a light wave rolls up
/// through them, once every 1.6 s; 0 is the still gradient.
fn draw_prompt_mark(cv: &mut Canvas, cell: Rect, t: f32, color: u32, bg: u32) {
    const COLS: i32 = 2;
    const ROWS: i32 = 4;
    let gap = (cell.h / 18).max(1);
    let s = ((cell.h * 17 / 20 - (ROWS - 1) * gap) / ROWS).max(1); // sprite side, whole pixels: crisp edges
    let (w, h) = (COLS * s + (COLS - 1) * gap, ROWS * s + (ROWS - 1) * gap);
    let (x0, y0) = (cell.x + (cell.w - w) / 2, cell.y + (cell.h - h) / 2);
    for row in 0..ROWS {
        for col in 0..COLS {
            let down = row as f32 / (ROWS - 1) as f32; // 0 at the top
            let mut c = mix(color, bg, 0.5 * down);
            if t > 0.0 {
                // the wave: rows light up bottom to top, the right column a beat behind the left
                let phase = t / 1.6 * std::f32::consts::TAU + row as f32 * 0.9 - col as f32 * 0.5;
                c = mix(c, bg, 0.55 * (0.5 + 0.5 * phase.sin()));
            }
            cv.rect(Rect { x: x0 + col * (s + gap), y: y0 + row * (s + gap), w: s, h: s }, c);
        }
    }
}

/// The scrollback search box, in the terminal's top-right corner: the query and "3 of 12", styled like the picker.
fn draw_search(cv: &mut Canvas, ui: &mut Font, th: &Theme, s: f32, search: &Search, inner: Rect) {
    let (item_h, inset) = (px(32.0, s), px(PICK_INSET, s));
    let w = px(320.0, s).min(inner.w);
    let r = Rect { x: inner.x + inner.w - w, y: inner.y, w, h: item_h + 2 };
    cv.card(r, RADIUS, mix(th.border, th.ui_fg, 0.18), th.ui_bg);
    let (f, ty) = (&search.field, r.y + 1 + (item_h - ui.ch) / 2);
    let count = match (f.query.is_empty(), search.hits.len()) {
        (true, _) => String::new(),
        (false, 0) => "No matches".into(),
        (false, n) => format!("{} of {n}", search.cur + 1),
    };
    let cw = ui.width(&count);
    cv.text(ui, &count, r.x + r.w - 1 - inset - cw, ty, th.sub);
    let (old, x) = (cv.clip(), r.x + 1 + inset);
    cv.set_clip(Rect { x, y: r.y, w: r.w - 2 - 3 * inset - cw, h: r.h }.intersect(&old)); // a long query never runs into the count
    if f.query.is_empty() {
        cv.text(ui, "Search", x, ty, th.sub);
    }
    cv.text(ui, &f.query, x, ty, th.ui_fg);
    cv.rect(Rect { x: x + ui.width(&f.query[..f.caret]), y: ty, w: px(1.0, s), h: ui.ch }, th.ui_fg);
    cv.set_clip(old);
}

/// The icons we use, all from Lucide (https://lucide.dev, ISC license): one font glyph each.
#[derive(Clone, Copy)]
enum Icon {
    Chevron,
    Plus,
    Close,
    Minimize,
    Maximize,
    Restore,
    Check,
}

impl Icon {
    fn glyph(self) -> char {
        match self {
            Icon::Chevron => '\u{e06d}', // chevron-down
            Icon::Plus => '\u{e13d}',    // plus
            Icon::Close => '\u{e1b2}',   // x
            Icon::Minimize => '\u{e11c}', // minus
            Icon::Maximize => '\u{e167}', // square
            Icon::Restore => '\u{e09e}',  // copy (two overlapping squares)
            Icon::Check => '\u{e06c}',   // check
        }
    }
}

/// Draws `kind` centred in `r`. `icons` is sized to ICON, so every icon has the same size.
fn icon(cv: &mut Canvas, icons: &mut Font, kind: Icon, r: Rect, color: u32) {
    cv.glyph_centered(icons, kind.glyph(), r.x + r.w / 2, r.y + r.h / 2, color);
}

fn draw_suggest(cv: &mut Canvas, font: &Font, ui: &mut Font, s: &Suggest, th: &Theme, p: &Pane, inner: Rect) {
    let longest = s.items.iter().map(|n| ui.width(n)).max().unwrap_or(0);
    let row_h = ui.ch + 8;
    let (w, h) = (longest + 32, s.items.len() as i32 * row_h + 8);
    let x = (inner.x + s.col as i32 * font.cw).min(inner.x + inner.w - w).max(inner.x);
    let below = inner.y + (p.term.cy as i32 + 1) * font.ch + 2;
    let y = if below + h <= inner.y + inner.h { below } else { inner.y + p.term.cy as i32 * font.ch - h - 2 };
    let r = Rect { x, y, w, h };
    cv.card(r, RADIUS, th.border, th.ui_bg);
    for (i, name) in s.items.iter().enumerate() {
        let row = Rect { x: x + 4, y: y + 4 + i as i32 * row_h, w: w - 8, h: row_h };
        if i == s.sel {
            cv.rect(row, th.accent);
        }
        cv.text(ui, name, row.x + 12, row.y + (row.h - ui.ch) / 2, if i == s.sel { th.on_accent } else { th.ui_fg });
    }
}

struct App {
    stats: bool,
    last_stats: Instant,
    proxy: EventLoopProxy<Ev>,
    cfg: Option<Config>,
    ui: Option<Ui>,
}

impl ApplicationHandler<Ev> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if let (None, Some(cfg)) = (&self.ui, self.cfg.take()) {
            self.ui = Some(Ui::new(el, self.proxy.clone(), cfg));
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        el.set_control_flow(ControlFlow::Wait);
        if self.stats && self.last_stats.elapsed() >= Duration::from_secs(1) {
            self.last_stats = Instant::now();
            let animating = self.ui.as_ref().is_some_and(|u| u.animating);
            eprintln!("stats: {} draws, {} output chunks, animating={animating}", DRAWS.swap(0, Ordering::Relaxed), OUTPUTS.swap(0, Ordering::Relaxed));
        }
        let Some(ui) = &mut self.ui else { return };
        if ui.animating {
            // Pace animation frames with a precise sleep: `WaitUntil` is rounded to the ~15.6 ms Windows timer
            // tick, which makes frame intervals uneven and the animation look stuttery.
            let now = Instant::now();
            if now < ui.next_frame {
                std::thread::sleep(ui.next_frame - now);
            }
            let after = Instant::now();
            ui.next_frame = if after > ui.next_frame + FRAME { after + FRAME } else { ui.next_frame + FRAME };
            ui.partial_ok = true;
            ui.window.request_redraw();
        } else if let Some(r) = ui.mark {
            // the prompt mark turns at MARK_FPS, repainting only its own cell, and only while it is on screen
            let now = Instant::now();
            if now >= ui.next_mark {
                ui.next_mark = now + Duration::from_secs_f32(1.0 / MARK_FPS);
                ui.invalidate_rect(r);
            }
            el.set_control_flow(ControlFlow::WaitUntil(ui.next_mark));
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: Ev) {
        let Some(ui) = &mut self.ui else { return };
        match ev {
            Ev::Output(id, bytes) => ui.on_output(id, &bytes),
            Ev::Exit(id) => {
                if let Some(i) = ui.panes.iter().position(|p| p.id == id) {
                    ui.begin_close(i);
                }
            }
            Ev::Hook(h) => ui.on_hook(h),
        }
        if ui.quit {
            ui.finish_memory();
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        let Some(ui) = &mut self.ui else { return };
        match ev {
            WindowEvent::CloseRequested => ui.quit = true,
            WindowEvent::Focused(f) => {
                ui.focused = f;
                if f {
                    ui.panes[ui.focus].attention = false;
                }
                ui.invalidate();
            }
            WindowEvent::Resized(PhysicalSize { width, height }) if width > 0 && height > 0 => ui.relayout(),
            WindowEvent::RedrawRequested => ui.draw(),
            WindowEvent::ModifiersChanged(m) => ui.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => ui.on_key(&event),
            WindowEvent::CursorMoved { position, .. } => ui.on_cursor(position.x as i32, position.y as i32),
            WindowEvent::MouseInput { state, button, .. } => ui.on_mouse_button(button, state),
            WindowEvent::MouseWheel { delta, .. } => ui.on_wheel(delta),
            _ => {}
        }
        if ui.quit {
            ui.finish_memory();
            el.exit();
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("hook") {
        return claude::hook_main(); // run by Claude Code, see claude.rs
    }
    let el = EventLoop::<Ev>::with_user_event().build().unwrap();
    let mut app = App { stats: std::env::var_os("MAULA_STATS").is_some(), last_stats: Instant::now(), proxy: el.create_proxy(), cfg: Some(config::load()), ui: None };
    el.run_app(&mut app).unwrap();
}

/// Dev tool, not part of the app: renders the real header + dropdown (the same functions the app calls) in both
/// themes to a PNG, so they can be inspected without a window.
/// `MAULA_PREVIEW=out.png MAULA_SCALE=1.5 cargo test header_full -- --ignored`
#[cfg(test)]
mod header_preview {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    fn write_png(path: &str, w: usize, h: usize, px: &[u32]) {
        std::fs::write(path, png(w, h, px)).unwrap();
    }

    /// Minimal PNG encoder (stored deflate blocks), opaque RGBA: enough for screenshots and icon images.
    fn png(w: usize, h: usize, px: &[u32]) -> Vec<u8> {
        let mut raw = Vec::new();
        for y in 0..h {
            raw.push(0u8);
            for x in 0..w {
                let p = px[y * w + x];
                raw.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]);
            }
        }
        let mut z = vec![0x78, 0x01];
        let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, c) in chunks.iter().enumerate() {
            z.push((i + 1 == chunks.len()) as u8);
            z.extend((c.len() as u16).to_le_bytes());
            z.extend((!(c.len() as u16)).to_le_bytes());
            z.extend(*c);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &v in &raw {
            a = (a + v as u32) % 65521;
            b = (b + a) % 65521;
        }
        z.extend(((b << 16) | a).to_be_bytes());
        let mut out = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10];
        let mut chunk = |kind: &[u8], data: &[u8]| {
            out.extend((data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend(data);
            out.extend(&body);
            out.extend(crc32(&body).to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend((w as u32).to_be_bytes());
        ihdr.extend((h as u32).to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]); // 8-bit RGBA
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &z);
        chunk(b"IEND", &[]);
        out
    }

    /// The app icon at `n` x `n` px: the prompt mark's sprite column on a dark square tile (dark theme colours).
    fn icon_pixels(n: i32) -> Vec<u32> {
        let th = Theme::dark();
        let mut buf = vec![th.head_bg; (n * n) as usize];
        let mut cv = Canvas::new(&mut buf, n, n);
        let side = n * 7 / 8; // bold: at 16 px each sprite is still 2 px
        let cell = Rect { x: (n - side) / 2, y: (n - side) / 2, w: side, h: side };
        draw_prompt_mark(&mut cv, cell, 0.0, th.accent, th.head_bg);
        buf
    }

    /// Writes assets/maula.ico (every size drawn natively, PNG entries) and a preview of them all:
    /// `cargo test app_icon -- --ignored`. The exe embeds the .ico through build.rs.
    #[test]
    #[ignore]
    fn app_icon() {
        let sizes = [16, 20, 24, 32, 40, 48, 64, 256];
        let images: Vec<Vec<u8>> = sizes.iter().map(|&n| png(n as usize, n as usize, &icon_pixels(n))).collect();
        let mut ico = vec![0, 0, 1, 0];
        ico.extend((sizes.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * sizes.len();
        for (&n, img) in sizes.iter().zip(&images) {
            ico.extend([(n % 256) as u8, (n % 256) as u8, 0, 0]); // 256 is written as 0
            ico.extend(1u16.to_le_bytes()); // planes
            ico.extend(32u16.to_le_bytes()); // bits per pixel
            ico.extend((img.len() as u32).to_le_bytes());
            ico.extend((offset as u32).to_le_bytes());
            offset += img.len();
        }
        images.iter().for_each(|img| ico.extend(img));
        std::fs::write("assets/maula.ico", ico).unwrap();

        // every size side by side, for a look
        let (w, h) = (sizes.iter().map(|n| n + 8).sum::<i32>(), 256);
        let mut all = vec![0x808080u32; (w * h) as usize];
        let mut x = 0;
        for n in sizes {
            for (i, p) in icon_pixels(n).into_iter().enumerate() {
                all[(i as i32 / n * w + x + i as i32 % n) as usize] = p;
            }
            x += n + 8;
        }
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "icon.png".into());
        write_png(&path, w as usize, h as usize, &all);
    }

    fn render(th: Theme, theme: usize, menu: bool, sel: Option<usize>, mouse: (i32, i32), scale: f32) -> (i32, i32, Vec<u32>) {
        let (mut ui, mut icons) = (Font::new(UI_PX * scale), Font::icons(ICON * scale));
        let hh = px(HEADER, scale);
        let (w, h) = ((1100.0 * scale) as i32, hh + px(if menu { 600.0 } else { 24.0 }, scale));
        let mut buf = vec![th.bg; (w * h) as usize]; // below the header: the terminal surface, to show the active tab joining it
        let none = HashMap::new();
        let mut chrome = Chrome { th, scale, w, fades: &none, view: View::Grid, theme, menu_open: menu, menu_sel: sel, maximized: false, can_add: true, hooks: false };
        let mut cv = Canvas::new(&mut buf, w, h);
        let mut hits = Vec::new();
        draw_header(&mut cv, &mut icons, &chrome, &mut hits); // pass 1: find what is under the mouse
        let fades: HashMap<RectKey, Tween> = hits.iter().filter(|(r, _)| r.contains(mouse.0, mouse.1)).map(|(r, _)| (rect_key(*r), Tween::at(1.0))).collect();
        chrome.fades = &fades;
        hits.clear();
        draw_header(&mut cv, &mut icons, &chrome, &mut hits);
        if menu {
            draw_menu(&mut cv, &mut ui, &mut icons, &chrome, TEXT_DEFAULT, Some((4, Some(2))), &mut hits); // the Accent submenu open
        }
        (w, h, buf)
    }

    #[test]
    #[ignore]
    fn header_full() {
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "header.png".into());
        let scale: f32 = std::env::var("MAULA_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.5);
        let (mut all, mut total_h, mut width) = (Vec::new(), 0, 0);
        for (theme, th) in [Theme::light(), Theme::dark(), Theme::friendly()].into_iter().enumerate() {
            // closed, a tab hovered, dropdown open with a row highlighted
            for (menu, sel, mouse) in [(false, None, (-1, -1)), (false, None, (px(230.0, scale), 6)), (true, Some(3), (40, 150))] {
                let (w, h, b) = render(th, theme, menu, sel, mouse, scale);
                all.extend(b);
                all.extend(vec![0x808080; (w * 6) as usize]); // divider between states
                (total_h, width) = (total_h + h + 6, w);
            }
        }
        write_png(&path, width as usize, total_h as usize, &all);
    }

    /// Renders the header with space tabs: plain, one being renamed, and a tab menu with its Load list open:
    /// `MAULA_PREVIEW=out.png MAULA_SCALE=1.5 cargo test space_tabs_full -- --ignored`
    #[test]
    #[ignore]
    fn space_tabs_full() {
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "space_tabs.png".into());
        let scale: f32 = std::env::var("MAULA_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.5);
        let (mut all, mut total_h, mut width) = (Vec::new(), 0, 0);
        for (theme, th) in [Theme::light(), Theme::dark(), Theme::friendly()].into_iter().enumerate() {
            let (mut ui, mut titlef, mut icons) = (Font::new(UI_PX * scale), Font::medium(TITLE_PX * scale), Font::icons(ICON * scale));
            let none = HashMap::new();
            let names = ["Space 1", "api work with a long name", "Space 3"];
            let tabs: Vec<Tab> = names.iter().enumerate().map(|(id, name)| Tab { id, name, active: id == 1, attention: id == 2 }).collect();
            for state in 0..3 {
                let (w, h) = ((1100.0 * scale) as i32, px(HEADER, scale) + px(210.0, scale));
                let mut buf = vec![th.bg; (w * h) as usize];
                let chrome = Chrome { th, scale, w, fades: &none, view: View::Space, theme, menu_open: false, menu_sel: None, maximized: false, can_add: true, hooks: false };
                let mut cv = Canvas::new(&mut buf, w, h);
                let mut hits = Vec::new();
                draw_header(&mut cv, &mut icons, &chrome, &mut hits);
                let editing = (state == 1).then_some((1, "api work", 3));
                draw_tabs(&mut cv, &mut titlef, &mut icons, &chrome, &tabs, (state == 2).then_some(1), editing, &mut hits);
                if state == 2 {
                    let chev = hits.iter().find(|h| matches!(h.1, Action::SpaceMenu(1))).unwrap().0;
                    draw_space_menu(&mut cv, &mut ui, &chrome, 1, chev, Some(1), 2, &mut hits);
                    let load = hits.iter().find(|h| matches!(h.1, Action::LoadMenu(1))).unwrap().0;
                    let pk = Picker::saved(1, ["api".to_string(), "web".to_string()]);
                    draw_picker(&mut cv, &mut ui, &mut icons, &th, scale, &pk, load, (w, h), &mut hits);
                }
                all.extend(buf);
                all.extend(vec![0x808080; (w * 6) as usize]);
                (total_h, width) = (total_h + h + 6, w);
            }
        }
        write_png(&path, width as usize, total_h as usize, &all);
    }

    /// Renders the top header and three panels (active, close hovered, half faded in), both themes:
    /// `MAULA_PREVIEW=out.png MAULA_SCALE=1.0 cargo test panels_full -- --ignored`
    #[test]
    #[ignore]
    fn panels_full() {
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "panels.png".into());
        let scale: f32 = std::env::var("MAULA_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let (mut all, mut total_h, mut width) = (Vec::new(), 0, 0);
        for (theme, th) in [Theme::light(), Theme::dark(), Theme::friendly()].into_iter().enumerate() {
            let (mut titlef, mut icons, mut font, mut closef) = (Font::medium(TITLE_PX * scale), Font::icons(ICON * scale), Font::new(FONT_PX * scale), Font::medium(CLOSE_PX * scale));
            let hh = px(HEADER, scale);
            let (pw, ph) = ((380.0 * scale) as i32, (230.0 * scale) as i32);
            let (w, h) = (pw * 3, hh + ph);
            let mut buf = vec![th.bg; (w * h) as usize];
            let mut cv = Canvas::new(&mut buf, w, h);
            let none = HashMap::new();
            let chrome = Chrome { th, scale, w, fades: &none, view: View::Grid, theme, menu_open: false, menu_sel: None, maximized: false, can_add: true, hooks: false };
            draw_header(&mut cv, &mut icons, &chrome, &mut Vec::new());
            let names = ["maula", "api-server", "new terminal"];
            let drives = [Some('D'), Some('C'), None];
            for k in 0..3 {
                let outer = Rect { x: k * pw, y: hh, w: pw, h: ph };
                let inner = pane_inner(outer, scale);
                cv.set_clip(outer);
                let hr = Rect { x: outer.x, y: outer.y, w: outer.w - 1, h: hh };
                let name = names[k as usize];
                let hit = draw_panel_header(&mut cv, &mut titlef, &mut closef, &mut icons, &th, scale, hr, k as usize, name, drives[k as usize], k == 0, false, false, &HashMap::new());
                let fades: HashMap<RectKey, Tween> = if k == 1 { [(rect_key(hit.close), Tween::at(1.0)), (rect_key(hit.dir), Tween::at(1.0))].into() } else { HashMap::new() };
                draw_panel_header(&mut cv, &mut titlef, &mut closef, &mut icons, &th, scale, hr, k as usize, name, drives[k as usize], k == 0, false, false, &fades); // again, with the hover applied
                cv.rect(Rect { x: outer.x + outer.w - 1, y: outer.y, w: 1, h: outer.h }, th.border);
                cv.rect(Rect { x: outer.x, y: outer.y + outer.h - 1, w: outer.w, h: 1 }, th.border);
                if k == 0 {
                    cv.rect(Rect { x: outer.x, y: outer.y + outer.h - 2, w: outer.w - 1, h: 2 }, th.accent);
                }
                let mut t = Term::new((inner.w / font.cw) as usize, (inner.h / font.ch) as usize);
                t.feed(b"PS D:\\work\\maula> cargo run\r\n   Compiling maula v0.1.0\r\n    Finished dev in 1.2s\r\nPS D:\\work\\maula> ");
                cv.set_clip(inner);
                draw_term(&mut cv, &mut font, &t, 0.0, None, &th, inner, k == 0, if k == 0 { 0.0 } else { 1.0 }); // card view: inactive greyed
                if k == 2 {
                    cv.set_clip(outer);
                    cv.rrect(outer, 0, th.bg, 128); // halfway through fading in
                }
            }
            all.extend(buf);
            all.extend(vec![0x808080; (w * 6) as usize]);
            (total_h, width) = (total_h + h + 6, w);
        }
        write_png(&path, width as usize, total_h as usize, &all);
    }

    /// Renders a panel header with its folder picker (open on the parent of the working directory, typed
    /// into) and another with the drive picker, in every theme: `MAULA_PREVIEW=out.png cargo test picker_full -- --ignored`
    #[test]
    #[ignore]
    fn picker_full() {
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "picker.png".into());
        let scale: f32 = std::env::var("MAULA_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let cwd = std::env::var_os("MAULA_DIR").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap().join("src")); // MAULA_DIR: a folder with many sub-folders shows the scrollbar
        let (mut all, mut total_h, mut width) = (Vec::new(), 0, 0);
        for th in [Theme::light(), Theme::dark(), Theme::friendly()] {
            let (mut titlef, mut icons, mut closef, mut ui) = (Font::medium(TITLE_PX * scale), Font::icons(ICON * scale), Font::medium(CLOSE_PX * scale), Font::new(UI_PX * scale));
            let hh = px(HEADER, scale);
            let (pw, h) = (px(420.0, scale), hh + px(400.0, scale));
            let w = pw * 2;
            let mut buf = vec![th.bg; (w * h) as usize];
            let mut cv = Canvas::new(&mut buf, w, h);
            let mut hits = Vec::new();
            for k in 0..2 {
                let hr = Rect { x: k * pw, y: 0, w: pw - 1, h: hh };
                let head = draw_panel_header(&mut cv, &mut titlef, &mut closef, &mut icons, &th, scale, hr, k as usize, "maula", Some('D'), k == 0, false, false, &HashMap::new());
                let (pk, anchor) = if k == 0 {
                    let mut pk = Picker::dirs(0, cwd.clone());
                    pk.sel = 1;
                    pk.scroll(4);
                    (pk, head.dir)
                } else {
                    (Picker::drives(0, Some('D')), head.drive.unwrap())
                };
                draw_picker(&mut cv, &mut ui, &mut icons, &th, scale, &pk, anchor, (w, h), &mut hits);
            }
            all.extend(buf);
            all.extend(vec![0x808080; (w * 6) as usize]);
            (total_h, width) = (total_h + h + 6, w);
        }
        write_png(&path, width as usize, total_h as usize, &all);
    }

    #[test]
    fn accent_and_maximize_come_from_the_config() {
        let c = config::parse_config("accent = \"Blue\"\nmaximize = \"window\"").unwrap();
        assert_eq!(text_of(&c)[4..], [2, 1]);
        assert_eq!(text_of(&Config::default())[4..], [0, 0], "unset: the theme's own accent, borderless fullscreen");
        let t = with_accent(Theme::dark(), 2);
        assert_eq!((t.accent, t.cursor, t.on_accent), (0x4f86d9, 0x4f86d9, 0xffffff));
        assert_eq!(with_accent(Theme::dark(), 0).accent, Theme::dark().accent);
        assert_eq!(MENU_GROUPS.iter().map(|g| g.1).sum::<usize>(), MENU_ACTIONS.len(), "every row is in a group");
    }

    #[test]
    fn a_click_lands_the_caret_between_the_nearest_characters() {
        let mut f = Font::new(UI_PX);
        let cw = f.cw;
        assert_eq!(caret_at(&mut f, "héllo", -20), 0, "left of the text: the start");
        assert_eq!(caret_at(&mut f, "héllo", cw + cw / 4), 1, "a quarter into the 2nd char: before it");
        assert_eq!(caret_at(&mut f, "héllo", cw + cw * 3 / 4), 3, "most of the way through the 2nd char: after it (é is 2 bytes)");
        assert_eq!(caret_at(&mut f, "héllo", 999), 6, "right of the text: the end");
        assert_eq!(caret_at(&mut f, "", 50), 0);
    }

    #[test]
    fn two_cards_per_view_only_when_the_window_is_wide_enough() {
        let (cw, pad) = (14, 36); // 150% scale
        let min = CARD_COLS * cw + 2 * pad + 1;
        assert_eq!(card_width(1280, cw, pad), min, "narrow window: fixed 90-column cards");
        assert_eq!(card_width(2 * min - 1, cw, pad), min, "one pixel short of two cards");
        assert_eq!(card_width(2 * min, cw, pad), min, "exactly two fit: exactly half the window");
        for w in [2 * min, 3000, 3840] {
            let cw_card = card_width(w, cw, pad);
            assert_eq!(cw_card, w / 2);
            assert!((cw_card - 1 - 2 * pad) / cw >= CARD_COLS, "never fewer than 90 columns at {w}px");
        }
    }

    #[test]
    fn carousel_is_a_ring_with_the_focused_card_in_front_and_centered() {
        let area = Rect { x: 0, y: 36, w: 1000, h: 500 };
        let (cw, n) = (600, 6); // 200px of room on each side
        let ring = |focus: i32| carousel_rects(area, cw, focus * cw, n);
        let x_of = |r: &[(usize, Rect)], i: usize| r.iter().find(|s| s.0 == i).unwrap().1.x;
        let at = |r: &[(usize, Rect)], i: usize| r.iter().position(|s| s.0 == i).unwrap();

        let r = ring(2);
        assert_eq!((r.last().unwrap().0, x_of(&r, 2)), (2, 200), "the focused card is drawn last, centered");
        assert!(x_of(&r, 1) < 200 && x_of(&r, 3) > 200, "its neighbours stand on either side");
        assert_eq!(x_of(&r, 1) + x_of(&r, 3), 400, "symmetric about the center");
        assert!(at(&r, 4) < at(&r, 3), "the far side of the ring is behind the near side");
        assert_eq!((x_of(&r, 5), at(&r, 5)), (200, 0), "even count: the opposite card is directly behind, drawn first");

        // the ring wraps: from the last card, the first is its right-hand neighbour
        let r = ring(5);
        assert!(x_of(&r, 0) > 200 && x_of(&r, 4) < 200);

        // halfway through turning from card 2 to card 3: the two trade places smoothly
        let mid = carousel_rects(area, cw, 2 * cw + cw / 2, n);
        assert_eq!(x_of(&mid, 2) + x_of(&mid, 3), 400, "symmetric about the center");

        // a window narrower than a card: no room to peek, never a negative offset
        let narrow = carousel_rects(Rect { w: 500, ..area }, cw, 0, n);
        assert!(narrow.iter().all(|s| s.1.x == 0));
    }

    #[test]
    fn spaces_take_the_lowest_free_number_and_five_tabs_fill_the_header() {
        assert_eq!(next_space_name(&[]), "Space 1");
        assert_eq!(next_space_name(&["Space 1", "Space 3"]), "Space 2", "a closed space's number is reused");
        assert_eq!(next_space_name(&["api", "Space 1"]), "Space 2", "a renamed space frees its number");
        let sq = 36;
        for w in [480, 1280, 1920, 3840] {
            let left = w - 5 * sq - MAX_SPACES as i32 * tab_width(w, sq); // between "+" and the window buttons, after five tabs
            assert!((0..MAX_SPACES as i32).contains(&left), "five tabs fit, with only rounding to spare: {left} at {w}px");
        }
    }

    #[test]
    fn a_space_shows_its_terminals_side_by_side() {
        let area = Rect { x: 0, y: 36, w: 1000, h: 500 };
        let one = tile(area, vec![4]);
        assert_eq!((one[0].0, one[0].1.w), (4, 1000), "alone: the whole window");
        let two = tile(area, vec![4, 7]);
        assert_eq!((two[0].0, two[0].1.w, two[1].0, two[1].1.x, two[1].1.w), (4, 500, 7, 500, 500));
        assert_eq!((two[0].1.h, two[1].1.h), (500, 500), "both full height");
    }

    #[test]
    fn scrollbar_stays_below_the_header_and_above_the_bottom_border() {
        let th = Theme::light();
        let (hh, outer) = (36, Rect { x: 0, y: 10, w: 200, h: 300 });
        let mut t = Term::new(10, 3);
        for i in 0..40 {
            t.feed(format!("line {i}
").as_bytes());
        }
        let hist = t.history.len() as f32;
        for pos in [0.0, hist / 2.0, hist] {
            // scrolled to the bottom, the middle and the very top
            let mut buf = vec![0xffffffu32; 200 * 320];
            let mut cv = Canvas::new(&mut buf, 200, 320);
            draw_scrollbar(&mut cv, &t, pos, &th, outer, hh, 1.0);
            let touched: Vec<i32> = (0..320).filter(|&y| (0..200).any(|x| buf[(y * 200 + x) as usize] != 0xffffff)).collect();
            assert!(!touched.is_empty(), "the scrollbar is drawn");
            assert!(*touched.first().unwrap() > outer.y + hh, "pos {pos}: starts below the header, got y={}", touched.first().unwrap());
            assert!(*touched.last().unwrap() < outer.y + outer.h - 2, "pos {pos}: ends above the bottom border, got y={}", touched.last().unwrap());
        }
    }

    #[test]
    fn dimming_greys_terminal_text_toward_the_background() {
        let th = Theme::light();
        let mut font = Font::new(FONT_PX);
        let mut t = Term::new(4, 1);
        t.feed(b"XXXX");
        let (w, h) = (font.cw * 4, font.ch);
        let mut darkest = |dim: f32| {
            let mut buf = vec![th.bg; (w * h) as usize];
            let mut cv = Canvas::new(&mut buf, w, h);
            draw_term(&mut cv, &mut font, &t, 0.0, None, &th, Rect { x: 0, y: 0, w, h }, false, dim);
            buf.iter().map(|p| (p & 0xff) as i32).min().unwrap() // darkest blue channel: the ink
        };
        let (live, dimmed) = (darkest(0.0), darkest(1.0));
        assert!(dimmed > live + 40, "dimmed text must be clearly lighter: {live} -> {dimmed}");
        assert!(dimmed < (th.bg & 0xff) as i32, "but still visible against the background");
    }

    #[test]
    fn panel_padding_is_24_design_px_on_every_side() {
        for scale in [1.0f32, 1.5, 2.0] {
            let pad = px(PANE_PAD, scale);
            let outer = Rect { x: 100, y: 50, w: 640, h: 380 };
            let i = pane_inner(outer, scale);
            assert_eq!((i.x - outer.x, i.y - outer.y - px(HEADER, scale)), (pad, pad)); // left; top, below the panel header
            assert_eq!(outer.x + outer.w - 1 - (i.x + i.w), pad); // right, inside the 1px divider
            assert_eq!(outer.y + outer.h - 1 - (i.y + i.h), pad); // bottom, inside the 1px divider
            assert_eq!(pad, (24.0 * scale).round() as i32);
        }
    }

    #[test]
    fn easing_is_css_ease_in_out() {
        // reference points of cubic-bezier(0.42, 0, 0.58, 1)
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert!(near(ease_in_out(0.0), 0.0) && near(ease_in_out(1.0), 1.0) && near(ease_in_out(0.5), 0.5));
        assert!(near(ease_in_out(0.25), 0.129), "slow start: {}", ease_in_out(0.25));
        assert!(near(ease_in_out(0.75), 0.871), "slow finish: {}", ease_in_out(0.75));
        let mut prev = 0.0;
        for i in 1..=100 {
            let v = ease_in_out(i as f32 / 100.0);
            assert!(v >= prev, "must never go backwards");
            prev = v;
        }
    }

    #[test]
    fn continuous_scroll_input_never_stalls() {
        // A touchpad-ish stream: the goal advances 0.5 line every 8 ms for 0.4 s; we draw every 16 ms.
        // Restarting a timed ease on every event (the old behaviour) leaves the view crawling near its start.
        let (mut pos, mut goal, mut prev) = (0.0f32, 0.0f32, 0.0f32);
        for frame in 1..=25 {
            goal += 1.0; // two events per frame
            pos = chase(pos, goal, 0.016, SCROLL_TAU);
            assert!(pos > prev, "frame {frame}: must keep moving while input continues");
            prev = pos;
        }
        assert!(pos > goal * 0.75, "stays close behind the fingers: {pos} of {goal}");
        // once input stops it settles on the goal, without overshooting
        for _ in 0..60 {
            pos = chase(pos, goal, 0.016, SCROLL_TAU);
            assert!(pos <= goal);
        }
        assert_eq!(pos, goal);
        // dt = 0 (first frame after idle) does not move
        assert_eq!(chase(1.0, 5.0, 0.0, SCROLL_TAU), 1.0);
    }

    #[test]
    fn tween_takes_exactly_the_duration_and_reverses_smoothly() {
        let mut tw = Tween::at(0.0);
        assert!(tw.done() && tw.value() == 0.0);
        tw.retarget(1.0, ANIM_SECS);
        assert!(!tw.done() && tw.value() == 0.0);
        tw.step(0.1);
        assert!((tw.value() - 0.5).abs() < 0.01, "halfway in time is halfway in value: {}", tw.value());
        tw.step(0.05);
        assert!(!tw.done());
        tw.step(0.05);
        assert!(tw.done() && tw.value() == 1.0, "done after 0.2s");
        // reversing half-way starts from the current value, so there is no jump
        let mut tw = Tween::at(0.0);
        tw.retarget(1.0, ANIM_SECS);
        tw.step(0.1);
        let mid = tw.value();
        tw.retarget(0.0, ANIM_SECS * mid);
        assert!((tw.value() - mid).abs() < 1e-6 && tw.to == 0.0);
        tw.step(10.0);
        assert!(tw.done() && tw.value() == 0.0);
        // shifting moves both ends: nothing to animate
        let mut tw = Tween::at(3.0);
        tw.shift(2.0);
        assert!(tw.done() && tw.value() == 5.0);
    }

    /// Renders a terminal with scrollback at several fractional scroll positions: `MAULA_PREVIEW=out.png cargo test scroll_frames -- --ignored`
    #[test]
    #[ignore]
    fn scroll_frames() {
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "scroll.png".into());
        let mut font = Font::new(FONT_PX);
        let mut t = Term::new(24, 5);
        for i in 1..=12 {
            t.feed(format!("line {i:02} the quick brown\r\n").as_bytes());
        }
        let th = Theme::light();
        let r = Rect { x: 8, y: 8, w: 24 * font.cw, h: 5 * font.ch };
        let (w, h) = (r.w + 16, r.h + 16);
        let (mut all, mut total) = (Vec::new(), 0);
        for pos in [0.0f32, 0.5, 1.0, 1.5, 4.0] {
            let mut buf = vec![th.bg; (w * h) as usize];
            let mut cv = Canvas::new(&mut buf, w, h);
            cv.set_clip(r);
            draw_term(&mut cv, &mut font, &t, pos, None, &th, r, true, 0.0);
            all.extend(buf);
            all.extend(vec![0x808080; w as usize * 4]);
            total += h + 4;
        }
        write_png(&path, w as usize, total as usize, &all);
    }

    /// Renders the prompt mark through one wave, large and at real size, and a panel header with memory on and
    /// Claude waiting: `MAULA_PREVIEW=out.png cargo test prompt_mark_frames -- --ignored`
    #[test]
    #[ignore]
    fn prompt_mark_frames() {
        let path = std::env::var("MAULA_PREVIEW").unwrap_or_else(|_| "mark.png".into());
        let th = Theme::dark();
        let (mut font, mut titlef, mut closef, mut icons) = (Font::new(FONT_PX * 1.5), Font::medium(TITLE_PX * 1.5), Font::medium(CLOSE_PX * 1.5), Font::icons(ICON * 1.5));
        let (w, h) = (900, 320);
        let mut buf = vec![th.bg; (w * h) as usize];
        let mut cv = Canvas::new(&mut buf, w, h);
        for k in 0..6 {
            let t = k as f32 * 0.32; // 0 (still) .. 1.6 s: one wave
            draw_prompt_mark(&mut cv, Rect { x: 20 + k * 140, y: 20, w: 120, h: 120 }, t, th.accent, th.bg);
            let cell = Rect { x: 20 + k * 140, y: 160, w: 2 * font.cw, h: font.ch };
            draw_prompt_mark(&mut cv, cell, t, th.accent, th.bg);
            let x = cell.x + 3 * font.cw;
            cv.text(&mut font, "cargo run", x, cell.y, th.fg);
        }
        let hr = Rect { x: 0, y: 220, w, h: px(HEADER, 1.5) };
        draw_panel_header(&mut cv, &mut titlef, &mut closef, &mut icons, &th, 1.5, hr, 0, "maula", Some('D'), true, true, true, &HashMap::new());
        let hr = Rect { y: 220 + px(HEADER, 1.5) + 8, ..hr };
        draw_panel_header(&mut cv, &mut titlef, &mut closef, &mut icons, &th, 1.5, hr, 1, "api", Some('C'), false, false, false, &HashMap::new());
        write_png(&path, w as usize, h as usize, &buf);
    }

    /// The header must be exactly HEADER design px tall at every scale, and icons must be one size.
    #[test]
    fn sizes_are_consistent() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            assert_eq!(px(HEADER, scale), (36.0 * scale).round() as i32);
            let mut icons = Font::icons(ICON * scale);
            let sizes: Vec<(usize, usize)> = [Icon::Chevron, Icon::Plus, Icon::Close, Icon::Minimize, Icon::Maximize, Icon::Restore, Icon::Check]
                .iter()
                .map(|i| {
                    let g = icons.glyph(i.glyph());
                    assert!(g.ink_size().0 > 0, "missing glyph for {:?}", i.glyph());
                    g.ink_size()
                })
                .collect();
            let tallest = sizes.iter().map(|s| s.1).max().unwrap() as f32;
            assert!(tallest <= ICON * scale + 1.0, "icon ink {tallest}px exceeds the {}px icon size at scale {scale}", ICON * scale);
        }
    }
}

/// Timing probe: how long one full-window frame takes (same build profile as `cargo run`).
/// `MAULA_SCALE=1.5 cargo test frame_cost -- --ignored --nocapture`
#[cfg(test)]
mod frame_cost {
    use super::*;

    #[test]
    #[ignore]
    fn frame_cost() {
        let scale: f32 = std::env::var("MAULA_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.5);
        let (w, h) = ((1280.0 * scale) as i32, (800.0 * scale) as i32);
        let th = Theme::light();
        let (mut titlef, mut icons, mut font, mut closef) = (Font::medium(TITLE_PX * scale), Font::icons(ICON * scale), Font::new(FONT_PX * scale), Font::medium(CLOSE_PX * scale));
        let hh = px(HEADER, scale);
        let (pw, ph) = (w / 2, (h - hh) / 2);
        let mut terms = Vec::new();
        for _ in 0..4 {
            let inner = pane_inner(Rect { x: 0, y: 0, w: pw, h: ph }, scale);
            let mut t = Term::new((inner.w / font.cw) as usize, (inner.h / font.ch) as usize);
            for i in 0..t.rows {
                t.feed(format!("line {i:03} the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnopqrstuvwxyz\r\n").as_bytes());
            }
            terms.push(t);
        }
        let mut buf = vec![0u32; (w * h) as usize];
        let frame = |buf: &mut Vec<u32>, font: &mut Font, titlef: &mut Font, closef: &mut Font, icons: &mut Font, area: Rect| {
            let mut cv = Canvas::new(buf, w, h);
            cv.set_bounds(area);
            cv.rect(Rect { x: 0, y: 0, w, h }, th.ui_bg);
            let none = HashMap::new();
            let chrome = Chrome { th, scale, w, fades: &none, view: View::Grid, theme: 0, menu_open: false, menu_sel: None, maximized: false, can_add: true, hooks: false };
            draw_header(&mut cv, icons, &chrome, &mut Vec::new());
            for (k, t) in terms.iter().enumerate() {
                let outer = Rect { x: (k as i32 % 2) * pw, y: hh + (k as i32 / 2) * ph, w: pw, h: ph };
                let inner = pane_inner(outer, scale);
                cv.set_clip(outer);
                cv.rect(outer, th.bg);
                draw_panel_header(&mut cv, titlef, closef, icons, &th, scale, Rect { x: outer.x, y: outer.y, w: outer.w - 1, h: hh }, k, "maula", Some('C'), k == 0, false, false, &none);
                cv.set_clip(inner);
                draw_term(&mut cv, font, t, 0.0, None, &th, inner, k == 0, 0.0);
            }
        };
        let all = Rect { x: 0, y: 0, w, h };
        frame(&mut buf, &mut font, &mut titlef, &mut closef, &mut icons, all); // warm the glyph cache
        let n = 30;
        let t0 = Instant::now();
        for _ in 0..n {
            frame(&mut buf, &mut font, &mut titlef, &mut closef, &mut icons, all);
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let part = Rect { x: 0, y: 0, w: 3 * px(HEADER, scale), h: hh }; // one hovered header button
        let tp = Instant::now();
        for _ in 0..n {
            frame(&mut buf, &mut font, &mut titlef, &mut closef, &mut icons, part);
        }
        let partial = tp.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t1 = Instant::now();
        let copy = vec![0u32; buf.len()];
        let mut dst = vec![0u32; buf.len()];
        for _ in 0..n {
            dst.copy_from_slice(&copy);
        }
        println!("FRAME {w}x{h}: {per:.1} ms full redraw, {partial:.2} ms hover-only redraw, {:.1} ms buffer copy (budget at 60fps: 16.7 ms)", t1.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }
}

/// Timing probe for card layouts: `MAULA_SCALE=1.5 cargo test card_layouts -- --ignored --nocapture`
#[cfg(test)]
mod card_layouts {
    use super::*;

    #[test]
    #[ignore]
    fn card_layouts() {
        let scale: f32 = std::env::var("MAULA_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.5);
        let (w, h) = ((1280.0 * scale) as i32, (800.0 * scale) as i32);
        let th = Theme::light();
        let (mut titlef, mut closef, mut font, mut icons) = (Font::medium(TITLE_PX * scale), Font::medium(CLOSE_PX * scale), Font::new(FONT_PX * scale), Font::icons(ICON * scale));
        let hh = px(HEADER, scale);
        let none = HashMap::new();
        let content = Rect { x: 0, y: hh, w, h: h - hh };
        let mut buf = vec![0u32; (w * h) as usize];
        let card_w = CARD_COLS * font.cw + 2 * px(PANE_PAD, scale) + 1;

        // a terminal full of text, sized for the given panel width
        let (cw, ch) = (font.cw, font.ch);
        let term_for = |outer: Rect| {
            let inner = pane_inner(outer, scale);
            let mut t = Term::new((inner.w / cw).max(2) as usize, (inner.h / ch).max(1) as usize);
            for i in 0..t.rows {
                t.feed(format!("line {i:03} the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnopqrstuvwxyz\r\n").as_bytes());
            }
            t
        };
        let all = Rect { x: 0, y: 0, w, h };
        let mut time = |label: &str, rects: Vec<Rect>, terms: &[Term], area: Rect| {
            let mut frame = |buf: &mut Vec<u32>| {
                let mut cv = Canvas::new(buf, w, h);
                cv.set_bounds(area);
                cv.rect(Rect { x: 0, y: 0, w, h }, th.ui_bg);
                for (k, (outer, t)) in rects.iter().zip(terms).enumerate() {
                    cv.set_clip(outer.intersect(&content));
                    cv.rect(*outer, th.bg);
                    draw_panel_header(&mut cv, &mut titlef, &mut closef, &mut icons, &th, scale, Rect { x: outer.x, y: outer.y, w: outer.w - 1, h: hh }, k, "maula", Some('C'), k == 0, false, false, &none);
                    let inner = pane_inner(*outer, scale);
                    cv.set_clip(inner.intersect(&content));
                    draw_term(&mut cv, &mut font, t, 0.0, None, &th, inner, k == 0, 0.0);
                }
            };
            frame(&mut buf);
            let t0 = Instant::now();
            for _ in 0..40 {
                frame(&mut buf);
            }
            println!("CARDS {label}: {:.2} ms/frame", t0.elapsed().as_secs_f64() * 1000.0 / 40.0);
        };

        // 90-column cards (today): 4 cards, strip scrolled to different offsets
        let strip = |off: i32| (0..4).map(|i| Rect { x: i * card_w - off, y: hh, w: card_w, h: h - hh }).collect::<Vec<_>>();
        let cards: Vec<Term> = strip(0).iter().map(|r| term_for(*r)).collect();
        time("90-col cards, at rest, whole window", strip(0), &cards, all);
        time("90-col cards, ONE card repainted (a visible terminal printing)", strip(0), &cards, strip(0)[0]);
        time("90-col cards, mid-scroll, whole window", strip(card_w / 2 + 40), &cards, all);
        // two panels per view: each exactly half the window
        let halves = vec![Rect { x: 0, y: hh, w: w / 2, h: h - hh }, Rect { x: w / 2, y: hh, w: w - w / 2, h: h - hh }];
        let two: Vec<Term> = halves.iter().map(|r| term_for(*r)).collect();
        time("2 panels per view, whole window", halves.clone(), &two, all);
        time("2 panels per view, ONE panel repainted", halves.clone(), &two, halves[0]);
        // paging between two views: the strip slides by one window width, so up to 4 half-width panels show at once
        let quarter: Vec<Rect> = (0..4).map(|i| Rect { x: i * (w / 2) - w / 4, y: hh, w: w / 2, h: h - hh }).collect();
        let four: Vec<Term> = quarter.iter().map(|r| term_for(*r)).collect();
        time("2 per view, mid-page-turn", quarter, &four, all);
    }
}
