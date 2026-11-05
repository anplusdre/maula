//! TrueType fonts: reading glyph outlines (`glyf`) and rasterizing them to anti-aliased coverage.
//! ponytail: TrueType outlines only (no CFF/OpenType-PostScript, no hinting, no kerning, no colour layers: a colour
//! emoji draws as its monochrome outline); every font maula loads on Windows is TrueType.

use std::borrow::Cow;

/// A rasterized glyph: `w` x `h` coverage bytes (0..=255, rows top-down). (`xmin`, `ymin`) is the bitmap's bottom-left
/// corner relative to the pen position on the baseline, y up; `adv` is how far the pen moves, in px.
pub struct Bitmap {
    pub xmin: i32,
    pub ymin: i32,
    pub w: usize,
    pub h: usize,
    pub adv: f32,
    pub cov: Vec<u8>,
}

pub struct Face {
    data: Cow<'static, [u8]>,
    units: f32,      // units per em
    long_loca: bool, // `loca` holds u32 offsets (else u16 halves)
    glyphs: u16,
    h_metrics: u16, // hmtx entries with their own advance
    ascent: i16,
    descent: i16,
    cmap: usize, // the Unicode cmap subtable (format 4 or 12)
    loca: usize,
    glyf: usize,
    hmtx: usize,
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn i16_at(d: &[u8], o: usize) -> Option<i16> {
    u16_at(d, o).map(|v| v as i16)
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

impl Face {
    /// A .ttf, or the first font of a .ttc collection. None if it is not a TrueType-outline font.
    pub fn parse(data: impl Into<Cow<'static, [u8]>>) -> Option<Face> {
        let data = data.into();
        let d = &data[..];
        let base = if d.get(..4)? == b"ttcf" { u32_at(d, 12)? as usize } else { 0 };
        let count = u16_at(d, base + 4)? as usize;
        let table = |tag: &[u8; 4]| -> Option<usize> {
            (0..count).map(|i| base + 12 + 16 * i).find(|&r| d.get(r..r + 4) == Some(tag)).and_then(|r| u32_at(d, r + 8)).map(|o| o as usize)
        };
        let (head, hhea, maxp, cmap) = (table(b"head")?, table(b"hhea")?, table(b"maxp")?, table(b"cmap")?);
        let (loca, glyf, hmtx) = (table(b"loca")?, table(b"glyf")?, table(b"hmtx")?);
        // line metrics as most renderers take them: OS/2's typographic ones when the font asks for that, else hhea's
        let (mut ascent, mut descent) = (i16_at(d, hhea + 4)?, i16_at(d, hhea + 6)?);
        if let Some(os2) = table(b"OS/2") {
            if u16_at(d, os2 + 62).is_some_and(|sel| sel & 0x80 != 0) {
                (ascent, descent) = (i16_at(d, os2 + 68)?, i16_at(d, os2 + 70)?);
            }
        }
        Some(Face {
            units: u16_at(d, head + 18)? as f32,
            long_loca: i16_at(d, head + 50)? == 1,
            glyphs: u16_at(d, maxp + 4)?,
            h_metrics: u16_at(d, hhea + 34)?,
            ascent,
            descent,
            cmap: unicode_cmap(d, cmap)?,
            loca,
            glyf,
            hmtx,
            data,
        })
    }

    /// (ascent, descent) in px at size `px`; descent is negative.
    pub fn line_metrics(&self, px: f32) -> (f32, f32) {
        let s = px / self.units;
        (self.ascent as f32 * s, self.descent as f32 * s)
    }

    /// The glyph for `c`; 0 (the font's "missing" glyph) when it has none.
    pub fn glyph_index(&self, c: char) -> u16 {
        let (d, t, c) = (&self.data[..], self.cmap, c as u32);
        let g = match u16_at(d, t) {
            Some(4) => (|| {
                if c > 0xffff {
                    return Some(0);
                }
                let segs = u16_at(d, t + 6)? as usize / 2;
                let (ends, starts) = (t + 14, t + 16 + 2 * segs);
                let (deltas, ranges) = (starts + 2 * segs, starts + 4 * segs);
                // the first segment whose end is >= c (ends are sorted)
                let (mut lo, mut hi) = (0, segs);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if (u16_at(d, ends + 2 * mid)? as u32) < c { lo = mid + 1 } else { hi = mid }
                }
                let i = lo;
                if i == segs || (u16_at(d, starts + 2 * i)? as u32) > c {
                    return Some(0);
                }
                let (start, delta, range) = (u16_at(d, starts + 2 * i)? as u32, u16_at(d, deltas + 2 * i)? as u32, u16_at(d, ranges + 2 * i)? as usize);
                if range == 0 {
                    return Some((c + delta) as u16); // modulo 65536, as the format says
                }
                let g = u16_at(d, ranges + 2 * i + range + 2 * (c - start) as usize)? as u32;
                Some(if g == 0 { 0 } else { (g + delta) as u16 })
            })(),
            Some(12) => (|| {
                let n = u32_at(d, t + 12)? as usize;
                let (mut lo, mut hi) = (0, n);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let g = t + 16 + 12 * mid;
                    let (start, end) = (u32_at(d, g)?, u32_at(d, g + 4)?);
                    if c < start {
                        hi = mid;
                    } else if c > end {
                        lo = mid + 1;
                    } else {
                        return u16::try_from(u32_at(d, g + 8)? + (c - start)).ok();
                    }
                }
                Some(0)
            })(),
            _ => None,
        };
        g.filter(|&g| g < self.glyphs).unwrap_or(0)
    }

    /// The advance of glyph `g`, in px at size `px`.
    pub fn advance(&self, g: u16, px: f32) -> f32 {
        let i = g.min(self.h_metrics.saturating_sub(1)) as usize; // glyphs past the table share the last advance
        u16_at(&self.data, self.hmtx + 4 * i).unwrap_or(0) as f32 * px / self.units
    }

    /// `c` drawn at size `px` (the em is `px` pixels tall).
    pub fn rasterize(&self, c: char, px: f32) -> Bitmap {
        let g = self.glyph_index(c);
        let adv = self.advance(g, px);
        let mut contours = Vec::new();
        self.outline(g, Transform::IDENTITY, 0, &mut contours);
        let s = px / self.units;
        let scaled = |p: Point| (p.x * s, p.y * s);
        let mut segs = Vec::new();
        for c in &contours {
            path(c, |a, ctl, b| segs.push((scaled(a), ctl.map(scaled), scaled(b))));
        }
        // the ink's bounds: segment ends, plus where a curve turns (its control point may lie well outside)
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        let mut grow = |(x, y): (f32, f32)| (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        for &(a, ctl, b) in &segs {
            grow(a);
            grow(b);
            if let Some(c) = ctl {
                for t in [extremum(a.0, c.0, b.0), extremum(a.1, c.1, b.1)].into_iter().flatten() {
                    grow(quad_at(a, c, b, t));
                }
            }
        }
        let (left, bottom) = (x0.floor(), y0.floor());
        let (w, h) = ((x1.ceil() - left).max(0.0) as usize, (y1.ceil() - bottom).max(0.0) as usize);
        if segs.is_empty() || w == 0 || h == 0 {
            return Bitmap { xmin: 0, ymin: 0, w: 0, h: 0, adv, cov: Vec::new() }; // a space: nothing to draw
        }
        let top = bottom + h as f32;
        let mut r = Raster::new(w, h);
        // to bitmap space: x from the left edge, y down from the top row
        let to = |(x, y): (f32, f32)| (x - left, top - y);
        for (a, ctl, b) in segs {
            match ctl {
                Some(c) => r.quad(to(a), to(c), to(b)),
                None => r.line(to(a), to(b)),
            }
        }
        Bitmap { xmin: left as i32, ymin: bottom as i32, w, h, adv, cov: r.coverage() }
    }

    /// Appends glyph `g`'s contours, transformed by `t`, to `out`. Composite glyphs recurse (a few levels at most).
    fn outline(&self, g: u16, t: Transform, depth: u32, out: &mut Vec<Vec<Point>>) -> Option<()> {
        let d = &self.data[..];
        let (start, end) = if self.long_loca {
            (u32_at(d, self.loca + 4 * g as usize)? as usize, u32_at(d, self.loca + 4 * g as usize + 4)? as usize)
        } else {
            (u16_at(d, self.loca + 2 * g as usize)? as usize * 2, u16_at(d, self.loca + 2 * g as usize + 2)? as usize * 2)
        };
        if end <= start || depth > 8 {
            return Some(()); // empty glyph (a space), or a composite that never ends
        }
        let o = self.glyf + start;
        let contours = i16_at(d, o)?;
        if contours >= 0 {
            return simple(d, o, contours as usize, t, out);
        }
        // composite: components, each another glyph placed with an offset and maybe a scale
        let mut p = o + 10;
        loop {
            let (flags, child) = (u16_at(d, p)?, u16_at(d, p + 2)?);
            p += 4;
            let (dx, dy) = if flags & 1 != 0 {
                p += 4;
                (i16_at(d, p - 4)? as f32, i16_at(d, p - 2)? as f32)
            } else {
                p += 2;
                (*d.get(p - 2)? as i8 as f32, *d.get(p - 1)? as i8 as f32)
            };
            // ponytail: point-matched placement (flag 2 clear) is rare; such a component goes at offset 0
            let (dx, dy) = if flags & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
            let f2 = |o: usize| i16_at(d, o).map(|v| v as f32 / 16384.0);
            let (a, b, c, e) = if flags & 0x8 != 0 {
                p += 2;
                let s = f2(p - 2)?;
                (s, 0.0, 0.0, s)
            } else if flags & 0x40 != 0 {
                p += 4;
                (f2(p - 4)?, 0.0, 0.0, f2(p - 2)?)
            } else if flags & 0x80 != 0 {
                p += 8;
                (f2(p - 8)?, f2(p - 6)?, f2(p - 4)?, f2(p - 2)?)
            } else {
                (1.0, 0.0, 0.0, 1.0)
            };
            let local = Transform { a, b, c, d: e, x: dx, y: dy };
            self.outline(child, t.then(&local), depth + 1, out)?;
            if flags & 0x20 == 0 {
                return Some(());
            }
        }
    }
}

/// The Unicode subtable of `cmap` at offset `t`: format 12 if there is one, else format 4.
fn unicode_cmap(d: &[u8], t: usize) -> Option<usize> {
    let n = u16_at(d, t + 2)? as usize;
    // (format, offset) of each Unicode subtable: platform 0, or Windows (3) with Unicode BMP (1) or full (10)
    let subs: Vec<(u16, usize)> = (0..n)
        .filter_map(|i| {
            let r = t + 4 + 8 * i;
            let (platform, encoding) = (u16_at(d, r)?, u16_at(d, r + 2)?);
            let off = t + u32_at(d, r + 4)? as usize;
            (platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10))).then_some(())?;
            Some((u16_at(d, off)?, off))
        })
        .collect();
    [12, 4].iter().find_map(|&f| subs.iter().find(|s| s.0 == f).map(|s| s.1))
}

#[derive(Clone, Copy)]
struct Point {
    x: f32,
    y: f32,
    on: bool, // on the curve; off-curve points are quadratic control points
}

/// x' = a·x + c·y + x, y' = b·x + d·y + y
#[derive(Clone, Copy)]
struct Transform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    x: f32,
    y: f32,
}

impl Transform {
    const IDENTITY: Transform = Transform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, x: 0.0, y: 0.0 };
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (self.a * x + self.c * y + self.x, self.b * x + self.d * y + self.y)
    }
    /// `inner` first, then `self`.
    fn then(&self, inner: &Transform) -> Transform {
        let (x, y) = self.apply(inner.x, inner.y);
        Transform { a: self.a * inner.a + self.c * inner.b, b: self.b * inner.a + self.d * inner.b, c: self.a * inner.c + self.c * inner.d, d: self.b * inner.c + self.d * inner.d, x, y }
    }
}

/// A simple glyph's contours (its points decoded from the packed flag/x/y arrays).
fn simple(d: &[u8], o: usize, contours: usize, t: Transform, out: &mut Vec<Vec<Point>>) -> Option<()> {
    let ends: Vec<usize> = (0..contours).map(|i| u16_at(d, o + 10 + 2 * i).map(|e| e as usize)).collect::<Option<_>>()?;
    let n = ends.last().map_or(0, |e| e + 1);
    let mut p = o + 10 + 2 * contours;
    p += 2 + u16_at(d, p)? as usize; // skip the hinting instructions
    let mut flags = Vec::with_capacity(n);
    while flags.len() < n {
        let f = *d.get(p)?;
        p += 1;
        let repeat = if f & 8 != 0 {
            p += 1;
            *d.get(p - 1)? as usize
        } else {
            0
        };
        flags.extend(std::iter::repeat_n(f, (repeat + 1).min(n - flags.len())));
    }
    // x then y: each a short (1 byte, with a sign flag) or long (i16) delta, or "same as before"
    let mut coord = |short: u8, same: u8| -> Option<Vec<f32>> {
        let mut v = 0i32;
        let mut out = Vec::with_capacity(n);
        for &f in &flags {
            if f & short != 0 {
                let b = *d.get(p)? as i32;
                p += 1;
                v += if f & same != 0 { b } else { -b };
            } else if f & same == 0 {
                v += i16_at(d, p)? as i32;
                p += 2;
            }
            out.push(v as f32);
        }
        Some(out)
    };
    let xs = coord(2, 16)?;
    let ys = coord(4, 32)?;
    let mut first = 0;
    for &end in &ends {
        if end < first || end >= n {
            return None;
        }
        out.push(
            (first..=end)
                .map(|i| {
                    let (x, y) = t.apply(xs[i], ys[i]);
                    Point { x, y, on: flags[i] & 1 != 0 }
                })
                .collect(),
        );
        first = end + 1;
    }
    Some(())
}

/// Where a quadratic's coordinate (a, control c, b) turns back, as t in (0, 1); None if it never does.
fn extremum(a: f32, c: f32, b: f32) -> Option<f32> {
    let den = a - 2.0 * c + b;
    let t = (a - c) / den;
    (den != 0.0 && t > 0.0 && t < 1.0).then_some(t)
}

fn quad_at(a: (f32, f32), c: (f32, f32), b: (f32, f32), t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    (u * u * a.0 + 2.0 * u * t * c.0 + t * t * b.0, u * u * a.1 + 2.0 * u * t * c.1 + t * t * b.1)
}

/// Walks a closed contour as segments `seg(from, control, to)`: a line without a control point, else a quadratic
/// curve. Two off-curve points in a row imply an on-curve point halfway between them.
fn path(c: &[Point], mut seg: impl FnMut(Point, Option<Point>, Point)) {
    let n = c.len();
    if n < 2 {
        return;
    }
    let mid = |a: Point, b: Point| Point { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0, on: true };
    // start on an on-curve point (or the midpoint of the first two, when every point is off-curve)
    let s = c.iter().position(|p| p.on);
    let start = s.map_or_else(|| mid(c[0], c[1]), |i| c[i]);
    let first = s.map_or(0, |i| i + 1);
    let (mut pen, mut ctl) = (start, None::<Point>);
    for k in 0..n {
        let p = c[(first + k) % n];
        match (p.on, ctl) {
            (true, None) => {
                seg(pen, None, p);
                pen = p;
            }
            (true, Some(q)) => {
                seg(pen, Some(q), p);
                (pen, ctl) = (p, None);
            }
            (false, None) => ctl = Some(p),
            (false, Some(q)) => {
                let m = mid(q, p);
                seg(pen, Some(q), m);
                (pen, ctl) = (m, Some(p));
            }
        }
    }
    match ctl {
        Some(q) => seg(pen, Some(q), start),
        None => seg(pen, None, start),
    }
}

/// Signed-area coverage accumulation: each edge adds how much of each pixel lies to its right; a running sum along
/// a row then gives the coverage. Exact for straight edges; curves are split into short lines first.
struct Raster {
    w: usize,
    h: usize,
    acc: Vec<f32>, // (w + 2) per row: an edge's area can spill one or two cells right of the last pixel
}

impl Raster {
    fn new(w: usize, h: usize) -> Raster {
        Raster { w, h, acc: vec![0.0; (w + 2) * h] }
    }

    fn quad(&mut self, a: (f32, f32), c: (f32, f32), b: (f32, f32)) {
        // how far the curve bends away from the a-b line decides the number of pieces (error ~ 0.03 px)
        let (dx, dy) = (a.0 - 2.0 * c.0 + b.0, a.1 - 2.0 * c.1 + b.1);
        let n = 1 + ((dx * dx + dy * dy).sqrt() / 0.25).sqrt() as usize;
        let mut prev = a;
        for i in 1..=n {
            let p = quad_at(a, c, b, i as f32 / n as f32);
            self.line(prev, p);
            prev = p;
        }
    }

    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if p0.1 == p1.1 {
            return; // horizontal: covers no height
        }
        let (dir, p0, p1) = if p0.1 < p1.1 { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let wmax = self.w as f32;
        let mut x = p0.0;
        let y_first = p0.1.max(0.0);
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let stride = self.w + 2;
        for y in y_first as usize..self.h.min(p1.1.ceil() as usize) {
            let row = y * stride;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (xa, xb) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let (xa, xb) = (xa.clamp(0.0, wmax), xb.clamp(0.0, wmax));
            let (xa_floor, xb_ceil) = (xa.floor(), xb.ceil());
            let (ia, ib) = (xa_floor as usize, xb_ceil as usize);
            if ib <= ia + 1 {
                // within one pixel column: the area right of the edge's midpoint
                let xm = 0.5 * (xa + xb) - xa_floor;
                self.acc[row + ia] += d - d * xm;
                self.acc[row + ia + 1] += d * xm;
            } else {
                let s = 1.0 / (xb - xa);
                let fa = xa - xa_floor;
                let a0 = 0.5 * s * (1.0 - fa) * (1.0 - fa);
                let fb = xb - xb_ceil + 1.0;
                let am = 0.5 * s * fb * fb;
                self.acc[row + ia] += d * a0;
                if ib == ia + 2 {
                    self.acc[row + ia + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - fa);
                    self.acc[row + ia + 1] += d * (a1 - a0);
                    for i in ia + 2..ib - 1 {
                        self.acc[row + i] += d * s;
                    }
                    let a2 = a1 + (ib - ia - 3) as f32 * s;
                    self.acc[row + ib - 1] += d * (1.0 - a2 - am);
                }
                self.acc[row + ib] += d * am;
            }
            x = xnext;
        }
    }

    fn coverage(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.w * self.h);
        for row in self.acc.chunks(self.w + 2) {
            let mut sum = 0.0;
            for &a in &row[..self.w] {
                sum += a;
                out.push((sum.abs() * 255.9).min(255.0) as u8);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_square_fills_exactly_and_edges_are_partial() {
        let sq = |x0: f32, y0: f32, x1: f32, y1: f32| [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        let mut r = Raster::new(4, 4);
        let p = sq(1.0, 1.0, 3.0, 3.0);
        for i in 0..4 {
            r.line(p[i], p[(i + 1) % 4]);
        }
        let c = r.coverage();
        assert_eq!(c.iter().filter(|&&v| v == 255).count(), 4, "the 2x2 inside is full");
        assert_eq!(c.iter().filter(|&&v| v == 0).count(), 12, "everything else empty");
        let mut r = Raster::new(2, 1);
        let p = sq(0.5, 0.0, 2.0, 1.0);
        for i in 0..4 {
            r.line(p[i], p[(i + 1) % 4]);
        }
        assert_eq!(r.coverage(), [127, 255], "a half-covered pixel");
    }

    #[test]
    fn bundled_fonts_read() {
        let f = Face::parse(&include_bytes!("../assets/JetBrainsMono-Regular.ttf")[..]).unwrap();
        assert_ne!(f.glyph_index('M'), 0);
        assert_ne!(f.glyph_index('│'), 0, "box drawing");
        assert_eq!(f.glyph_index('中'), 0, "not in the font: falls back");
        let m = f.rasterize('M', 20.0);
        assert!(m.w > 5 && m.h > 10 && m.cov.iter().any(|&c| c == 255));
        assert_eq!(f.rasterize(' ', 20.0).w, 0);
        let icons = Face::parse(&include_bytes!("../assets/lucide-subset.ttf")[..]).unwrap();
        assert!(icons.glyphs > 1);
        assert!(Face::parse(&b"not a font"[..]).is_none());
    }
}
