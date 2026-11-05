//! Minimal VT terminal: bytes in -> character grid out. No GUI, no PTY.
//! ponytail: every char is 1 or 2 cells (combining marks dropped), no mouse reporting.

use std::collections::VecDeque;

const HISTORY_MAX: usize = 10_000;

/// Colors are references resolved against the active theme at draw time, so themes switch live.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Color {
    Fg,
    Bg,
    Ansi(u8),
    Rgb(u32),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub ch: char, // '\0' = right half of a wide char
    pub fg: Color,
    pub bg: Color,
    pub dim: bool, // SGR 2: faint text, e.g. Claude Code's tab-completion suggestion
}

pub const BLANK: Cell = Cell { ch: ' ', fg: Color::Fg, bg: Color::Bg, dim: false };

/// A history line. `wrapped`: it ran into the right edge and continues on the next line (it is full width then);
/// otherwise it ended with a newline and its trailing blanks are dropped.
pub struct Line {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

enum State {
    Ground,
    Esc,
    EscSkip, // ESC ( B and friends: swallow one byte
    Csi,
    Osc, // OSC, or DCS/APC/PM/SOS (osc_is_osc == false): collected until BEL or ST
    OscEsc,
}

/// Which mouse events the program wants reported, each level including the ones before it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Mouse {
    Off,
    /// presses, releases and the wheel (1000)
    Click,
    /// also motion while a button is held (1002)
    Drag,
    /// also any motion (1003)
    Motion,
}

/// What happened to the mouse, for `Term::mouse_report`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseEv {
    /// button 0 left, 1 middle, 2 right
    Press(u8),
    Release(u8),
    /// the pointer moved; the button held, if any
    Move(Option<u8>),
    WheelUp,
    WheelDown,
}

pub struct Term {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    /// Per grid row: continues on the next row (see `Line::wrapped`).
    wrapped: Vec<bool>,
    pub cx: usize,
    pub cy: usize,
    pub cursor_visible: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Mouse reporting the program asked for (DECSET 1000/1002/1003); while it is on, the program owns the mouse.
    pub mouse: Mouse,
    /// Mouse reports in SGR form (DECSET 1006), else the legacy `CSI M` bytes.
    pub mouse_sgr: bool,
    /// Report focus in and out as `CSI I` / `CSI O` (DECSET 1004).
    pub focus_events: bool,
    /// When the grid last resized: ConPTY repaints with `2J 3J` right after, which must not wipe the scrollback.
    resized_at: Option<std::time::Instant>,
    /// Working directory reported by the shell (OSC 7 / OSC 9;9).
    pub cwd: Option<String>,
    /// Where the shell's last prompt starts (OSC 133;A): (absolute line, col). Cleared by the caller once a command is sent.
    pub prompt: Option<(usize, usize)>,
    /// Lines scrolled off the top, oldest first.
    pub history: VecDeque<Line>,
    /// How many lines the view is scrolled back (0 = live).
    pub scroll: usize,
    /// Bytes to send back to the program (answers to its queries). Caller drains this.
    pub reply: Vec<u8>,
    fg: Color,
    bg: Color,
    reverse: bool,
    dim: bool,
    wrap_pending: bool,
    saved: (usize, usize),
    top: usize,
    bot: usize,
    alt: Option<(Vec<Cell>, Vec<bool>, (usize, usize))>, // the main screen while the alternate one is up
    state: State,
    csi: Vec<u8>,
    osc: Vec<u8>,
    osc_is_osc: bool,
    utf8: Vec<u8>,
}

fn color256(n: usize) -> Color {
    match n {
        0..=15 => Color::Ansi(n as u8),
        16..=231 => {
            let lvl = |v: usize| if v == 0 { 0 } else { 55 + v as u32 * 40 };
            let n = n - 16;
            Color::Rgb(lvl(n / 36) << 16 | lvl(n / 6 % 6) << 8 | lvl(n % 6))
        }
        _ => {
            let g = 8 + (n.min(255) as u32 - 232) * 10;
            Color::Rgb(g << 16 | g << 8 | g)
        }
    }
}

/// The alternate screen on resize: cut or padded, no rewrapping (the full-screen app redraws it anyway).
fn crop(old: &[Cell], oc: usize, or: usize, nc: usize, nr: usize) -> Vec<Cell> {
    let mut cells = vec![BLANK; nc * nr];
    for y in 0..nr.min(or) {
        for x in 0..nc.min(oc) {
            cells[y * nc + x] = old[y * oc + x];
        }
    }
    cells
}

/// Length of `row` without its trailing default blanks.
fn trimmed_len(row: &[Cell]) -> usize {
    row.iter().rposition(|c| *c != BLANK).map_or(0, |i| i + 1)
}

/// Rewraps the main screen (`cells`, `oc` wide, flags `wrapped`) and `history` to `nc` columns: wrapped pieces join
/// back into whole lines and split again at the new width, a wide char never split. Returns the new grid (`nr` rows:
/// the last lines, the cursor kept on screen), its flags, and the cursor, still on the same character.
/// ponytail: the blank a wide char leaves at a row's end when it does not fit is kept as a space; cursor clamps to the last column.
fn rewrap(history: &mut VecDeque<Line>, cells: &[Cell], wrapped: &[bool], (cx, cy): (usize, usize), oc: usize, nc: usize, nr: usize) -> (Vec<Cell>, Vec<bool>, (usize, usize)) {
    let row = |r: usize| &cells[r * oc..(r + 1) * oc];
    // rows below both the cursor and the last text are empty: not lines
    let used = (0..wrapped.len()).rev().find(|&r| trimmed_len(row(r)) > 0).map_or(0, |r| r + 1).max(cy + 1);
    let cursor_row = history.len() + cy;
    let rows = std::mem::take(history).into_iter().map(|l| (l.cells, l.wrapped)).chain((0..used).map(|r| (row(r).to_vec(), wrapped[r])));
    let (mut logical, mut cur, mut at) = (Vec::new(), Vec::new(), (0, 0)); // at: (logical line, offset) of the cursor
    for (i, (mut l, w)) in rows.enumerate() {
        if i == cursor_row {
            at = (logical.len(), cur.len() + cx);
        }
        if !w {
            l.truncate(trimmed_len(&l));
        }
        if !w && cur.is_empty() {
            logical.push(l); // a line on its own (the usual case): moved, not copied
        } else {
            cur.extend(l);
            if !w {
                logical.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        logical.push(cur);
    }

    let (mut out, mut pos) = (Vec::new(), (0, 0));
    for (li, l) in logical.into_iter().enumerate() {
        if l.len() <= nc && li != at.0 {
            out.push(Line { cells: l, wrapped: false }); // fits as it is
            continue;
        }
        let mut start = 0;
        loop {
            let mut end = (start + nc).min(l.len());
            if end < l.len() && l[end].ch == '\0' && end > start + 1 {
                end -= 1; // the wide char's left half would end the row: move it to the next one
            }
            let last = end >= l.len();
            if li == at.0 && at.1 >= start && (at.1 < end || last) {
                pos = (out.len(), (at.1 - start).min(nc - 1));
            }
            out.push(Line { cells: l[start..end].to_vec(), wrapped: !last });
            if last {
                break;
            }
            start = end;
        }
    }

    let start = out.len().saturating_sub(nr).min(pos.0);
    let (mut grid, mut flags) = (vec![BLANK; nc * nr], vec![false; nr]);
    for (r, l) in out.drain(start..).take(nr).enumerate() {
        grid[r * nc..r * nc + l.cells.len()].copy_from_slice(&l.cells);
        flags[r] = l.wrapped;
    }
    history.extend(out);
    (grid, flags, (pos.1, pos.0 - start))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = if b[i] == b'%' && i + 2 < b.len() { std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) } else { None };
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `host/C:/dir/x%20y` (the part after `file://`) -> `C:/dir/x y`
fn file_url_path(rest: &str) -> String {
    let path = percent_decode(rest.find('/').map_or("", |i| &rest[i..]));
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[2] == b':' { path[1..].to_string() } else { path }
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        Term {
            cols,
            rows,
            cells: vec![BLANK; cols * rows],
            wrapped: vec![false; rows],
            cx: 0,
            cy: 0,
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
            mouse: Mouse::Off,
            mouse_sgr: false,
            focus_events: false,
            resized_at: None,
            cwd: None,
            prompt: None,
            history: VecDeque::new(),
            scroll: 0,
            reply: Vec::new(),
            fg: Color::Fg,
            bg: Color::Bg,
            reverse: false,
            dim: false,
            wrap_pending: false,
            saved: (0, 0),
            top: 0,
            bot: rows - 1,
            alt: None,
            state: State::Ground,
            csi: Vec::new(),
            osc: Vec::new(),
            osc_is_osc: false,
            utf8: Vec::new(),
        }
    }

    // ---- reading the grid (for the renderer / selection) ----

    pub fn total_lines(&self) -> usize {
        self.history.len() + self.rows
    }

    /// Absolute index (history first, then the live grid) of the top visible line.
    #[cfg(test)]
    pub fn view_start(&self) -> usize {
        self.history.len() - self.scroll
    }

    pub fn line(&self, idx: usize) -> &[Cell] {
        if idx < self.history.len() {
            &self.history[idx].cells
        } else {
            let r = idx - self.history.len();
            &self.cells[r * self.cols..(r + 1) * self.cols]
        }
    }

    #[cfg(test)]
    pub fn scroll_by(&mut self, d: isize) {
        self.scroll = (self.scroll as isize + d).clamp(0, self.history.len() as isize) as usize;
    }

    /// Text between two (line, col) positions, inclusive, trailing spaces trimmed per line.
    pub fn text_range(&self, a: (usize, usize), b: (usize, usize)) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let last = self.total_lines() - 1;
        let mut out = Vec::new();
        let mut split = false; // the previous row was a word the program split: drop this row's indent
        for l in a.0.min(last)..=b.0.min(last) {
            let (s, e) = (if l == a.0 { a.1 } else { 0 }, if l == b.0 { b.1 } else { usize::MAX });
            let text: String = self.line(l).iter().enumerate().filter(|(i, c)| *i >= s && *i <= e && c.ch != '\0').map(|(_, c)| c.ch).collect();
            let text = if split { text.trim_start().to_string() } else { text };
            let joins = l < b.0.min(last) && self.joins(l);
            split = joins && !self.wraps(l);
            out.push(if joins { text } else { text.trim_end().to_string() + "\n" });
        }
        let mut out = out.concat();
        out.pop(); // the last line's newline
        out
    }

    /// Copying line `l` should run straight on into the next one: it wrapped at the edge, or the program broke a
    /// word too long for its width onto the next row itself (Claude Code does this to long paths, indenting the rest).
    /// ponytail: a full row ending in a token too long to have fit whole means a split word; other full rows keep their newline.
    fn joins(&self, l: usize) -> bool {
        if self.wraps(l) {
            return true;
        }
        if l + 1 >= self.total_lines() {
            return false;
        }
        let text = |l: usize| self.line(l).iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect::<String>();
        let (a, b) = (text(l), text(l + 1));
        let (a, end) = (a.trim_end(), trimmed_len(self.line(l)));
        let indent = b.chars().take_while(|c| *c == ' ').count();
        if end + 2 < self.cols || b.trim().is_empty() || indent == 0 || end <= indent {
            return false;
        }
        let last = a.rsplit(' ').next().unwrap_or("").chars().count();
        let first = b.trim_start().split(' ').next().unwrap_or("").chars().count();
        last > 0 && last + first > end - indent
    }

    /// Whether line `idx` (absolute) runs on into the next one.
    fn wraps(&self, idx: usize) -> bool {
        match idx.checked_sub(self.history.len()) {
            None => self.history[idx].wrapped,
            Some(r) => self.wrapped.get(r).copied().unwrap_or(false),
        }
    }

    /// The first row of the cursor's line (rows that wrapped into it count as the same line), absolute.
    fn line_start(&self) -> usize {
        let mut l = self.history.len() + self.cy;
        while l > 0 && self.wraps(l - 1) {
            l -= 1;
        }
        l
    }

    /// A full-screen program is up.
    pub fn alt_screen(&self) -> bool {
        self.alt.is_some()
    }

    /// The shell is waiting at its prompt: the cursor's line is the one the last prompt started.
    pub fn at_prompt(&self) -> bool {
        self.alt.is_none() && self.prompt.is_some_and(|(l, _)| l == self.line_start())
    }

    /// Every match of `q`, ignoring case, as (line, first col, last col), oldest first.
    /// ponytail: a match never spans two rows; every keystroke rescans all lines (a few ms at 10k).
    pub fn find(&self, q: &str) -> Vec<(usize, usize, usize)> {
        let low = |c: char| c.to_lowercase().next().unwrap_or(c);
        let q: Vec<char> = q.chars().map(low).collect();
        let mut out = Vec::new();
        if q.is_empty() {
            return out;
        }
        for l in 0..self.total_lines() {
            let cells: Vec<(usize, char)> = self.line(l).iter().enumerate().filter(|(_, c)| c.ch != '\0').map(|(i, c)| (i, low(c.ch))).collect();
            let mut i = 0;
            while i + q.len() <= cells.len() {
                if cells[i..i + q.len()].iter().map(|c| c.1).eq(q.iter().copied()) {
                    out.push((l, cells[i].0, cells[i + q.len() - 1].0));
                    i += q.len();
                } else {
                    i += 1;
                }
            }
        }
        out
    }

    /// The whitespace-delimited word at (line, col), following rows that wrapped into each other; None on a blank.
    pub fn word_at(&self, line: usize, col: usize) -> Option<String> {
        if line >= self.total_lines() {
            return None;
        }
        let (mut a, mut b) = (line, line);
        while a > 0 && self.wraps(a - 1) {
            a -= 1;
        }
        while b + 1 < self.total_lines() && self.wraps(b) {
            b += 1;
        }
        let (mut chars, mut at) = (Vec::new(), None);
        for l in a..=b {
            for (c, cell) in self.line(l).iter().enumerate() {
                if l == line && c == col {
                    at = Some(if cell.ch == '\0' { chars.len().saturating_sub(1) } else { chars.len() }); // right half: its char
                }
                if cell.ch != '\0' {
                    chars.push(cell.ch);
                }
            }
        }
        let at = at?;
        if chars.get(at).is_none_or(|c| c.is_whitespace()) {
            return None;
        }
        let s = chars[..at].iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
        let e = chars[at..].iter().position(|c| c.is_whitespace()).map_or(chars.len(), |i| at + i);
        Some(chars[s..e].iter().collect())
    }

    // ---- mouse ----

    /// The bytes reporting `ev` at cell (`col`, `row`) of the screen, or None when the program did not ask for it.
    /// `mods`: shift 4, alt 8, ctrl 16, as xterm adds them to the button code.
    pub fn mouse_report(&self, ev: MouseEv, col: usize, row: usize, mods: u8) -> Option<Vec<u8>> {
        let (code, release) = match ev {
            _ if self.mouse == Mouse::Off => return None,
            MouseEv::Press(b) => (b, false),
            MouseEv::Release(b) => (if self.mouse_sgr { b } else { 3 }, true), // legacy releases do not say which button
            MouseEv::Move(b) if self.mouse == Mouse::Motion || (self.mouse == Mouse::Drag && b.is_some()) => (32 + b.unwrap_or(3), false),
            MouseEv::Move(_) => return None,
            MouseEv::WheelUp => (64, false),
            MouseEv::WheelDown => (65, false),
        };
        let code = code + mods;
        if self.mouse_sgr {
            return Some(format!("[<{code};{};{}{}", col + 1, row + 1, if release { 'm' } else { 'M' }).into_bytes());
        }
        // legacy: each value plus 32 in one byte, so a cell past column/row 223 cannot be reported
        let (x, y) = (u8::try_from(col + 33).ok().filter(|&v| v < 255)?, u8::try_from(row + 33).ok().filter(|&v| v < 255)?);
        Some(vec![0x1b, b'[', b'M', 32 + code, x, y])
    }

    // ---- resize ----

    /// The main screen and its history rewrap to the new width (see `rewrap`); the alternate screen is just cropped.
    /// ponytail: Windows ConPTY also repaints the screen after a resize; that repaint lands on the rewrapped grid.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.resized_at = Some(std::time::Instant::now());
        if let Some((cells, wrapped, pos)) = &mut self.alt {
            (*cells, *wrapped, *pos) = rewrap(&mut self.history, cells, wrapped, *pos, self.cols, cols, rows);
            self.cells = crop(&self.cells, self.cols, self.rows, cols, rows);
            self.wrapped = vec![false; rows];
        } else {
            let at_prompt = self.prompt.filter(|_| self.at_prompt());
            let main = rewrap(&mut self.history, &self.cells, &self.wrapped, (self.cx, self.cy), self.cols, cols, rows);
            (self.cells, self.wrapped, (self.cx, self.cy)) = main;
            // ponytail: a prompt the cursor has left (output printed after it) is forgotten until the next one
            self.prompt = at_prompt.map(|(_, c)| (self.line_start(), c.min(cols - 1)));
        }
        self.trim_history();
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.top = 0;
        self.bot = rows - 1;
        self.wrap_pending = false;
    }

    fn trim_history(&mut self) {
        while self.history.len() > HISTORY_MAX {
            self.history.pop_front();
            self.prompt = self.prompt.and_then(|(l, c)| Some((l.checked_sub(1)?, c))); // lines moved up by one
        }
        self.scroll = self.scroll.min(self.history.len());
    }

    // ---- parsing ----

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            State::Ground => match b {
                0x1b => self.state = State::Esc,
                0x08 => {
                    self.cx = self.cx.saturating_sub(1);
                    self.wrap_pending = false;
                }
                0x09 => self.cx = ((self.cx / 8 + 1) * 8).min(self.cols - 1),
                0x0a..=0x0c => {
                    self.linefeed();
                    // ConPTY passes OSC 133;A through ahead of text it has not drawn yet (the echoed command and
                    // its newline): a newline after the prompt mark is that stale text, so the prompt is below it
                    if self.prompt.is_some() {
                        self.prompt = Some((self.history.len() + self.cy, 0));
                    }
                }
                0x0d => {
                    self.cx = 0;
                    self.wrap_pending = false;
                }
                0x00..=0x1f | 0x7f => {}
                _ => self.text(b),
            },
            State::Esc => {
                self.state = State::Ground;
                match b {
                    b'[' => {
                        self.csi.clear();
                        self.state = State::Csi;
                    }
                    b']' | b'P' | b'_' | b'^' | b'X' => {
                        self.osc.clear();
                        self.osc_is_osc = b == b']';
                        self.state = State::Osc;
                    }
                    b'(' | b')' | b'*' | b'+' | b'#' => self.state = State::EscSkip,
                    b'7' => self.saved = (self.cx, self.cy),
                    b'8' => self.restore_cursor(),
                    b'D' => self.linefeed(),
                    b'E' => {
                        self.cx = 0;
                        self.linefeed();
                    }
                    b'M' => {
                        if self.cy == self.top {
                            self.scroll_down(self.top, self.bot, 1);
                        } else {
                            self.cy = self.cy.saturating_sub(1);
                        }
                    }
                    b'c' => *self = Term::new(self.cols, self.rows),
                    _ => {}
                }
            }
            State::EscSkip => self.state = State::Ground,
            State::Csi => match b {
                0x40..=0x7e => {
                    self.state = State::Ground;
                    self.csi_dispatch(b);
                }
                0x1b => self.state = State::Esc,
                _ => {
                    if self.csi.len() < 64 {
                        self.csi.push(b);
                    }
                }
            },
            State::Osc => match b {
                0x07 => {
                    self.state = State::Ground;
                    self.osc_dispatch();
                }
                0x1b => self.state = State::OscEsc,
                _ => {
                    if self.osc.len() < 2048 {
                        self.osc.push(b);
                    }
                }
            },
            State::OscEsc => {
                self.state = State::Ground;
                self.osc_dispatch();
            }
        }
    }

    fn osc_dispatch(&mut self) {
        let data = String::from_utf8_lossy(&std::mem::take(&mut self.osc)).into_owned();
        if !self.osc_is_osc {
            return;
        }
        let Some((code, rest)) = data.split_once(';') else { return };
        match code {
            "7" => {
                if let Some(r) = rest.strip_prefix("file://") {
                    self.cwd = Some(file_url_path(r));
                }
            }
            "9" => {
                if let Some(r) = rest.strip_prefix("9;") {
                    self.cwd = Some(r.trim_matches('"').to_string());
                }
            }
            "133" if rest.starts_with('A') && self.alt.is_none() => self.prompt = Some((self.history.len() + self.cy, self.cx)),
            _ => {}
        }
    }

    fn text(&mut self, b: u8) {
        if b < 0x80 && self.utf8.is_empty() {
            return self.put(b as char);
        }
        self.utf8.push(b);
        match std::str::from_utf8(&self.utf8) {
            Ok(s) => {
                let s = s.to_owned();
                self.utf8.clear();
                for c in s.chars() {
                    self.put(c);
                }
            }
            Err(e) if e.error_len().is_none() => {} // incomplete, wait for more bytes
            Err(_) => {
                self.utf8.clear();
                self.put('\u{fffd}');
            }
        }
    }

    fn put(&mut self, ch: char) {
        let w = match crate::width::width(ch) {
            0 => return, // ponytail: combining marks dropped
            w => w.min(self.cols),
        };
        if self.wrap_pending || self.cx + w > self.cols {
            self.wrapped[self.cy] = true;
            self.cx = 0;
            self.linefeed();
            self.wrap_pending = false;
        }
        let (fg, bg) = if self.reverse { (self.bg, self.fg) } else { (self.fg, self.bg) };
        let i = self.cy * self.cols + self.cx;
        self.cells[i] = Cell { ch, fg, bg, dim: self.dim };
        if w == 2 {
            self.cells[i + 1] = Cell { ch: '\0', fg, bg, dim: self.dim };
        }
        self.cx += w;
        if self.cx >= self.cols {
            self.cx = self.cols - 1;
            self.wrap_pending = true;
        }
    }

    /// The saved position may predate a resize: clamp it, or the next `put` indexes past the grid.
    fn restore_cursor(&mut self) {
        (self.cx, self.cy) = (self.saved.0.min(self.cols - 1), self.saved.1.min(self.rows - 1));
        self.wrap_pending = false;
    }

    /// A grid row as a history line: trailing default blanks dropped unless it wraps (readers pad short lines with BLANK).
    fn push_history(&mut self, r: usize) {
        let (row, wrapped) = (&self.cells[r * self.cols..(r + 1) * self.cols], self.wrapped[r]);
        let len = if wrapped { row.len() } else { trimmed_len(row) };
        self.history.push_back(Line { cells: row[..len].to_vec(), wrapped });
    }

    fn linefeed(&mut self) {
        if self.cy == self.bot {
            self.scroll_up(self.top, self.bot, 1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    /// Erase cells [from, to) using the current background color.
    fn fill(&mut self, from: usize, to: usize) {
        let blank = Cell { ch: ' ', fg: Color::Fg, bg: self.bg, dim: false };
        self.cells[from..to].fill(blank);
        let c = self.cols;
        if from < to {
            for r in from / c..=(to - 1) / c {
                if to >= (r + 1) * c {
                    self.wrapped[r] = false; // erased to its end: it no longer runs on into the next row
                }
            }
        }
    }

    fn scroll_up(&mut self, top: usize, bot: usize, n: usize) {
        let (c, n) = (self.cols, n.min(bot - top + 1));
        if top == 0 && self.alt.is_none() {
            for r in 0..n {
                self.push_history(r);
            }
            if self.scroll > 0 {
                self.scroll += n; // keep the user's place while output streams in
            }
            self.trim_history();
        }
        self.cells.copy_within((top + n) * c..(bot + 1) * c, top * c);
        self.wrapped.copy_within(top + n..bot + 1, top);
        self.fill((bot + 1 - n) * c, (bot + 1) * c);
    }

    fn scroll_down(&mut self, top: usize, bot: usize, n: usize) {
        let (c, n) = (self.cols, n.min(bot - top + 1));
        self.cells.copy_within(top * c..(bot + 1 - n) * c, (top + n) * c);
        self.wrapped.copy_within(top..bot + 1 - n, top + n);
        self.fill(top * c, (top + n) * c);
    }

    fn set_alt(&mut self, on: bool) {
        if on && self.alt.is_none() {
            let (blank, flags) = (vec![BLANK; self.cells.len()], vec![false; self.rows]);
            self.alt = Some((std::mem::replace(&mut self.cells, blank), std::mem::replace(&mut self.wrapped, flags), (self.cx, self.cy)));
            (self.cx, self.cy) = (0, 0);
        } else if !on {
            let Some((cells, wrapped, pos)) = self.alt.take() else { return };
            (self.cells, self.wrapped) = (cells, wrapped);
            (self.cx, self.cy) = (pos.0.min(self.cols - 1), pos.1.min(self.rows - 1));
        } else {
            return;
        }
        self.wrap_pending = false;
        (self.top, self.bot) = (0, self.rows - 1);
    }

    fn csi_dispatch(&mut self, fin: u8) {
        let buf = std::mem::take(&mut self.csi);
        if buf.iter().any(|b| (0x20..=0x2f).contains(b)) {
            return; // intermediates (e.g. cursor style `CSI Ps SP q`): ignored
        }
        let private = matches!(buf.first(), Some(b'?' | b'>' | b'=' | b'<'));
        let s = std::str::from_utf8(&buf[private as usize..]).unwrap_or("");
        let p: Vec<usize> = s.split(';').map(|t| t.parse().unwrap_or(0)).collect();
        let n = |i: usize, d: usize| match p.get(i) {
            Some(&v) if v > 0 => v,
            _ => d,
        };
        let (cols, rows) = (self.cols, self.rows);
        if private {
            if fin == b'h' || fin == b'l' {
                let on = fin == b'h';
                for &m in &p {
                    match m {
                        25 => self.cursor_visible = on,
                        1 => self.app_cursor = on,
                        2004 => self.bracketed_paste = on,
                        1000 | 1002 | 1003 => {
                            let m = match m { 1000 => Mouse::Click, 1002 => Mouse::Drag, _ => Mouse::Motion };
                            if on {
                                self.mouse = m;
                            } else if self.mouse == m {
                                self.mouse = Mouse::Off;
                            }
                        }
                        1006 => self.mouse_sgr = on,
                        1004 => self.focus_events = on,
                        47 | 1047 | 1049 => self.set_alt(on),
                        _ => {}
                    }
                }
            }
            return;
        }
        if matches!(fin, b'A'..=b'H' | b'f' | b'd') {
            self.wrap_pending = false;
        }
        let (i, row) = (self.cy * cols + self.cx, self.cy * cols);
        match fin {
            b'A' => self.cy = self.cy.saturating_sub(n(0, 1)),
            b'B' => self.cy = (self.cy + n(0, 1)).min(rows - 1),
            b'C' => self.cx = (self.cx + n(0, 1)).min(cols - 1),
            b'D' => self.cx = self.cx.saturating_sub(n(0, 1)),
            b'E' => (self.cx, self.cy) = (0, (self.cy + n(0, 1)).min(rows - 1)),
            b'F' => (self.cx, self.cy) = (0, self.cy.saturating_sub(n(0, 1))),
            b'G' => self.cx = (n(0, 1) - 1).min(cols - 1),
            b'd' => self.cy = (n(0, 1) - 1).min(rows - 1),
            b'H' | b'f' => {
                self.cy = (n(0, 1) - 1).min(rows - 1);
                self.cx = (n(1, 1) - 1).min(cols - 1);
            }
            b'J' => match p[0] {
                0 => self.fill(i, cols * rows),
                1 => self.fill(0, i + 1),
                // erase saved lines (`clear`, `cls`). Ignored while a TUI that asked for focus events is running, and
                // for a moment after a resize: Claude Code and ConPTY send `2J 3J` on every full repaint, i.e. on
                // each resize and focus change, which would wipe the scrollback for good
                3 => {
                    let settled = self.resized_at.is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(1500));
                    if self.alt.is_none() && !self.focus_events && settled {
                        self.history.clear();
                        self.scroll = 0;
                        self.prompt = None;
                    }
                }
                _ => self.fill(0, cols * rows),
            },
            b'K' => match p[0] {
                0 => self.fill(i, row + cols),
                1 => self.fill(row, i + 1),
                _ => self.fill(row, row + cols),
            },
            b'L' if (self.top..=self.bot).contains(&self.cy) => self.scroll_down(self.cy, self.bot, n(0, 1)),
            b'M' if (self.top..=self.bot).contains(&self.cy) => self.scroll_up(self.cy, self.bot, n(0, 1)),
            b'S' => self.scroll_up(self.top, self.bot, n(0, 1)),
            b'T' => self.scroll_down(self.top, self.bot, n(0, 1)),
            b'@' => {
                let k = n(0, 1).min(cols - self.cx);
                self.cells.copy_within(i..row + cols - k, i + k);
                self.fill(i, i + k);
            }
            b'P' => {
                let k = n(0, 1).min(cols - self.cx);
                self.cells.copy_within(i + k..row + cols, i);
                self.fill(row + cols - k, row + cols);
            }
            b'X' => self.fill(i, i + n(0, 1).min(cols - self.cx)),
            b'm' => self.sgr(&p),
            b'r' => {
                let (t, b) = (n(0, 1) - 1, n(1, rows).min(rows) - 1);
                if t < b {
                    (self.top, self.bot) = (t, b);
                }
                (self.cx, self.cy) = (0, 0);
            }
            b's' => self.saved = (self.cx, self.cy),
            b'u' => self.restore_cursor(),
            b'n' => match p[0] {
                6 => self.reply.extend(format!("\x1b[{};{}R", self.cy + 1, self.cx + 1).bytes()),
                5 => self.reply.extend(b"\x1b[0n"),
                _ => {}
            },
            b'c' => self.reply.extend(b"\x1b[?1;2c"),
            _ => {}
        }
    }

    fn sgr(&mut self, p: &[usize]) {
        let mut i = 0;
        while i < p.len() {
            match p[i] {
                0 => (self.fg, self.bg, self.reverse, self.dim) = (Color::Fg, Color::Bg, false, false),
                2 => self.dim = true,
                22 => self.dim = false,
                7 => self.reverse = true,
                27 => self.reverse = false,
                30..=37 => self.fg = Color::Ansi((p[i] - 30) as u8),
                39 => self.fg = Color::Fg,
                40..=47 => self.bg = Color::Ansi((p[i] - 40) as u8),
                49 => self.bg = Color::Bg,
                90..=97 => self.fg = Color::Ansi((p[i] - 90 + 8) as u8),
                100..=107 => self.bg = Color::Ansi((p[i] - 100 + 8) as u8),
                38 | 48 => {
                    let is_fg = p[i] == 38;
                    let color = match p.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            p.get(i).map(|&n| color256(n))
                        }
                        Some(2) => {
                            i += 4;
                            match p.get(i - 2..=i) {
                                Some([r, g, b]) => Some(Color::Rgb((*r as u32 & 255) << 16 | (*g as u32 & 255) << 8 | (*b as u32 & 255))),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = color {
                        if is_fg { self.fg = c } else { self.bg = c }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Term {
        fn dump(&self) -> String {
            let rows: Vec<String> = self.cells.chunks(self.cols).map(|r| r.iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect();
            rows.join("\n")
        }
    }

    #[test]
    fn copy_joins_a_path_the_program_split_but_not_word_wrapped_prose() {
        let mut t = Term::new(12, 4);
        t.feed(b"  D:\\work\\en\r\n  gine\\x .\r\n  hello you\r\n  there");
        assert_eq!(t.text_range((0, 0), (3, 11)), "  D:\\work\\engine\\x .\n  hello you\n  there");
    }

    #[test]
    fn faint_text_is_marked_dim() {
        let mut t = Term::new(10, 1);
        t.feed(b"a[2mb[22mc[2md[0me");
        let dims: Vec<bool> = t.cells[..5].iter().map(|c| c.dim).collect();
        assert_eq!(dims, [false, true, false, true, false]);
    }

    #[test]
    fn grid_behaviour() {
        let mut t = Term::new(10, 3);
        t.feed(b"hi\r\nyo");
        assert_eq!(t.dump(), "hi\nyo\n");
        t.feed(b"\x1b[2J\x1b[2;3Hx");
        assert_eq!(t.dump(), "\n  x\n");

        let mut t = Term::new(4, 2); // scroll off the top
        t.feed(b"a\r\nb\r\nc");
        assert_eq!(t.dump(), "b\nc");

        let mut t = Term::new(3, 2); // wrap
        t.feed(b"abcd");
        assert_eq!(t.dump(), "abc\nd");

        let mut t = Term::new(10, 2); // erase line + backspace overwrite
        t.feed(b"hello\x08\x08\x1b[Kxy");
        assert_eq!(t.dump(), "helxy\n");

        let mut t = Term::new(10, 2); // utf8 split across chunks
        t.feed(&[0xe2, 0x82]);
        t.feed(&[0xac]);
        assert_eq!(t.cells[0].ch, '€');
    }

    #[test]
    fn colors_and_replies() {
        let mut t = Term::new(10, 2);
        t.feed(b"\x1b[31mA\x1b[0mB\x1b[38;2;1;2;3mC\x1b[7mD");
        assert_eq!(t.cells[0].fg, Color::Ansi(1));
        assert_eq!(t.cells[1].fg, Color::Fg);
        assert_eq!(t.cells[2].fg, Color::Rgb(0x010203));
        assert_eq!((t.cells[3].fg, t.cells[3].bg), (Color::Bg, Color::Rgb(0x010203)));

        let mut t = Term::new(10, 3);
        t.feed(b"\x1b[2;4H\x1b[6n");
        assert_eq!(t.reply, b"\x1b[2;4R");
        t.feed(b"\x1b]0;title\x07ok"); // OSC swallowed, text after it kept
        assert_eq!(t.dump(), "\n   ok\n");
    }

    #[test]
    fn scrollback_and_selection() {
        let mut t = Term::new(4, 2);
        t.feed(b"a\r\nb\r\nc\r\nd");
        assert_eq!(t.history.len(), 2);
        assert_eq!(t.text_range((0, 0), (3, 3)), "a\nb\nc\nd");
        assert_eq!(t.text_range((3, 0), (0, 0)), "a\nb\nc\nd"); // reversed anchor
        t.scroll_by(1);
        assert_eq!(t.line(t.view_start())[0].ch, 'b');
        t.scroll_by(99);
        assert_eq!(t.scroll, 2); // clamped
        t.feed(b"\r\ne"); // output while scrolled back keeps the place
        assert_eq!(t.line(t.view_start())[0].ch, 'a');
        t.resized_at = Some(std::time::Instant::now());
        t.feed(b"\x1b[3J"); // a repaint right after a resize: scrollback and screen both kept
        assert_eq!((t.history.len(), t.dump().as_str()), (3, "d\ne"));
        t.resized_at = None;
        t.feed(b"\x1b[2J\x1b[3J"); // `clear`: the scrollback goes too
        assert_eq!((t.history.len(), t.scroll), (0, 0));
    }

    #[test]
    fn alt_screen_wide_and_osc() {
        let mut t = Term::new(6, 2);
        t.feed(b"x\x1b[?1049hy\x1b[?1049l");
        assert_eq!(t.dump(), "x\n");
        assert!(t.history.is_empty());

        let mut t = Term::new(6, 2);
        t.feed("a日b".as_bytes());
        assert_eq!((t.cells[1].ch, t.cells[2].ch, t.cells[3].ch, t.cx), ('日', '\0', 'b', 4));

        let mut t = Term::new(6, 2);
        t.feed(b"\x1b]9;9;\"C:\\dev\"\x1b\\");
        assert_eq!(t.cwd.as_deref(), Some("C:\\dev"));
        t.feed(b"\x1b]7;file://host/C:/dev/my%20dir\x07");
        assert_eq!(t.cwd.as_deref(), Some("C:/dev/my dir"));
    }

    #[test]
    fn saved_cursor_survives_a_shrink_and_history_is_trimmed() {
        let mut t = Term::new(10, 10);
        t.feed(b"\x1b[9;9H\x1b7\x1b[1;1H"); // save at row 9, col 9, then go home
        t.resize(5, 3);
        t.feed(b"\x1b8"); // used to leave the cursor past the grid, so the next char panicked
        assert_eq!((t.cx, t.cy), (4, 2));
        t.feed(b"x");
        assert_eq!(t.cells[2 * 5 + 4].ch, 'x');

        let mut t = Term::new(80, 2);
        t.feed(b"ab\r\n\r\n\r\n");
        assert_eq!(t.history[0].cells.len(), 2, "trailing blanks are not stored");
        assert_eq!(t.text_range((0, 0), (0, 79)), "ab");
    }

    #[test]
    fn prompt_mark_follows_its_line() {
        let mut t = Term::new(6, 3);
        t.feed(b"out\r\n\x1b]133;A\x07   abcdefgh"); // prompt on row 1, the typed text wraps
        assert!(t.prompt == Some((1, 0)) && t.at_prompt(), "the cursor is on the wrapped row of the prompt's line");

        let mut t = Term::new(10, 2);
        t.feed(b"   \x1b]133;A\x07dir\r\nfile\r\n   "); // ConPTY: the marker came before the last command's echo and output
        assert!(t.prompt == Some((2, 0)) && t.at_prompt(), "the prompt is on the line after them (scrolled)");

        let mut t = Term::new(6, 3);
        t.feed(b"\x1b]133;A\x07   abcdefgh"); // cursor on the wrapped second row
        t.resize(20, 3);
        assert_eq!((t.prompt, t.cy), (Some((0, 0)), 0), "joined back: still the start of the cursor's line");
        t.resize(4, 3);
        assert_eq!(t.prompt, Some((t.history.len() + t.cy - 2, 0)), "split over 3 rows: the first of them");

        let mut t = Term::new(6, 2);
        t.feed(b"\x1b[?1049h\x1b]133;A\x07");
        assert_eq!(t.prompt, None, "not inside a full-screen app");
    }

    #[test]
    fn find_and_word_at() {
        let mut t = Term::new(8, 2);
        t.feed(b"Foo foo\r\nab foOx\r\nzz");
        assert_eq!(t.find("FOO"), [(0, 0, 2), (0, 4, 6), (1, 3, 5)], "every line, history first, any case");
        assert!(t.find("").is_empty() && t.find("nope").is_empty());

        let mut t = Term::new(5, 3);
        t.feed(b"see https://x.y/z ok"); // the link wraps over two rows
        assert_eq!(t.word_at(0, 4).as_deref(), Some("https://x.y/z"));
        assert_eq!(t.word_at(1, 2).as_deref(), Some("https://x.y/z"), "clicked on the second row of it");
        assert_eq!(t.word_at(0, 3), None, "a blank");
        assert_eq!(t.word_at(3, 4).as_deref(), Some("ok"));
        t.feed("\r\n日x".as_bytes());
        assert_eq!(t.word_at(t.history.len() + t.cy, 1).as_deref(), Some("日x"), "right half of a wide char");
    }

    #[test]
    fn resize_rewraps_lines_and_keeps_the_cursor_on_its_character() {
        let mut t = Term::new(10, 3);
        t.feed(b"abcdefgh\r\nxy");
        t.resize(3, 6); // narrow: the long line splits, the newline-ended one stays its own line
        assert_eq!(t.dump(), "abc\ndef\ngh\nxy\n\n");
        assert_eq!((t.cx, t.cy), (2, 3), "still right after the y");
        t.resize(10, 3); // and back: joined again
        assert_eq!(t.dump(), "abcdefgh\nxy\n");
        assert_eq!((t.cx, t.cy), (2, 1));

        let mut t = Term::new(4, 2); // lines that went into history rewrap too
        t.feed(b"abcdefgh\r\nxy");
        assert_eq!(t.history.len(), 1);
        t.resize(8, 2);
        assert_eq!((t.history.len(), t.dump().as_str()), (0, "abcdefgh\nxy"));
        t.resize(2, 2); // narrower than before: pieces go back into history, the cursor's line stays on screen
        assert_eq!(t.text_range((0, 0), (t.total_lines() - 1, 9)), "abcdefgh\nxy"); // wrapped pieces copy as one line
        assert_eq!((t.cx, t.cy, t.line(t.history.len() + t.cy)[0].ch), (1, 1, 'x'));

        let mut t = Term::new(6, 2); // a wide char is never cut in half
        t.feed("ab日c".as_bytes());
        t.resize(3, 3);
        assert_eq!(t.dump(), "ab\n日\0c\n"); // '\0': the wide char's right half

        let mut t = Term::new(6, 2); // the alternate screen is cropped; the main screen behind it still rewraps
        t.feed(b"abcdefgh\x1b[?1049hzz");
        t.resize(3, 4);
        assert_eq!(t.dump(), "zz\n\n\n");
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.dump(), "abc\ndef\ngh\n");
    }

    #[test]
    fn mouse_reports_follow_the_modes_asked_for() {
        let mut t = Term::new(10, 3);
        assert_eq!(t.mouse_report(MouseEv::WheelUp, 0, 0, 0), None, "off until the program asks");
        t.feed(b"[?1000h[?1006h[?1004h");
        assert!(t.focus_events);
        let r = |t: &Term, ev, m| t.mouse_report(ev, 4, 1, m).map(|b| String::from_utf8(b).unwrap());
        assert_eq!(r(&t, MouseEv::WheelUp, 0).as_deref(), Some("[<64;5;2M"));
        assert_eq!(r(&t, MouseEv::WheelDown, 16).as_deref(), Some("[<81;5;2M"), "ctrl");
        assert_eq!(r(&t, MouseEv::Press(0), 0).as_deref(), Some("[<0;5;2M"));
        assert_eq!(r(&t, MouseEv::Release(0), 0).as_deref(), Some("[<0;5;2m"));
        assert_eq!(r(&t, MouseEv::Move(Some(0)), 0), None, "1000 reports no motion");
        t.feed(b"[?1002h");
        assert_eq!(r(&t, MouseEv::Move(Some(0)), 0).as_deref(), Some("[<32;5;2M"));
        assert_eq!(r(&t, MouseEv::Move(None), 0), None, "1002: only while a button is held");
        t.feed(b"[?1003h");
        assert_eq!(r(&t, MouseEv::Move(None), 0).as_deref(), Some("[<35;5;2M"));
        t.feed(b"[?1006l");
        assert_eq!(t.mouse_report(MouseEv::Release(0), 4, 1, 0), Some(b"[M#%\"".to_vec()), "legacy bytes");
        t.feed(b"[?1003l[?1004l");
        assert_eq!((t.mouse, t.focus_events), (Mouse::Off, false));
        t.feed(b"[?1000hc");
        assert_eq!(t.mouse, Mouse::Off, "a reset turns it off");
    }

    /// Replays a `MAULA_RECORD` file and prints every place the history shrank, with the bytes just before it:
    /// `MAULA_REPLAY=dir/pane-0.out cargo test replay -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn replay() {
        let data = std::fs::read(std::env::var("MAULA_REPLAY").expect("MAULA_REPLAY=<file>")).unwrap();
        let mark = b"\x1b_maula-resize;";
        let mut t = Term::new(80, 24);
        let (mut i, mut max) = (0, 0);
        while i < data.len() {
            if data[i..].starts_with(mark) {
                let end = i + data[i..].windows(2).position(|w| w == b"\x1b\\").unwrap();
                let s = std::str::from_utf8(&data[i + mark.len()..end]).unwrap();
                let (c, r) = s.split_once(';').unwrap();
                let before = t.history.len();
                t.resize(c.parse().unwrap(), r.parse().unwrap());
                println!("@{i}: resize {s}  history {before} -> {}", t.history.len());
                i = end + 2;
                continue;
            }
            let before = t.history.len();
            t.feed(&data[i..i + 1]);
            max = max.max(t.history.len());
            if t.history.len() < before {
                let ctx = String::from_utf8_lossy(&data[i.saturating_sub(120)..=i]).escape_debug().to_string();
                println!("@{i}: history {before} -> {}  after: {ctx}", t.history.len());
            }
            i += 1;
        }
        println!("end: history {} (max {max}), alt screen {}", t.history.len(), t.alt.is_some());
    }
}
