//! Shelf packer + CPU-side R8 atlas image. Uploaded once with a single write_texture of the used rows.

/// Shelf packer: rows ("shelves") of the height of their first glyph; a glyph goes on the first
/// shelf that is tall enough and has room, else opens a new shelf. Good enough for near-uniform glyph sizes.
pub struct Atlas {
    pub w: u32,
    pub h: u32,
    pub pixels: Vec<u8>,
    /// (y, height, next free x)
    shelves: Vec<(u32, u32, u32)>,
}

/// 1 px gap so bilinear sampling (if T07 ever uses it) never bleeds between glyphs.
const PAD: u32 = 1;

impl Atlas {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            pixels: vec![0; (w * h) as usize],
            shelves: Vec::new(),
        }
    }

    /// Reserve a w×h rect; None when the atlas is full (caller decides: grow or clear + re-rasterise).
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (pw, ph) = (w + PAD, h + PAD);
        if pw > self.w {
            return None;
        }
        for s in &mut self.shelves {
            if ph <= s.1 && s.2 + pw <= self.w {
                let x = s.2;
                s.2 += pw;
                return Some((x, s.0));
            }
        }
        let y = self.shelves.last().map_or(0, |s| s.0 + s.1);
        if y + ph > self.h {
            return None;
        }
        self.shelves.push((y, ph, pw));
        Some((0, y))
    }

    /// Copy an R8 bitmap (row-major, len w*h) to (x, y).
    pub fn blit(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
        assert_eq!(src.len(), (w * h) as usize, "glyph bitmap size mismatch");
        for r in 0..h {
            let d = ((y + r) * self.w + x) as usize;
            let s = (r * w) as usize;
            self.pixels[d..d + w as usize].copy_from_slice(&src[s..s + w as usize]);
        }
    }

    /// Rows actually used (upload only these).
    pub fn used_h(&self) -> u32 {
        self.shelves.last().map_or(0, |s| s.0 + s.1).min(self.h)
    }
}
