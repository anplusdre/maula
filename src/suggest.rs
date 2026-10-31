//! `cd <3+ chars>` directory suggestions: lists sub-directories whose name contains the typed text.

use std::path::Path;

pub struct Suggest {
    /// The typed argument; used to remember a dismissal.
    pub key: String,
    /// Chars of the last path segment typed (what gets replaced on accept).
    pub frag_len: usize,
    /// Grid column where the fragment starts (popup anchor).
    pub col: usize,
    pub quoted: bool,
    pub items: Vec<String>,
    pub sel: usize,
}

/// The argument typed after the last `cd ` in `line` (the text left of the cursor).
pub fn cd_arg(line: &str) -> Option<&str> {
    for (i, _) in line.rmatch_indices("cd ") {
        let before_ok = line[..i].chars().next_back().is_none_or(|c| c.is_whitespace() || ">$#%".contains(c));
        if before_ok {
            return Some(&line[i + 3..]);
        }
    }
    None
}

pub fn find(cwd: &Path, arg: &str) -> Option<Suggest> {
    let quoted = arg.starts_with('"');
    let bare = arg.trim_start_matches('"');
    let (prefix, frag) = match bare.rfind(['/', '\\']) {
        Some(i) => bare.split_at(i + 1),
        None => ("", bare),
    };
    let frag_len = frag.chars().count();
    if frag_len < 3 {
        return None;
    }
    let needle = frag.to_lowercase();
    let mut items: Vec<String> = std::fs::read_dir(cwd.join(prefix))
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.to_lowercase().contains(&needle))
        .collect();
    if items.is_empty() || (items.len() == 1 && items[0].to_lowercase() == needle) {
        return None; // nothing to suggest, or already typed in full
    }
    items.sort_by_key(|n| (!n.to_lowercase().starts_with(&needle), n.to_lowercase()));
    items.truncate(8);
    Some(Suggest { key: arg.to_string(), frag_len, col: 0, quoted, items, sel: 0 })
}

impl Suggest {
    /// Keystrokes that replace the typed fragment with the selected directory.
    pub fn accept_bytes(&self) -> Vec<u8> {
        let name = &self.items[self.sel];
        let mut s = "\x7f".repeat(self.frag_len);
        let needs_quote = name.contains(|c: char| c.is_whitespace() || "&()[]{}^=;!'+,`~@#$%".contains(c));
        if self.quoted {
            s += name;
            s.push('"');
        } else if needs_quote {
            s = s + "\"" + name + "\"";
        } else {
            s += name;
        }
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert_eq!(cd_arg("PS C:\\x> cd alp"), Some("alp"));
        assert_eq!(cd_arg("abcd x"), None); // "cd " inside a word is not a cd command

        let dir = std::env::temp_dir().join(format!("maula_suggest_{}", std::process::id()));
        for d in ["alpha-project", "my_alpha", "beta", "has space alpha"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("alphafile"), "").unwrap(); // files are not suggested

        let s = find(&dir, "alp").unwrap();
        assert_eq!(s.items, ["alpha-project", "has space alpha", "my_alpha"]); // prefix matches first
        assert!(find(&dir, "al").is_none()); // under 3 chars
        assert!(find(&dir, "ALPHA-project").is_none()); // exact match: nothing left to suggest

        let mut s = find(&dir, "alp").unwrap();
        assert_eq!(s.accept_bytes(), b"\x7f\x7f\x7falpha-project");
        s.sel = 1;
        assert_eq!(s.accept_bytes(), b"\x7f\x7f\x7f\"has space alpha\"");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
