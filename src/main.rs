#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod claude;
mod config;
mod gfx;
mod json;
mod link;
mod notes_api;
mod picker;
mod rich;
mod suggest;
mod term;
mod ttf;
mod width;
mod win;
mod window;

use std::collections::VecDeque;
use config::{Config, PanelCfg, SavedSpace, Theme, THEME_NAMES};
use gfx::{blend, Canvas, Font, Rect};
use picker::{Kind, Picker};
use rich::{rel_cat, Flag, Span, Spans, Style};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};
use suggest::Suggest;
use term::{Cell, Color, Mouse, MouseEv, Term, BLANK};
use window::{Control, CursorIcon, ElementState, Event, Key, KeyEvent, ModifiersState, MouseButton, MouseScrollDelta, NamedKey, ResizeDirection, Window};
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
const MENU_ACTIONS: [Action; 14] = [
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
    Action::SubMenu(6),
    Action::InstallHooks,
];
const MENU_GROUPS: [(&str, usize); 4] = [("Theme", 4), ("View", 4), ("Text", 4), ("Claude", 2)];

/// The dropdown's submenu rows. The Text group (the first four) applies to the terminal and the header titles;
/// Accent sits in the Theme group, Maximize in the View group, Render in the Claude group.
const TEXT_ROWS: [&str; 7] = ["Font", "Size", "Line height", "Letter spacing", "Accent", "Maximize", "Render"];
const SIZES: [f32; 6] = [85.0, 90.0, 100.0, 110.0, 125.0, 150.0]; // percent of FONT_PX / TITLE_PX
const LINES: [f32; 5] = [0.0, 1.2, 1.4, 1.6, 1.8]; // terminal row height, times the font size; 0 = the font's own (~1.32)
const TRACKS: [f32; 5] = [0.0, 0.5, 1.0, 1.5, 2.0]; // letter-spacing, design px
const ACCENTS: [(&str, u32); 6] = [("Default", 0), ("Terracotta", 0xd97757), ("Blue", 0x4f86d9), ("Teal", 0x2a9d8f), ("Violet", 0x8b6fd6), ("Rose", 0xd9577f)];
const MAXIMIZE: [&str; 2] = ["Borderless fullscreen", "Maximized window"]; // saved as "fullscreen" / "window"
const RENDER: [&str; 2] = ["Terminal", "Software"]; // how panels running Claude draw; saved in lower case
const TEXT_DEFAULT: [usize; 7] = [0, 2, 0, 0, 0, 0, 0]; // chosen index per TEXT_ROWS entry

/// Labels of the choices in Text submenu `k`.
fn text_choices(k: usize) -> Vec<String> {
    match k {
        0 => vec!["JetBrains Mono".into()],
        1 => SIZES.iter().map(|s| format!("{s}%")).collect(),
        2 => LINES.iter().map(|&l| if l == 0.0 { "Default".into() } else { format!("{l}") }).collect(),
        4 => ACCENTS.iter().map(|a| a.0.into()).collect(),
        5 => MAXIMIZE.iter().map(|&m| m.into()).collect(),
        6 => RENDER.iter().map(|&r| r.into()).collect(),
        _ => TRACKS.iter().map(|t| format!("{t}px")).collect(),
    }
}

/// The Text choices saved in the config; a missing value, or one not on the menu, is the default.
fn text_of(cfg: &Config) -> [usize; 7] {
    let at = |list: &[f32], v: Option<f32>, k: usize| v.and_then(|v| list.iter().position(|&x| x == v)).unwrap_or(TEXT_DEFAULT[k]);
    let accent = ACCENTS.iter().position(|a| a.0.eq_ignore_ascii_case(&cfg.accent)).unwrap_or(0);
    let window = usize::from(cfg.maximize.eq_ignore_ascii_case("window"));
    let software = usize::from(cfg.render.eq_ignore_ascii_case("software"));
    [0, at(&SIZES, cfg.font_size, 1), at(&LINES, cfg.line_height, 2), at(&TRACKS, cfg.letter_spacing, 3), accent, window, software]
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

/// Terminal, title and running-text fonts for the Text choices `text` (see `TEXT_DEFAULT`).
fn text_fonts(text: [usize; 7], scale: f32) -> (Font, Font, Font) {
    let (k, track) = (SIZES[text[1]] / 100.0 * scale, TRACKS[text[3]] * scale);
    let mono = Font::new(FONT_PX * k).spaced(LINES[text[2]], track);
    let prose = Font::prose(FONT_PX * k, mono.ascent);
    (mono, Font::medium(TITLE_PX * k).spaced(0.0, track), prose)
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

/// Dev aid: with `MAULA_RECORD=<dir>` every panel's raw shell output is appended to `<dir>/pane-<id>.out`, its resizes
/// in-band as `ESC _ maula-resize;<cols>;<rows> ESC \` (an APC, which the terminal ignores), so a session can be
/// replayed through `Term` byte for byte.
fn record(id: usize, bytes: &[u8]) {
    let Some(dir) = std::env::var_os("MAULA_RECORD") else { return };
    let path = PathBuf::from(dir).join(format!("pane-{id}.out"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(bytes);
    }
}

enum Ev {
    Output(usize, Vec<u8>),
    Exit(usize),
    Hook(claude::Hook),
    Note(notes_api::Request), // `maula note ...` from Claude in a pane
}

/// Hands `Ev`s from other threads (shell output, hooks) to the UI thread, waking its event loop.
#[derive(Clone)]
struct Proxy {
    tx: std::sync::mpsc::Sender<Ev>,
    waker: window::Waker,
}

impl Proxy {
    /// Err once the UI is gone: the sending thread can stop.
    fn send_event(&self, ev: Ev) -> Result<(), ()> {
        self.tx.send(ev).map_err(drop)?;
        self.waker.wake();
        Ok(())
    }
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
    Notes(usize),          // open or close pane i's notepad
    NoteFmt(NoteCmd),      // a formatting button of the open notepad
    NoteSave(usize),       // save the space of pane i (and with it the notes)
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
    doc: NoteDoc,                    // the notepad of this pane
}

const LF: char = '\u{a}';

/// A pane's notepad: the text, its styles, the caret and the undo history.
struct NoteDoc {
    note: String,                    // the notepad of this pane
    note_caret: usize,               // byte offset into `note`, on a char boundary
    note_saved: String,              // the note as last saved with its space
    note_anchor: usize,              // the other end of the selection (== note_caret: none)
    note_dirty: bool,                // note != note_saved, kept up to date by edits
    note_undo: VecDeque<NoteEdit>,   // edits as deltas, oldest first
    note_redo: Vec<NoteEdit>,
    note_join: bool,                 // the next typing / delete may merge into the last undo step
    note_undo_bytes: usize,
    note_spans: Spans,               // character styles of `note`, by byte range
    note_pending: Option<Style>,     // the style of the next typed text, after a style toggle with nothing selected
    note_px: u16,                    // the default font size, for text without a size of its own
    note_line: usize,                // index into NOTE_LINES
    note_spans_saved: Spans,         // styling as last saved with the space
    note_px_saved: u16,
    note_line_saved: usize,
}

impl Default for NoteDoc {
    fn default() -> Self {
        NoteDoc {
            note: String::new(),
            note_caret: 0,
            note_saved: String::new(),
            note_anchor: 0,
            note_dirty: false,
            note_undo: VecDeque::new(),
            note_redo: Vec::new(),
            note_join: false,
            note_undo_bytes: 0,
            note_spans: Spans::default(),
            note_pending: None,
            note_px: config::NOTE_SIZE_DEFAULT as u16,
            note_line: 0,
            note_spans_saved: Spans::default(),
            note_px_saved: config::NOTE_SIZE_DEFAULT as u16,
            note_line_saved: 0,
        }
    }
}

impl std::ops::Deref for Pane {
    type Target = NoteDoc;
    fn deref(&self) -> &NoteDoc {
        &self.doc
    }
}

impl std::ops::DerefMut for Pane {
    fn deref_mut(&mut self) -> &mut NoteDoc {
        &mut self.doc
    }
}

impl NoteDoc {
    /// The note holds something and has not been saved with its space: this panel may not be closed.
    fn note_blocks(&self) -> bool {
        self.note_dirty && !self.note.trim().is_empty()
    }

    /// The selected bytes of the note, if any.
    fn note_sel(&self) -> Option<(usize, usize)> {
        (self.note_anchor != self.note_caret).then(|| (self.note_anchor.min(self.note_caret), self.note_anchor.max(self.note_caret)))
    }

    /// Replaces `old` bytes at `at` with `s` (one operation, however long `s` is) and puts the caret after it. The edit
    /// is recorded as a delta for undo; `kind` says whether it may merge into the step before: 1 typing, 2 Backspace,
    /// 3 Delete, 0 never (paste, cut, selections, ...).
    fn note_replace(&mut self, at: usize, old: usize, s: &str, kind: u8, style: Style) {
        let removed = self.note[at..at + old].to_string();
        let removed_sty = self.note_spans.slice(at, old);
        let ins = if style.is_plain() || s.is_empty() { Vec::new() } else { vec![Span { start: 0, end: s.len(), style }] };
        let (caret, anchor) = (self.note_caret, self.note_anchor);
        let merged = kind != 0
            && self.note_join
            && self.note_undo.back_mut().is_some_and(|e| {
                let fits = e.kind == kind
                    && match kind {
                        1 => e.removed.is_empty() && e.at + e.inserted.len() == at && !s.contains('\n'),
                        2 => e.inserted.is_empty() && at + old == e.at,
                        _ => e.inserted.is_empty() && at == e.at,
                    };
                if fits {
                    match kind {
                        1 => {
                            rel_cat(&mut e.inserted_sty, e.inserted.len(), &ins);
                            e.inserted.push_str(s);
                        }
                        2 => {
                            let mut joined = removed_sty.clone();
                            rel_cat(&mut joined, removed.len(), &e.removed_sty);
                            e.removed_sty = joined;
                            e.removed.insert_str(0, &removed);
                            e.at = at;
                        }
                        _ => {
                            rel_cat(&mut e.removed_sty, e.removed.len(), &removed_sty);
                            e.removed.push_str(&removed);
                        }
                    }
                }
                fits
            });
        self.note_undo_bytes += removed.len() + s.len();
        if !merged {
            self.note_undo.push_back(NoteEdit { at, removed, inserted: s.to_string(), removed_sty, inserted_sty: ins.clone(), caret, anchor, kind });
        }
        while self.note_undo_bytes > NOTE_UNDO_CAP && self.note_undo.len() > 1 {
            let e = self.note_undo.pop_front().unwrap();
            self.note_undo_bytes -= e.removed.len() + e.inserted.len();
        }
        self.note_redo.clear();
        self.note_join = kind != 0;
        self.note.replace_range(at..at + old, s);
        self.note_spans.splice(at, old, s.len(), &ins);
        self.note_pending = None;
        self.note_caret = at + s.len();
        self.note_anchor = self.note_caret;
        self.note_recheck();
    }

    /// An edit from `maula note`: write replaces everything (one undo step, no spans survive), append adds at the end
    /// in the style found there, clear deletes all. Returns (at, old, new len) for the layout, None when nothing changed.
    fn note_api(&mut self, op: notes_api::Op, text: &str) -> Option<(usize, usize, usize)> {
        use notes_api::Op;
        let text = note_clean(text);
        let (at, old, ins, style) = match op {
            Op::Read => return None,
            Op::Write => (0, self.note.len(), text, Style::default()),
            Op::Append => (self.note.len(), 0, text, self.note_spans.style_at_caret(self.note.len())),
            Op::Clear => (0, self.note.len(), String::new(), Style::default()),
        };
        if old == 0 && ins.is_empty() || self.note[at..at + old] == ins && self.note_spans.slice(at, old).is_empty() {
            return None;
        }
        self.note_replace(at, old, &ins, 0, style);
        Some((at, old, ins.len()))
    }

    /// Updates `note_dirty`: the text or its formatting differs from what was saved. Once per edit, not per frame.
    fn note_recheck(&mut self) {
        self.note_dirty = self.note != self.note_saved || self.note_px != self.note_px_saved || self.note_line != self.note_line_saved || self.note_spans != self.note_spans_saved;
    }

    /// The style text typed at `pos` takes: that of the character before it, but on an empty line that of its line break.
    fn note_style_for(&self, pos: usize) -> Style {
        let empty = self.note[..pos].chars().next_back().is_none_or(|c| c == LF) && self.note[pos..].starts_with(LF);
        if empty { self.note_spans.style_of(pos) } else { self.note_spans.style_at_caret(pos) }
    }

    /// The style the next typed character gets.
    fn note_caret_style(&self) -> Style {
        self.note_pending.unwrap_or_else(|| self.note_style_for(self.note_caret))
    }

    /// The size in points the toolbar shows: at the caret, or all of the selection; None when the selection mixes sizes.
    fn note_size_now(&self) -> Option<u8> {
        let def = self.note_px as u8;
        let eff = |s: Style| if s.size == 0 { def } else { s.size };
        match self.note_sel() {
            Some((a, b)) => {
                let segs = self.note_spans.segments(a, b);
                let first = eff(segs[0].2);
                segs.iter().all(|s| eff(s.2) == first).then_some(first)
            }
            None => Some(eff(self.note_caret_style())),
        }
    }

    /// The paragraph (newline-delimited line) holding byte `c`: its start and its end before the line break.
    fn note_para(&self, c: usize) -> (usize, usize) {
        (self.note[..c].rfind(LF).map_or(0, |i| i + 1), self.note[c..].find(LF).map_or(self.note.len(), |i| c + i))
    }

    /// The style of the character at the caret: what a toolbar button shows. A selection shows what all of it shares.
    fn note_style_now(&self) -> Style {
        if let Some(p) = self.note_pending.filter(|_| self.note_sel().is_none()) {
            return p;
        }
        match self.note_sel() {
            Some((a, b)) => {
                let all = |f: &dyn Fn(&Style) -> bool| self.note_spans.all(a, b, f);
                let hl = self.note_spans.style_of(a).hl;
                Style { bold: all(&|s| s.bold), italic: all(&|s| s.italic), underline: all(&|s| s.underline), hl: if all(&|s| s.hl == hl) { hl } else { 0 }, size: 0 }
            }
            None => self.note_style_for(self.note_caret),
        }
    }

    /// Undoes (or redoes) one step. Returns (at, old bytes, new bytes) for the layout, or None if there is nothing.
    fn note_step(&mut self, redo: bool) -> Option<(usize, usize, usize)> {
        let e = if redo { self.note_redo.pop()? } else { self.note_undo.pop_back()? };
        let (old, new) = if redo { (&e.removed, &e.inserted) } else { (&e.inserted, &e.removed) };
        let (at, n_old, n_new) = (e.at, old.len(), new.len());
        self.note.replace_range(at..at + n_old, new);
        self.note_spans.splice(at, n_old, n_new, if redo { &e.inserted_sty } else { &e.removed_sty });
        self.note_pending = None;
        if redo {
            (self.note_caret, self.note_anchor) = (at + n_new, at + n_new);
            self.note_undo.push_back(e);
        } else {
            (self.note_caret, self.note_anchor) = (e.caret, e.anchor);
            self.note_redo.push(e);
        }
        self.note_join = false;
        self.note_recheck();
        Some((at, n_old, n_new))
    }

    /// Where Ctrl+Left / Ctrl+Right from `at` lands: over the whitespace, then over a run of one kind of character
    /// (Right: over the run, then the whitespace after it).
    fn note_word(&self, at: usize, left: bool) -> usize {
        let class = |c: char| if c.is_whitespace() { 0 } else if c.is_alphanumeric() || c == '_' { 1 } else { 2 };
        if left {
            let mut i = at;
            let mut chars = self.note[..at].chars().rev().peekable();
            while let Some(c) = chars.next_if(|&c| class(c) == 0) {
                i -= c.len_utf8();
            }
            if let Some(&c0) = chars.peek() {
                while let Some(c) = chars.next_if(|&c| class(c) == class(c0)) {
                    i -= c.len_utf8();
                }
            }
            i
        } else {
            let mut i = at;
            let mut chars = self.note[at..].chars().peekable();
            if let Some(&c0) = chars.peek() {
                if class(c0) != 0 {
                    while let Some(c) = chars.next_if(|&c| class(c) == class(c0)) {
                        i += c.len_utf8();
                    }
                }
            }
            while let Some(c) = chars.next_if(|&c| class(c) == 0) {
                i += c.len_utf8();
            }
            i
        }
    }

    /// The word (or the run of spaces / punctuation) around `at`.
    fn note_word_at(&self, at: usize) -> (usize, usize) {
        let class = |c: char| if c == '\n' { 3 } else if c.is_whitespace() { 0 } else if c.is_alphanumeric() || c == '_' { 1 } else { 2 };
        let Some(k) = self.note[at..].chars().next().or_else(|| self.note[..at].chars().next_back()).map(class) else { return (at, at) };
        let mut a = at;
        for c in self.note[..at].chars().rev().take_while(|&c| class(c) == k) {
            a -= c.len_utf8();
        }
        let mut b = at;
        for c in self.note[at..].chars().take_while(|&c| class(c) == k) {
            b += c.len_utf8();
        }
        (a, b)
    }

    fn note_prev(&self, at: usize) -> usize {
        self.note[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn note_next(&self, at: usize) -> usize {
        at + self.note[at..].chars().next().map_or(0, char::len_utf8)
    }
}

impl Pane {
    /// Draw this panel with the Software render: chosen in the dropdown, Claude runs here (hooks report it), and it is
    /// on its classic renderer. A full-screen program places every cell itself, so that stays a grid.
    fn software(&self, text: [usize; 7]) -> bool {
        text[6] == 1 && self.claude.is_some() && self.term.mouse == Mouse::Off && !self.term.alt_screen()
    }

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
fn spawn_pty(proxy: &Proxy, id: usize, shell: Option<&str>, dir: Option<&Path>, ipc: Option<&(u16, String)>) -> (win::Pty, std::fs::File) {
    let (exe, args, mut env) = shell_command(shell);
    // Claude Code keeps its classic renderer here: its full-screen one draws on an alternate screen with its own
    // scrollback, which leaves maula's scrollback, scrollbar, search and selection with nothing to work on.
    // Only shells started by maula get it. A `/tui fullscreen` typed in Claude still switches: it relaunches itself past this.
    env.push(("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "1".to_string()));
    if let Some((port, token)) = ipc {
        env.extend([("MAULA_PANE", id.to_string()), ("MAULA_PORT", port.to_string()), ("MAULA_TOKEN", token.clone()), ("MAULA_NOTE_PORT", notes_api::PORT.load(std::sync::atomic::Ordering::Relaxed).to_string())]);
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
        },
        Key::Character(c) => {
            let up = c.chars().next()?.to_ascii_uppercase();
            if ctrl && ('@'..='_').contains(&up) {
                char::from(up as u8 & 0x1f).to_string()
            } else {
                c.to_string()
            }
        }
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
    window: Window,
    hwnd: isize, // the window, for presenting frames and owning the clipboard
    font: Font,    // terminal (monospace)
    prose: Font,   // running text in the Software render (proportional)
    ui_font: Font, // dropdown, popups
    title_font: Font, // panel titles
    icon_font: Font, // Lucide, sized to ICON
    scale: f32,      // display scaling (1.0 = 100%); all design sizes go through `px`
    proxy: Proxy,
    shell: Option<String>,
    editor: Option<String>, // what Ctrl+click opens file paths in
    search: Option<Search>,
    notes: Option<usize>, // pane whose notepad is open
    nv: NoteView,         // its layout, scrolling and geometry
    note_fonts: NoteFonts, // Segoe UI in the sizes and styles the notes use
    note_hint: bool,      // a close was refused: the footer says why
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
    bar_drag: Option<(usize, i32)>, // (pane, px below the thumb's top) while a terminal scrollbar thumb is held
    bar_hot: Option<usize>,         // pane whose scrollbar the pointer is over
    app_drag: Option<(usize, u8)>, // (pane index, button) pressed inside a program that owns the mouse
    app_cell: Option<(usize, usize, usize)>, // (pane, col, row) of the last motion reported, so each cell is sent once
    wheel_acc: f32,      // wheel notches not yet sent to a program that owns the mouse
    hover: Option<usize>, // index into `hits` under the mouse
    last_header_click: Option<Instant>, // for double-click to maximize
    menu_open: bool,                    // theme / view dropdown
    menu_sel: Option<usize>,            // highlighted dropdown row (hover or keyboard)
    sub: Option<usize>,                 // open Text submenu
    sub_sel: Option<usize>,             // highlighted row of it
    text: [usize; 7],                   // submenu choices, see TEXT_DEFAULT
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
    fn new(window: Window, proxy: Proxy, cfg: Config) -> Ui {
        let scale = window.scale_factor() as f32;
        let text = text_of(&cfg);
        let ((font, title_font, prose), ui_font) = (text_fonts(text, scale), Font::new(UI_PX * scale));
        let (icon_font, close_font) = (Font::icons(ICON * scale), Font::medium(CLOSE_PX * scale));
        let hwnd = window.hwnd;
        let p = proxy.clone();
        let ipc = claude::listen(move |h| {
            let _ = p.send_event(Ev::Hook(h));
        });
        if let Some((_, token)) = &ipc {
            let p = proxy.clone();
            notes_api::listen(token.clone(), move |r| {
                let _ = p.send_event(Ev::Note(r));
            });
        }
        let mut ui = Ui {
            window,
            hwnd,
            font,
            prose,
            ui_font,
            title_font,
            icon_font,
            scale,
            proxy,
            shell: cfg.shell.clone(),
            editor: cfg.editor.clone(),
            search: None,
            notes: None,
            nv: NoteView::default(),
            note_fonts: NoteFonts::default(),
            note_hint: false,
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
            app_drag: None,
            bar_drag: None,
            bar_hot: None,
            app_cell: None,
            wheel_acc: 0.0,
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
        self.window.is_maximized() || self.window.is_fullscreen()
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
                record(p.id, format!("_maula-resize;{cols};{rows}\\").as_bytes());
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
        self.window.set_dark(self.theme != 0);
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
            doc: NoteDoc::default(),
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

    /// Opens the notepad of pane `i`: a fresh layout, scrolled to the caret.
    fn open_notes(&mut self, i: usize, hint: bool) {
        self.notes = Some(i);
        self.note_hint = hint;
        self.nv.reset();
    }

    /// Types `s` over the selection (or at the caret).
    fn note_type(&mut self, n: usize, s: &str, kind: u8) {
        let s = note_clean(s);
        let p = &self.panes[n];
        let (at, old) = p.note_sel().map_or((p.note_caret, 0), |(a, b)| (a, b - a));
        self.note_edit(n, at, old, &s, if old == 0 { kind } else { 0 });
    }

    /// Replaces `old` bytes at `at` with `s` in the note and brings the cached layout up to date.
    fn note_edit(&mut self, n: usize, at: usize, old: usize, s: &str, kind: u8) {
        if old == 0 && s.is_empty() {
            return;
        }
        let k = self.note_k();
        let p = &mut self.panes[n];
        let style = if s.is_empty() { Style::default() } else { p.note_pending.unwrap_or_else(|| p.note_style_for(at)) };
        p.note_replace(at, old, s, kind, style);
        if self.nv.layout.valid() {
            let mut m = Measure::of(&mut self.note_fonts, k, p);
            self.nv.layout.edit(&p.note, at, old, s.len(), &mut m);
        }
        self.nv.follow = true;
    }

    /// `maula note read|write|append|clear` from the Claude session in pane `r.pane`; replies with the note's text.
    fn on_note(&mut self, r: notes_api::Request) {
        let Some(n) = self.panes.iter().position(|p| p.id == r.pane) else { return drop(r.reply.send(Err(format!("no pane {}", r.pane)))) };
        if let Some((at, old, new)) = self.panes[n].note_api(r.op, &r.text) {
            if n == self.focus && self.nv.layout.valid() {
                let k = self.note_k();
                let p = &self.panes[n];
                let mut m = Measure::of(&mut self.note_fonts, k, p);
                self.nv.layout.edit(&p.note, at, old, new, &mut m);
            }
            self.nv.follow = n == self.focus;
            self.window.request_redraw();
        }
        let _ = r.reply.send(Ok(self.panes[n].note.clone()));
    }

    /// Pixels per point of the note's text: scaled like the terminal's text.
    fn note_k(&self) -> f32 {
        SIZES[self.text[1]] / 100.0 * self.scale
    }

    /// A formatting command on pane `n`'s note.
    fn note_cmd(&mut self, n: usize, cmd: NoteCmd) {
        let p = &mut self.panes[n];
        let layout_changes = match cmd {
            NoteCmd::Style(flag) => {
                p.note_join = false;
                match p.note_sel() {
                    Some((a, b)) => {
                        p.note_spans.toggle(a, b, flag);
                        p.note_pending = None;
                    }
                    None => {
                        let mut st = p.note_caret_style();
                        let on = !flag.get(&st);
                        flag.set(&mut st, on);
                        p.note_pending = Some(st);
                    }
                }
                flag != Flag::Underline
            }
            NoteCmd::Highlight(h) => {
                p.note_join = false;
                match p.note_sel() {
                    Some((a, b)) => p.note_spans.set_hl(a, b, h),
                    None => {
                        let mut st = p.note_caret_style();
                        st.hl = h;
                        p.note_pending = Some(st);
                    }
                }
                self.nv.pop = false;
                false
            }
            NoteCmd::HlMenu => {
                self.nv.pop = !self.nv.pop;
                false
            }
            NoteCmd::SizeDown | NoteCmd::SizeUp => {
                // only the selection, or the line the caret is on, changes size
                let (up, def) = (cmd == NoteCmd::SizeUp, p.note_px as u8);
                p.note_join = false;
                match p.note_sel() {
                    Some((a, b)) => {
                        p.note_spans.step_sizes(a, b, up, def);
                        p.note_pending = None;
                    }
                    None => {
                        let (ps, pe) = p.note_para(p.note_caret);
                        let hi = (pe + 1).min(p.note.len()); // the line break carries the size of an empty line
                        if ps == pe {
                            let mut st = p.note_caret_style();
                            let eff = if st.size == 0 { def } else { st.size };
                            st.size = rich::store_size(rich::step_size(eff, up), def);
                            p.note_spans.set_size(ps, hi, st.size);
                            p.note_pending = Some(st);
                        } else {
                            p.note_spans.step_sizes(ps, hi, up, def);
                        }
                    }
                }
                true
            }
            NoteCmd::Line => {
                p.note_line = (p.note_line + 1) % NOTE_LINES.len();
                true
            }
        };
        p.note_recheck();
        if layout_changes {
            self.nv.layout.width = 0; // widths or heights changed: wrapped again by the next frame
        }
        self.nv.follow = true;
    }

    /// Undo (or redo) one step and patch the layout.
    fn note_undo(&mut self, n: usize, redo: bool) {
        if let Some((at, old, new)) = self.panes[n].note_step(redo) {
            if self.nv.layout.valid() {
                let (k, p) = (self.note_k(), &self.panes[n]);
                let mut m = Measure::of(&mut self.note_fonts, k, p);
                self.nv.layout.edit(&p.note, at, old, new, &mut m);
            }
            self.nv.follow = true;
        }
    }

    fn note_goto(&mut self, n: usize, to: usize, extend: bool) {
        let p = &mut self.panes[n];
        p.note_join = false;
        p.note_pending = None;
        p.note_caret = to;
        if !extend {
            p.note_anchor = to;
        }
    }

    /// Start (or end) of the shown line holding the caret.
    fn note_line_edge(&self, n: usize, end: bool) -> usize {
        let p = &self.panes[n];
        let c = p.note_caret;
        if self.nv.layout.valid() {
            let l = &self.nv.layout;
            let (a, b, last) = l.lines[l.line_of(c)];
            if !end {
                a
            } else if last || b == a {
                b
            } else {
                p.note_prev(b)
            }
        } else if end {
            p.note[c..].find('\n').map_or(p.note.len(), |i| c + i)
        } else {
            p.note[..c].rfind('\n').map_or(0, |i| i + 1)
        }
    }

    /// Up / Down by `rows` shown lines, or (`page`) by a screenful of pixels, keeping the column.
    fn note_vert(&mut self, n: usize, rows: isize, page: bool, extend: bool) {
        if !self.nv.layout.valid() {
            return;
        }
        let (k, l, p) = (self.note_k(), &self.nv.layout, &self.panes[n]);
        let mut m = Measure::of(&mut self.note_fonts, k, p);
        let cur = l.line_of(p.note_caret);
        let x = m.width(&p.note, l.lines[cur].0, p.note_caret).round() as i32;
        let t = if page {
            let y = l.ys[cur] + rows.signum() as i32 * self.nv.body.h.max(1);
            if rows < 0 && y < 0 { 0 } else { l.line_at_y(y) }
        } else {
            (cur as isize + rows).clamp(0, l.lines.len() as isize - 1) as usize
        };
        let to = if t == cur {
            if rows < 0 { 0 } else { p.note.len() }
        } else {
            col_at(&p.note, l.lines[t], x, &mut m)
        };
        self.note_goto(n, to, extend);
    }

    /// A key in the notepad (clipboard and save shortcuts are handled by the caller).
    fn note_key(&mut self, n: usize, key: &Key, ctrl: bool, alt: bool, shift: bool) {
        match key {
            Key::Named(NamedKey::Escape) => {
                if self.nv.pop {
                    self.nv.pop = false; // the palette first
                } else {
                    self.notes = None;
                }
            }
            Key::Named(NamedKey::Enter) => self.note_type(n, "\n", 0),
            Key::Named(NamedKey::Space) => self.note_type(n, " ", 1),
            Key::Named(NamedKey::Tab) => self.note_type(n, "  ", 0),
            Key::Named(NamedKey::Backspace | NamedKey::Delete) => {
                let back = matches!(key, Key::Named(NamedKey::Backspace));
                let p = &self.panes[n];
                let sel = p.note_sel();
                let (at, old) = match sel {
                    Some((a, b)) => (a, b - a),
                    None if back => {
                        let a = if ctrl { p.note_word(p.note_caret, true) } else { p.note_prev(p.note_caret) };
                        (a, p.note_caret - a)
                    }
                    None => (p.note_caret, if ctrl { p.note_word(p.note_caret, false) } else { p.note_next(p.note_caret) } - p.note_caret),
                };
                self.note_edit(n, at, old, "", if sel.is_some() || ctrl { 0 } else if back { 2 } else { 3 });
            }
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
                let left = matches!(key, Key::Named(NamedKey::ArrowLeft));
                let p = &self.panes[n];
                let to = match p.note_sel() {
                    Some((a, b)) if !shift => if left { a } else { b },
                    _ if ctrl => p.note_word(p.note_caret, left),
                    _ if left => p.note_prev(p.note_caret),
                    _ => p.note_next(p.note_caret),
                };
                self.note_goto(n, to, shift);
            }
            Key::Named(NamedKey::ArrowUp) => self.note_vert(n, -1, false, shift),
            Key::Named(NamedKey::ArrowDown) => self.note_vert(n, 1, false, shift),
            Key::Named(NamedKey::PageUp) => self.note_vert(n, -1, true, shift),
            Key::Named(NamedKey::PageDown) => self.note_vert(n, 1, true, shift),
            Key::Named(NamedKey::Home) => {
                let to = if ctrl { 0 } else { self.note_line_edge(n, false) };
                self.note_goto(n, to, shift);
            }
            Key::Named(NamedKey::End) => {
                let to = if ctrl { self.panes[n].note.len() } else { self.note_line_edge(n, true) };
                self.note_goto(n, to, shift);
            }
            Key::Character(c) if !ctrl && !alt => self.note_type(n, c, 1),
            _ => {}
        }
    }

    /// The byte of the note under the pointer, by the layout of the last frame.
    fn note_pos_at(&mut self, x: i32, y: i32) -> Option<usize> {
        let n = self.notes?;
        let (k, nv, p) = (self.note_k(), &self.nv, self.panes.get(n)?);
        if !nv.layout.valid() {
            return None;
        }
        let line = nv.layout.lines[nv.layout.line_at_y(((y - nv.body.y) as f32 + nv.scroll).floor() as i32)];
        let mut m = Measure::of(&mut self.note_fonts, k, p);
        Some(col_at(&p.note, line, x - nv.body.x, &mut m))
    }

    /// A left click inside the notepad's text or scrollbar. False: it is somewhere else.
    fn note_press(&mut self, x: i32, y: i32) -> bool {
        let Some(n) = self.notes else { return false };
        if self.nv.thumb.w > 0 && self.nv.track.contains(x, y) {
            let th = self.nv.thumb;
            let grab = if y >= th.y && y < th.y + th.h { y - th.y } else { th.h / 2 };
            self.nv.drag = Some(NoteDrag::Thumb(grab));
            self.note_drag(x, y);
            return true;
        }
        if self.nv.body.contains(x, y) {
            if let Some(pos) = self.note_pos_at(x, y) {
                let now = Instant::now();
                let twice = self.nv.click.is_some_and(|(t, at)| now.duration_since(t) < Duration::from_millis(400) && at == (x, y));
                self.nv.click = (!twice).then_some((now, (x, y)));
                if twice {
                    let (a, b) = self.panes[n].note_word_at(pos);
                    self.note_goto(n, a, false);
                    self.panes[n].note_caret = b; // the word is selected
                } else {
                    self.note_goto(n, pos, self.mods.shift_key());
                    self.nv.drag = Some(NoteDrag::Text);
                }
            }
            self.invalidate();
            return true;
        }
        false
    }

    /// The pointer moved with the button down in the notepad: drag the scrollbar thumb or extend the selection.
    fn note_drag(&mut self, x: i32, y: i32) {
        let Some(n) = self.notes else { return };
        match self.nv.drag {
            Some(NoteDrag::Thumb(grab)) => {
                let nv = &mut self.nv;
                nv.goal = thumb_frac(y, grab, nv.track.y, nv.track.h, nv.thumb.h) * nv.max;
                nv.scroll = nv.goal;
            }
            Some(NoteDrag::Text) => {
                let nv = &mut self.nv;
                let over = if y < nv.body.y { y - nv.body.y } else { (y - (nv.body.y + nv.body.h)).max(0) };
                if over != 0 {
                    nv.goal = (nv.goal + over as f32 / 2.0).clamp(0.0, nv.max); // dragging past the edge scrolls
                    nv.scroll = nv.goal;
                }
                if let Some(pos) = self.note_pos_at(x, y) {
                    self.panes[n].note_caret = pos;
                }
            }
            None => return,
        }
        self.invalidate();
    }

    /// Starts closing a panel: it fades out, then `tick` removes it. Closing the last panel exits the app.
    /// If a panel selected by `which` has a note that is not saved, opens that notepad with a hint and says so.
    fn guard(&mut self, which: impl Fn(&Pane) -> bool) -> bool {
        let Some(i) = self.panes.iter().position(|p| !p.closing && which(p) && p.note_blocks()) else { return false };
        self.focus_pane(i);
        self.open_notes(i, true);
        (self.menu_open, self.picker, self.smenu) = (false, None, None);
        self.invalidate();
        true
    }

    /// Closing the window ends every shell: not while an unsaved note would go with them.
    fn request_quit(&mut self) {
        if !self.guard(|_| true) {
            self.quit = true;
        }
    }

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
        for (k, dir) in saved.dirs.into_iter().take(SPACE_SIZE).enumerate() {
            self.add_pane(PanelCfg { dir: Some(dir), ..Default::default() }, id);
            let note = saved.notes.get(k).cloned().unwrap_or_default();
            let p = self.panes.last_mut().unwrap();
            let spans = Spans::decode(saved.note_styles.get(k).map_or("", String::as_str), &note);
            let px = saved.note_size.get(k).and_then(|&z| rich::LADDER.iter().copied().find(|&n| n as f32 == z)).map_or(config::NOTE_SIZE_DEFAULT as u16, u16::from);
            let line = saved.note_line.get(k).and_then(|&l| NOTE_LINES.iter().position(|&n| n == l)).unwrap_or(0);
            (p.note_caret, p.note_anchor, p.note_dirty, p.note, p.note_saved) = (note.len(), note.len(), false, note.clone(), note);
            (p.note_spans, p.note_spans_saved, p.note_px, p.note_px_saved, p.note_line, p.note_line_saved) = (spans.clone(), spans, px, px, line, line);
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
        let (mut dirs, mut notes, mut note_styles, mut note_size, mut note_line) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for p in self.panes.iter_mut().filter(|p| p.space == id && !p.closing) {
            if let Some(d) = p.cwd() {
                dirs.push(d.display().to_string());
                notes.push(p.note.clone());
                note_styles.push(p.note_spans.encode());
                note_size.push(p.note_px as f32);
                note_line.push(NOTE_LINES[p.note_line]);
                p.note_saved = p.note.clone(); // saved with the space: it may close now
                (p.note_spans_saved, p.note_px_saved, p.note_line_saved) = (p.note_spans.clone(), p.note_px, p.note_line);
                p.note_dirty = false;
            }
        }
        config::save_space(&mut self.saved, SavedSpace { name, dirs, notes, note_styles, note_size, note_line });
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
        (self.picker, self.search, self.notes) = (None, None, None); // they name a pane by index
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
            if i != self.focus && self.focused {
                self.focus_report(self.focus, false);
                self.focus_report(i, true);
            }
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
                    self.window.request_user_attention();
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
        let [font, size, line, track, accent, window, render] = self.text;
        config::save(&[
            ("theme", format!("{:?}", THEME_NAMES[self.theme].to_lowercase())),
            ("view", format!("{view:?}")),
            ("font", format!("{:?}", text_choices(0)[font])),
            ("font_size", SIZES[size].to_string()),
            ("line_height", LINES[line].to_string()),
            ("letter_spacing", TRACKS[track].to_string()),
            ("accent", format!("{:?}", ACCENTS[accent].0.to_lowercase())),
            ("maximize", format!("{:?}", ["fullscreen", "window"][window])),
            ("render", format!("{:?}", RENDER[render].to_lowercase())),
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
            Action::CloseSpace(id) => {
                if !self.guard(|p| p.space == id) {
                    self.close_space(id)
                }
            }
            Action::Notes(i) => {
                if i >= self.panes.len() || self.panes[i].closing {
                    return;
                }
                if self.notes == Some(i) {
                    self.notes = None;
                } else {
                    self.focus_pane(i);
                    self.open_notes(i, false);
                }
                self.invalidate();
            }
            Action::NoteFmt(cmd) => {
                if let Some(n) = self.notes.filter(|&n| n < self.panes.len()) {
                    self.note_cmd(n, cmd);
                }
                self.invalidate();
            }
            Action::NoteSave(i) => {
                if let Some(space) = self.panes.get(i).map(|p| p.space) {
                    self.save_space(space);
                }
                self.note_hint = false;
                self.invalidate();
            }
            Action::PickDrive(i) | Action::PickDir(i) => {
                let Some(cwd) = self.panes.get(i).and_then(Pane::cwd) else { return };
                self.focus_pane(i);
                (self.menu_open, self.pick_scroll) = (false, 0.0);
                self.picker = Some(if matches!(a, Action::PickDrive(_)) { Picker::drives(i, picker::drive_of(&cwd)) } else { Picker::dirs(i, cwd) });
                self.invalidate();
            }
            Action::Pick(k) => self.pick(k),
            Action::Close(i) => {
                let id = self.panes.get(i).map_or(usize::MAX, |p| p.id);
                if !self.guard(|p| p.id == id) {
                    self.begin_close(i)
                }
            }
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
                (self.font, self.title_font, self.prose) = text_fonts(self.text, self.scale);
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
            Action::Minimize => self.window.set_minimized(),
            Action::ToggleMaximize => {
                if self.maximized() {
                    self.window.set_fullscreen(false);
                    self.window.set_maximized(false);
                } else if self.text[5] == 0 {
                    self.window.set_fullscreen(true); // the monitor the window is on
                } else {
                    self.window.set_maximized(true);
                }
            }
            Action::CloseWindow => self.request_quit(),
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
        record(id, bytes);
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
        if let Some(n) = self.notes {
            // the notepad: typing edits the note; Esc closes it, Ctrl+S saves the space (and so the notes)
            if n >= self.panes.len() {
                self.notes = None;
            } else {
                let ch = if let Key::Character(c) = key { c.to_lowercase() } else { String::new() };
                self.note_hint = false;
                if ctrl && !alt && ch == "s" {
                    self.run(Action::NoteSave(n));
                } else if ctrl && !alt && ch == "v" {
                    if let Some(t) = win::clipboard_get(self.hwnd) {
                        self.note_type(n, &t, 0);
                    }
                } else if ctrl && !alt && (ch == "z" || ch == "y") {
                    self.note_undo(n, ch == "y" || shift);
                } else if ctrl && !alt && matches!(ch.as_str(), "b" | "i" | "u" | "=" | "+" | "-" | "_") {
                    let cmd = match ch.as_str() {
                        "b" => NoteCmd::Style(Flag::Bold),
                        "i" => NoteCmd::Style(Flag::Italic),
                        "u" => NoteCmd::Style(Flag::Underline),
                        "=" | "+" => NoteCmd::SizeUp,
                        _ => NoteCmd::SizeDown,
                    };
                    self.note_cmd(n, cmd);
                } else if ctrl && !alt && ch == "a" {
                    let p = &mut self.panes[n];
                    (p.note_anchor, p.note_caret) = (0, p.note.len());
                } else if ctrl && !alt && (ch == "c" || ch == "x") {
                    if let Some((a, b)) = self.panes[n].note_sel() {
                        win::clipboard_set(self.hwnd, &self.panes[n].note[a..b]);
                        if ch == "x" {
                            self.note_edit(n, a, b - a, "", 0);
                        }
                    }
                } else {
                    self.note_key(n, key, ctrl, alt, shift);
                    self.nv.follow = true;
                }
                return self.invalidate();
            }
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

    /// Tells pane `i`'s program it gained or lost focus, if it asked to know (DECSET 1004).
    fn focus_report(&mut self, i: usize, on: bool) {
        if let Some(p) = self.panes.get_mut(i).filter(|p| p.term.focus_events) {
            let _ = p.writer.write_all(if on { b"[I" } else { b"[O" });
            let _ = p.writer.flush();
        }
    }

    /// The program in pane `i` wants mouse reports, and Shift (which keeps the mouse for maula: select, scrollback)
    /// is not held.
    fn app_mouse(&self, i: usize) -> bool {
        self.panes[i].term.mouse != Mouse::Off && !self.mods.shift_key()
    }

    /// Reports `ev` at (`x`, `y`) to pane `i`'s program; false when it did not ask for that kind of event.
    fn report_mouse(&mut self, i: usize, inner: Rect, (x, y): (i32, i32), ev: MouseEv) -> bool {
        let mods = if self.mods.alt_key() { 8 } else { 0 } + if self.mods.control_key() { 16 } else { 0 };
        let (cw, ch) = (self.font.cw, self.font.ch);
        let p = &mut self.panes[i];
        let col = ((x - inner.x) / cw).clamp(0, p.term.cols as i32 - 1) as usize;
        let row = ((y - inner.y) / ch).clamp(0, p.term.rows as i32 - 1) as usize;
        if matches!(ev, MouseEv::Move(_)) {
            if self.app_cell == Some((i, col, row)) {
                return true; // still the same cell: nothing new to say
            }
            self.app_cell = Some((i, col, row));
        }
        let Some(bytes) = p.term.mouse_report(ev, col, row, mods) else { return false };
        let _ = p.writer.write_all(&bytes);
        let _ = p.writer.flush();
        true
    }

    fn cell_at(&mut self, i: usize, inner: Rect, x: i32, y: i32) -> Pos {
        let p = &self.panes[i];
        let t = &p.term;
        let row = ((y - inner.y) / self.font.ch).clamp(0, t.rows as i32 - 1) as usize;
        let line = (t.history.len() as f32 - p.scroll_pos).round() as usize + row;
        let dx = x - inner.x;
        let xs = if p.software(self.text) { prose_xs(&mut self.prose, self.font.cw, t.line(line), t.cols) } else { None };
        let col = match xs {
            Some(xs) => xs.partition_point(|&e| e <= dx).saturating_sub(1).min(t.cols - 1),
            None => (dx / self.font.cw).clamp(0, t.cols as i32 - 1) as usize,
        };
        (line, col)
    }

    /// The pane whose scrollbar strip is under (`x`, `y`), with the strip and its thumb.
    fn bar_at(&self, x: i32, y: i32) -> Option<(usize, Rect, Rect)> {
        let hh = self.toolbar_h();
        self.slots().into_iter().rev().find_map(|(i, outer, _)| {
            let p = &self.panes[i];
            let (track, thumb) = term_bar(&p.term, p.scroll_pos, outer, hh, self.scale)?;
            (!p.closing && y >= hh && track.contains(x, y)).then_some((i, track, thumb))
        })
    }

    /// Scrolls pane `i` so its thumb sits where the pointer at `y` (holding the thumb `grab` px below its top) puts it.
    fn bar_scroll(&mut self, i: usize, grab: i32, y: i32) {
        let Some((_, outer, _)) = self.slots().into_iter().find(|s| s.0 == i) else { return };
        let (hh, scale) = (self.toolbar_h(), self.scale);
        let p = &mut self.panes[i];
        let Some((track, thumb)) = term_bar(&p.term, p.scroll_pos, outer, hh, scale) else { return };
        let hist = p.term.history.len() as f32;
        let top = thumb_frac(y, grab, track.y, track.h, thumb.h) * hist; // lines above the view: 0 = oldest
        p.scroll_goal = (hist - top).clamp(0.0, hist);
        p.term.scroll = p.scroll_goal.round() as usize;
        p.scroll_pos = p.scroll_goal; // the thumb tracks the pointer, no glide
        self.invalidate_rect(outer);
    }

    fn on_mouse_button(&mut self, button: MouseButton, state: ElementState) {
        let (x, y) = self.mouse;
        if state == ElementState::Released && self.bar_drag.take().is_some() {
            self.on_cursor(x, y); // refresh the hover look
            return;
        }
        if state == ElementState::Pressed && button == MouseButton::Left && !self.modal() && self.notes.is_none() && self.edge_at(x, y).is_none() && !self.hits.iter().any(|(r, _)| r.contains(x, y)) {
            if let Some((i, _, thumb)) = self.bar_at(x, y) {
                // the scrollbar takes the click: it never reaches the program or starts a selection
                self.commit_rename();
                if i != self.focus {
                    self.focus_pane(i);
                }
                let grab = if y >= thumb.y && y < thumb.y + thumb.h { y - thumb.y } else { thumb.h / 2 }; // off the thumb: jump to centre it here
                self.bar_drag = Some((i, grab));
                self.bar_scroll(i, grab, y);
                return;
            }
        }
        let code = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };
        if state == ElementState::Released {
            (self.drag, self.nv.drag) = (None, None);
            if let Some((i, b)) = self.app_drag.filter(|&(_, b)| b == code) {
                self.app_drag = None;
                if let Some((_, _, inner)) = self.slots().into_iter().find(|s| s.0 == i) {
                    self.report_mouse(i, inner, (x, y), MouseEv::Release(b));
                }
            }
            return;
        }
        if button != MouseButton::Right && !self.modal() && self.notes.is_none() && !self.hits.iter().any(|(r, _)| r.contains(x, y)) && self.edge_at(x, y).is_none() {
            // a program that owns the mouse (Claude Code's full-screen view, vim, htop) gets the click; right click
            // stays maula's copy/paste
            if let Some((i, _, inner)) = self.hit_pane(x, y).filter(|&(i, _, inner)| inner.contains(x, y) && self.app_mouse(i)) {
                if i != self.focus {
                    self.focus_pane(i);
                }
                self.commit_rename();
                if self.report_mouse(i, inner, (x, y), MouseEv::Press(code)) {
                    self.app_drag = Some((i, code));
                }
                return;
            }
        }
        if self.notes.is_some() && button != MouseButton::Left {
            return;
        }
        match button {
            MouseButton::Left => {
                if let Some(dir) = self.edge_at(x, y) {
                    let _ = self.window.drag_resize_window(dir);
                    return;
                }
                self.commit_rename(); // a click anywhere else keeps the name typed so far
                if self.notes.is_some() {
                    let hit = self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|h| h.1);
                    if self.nv.pop {
                        // the palette is open: its swatches and button respond, any other click only closes it
                        match hit {
                            Some(a @ Action::NoteFmt(NoteCmd::Highlight(_) | NoteCmd::HlMenu)) => return self.run(a),
                            _ => {
                                self.nv.pop = false;
                                self.invalidate();
                            }
                        }
                    }
                    if self.note_press(x, y) {
                        return;
                    }
                    // the open notepad: its own buttons and the panel buttons respond; a click outside closes it
                    return match hit {
                        Some(a @ (Action::Notes(_) | Action::NoteSave(_) | Action::NoteFmt(_) | Action::Nop)) => self.run(a),
                        _ => {
                            self.notes = None;
                            self.invalidate();
                        }
                    };
                }
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
        if self.notes.is_some() && self.nv.drag.is_some() {
            return self.note_drag(x, y);
        }
        if let Some((i, grab)) = self.bar_drag {
            return self.bar_scroll(i, grab, y); // the pointer may leave the strip: the thumb keeps following it
        }
        if self.notes.is_some() {
            let hot = self.nv.thumb.w > 0 && self.nv.track.contains(x, y);
            if hot != self.nv.hot {
                self.nv.hot = hot;
                self.invalidate();
            }
        }
        let hot = if self.modal() || self.notes.is_some() { None } else { self.bar_at(x, y).map(|b| b.0) };
        if hot != self.bar_hot {
            let old = std::mem::replace(&mut self.bar_hot, hot);
            for i in [old, hot].into_iter().flatten() {
                if let Some((_, outer, _)) = self.slots().into_iter().find(|s| s.0 == i) {
                    self.invalidate_rect(outer);
                }
            }
        }
        let hover = self.hits.iter().position(|(r, _)| r.contains(x, y));
        let over_text = hover.is_none() && self.hit_pane(x, y).is_some_and(|(_, _, inner)| inner.contains(x, y));
        let icon = match self.edge_at(x, y) {
            Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
            Some(ResizeDirection::East | ResizeDirection::West) => CursorIcon::EwResize,
            Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => CursorIcon::NwseResize,
            Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => CursorIcon::NeswResize,
            None if self.notes.is_some() && self.nv.drag.is_some() => CursorIcon::Text,
            None if self.notes.is_some() && hover.is_some_and(|h| matches!(self.hits[h].1, Action::Nop)) => {
                // the notepad card swallows the pointer: I-beam over the text, the arrow elsewhere (scrollbar, bars)
                if self.nv.body.contains(x, y) && !self.nv.track.contains(x, y) { CursorIcon::Text } else { CursorIcon::Default }
            }
            None if hover.is_some_and(|h| matches!(self.hits[h].1, Action::Search)) => CursorIcon::Text,
            None if hover.is_some() => CursorIcon::Pointer,
            None if over_text && !self.hit_pane(x, y).is_some_and(|(i, ..)| self.app_mouse(i)) => CursorIcon::Text,
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
        if moved {
            let target = match self.app_drag {
                Some((i, b)) => self.slots().into_iter().find(|s| s.0 == i).map(|s| (i, s.2, Some(b))),
                None if !self.modal() => self.hit_pane(x, y).filter(|&(i, _, inner)| inner.contains(x, y) && self.app_mouse(i)).map(|(i, _, inner)| (i, inner, None)),
                None => None,
            };
            if let Some((i, inner, b)) = target {
                self.report_mouse(i, inner, (x, y), MouseEv::Move(b));
            }
        }
        let Some(i) = self.drag else { return };
        let Some((_, outer, inner)) = self.slots().into_iter().find(|s| s.0 == i) else { return };
        self.extend_sel(i, inner);
        self.invalidate_rect(outer);
        if !self.animating && edge_speed(y, inner, self.font.ch) != 0.0 {
            self.start_animating(); // past the edge: `tick` keeps scrolling while the pointer rests there
        }
    }

    /// Moves the end of pane `i`'s drag selection to the cell under the pointer.
    fn extend_sel(&mut self, i: usize, inner: Rect) {
        let head = self.cell_at(i, inner, self.mouse.0, self.mouse.1);
        let p = &mut self.panes[i];
        p.sel = if head != p.anchor { Some((p.anchor, head)) } else { None };
    }

    /// Dragging a selection past the top or bottom of the text scrolls the view, so a selection can run beyond what
    /// is on screen. Returns the pane while it can still move.
    fn edge_scroll(&mut self, dt: f32) -> Option<usize> {
        let i = self.drag?;
        let (_, _, inner) = self.slots().into_iter().find(|s| s.0 == i)?;
        let speed = edge_speed(self.mouse.1, inner, self.font.ch);
        let p = &mut self.panes[i];
        let max = p.term.history.len() as f32;
        if speed == 0.0 || (speed > 0.0 && p.scroll_goal >= max) || (speed < 0.0 && p.scroll_goal <= 0.0) {
            return None;
        }
        p.scroll_goal = (p.scroll_goal + speed * dt).clamp(0.0, max);
        p.term.scroll = p.scroll_goal.round() as usize;
        p.scroll_pos = p.scroll_goal; // no glide: the selection must follow the text exactly
        self.extend_sel(i, inner);
        Some(i)
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        // `fine`: a touchpad (or free-spinning wheel) sends many small deltas and does its own inertia, so it is
        // applied as-is. A notched wheel sends whole steps, which glide.
        let (dy, fine) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (y * 3.0, x.fract() != 0.0 || y.fract() != 0.0), // dy in lines
        };
        if self.notes.is_some() {
            // the notepad scrolls (smoothly for a notched wheel); the terminal behind stays put. Ctrl+wheel: the font size
            if self.mods.control_key() {
                if let Some(n) = self.notes.filter(|&n| n < self.panes.len()).filter(|_| dy != 0.0) {
                    self.note_cmd(n, if dy > 0.0 { NoteCmd::SizeUp } else { NoteCmd::SizeDown });
                    return self.invalidate();
                }
            }
            let nv = &mut self.nv;
            nv.goal = (nv.goal - dy * nv.lh.max(1) as f32).clamp(0.0, nv.max);
            if fine {
                nv.scroll = nv.goal;
            }
            self.start_animating();
            return self.invalidate();
        }
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
        if let Some((i, _, inner)) = self.hit_pane(self.mouse.0, self.mouse.1).filter(|&(i, ..)| self.app_mouse(i)) {
            // the program scrolls itself: one wheel report per notch, a touchpad's small deltas adding up to one
            self.wheel_acc += dy / 3.0;
            while self.wheel_acc.abs() >= 1.0 {
                let up = self.wheel_acc > 0.0;
                self.report_mouse(i, inner, self.mouse, if up { MouseEv::WheelUp } else { MouseEv::WheelDown });
                self.wheel_acc -= if up { 1.0 } else { -1.0 };
            }
            return;
        }
        self.wheel_acc = 0.0;
        // a drag selection keeps its panel even with the pointer outside it
        let target = match self.drag {
            Some(i) => self.slots().into_iter().find(|s| s.0 == i),
            None => self.hit_pane(self.mouse.0, self.mouse.1),
        };
        if let Some((i, outer, inner)) = target {
            let dragging = self.drag == Some(i);
            let p = &mut self.panes[i];
            // keep the fractional part: small touchpad deltas add up instead of being rounded away
            p.scroll_goal = (p.scroll_goal + dy).clamp(0.0, p.term.history.len() as f32);
            p.term.scroll = p.scroll_goal.round() as usize;
            if fine || dragging {
                p.scroll_pos = p.scroll_goal; // follow the fingers exactly; a selection must follow the text
            }
            if dragging {
                self.extend_sel(i, inner); // the wheel carries a selection past the screen
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

        if self.notes.is_some() {
            let nv = &mut self.nv;
            nv.goal = nv.goal.clamp(0.0, nv.max);
            let next = chase(nv.scroll, nv.goal, dt, SCROLL_TAU).clamp(0.0, nv.max);
            busy_full |= next != nv.scroll;
            nv.scroll = next;
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
        moving.extend(self.edge_scroll(dt));
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
        let (bar_hot, bar_drag) = (self.bar_hot, self.bar_drag.map(|d| d.0));
        let Ui { hwnd, frame, font, prose, ui_font: ui, title_font: titlef, close_font: closef, icon_font: icons, panes, hits, focus, view, menu_open, menu_sel, sub, sub_sel, text, picker, fades, themes, theme, spaces, rename, search, notes, nv, note_hint, note_fonts, .. } = self;
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
            if !p.note.trim().is_empty() {
                // a note is kept here: green once saved, accent while it is not
                let d = px(5.0, scale);
                cv.rect(Rect { x: head.notes.x + head.notes.w / 2 + px(4.0, scale), y: head.notes.y + head.notes.h / 2 - px(9.0, scale), w: d, h: d }, if !p.note_dirty { pt.green } else { pt.accent });
            }
            let first = hits.len();
            if !p.closing {
                // before the header itself, so they win the click
                hits.push((head.close, Action::Close(*idx)));
                hits.push((head.memory, Action::Memory(*idx)));
                hits.push((head.notes, Action::Notes(*idx)));
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
            let software = p.software(*text).then_some(&mut *prose);
            draw_term(&mut cv, font, software, &p.term, p.scroll_pos, p.sel, &pt, *inner, on, dim);
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
            draw_scrollbar(&mut cv, &p.term, p.scroll_pos, &pt, *outer, hh, scale, bar_hot == Some(*idx) || bar_drag == Some(*idx));
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

        if let Some(n) = *notes {
            if let (Some((_, outer, _)), Some(p)) = (slots.iter().find(|s| s.0 == n), panes.get(n)) {
                cv.set_clip(outer.intersect(&content));
                draw_notes(&mut cv, note_fonts, SIZES[text[1]] / 100.0 * scale, ui, icons, &th, scale, *outer, &title(p), p, n, nv, *note_hint, fades, hits);
                cv.set_clip(Rect { x: 0, y: 0, w, h });
            }
        }
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

/// Whether a row reads as running text, so the Software render may draw it in a proportional font: at least three
/// words, and nothing that relies on columns lining up (aligned gaps, box drawing, a background such as a diff's or
/// the input cursor's) or looks like code (its punctuation, or syntax colours).
/// ponytail: a guess from one row: an uncoloured line of code without that punctuation reads as text. Claude's output
/// is not rewrapped either, so rows keep Claude's line breaks. Upgrade: parse the transcript instead of the screen.
fn is_prose(line: &[Cell]) -> bool {
    let (Some(start), Some(end)) = (line.iter().position(|c| c.ch != ' '), line.iter().rposition(|c| c.ch != ' ')) else { return false };
    let body = &line[start..=end];
    let mut colors = Vec::new();
    for (i, c) in body.iter().enumerate() {
        // `❯`: Claude's input row, which would jump off the grid while you type its third word
        if c.bg != Color::Bg || matches!(c.ch, '\u{2500}'..='\u{259f}' | '{' | '}' | '|' | '=' | '❯') || (c.ch == ' ' && body[i + 1].ch == ' ') {
            return false;
        }
        if c.ch.is_alphabetic() && !colors.contains(&c.fg) {
            colors.push(c.fg);
        }
    }
    colors.len() <= 2 && body.split(|c| c.ch == ' ').filter(|w| w.iter().any(|c| c.ch.is_alphabetic())).count() >= 3
}

/// The left edge in pixels of each column (and of the end of the last) when `line` is drawn proportionally, or None
/// when it stays on the grid: not running text, or too wide. Indent and the blanks after the text keep the grid's `cw`.
fn prose_xs(prose: &mut Font, cw: i32, line: &[Cell], cols: usize) -> Option<Vec<i32>> {
    if !is_prose(line) {
        return None;
    }
    let (start, end) = (line.iter().position(|c| c.ch != ' ')?, line.iter().rposition(|c| c.ch != ' ')?);
    let (mut xs, mut x) = (Vec::with_capacity(cols + 1), 0.0);
    for col in 0..=cols {
        xs.push(f32::round(x) as i32);
        x += match line.get(col) {
            Some(c) if (start..=end).contains(&col) => if c.ch == '\0' { 0.0 } else { prose.advance(c.ch) },
            _ => cw as f32,
        };
    }
    (*xs.get(end + 1)? <= cols as i32 * cw).then_some(xs)
}

/// `dim` (0..1) greys the text toward the background: used for inactive terminals. `prose`: the Software render's font.
#[allow(clippy::too_many_arguments)]
fn draw_term(cv: &mut Canvas, font: &mut Font, mut prose: Option<&mut Font>, t: &Term, scroll_pos: f32, sel: Option<(Pos, Pos)>, th: &Theme, r: Rect, focused: bool, dim: f32) {
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
        let xs = prose.as_deref_mut().and_then(|f| prose_xs(f, font.cw, line, t.cols));
        let cols = if xs.is_some() { 0..t.cols } else { first.min(t.cols)..last.min(t.cols) };
        for col in cols {
            let cell = line.get(col).copied().unwrap_or(BLANK);
            let (x, w) = xs.as_ref().map_or((r.x + col as i32 * font.cw, font.cw), |v| (r.x + v[col], v[col + 1] - v[col]));
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
                cv.rect(Rect { x, y, w, h: font.ch }, bg);
            }
            if cell.ch != ' ' && cell.ch != '\0' {
                match prose.as_deref_mut().filter(|_| xs.is_some()) {
                    Some(f) => cv.glyph(f, cell.ch, x, y, fg),
                    None => cv.glyph(font, cell.ch, x, y, fg),
                }
            }
        }
    }
}

/// How fast a drag selection scrolls with the pointer at height `y`, in lines per second: 0 inside the text, positive
/// (back through history) above it, negative below it, faster the further out.
fn edge_speed(y: i32, inner: Rect, line_h: i32) -> f32 {
    let (over, sign) = if y < inner.y {
        (inner.y - y, 1.0)
    } else if y >= inner.y + inner.h {
        (y - (inner.y + inner.h) + 1, -1.0)
    } else {
        return 0.0;
    };
    sign * (8.0 + 16.0 * over as f32 / line_h as f32).min(200.0)
}

/// Thin scrollbar on the panel's outer right edge, shown once there is scrollback. Its track starts below the
/// panel header and stops above the bottom border, so it never overlaps either.
fn draw_scrollbar(cv: &mut Canvas, t: &Term, scroll_pos: f32, th: &Theme, outer: Rect, header_h: i32, scale: f32, hot: bool) {
    let Some((_, thumb)) = term_bar(t, scroll_pos, outer, header_h, scale) else { return };
    let thick = if hot { px(7.0, scale) } else { px(4.0, scale) }; // widens under the pointer
    let r = Rect { x: outer.x + outer.w - px(3.0, scale) - thick, y: thumb.y, w: thick, h: thumb.h };
    cv.rrect(r, thick / 2, th.sub, if hot { 200 } else { 120 });
}

/// The hit area of a pane's scrollbar (a strip at its right edge, wider than the drawn line) and its thumb at the
/// displayed scroll position. None while there is nothing to scroll or the program scrolls itself.
fn term_bar(t: &Term, scroll_pos: f32, outer: Rect, header_h: i32, scale: f32) -> Option<(Rect, Rect)> {
    if t.history.is_empty() || t.mouse != Mouse::Off {
        return None;
    }
    let hist = t.history.len() as f32;
    let top = hist - scroll_pos.clamp(0.0, hist); // lines above the view: 0 = oldest line
    let w = px(12.0, scale);
    let track = Rect { x: outer.x + outer.w - w, y: outer.y + header_h + 1, w, h: outer.h - header_h - 3 };
    let len = thumb_len(track.h, t.rows as i32, t.total_lines() as i32, px(14.0, scale));
    let y = track.y + thumb_top(track.h, len, top / hist);
    Some((track, Rect { x: track.x, y, w, h: len }))
}

/// Scrollbar math shared by the terminal's and the notepad's bars. Thumb height: the visible share of the track.
fn thumb_len(track: i32, visible: i32, total: i32, min: i32) -> i32 {
    (track as i64 * visible as i64 / total.max(1) as i64).clamp(min.min(track).max(0) as i64, track.max(0) as i64) as i32
}

/// Thumb offset from the track's start for a scroll fraction 0..1 (0 = start of the content).
fn thumb_top(track: i32, thumb: i32, frac: f32) -> i32 {
    ((track - thumb).max(0) as f32 * frac.clamp(0.0, 1.0)).round() as i32
}

/// Scroll fraction 0..1 for a pointer at `pointer` holding the thumb `grab` px below its top.
fn thumb_frac(pointer: i32, grab: i32, track_start: i32, track: i32, thumb: i32) -> f32 {
    ((pointer - grab - track_start) as f32 / (track - thumb).max(1) as f32).clamp(0.0, 1.0)
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
    notes: Rect,
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
    let notes = cell("", px(11.0, scale), mem.x - 1);
    let (pw, ph) = (px(11.0, scale), px(14.0, scale));
    let ink = mix(th.sub, th.ui_fg, fade_of(fades, notes));
    let (x0, y0, t) = (notes.x + (notes.w - pw) / 2, hr.y + (hr.h - ph) / 2, px(1.0, scale).max(1));
    for r in [Rect { x: x0, y: y0, w: pw, h: t }, Rect { x: x0, y: y0 + ph - t, w: pw, h: t }, Rect { x: x0, y: y0, w: t, h: ph }, Rect { x: x0 + pw - t, y: y0, w: t, h: ph }] {
        cv.rect(r, ink); // a small page
    }
    for k in 0..3 {
        cv.rect(Rect { x: x0 + px(3.0, scale), y: y0 + px(4.0, scale) + k * px(3.0, scale), w: pw - 2 * px(3.0, scale), h: t }, ink); // its lines
    }
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
    let room = notes.x - 1 - x - pad - px(TITLE_GAP, scale) - px(ICON, scale);
    let label = fit(titlef, title, room, track);
    let dir = chip(cv, titlef, icons, th, scale, hr, x, &label, TITLE_GAP, fades);
    cv.rect(Rect { x: hr.x, y: hr.y + hr.h - 1, w: hr.w + 1, h: 1 }, th.border);
    PanelHits { close, memory: mem, notes, drive, dir }
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
fn draw_menu(cv: &mut Canvas, ui: &mut Font, icons: &mut Font, c: &Chrome, text: [usize; 7], sub: Option<(usize, Option<usize>)>, hits: &mut Vec<(Rect, Action)>) {
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

/// How much text the undo steps of one note may hold before the oldest are forgotten.
const NOTE_UNDO_CAP: usize = 32 << 20;

/// One edit of the note, as a delta: `removed` was at `at` and `inserted` took its place.
struct NoteEdit {
    at: usize,
    removed: String,
    inserted: String,
    removed_sty: Vec<Span>, // styles of `removed` and `inserted`, relative to `at`
    inserted_sty: Vec<Span>,
    caret: usize, // caret and selection anchor before the edit
    anchor: usize,
    kind: u8,
}

/// Which part of the notepad a held mouse button is dragging.
#[derive(Clone, Copy)]
enum NoteDrag {
    Thumb(i32), // the scrollbar thumb, grabbed this many pixels below its top
    Text,       // a selection
}

/// What the notepad needs between frames: the wrapped layout, the scroll position and where things were drawn.
#[derive(Default)]
struct NoteView {
    layout: NoteLayout,
    scroll: f32, // displayed scroll, in pixels; chases `goal`
    goal: f32,
    max: f32,     // the largest scroll
    follow: bool, // the next frame scrolls the caret into view
    lh: i32,      // line height
    body: Rect,   // the text area
    track: Rect,  // the scrollbar's hit area
    thumb: Rect,  // the thumb (w == 0: nothing to scroll)
    drag: Option<NoteDrag>,
    hot: bool, // the pointer is over the scrollbar
    click: Option<(Instant, (i32, i32))>, // the last click, for double-click
    pop: bool, // the highlight palette is open
}

impl NoteView {
    fn reset(&mut self) {
        self.layout.width = 0; // laid out again by the next frame
        (self.scroll, self.goal, self.follow, self.drag, self.pop) = (0.0, 0.0, true, None, false);
    }
}

/// The line heights of the notepad, times the font's own line height. The first is the default.
const NOTE_LINES: [f32; 3] = [1.0, 1.2, 1.5];
/// The highlight palette (entry n is style `hl` n + 1): soft, faded colours. Text on them is always black.
const NOTE_HL: [u32; 7] = [0xfff2a8, 0xbff0c0, 0xb3ecf5, 0xffc2dc, 0xffd3a1, 0xd9ccff, 0xbcd8ff];

/// A formatting command of the notepad: a toolbar button or a shortcut.
#[derive(Clone, Copy, PartialEq, Debug)]
enum NoteCmd {
    Style(Flag),
    Highlight(u8), // 0: none
    HlMenu,        // open or close the palette
    SizeDown,
    SizeUp,
    Line, // next line height
}

/// Segoe UI in every size and style the notes use, loaded when first needed.
#[derive(Default)]
struct NoteFonts {
    map: HashMap<(u32, u8), Font>,
}

impl NoteFonts {
    /// The font for `px` and style `key` (see `Style::key`). Every style of one size shares the regular face's baseline.
    fn get(&mut self, px: f32, key: u8) -> &mut Font {
        let id = ((px * 8.0) as u32, key);
        if !self.map.contains_key(&id) {
            if self.map.len() > 48 {
                self.map.clear(); // the sizes tried so far are not coming back
            }
            let ascent = if key == 0 { None } else { Some(self.get(px, 0).ascent) };
            let mut f = Font::note(px, key & 1 != 0, key & 2 != 0);
            if let Some(a) = ascent {
                f.ascent = a;
            }
            self.map.insert(id, f);
        }
        self.map.get_mut(&id).unwrap()
    }
}

/// What it takes to measure the note's text: the fonts, the scale, and which bytes are bold, italic or sized.
struct Measure<'a> {
    fonts: &'a mut NoteFonts,
    k: f32,    // pixels per point
    def: u8,   // the size of unsized text, in points
    line: f32, // the line height, times the font's own
    spans: &'a Spans,
}

impl<'a> Measure<'a> {
    fn of(fonts: &'a mut NoteFonts, k: f32, doc: &'a NoteDoc) -> Self {
        Measure { fonts, k, def: doc.note_px as u8, line: NOTE_LINES[doc.note_line], spans: &doc.note_spans }
    }

    /// The size in points a style draws at.
    fn eff(&self, st: Style) -> u8 {
        if st.size == 0 { self.def } else { st.size }
    }

    fn font(&mut self, st: Style) -> &mut Font {
        let px = self.eff(st) as f32 * self.k;
        self.fonts.get(px, st.key())
    }

    /// The natural height and ascent of text of `size` points.
    fn metrics(&mut self, size: u8) -> (i32, i32) {
        let f = self.fonts.get(size as f32 * self.k, 0);
        (f.ch, f.ascent)
    }

    /// The height of a line whose largest text is `size` points.
    fn lh(&mut self, size: u8) -> i32 {
        line_height(self.metrics(size).0, self.line)
    }

    /// Width of `text[a..b]` (the note's bytes), each run in its own font.
    fn width(&mut self, text: &str, a: usize, b: usize) -> f32 {
        let mut w = 0.0;
        for (s, e, st) in self.spans.segments(a, b) {
            let f = self.font(st);
            w += text[s..e].chars().map(|c| f.advance(c)).sum::<f32>();
        }
        w
    }
}

/// A line's height: the natural height of its largest font times the note's line height setting.
fn line_height(ch: i32, mult: f32) -> i32 {
    ((ch as f32 * mult).round() as i32).max(1)
}

/// The note wrapped to a pixel width: one entry per shown line, as a byte range and whether it ends its text line.
/// Each line has its own height (its largest font) and the top y of every line is kept as a running sum.
/// Kept between frames and patched per edit, so neither typing nor drawing walks the whole note.
#[derive(Default)]
struct NoteLayout {
    lines: Vec<(usize, usize, bool)>,
    hs: Vec<i32>, // height of each line
    ys: Vec<i32>, // top of each line; one more entry: the total height
    width: i32,   // 0: not laid out
    ch: i32,
}

impl NoteLayout {
    fn valid(&self) -> bool {
        self.width > 0 && !self.lines.is_empty()
    }

    fn fits(&self, width: i32, ch: i32) -> bool {
        self.valid() && self.width == width && self.ch == ch
    }

    fn rebuild(&mut self, text: &str, width: i32, ch: i32, m: &mut Measure) {
        self.lines.clear();
        self.hs.clear();
        wrap_into(text, 0, text.len(), width, m, &mut self.lines, &mut self.hs);
        (self.width, self.ch) = (width.max(1), ch);
        self.sum_from(0);
    }

    /// Recomputes the tops of lines `i..`.
    fn sum_from(&mut self, i: usize) {
        self.ys.truncate(i);
        let mut y = if i == 0 { 0 } else { self.ys[i - 1] + self.hs[i - 1] };
        self.ys.reserve(self.hs.len() + 1 - i);
        for &h in &self.hs[i..] {
            self.ys.push(y);
            y += h;
        }
        self.ys.push(y);
    }

    /// The height of all lines together.
    fn total(&self) -> i32 {
        self.ys.last().copied().unwrap_or(0)
    }

    /// The shown line at height `y` (clamped to the first and last).
    fn line_at_y(&self, y: i32) -> usize {
        self.ys[..self.lines.len()].partition_point(|&t| t <= y).saturating_sub(1)
    }

    /// The shown line holding byte `off` (a wrapped line's end belongs to the next line).
    fn line_of(&self, off: usize) -> usize {
        self.lines.partition_point(|l| l.0 <= off).saturating_sub(1)
    }

    /// `text` is the note after `old` bytes at `at` became `new` bytes: wraps just the text lines this touched.
    fn edit(&mut self, text: &str, at: usize, old: usize, new: usize, m: &mut Measure) {
        let mut i0 = self.line_of(at);
        while i0 > 0 && !self.lines[i0 - 1].2 {
            i0 -= 1;
        }
        let mut i1 = self.line_of(at + old);
        while !self.lines[i1].2 {
            i1 += 1;
        }
        let delta = new as isize - old as isize;
        let from = self.lines[i0].0;
        let to = (self.lines[i1].1 as isize + delta) as usize;
        let (mut fresh, mut fresh_h) = (Vec::new(), Vec::new());
        wrap_into(text, from, to, self.width, m, &mut fresh, &mut fresh_h);
        for l in &mut self.lines[i1 + 1..] {
            l.0 = (l.0 as isize + delta) as usize;
            l.1 = (l.1 as isize + delta) as usize;
        }
        self.lines.splice(i0..=i1, fresh);
        self.hs.splice(i0..=i1, fresh_h);
        self.sum_from(i0);
    }
}

/// Word-wraps `text[base..end]` (bytes of the note) to `width` pixels, appending its shown lines to `out` and their
/// heights to `hs`. Each run is measured in its own font; a line is as high as its largest one.
fn wrap_into(text: &str, base: usize, end: usize, width: i32, m: &mut Measure, out: &mut Vec<(usize, usize, bool)>, hs: &mut Vec<i32>) {
    let w = width as f32;
    // ASCII advances per (size, font style), so the common case skips the glyph cache
    let mut tables: Vec<(u8, [[f32; 128]; 4])> = Vec::new();
    let mut ti = usize::MAX;
    let mut heights: Vec<(u8, i32)> = Vec::new(); // line height per size
    let mut height = |m: &mut Measure, sz: u8| match heights.iter().find(|h| h.0 == sz) {
        Some(h) => h.1,
        None => {
            let h = m.lh(sz);
            heights.push((sz, h));
            h
        }
    };
    let spans = m.spans;
    let mut cur = spans.cursor(base);
    let mut start = base;
    for para in text[base..end].split('\n') {
        let (mut from, mut x) = (0usize, 0.0f32);
        let mut brk: Option<(usize, f32)> = None; // after the last space: its byte and x
        // the largest size in the line so far: up to the last break point, and after it
        let (mut run_max, mut pre_max, mut post_max) = (0u8, 0u8, 0u8);
        for (i, c) in para.char_indices() {
            let st = cur.style(start + i);
            let (key, sz) = (st.key(), m.eff(st));
            if ti == usize::MAX || tables[ti].0 != sz {
                ti = match tables.iter().position(|t| t.0 == sz) {
                    Some(t) => t,
                    None => {
                        tables.push((sz, [[-1.0; 128]; 4]));
                        tables.len() - 1
                    }
                };
            }
            let cw = match tables[ti].1[key as usize].get_mut(c as usize) {
                Some(t) => {
                    if *t < 0.0 {
                        *t = m.font(st).advance(c);
                    }
                    *t
                }
                None => m.font(st).advance(c),
            };
            if x + cw > w && i > from {
                let (cut, nx, lmax) = match brk {
                    Some((b, bx)) if b > from => (b, x - bx, pre_max),
                    _ => (i, 0.0, run_max),
                };
                out.push((start + from, start + cut, false));
                hs.push(height(m, lmax));
                (run_max, post_max) = if cut == i { (0, 0) } else { (post_max, 0) };
                (from, x, brk) = (cut, nx, None);
            }
            x += cw;
            run_max = run_max.max(sz);
            post_max = post_max.max(sz);
            if c == ' ' {
                brk = Some((i + 1, x));
                pre_max = run_max;
                post_max = 0;
            }
        }
        let last_max = if para.is_empty() {
            let st = m.spans.empty_line_style(start, text.len());
            m.eff(st)
        } else {
            run_max
        };
        out.push((start + from, start + para.len(), true));
        hs.push(height(m, last_max));
        start += para.len() + 1;
    }
}

/// The byte of shown line `line` nearest to pixel `x` from its left edge.
fn col_at(text: &str, line: (usize, usize, bool), x: i32, m: &mut Measure) -> usize {
    let (a, b, last) = line;
    let mut acc = 0.0f32;
    let spans = m.spans;
    let mut cur = spans.cursor(a);
    for (i, c) in text[a..b].char_indices() {
        let st = cur.style(a + i);
        let adv = m.font(st).advance(c);
        if (x as f32) < acc + adv / 2.0 {
            return a + i;
        }
        acc += adv;
    }
    if !last && b > a {
        text[..b].char_indices().next_back().map_or(a, |(i, _)| i) // the end of a wrapped line is the start of the next
    } else {
        b
    }
}

/// Pasted or typed text for the note: line ends become \n, tabs two spaces, other control characters go.
fn note_clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\t' => out.push_str("  "),
            c if c == '\n' || !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}

/// A toolbar button's background: the accent while its state is on, a faint wash while the pointer is over it.
fn tool_bg(cv: &mut Canvas, r: Rect, on: bool, fade: f32, th: &Theme, s: f32) {
    if on {
        cv.rrect(r, px(5.0, s), th.accent, 70 + (fade * 50.0) as u32);
    } else if fade > 0.0 {
        cv.rrect(r, px(5.0, s), th.ui_fg, (fade * 30.0) as u32);
    }
}

/// `text` in the middle of `r`.
fn centered(cv: &mut Canvas, font: &mut Font, text: &str, r: Rect, color: u32) {
    let (x, y) = (r.x + (r.w - font.width(text)) / 2, r.y + (r.h - font.ch) / 2);
    cv.text(font, text, x, y, color);
}

/// The x of byte `pos` in a shown line whose runs start at the x of `marks` (start, end, style, x).
fn mark_x(marks: &[(usize, usize, Style, f32)], text: &str, m: &mut Measure, pos: usize) -> f32 {
    match marks.iter().rev().find(|mk| mk.0 <= pos) {
        Some(&(a, b, st, x0)) => {
            let f = m.font(st);
            x0 + text[a..pos.min(b)].chars().map(|c| f.advance(c)).sum::<f32>()
        }
        None => 0.0,
    }
}

/// The notepad over pane `n` (`outer` is the pane): 80% of its width and height, centred. Title bar with the close
/// button, a toolbar (bold, italic, underline, size, highlight, line height), the wrapped styled text with its caret,
/// and a footer with the saved state and a Save button. `k` scales the note's point sizes like the terminal's text.
#[allow(clippy::too_many_arguments)]
fn draw_notes(cv: &mut Canvas, fonts: &mut NoteFonts, k: f32, ui: &mut Font, icons: &mut Font, th: &Theme, s: f32, outer: Rect, name: &str, p: &Pane, n: usize, nv: &mut NoteView, hint: bool, fades: &HashMap<RectKey, Tween>, hits: &mut Vec<(Rect, Action)>) {
    let (bar, inset) = (px(32.0, s), px(12.0, s));
    let (w, h) = (outer.w * 4 / 5, outer.h * 4 / 5);
    let r = Rect { x: outer.x + (outer.w - w) / 2, y: outer.y + (outer.h - h) / 2, w, h };
    // the card is brighter than the terminal: white in light themes, the surface lifted toward white otherwise
    let lum = |c: u32| ((c >> 16 & 0xff) * 299 + (c >> 8 & 0xff) * 587 + (c & 0xff) * 114) / 1000;
    let paper = if lum(th.bg) >= 128 { 0xffffff } else { mix(th.bg, 0xffffff, 0.10) };
    cv.card(r, RADIUS, mix(th.border, th.ui_fg, 0.30), paper);
    let mut local = Vec::new();
    let mut pop_hits = Vec::new(); // the palette sits on top: its hits come first
    let (mut pal, mut cells): (Option<Rect>, Vec<Rect>) = (None, Vec::new());
    // title bar
    let x = Rect { x: r.x + r.w - 1 - bar, y: r.y + 1, w: bar, h: bar };
    let f = fade_of(fades, x);
    if f > 0.0 {
        let g = px(4.0, s);
        cv.rrect(Rect { x: x.x + g, y: x.y + g, w: x.w - 2 * g, h: x.h - 2 * g }, px(5.0, s), mix(paper, 0xe81123, f), 255);
    }
    icon(cv, icons, Icon::Close, x, mix(th.sub, 0xffffff, f));
    local.push((x, Action::Notes(n)));
    let (old, ty) = (cv.clip(), r.y + 1 + (bar - ui.ch) / 2);
    cv.set_clip(Rect { x: r.x + 1, y: r.y, w: r.w - 2 - bar, h: bar }.intersect(&old));
    cv.text(ui, &format!("Notes - {name}"), r.x + 1 + inset, ty, th.ui_fg);
    cv.set_clip(old);
    cv.rect(Rect { x: r.x + 1, y: r.y + 1 + bar, w: r.w - 2, h: 1 }, th.border);
    // toolbar: the same paper as the title bar, under it
    let tbh = px(34.0, s);
    let tb = Rect { x: r.x + 1, y: r.y + 2 + bar, w: r.w - 2, h: tbh };
    let top = tb.y + tbh + 1; // the editing area starts here
    cv.rect(Rect { x: tb.x, y: tb.y + tbh, w: tb.w, h: 1 }, th.border);
    let st = p.note_style_now();
    {
        let (bh, by, pad) = (tbh - 2 * px(5.0, s), tb.y + px(5.0, s), px(8.0, s));
        let mut bx = tb.x + inset;
        let slot = |bx: &mut i32, w: i32, gap: i32| {
            let rect = Rect { x: *bx, y: by, w, h: bh };
            *bx += w + gap;
            rect
        };
        let lpx = UI_PX * s * 1.1;
        let label_color = |on: bool, f: f32| if on { th.ui_fg } else { mix(th.sub, th.ui_fg, f) };
        let gap = px(2.0, s);
        let sep = |cv: &mut Canvas, rect: Rect| {
            cv.rect(Rect { x: rect.x + rect.w + px(5.0, s), y: by + px(4.0, s), w: 1, h: bh - px(8.0, s) }, th.border);
        };
        for (flag, key, glyph) in [(Flag::Bold, 1u8, "B"), (Flag::Italic, 2, "I"), (Flag::Underline, 0, "U")] {
            let rect = slot(&mut bx, bh, gap);
            let (on, f) = (flag.get(&st), fade_of(fades, rect));
            tool_bg(cv, rect, on, f, th, s);
            let font = fonts.get(lpx, key);
            let (gw, gy) = (font.width(glyph), rect.y + (rect.h - font.ch) / 2);
            let gx = rect.x + (rect.w - gw) / 2;
            let col = label_color(on, f);
            cv.text(font, glyph, gx, gy, col);
            if font.fake_bold {
                cv.text(font, glyph, gx + 1, gy, col);
            }
            if flag == Flag::Underline {
                cv.rect(Rect { x: gx, y: gy + font.ascent + px(2.0, s), w: gw, h: px(1.0, s).max(1) }, col);
            }
            local.push((rect, Action::NoteFmt(NoteCmd::Style(flag))));
        }
        let last = Rect { x: bx - gap, y: by, w: 0, h: bh };
        sep(cv, last);
        bx += px(12.0, s);
        let label = |cv: &mut Canvas, ui: &mut Font, local: &mut Vec<(Rect, Action)>, text: &str, rect: Rect, cmd: Option<NoteCmd>| {
            let f = if cmd.is_some() { fade_of(fades, rect) } else { 0.0 };
            if cmd.is_some() {
                tool_bg(cv, rect, false, f, th, s);
            }
            centered(cv, ui, text, rect, label_color(cmd.is_none(), f));
            if let Some(c) = cmd {
                local.push((rect, Action::NoteFmt(c)));
            }
        };
        let down = slot(&mut bx, ui.width("A-") + 2 * pad, 0);
        label(cv, ui, &mut local, "A-", down, Some(NoteCmd::SizeDown));
        let size = slot(&mut bx, ui.width("00") + pad, 0);
        label(cv, ui, &mut local, &p.note_size_now().map_or("-".to_string(), |z| z.to_string()), size, None);
        let up = slot(&mut bx, ui.width("A+") + 2 * pad, 0);
        label(cv, ui, &mut local, "A+", up, Some(NoteCmd::SizeUp));
        sep(cv, Rect { x: bx - gap, ..up });
        bx += px(12.0, s);
        // highlight: an "A" on the colour in use (yellow until one is chosen); opens the palette
        let hlr = slot(&mut bx, bh + px(6.0, s), 0);
        let (f, on) = (fade_of(fades, hlr), nv.pop);
        tool_bg(cv, hlr, on, f, th, s);
        let swatch = Rect { x: hlr.x + px(5.0, s), y: hlr.y + px(5.0, s), w: hlr.w - 2 * px(5.0, s), h: hlr.h - 2 * px(5.0, s) };
        let shown = NOTE_HL.get((st.hl as usize).wrapping_sub(1)).copied().unwrap_or(NOTE_HL[0]);
        cv.rrect(swatch, px(3.0, s), shown, 255);
        centered(cv, ui, "A", swatch, 0x000000);
        local.push((hlr, Action::NoteFmt(NoteCmd::HlMenu)));
        sep(cv, hlr);
        bx += px(12.0, s);
        let line = slot(&mut bx, ui.width("Line 1.0") + 2 * pad, 0);
        label(cv, ui, &mut local, &format!("Line {:.1}", NOTE_LINES[p.note_line]), line, Some(NoteCmd::Line));
        // the palette: "none" and the colours, in a row under the button
        if nv.pop {
            let (cell, g, pd) = (px(24.0, s), px(4.0, s), px(6.0, s));
            let n_cells = NOTE_HL.len() + 1;
            let pr = Rect { x: hlr.x, y: tb.y + tbh + px(3.0, s), w: n_cells as i32 * cell + (n_cells as i32 - 1) * g + 2 * pd, h: cell + 2 * pd };
            let pr = Rect { x: pr.x.min(r.x + r.w - 1 - pr.w).max(r.x + 1), ..pr };
            for i in 0..n_cells {
                let c = Rect { x: pr.x + pd + i as i32 * (cell + g), y: pr.y + pd, w: cell, h: cell };
                pop_hits.push((c, Action::NoteFmt(NoteCmd::Highlight(i as u8))));
                cells.push(c);
            }
            pop_hits.push((pr, Action::Nop)); // gaps and padding of the palette swallow the click
            pal = Some(pr);
        }
    }
    // footer
    let fy = r.y + r.h - 1 - bar;
    cv.rect(Rect { x: r.x + 1, y: fy - 1, w: r.w - 2, h: 1 }, th.border);
    // the editing area is darker than the (bright) title bar and footer
    let dark_bg = if lum(th.bg) >= 128 { mix(th.bg, 0xffffff, 0.5) } else { mix(th.bg, 0xffffff, 0.05) };
    cv.rect(Rect { x: r.x + 1, y: top, w: r.w - 2, h: fy - 1 - top }, dark_bg);
    let save = Rect { x: r.x + r.w - 1 - inset - ui.width("Save") - inset, y: fy, w: ui.width("Save") + 2 * inset, h: bar };
    cv.text(ui, "Save", save.x + inset, fy + (bar - ui.ch) / 2, th.accent);
    local.push((save, Action::NoteSave(n)));
    let (msg, color) = if hint {
        ("Note not saved - save the space or clear the note", th.accent)
    } else if p.note.trim().is_empty() {
        ("Empty", th.sub)
    } else if !p.note_dirty {
        ("Saved with the space", th.green)
    } else {
        ("Not saved - Save (Ctrl+S) keeps it with the space", th.sub)
    };
    cv.set_clip(Rect { x: r.x + 1, y: fy, w: save.x - r.x - 1, h: bar }.intersect(&old));
    cv.text(ui, msg, r.x + 1 + inset, fy + (bar - ui.ch) / 2, color);
    // text: proportional, wrapped to the width by pixels (cached), only the lines in view are drawn
    let pad = px(20.0, s);
    let body = Rect { x: r.x + 1 + pad, y: top + pad, w: r.w - 2 - 2 * pad, h: fy - 1 - top - 2 * pad };
    cv.set_clip(body.intersect(&old));
    let mut m = Measure::of(fonts, k, p);
    let lh = m.lh(m.def);
    if !nv.layout.fits(body.w, lh) {
        nv.layout.rebuild(&p.note, body.w, lh, &mut m);
    }
    let count = nv.layout.lines.len();
    let total = nv.layout.total();
    let max = (total - body.h).max(0) as f32;
    let at = nv.layout.line_of(p.note_caret);
    nv.goal = nv.goal.clamp(0.0, max);
    if nv.follow {
        nv.follow = false;
        let (top_y, bottom) = (nv.layout.ys[at] as f32, (nv.layout.ys[at + 1] - body.h) as f32);
        nv.goal = nv.goal.min(top_y).max(bottom).clamp(0.0, max);
        nv.scroll = nv.goal; // the caret is never left behind by a glide
    }
    nv.scroll = nv.scroll.clamp(0.0, max);
    (nv.max, nv.lh, nv.body) = (max, lh, body);
    let off = nv.scroll.round() as i32;
    let sel = p.note_sel();
    let first = nv.layout.line_at_y(off);
    let mut end = first;
    while end < count && nv.layout.ys[end] < off + body.h {
        end += 1;
    }
    let caret_sty = p.note_caret_style();
    for kline in first..end {
        let (a, b, last) = nv.layout.lines[kline];
        let lh = nv.layout.hs[kline];
        let y = body.y + nv.layout.ys[kline] - off;
        // the line as runs of one style, with the x each starts at
        let mut marks: Vec<(usize, usize, Style, f32)> = Vec::new();
        let mut pen = 0.0f32;
        for (sa, sb, sty) in p.note_spans.segments(a, b) {
            let f = m.font(sty);
            marks.push((sa, sb, sty, pen));
            pen += p.note[sa..sb].chars().map(|c| f.advance(c)).sum::<f32>();
        }
        // runs of different sizes share one baseline: that of the largest
        let big = marks.iter().map(|mk| m.eff(mk.2)).max().unwrap_or_else(|| m.eff(p.note_spans.empty_line_style(a, p.note.len())));
        let (big_ch, big_asc) = m.metrics(big);
        let base = y + (lh - big_ch) / 2 + big_asc;
        let end_x = |i: usize| marks.get(i + 1).map_or(pen, |mk| mk.3);
        for (i, mk) in marks.iter().enumerate() {
            if let Some(&c) = NOTE_HL.get((mk.2.hl as usize).wrapping_sub(1)) {
                let (x0, x1) = (body.x + mk.3.round() as i32, body.x + end_x(i).round() as i32);
                cv.rect(Rect { x: x0, y, w: x1 - x0, h: lh }, c);
            }
        }
        if let Some((sa, sb)) = sel {
            let (lo, hi) = (sa.max(a), sb.min(b));
            let brk = last && sb > b && sa <= b; // the line break is selected too
            if lo < hi || brk {
                let (lo, hi) = (lo.min(hi), hi.max(lo));
                let x0 = body.x + mark_x(&marks, &p.note, &mut m, lo).round() as i32;
                let x1 = body.x + mark_x(&marks, &p.note, &mut m, hi).round() as i32 + if brk { px(4.0, s) } else { 0 };
                cv.rrect(Rect { x: x0, y, w: (x1 - x0).max(0), h: lh }, 0, th.accent, 90);
            }
        }
        for (i, mk) in marks.iter().enumerate() {
            let col = if mk.2.hl > 0 { 0x000000 } else { th.ui_fg }; // on a highlight the text is black in every theme
            let rpx = m.eff(mk.2) as f32 * k;
            let f = m.font(mk.2);
            let ytext = base - f.ascent;
            let mut x = mk.3;
            for c in p.note[mk.0..mk.1].chars() {
                if c != ' ' {
                    let gx = body.x + x.round() as i32;
                    cv.glyph(f, c, gx, ytext, col);
                    if f.fake_bold {
                        cv.glyph(f, c, gx + 1, ytext, col);
                    }
                }
                x += f.advance(c);
            }
            if mk.2.underline {
                let (x0, x1) = (body.x + mk.3.round() as i32, body.x + end_x(i).round() as i32);
                let (ul_off, ul_th) = ((rpx * 0.1).round().max(1.0) as i32, (rpx / 14.0).round().max(1.0) as i32);
                cv.rect(Rect { x: x0, y: base + ul_off, w: x1 - x0, h: ul_th }, col);
            }
        }
        if kline == at {
            let cx = body.x + mark_x(&marks, &p.note, &mut m, p.note_caret.clamp(a, b)).round() as i32;
            let (cch, casc) = m.metrics(m.eff(caret_sty));
            let (cy0, cy1) = ((base - casc).max(y), (base - casc + cch).min(y + lh));
            cv.rect(Rect { x: cx, y: cy0, w: px(1.0, s).max(1), h: (cy1 - cy0).max(1) }, th.ui_fg);
        }
    }
    // scrollbar in the gutter right of the text: same look as the terminal's
    nv.track = Rect { x: r.x + r.w - 1 - inset, y: top, w: inset - 1, h: fy - 1 - top };
    nv.thumb = Rect { x: 0, y: 0, w: 0, h: 0 };
    if max > 0.0 {
        cv.set_clip(r.intersect(&old)); // the gutter lies outside the text's clip
        let hot = nv.hot || matches!(nv.drag, Some(NoteDrag::Thumb(_)));
        let thick = if hot { px(7.0, s) } else { px(4.0, s) };
        let h = thumb_len(nv.track.h, body.h, total, px(14.0, s));
        let y = nv.track.y + thumb_top(nv.track.h, h, nv.scroll / max);
        nv.thumb = Rect { x: nv.track.x + (nv.track.w - thick) / 2, y, w: thick, h };
        cv.rrect(nv.thumb, thick / 2, th.sub, if hot { 200 } else { 120 });
        nv.thumb = Rect { x: nv.track.x, w: nv.track.w, ..nv.thumb }; // hit area: the whole strip's width
    } else {
        nv.track = nv.thumb;
    }
    if let Some(pr) = pal {
        // the palette, over the text
        cv.set_clip(r.intersect(&old));
        cv.card(pr, px(8.0, s), mix(th.border, th.ui_fg, 0.30), paper);
        for (i, &c) in cells.iter().enumerate() {
            let f = fade_of(fades, c);
            let ring = if st.hl as usize == i { th.accent } else { mix(th.border, th.ui_fg, f * 0.6) };
            if i == 0 {
                cv.card(c, px(5.0, s), ring, paper);
                centered(cv, ui, "x", c, th.sub);
            } else {
                cv.card(c, px(5.0, s), ring, NOTE_HL[i - 1]);
            }
        }
    }
    cv.set_clip(old);
    local.push((r, Action::Nop)); // the rest of the card swallows clicks
    pop_hits.append(&mut local);
    drop(hits.splice(0..0, pop_hits));
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
    rx: std::sync::mpsc::Receiver<Ev>,
    ui: Ui,
}

impl App {
    fn handle(&mut self, ev: Event) -> Control {
        let ui = &mut self.ui;
        match ev {
            Event::Close => ui.request_quit(),
            Event::Focused(f) => {
                if f != ui.focused {
                    ui.focus_report(ui.focus, f);
                }
                ui.focused = f;
                if f {
                    ui.panes[ui.focus].attention = false;
                }
                ui.invalidate();
            }
            Event::Resized => ui.relayout(),
            Event::Redraw => ui.draw(),
            Event::Modifiers(m) => ui.mods = m,
            Event::Key(k) => ui.on_key(&k),
            Event::CursorMoved(x, y) => ui.on_cursor(x, y),
            Event::Mouse(button, state) => ui.on_mouse_button(button, state),
            Event::Wheel(delta) => ui.on_wheel(delta),
            Event::Wake => {
                while let Ok(ev) = self.rx.try_recv() {
                    match ev {
                        Ev::Output(id, bytes) => ui.on_output(id, &bytes),
                        Ev::Exit(id) => {
                            if let Some(i) = ui.panes.iter().position(|p| p.id == id) {
                                ui.begin_close(i);
                            }
                        }
                        Ev::Hook(h) => ui.on_hook(h),
            Ev::Note(r) => ui.on_note(r),
                    }
                }
            }
            Event::AboutToWait => return self.about_to_wait(),
        }
        if self.ui.quit {
            self.ui.finish_memory();
            return Control::Exit;
        }
        Control::Wait
    }

    fn about_to_wait(&mut self) -> Control {
        if self.stats && self.last_stats.elapsed() >= Duration::from_secs(1) {
            self.last_stats = Instant::now();
            eprintln!("stats: {} draws, {} output chunks, animating={}", DRAWS.swap(0, Ordering::Relaxed), OUTPUTS.swap(0, Ordering::Relaxed), self.ui.animating);
        }
        let ui = &mut self.ui;
        if ui.animating {
            // Pace animation frames with a precise sleep: a timed wait is rounded to the ~15.6 ms Windows timer
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
            return Control::WaitUntil(ui.next_mark);
        }
        Control::Wait
    }
}

/// A windowed exe has no console, so a panic would close maula without a trace: keep them in `crash.log`.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let dir = config::config_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = writeln!(f, "unix time {now}, maula {}: {info}", env!("CARGO_PKG_VERSION"));
        }
        default(info);
    }));
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("hook") {
        return claude::hook_main(); // run by Claude Code, see claude.rs
    }
    if std::env::args().nth(1).as_deref() == Some("note") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(notes_api::note_main(&args)); // `maula note read|write|append|clear`, see notes_api.rs
    }
    log_panics();
    let cfg = config::load();
    let (window, waker) = Window::new("maula", 1280.0, 800.0, 480.0, 300.0);
    let (tx, rx) = std::sync::mpsc::channel();
    let ui = Ui::new(window, Proxy { tx, waker }, cfg);
    let mut app = App { stats: std::env::var_os("MAULA_STATS").is_some(), last_stats: Instant::now(), rx, ui };
    window::run(move |ev| app.handle(ev));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_text(n: usize) -> String {
        let words = ["lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "wide", "x"];
        let mut s = String::with_capacity(n + 64);
        let mut i = 0usize;
        while s.len() < n {
            s.push_str(words[i * 7 % words.len()]);
            s.push(if i % 23 == 22 { '\n' } else { ' ' });
            i += 1;
        }
        s
    }

    fn lines_of(text: &str, fonts: &mut NoteFonts, spans: &Spans) -> Vec<(usize, usize, bool)> {
        let mut l = NoteLayout::default();
        l.rebuild(text, 300, 10, &mut Measure { fonts, k: 1.0, def: 14, line: 1.0, spans });
        l.lines
    }

    #[test]
    fn note_layout_edits_match_a_full_wrap() {
        let (mut fonts, mut spans) = (NoteFonts::default(), Spans::default());
        let mut text = note_text(5000);
        let mut l = NoteLayout::default();
        l.rebuild(&text, 300, 10, &mut Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans });
        for (at, old, ins) in [(100, 0, "typed "), (2000, 300, ""), (50, 0, "a\nb\n\nc dd ee ff gg hh ii jj kk ll mm nn oo pp qq rr ss tt uu"), (text.len() - 3, 3, "end"), (0, 5, "")] {
            let at = at.min(text.len());
            let old = old.min(text.len() - at);
            text.replace_range(at..at + old, ins);
            // every other edit types bold text, so wraps depend on the styles too
            let style = if ins.len() % 2 == 0 { Style::default() } else { Style { bold: true, ..Style::default() } };
            let rel = if style.is_plain() || ins.is_empty() { vec![] } else { vec![Span { start: 0, end: ins.len(), style }] };
            spans.splice(at, old, ins.len(), &rel);
            l.edit(&text, at, old, ins.len(), &mut Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans });
            assert_eq!(l.lines, lines_of(&text, &mut fonts, &spans), "edit at {at}");
        }
        spans.toggle(10, 1500, Flag::Bold);
        spans.toggle(1000, 2000, Flag::Italic);
        assert_eq!(lines_of(&text, &mut fonts, &spans).len() >= lines_of(&text, &mut fonts, &Spans::default()).len(), true, "bold text is wider: never fewer lines");
        assert!(l.lines.iter().all(|&(a, b, _)| a <= b && text.is_char_boundary(a) && text.is_char_boundary(b)));
    }

    #[test]
    fn mixed_sizes_make_variable_line_heights() {
        let mut fonts = NoteFonts::default();
        let mut spans = Spans::default();
        let text = "small line\nbig big big\n\nlast";
        spans.set_size(11, 22, 28); // "big big big"
        spans.set_size(23, 24, 24); // the empty line's own break
        let mut l = NoteLayout::default();
        let mut m = Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans };
        let (h14, h28, h24) = (m.lh(14), m.lh(28), m.lh(24));
        assert!(h14 < h24 && h24 < h28);
        l.rebuild(text, 10_000, 0, &mut m);
        assert_eq!(l.hs, [h14, h28, h24, h14], "the empty line takes the size of its own break");
        assert_eq!(l.ys, [0, h14, h14 + h28, h14 + h28 + h24, 2 * h14 + h28 + h24]);
        assert_eq!(l.total(), 2 * h14 + h28 + h24);
        assert_eq!((l.line_at_y(-5), l.line_at_y(0), l.line_at_y(h14 - 1), l.line_at_y(h14), l.line_at_y(9999)), (0, 0, 0, 1, 3));
        // a line height of 1.5 scales every line
        m.line = 1.5;
        assert_eq!(m.lh(28), (m.metrics(28).0 as f32 * 1.5).round() as i32);
        // a wrapped line is as high as its largest run: one big word makes its whole line tall
        let t = "aaaaa bbbbb ccccc";
        let mut sp = Spans::default();
        sp.set_size(6, 11, 32);
        let mut l2 = NoteLayout::default();
        let mut m = Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &sp };
        let w = m.width(t, 6, 11).ceil() as i32 + 8;
        l2.rebuild(t, w, 0, &mut m);
        assert_eq!(l2.lines.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(), [(0, 6), (6, 12), (12, 17)]);
        let (a, b) = (m.lh(14), m.lh(32));
        assert_eq!(l2.hs, [a, b, a], "the big word's own line is tall, its neighbours are not");
        // incremental edits keep heights and tops equal to a fresh wrap
        let mut text = String::from(text);
        text.insert_str(3, "XX");
        spans.splice(3, 0, 2, &[Span { start: 0, end: 2, style: Style { size: 32, ..Style::default() } }]);
        let mut m = Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans };
        l.edit(&text, 3, 0, 2, &mut m);
        let mut fresh = NoteLayout::default();
        fresh.rebuild(&text, 10_000, 0, &mut m);
        assert_eq!((&l.lines, &l.hs, &l.ys), (&fresh.lines, &fresh.hs, &fresh.ys));
    }

    #[test]
    fn note_clean_normalises_line_ends() {
        assert_eq!(note_clean("a\r\nb\rc\td\x07"), "a\nb\nc  d");
    }

    #[test]
    fn a_big_paste_is_laid_out_once_and_edits_stay_cheap() {
        let (mut fonts, mut spans) = (NoteFonts::default(), Spans::default());
        let mut text = note_text(2 << 20);
        let t0 = Instant::now();
        let clean = note_clean(&text);
        assert_eq!(clean.len(), text.len());
        let mut l = NoteLayout::default();
        l.rebuild(&text, 600, 10, &mut Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans });
        let rebuild = t0.elapsed();
        let t1 = Instant::now();
        for k in 0..50 {
            let at = text.len() / 2 + k;
            text.insert(at, 'z');
            l.edit(&text, at, 0, 1, &mut Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans });
        }
        let per_key = t1.elapsed() / 50;
        assert!(rebuild < Duration::from_secs(5), "rebuild took {rebuild:?}");
        assert!(per_key < Duration::from_millis(50), "a keystroke took {per_key:?}");
        // the same note with styled text in it: a styled rebuild and a keystroke stay cheap too
        spans.toggle(1000, 1_500_000, Flag::Bold);
        spans.toggle(2000, 800_000, Flag::Italic);
        let t2 = Instant::now();
        l.rebuild(&text, 600, 10, &mut Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans });
        let styled = t2.elapsed();
        let t3 = Instant::now();
        for k in 0..50 {
            let at = text.len() / 2 + k;
            text.insert(at, 'z');
            spans.splice(at, 0, 1, &[Span { start: 0, end: 1, style: Style { bold: true, ..Style::default() } }]);
            l.edit(&text, at, 0, 1, &mut Measure { fonts: &mut fonts, k: 1.0, def: 14, line: 1.0, spans: &spans });
        }
        assert!(styled < Duration::from_secs(5), "styled rebuild took {styled:?}");
        assert!(t3.elapsed() / 50 < Duration::from_millis(50), "a styled keystroke took {:?}", t3.elapsed() / 50);
    }

    #[test]
    fn note_api_write_append_clear() {
        use notes_api::Op;
        let mut p = NoteDoc::default();
        p.note_replace(0, 0, "old", 1, Style::default());
        p.note_spans.toggle(0, 3, Flag::Bold);
        assert!(p.note_api(Op::Read, "").is_none());
        assert_eq!(p.note_api(Op::Write, "a
b"), Some((0, 3, 3)));
        assert_eq!(p.note, "a
b");
        assert!(p.note_spans.slice(0, 3).is_empty(), "spans over replaced text are dropped");
        assert!(p.note_dirty && p.note_blocks());
        p.note_api(Op::Append, "!");
        assert_eq!(p.note, "a
b!");
        p.note_api(Op::Clear, "");
        assert_eq!(p.note, "");
        // write was one undo step: undo x2 (clear, append) then the write
        for _ in 0..3 {
            p.note_step(false).unwrap();
        }
        assert_eq!(p.note, "old");
    }

    #[test]
    fn a_pane_note_keeps_styles_through_edits_undo_and_save_state() {
        let mut p = NoteDoc::default();
        let bold = Style { bold: true, ..Style::default() };
        p.note_replace(0, 0, "hello world", 1, Style::default());
        p.note_spans.toggle(0, 5, Flag::Bold);
        p.note_recheck();
        assert!(p.note_dirty && p.note_blocks(), "formatting counts as unsaved");
        p.note_saved = p.note.clone();
        (p.note_spans_saved, p.note_px_saved, p.note_line_saved) = (p.note_spans.clone(), p.note_px, p.note_line);
        p.note_recheck();
        assert!(!p.note_dirty);
        p.note_px = 18;
        p.note_recheck();
        assert!(p.note_dirty, "a size change is unsaved");
        p.note_px = 14;
        p.note_recheck();
        assert!(!p.note_dirty);
        // delete "llo wo" (bytes 2..8), then undo: the bold part comes back
        let before = p.note_spans.clone();
        p.note_replace(2, 6, "", 0, Style::default());
        assert_eq!(p.note, "herld");
        assert_eq!(p.note_spans.style_of(1), bold);
        assert_eq!(p.note_spans.style_of(2), Style::default());
        p.note_step(false);
        assert_eq!((p.note.as_str(), &p.note_spans), ("hello world", &before));
        p.note_step(true);
        assert_eq!(p.note, "herld");
        p.note_step(false);
        // typing bold text at the end, one undo step for the whole run
        p.note_caret = p.note.len();
        for ch in ["a", "b", "c"] {
            let at = p.note.len();
            p.note_replace(at, 0, ch, 1, bold);
        }
        assert_eq!(p.note_undo.back().map(|e| e.inserted_sty.len()), Some(1), "merged typing keeps one span");
        assert!(p.note_spans.all(11, 14, |s| s.bold));
        p.note_step(false);
        assert_eq!((p.note.as_str(), &p.note_spans), ("hello world", &before));
        // Backspace and Delete runs over styled text restore it too, each run being one undo step
        let steps = p.note_undo.len();
        p.note_replace(4, 1, "", 2, Style::default());
        p.note_replace(3, 1, "", 2, Style::default());
        assert_eq!(p.note, "hel world");
        assert_eq!(p.note_undo.len(), steps + 1, "merged");
        p.note_step(false);
        assert_eq!((p.note.as_str(), &p.note_spans), ("hello world", &before));
        p.note_replace(2, 1, "", 3, Style::default());
        p.note_replace(2, 1, "", 3, Style::default());
        assert_eq!(p.note, "heo world");
        p.note_step(false);
        assert_eq!((p.note.as_str(), &p.note_spans), ("hello world", &before));
    }

    fn row(t: &mut Term, bytes: &[u8]) -> Vec<Cell> {
        t.feed(bytes);
        t.line(t.history.len() + t.cy).to_vec()
    }

    #[test]
    fn only_running_text_leaves_the_grid() {
        let mut t = Term::new(80, 3);
        assert!(is_prose(&row(&mut t, b"  Here is what I changed in the file.")));
        for (bytes, why) in [
            (&b"\r\n\x1b[2K  short label"[..], "two words"),
            (b"\r\n\x1b[2K  name   size   date of change", "aligned columns"),
            (b"\r\n\x1b[2K  let x = foo(bar) + baz", "code punctuation"),
            (b"\r\n\x1b[2K\x1b[31mfn \x1b[32mmain \x1b[33mruns \x1b[34mthings\x1b[0m", "syntax colours"),
            (b"\r\n\x1b[2K\x1b[41m  removed this line right here\x1b[0m", "a background"),
            ("\r\n\x1b[2K│ inside a nice box │".as_bytes(), "box drawing"),
            ("\r\n\x1b[2K❯ typing a long prompt".as_bytes(), "the input row"),
        ] {
            assert!(!is_prose(&row(&mut t, bytes)), "{why}");
        }
        assert!(is_prose(&row(&mut t, b"\r\n\x1b[2K  Use the \x1b[36mfind\x1b[0m command for this.")), "one inline code colour");
    }

    #[test]
    fn proportional_columns_keep_the_indent_and_map_back_to_cells() {
        let mut t = Term::new(40, 1);
        let line = row(&mut t, b"  iii WWW tail");
        let mut prose = Font::prose(14.0, 10);
        let xs = prose_xs(&mut prose, 8, &line, 40).expect("running text");
        assert_eq!(xs.len(), 41);
        assert_eq!(xs[2], 16, "the indent stays on the grid");
        assert!(xs.windows(2).all(|w| w[0] <= w[1]), "left to right: {xs:?}");
        assert!(xs[3] - xs[2] < xs[7] - xs[6], "an i is narrower than a W, so it left the grid");
        let hit = |dx: i32| xs.partition_point(|&e| e <= dx).saturating_sub(1).min(39);
        assert_eq!(hit(xs[7] + 1), 7, "a click finds its cell");
        assert_eq!(hit(-5), 0);
        assert_eq!(hit(99_999), 39);

        let long = row(&mut Term::new(10, 1), b"a b c MMMMMM");
        assert_eq!(prose_xs(&mut Font::prose(40.0, 10), 8, &long, 10), None, "too wide for the panel: stays on the grid");
    }

    #[test]
    fn a_drag_selection_scrolls_only_past_the_edges() {
        let inner = Rect { x: 0, y: 100, w: 500, h: 200 };
        assert_eq!(edge_speed(100, inner, 20), 0.0, "first pixel inside");
        assert_eq!(edge_speed(299, inner, 20), 0.0, "last pixel inside");
        assert!(edge_speed(99, inner, 20) > 0.0, "above: back through history");
        assert!(edge_speed(300, inner, 20) < 0.0, "below: towards the live screen");
        assert!(edge_speed(0, inner, 20) > edge_speed(90, inner, 20), "faster the further out");
        assert_eq!(edge_speed(-100_000, inner, 20), 200.0, "capped");
    }
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
                draw_term(&mut cv, &mut font, None, &t, 0.0, None, &th, inner, k == 0, if k == 0 { 0.0 } else { 1.0 }); // card view: inactive greyed
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
        assert_eq!(text_of(&c)[4..], [2, 1, 0]);
        assert_eq!(text_of(&Config::default())[4..], [0, 0, 0], "unset: the theme's own accent, borderless fullscreen, terminal render");
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
            draw_scrollbar(&mut cv, &t, pos, &th, outer, hh, 1.0, false);
            let touched: Vec<i32> = (0..320).filter(|&y| (0..200).any(|x| buf[(y * 200 + x) as usize] != 0xffffff)).collect();
            assert!(!touched.is_empty(), "the scrollbar is drawn");
            assert!(*touched.first().unwrap() > outer.y + hh, "pos {pos}: starts below the header, got y={}", touched.first().unwrap());
            assert!(*touched.last().unwrap() < outer.y + outer.h - 2, "pos {pos}: ends above the bottom border, got y={}", touched.last().unwrap());
        }
    }

    #[test]
    fn thumb_and_offset_map_both_ways() {
        assert_eq!(thumb_len(200, 10, 100, 14), 20);
        assert_eq!(thumb_len(200, 1, 1000, 14), 14, "minimum size");
        assert_eq!(thumb_len(200, 100, 100, 14), 200, "everything visible");
        assert_eq!(thumb_len(10, 1, 1000, 14), 10, "never longer than the track");
        assert_eq!(thumb_top(200, 20, 0.0), 0);
        assert_eq!(thumb_top(200, 20, 1.0), 180);
        assert_eq!(thumb_top(200, 20, 0.5), 90);
        // grabbing the thumb 5px below its top and moving to y=105 puts the top at 100 of a 50..250 track
        assert!((thumb_frac(105, 5, 50, 200, 20) - 50.0 / 180.0).abs() < 1e-6);
        assert_eq!(thumb_frac(-500, 5, 50, 200, 20), 0.0, "clamped above");
        assert_eq!(thumb_frac(9999, 5, 50, 200, 20), 1.0, "clamped below");
        for f in [0.0, 0.25, 0.8, 1.0] {
            let top = thumb_top(200, 20, f);
            assert!((thumb_frac(50 + top + 7, 7, 50, 200, 20) - f).abs() < 0.01, "round trip {f}");
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
            draw_term(&mut cv, &mut font, None, &t, 0.0, None, &th, Rect { x: 0, y: 0, w, h }, false, dim);
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
            draw_term(&mut cv, &mut font, None, &t, pos, None, &th, r, true, 0.0);
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
                draw_term(&mut cv, font, None, t, 0.0, None, &th, inner, k == 0, 0.0);
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
                    draw_term(&mut cv, &mut font, None, t, 0.0, None, &th, inner, k == 0, 0.0);
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
