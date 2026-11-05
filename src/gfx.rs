//! CPU drawing: font with lazy fallbacks, a clipped pixel canvas.

use crate::ttf::Face;
use std::collections::HashMap;

/// JetBrains Mono Regular (SIL OFL, see assets/). System fonts below are only a fallback.
const BUNDLED: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
const BUNDLED_MEDIUM: &[u8] = include_bytes!("../assets/JetBrainsMono-Medium.ttf");

/// Lucide icons used by the UI (chevron-down, plus, x, minus, square, copy, check), subsetted.
const LUCIDE: &[u8] = include_bytes!("../assets/lucide-subset.ttf");

const FONTS: &[&str] = &[r"C:\Windows\Fonts\consola.ttf"];

/// Tried in order, loaded only when a glyph is missing from the primary font.
const FALLBACKS: &[&str] = &[
    r"C:\Windows\Fonts\seguisym.ttf",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\malgun.ttf",
    r"C:\Windows\Fonts\YuGothM.ttc",
    r"C:\Windows\Fonts\seguiemj.ttf",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = ((self.x + self.w).min(o.x + o.w), (self.y + self.h).min(o.y + o.h));
        Rect { x: x0, y: y0, w: (x1 - x0).max(0), h: (y1 - y0).max(0) }
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, o: &Rect) -> Rect {
        let (x0, y0) = (self.x.min(o.x), self.y.min(o.y));
        let (x1, y1) = ((self.x + self.w).max(o.x + o.w), (self.y + self.h).max(o.y + o.h));
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    pub fn inset(&self, d: i32) -> Rect {
        Rect { x: self.x + d, y: self.y + d, w: (self.w - 2 * d).max(0), h: (self.h - 2 * d).max(0) }
    }
}

pub struct Glyph {
    xmin: i32,
    ymin: i32,
    w: usize,
    h: usize,
    adv: f32,
    cov: Vec<u8>,
}

impl Glyph {
    #[cfg(test)]
    pub fn ink_size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    fn new(font: &Face, c: char, px: f32) -> Glyph {
        let b = font.rasterize(c, px);
        Glyph { xmin: b.xmin, ymin: b.ymin, w: b.w, h: b.h, adv: b.adv, cov: b.cov }
    }
}

struct Fallback {
    path: &'static str,
    font: Option<Face>,
    tried: bool,
}

pub struct Font {
    pub cw: i32,
    pub ch: i32,
    pub ascent: i32,
    pub track: f32, // letter-spacing set by `spaced`, already counted in `cw`
    primary: Face,
    fallbacks: Vec<Fallback>,
    px: f32,
    cache: HashMap<char, Glyph>,
}

fn load(path: &str) -> Option<Face> {
    Face::parse(std::fs::read(path).ok()?)
}

impl Font {
    /// JetBrains Mono, bundled: used for the terminal and for all UI text.
    pub fn new(px: f32) -> Self {
        let primary = Face::parse(BUNDLED).or_else(|| FONTS.iter().find_map(|p| load(p))).expect("no monospace font found");
        Self::with(primary, px)
    }

    /// JetBrains Mono Medium (weight 500): tab titles.
    pub fn medium(px: f32) -> Self {
        Self::with(Face::parse(BUNDLED_MEDIUM).unwrap(), px)
    }

    /// Segoe UI, for running text that shares rows with the terminal font: `ascent` is that font's, so baselines line up.
    /// Without Segoe UI (it ships with Windows), the bundled font.
    pub fn prose(px: f32, ascent: i32) -> Self {
        let face = load(r"C:\Windows\Fonts\segoeui.ttf").or_else(|| Face::parse(BUNDLED)).expect("no font found");
        Font { ascent, ..Self::with(face, px) }
    }

    /// How far the pen moves after `c`.
    pub fn advance(&mut self, c: char) -> f32 {
        self.glyph(c).adv
    }

    /// Icon font: a 2 KB subset of Lucide (ISC license, see assets/).
    pub fn icons(px: f32) -> Self {
        Self::with(Face::parse(LUCIDE).unwrap(), px)
    }

    /// Width in pixels of `s` set in this font.
    pub fn width(&mut self, s: &str) -> i32 {
        self.width_tracked(s, 0.0)
    }

    /// Width of `s` with `track` extra pixels after every character (letter-spacing).
    pub fn width_tracked(&mut self, s: &str, track: f32) -> i32 {
        s.chars().map(|c| self.glyph(c).adv + track).sum::<f32>().round() as i32
    }

    fn with(primary: Face, px: f32) -> Self {
        let (ascent, descent) = primary.line_metrics(px);
        Font {
            cw: primary.advance(primary.glyph_index('M'), px).round() as i32,
            ch: (ascent - descent).ceil() as i32,
            ascent: ascent.round() as i32,
            track: 0.0,
            primary,
            fallbacks: FALLBACKS.iter().map(|&path| Fallback { path, font: None, tried: false }).collect(),
            px,
            cache: HashMap::new(),
        }
    }

    /// `track` px of letter-spacing (widens `cw`) and, when `line` > 0, rows `line` times the size with the glyphs
    /// centred in them; 0 keeps the font's own line height.
    pub fn spaced(mut self, line: f32, track: f32) -> Self {
        if line > 0.0 {
            let ch = (self.px * line).round() as i32;
            self.ascent += (ch - self.ch) / 2;
            self.ch = ch;
        }
        self.cw = (self.primary.advance(self.primary.glyph_index('M'), self.px) + track).round() as i32;
        self.track = track;
        self
    }

    pub fn glyph(&mut self, c: char) -> &Glyph {
        if !self.cache.contains_key(&c) {
            let g = self.render(c);
            self.cache.insert(c, g);
        }
        &self.cache[&c]
    }

    fn render(&mut self, c: char) -> Glyph {
        let px = self.px;
        if self.primary.glyph_index(c) == 0 {
            for f in &mut self.fallbacks {
                if !f.tried {
                    f.tried = true;
                    f.font = load(f.path);
                }
                if let Some(font) = &f.font {
                    if font.glyph_index(c) != 0 {
                        return Glyph::new(font, c, px);
                    }
                }
            }
        }
        Glyph::new(&self.primary, c, px)
    }
}

pub fn blend(bg: u32, fg: u32, a: u32) -> u32 {
    let mix = |s: u32| ((bg >> s & 0xff) * (255 - a) + (fg >> s & 0xff) * a) / 255;
    mix(16) << 16 | mix(8) << 8 | mix(0)
}

/// Pixel buffer with a clip rectangle; every primitive respects the clip.
pub struct Canvas<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
    clip: Rect,
    bounds: Rect, // nothing outside this is ever touched: set to the damaged area for a partial redraw
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u32], w: i32, h: i32) -> Self {
        let full = Rect { x: 0, y: 0, w, h };
        Canvas { buf, w, h, clip: full, bounds: full }
    }

    /// Restricts every later drawing call (including `set_clip`) to `r`.
    pub fn set_bounds(&mut self, r: Rect) {
        self.bounds = r.intersect(&Rect { x: 0, y: 0, w: self.w, h: self.h });
        self.clip = self.bounds;
    }

    pub fn set_clip(&mut self, r: Rect) {
        self.clip = r.intersect(&self.bounds);
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    pub fn rect(&mut self, r: Rect, color: u32) {
        let r = r.intersect(&self.clip);
        if r.w == 0 || r.h == 0 {
            return; // fully clipped: x/y may lie outside the buffer
        }
        for y in r.y..r.y + r.h {
            let row = (y * self.w) as usize;
            self.buf[row + r.x as usize..row + (r.x + r.w) as usize].fill(color);
        }
    }

    /// Anti-aliased rounded rectangle; `alpha` is 0..=255 (255 = opaque).
    pub fn rrect(&mut self, r: Rect, rad: i32, color: u32, alpha: u32) {
        let c = r.intersect(&self.clip);
        if c.w == 0 || c.h == 0 {
            return;
        }
        let rad = rad.clamp(0, (r.w / 2).min(r.h / 2));
        for y in c.y..c.y + c.h {
            let row = (y * self.w) as usize;
            let in_band = y < r.y + rad || y >= r.y + r.h - rad; // rows that contain rounded corners
            if !in_band && alpha == 255 {
                self.buf[row + c.x as usize..row + (c.x + c.w) as usize].fill(color);
                continue;
            }
            for x in c.x..c.x + c.w {
                let mut cov = 1.0f32;
                if in_band && (x < r.x + rad || x >= r.x + r.w - rad) {
                    let ax = if x < r.x + rad { r.x + rad } else { r.x + r.w - rad };
                    let ay = if y < r.y + rad { r.y + rad } else { r.y + r.h - rad };
                    let (dx, dy) = (x as f32 + 0.5 - ax as f32, y as f32 + 0.5 - ay as f32);
                    cov = (rad as f32 + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                }
                let a = (cov * alpha as f32) as u32;
                let i = row + x as usize;
                if a >= 255 {
                    self.buf[i] = color;
                } else if a > 0 {
                    self.buf[i] = blend(self.buf[i], color, a);
                }
            }
        }
    }

    /// 1px outlined rounded rect: `border` ring around a `fill` interior.
    pub fn card(&mut self, r: Rect, rad: i32, border: u32, fill: u32) {
        self.rrect(r, rad, border, 255);
        self.rrect(r.inset(1), (rad - 1).max(0), fill, 255);
    }

    /// (x, y) is the top-left of the character cell.
    pub fn glyph(&mut self, font: &mut Font, c: char, x: i32, y: i32, color: u32) {
        let ascent = font.ascent;
        let g = font.glyph(c);
        self.blit(g, x + g.xmin, y + ascent - g.ymin - g.h as i32, color);
    }

    /// Draws `c` with its ink centred on (cx, cy): for icon-font glyphs.
    pub fn glyph_centered(&mut self, font: &mut Font, c: char, cx: i32, cy: i32, color: u32) {
        let g = font.glyph(c);
        self.blit(g, cx - g.w as i32 / 2, cy - g.h as i32 / 2, color);
    }

    fn blit(&mut self, g: &Glyph, left: i32, top: i32, color: u32) {
        let clip = self.clip;
        for gy in 0..g.h as i32 {
            let py = top + gy;
            if py < clip.y || py >= clip.y + clip.h {
                continue;
            }
            for gx in 0..g.w as i32 {
                let px = left + gx;
                let a = g.cov[gy as usize * g.w + gx as usize] as u32;
                if a == 0 || px < clip.x || px >= clip.x + clip.w {
                    continue;
                }
                let i = (py * self.w + px) as usize;
                self.buf[i] = blend(self.buf[i], color, a);
            }
        }
    }

    /// Proportional text; (x, y) is the top-left of the line. Returns the width drawn.
    pub fn text(&mut self, font: &mut Font, s: &str, x: i32, y: i32, color: u32) -> i32 {
        self.text_tracked(font, s, x, y, color, 0.0)
    }

    /// Like `text`, with `track` extra pixels after every character (letter-spacing).
    pub fn text_tracked(&mut self, font: &mut Font, s: &str, x: i32, y: i32, color: u32, track: f32) -> i32 {
        let mut pen = 0.0f32;
        for c in s.chars() {
            if c != ' ' {
                self.glyph(font, c, x + pen.round() as i32, y, color);
            }
            pen += font.glyph(c).adv + track;
        }
        pen.round() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_confine_all_drawing_even_after_set_clip() {
        let mut buf = vec![5u32; 20 * 10];
        let mut cv = Canvas::new(&mut buf, 20, 10);
        cv.set_bounds(Rect { x: 4, y: 2, w: 6, h: 3 });
        cv.set_clip(Rect { x: 0, y: 0, w: 20, h: 10 }); // asking for more must not escape the bounds
        cv.rect(Rect { x: 0, y: 0, w: 20, h: 10 }, 9);
        cv.rrect(Rect { x: 0, y: 0, w: 20, h: 10 }, 3, 9, 255);
        assert_eq!(buf.iter().filter(|&&p| p == 9).count(), 6 * 3);
        assert_eq!(buf[2 * 20 + 4], 9);
        assert_eq!(buf[1 * 20 + 4], 5);
        assert_eq!(buf[2 * 20 + 3], 5);
    }

    #[test]
    fn fully_clipped_rects_do_not_panic() {
        let mut buf = vec![0u32; 10 * 4];
        let mut cv = Canvas::new(&mut buf, 10, 4);
        cv.rect(Rect { x: 50, y: 0, w: 5, h: 4 }, 1); // off the right edge: used to slice out of bounds
        cv.rect(Rect { x: -20, y: 3, w: 5, h: 9 }, 1);
        cv.rrect(Rect { x: 50, y: 0, w: 5, h: 4 }, 2, 1, 255);
        cv.rect(Rect { x: 2, y: 1, w: 3, h: 2 }, 7);
        assert_eq!(buf.iter().filter(|&&p| p == 7).count(), 6);
        assert_eq!(buf.iter().filter(|&&p| p == 1).count(), 0);
    }
}
