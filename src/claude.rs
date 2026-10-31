//! Claude Code integration. Claude's hooks run `maula hook`, which forwards the event to the maula window that owns
//! the terminal (found through MAULA_* variables every shell gets). The events tell maula which session runs in which
//! panel and when Claude stops or asks something; maula then reads the new part of that session's transcript to keep
//! the panel's memory: your prompts and Claude's explanations, nothing else.

use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Claude Code events maula listens to.
const EVENTS: [&str; 4] = ["SessionStart", "Stop", "Notification", "SessionEnd"];

/// Where a session's memory lives: in the project, so every Claude account can read it.
const MEMORY_DIR: &str = ".maula/memory";

const SUMMARY_PROMPT: &str = "Summarize this coding session for a future session that has no other context. \
Short markdown sections: What the user needed. Decisions made, each with the problem it solves. Open items. \
Be terse and output only the summary.";

/// One hook event, as forwarded to the window.
#[derive(Debug, Default)]
pub struct Hook {
    pub pane: usize,
    pub event: String,
    pub session: String,
    pub transcript: PathBuf,
    pub cwd: PathBuf,
    pub config_dir: Option<String>, // the Claude account (CLAUDE_CONFIG_DIR) the session runs under
}

// ---- the `maula hook` subcommand ----

/// Run by Claude Code for each event, with the event as JSON on stdin. Outside a maula terminal it does nothing.
/// Always exits 0: a hook error must never get in Claude's way.
pub fn hook_main() {
    let var = |k: &str| std::env::var(k).ok();
    let (Some(pane), Some(port), Some(token)) = (var("MAULA_PANE"), var("MAULA_PORT"), var("MAULA_TOKEN")) else { return };
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Ok(port) = port.parse::<u16>() else { return };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        let _ = write!(s, "{token}\n{pane}\n{}\n{input}", var("CLAUDE_CONFIG_DIR").unwrap_or_default());
    }
}

/// Listens on a random local port for `hook_main`; `on` gets every event. Returns (port, token): the shells get both,
/// and a connection without the token is ignored.
pub fn listen(on: impl Fn(Hook) + Send + 'static) -> Option<(u16, String)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let token = format!("{:x}{:x}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos());
    let want = token.clone();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut text = String::new();
            let _ = s.read_to_string(&mut text);
            if let Some(h) = parse_hook(&text, &want) {
                on(h);
            }
        }
    });
    Some((port, token))
}

/// `token \n pane \n config dir \n json` -> Hook
fn parse_hook(text: &str, token: &str) -> Option<Hook> {
    let mut it = text.splitn(4, '\n');
    if it.next()? != token {
        return None;
    }
    let pane = it.next()?.trim().parse().ok()?;
    let config_dir = Some(it.next()?.trim().to_string()).filter(|d| !d.is_empty());
    let v: Value = serde_json::from_str(it.next()?).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Some(Hook { pane, event: s("hook_event_name"), session: s("session_id"), transcript: s("transcript_path").into(), cwd: s("cwd").into(), config_dir })
}

// ---- a session's memory ----

/// A user prompt or one of Claude's explanations.
#[derive(Debug, PartialEq)]
pub struct Note {
    pub user: bool,
    pub text: String,
}

/// The Claude session running in a panel, and what has been read from its transcript so far.
pub struct Session {
    pub id: String,
    transcript: PathBuf,
    project: PathBuf,
    config_dir: Option<String>,
    offset: u64,          // transcript bytes already read
    pub notes: Vec<Note>, // kept in memory always; written to `file` only while the panel's memory is on
    written: usize,       // notes already in the file
    summarized: usize,    // notes covered by the last summary
}

impl Session {
    pub fn new(h: &Hook) -> Self {
        Session { id: h.session.clone(), transcript: h.transcript.clone(), project: h.cwd.clone(), config_dir: h.config_dir.clone(), offset: 0, notes: Vec::new(), written: 0, summarized: 0 }
    }

    fn file(&self) -> PathBuf {
        self.project.join(MEMORY_DIR).join(format!("session-{}.md", &self.id[..self.id.len().min(8)]))
    }

    /// Reads the transcript lines added since the last call (only whole lines: the last one may still be written).
    pub fn read(&mut self) {
        let Ok(mut f) = std::fs::File::open(&self.transcript) else { return };
        let mut buf = String::new();
        if f.seek(SeekFrom::Start(self.offset)).is_err() || f.read_to_string(&mut buf).is_err() {
            return;
        }
        let whole = buf.rfind('\n').map_or(0, |i| i + 1);
        self.notes.extend(buf[..whole].lines().filter_map(note));
        self.offset += whole as u64;
    }

    /// Appends the notes not yet in the memory file (all of them, the first time).
    pub fn flush(&mut self) {
        if self.written == self.notes.len() {
            return;
        }
        let dir = self.project.join(MEMORY_DIR);
        let _ = std::fs::create_dir_all(&dir);
        let ignore = self.project.join(".maula/.gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(ignore, "*\n"); // memory is personal: kept out of git
        }
        let text: String = self.notes[self.written..].iter().map(|n| format!("## {}\n\n{}\n\n", if n.user { "User" } else { "Claude" }, n.text.trim())).collect();
        let ok = std::fs::OpenOptions::new().create(true).append(true).open(self.file()).and_then(|mut f| f.write_all(text.as_bytes()));
        match ok {
            Ok(()) => self.written = self.notes.len(),
            Err(e) => eprintln!("maula: cannot write memory {}: {e}", self.file().display()),
        }
    }

    /// Asks Claude (Haiku, the session's own account) for a summary of the memory file, written to `latest.md`. The
    /// `claude` process writes the file itself, so it finishes even if maula exits first. Nothing new: nothing to do.
    /// ponytail: a failed run leaves its error text in latest.md.
    pub fn summarize(&mut self) {
        self.flush();
        if self.written == 0 || self.summarized == self.written {
            return;
        }
        self.summarized = self.written;
        let (Ok(input), Ok(output)) = (std::fs::File::open(self.file()), std::fs::File::create(self.project.join(MEMORY_DIR).join("latest.md"))) else { return };
        let mut cmd = claude_command();
        cmd.args(["-p", "--model", "haiku", SUMMARY_PROMPT]).current_dir(&self.project).stdin(input).stdout(output).stderr(std::process::Stdio::null());
        for k in ["MAULA_PANE", "MAULA_PORT", "MAULA_TOKEN"] {
            cmd.env_remove(k); // its own hooks must not report to a panel
        }
        if let Some(d) = &self.config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", d);
        }
        if let Err(e) = cmd.spawn() {
            eprintln!("maula: cannot run claude for the summary: {e}");
        }
    }
}

#[cfg(windows)]
fn claude_command() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd"); // `claude` may be claude.cmd, which Windows does not find by name
    c.args(["/c", "claude"]).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

#[cfg(not(windows))]
fn claude_command() -> std::process::Command {
    std::process::Command::new("claude")
}

/// The note in one transcript line: a prompt you typed, or the text of one of Claude's replies. Tool calls and
/// results, thinking, subagents, slash commands and system messages are skipped.
fn note(line: &str) -> Option<Note> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["isSidechain"].as_bool() == Some(true) || v["isMeta"].as_bool() == Some(true) {
        return None;
    }
    let content = &v["message"]["content"];
    let text = match (v["type"].as_str()?, content) {
        ("user", Value::String(s)) => s.clone(),
        (kind @ ("user" | "assistant"), Value::Array(parts)) => {
            if parts.iter().any(|p| p["type"] == "tool_result") {
                return None;
            }
            let texts: Vec<&str> = parts.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect();
            if kind == "user" && texts.is_empty() {
                return None;
            }
            texts.join("\n\n")
        }
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') {
        return None; // `<command-name>`, `<local-command-stdout>`, `<system-reminder>`...: not something you wrote
    }
    Some(Note { user: v["type"] == "user", text: text.to_string() })
}

// ---- installing the hooks ----

/// The command the hooks run: this very executable.
fn hook_command() -> Option<String> {
    Some(format!("\"{}\" hook", std::env::current_exe().ok()?.display().to_string().replace('\\', "/")))
}

/// A hook entry maula added (any build of it).
fn is_ours(h: &Value) -> bool {
    h["command"].as_str().is_some_and(|c| c.contains("maula") && c.ends_with(" hook"))
}

/// The settings.json of every Claude account on this machine: `~/.claude*` folders that hold projects, and
/// CLAUDE_CONFIG_DIR.
fn accounts() -> Vec<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = home
        .and_then(|h| std::fs::read_dir(h).ok())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".claud") && e.path().join("projects").is_dir())
        .map(|e| e.path())
        .collect();
    dirs.extend(std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    dirs.sort();
    dirs.dedup();
    dirs.into_iter().map(|d| d.join("settings.json")).collect()
}

/// `settings` with maula's hooks for `EVENTS` running `cmd`, replacing any older maula entries; everything else kept.
fn with_hooks(mut settings: Value, cmd: &str) -> Option<Value> {
    let hooks = settings.as_object_mut()?.entry("hooks").or_insert(json!({})).as_object_mut()?;
    for ev in EVENTS {
        let groups = hooks.entry(ev).or_insert(json!([])).as_array_mut()?;
        for g in groups.iter_mut() {
            if let Some(list) = g["hooks"].as_array_mut() {
                list.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|l| !l.is_empty()));
        groups.push(json!({ "hooks": [{ "type": "command", "command": cmd }] }));
    }
    Some(settings)
}

/// Whether `settings` already runs `cmd` for every event.
fn has_hooks(settings: &Value, cmd: &str) -> bool {
    EVENTS.iter().all(|ev| settings["hooks"][ev].as_array().is_some_and(|gs| gs.iter().any(|g| g["hooks"].as_array().is_some_and(|l| l.iter().any(|h| h["command"] == cmd)))))
}

fn read_settings(path: &Path) -> Option<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).ok(), // unreadable JSON: never overwrite it
        Err(_) => Some(json!({})),
    }
}

/// True once every account runs this executable's hooks.
pub fn hooks_installed() -> bool {
    let Some(cmd) = hook_command() else { return false };
    let all = accounts();
    !all.is_empty() && all.iter().all(|p| read_settings(p).is_some_and(|s| has_hooks(&s, &cmd)))
}

/// Adds the hooks to every account's settings.json (a copy of the old file is kept as settings.json.maula-bak).
pub fn install_hooks() {
    let Some(cmd) = hook_command() else { return };
    for path in accounts() {
        let Some(new) = read_settings(&path).and_then(|s| with_hooks(s, &cmd)) else {
            eprintln!("maula: {} is not valid JSON, left alone", path.display());
            continue;
        };
        if path.exists() {
            let _ = std::fs::copy(&path, path.with_extension("json.maula-bak"));
        }
        let text = serde_json::to_string_pretty(&new).unwrap_or_default();
        if let Err(e) = std::fs::write(&path, text + "\n") {
            eprintln!("maula: cannot write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_lines_become_notes() {
        let user = |t: &str| Some(Note { user: true, text: t.into() });
        assert_eq!(note(r#"{"type":"user","message":{"role":"user","content":"fix the bug"}}"#), user("fix the bug"));
        assert_eq!(note(r#"{"type":"user","message":{"content":[{"type":"text","text":"see image"},{"type":"image"}]}}"#), user("see image"));
        assert_eq!(note(r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#), None, "tool output");
        assert_eq!(note(r#"{"type":"user","isMeta":true,"message":{"content":"Caveat"}}"#), None);
        assert_eq!(note(r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#), None, "slash command");
        let reply = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"Done."},{"type":"tool_use","name":"Bash"}]}}"#;
        assert_eq!(note(reply), Some(Note { user: false, text: "Done.".into() }), "only the text, no thinking or tool calls");
        assert_eq!(note(r#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#), None);
        assert_eq!(note(r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}]}}"#), None, "subagent");
        assert_eq!(note("not json"), None);
    }

    #[test]
    fn session_reads_new_whole_lines_and_writes_memory() {
        let dir = std::env::temp_dir().join(format!("maula_claude_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tr = dir.join("t.jsonl");
        std::fs::write(&tr, "{\"type\":\"user\",\"message\":{\"content\":\"one\"}}\n{\"type\":\"user\",\"mess").unwrap(); // second line half-written
        let mut s = Session::new(&Hook { session: "abcdef123456".into(), transcript: tr.clone(), cwd: dir.clone(), ..Default::default() });
        s.read();
        assert_eq!(s.notes.len(), 1);
        std::fs::OpenOptions::new().append(true).open(&tr).unwrap().write_all(b"age\":{\"content\":\"two\"}}\n").unwrap();
        s.read();
        assert_eq!(s.notes.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["one", "two"], "the half line is read once complete");
        s.flush();
        s.flush(); // nothing new: nothing appended
        let text = std::fs::read_to_string(dir.join(".maula/memory/session-abcdef12.md")).unwrap();
        assert_eq!(text, "## User\n\none\n\n## User\n\ntwo\n\n");
        assert_eq!(std::fs::read_to_string(dir.join(".maula/.gitignore")).unwrap(), "*\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hooks_are_added_once_and_keep_the_rest() {
        let old = json!({ "model": "opus", "hooks": { "Stop": [{ "hooks": [
            { "type": "command", "command": "\"D:/old/maula.exe\" hook" },
            { "type": "command", "command": "say done" }
        ] }] } });
        let cmd = "\"D:/new/maula.exe\" hook";
        let new = with_hooks(old, cmd).unwrap();
        assert!(has_hooks(&new, cmd));
        assert_eq!(new["model"], "opus", "other settings kept");
        let stop = new["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop[0]["hooks"], json!([{ "type": "command", "command": "say done" }]), "the old maula entry is replaced, other hooks stay");
        assert_eq!(with_hooks(new.clone(), cmd).unwrap(), new, "installing again changes nothing");
        assert!(!has_hooks(&json!({}), cmd));
    }

    #[test]
    fn hook_messages_need_the_token() {
        let msg = "tok\n3\nC:/Users/x/.claude-work\n{\"hook_event_name\":\"Stop\",\"session_id\":\"s1\",\"transcript_path\":\"C:/t.jsonl\",\"cwd\":\"D:/p\"}";
        let h = parse_hook(msg, "tok").unwrap();
        assert_eq!((h.pane, h.event.as_str(), h.session.as_str(), h.config_dir.as_deref()), (3, "Stop", "s1", Some("C:/Users/x/.claude-work")));
        assert!(parse_hook(msg, "other").is_none());
        assert!(parse_hook("tok\n3\n\n{}", "tok").unwrap().config_dir.is_none());
    }
}
