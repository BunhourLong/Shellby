//! CPU atlas packing, growth, cap and glyph-cache behaviour (T07 R-1..R-4).

use std::path::Path;

use nt_render::__test::{Atlas, AtlasOp, Font, GlyphCache, Page, Style};
use nt_render::FontConfig;

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

fn cache() -> GlyphCache {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/JetBrainsMono-Regular.ttf");
    let cfg = FontConfig {
        family: None,
        size_pt: 12.0,
        line_height: 1.0,
    };
    let font = Font::from_file(&path, &cfg, 1.0).expect("fixture font");
    GlyphCache::new(font, 4096)
}

/// R-1: 1,000 random-sized bitmaps never overlap and keep 1 px of padding.
#[test]
fn r1_random_packing_no_overlap() {
    let mut atlas = Atlas::new(4096);
    let mut rng = Rng(0x1234_5678);
    let mut placed: Vec<(u32, u32, u32, u32)> = Vec::new();
    for _ in 0..1_000 {
        let w = u16::try_from(rng.range(1, 40)).expect("width fits");
        let h = u16::try_from(rng.range(1, 40)).expect("height fits");
        let data = vec![0x5au8; usize::from(w) * usize::from(h)];
        let [x, y] = atlas
            .insert(Page::Mask, w, h, &data)
            .expect("4096 page holds 1,000 small glyphs");
        let (x, y) = (u32::from(x), u32::from(y));
        let (w, h) = (u32::from(w), u32::from(h));
        let size = atlas.size(Page::Mask);
        assert!(x + w <= size, "x out of bounds at ({x},{y})");
        assert!(y + h <= size, "y out of bounds at ({x},{y})");
        for &(ox, oy, ow, oh) in &placed {
            let separated = x + w < ox || ox + ow < x || y + h < oy || oy + oh < y;
            assert!(
                separated,
                "overlap/missing padding at ({x},{y}) vs ({ox},{oy})"
            );
        }
        placed.push((x, y, w, h));
    }
    assert!(atlas.size(Page::Mask) <= 4096);
}

/// R-2: filling a 1024 page grows to 2048; earlier slots and bytes are preserved.
#[test]
fn r2_growth_preserves_slots_and_bytes() {
    let mut atlas = Atlas::new(4096);
    assert_eq!(atlas.size(Page::Mask), 1024);

    let mut placed: Vec<([u16; 2], Vec<u8>, u16, u16)> = Vec::new();
    let mut uploads: Vec<(u32, u32, u32, u32, Vec<u8>)> = Vec::new();
    let mut grow = None;
    let mut index = 0usize;
    while grow.is_none() {
        let w = 100u16;
        let h = 100u16;
        let len = usize::from(w) * usize::from(h);
        let data: Vec<u8> = (0..len)
            .map(|k| u8::try_from((k + index * 13) % 251).expect("byte fits"))
            .collect();
        let slot = atlas
            .insert(Page::Mask, w, h, &data)
            .expect("page grows before running out");
        placed.push((slot, data, w, h));
        atlas.drain(|op| match op {
            AtlasOp::Grow { page, size } => grow = Some((page, size)),
            AtlasOp::Upload {
                page,
                x,
                y,
                w,
                h,
                data,
            } => {
                assert_eq!(page, Page::Mask);
                uploads.push((x, y, w, h, data.to_vec()));
            }
        });
        index += 1;
        assert!(index < 1_000, "no grow after {index} inserts");
    }

    assert_eq!(grow, Some((Page::Mask, 2048)));
    assert_eq!(atlas.size(Page::Mask), 2048);
    assert_eq!(uploads.len(), placed.len());

    for (k, (upload, place)) in uploads.iter().zip(placed.iter()).enumerate() {
        assert_eq!(upload.0, u32::from(place.0[0]), "upload {k} x");
        assert_eq!(upload.1, u32::from(place.0[1]), "upload {k} y");
        assert_eq!(upload.2, u32::from(place.2), "upload {k} w");
        assert_eq!(upload.3, u32::from(place.3), "upload {k} h");
        assert_eq!(&upload.4, &place.1, "upload {k} bytes");
    }

    let before_grow = placed.len() - 1;
    for (k, place) in placed.iter().take(before_grow).enumerate() {
        assert!(
            u32::from(place.0[0]) + u32::from(place.2) <= 1024,
            "slot {k} moved outside the original page"
        );
        assert!(u32::from(place.0[1]) + u32::from(place.3) <= 1024);
    }
}

/// R-3: a 1024-cap atlas refuses inserts, `clear()` recovers it, and color uploads are 4 B/px.
#[test]
fn r3_cap_clear_and_color_bpp() {
    assert_eq!(Page::Mask.bpp(), 1);
    assert_eq!(Page::Color.bpp(), 4);

    let mut atlas = Atlas::new(1024);
    let data = vec![0x33u8; 100 * 100];
    let mut filled = 0usize;
    loop {
        if atlas.insert(Page::Mask, 100, 100, &data).is_none() {
            break;
        }
        filled += 1;
        assert!(filled <= 200);
    }
    assert_eq!(filled, 90);
    assert_eq!(atlas.size(Page::Mask), 1024);

    atlas.clear();
    assert_eq!(atlas.size(Page::Mask), 1024);
    assert_eq!(atlas.insert(Page::Mask, 100, 100, &data), Some([0, 0]));
    atlas.drain(|_| {});

    let color_data = vec![0x77u8; 10 * 5 * 4];
    assert_eq!(atlas.insert(Page::Color, 10, 5, &color_data), Some([0, 0]));
    let mut uploads = Vec::new();
    atlas.drain(|op| {
        if let AtlasOp::Upload {
            page,
            x,
            y,
            w,
            h,
            data,
        } = op
        {
            uploads.push((page, x, y, w, h, data.len()));
        }
    });
    assert_eq!(uploads, vec![(Page::Color, 0, 0, 10, 5, 200)]);
}

/// R-4: repeated lookups hit the cache; reset bumps the epoch and forces a re-upload.
#[test]
fn r4_glyph_cache_hits_and_epoch() {
    let mut glyphs = cache();
    let first = glyphs.get('a', Style::Regular, false).expect("a glyph");

    let mut uploads = 0usize;
    glyphs.atlas_mut().drain(|op| {
        if matches!(op, AtlasOp::Upload { .. }) {
            uploads += 1;
        }
    });
    assert!(uploads >= 1, "first lookup uploads a glyph");

    let second = glyphs.get('a', Style::Regular, false);
    assert_eq!(second, Some(first));
    let mut again = 0usize;
    glyphs.atlas_mut().drain(|op| {
        if matches!(op, AtlasOp::Upload { .. }) {
            again += 1;
        }
    });
    assert_eq!(again, 0, "cache hit must not upload");

    let epoch = glyphs.epoch();
    glyphs.reset();
    assert_eq!(glyphs.epoch(), epoch + 1);

    let after = glyphs.get('a', Style::Regular, false);
    assert_eq!(after, Some(first), "same slot after a full reset");
    let mut reuploaded = 0usize;
    glyphs.atlas_mut().drain(|op| {
        if matches!(op, AtlasOp::Upload { .. }) {
            reuploaded += 1;
        }
    });
    assert!(reuploaded >= 1, "lookup after reset re-uploads");
}
