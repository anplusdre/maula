//! Lets Claude Code read and write the note of the pane it runs in: `maula note read|write|append|clear`.
//! The CLI talks over loopback TCP to the running window (same token as `claude::listen`); the pane comes from
//! MAULA_PANE. Plain text only. Wire format, request: `token \n pane \n op \n body`, then the client closes its
//! write side; reply: `ok \n text` or `err \n message`.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Largest note text accepted.
pub const MAX_BYTES: usize = 8 << 20;

/// Port of the listener, read by `spawn_pty` to give every shell MAULA_NOTE_PORT. 0: not listening.
pub static PORT: AtomicU16 = AtomicU16::new(0);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Read,
    Write,
    Append,
    Clear,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Append => "append",
            Op::Clear => "clear",
        }
    }
    fn parse(s: &str) -> Option<Op> {
        [Op::Read, Op::Write, Op::Append, Op::Clear].into_iter().find(|o| o.name() == s)
    }
}

/// A request for the window; answer through `reply`: Ok(note text after the operation) or Err(message).
pub struct Request {
    pub pane: usize,
    pub op: Op,
    pub text: String,
    pub reply: mpsc::Sender<Result<String, String>>,
}

pub fn encode_request(token: &str, pane: usize, op: Op, text: &str) -> Vec<u8> {
    format!("{token}\n{pane}\n{}\n{text}", op.name()).into_bytes()
}

/// -> (pane, op, text). Ok(None): wrong token or malformed, ignored. Err: text not UTF-8 or over MAX_BYTES.
pub fn parse_request(raw: &[u8], token: &str) -> Result<Option<(usize, Op, String)>, String> {
    let s = std::str::from_utf8(raw).map_err(|_| "text is not valid UTF-8".to_string())?;
    let mut it = s.splitn(4, '\n');
    if it.next() != Some(token) {
        return Ok(None);
    }
    let (Some(pane), Some(op), Some(text)) = (it.next().and_then(|p| p.trim().parse().ok()), it.next().and_then(Op::parse), it.next()) else { return Ok(None) };
    if text.len() > MAX_BYTES {
        return Err(format!("text is larger than {MAX_BYTES} bytes"));
    }
    Ok(Some((pane, op, text.to_string())))
}

pub fn encode_reply(r: &Result<String, String>) -> Vec<u8> {
    match r {
        Ok(t) => format!("ok\n{t}"),
        Err(e) => format!("err\n{e}"),
    }
    .into_bytes()
}

pub fn parse_reply(raw: &[u8]) -> Result<String, String> {
    let s = String::from_utf8_lossy(raw);
    match s.split_once('\n') {
        Some(("ok", t)) => Ok(t.to_string()),
        Some(("err", e)) => Err(e.to_string()),
        _ => Err("unexpected reply from maula".into()),
    }
}

/// Starts the loopback listener (token: the one the shells get in MAULA_TOKEN); `on` gets each valid request and
/// should hand it to the UI thread, which answers through `Request::reply`.
pub fn listen(token: String, on: impl Fn(Request) + Send + 'static) {
    let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else { return };
    let Ok(addr) = listener.local_addr() else { return };
    PORT.store(addr.port(), Ordering::Relaxed);
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            serve(s, &token, &on);
        }
    });
}

fn serve(mut s: TcpStream, token: &str, on: &impl Fn(Request)) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut raw = Vec::new();
    let _ = (&mut s).take(MAX_BYTES as u64 + 256).read_to_end(&mut raw);
    let reply = match parse_request(&raw, token) {
        Ok(Some((pane, op, text))) => {
            let (tx, rx) = mpsc::channel();
            on(Request { pane, op, text, reply: tx });
            rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| Err("maula did not answer".into()))
        }
        Ok(None) => return, // not ours: ignored like a hook without the token
        Err(e) => Err(e),
    };
    let _ = s.write_all(&encode_reply(&reply));
    let _ = s.shutdown(Shutdown::Both);
}

// ---- the `maula note` subcommand ----

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Read,
    Write(Option<String>), // None: from stdin
    Append(Option<String>),
    Clear,
}

/// `args` are those after `note`. "-" means stdin; write/append without text also read stdin.
pub fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let text = |rest: &[String]| match rest {
        [] => Ok(None),
        [t] if t == "-" => Ok(None),
        [t] => Ok(Some(t.clone())),
        _ => Err("give the text as one quoted argument, or '-' to read stdin".to_string()),
    };
    match args.split_first() {
        Some((c, [])) if c == "read" => Ok(Cmd::Read),
        Some((c, [])) if c == "clear" => Ok(Cmd::Clear),
        Some((c, rest)) if c == "write" => Ok(Cmd::Write(text(rest)?)),
        Some((c, rest)) if c == "append" => Ok(Cmd::Append(text(rest)?)),
        _ => Err("usage: maula note read | write [\"text\"|-] | append [\"text\"|-] | clear".into()),
    }
}

/// Runs `maula note ...`; returns the exit code.
pub fn note_main(args: &[String]) -> i32 {
    let fail = |m: &str| {
        eprintln!("maula note: {m}");
        1
    };
    let cmd = match parse_args(args) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let var = |k: &str| std::env::var(k).ok();
    let (Some(pane), Some(port), Some(token)) = (var("MAULA_PANE").and_then(|p| p.parse::<usize>().ok()), var("MAULA_NOTE_PORT").and_then(|p| p.parse::<u16>().ok()), var("MAULA_TOKEN")) else {
        return fail("not running inside a maula pane (MAULA_PANE / MAULA_NOTE_PORT / MAULA_TOKEN missing)");
    };
    let (op, text) = match cmd {
        Cmd::Read => (Op::Read, None),
        Cmd::Clear => (Op::Clear, None),
        Cmd::Write(t) => (Op::Write, t),
        Cmd::Append(t) => (Op::Append, t),
    };
    let text = match (op, text) {
        (Op::Read | Op::Clear, _) => String::new(),
        (_, Some(t)) => t,
        (_, None) => {
            let mut raw = Vec::new();
            if std::io::stdin().take(MAX_BYTES as u64 + 1).read_to_end(&mut raw).is_err() {
                return fail("cannot read stdin");
            }
            match String::from_utf8(raw) {
                Ok(t) => t,
                Err(_) => return fail("stdin is not valid UTF-8"),
            }
        }
    };
    if text.len() > MAX_BYTES {
        return fail(&format!("text is larger than {MAX_BYTES} bytes"));
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else { return fail("maula is not reachable (is the window still running?)") };
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut raw = Vec::new();
    if s.write_all(&encode_request(&token, pane, op, &text)).is_err() || s.shutdown(Shutdown::Write).is_err() || s.read_to_end(&mut raw).is_err() {
        return fail("connection to maula failed");
    }
    match parse_reply(&raw) {
        Ok(t) => {
            if op == Op::Read {
                let _ = std::io::stdout().write_all(t.as_bytes());
            }
            0
        }
        Err(e) => fail(&e),
    }
}

/// Text for Claude's SessionStart hook (additionalContext), so a session in a maula pane learns about the note.
pub fn session_hint() -> Option<String> {
    std::env::var("MAULA_NOTE_PORT").ok()?;
    let exe = std::env::current_exe().ok()?;
    let exe = exe.display();
    Some(format!(
        "This terminal pane is in maula, which has a small notepad for this pane. You can read and edit it from the shell: \
`\"{exe}\" note read` prints it; `\"{exe}\" note write \"text\"` replaces it; `\"{exe}\" note append \"text\"` adds to it; \
`\"{exe}\" note clear` empties it (for longer text pipe it in: `... note write -`). Plain text only. \
Use it for short notes the user can see next to this session, and read it when the user mentions their note."
    ))
}

/// The hook's stdout for a SessionStart event.
pub fn hint_json(hint: &str) -> String {
    use crate::json::Json;
    let inner = Json::obj(&[("hookEventName", Json::str("SessionStart")), ("additionalContext", Json::str(hint))]);
    crate::json::pretty(&Json::obj(&[("hookSpecificOutput", inner)])).replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn request_round_trip() {
        let raw = encode_request("tok", 3, Op::Append, "a\nb\n\u{e9}");
        assert_eq!(parse_request(&raw, "tok"), Ok(Some((3, Op::Append, "a\nb\n\u{e9}".into()))));
        assert_eq!(parse_request(&raw, "other"), Ok(None), "wrong token");
        assert_eq!(parse_request(b"tok\nx\nread\n", "tok"), Ok(None), "bad pane");
        assert_eq!(parse_request(b"tok\n1\nfly\n", "tok"), Ok(None), "bad op");
        assert!(parse_request(b"tok\n1\nwrite\n\xff", "tok").is_err(), "invalid utf-8");
        let big = encode_request("tok", 1, Op::Write, &"x".repeat(MAX_BYTES + 1));
        assert!(parse_request(&big, "tok").is_err(), "too large");
    }

    #[test]
    fn reply_round_trip() {
        assert_eq!(parse_reply(&encode_reply(&Ok("hi\nthere".into()))), Ok("hi\nthere".into()));
        assert_eq!(parse_reply(&encode_reply(&Ok(String::new()))), Ok(String::new()));
        assert_eq!(parse_reply(&encode_reply(&Err("no such pane".into()))), Err("no such pane".into()));
        assert!(parse_reply(b"garbage").is_err());
    }

    #[test]
    fn args() {
        assert_eq!(parse_args(&s(&["read"])), Ok(Cmd::Read));
        assert_eq!(parse_args(&s(&["clear"])), Ok(Cmd::Clear));
        assert_eq!(parse_args(&s(&["write", "x y"])), Ok(Cmd::Write(Some("x y".into()))));
        assert_eq!(parse_args(&s(&["write", "-"])), Ok(Cmd::Write(None)));
        assert_eq!(parse_args(&s(&["append"])), Ok(Cmd::Append(None)));
        assert!(parse_args(&s(&["write", "a", "b"])).is_err());
        assert!(parse_args(&s(&["read", "x"])).is_err());
        assert!(parse_args(&s(&[])).is_err());
    }

    #[test]
    fn server_serves_requests() {
        listen("t0k".into(), |r| {
            let _ = r.reply.send(match r.op {
                Op::Read => Ok(format!("pane {}", r.pane)),
                _ => Err(format!("{}:{}", r.op.name(), r.text)),
            });
        });
        let port = PORT.load(Ordering::Relaxed);
        assert_ne!(port, 0);
        let ask = |op, text: &str| {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(&encode_request("t0k", 7, op, text)).unwrap();
            c.shutdown(Shutdown::Write).unwrap();
            let mut raw = Vec::new();
            c.read_to_end(&mut raw).unwrap();
            parse_reply(&raw)
        };
        assert_eq!(ask(Op::Read, ""), Ok("pane 7".into()));
        assert_eq!(ask(Op::Write, "hey"), Err("write:hey".into()));
    }

    #[test]
    fn hint_is_valid_json() {
        let mut v = crate::json::parse(&hint_json("a \"b\"
c")).unwrap();
        let o = v.get_mut("hookSpecificOutput").unwrap();
        assert_eq!(o.get_mut("additionalContext").and_then(|j| j.as_str()), Some("a \"b\"
c"));
    }
}
