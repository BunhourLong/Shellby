//! Procedural glyphs drawn at exact cell size so they connect across cells: box drawing
//! U+2500–257F, block elements U+2580–259F, Powerline U+E0B0–E0B3, and decoration sprites.
//! Pure pixel math; output is an R8 coverage bitmap, row-major, no padding.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::match_same_arms,
    clippy::too_many_lines,
    reason = "pixel math uses small signed coordinates, short arm names and byte coverage"
)]

/// Underline styles drawn from a sprite (solid single underline and strikeout are rects).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Deco {
    /// Two lines `thickness` thick with a `thickness` gap; sprite height 3 × thickness.
    Double,
    /// One sine period per cell; sprite height ≈ 3 × thickness.
    Undercurl,
    /// Dots `thickness` square, gap `thickness`; sprite height = thickness.
    Dotted,
    /// Two dashes per cell, each half of it minus a gap; sprite height = thickness.
    Dashed,
}

const PI: f32 = std::f32::consts::PI;

const ARMS: [u8; 76] = [
    0x05, 0x0A, 0x50, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x44, 0x48, 0x84, 0x88,
    0x41, 0x42, 0x81, 0x82, 0x14, 0x18, 0x24, 0x28, 0x11, 0x12, 0x21, 0x22, 0x54, 0x58, 0x64, 0x94,
    0xA4, 0x68, 0x98, 0xA8, 0x51, 0x52, 0x61, 0x91, 0xA1, 0x62, 0x92, 0xA2, 0x45, 0x46, 0x49, 0x4A,
    0x85, 0x86, 0x89, 0x8A, 0x15, 0x16, 0x19, 0x1A, 0x25, 0x26, 0x29, 0x2A, 0x55, 0x56, 0x59, 0x5A,
    0x65, 0x95, 0xA5, 0x66, 0x69, 0x96, 0x99, 0x6A, 0x9A, 0xA6, 0xA9, 0xAA,
];

const DOUBLE: [u8; 29] = [
    0x0A, 0xA0, 0x48, 0x84, 0x88, 0x42, 0x81, 0x82, 0x18, 0x24, 0x28, 0x12, 0x21, 0x22, 0x58, 0xA4,
    0xA8, 0x52, 0xA1, 0xA2, 0x4A, 0x85, 0x8A, 0x1A, 0x25, 0x2A, 0x5A, 0xA5, 0xAA,
];

const HALF: [u8; 12] = [
    0x01, 0x10, 0x04, 0x40, 0x02, 0x20, 0x08, 0x80, 0x09, 0x90, 0x06, 0x60,
];

struct Canvas<'a> {
    out: &'a mut [u8],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    fn put(&mut self, x: i32, y: i32, cov: u8) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let idx = y as usize * self.w as usize + x as usize;
        if self.out[idx] < cov {
            self.out[idx] = cov;
        }
    }

    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, val: u8) {
        let xs = x0.max(0).min(self.w);
        let xe = x1.max(0).min(self.w);
        let ys = y0.max(0).min(self.h);
        let ye = y1.max(0).min(self.h);
        if xe <= xs || ye <= ys {
            return;
        }
        let w = self.w as usize;
        for y in ys..ye {
            let base = y as usize * w;
            let row = &mut self.out[base + xs as usize..base + xe as usize];
            for p in row.iter_mut() {
                if *p < val {
                    *p = val;
                }
            }
        }
    }

    fn segment(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, half: f32) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len2 = dx * dx + dy * dy;
        let pad = half + 1.0;
        let xs = (x0.min(x1) - pad).floor().max(0.0) as i32;
        let xe = (x0.max(x1) + pad).ceil().min(self.w as f32) as i32;
        let ys = (y0.min(y1) - pad).floor().max(0.0) as i32;
        let ye = (y0.max(y1) + pad).ceil().min(self.h as f32) as i32;
        for y in ys..ye {
            for x in xs..xe {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let t = if len2 > 0.0 {
                    (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let cx = x0 + t * dx;
                let cy = y0 + t * dy;
                let d = ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt();
                let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.put(x, y, (cov * 255.0).round() as u8);
                }
            }
        }
    }

    fn arc(&mut self, ox: f32, oy: f32, r: f32, a0: f32, a1: f32, half: f32) {
        let pad = r + half + 1.0;
        let xs = (ox - pad).floor().max(0.0) as i32;
        let xe = (ox + pad).ceil().min(self.w as f32) as i32;
        let ys = (oy - pad).floor().max(0.0) as i32;
        let ye = (oy + pad).ceil().min(self.h as f32) as i32;
        for y in ys..ye {
            for x in xs..xe {
                let dx = x as f32 + 0.5 - ox;
                let dy = y as f32 + 0.5 - oy;
                let dist = (dx * dx + dy * dy).sqrt();
                let mut ang = dy.atan2(dx);
                while ang < a0 {
                    ang += 2.0 * PI;
                }
                if ang > a1 {
                    continue;
                }
                let cov = (half + 0.5 - (dist - r).abs()).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.put(x, y, (cov * 255.0).round() as u8);
                }
            }
        }
    }
}

/// True if `ch` is drawn by `rasterize` instead of the font.
pub fn covers(ch: char) -> bool {
    let cp = ch as u32;
    (0x2500..=0x259F).contains(&cp) || (0xE0B0..=0xE0B3).contains(&cp)
}

/// Writes a `cell_w × cell_h` coverage bitmap for `ch` into `out` (cleared, then resized;
/// capacity reused). Light lines are `thickness` px, heavy 2 × thickness. Returns false
/// (and leaves `out` empty) if `ch` is not covered. Never panics for sizes 1..=512.
/// For a wide (2-cell) char the caller passes `cell_w` = 2 × the cell width.
pub fn rasterize(ch: char, cell_w: u16, cell_h: u16, thickness: u16, out: &mut Vec<u8>) -> bool {
    if !covers(ch) {
        out.clear();
        return false;
    }
    out.clear();
    out.resize(cell_w as usize * cell_h as usize, 0);
    let t = thickness.max(1) as i32;
    let mut c = Canvas {
        out: out.as_mut_slice(),
        w: cell_w as i32,
        h: cell_h as i32,
    };
    let cp = ch as u32;
    match cp {
        0x2500..=0x2503 | 0x250C..=0x254B => {
            render_arms(&mut c, t, ARMS[(cp - 0x2500) as usize]);
        }
        0x2504..=0x250B | 0x254C..=0x254F => render_dashes(&mut c, t, cp),
        0x2550..=0x256C => render_double(&mut c, t, DOUBLE[(cp - 0x2550) as usize]),
        0x256D..=0x2570 => render_rounded(&mut c, t, cp),
        0x2571..=0x2573 => render_diagonal(&mut c, t, cp),
        0x2574..=0x257F => render_arms(&mut c, t, HALF[(cp - 0x2574) as usize]),
        0x2580..=0x259F => render_block(&mut c, cp),
        0xE0B0..=0xE0B3 => render_powerline(&mut c, t, cp),
        _ => {}
    }
    true
}

/// Writes a `cell_w × height` decoration sprite into `out` and returns `height`.
pub fn deco(kind: Deco, cell_w: u16, thickness: u16, out: &mut Vec<u8>) -> u16 {
    let t = thickness.max(1) as u32;
    let w = cell_w as u32;
    let height = match kind {
        Deco::Double => t * 3,
        Deco::Undercurl => (t * 3).max(3),
        Deco::Dotted => t,
        Deco::Dashed => t,
    }
    .min(u16::MAX as u32) as u16;
    out.clear();
    out.resize(w as usize * height as usize, 0);
    let wi = w as i32;
    let hi = height as i32;
    let ti = t as i32;
    let mut c = Canvas {
        out: out.as_mut_slice(),
        w: wi,
        h: hi,
    };
    match kind {
        Deco::Double => {
            c.rect(0, 0, wi, ti, 255);
            c.rect(0, 2 * ti, wi, 3 * ti, 255);
        }
        Deco::Dotted => {
            for x in 0..wi {
                if (x / ti) % 2 == 0 {
                    c.rect(x, 0, x + 1, hi, 255);
                }
            }
        }
        Deco::Dashed => {
            let gap = (wi / 8).max(1);
            let half = wi / 2;
            if half > gap {
                for k in 0..2 {
                    let hs = k * half;
                    let left = gap / 2;
                    let right = gap - gap / 2;
                    let start = hs + left;
                    let end = hs + half - right;
                    if end > start {
                        c.rect(start, 0, end, ti, 255);
                    }
                }
            }
        }
        Deco::Undercurl => {
            if wi > 0 {
                let amp = (hi as f32 - ti as f32) / 2.0;
                let mid = hi as f32 / 2.0;
                let half = ti as f32 / 2.0;
                for x in 0..wi {
                    let phase = (x as f32 + 0.5) / wi as f32 * 2.0 * PI;
                    let yc = mid + amp * phase.sin();
                    let y0 = (yc - half - 1.0).floor().max(0.0) as i32;
                    let y1 = (yc + half + 1.0).ceil().min(hi as f32) as i32;
                    for y in y0..y1 {
                        let d = (y as f32 + 0.5 - yc).abs();
                        let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                        if cov > 0.0 {
                            c.put(x, y, (cov * 255.0).round() as u8);
                        }
                    }
                }
            }
        }
    }
    height
}

fn render_arms(c: &mut Canvas, t: i32, arms: u8) {
    let cx = c.w / 2;
    let cy = c.h / 2;
    let left = (arms & 0x03) as i32;
    let right = ((arms >> 2) & 0x03) as i32;
    let up = ((arms >> 4) & 0x03) as i32;
    let down = ((arms >> 6) & 0x03) as i32;
    if left > 0 {
        let tt = left * t;
        let y0 = cy - tt / 2;
        c.rect(0, y0, cx + tt / 2, y0 + tt, 255);
    }
    if right > 0 {
        let tt = right * t;
        let y0 = cy - tt / 2;
        c.rect(cx - tt / 2, y0, c.w, y0 + tt, 255);
    }
    if up > 0 {
        let tt = up * t;
        let x0 = cx - tt / 2;
        c.rect(x0, 0, x0 + tt, cy + tt / 2, 255);
    }
    if down > 0 {
        let tt = down * t;
        let x0 = cx - tt / 2;
        c.rect(x0, cy - tt / 2, x0 + tt, c.h, 255);
    }
}

fn render_dashes(c: &mut Canvas, t: i32, cp: u32) {
    let (horizontal, heavy, count) = match cp {
        0x2504 => (true, false, 3),
        0x2505 => (true, true, 3),
        0x2506 => (false, false, 3),
        0x2507 => (false, true, 3),
        0x2508 => (true, false, 4),
        0x2509 => (true, true, 4),
        0x250A => (false, false, 4),
        0x250B => (false, true, 4),
        0x254C => (true, false, 2),
        0x254D => (true, true, 2),
        0x254E => (false, false, 2),
        0x254F => (false, true, 2),
        _ => (true, false, 3),
    };
    let tt = if heavy { 2 * t } else { t };
    let len = if horizontal { c.w } else { c.h };
    let denom = 4 * count;
    for i in 0..count {
        let start = (4 * i + 1) * len / denom;
        let end = (4 * i + 3) * len / denom;
        if horizontal {
            let y0 = c.h / 2 - tt / 2;
            c.rect(start, y0, end, y0 + tt, 255);
        } else {
            let x0 = c.w / 2 - tt / 2;
            c.rect(x0, start, x0 + tt, end, 255);
        }
    }
}

fn render_double(c: &mut Canvas, t: i32, arms: u8) {
    let l = (arms & 0x03) as i32;
    let r = ((arms >> 2) & 0x03) as i32;
    let u = ((arms >> 4) & 0x03) as i32;
    let d = ((arms >> 6) & 0x03) as i32;
    let hs = l.max(r);
    let vs = u.max(d);
    let cx = c.w / 2;
    let cy = c.h / 2;

    if hs == 2 {
        for sy in [-1, 1] {
            let vd = if sy < 0 {
                if u > 0 { -1 } else { 1 }
            } else if d > 0 {
                1
            } else {
                -1
            };
            let x0 = if l > 0 {
                0
            } else if vs == 2 {
                cx + vd * sy * t
            } else {
                cx
            };
            let x1 = if r > 0 {
                c.w
            } else if vs == 2 {
                cx - vd * sy * t
            } else {
                cx
            };
            let y0 = cy + sy * t - t / 2;
            let y1 = y0 + t;
            let split = l > 0 && r > 0 && vs == 2 && ((sy < 0 && u > 0) || (sy > 0 && d > 0));
            if split {
                let g0 = cx - t / 2;
                let g1 = g0 + t;
                c.rect(x0, y0, x1.min(g0), y1, 255);
                c.rect(x0.max(g1), y0, x1, y1, 255);
            } else {
                c.rect(x0, y0, x1, y1, 255);
            }
        }
    } else if hs == 1 {
        let x0 = if l > 0 {
            0
        } else if vs == 2 {
            if u > 0 && d > 0 { cx + t } else { cx - t }
        } else {
            cx
        };
        let x1 = if r > 0 {
            c.w
        } else if vs == 2 {
            if u > 0 && d > 0 { cx - t } else { cx + t }
        } else {
            cx
        };
        let y0 = cy - t / 2;
        c.rect(x0, y0, x1, y0 + t, 255);
    }

    if vs == 2 {
        for sx in [-1, 1] {
            let hd = if sx < 0 {
                if l > 0 { -1 } else { 1 }
            } else if r > 0 {
                1
            } else {
                -1
            };
            let y0 = if u > 0 {
                0
            } else if hs == 2 {
                cy + hd * sx * t
            } else {
                cy
            };
            let y1 = if d > 0 {
                c.h
            } else if hs == 2 {
                cy - hd * sx * t
            } else {
                cy
            };
            let x0 = cx + sx * t - t / 2;
            let x1 = x0 + t;
            let split = u > 0 && d > 0 && hs == 2 && ((sx < 0 && l > 0) || (sx > 0 && r > 0));
            if split {
                let g0 = cy - t / 2;
                let g1 = g0 + t;
                c.rect(x0, y0, x1, y1.min(g0), 255);
                c.rect(x0, y0.max(g1), x1, y1, 255);
            } else {
                c.rect(x0, y0, x1, y1, 255);
            }
        }
    } else if vs == 1 {
        let y0 = if u > 0 {
            0
        } else if hs == 2 {
            if l > 0 && r > 0 { cy + t } else { cy - t }
        } else {
            cy
        };
        let y1 = if d > 0 {
            c.h
        } else if hs == 2 {
            if l > 0 && r > 0 { cy - t } else { cy + t }
        } else {
            cy
        };
        let x0 = cx - t / 2;
        c.rect(x0, y0, x0 + t, y1, 255);
    }
}

struct RoundArm {
    hx0: f32,
    hx1: f32,
    vy0: f32,
    vy1: f32,
    ox: f32,
    oy: f32,
    a0: f32,
    a1: f32,
}

fn render_rounded(c: &mut Canvas, t: i32, cp: u32) {
    let wf = c.w as f32;
    let hf = c.h as f32;
    let cxf = wf / 2.0;
    let cyf = hf / 2.0;
    let r = wf.min(hf) / 2.0;
    let cx = c.w / 2;
    let cy = c.h / 2;
    let arm = match cp {
        0x256D => RoundArm {
            hx0: cxf + r,
            hx1: wf,
            vy0: cyf + r,
            vy1: hf,
            ox: cxf + r,
            oy: cyf + r,
            a0: PI,
            a1: 1.5 * PI,
        },
        0x256E => RoundArm {
            hx0: 0.0,
            hx1: cxf - r,
            vy0: cyf + r,
            vy1: hf,
            ox: cxf - r,
            oy: cyf + r,
            a0: 1.5 * PI,
            a1: 2.0 * PI,
        },
        0x256F => RoundArm {
            hx0: 0.0,
            hx1: cxf - r,
            vy0: 0.0,
            vy1: cyf - r,
            ox: cxf - r,
            oy: cyf - r,
            a0: 0.0,
            a1: 0.5 * PI,
        },
        0x2570 => RoundArm {
            hx0: cxf + r,
            hx1: wf,
            vy0: 0.0,
            vy1: cyf - r,
            ox: cxf + r,
            oy: cyf - r,
            a0: 0.5 * PI,
            a1: PI,
        },
        _ => RoundArm {
            hx0: 0.0,
            hx1: 0.0,
            vy0: 0.0,
            vy1: 0.0,
            ox: 0.0,
            oy: 0.0,
            a0: 0.0,
            a1: 0.0,
        },
    };
    let hy0 = cy - t / 2;
    let hxi0 = arm.hx0.round() as i32;
    let hxi1 = arm.hx1.round() as i32;
    if hxi1 > hxi0 {
        c.rect(hxi0, hy0, hxi1, hy0 + t, 255);
    }
    let vx0 = cx - t / 2;
    let vyi0 = arm.vy0.round() as i32;
    let vyi1 = arm.vy1.round() as i32;
    if vyi1 > vyi0 {
        c.rect(vx0, vyi0, vx0 + t, vyi1, 255);
    }
    if r >= 0.5 {
        c.arc(arm.ox, arm.oy, r, arm.a0, arm.a1, t as f32 / 2.0);
    }
}

fn render_diagonal(c: &mut Canvas, t: i32, cp: u32) {
    let w = c.w as f32;
    let h = c.h as f32;
    let half = t as f32 / 2.0;
    match cp {
        0x2571 => c.segment(0.0, h, w, 0.0, half),
        0x2572 => c.segment(0.0, 0.0, w, h, half),
        0x2573 => {
            c.segment(0.0, 0.0, w, h, half);
            c.segment(0.0, h, w, 0.0, half);
        }
        _ => {}
    }
}

fn render_block(c: &mut Canvas, cp: u32) {
    let w = c.w;
    let h = c.h;
    let bw = boundary(4, w);
    let bh = boundary(4, h);
    match cp {
        0x2580 => c.rect(0, 0, w, bh, 255),
        0x2581..=0x2587 => {
            let n = (cp - 0x2580) as i32;
            let top = h - boundary(n, h);
            c.rect(0, top, w, h, 255);
        }
        0x2588 => c.rect(0, 0, w, h, 255),
        0x2589..=0x258F => {
            let n = (0x2590 - cp) as i32;
            let right = boundary(n, w);
            c.rect(0, 0, right, h, 255);
        }
        0x2590 => c.rect(bw, 0, w, h, 255),
        0x2591 => c.out.fill(64),
        0x2592 => c.out.fill(128),
        0x2593 => c.out.fill(191),
        0x2594 => {
            let bottom = boundary(1, h);
            c.rect(0, 0, w, bottom, 255);
        }
        0x2595 => {
            let left = w - boundary(1, w);
            c.rect(left, 0, w, h, 255);
        }
        0x2596..=0x259F => {
            for y in 0..h {
                for x in 0..w {
                    let left = x < bw;
                    let right = x >= bw;
                    let top = y < bh;
                    let bottom = y >= bh;
                    let on = match cp {
                        0x2596 => left && bottom,
                        0x2597 => right && bottom,
                        0x2598 => left && top,
                        0x2599 => !(right && top),
                        0x259A => (left && top) || (right && bottom),
                        0x259B => !(right && bottom),
                        0x259C => !(left && bottom),
                        0x259D => right && top,
                        0x259E => (right && top) || (left && bottom),
                        0x259F => !(left && top),
                        _ => false,
                    };
                    if on {
                        c.put(x, y, 255);
                    }
                }
            }
        }
        _ => {}
    }
}

fn boundary(n: i32, size: i32) -> i32 {
    if size <= 0 {
        return 0;
    }
    ((n * size + 4) / 8).clamp(0, size)
}

fn render_powerline(c: &mut Canvas, t: i32, cp: u32) {
    let w = c.w;
    let h = c.h;
    let wf = w as f32;
    let hf = h as f32;
    let half = t as f32 / 2.0;
    match cp {
        0xE0B0 => {
            for y in 0..h {
                for x in 0..w {
                    let fx = if w > 1 {
                        x as f32 / (w - 1) as f32
                    } else {
                        0.0
                    };
                    let dy = hf / 2.0 * (1.0 - fx);
                    let d = (y as f32 + 0.5 - hf / 2.0).abs();
                    let cov = (dy - d + 0.5).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        c.put(x, y, (cov * 255.0).round() as u8);
                    }
                }
            }
        }
        0xE0B2 => {
            for y in 0..h {
                for x in 0..w {
                    let fx = if w > 1 {
                        x as f32 / (w - 1) as f32
                    } else {
                        0.0
                    };
                    let dy = hf / 2.0 * fx;
                    let d = (y as f32 + 0.5 - hf / 2.0).abs();
                    let cov = (dy - d + 0.5).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        c.put(x, y, (cov * 255.0).round() as u8);
                    }
                }
            }
        }
        0xE0B1 => {
            c.segment(0.0, 0.0, wf, hf / 2.0, half);
            c.segment(0.0, hf, wf, hf / 2.0, half);
        }
        0xE0B3 => {
            c.segment(wf, 0.0, 0.0, hf / 2.0, half);
            c.segment(wf, hf, 0.0, hf / 2.0, half);
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(
    clippy::needless_range_loop,
    reason = "index arithmetic mirrors the bitmap layout"
)]
mod tests {
    use super::*;

    fn raster(ch: char, w: u16, h: u16, t: u16) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(rasterize(ch, w, h, t, &mut out));
        assert_eq!(out.len(), w as usize * h as usize);
        out
    }

    #[test]
    fn covers_ranges() {
        assert!(!covers('\u{24FF}'));
        assert!(covers('\u{2500}'));
        assert!(covers('\u{259F}'));
        assert!(!covers('\u{25A0}'));
        assert!(!covers('\u{E0AF}'));
        assert!(covers('\u{E0B0}'));
        assert!(covers('\u{E0B3}'));
        assert!(!covers('\u{E0B4}'));
    }

    #[test]
    fn box_lines_tile_and_align() {
        for &(w, h) in &[(7u16, 15u16), (8, 16), (9, 19), (20, 41)] {
            for &t in &[1u16, 2] {
                let wi = w as usize;
                let hi = h as usize;
                let dash = raster('─', w, h, t);
                let tee = raster('├', w, h, t);
                let cross = raster('┼', w, h, t);
                for y in 0..hi {
                    let full = dash[y * wi..(y + 1) * wi].iter().all(|&v| v == 255);
                    assert_eq!(full, tee[y * wi + wi - 1] > 0, "tee rows {w}x{h} t{t}");
                    assert_eq!(full, cross[y * wi + wi - 1] > 0, "cross rows {w}x{h} t{t}");
                }
                let pipe = raster('│', w, h, t);
                for x in 0..wi {
                    let full = (0..hi).all(|y| pipe[y * wi + x] == 255);
                    assert_eq!(full, cross[(hi - 1) * wi + x] > 0, "pipe cols {w}x{h} t{t}");
                }
                assert_eq!(cross[(hi / 2) * wi + wi / 2], 255);
            }
        }
    }

    #[test]
    fn blocks_and_quadrants() {
        for &(w, h) in &[(7u16, 15u16), (8, 16), (9, 19), (20, 41)] {
            let wi = w as usize;
            let hi = h as usize;
            let full = raster('█', w, h, 1);
            assert!(full.iter().all(|&v| v == 255));

            let upper = raster('▀', w, h, 1);
            let bh = ((4 * h as i32 + 4) / 8) as usize;
            for y in 0..hi {
                for x in 0..wi {
                    let v = upper[y * wi + x];
                    if y < bh {
                        assert_eq!(v, 255);
                    } else {
                        assert_eq!(v, 0);
                    }
                }
            }

            let left = raster('▌', w, h, 1);
            let right = raster('▐', w, h, 1);
            for i in 0..full.len() {
                assert_eq!(left[i] as u16 + right[i] as u16, 255);
            }

            let mut prev = 0u64;
            for cp in 0x2581u32..=0x2588 {
                let ch = char::from_u32(cp).unwrap();
                let v = raster(ch, w, h, 1);
                let sum: u64 = v.iter().map(|&p| p as u64).sum();
                assert!(sum >= prev, "eighths not monotonic at {w}x{h}");
                prev = sum;
            }
        }
    }

    #[test]
    fn rejects_uncovered() {
        let mut out = vec![1, 2, 3];
        assert!(!rasterize('A', 8, 16, 1, &mut out));
        assert_eq!(out, [] as [u8; 0]);
    }

    #[test]
    fn no_panic_all_sizes() {
        let chars = (0x2500u32..=0x259F).chain(0xE0B0..=0xE0B3);
        for cp in chars {
            let ch = char::from_u32(cp).unwrap();
            for &(w, h) in &[(1u16, 1u16), (2, 3)] {
                for t in 1u16..=8 {
                    let mut out = Vec::new();
                    assert!(rasterize(ch, w, h, t, &mut out));
                    assert_eq!(out.len(), w as usize * h as usize);
                }
            }
            let mut out = Vec::new();
            assert!(rasterize(ch, 512, 512, 8, &mut out));
            assert_eq!(out.len(), 512 * 512);
        }
    }

    #[test]
    fn deco_heights() {
        for &w in &[1u16, 8, 13, 40] {
            for &t in &[1u16, 2, 3, 8] {
                let cases = [
                    (Deco::Double, 3 * t),
                    (Deco::Undercurl, (3 * t).max(3)),
                    (Deco::Dotted, t),
                    (Deco::Dashed, t),
                ];
                for (kind, want) in cases {
                    let mut out = vec![9];
                    let got = deco(kind, w, t, &mut out);
                    assert_eq!(got, want);
                    assert_eq!(out.len(), w as usize * want as usize);
                }
            }
        }
    }
}
