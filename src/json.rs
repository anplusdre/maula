//! JSON, as much as maula needs: Claude's hook events, transcript lines and settings.json. Objects keep their key
//! order and numbers keep their exact text, so a settings file maula edits changes only where maula changed it.

use std::ops::Index;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(String), // as written
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>), // in file order
}

static NULL: Json = Json::Null;

/// `v["key"]`: the member, or null when there is none (or `v` is not an object), like JavaScript.
impl Index<&str> for Json {
    type Output = Json;
    fn index(&self, key: &str) -> &Json {
        match self {
            Json::Obj(m) => m.iter().find(|(k, _)| k == key).map_or(&NULL, |(_, v)| v),
            _ => &NULL,
        }
    }
}

/// `v["type"] == "text"`: a string with that text.
impl PartialEq<&str> for Json {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

impl Json {
    pub fn str(s: &str) -> Json {
        Json::Str(s.into())
    }
    pub fn obj(members: &[(&str, Json)]) -> Json {
        Json::Obj(members.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self { Some(s) } else { None }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self { Some(*b) } else { None }
    }
    pub fn as_array(&self) -> Option<&Vec<Json>> {
        if let Json::Arr(a) = self { Some(a) } else { None }
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        if let Json::Obj(m) = self { m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v) } else { None }
    }
    /// The member `key`, added as `default` when missing; None when `self` is not an object.
    pub fn entry(&mut self, key: &str, default: Json) -> Option<&mut Json> {
        let Json::Obj(m) = self else { return None };
        let i = match m.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                m.push((key.into(), default));
                m.len() - 1
            }
        };
        Some(&mut m[i].1)
    }
}

/// Nesting deeper than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 256;

/// The JSON document `text` (a leading byte order mark is skipped). None if it is not exactly one valid value.
pub fn parse(text: &str) -> Option<Json> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    let v = p.value(0)?;
    p.ws();
    (p.i == p.s.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let hit = self.s.get(self.i) == Some(&c);
        self.i += hit as usize;
        hit
    }
    fn word(&mut self, w: &str, v: Json) -> Option<Json> {
        self.s[self.i..].starts_with(w.as_bytes()).then(|| {
            self.i += w.len();
            v
        })
    }
    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.ws();
        match *self.s.get(self.i)? {
            b'n' => self.word("null", Json::Null),
            b't' => self.word("true", Json::Bool(true)),
            b'f' => self.word("false", Json::Bool(false)),
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                if self.eat(b']') {
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    if self.eat(b']') {
                        return Some(Json::Arr(a));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'{' => {
                self.i += 1;
                let mut m: Vec<(String, Json)> = Vec::new();
                if self.eat(b'}') {
                    return Some(Json::Obj(m));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return None;
                    }
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return None;
                    }
                    let v = self.value(depth + 1)?;
                    match m.iter_mut().find(|(key, _)| *key == k) {
                        Some(slot) => slot.1 = v, // a repeated key: the last value, in the first one's place
                        None => m.push((k, v)),
                    }
                    if self.eat(b'}') {
                        return Some(Json::Obj(m));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            _ => self.number(),
        }
    }
    /// -?(0|[1-9][0-9]*)(.[0-9]+)?([eE][+-]?[0-9]+)?
    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        let digits = |p: &mut Self| {
            let d = p.s[p.i..].iter().take_while(|c| c.is_ascii_digit()).count();
            p.i += d;
            d
        };
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let lead = self.s.get(self.i) == Some(&b'0');
        let n = digits(self);
        if n == 0 || (lead && n > 1) {
            return None;
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return None;
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return None;
            }
        }
        Some(Json::Num(String::from_utf8_lossy(&self.s[start..self.i]).into()))
    }
    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
    fn string(&mut self) -> Option<String> {
        self.i += 1; // the opening quote
        let mut out = String::new();
        loop {
            // copy the plain run up to the next quote, backslash or control character in one go
            let run = self.s[self.i..].iter().take_while(|&&c| c != b'"' && c != b'\\' && c >= 0x20).count();
            out.push_str(std::str::from_utf8(&self.s[self.i..self.i + run]).ok()?);
            self.i += run;
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    out.push(match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) && self.s[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + lo.checked_sub(0xDC00).filter(|l| *l < 0x400)?)?
                            } else {
                                char::from_u32(hi).unwrap_or('\u{fffd}') // a lone surrogate
                            }
                        }
                        _ => return None,
                    });
                }
                _ => return None, // a raw control character inside a string
            }
        }
    }
}

/// `v` as indented JSON: two spaces per level, `"key": value`, like most tools write settings.json.
pub fn pretty(v: &Json) -> String {
    let mut out = String::new();
    write(v, 0, &mut out);
    out
}

fn write(v: &Json, level: usize, out: &mut String) {
    let pad = |out: &mut String, l: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", l));
    };
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(n),
        Json::Str(s) => quote(s, out),
        Json::Arr(a) if a.is_empty() => out.push_str("[]"),
        Json::Obj(m) if m.is_empty() => out.push_str("{}"),
        Json::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                out.push_str(if i > 0 { "," } else { "" });
                pad(out, level + 1);
                write(x, level + 1, out);
            }
            pad(out, level);
            out.push(']');
        }
        Json::Obj(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                out.push_str(if i > 0 { "," } else { "" });
                pad(out, level + 1);
                quote(k, out);
                out.push_str(": ");
                write(x, level + 1, out);
            }
            pad(out, level);
            out.push('}');
        }
    }
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_writes_back() {
        let text = "{\n  \"z\": 1,\n  \"a\": [\n    true,\n    null,\n    -0.5e+3,\n    \"x\\\"\\\\\\n\\u0001é😀\"\n  ],\n  \"e\": {},\n  \"l\": []\n}";
        let v = parse(text).unwrap();
        assert_eq!(pretty(&v), text, "key order, number text, escapes and empty containers kept");
        assert_eq!(v["a"].as_array().unwrap()[3], Json::str("x\"\\\n\u{1}é😀"));
        assert_eq!(parse(r#""\ud83d\ude00 \u00e9 \/""#), Some(Json::str("😀 é /")), "surrogate pair, \\u and \\/");
        assert_eq!(parse("\u{feff}{}"), Some(Json::Obj(vec![])), "byte order mark skipped");
        assert_eq!(v["missing"]["deeper"], Json::Null, "missing members read as null");
        assert!(v["z"] != "1", "a number is not a string");
    }

    #[test]
    fn rejects_what_is_not_json() {
        for bad in ["", "not json", "{} x", "{\"a\" 1}", "[1,]", "{\"a\":1,}", "01", "1.", "-", "\"a\nb\"", "\"\\x\"", "nul", "{'a':1}"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        assert!(parse(&"[".repeat(MAX_DEPTH + 2)).is_none(), "too deep: refused, no stack overflow");
        assert!(parse(&format!("{}{}", "[".repeat(100), "]".repeat(100))).is_some());
    }

    #[test]
    fn entry_adds_or_finds() {
        let mut v = parse(r#"{"a":1}"#).unwrap();
        *v.entry("b", Json::Arr(vec![])).unwrap() = Json::str("x");
        v.entry("a", Json::Null);
        assert_eq!(pretty(&v), "{\n  \"a\": 1,\n  \"b\": \"x\"\n}");
        assert!(Json::Arr(vec![]).entry("a", Json::Null).is_none());
        assert_eq!(parse(r#"{"k":1,"j":2,"k":3}"#).unwrap(), Json::Obj(vec![("k".into(), Json::Num("3".into())), ("j".into(), Json::Num("2".into()))]));
    }
}
