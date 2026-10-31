//! Folder / drive picker behind the chevrons in a panel header: a search box over a list.

use std::fs::DirEntry;
use std::path::{Component, Path, PathBuf, Prefix};

/// Rows shown at once; the list scrolls to keep the selection in view.
pub const VISIBLE: usize = 10;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Dir,
    Drive,
    Space, // saved spaces to load; also the text box of a space tab being renamed
}

pub struct Picker {
    pub pane: usize, // the pane (Dir, Drive) or the space id (Space) it belongs to
    pub kind: Kind,
    pub base: PathBuf,   // folder being listed (Dir)
    pub query: String,
    pub caret: usize,    // byte offset into `query`, always on a char boundary
    pub sel: usize,      // index into `shown()`
    pub top: usize,      // first visible row
    pub current: String, // row that gets a check mark: the pane's drive
    start: PathBuf,      // where the picker opened: "." only appears once you browsed away
    names: Vec<(String, String)>, // (name, lowercase name), sorted
}

impl Picker {
    fn new(pane: usize, kind: Kind, base: PathBuf) -> Self {
        Picker { pane, kind, start: base.clone(), base, query: String::new(), caret: 0, sel: 0, top: 0, current: String::new(), names: Vec::new() }
    }

    /// Lists the sub-folders of `base`.
    pub fn dirs(pane: usize, base: PathBuf) -> Self {
        let mut p = Self::new(pane, Kind::Dir, base);
        p.list();
        p
    }

    /// Lists saved spaces by name, for the space with id `space`'s Load menu.
    pub fn saved(space: usize, names: impl IntoIterator<Item = String>) -> Self {
        let mut p = Self::new(space, Kind::Space, PathBuf::new());
        p.names = names.into_iter().map(|n| (n.clone(), n.to_lowercase())).collect();
        p
    }

    /// Just a text box holding `text`, caret at the end: the rename field of space `space`.
    pub fn text(space: usize, text: &str) -> Self {
        let mut p = Self::saved(space, []);
        (p.query, p.caret) = (text.to_string(), text.len());
        p
    }

    /// Lists the drives; `current` (the pane's own) starts highlighted.
    pub fn drives(pane: usize, current: Option<char>) -> Self {
        let mut p = Self::new(pane, Kind::Drive, PathBuf::new());
        p.current = current.map(|c| format!("{c}:")).unwrap_or_default();
        p.names = drives().into_iter().map(|d| (d.clone(), d.to_lowercase())).collect();
        p.sel = p.names.iter().position(|n| n.0 == p.current).unwrap_or(0);
        p.reveal();
        p
    }

    /// (Re)reads `base`. Hidden folders are never listed. ponytail: one read per folder entered, filtering is in memory.
    fn list(&mut self) {
        let mut v: Vec<String> = std::fs::read_dir(&self.base)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink() && e.path().is_dir()) && !hidden(e))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        v.sort_by_key(|n| n.to_lowercase());
        self.names = v.into_iter().map(|n| (n.clone(), n.to_lowercase())).collect();
        (self.query, self.caret, self.sel, self.top) = (String::new(), 0, 0, 0);
    }

    /// The rows for the current query: with an empty query "." (this folder, once browsed away) and ".." come
    /// first; then the names containing the query, those starting with it before the rest.
    pub fn shown(&self) -> Vec<&str> {
        let q = self.query.to_lowercase();
        let mut hits: Vec<&(String, String)> = self.names.iter().filter(|n| n.1.contains(&q)).collect();
        hits.sort_by_key(|n| !n.1.starts_with(&q)); // stable: alphabetical within each group
        let mut rows = Vec::new();
        if self.kind == Kind::Dir && q.is_empty() {
            if self.base != self.start {
                rows.push(".");
            }
            if self.base.parent().is_some() {
                rows.push("..");
            }
        }
        rows.extend(hits.into_iter().map(|n| n.0.as_str()));
        rows
    }

    /// The folder a row stands for.
    pub fn path_of(&self, row: &str) -> PathBuf {
        match row {
            "." => self.base.clone(),
            ".." => self.base.parent().unwrap_or(&self.base).to_path_buf(),
            _ => self.base.join(row),
        }
    }

    /// Browse into row `k` (list its sub-folders instead of leaving).
    pub fn enter(&mut self, k: usize) {
        let Some(row) = self.shown().get(k).map(|r| r.to_string()) else { return };
        if self.kind == Kind::Dir && row != "." {
            self.base = self.path_of(&row);
            self.list();
        }
    }

    pub fn up(&mut self) {
        if let Some(p) = self.base.parent().map(Path::to_path_buf) {
            self.base = p;
            self.list();
        }
    }

    /// Types `s` at the caret.
    pub fn push(&mut self, s: &str) {
        self.query.insert_str(self.caret, s);
        self.caret += s.len();
        (self.sel, self.top) = (0, 0);
    }

    /// Backspace: deletes the character before the caret; in an empty box, goes up a folder.
    pub fn pop(&mut self) {
        if let Some(c) = self.query[..self.caret].chars().next_back() {
            self.caret -= c.len_utf8();
            self.query.remove(self.caret);
            (self.sel, self.top) = (0, 0);
        } else if self.query.is_empty() {
            self.up();
        }
    }

    /// Delete: removes the character after the caret.
    pub fn delete(&mut self) {
        if self.caret < self.query.len() {
            self.query.remove(self.caret);
            (self.sel, self.top) = (0, 0);
        }
    }

    /// Moves the caret one character left (`d < 0`) or right.
    pub fn move_caret(&mut self, d: isize) {
        if d < 0 {
            self.caret -= self.query[..self.caret].chars().next_back().map_or(0, char::len_utf8);
        } else {
            self.caret += self.query[self.caret..].chars().next().map_or(0, char::len_utf8);
        }
    }

    /// Scrolls the visible window by `d` rows (the wheel), leaving the highlight where it is.
    pub fn scroll(&mut self, d: isize) {
        let max = self.shown().len().saturating_sub(VISIBLE) as isize;
        self.top = (self.top as isize + d).clamp(0, max) as usize;
    }

    /// Moves the highlight by `d` rows; it stops at the first and last row.
    pub fn step(&mut self, d: isize) {
        let n = self.shown().len() as isize;
        if n > 0 {
            self.sel = (self.sel as isize + d).clamp(0, n - 1) as usize;
            self.reveal();
        }
    }

    fn reveal(&mut self) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + VISIBLE {
            self.top = self.sel + 1 - VISIBLE;
        }
    }
}

fn hidden(e: &DirEntry) -> bool {
    #[cfg(windows)]
    if e.metadata().is_ok_and(|m| std::os::windows::fs::MetadataExt::file_attributes(&m) & 0x6 != 0) {
        return true; // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM: $Recycle.Bin, System Volume Information...
    }
    e.file_name().to_string_lossy().starts_with('.')
}

/// Drive roots present, as "C:".
#[cfg(windows)]
pub fn drives() -> Vec<String> {
    unsafe extern "system" {
        safe fn GetLogicalDrives() -> u32; // kernel32: a bitmask, no disk access (std already links it)
    }
    let mask = GetLogicalDrives();
    (0..26u8).filter(|i| mask >> i & 1 == 1).map(|i| format!("{}:", (b'A' + i) as char)).collect()
}

#[cfg(not(windows))]
pub fn drives() -> Vec<String> {
    Vec::new()
}

/// The drive letter of `p` ("C" for `C:\x`), if it is on one.
pub fn drive_of(p: &Path) -> Option<char> {
    match p.components().next()? {
        Component::Prefix(x) => match x.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d.to_ascii_uppercase() as char),
            _ => None,
        },
        _ => None,
    }
}

/// The line that moves `shell` to `dir`: cmd needs `/d` to change drive, PowerShell takes a literal path.
/// ponytail: PowerShell, cmd and POSIX shells only; anything else gets the POSIX form.
pub fn cd_line(shell: Option<&str>, dir: &Path) -> String {
    let d = dir.display().to_string();
    match flavor(shell) {
        Sh::Cmd => format!("cd /d \"{d}\"\r"),
        Sh::Ps => format!("cd -LiteralPath '{}'\r", d.replace('\'', "''")),
        Sh::Posix => format!("cd '{}'\r", d.replace('\'', r"'\''")),
    }
}

/// The line that switches `shell` to drive `d` ("D:"): cmd and PowerShell take the bare drive (its last folder),
/// a POSIX shell (Git Bash) goes to its root.
pub fn drive_line(shell: Option<&str>, d: &str) -> String {
    match flavor(shell) {
        Sh::Posix => cd_line(shell, Path::new(&format!("{d}/"))),
        _ => format!("{d}\r"),
    }
}

enum Sh {
    Cmd,
    Ps,
    Posix,
}

fn flavor(shell: Option<&str>) -> Sh {
    let exe = shell.unwrap_or(if cfg!(windows) { "powershell.exe" } else { "sh" }).to_lowercase();
    if Path::new(&exe).file_stem().is_some_and(|s| s == "cmd") {
        Sh::Cmd
    } else if exe.contains("powershell") || exe.contains("pwsh") {
        Sh::Ps
    } else {
        Sh::Posix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_filter_and_go_up() {
        let root = std::env::temp_dir().join(format!("maula_picker_{}", std::process::id()));
        for d in ["alpha", "Beta", "my_alpha", ".hidden", "alpha/inner"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("file.txt"), "").unwrap(); // files are not listed

        let mut p = Picker::dirs(0, root.clone());
        assert_eq!(p.shown(), ["..", "alpha", "Beta", "my_alpha"]); // sorted ignoring case, no hidden, no files
        p.push("ALP");
        assert_eq!(p.shown(), ["alpha", "my_alpha"], "case-insensitive; prefix matches first");
        p.push("x");
        assert!(p.shown().is_empty());
        for _ in 0..4 {
            p.pop();
        }
        assert_eq!((p.query.as_str(), &p.base), ("", &root), "Backspace deletes text first, it does not leave the folder");

        p.enter(1); // alpha
        assert_eq!(p.base, root.join("alpha"));
        assert_eq!(p.shown(), [".", "..", "inner"], "browsed away: this folder is selectable");
        assert_eq!(p.path_of("."), root.join("alpha"));
        assert_eq!(p.path_of(".."), root);
        p.pop(); // empty query: up
        assert_eq!(p.base, root);

        p.step(-1); // already on the first row: stays
        assert_eq!(p.sel, 0);
        p.step(10);
        assert_eq!(p.sel, 3, "stops on the last row, no wrap-around");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn saved_spaces_are_searched_by_name() {
        let mut p = Picker::saved(3, ["api".to_string(), "Web".to_string(), "my-api".to_string()]);
        assert_eq!(p.shown(), ["api", "Web", "my-api"], "no '.' or '..' rows: not folders");
        p.push("API");
        assert_eq!(p.shown(), ["api", "my-api"]);
        for _ in 0..3 {
            p.pop();
        }
        p.pop(); // empty box: nothing to go up to
        assert_eq!(p.shown().len(), 3);
        let t = Picker::text(1, "Space 1");
        assert_eq!((t.query.as_str(), t.caret), ("Space 1", 7));
    }

    #[test]
    fn editing_happens_at_the_caret() {
        let mut p = Picker::new(0, Kind::Dir, PathBuf::new());
        p.push("héllo"); // multi-byte char: the caret must stay on boundaries
        p.move_caret(-1);
        p.move_caret(-1);
        p.move_caret(-1);
        p.push("X");
        assert_eq!((p.query.as_str(), p.caret), ("héXllo", 4));
        p.pop(); // removes the X
        p.pop(); // removes the é
        assert_eq!((p.query.as_str(), p.caret), ("hllo", 1));
        p.delete();
        assert_eq!(p.query, "hlo");
        p.caret = 0;
        p.pop(); // caret at the start of a non-empty box: nothing to delete
        p.move_caret(-1);
        assert_eq!((p.query.as_str(), p.caret), ("hlo", 0));
        p.move_caret(1);
        p.move_caret(1);
        p.move_caret(1);
        p.move_caret(1);
        assert_eq!(p.caret, 3, "stops at the end");
    }

    #[test]
    fn long_lists_scroll_with_the_selection() {
        let mut p = Picker::new(0, Kind::Dir, PathBuf::new());
        p.names = (0..30).map(|i| (format!("d{i:02}"), format!("d{i:02}"))).collect();
        for _ in 0..VISIBLE {
            p.step(1);
        }
        assert_eq!((p.sel, p.top), (VISIBLE, 1), "the selection never leaves the window of rows");
        p.scroll(5); // the wheel moves the window, not the highlight
        assert_eq!((p.sel, p.top), (VISIBLE, 6));
        p.scroll(100);
        assert_eq!((p.sel, p.top), (VISIBLE, 30 - VISIBLE), "clamped at the bottom");
        p.scroll(-100);
        assert_eq!(p.top, 0, "and at the top");
        p.step(-100); // stops at the top
        assert_eq!((p.sel, p.top), (0, 0));
        p.step(100); // and at the bottom
        assert_eq!((p.sel, p.top), (29, 30 - VISIBLE));
    }

    #[test]
    fn cd_lines_per_shell() {
        let d = Path::new("D:/it's");
        assert_eq!(cd_line(Some("cmd.exe"), d), "cd /d \"D:/it's\"\r");
        assert_eq!(cd_line(Some("pwsh.exe"), d), "cd -LiteralPath 'D:/it''s'\r");
        assert_eq!(cd_line(Some("/bin/bash"), d), "cd 'D:/it'\\''s'\r");
        assert_eq!(drive_line(Some("pwsh.exe"), "D:"), "D:\r");
        assert_eq!(drive_line(Some(r"C:\Program Files\Git\bin\bash.exe"), "D:"), "cd 'D:/'\r");
    }

    #[cfg(windows)]
    #[test]
    fn drives_and_letters() {
        assert_eq!(drive_of(Path::new(r"d:\work")), Some('D'));
        assert_eq!(drive_of(Path::new(r"\\server\share\x")), None);
        assert!(drives().contains(&"C:".to_string()));
        let p = Picker::drives(0, Some('C'));
        assert_eq!(p.shown()[p.sel], "C:", "the pane's own drive starts highlighted");
    }
}
