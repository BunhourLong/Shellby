//! Two-page glyph atlas (R8 mask, RGBA8 premultiplied color): shelf packing with CPU-staged
//! uploads and a grow op log that `gpu.rs` replays. No wgpu types.

/// Atlas page.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    /// R8 coverage, 1 byte per texel.
    Mask = 0,
    /// RGBA8 premultiplied, 4 bytes per texel.
    Color = 1,
}

impl Page {
    /// Bytes per texel.
    pub fn bpp(self) -> usize {
        match self {
            Self::Mask => 1,
            Self::Color => 4,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Mask => 0,
            Self::Color => 1,
        }
    }
}

/// A pending GPU operation, replayed in order by `Atlas::drain`.
#[derive(Debug, PartialEq, Eq)]
pub enum AtlasOp<'a> {
    /// Recreate `page`'s texture at `size × size`, copying the old contents to (0, 0).
    Grow {
        /// Page.
        page: Page,
        /// New edge length in texels.
        size: u32,
    },
    /// Write `data` (tightly packed rows, `w * page.bpp()` bytes each) at `(x, y)`.
    Upload {
        /// Page.
        page: Page,
        /// Left texel.
        x: u32,
        /// Top texel.
        y: u32,
        /// Width in texels.
        w: u32,
        /// Height in texels.
        h: u32,
        /// Texel bytes.
        data: &'a [u8],
    },
}

#[derive(Debug)]
struct Shelf {
    y: u32,
    h: u32,
    next_x: u32,
}

#[derive(Debug, Default)]
struct PageState {
    size: u32,
    shelves: Vec<Shelf>,
}

#[derive(Debug)]
enum Staged {
    Grow {
        page: Page,
        size: u32,
    },
    Upload {
        page: Page,
        x: u32,
        y: u32,
        w: u16,
        h: u16,
        start: usize,
        end: usize,
    },
}

/// Shelf-packed atlas. Each page starts at `min(1024, max_dim)` and grows ×2 up to
/// `min(4096, max_dim)`; shelf heights are rounded up to a multiple of 4 px; 1 px padding
/// between glyphs.
#[derive(Debug)]
pub struct Atlas {
    pages: [PageState; 2],
    cap: u32,
    staging: Vec<u8>,
    ops: Vec<Staged>,
}

impl Atlas {
    /// Empty atlas; `max_dim` = the device's `max_texture_dimension_2d`.
    pub fn new(max_dim: u32) -> Self {
        let cap = max_dim.clamp(1, 4096);
        let start = max_dim.min(1024).clamp(1, cap);
        Self {
            pages: [
                PageState {
                    size: start,
                    shelves: Vec::new(),
                },
                PageState {
                    size: start,
                    shelves: Vec::new(),
                },
            ],
            cap,
            staging: Vec::new(),
            ops: Vec::new(),
        }
    }

    /// Places a `w × h` bitmap (`data` = tightly packed rows) and stages its upload,
    /// growing the page if needed. Returns the top-left texel, or `None` if the page is
    /// full at its cap (the caller then clears). `w == 0 || h == 0` is a caller bug.
    pub fn insert(&mut self, page: Page, w: u16, h: u16, data: &[u8]) -> Option<[u16; 2]> {
        debug_assert!(w > 0 && h > 0, "atlas insert with zero size");
        debug_assert_eq!(
            data.len(),
            usize::from(w) * usize::from(h) * page.bpp(),
            "bitmap size mismatch"
        );
        if w == 0 || h == 0 {
            return None;
        }
        let bucket = (u32::from(h) + 4) & !3;
        let pad_w = u32::from(w) + 1;
        let idx = page.index();
        loop {
            let size = self.pages[idx].size;
            if let Some([x, y]) = self.try_place(idx, bucket, pad_w) {
                let start = self.staging.len();
                self.staging.extend_from_slice(data);
                let end = self.staging.len();
                self.ops.push(Staged::Upload {
                    page,
                    x,
                    y,
                    w,
                    h,
                    start,
                    end,
                });
                return Some([x as u16, y as u16]);
            }
            if size >= self.cap {
                return None;
            }
            let new_size = (size * 2).min(self.cap);
            self.ops.push(Staged::Grow {
                page,
                size: new_size,
            });
            self.pages[idx].size = new_size;
        }
    }

    fn try_place(&mut self, idx: usize, bucket: u32, pad_w: u32) -> Option<[u32; 2]> {
        let page = &mut self.pages[idx];
        let size = page.size;
        for shelf in &mut page.shelves {
            if shelf.h == bucket && shelf.next_x + pad_w <= size {
                let x = shelf.next_x;
                shelf.next_x += pad_w;
                return Some([x, shelf.y]);
            }
        }
        let y = page.shelves.last().map_or(0, |s| s.y + s.h);
        if y + bucket > size {
            return None;
        }
        page.shelves.push(Shelf {
            y,
            h: bucket,
            next_x: pad_w,
        });
        Some([0, y])
    }

    /// Forgets every placement and pending op; page sizes are kept.
    pub fn clear(&mut self) {
        for page in &mut self.pages {
            page.shelves.clear();
        }
        self.ops.clear();
        self.staging.clear();
    }

    /// Current edge length of `page` in texels.
    pub fn size(&self, page: Page) -> u32 {
        self.pages[page.index()].size
    }

    /// True if `drain` would emit anything.
    pub fn has_pending(&self) -> bool {
        !self.ops.is_empty()
    }

    /// Calls `f` with every pending op in order, then forgets them (capacity kept).
    pub fn drain(&mut self, mut f: impl FnMut(AtlasOp<'_>)) {
        for op in &self.ops {
            match *op {
                Staged::Grow { page, size } => f(AtlasOp::Grow { page, size }),
                Staged::Upload {
                    page,
                    x,
                    y,
                    w,
                    h,
                    start,
                    end,
                } => f(AtlasOp::Upload {
                    page,
                    x,
                    y,
                    w: u32::from(w),
                    h: u32::from(h),
                    data: &self.staging[start..end],
                }),
            }
        }
        self.ops.clear();
        self.staging.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAD: u32 = 1;

    struct Rng(u32);

    impl Rng {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + self.next() % (hi - lo + 1)
        }
    }

    fn fill(w: u16, h: u16, bpp: usize) -> Vec<u8> {
        vec![0x5a; usize::from(w) * usize::from(h) * bpp]
    }

    #[test]
    fn random_packing_has_no_overlap_and_respects_padding() {
        let mut atlas = Atlas::new(4096);
        let mut rng = Rng(0x1234_5678);
        let mut placed: Vec<(Page, u32, u32, u32, u32)> = Vec::new();
        for i in 0..300 {
            let page = if i % 5 == 0 { Page::Color } else { Page::Mask };
            let w = u16::try_from(rng.range(1, 40)).unwrap();
            let h = u16::try_from(rng.range(1, 40)).unwrap();
            let data = fill(w, h, page.bpp());
            let [x, y] = atlas.insert(page, w, h, &data).expect("atlas full");
            let (x, y) = (u32::from(x), u32::from(y));
            let size = atlas.size(page);
            assert!(x + u32::from(w) <= size, "x out of bounds");
            assert!(y + u32::from(h) <= size, "y out of bounds");
            for &(p, ox, oy, ow, oh) in &placed {
                if p != page {
                    continue;
                }
                let separated = x + u32::from(w) + PAD <= ox
                    || ox + ow + PAD <= x
                    || y + u32::from(h) + PAD <= oy
                    || oy + oh + PAD <= y;
                assert!(
                    separated,
                    "overlap or missing padding at {x},{y} vs {ox},{oy}"
                );
            }
            placed.push((page, x, y, u32::from(w), u32::from(h)));
        }
    }

    #[test]
    fn growth_emits_doubled_size_and_keeps_old_slots() {
        let mut atlas = Atlas::new(4096);
        let data = vec![7u8; 1023 * 1020];
        let first = atlas.insert(Page::Mask, 1023, 1020, &data).unwrap();
        assert_eq!(first, [0, 0]);
        let second = atlas.insert(Page::Mask, 1023, 1020, &data).unwrap();
        assert_eq!(atlas.size(Page::Mask), 2048);
        assert_eq!(second, [1024, 0]);
        let mut grows = Vec::new();
        let mut uploads = Vec::new();
        atlas.drain(|op| match op {
            AtlasOp::Grow { page, size } => grows.push((page, size)),
            AtlasOp::Upload { x, y, .. } => uploads.push((x, y)),
        });
        assert_eq!(grows, vec![(Page::Mask, 2048)]);
        assert!(uploads.contains(&(0, 0)));
    }

    #[test]
    fn insert_returns_none_at_cap() {
        let mut atlas = Atlas::new(1024);
        let data = vec![1u8; 1023 * 1020];
        assert!(atlas.insert(Page::Mask, 1023, 1020, &data).is_some());
        assert!(
            atlas
                .insert(Page::Mask, 100, 1020, &data[..100 * 1020])
                .is_none()
        );
        assert_eq!(atlas.size(Page::Mask), 1024);
    }

    #[test]
    fn drain_replays_ops_in_order_with_staging_bytes() {
        let mut atlas = Atlas::new(2048);
        let a = [1u8, 2, 3, 4];
        let b = [9u8, 8, 7, 6];
        atlas.insert(Page::Mask, 2, 2, &a).unwrap();
        atlas.insert(Page::Color, 1, 1, &b).unwrap();
        let mut uploads = Vec::new();
        let mut grows = 0;
        let mut pending = atlas.has_pending();
        atlas.drain(|op| match op {
            AtlasOp::Grow { .. } => grows += 1,
            AtlasOp::Upload {
                page,
                x,
                y,
                w,
                h,
                data,
            } => {
                uploads.push((page, x, y, w, h, data.to_vec()));
            }
        });
        assert!(pending);
        assert_eq!(grows, 0);
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0].0, Page::Mask);
        assert_eq!(uploads[0].5, a);
        assert_eq!(uploads[1].0, Page::Color);
        assert_eq!(uploads[1].5, b);
        pending = atlas.has_pending();
        assert!(!pending);
        atlas.drain(|_| panic!("drained twice"));
    }

    #[test]
    fn clear_keeps_sizes_and_forgets_placements() {
        let mut atlas = Atlas::new(2048);
        let data = vec![3u8; 100 * 100];
        atlas.insert(Page::Mask, 100, 100, &data).unwrap();
        let size = atlas.size(Page::Mask);
        atlas.clear();
        assert_eq!(atlas.size(Page::Mask), size);
        assert!(!atlas.has_pending());
        assert_eq!(atlas.insert(Page::Mask, 100, 100, &data), Some([0, 0]));
    }
}
