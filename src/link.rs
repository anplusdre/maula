//! Ctrl+click on terminal text: web links open in the browser, file paths (`src/main.rs:12:5`) in the editor.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, PartialEq)]
pub enum Target {
    Url(String),
    File(PathBuf, Option<String>), // the path, and its "line[:col]" suffix
}

/// What a clicked word points at: an http(s) link anywhere in it (markdown `[x](https://…)` too), else a path,
/// relative to `cwd`, that exists.
pub fn parse(word: &str, cwd: &Path) -> Option<Target> {
    const LEAD: &[char] = &['(', '[', '{', '<', '\'', '"', '`'];
    const TRAIL: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '>', '\'', '"', '`'];
    if let Some(i) = word.find("https://").or_else(|| word.find("http://")) {
        return Some(Target::Url(word[i..].trim_end_matches(TRAIL).to_string()));
    }
    let w = word.trim_start_matches(LEAD).trim_end_matches(TRAIL);
    let (mut path, mut pos) = (w, None);
    for _ in 0..2 {
        // peel ":12", then ":5" of ":12:5"
        match path.rsplit_once(':') {
            Some((p, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                (path, pos) = (p, Some(w[p.len() + 1..].to_string()));
            }
            _ => break,
        }
    }
    let full = cwd.join(path);
    (!path.is_empty() && full.exists()).then_some(Target::File(full, pos))
}

/// Opens `t`: a link in the browser; a file in `editor` (e.g. "code -g", which is given `path:line:col`) or, without
/// one, shown selected in the file manager. Files are never opened with their default app: a printed `x.exe` must not run.
pub fn open(t: &Target, editor: Option<&str>) {
    let cmd = match t {
        Target::Url(u) => Some(browser(u)),
        Target::File(p, pos) => {
            let mut words: Vec<String> = editor.unwrap_or("").split_whitespace().map(String::from).collect();
            if words.is_empty() {
                Some(reveal(p))
            } else {
                words.push(match pos {
                    Some(pos) => format!("{}:{pos}", p.display()),
                    None => p.display().to_string(),
                });
                program(&words)
            }
        }
    };
    match cmd.map(|mut c| c.spawn()) {
        Some(Ok(mut child)) => drop(std::thread::spawn(move || child.wait())), // reaped, no zombie
        Some(Err(e)) => eprintln!("maula: cannot open {t:?}: {e}"),
        None => {}
    }
}

#[cfg(windows)]
fn browser(url: &str) -> Command {
    let mut c = Command::new("rundll32"); // the URL handler: no shell parses the link
    c.args(["url.dll,FileProtocolHandler", url]);
    c
}

#[cfg(windows)]
fn reveal(p: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("explorer");
    c.raw_arg(format!("/select,\"{}\"", p.display()));
    c
}

/// Through cmd, since editors are often `code.cmd`, which Windows does not find by name. None if the path holds a
/// character cmd would act on.
#[cfg(windows)]
fn program(words: &[String]) -> Option<Command> {
    use std::os::windows::process::CommandExt;
    if words.last()?.contains(['&', '|', '<', '>', '^', '%']) {
        return None;
    }
    let mut c = Command::new("cmd");
    c.arg("/c").args(words).creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashes up
    Some(c)
}

#[cfg(not(windows))]
fn browser(url: &str) -> Command {
    let mut c = Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
    c.arg(url);
    c
}

#[cfg(not(windows))]
fn reveal(p: &Path) -> Command {
    if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg("-R").arg(p);
        c
    } else {
        let mut c = Command::new("xdg-open"); // ponytail: no portable "select this file", opens its folder
        c.arg(p.parent().unwrap_or(p));
        c
    }
}

#[cfg(not(windows))]
fn program(words: &[String]) -> Option<Command> {
    let mut c = Command::new(&words[0]);
    c.args(&words[1..]);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_paths() {
        let dir = std::env::temp_dir().join(format!("maula_link_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/x.rs"), "").unwrap();
        let url = |s: &str| Some(Target::Url(s.into()));
        assert_eq!(parse("https://a.b/c?d=1).", &dir), url("https://a.b/c?d=1"), "trailing punctuation is not part of it");
        assert_eq!(parse("[docs](https://x.y/z)", &dir), url("https://x.y/z"), "markdown link");
        assert_eq!(parse("src/x.rs:12:5,", &dir), Some(Target::File(dir.join("src/x.rs"), Some("12:5".into()))));
        assert_eq!(parse("'src/x.rs'", &dir), Some(Target::File(dir.join("src/x.rs"), None)));
        assert_eq!(parse("src/nope.rs:3", &dir), None, "only paths that exist");
        assert_eq!(parse("ftp://x", &dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
