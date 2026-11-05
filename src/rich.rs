//! Rich text of the notepad: character styles kept as byte-range spans parallel to the plain note text.
//! Only non-default styles are stored, sorted and non-overlapping; adjacent equal spans are merged.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub hl: u8, // 0: no highlight, else a palette entry (1-based)
    pub size: u8, // font size in points; 0: the note's default size
}

/// The font sizes of the notepad's size buttons, in points at 100% text size.
pub const LADDER: [u8; 9] = [10, 12, 14, 16, 18, 20, 24, 28, 32];

/// The next size up or down the ladder from `cur` (a size in points, not 0), clamped at its ends.
pub fn step_size(cur: u8, up: bool) -> u8 {
    if up {
        LADDER.iter().copied().find(|&z| z > cur).unwrap_or(LADDER[LADDER.len() - 1].max(cur))
    } else {
        LADDER.iter().rev().copied().find(|&z| z < cur).unwrap_or(LADDER[0].min(cur))
    }
}

/// How a size of `eff` points is stored when the note's default is `def`: 0 for the default.
pub fn store_size(eff: u8, def: u8) -> u8 {
    if eff == def { 0 } else { eff }
}

impl Style {
    pub fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// Which font variant draws it: bit 0 bold, bit 1 italic.
    pub fn key(&self) -> u8 {
        self.bold as u8 | (self.italic as u8) << 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    Bold,
    Italic,
    Underline,
}

impl Flag {
    pub fn get(self, s: &Style) -> bool {
        match self {
            Flag::Bold => s.bold,
            Flag::Italic => s.italic,
            Flag::Underline => s.underline,
        }
    }

    pub fn set(self, s: &mut Style, v: bool) {
        match self {
            Flag::Bold => s.bold = v,
            Flag::Italic => s.italic = v,
            Flag::Underline => s.underline = v,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub style: Style,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Spans {
    v: Vec<Span>,
}

/// Walks the spans in step with a text position that only grows.
pub struct Cursor<'a> {
    v: &'a [Span],
    i: usize,
}

impl Cursor<'_> {
    pub fn style(&mut self, pos: usize) -> Style {
        while self.i < self.v.len() && self.v[self.i].end <= pos {
            self.i += 1;
        }
        match self.v.get(self.i) {
            Some(s) if s.start <= pos => s.style,
            _ => Style::default(),
        }
    }
}

/// Appends `b` (spans relative to its own start) after `a`, which covers `a_len` bytes, merging at the seam.
pub fn rel_cat(a: &mut Vec<Span>, a_len: usize, b: &[Span]) {
    for s in b {
        let n = Span { start: s.start + a_len, end: s.end + a_len, style: s.style };
        match a.last_mut() {
            Some(l) if l.end == n.start && l.style == n.style => l.end = n.end,
            _ => a.push(n),
        }
    }
}

impl Spans {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn cursor(&self, pos: usize) -> Cursor<'_> {
        Cursor { v: &self.v, i: self.first_after(pos) }
    }

    fn first_after(&self, pos: usize) -> usize {
        self.v.partition_point(|s| s.end <= pos)
    }

    /// The style of the character at byte `b`.
    pub fn style_of(&self, b: usize) -> Style {
        match self.v.get(self.first_after(b)) {
            Some(s) if s.start <= b => s.style,
            _ => Style::default(),
        }
    }

    /// The style text typed at `pos` takes: that of the character before it (at the start: the one after).
    pub fn style_at_caret(&self, pos: usize) -> Style {
        if self.v.is_empty() { Style::default() } else { self.style_of(pos.saturating_sub(1)) }
    }

    /// `[a, b)` cut into runs of one style each, plain gaps included.
    pub fn segments(&self, a: usize, b: usize) -> Vec<(usize, usize, Style)> {
        let (mut out, mut pos, mut i) = (Vec::new(), a, self.first_after(a));
        while pos < b {
            match self.v.get(i) {
                Some(s) if s.start < b => {
                    if s.start > pos {
                        out.push((pos, s.start, Style::default()));
                        pos = s.start;
                    }
                    let e = s.end.min(b);
                    out.push((pos, e, s.style));
                    pos = e;
                    i += 1;
                }
                _ => {
                    out.push((pos, b, Style::default()));
                    pos = b;
                }
            }
        }
        out
    }

    /// Every character of `[a, b)` satisfies `f`.
    pub fn all(&self, a: usize, b: usize, f: impl Fn(&Style) -> bool) -> bool {
        a < b && self.segments(a, b).iter().all(|s| f(&s.2))
    }

    /// The style of an empty line starting at byte `a` of a note `len` bytes long: that of its own line break, or at
    /// the very end that of the character before.
    pub fn empty_line_style(&self, a: usize, len: usize) -> Style {
        if a < len { self.style_of(a) } else { self.style_at_caret(a) }
    }

    /// Steps the size of every run in `[a, b)` one notch up or down from its own size (`def`: the unsized text's).
    pub fn step_sizes(&mut self, a: usize, b: usize, up: bool, def: u8) {
        self.apply(a, b, |mut s| {
            let eff = if s.size == 0 { def } else { s.size };
            s.size = store_size(step_size(eff, up), def);
            s
        });
    }

    pub fn set_size(&mut self, a: usize, b: usize, size: u8) {
        self.apply(a, b, |mut s| {
            s.size = size;
            s
        });
    }

    /// Changes the style of every character of `[a, b)` by `f`.
    pub fn apply(&mut self, a: usize, b: usize, f: impl Fn(Style) -> Style) {
        if a >= b {
            return;
        }
        let new: Vec<Span> = self.segments(a, b).into_iter().map(|(s, e, st)| Span { start: s, end: e, style: f(st) }).collect();
        self.splice_abs(a, b - a, b - a, new);
    }

    /// Turns `flag` on for `[a, b)`, or off if the whole range already has it. Returns the new state.
    pub fn toggle(&mut self, a: usize, b: usize, flag: Flag) -> bool {
        let on = !self.all(a, b, |s| flag.get(s));
        self.apply(a, b, |mut s| {
            flag.set(&mut s, on);
            s
        });
        on
    }

    pub fn set_hl(&mut self, a: usize, b: usize, hl: u8) {
        self.apply(a, b, |mut s| {
            s.hl = hl;
            s
        });
    }

    /// The non-plain spans inside `[at, at + len)`, relative to `at` and clipped to it.
    pub fn slice(&self, at: usize, len: usize) -> Vec<Span> {
        let mut out = Vec::new();
        for s in &self.v[self.first_after(at)..] {
            if s.start >= at + len {
                break;
            }
            out.push(Span { start: s.start.max(at) - at, end: s.end.min(at + len) - at, style: s.style });
        }
        out
    }

    /// The text `[at, at + old)` became `new_len` bytes long; `ins` (relative to `at`) styles the new bytes.
    pub fn splice(&mut self, at: usize, old: usize, new_len: usize, ins: &[Span]) {
        let abs: Vec<Span> = ins.iter().map(|s| Span { start: s.start + at, end: s.end + at, style: s.style }).collect();
        self.splice_abs(at, old, new_len, abs);
    }

    fn splice_abs(&mut self, at: usize, old: usize, new_len: usize, ins: Vec<Span>) {
        if self.v.is_empty() && ins.iter().all(|s| s.style.is_plain()) {
            return;
        }
        let b = at + old;
        let (i0, i1) = (self.first_after(at), self.v.partition_point(|s| s.start < b));
        let delta = new_len as isize - old as isize;
        let mut repl = Vec::with_capacity(ins.len() + 2);
        if i0 < i1 && self.v[i0].start < at {
            repl.push(Span { end: at, ..self.v[i0] });
        }
        repl.extend(ins.into_iter().filter(|s| s.start < s.end && !s.style.is_plain()));
        if i0 < i1 && self.v[i1 - 1].end > b {
            let l = self.v[i1 - 1];
            repl.push(Span { start: at + new_len, end: (l.end as isize + delta) as usize, style: l.style });
        }
        if delta != 0 {
            for s in &mut self.v[i1..] {
                s.start = (s.start as isize + delta) as usize;
                s.end = (s.end as isize + delta) as usize;
            }
        }
        let n = repl.len();
        self.v.splice(i0..i1, repl);
        self.merge(i0.saturating_sub(1), i0 + n);
    }

    /// Joins equal neighbours among the spans from index `lo` to `hi`.
    fn merge(&mut self, lo: usize, hi: usize) {
        let mut j = lo;
        while j + 1 < self.v.len() && j <= hi {
            if self.v[j].end == self.v[j + 1].start && self.v[j].style == self.v[j + 1].style {
                self.v[j].end = self.v[j + 1].end;
                self.v.remove(j + 1);
            } else {
                j += 1;
            }
        }
    }

    /// "start-end:flags:hl:size" per span, joined by ';' (flags: any of b, i, u; hl and size left out when 0). Bytes of the note.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for s in &self.v {
            if !out.is_empty() {
                out.push(';');
            }
            out += &format!("{}-{}:", s.start, s.end);
            for (on, c) in [(s.style.bold, 'b'), (s.style.italic, 'i'), (s.style.underline, 'u')] {
                if on {
                    out.push(c);
                }
            }
            if s.style.hl > 0 || s.style.size > 0 {
                out += &format!(":{}", s.style.hl);
            }
            if s.style.size > 0 {
                out += &format!(":{}", s.style.size);
            }
        }
        out
    }

    /// The reverse of `encode` for note text `text`: malformed, out-of-range or overlapping entries are dropped.
    pub fn decode(code: &str, text: &str) -> Spans {
        let one = |p: &str| -> Option<Span> {
            let mut f = p.split(':');
            let (a, b) = f.next()?.split_once('-')?;
            let (start, end): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            let flags = f.next().unwrap_or("");
            let hl = f.next().and_then(|h| h.trim().parse().ok()).unwrap_or(0);
            let size = f.next().and_then(|z| z.trim().parse().ok()).filter(|&z| z <= 96).unwrap_or(0);
            let style = Style { bold: flags.contains('b'), italic: flags.contains('i'), underline: flags.contains('u'), hl, size };
            (start < end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) && !style.is_plain()).then_some(Span { start, end, style })
        };
        let mut list: Vec<Span> = code.split(';').filter_map(one).collect();
        list.sort_by_key(|s| s.start);
        let mut out = Spans::default();
        let mut prev = 0;
        for mut s in list {
            s.start = s.start.max(prev);
            if s.start < s.end {
                prev = s.end;
                out.v.push(s);
            }
        }
        out.merge(0, usize::MAX);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Style = Style { bold: true, italic: false, underline: false, hl: 0, size: 0 };

    fn runs(s: &Spans) -> Vec<(usize, usize, bool, bool, bool, u8)> {
        s.v.iter().map(|s| (s.start, s.end, s.style.bold, s.style.italic, s.style.underline, s.style.hl)).collect()
    }

    #[test]
    fn toggle_sets_removes_and_merges() {
        let mut s = Spans::default();
        assert!(s.toggle(2, 5, Flag::Bold), "plain text: turned on");
        assert!(s.toggle(5, 8, Flag::Bold));
        assert_eq!(runs(&s), [(2, 8, true, false, false, 0)], "touching runs merge");
        assert!(s.toggle(0, 10, Flag::Bold), "part bold: the whole range gets it");
        assert_eq!(runs(&s), [(0, 10, true, false, false, 0)]);
        assert!(!s.toggle(3, 6, Flag::Bold), "all bold: removed");
        assert_eq!(runs(&s), [(0, 3, true, false, false, 0), (6, 10, true, false, false, 0)]);
        s.toggle(0, 10, Flag::Italic);
        s.set_hl(4, 7, 2);
        assert_eq!(s.style_of(5), Style { italic: true, hl: 2, ..Style::default() });
        s.set_hl(4, 7, 0);
        s.toggle(0, 10, Flag::Italic);
        s.toggle(0, 10, Flag::Bold);
        s.toggle(0, 10, Flag::Bold);
        assert!(s.is_empty(), "everything off again: no spans left");
    }

    #[test]
    fn segments_cover_the_range() {
        let mut s = Spans::default();
        s.toggle(3, 6, Flag::Bold);
        assert_eq!(s.segments(1, 8), [(1, 3, Style::default()), (3, 6, B), (6, 8, Style::default())]);
        assert_eq!(s.segments(4, 5), [(4, 5, B)]);
        assert!(s.segments(4, 4).is_empty());
        let mut c = s.cursor(0);
        assert_eq!((c.style(2), c.style(3), c.style(5), c.style(6)), (Style::default(), B, B, Style::default()));
    }

    #[test]
    fn spans_follow_edits() {
        let mut s = Spans::default();
        s.toggle(4, 8, Flag::Bold); // "0123[4567]89"
        s.splice(0, 0, 2, &[]); // insert 2 plain bytes before
        assert_eq!(runs(&s), [(6, 10, true, false, false, 0)]);
        s.splice(8, 0, 3, &[Span { start: 0, end: 3, style: B }]); // bold typed inside a bold run: still one run
        assert_eq!(runs(&s), [(6, 13, true, false, false, 0)]);
        s.splice(8, 0, 2, &[]); // plain text typed inside splits it
        assert_eq!(runs(&s), [(6, 8, true, false, false, 0), (10, 15, true, false, false, 0)]);
        s.splice(7, 5, 0, &[]); // delete across the gap: ends trimmed
        assert_eq!(runs(&s), [(6, 10, true, false, false, 0)]);
        s.splice(0, 20, 0, &[]); // delete everything
        assert!(s.is_empty());
    }

    #[test]
    fn removed_styles_come_back_on_undo() {
        let mut s = Spans::default();
        s.toggle(2, 6, Flag::Bold);
        s.toggle(4, 9, Flag::Underline);
        let before = s.clone();
        let removed = s.slice(3, 5); // bytes 3..8
        s.splice(3, 5, 1, &[]); // replaced by one plain byte
        s.splice(3, 1, 5, &removed); // undo
        assert_eq!(s, before);
        // typed text merged into one undo step keeps the styles of each part
        let (mut ins, mut len) = (Vec::new(), 0);
        rel_cat(&mut ins, len, &[Span { start: 0, end: 2, style: B }]);
        len += 2;
        rel_cat(&mut ins, len, &[Span { start: 0, end: 3, style: B }]);
        assert_eq!(ins, [Span { start: 0, end: 5, style: B }]);
    }

    #[test]
    fn codec_round_trips_and_rejects_junk() {
        let text = "héllo world, rich";
        let mut s = Spans::default();
        s.toggle(0, 3, Flag::Bold); // "h" + "é" (2 bytes)
        s.toggle(4, 9, Flag::Underline);
        s.toggle(4, 9, Flag::Italic);
        s.set_hl(6, 12, 3);
        let code = s.encode();
        assert_eq!(Spans::decode(&code, text), s);
        assert_eq!(Spans::decode("", text), Spans::default());
        let junk = Spans::decode("x;1-2;5-3:b;2-99:b;2-3:b:;1-2:b;0-3:b;1-9:i;2-4:iu:7", text);
        assert_eq!(junk.v.first().map(|s| s.start), Some(0), "a span in the middle of a character is dropped");
        assert!(junk.v.windows(2).all(|w| w[0].end <= w[1].start && w[0].start < w[0].end), "sorted, no overlap");
        assert!(junk.v.iter().all(|s| s.end <= text.len() && text.is_char_boundary(s.start) && text.is_char_boundary(s.end)));
    }

    #[test]
    fn sizes_step_per_run_and_round_trip() {
        assert_eq!((step_size(14, true), step_size(14, false), step_size(32, true), step_size(10, false)), (16, 12, 32, 10));
        assert_eq!((step_size(15, true), step_size(15, false)), (16, 14), "off the ladder: the next notch");
        let mut s = Spans::default();
        s.set_size(2, 6, 20);
        // [0,2) default 14, [2,6) 20, [6,9) default: stepping up moves each run from its own size
        s.step_sizes(0, 9, true, 14);
        let sz = |s: &Spans| s.segments(0, 9).iter().map(|x| (x.0, x.1, x.2.size)).collect::<Vec<_>>();
        assert_eq!(sz(&s), [(0, 2, 16), (2, 6, 24), (6, 9, 16)]);
        s.step_sizes(0, 9, false, 14);
        assert_eq!(sz(&s), [(0, 2, 0), (2, 6, 20), (6, 9, 0)], "back at the default, the size is not stored");
        s.step_sizes(0, 9, true, 14);
        s.toggle(1, 4, Flag::Bold);
        s.set_hl(3, 5, 2);
        let text = "0123456789";
        assert_eq!(Spans::decode(&s.encode(), text), s);
        assert_eq!(Spans::decode("0-3:b;4-6::2:18;7-9:i:0:12", text).segments(0, 9).iter().map(|x| x.2.size).collect::<Vec<_>>(), [0, 0, 18, 0, 12]);
        assert_eq!(Spans::decode("0-3:b:2", text).style_of(1), Style { bold: true, hl: 2, ..Style::default() }, "the old format still decodes");
        assert!(Spans::decode("0-3:::200", text).is_empty(), "absurd sizes are dropped");
    }

    #[test]
    fn sizes_follow_edits() {
        let mut s = Spans::default();
        s.set_size(4, 8, 24);
        s.splice(0, 0, 3, &[]);
        assert_eq!(s.style_of(8).size, 24);
        assert_eq!(s.style_of(6).size, 0);
        let big = Style { size: 24, ..Style::default() };
        s.splice(9, 0, 2, &[Span { start: 0, end: 2, style: big }]);
        assert_eq!(s.segments(7, 13), [(7, 13, big)], "typed at the same size: one run");
        assert_eq!(s.empty_line_style(7, 20).size, 24);
        assert_eq!(s.empty_line_style(20, 20).size, 0);
        assert_eq!(s.empty_line_style(13, 13).size, 24, "at the very end: the character before");
    }
}
