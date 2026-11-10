//! Glyph cache: (char, style, wide) → atlas slot, with an ASCII fast table, negative
//! caching, built-in glyphs, decoration sprites, tofu, and an atlas epoch. No wgpu types.

use std::collections::HashMap;

use crate::atlas::{Atlas, Page};
use crate::builtin::{self, Deco};
use crate::font::{Font, Metrics, Raster};
use nt_term::CellFlags;

/// Font style, from the cell's BOLD/ITALIC flags.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    /// Regular.
    Regular = 0,
    /// Bold.
    Bold = 1,
    /// Italic.
    Italic = 2,
    /// Bold italic.
    BoldItalic = 3,
}

impl Style {
    /// Style for a cell's flags.
    pub fn from_flags(flags: CellFlags) -> Self {
        match (
            flags.contains(CellFlags::BOLD),
            flags.contains(CellFlags::ITALIC),
        ) {
            (false, false) => Self::Regular,
            (true, false) => Self::Bold,
            (false, true) => Self::Italic,
            (true, true) => Self::BoldItalic,
        }
    }
}

/// Where a glyph sits in the atlas and where to draw it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GlyphSlot {
    /// Atlas page.
    pub page: Page,
    /// Texel rect `[x, y, w, h]`; the quad is drawn `w × h` px (no scaling on the GPU).
    pub uv: [u16; 4],
    /// Bitmap top-left relative to the cell's top-left, px (baseline and bearings applied).
    pub offset: [i16; 2],
}

const DECO_COUNT: usize = 4;

fn deco_index(kind: Deco) -> usize {
    match kind {
        Deco::Double => 0,
        Deco::Undercurl => 1,
        Deco::Dotted => 2,
        Deco::Dashed => 3,
    }
}

fn offset_y(baseline: u16, top: i16) -> i16 {
    (i32::from(baseline) - i32::from(top)).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Glyph cache owning the font and the atlas. Invalidated wholesale by `epoch` bumps.
#[derive(Debug)]
#[allow(
    clippy::option_option,
    reason = "outer Option = cached, inner Option = blank glyph"
)]
pub struct GlyphCache {
    font: Font,
    metrics: Metrics,
    ascii: [[Option<Option<GlyphSlot>>; 128]; 4],
    // shortcut: std SipHash off the ASCII path; swap the hasher only if a bench shows it.
    map: HashMap<(char, Style, bool), Option<GlyphSlot>>,
    deco: [Option<GlyphSlot>; DECO_COUNT],
    atlas: Atlas,
    epoch: u32,
    scratch: Vec<u8>,
    scaled: Vec<u8>,
}

impl GlyphCache {
    /// New cache; `max_dim` = the device's `max_texture_dimension_2d`.
    pub fn new(font: Font, max_dim: u32) -> Self {
        let metrics = font.metrics();
        Self {
            font,
            metrics,
            ascii: [[None; 128]; 4],
            map: HashMap::new(),
            deco: [None; DECO_COUNT],
            atlas: Atlas::new(max_dim),
            epoch: 0,
            scratch: Vec::new(),
            scaled: Vec::new(),
        }
    }

    /// Current cell metrics (the font's).
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Bumps whenever previously returned slots become invalid. Layers that encoded with
    /// another epoch must re-encode everything.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Slot for `ch`; `wide` = the cell is a 2-column char (glyph fits a 2-cell box).
    /// `None` = nothing to draw (blank glyph). Chars no face has get a tofu box.
    /// Built-in chars come from `builtin::rasterize` at exact cell size; color glyphs larger
    /// than their box are scaled down to fit, centred. Cache hits do not allocate.
    pub fn get(&mut self, ch: char, style: Style, wide: bool) -> Option<GlyphSlot> {
        let ascii = (ch as u32) < 128 && !wide;
        let style_index = style as usize;
        let char_index = ch as usize;
        if ascii {
            if let Some(cached) = self.ascii[style_index][char_index] {
                return cached;
            }
        } else if let Some(cached) = self.map.get(&(ch, style, wide)) {
            return *cached;
        }
        let slot = self.build_slot(ch, style, wide);
        if ascii {
            self.ascii[style_index][char_index] = Some(slot);
        } else {
            self.map.insert((ch, style, wide), slot);
        }
        slot
    }

    fn build_slot(&mut self, ch: char, style: Style, wide: bool) -> Option<GlyphSlot> {
        let cell_w = self.metrics.cell_w;
        let box_w = if wide {
            cell_w.saturating_mul(2)
        } else {
            cell_w
        };
        let box_h = self.metrics.cell_h;
        let thickness = self.metrics.thickness;

        if builtin::covers(ch) {
            if !builtin::rasterize(ch, box_w, box_h, thickness, &mut self.scratch) {
                return None;
            }
            return self.place(Page::Mask, box_w, box_h, [0, 0]);
        }

        let Some(raster) = self.font.rasterize(ch, style, &mut self.scratch) else {
            self.draw_tofu(box_w, box_h, thickness);
            return self.place(Page::Mask, box_w, box_h, [0, 0]);
        };
        if raster.w == 0 || raster.h == 0 {
            return None;
        }
        let baseline = self.metrics.baseline;
        if raster.color {
            if raster.w > box_w || raster.h > box_h {
                let (w, h) = self.scale_color(&raster, box_w, box_h);
                let x = (i32::from(box_w) - i32::from(w)) / 2;
                let y = (i32::from(box_h) - i32::from(h)) / 2;
                self.place(Page::Color, w, h, [x as i16, y as i16])
            } else {
                let offset = [raster.left, offset_y(baseline, raster.top)];
                self.place(Page::Color, raster.w, raster.h, offset)
            }
        } else {
            let offset = [raster.left, offset_y(baseline, raster.top)];
            self.place(Page::Mask, raster.w, raster.h, offset)
        }
    }

    fn place(&mut self, page: Page, w: u16, h: u16, offset: [i16; 2]) -> Option<GlyphSlot> {
        let scratch = std::mem::take(&mut self.scratch);
        let mut position = self.atlas.insert(page, w, h, &scratch);
        if position.is_none() {
            self.reset();
            position = self.atlas.insert(page, w, h, &scratch);
        }
        self.scratch = scratch;
        let [x, y] = position?;
        Some(GlyphSlot {
            page,
            uv: [x, y, w, h],
            offset,
        })
    }

    fn draw_tofu(&mut self, w: u16, h: u16, thickness: u16) {
        let width = usize::from(w);
        let height = usize::from(h);
        self.scratch.clear();
        self.scratch.resize(width * height, 0);
        if width == 0 || height == 0 {
            return;
        }
        let inset = if w.min(h) >= 10 { 2 } else { 1 };
        let left = inset.min(w.saturating_sub(1));
        let top = inset.min(h.saturating_sub(1));
        let right = w.saturating_sub(inset).saturating_sub(1).max(left);
        let bottom = h.saturating_sub(inset).saturating_sub(1).max(top);
        let span_x = right - left + 1;
        let span_y = bottom - top + 1;
        let stroke = thickness.max(1).min((span_x.min(span_y) / 2).max(1));
        for y in top..=bottom {
            for x in left..=right {
                let border = x < left.saturating_add(stroke)
                    || x.saturating_add(stroke) > right.saturating_add(1)
                    || y < top.saturating_add(stroke)
                    || y.saturating_add(stroke) > bottom.saturating_add(1);
                if border {
                    self.scratch[usize::from(y) * width + usize::from(x)] = 255;
                }
            }
        }
    }

    #[allow(
        clippy::cast_sign_loss,
        reason = "the scaled size is clamped to 1..=box_w/box_h before the cast"
    )]
    // shortcut: nearest-neighbour downscale; use a box filter if emoji look rough.
    fn scale_color(&mut self, raster: &Raster, box_w: u16, box_h: u16) -> (u16, u16) {
        let src_w = u32::from(raster.w);
        let src_h = u32::from(raster.h);
        let scale = f64::from(u32::from(box_w)) / f64::from(src_w);
        let scale_h = f64::from(u32::from(box_h)) / f64::from(src_h);
        let scale = scale.min(scale_h);
        let dst_w = (f64::from(src_w) * scale)
            .round()
            .clamp(1.0, f64::from(box_w)) as u16;
        let dst_h = (f64::from(src_h) * scale)
            .round()
            .clamp(1.0, f64::from(box_h)) as u16;
        let dw = u32::from(dst_w);
        let dh = u32::from(dst_h);
        self.scaled.clear();
        self.scaled
            .resize(usize::from(dst_w) * usize::from(dst_h) * 4, 0);
        let src_stride = usize::try_from(src_w).unwrap_or(0) * 4;
        let dst_stride = usize::from(dst_w) * 4;
        for dy in 0..dh {
            let sy = usize::try_from(dy * src_h / dh).unwrap_or(0);
            let dst_row = usize::try_from(dy).unwrap_or(0) * dst_stride;
            for dx in 0..dw {
                let sx = usize::try_from(dx * src_w / dw).unwrap_or(0);
                let si = sy * src_stride + sx * 4;
                let di = dst_row + usize::try_from(dx).unwrap_or(0) * 4;
                self.scaled[di..di + 4].copy_from_slice(&self.scratch[si..si + 4]);
            }
        }
        std::mem::swap(&mut self.scratch, &mut self.scaled);
        (dst_w, dst_h)
    }

    /// Decoration sprite slot (cell-wide), positioned at the underline row.
    pub fn deco(&mut self, kind: Deco) -> Option<GlyphSlot> {
        let index = deco_index(kind);
        if let Some(cached) = self.deco[index] {
            return Some(cached);
        }
        let cell_w = self.metrics.cell_w;
        let height = builtin::deco(kind, cell_w, self.metrics.thickness, &mut self.scratch);
        let slot = if height == 0 {
            None
        } else {
            let centre =
                i32::from(self.metrics.underline_y) + i32::from(self.metrics.thickness) / 2;
            let y = centre - i32::from(height) / 2;
            let max_y = (i32::from(self.metrics.cell_h) - i32::from(height)).max(0);
            let y = y.clamp(0, max_y) as i16;
            self.place(Page::Mask, cell_w, height, [0, y])
        };
        self.deco[index] = slot;
        slot
    }

    /// The font, for `rescale`; call `reset` afterwards.
    pub fn font_mut(&mut self) -> &mut Font {
        &mut self.font
    }

    /// Replaces the font (e.g. a new family); implies `reset`.
    pub fn set_font(&mut self, font: Font) {
        self.font = font;
        self.reset();
    }

    /// Recomputes metrics from the font, empties the cache and the atlas, bumps the epoch.
    pub fn reset(&mut self) {
        self.metrics = self.font.metrics();
        self.ascii = [[None; 128]; 4];
        self.map.clear();
        self.deco = [None; DECO_COUNT];
        self.atlas.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// The atlas, for `gpu.rs` to read sizes and drain pending ops.
    pub fn atlas_mut(&mut self) -> &mut Atlas {
        &mut self.atlas
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::FontConfig;

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

    #[test]
    fn same_char_twice_returns_same_slot() {
        let mut glyphs = cache();
        let first = glyphs.get('a', Style::Regular, false);
        let second = glyphs.get('a', Style::Regular, false);
        assert!(first.is_some());
        assert_eq!(first, second);
        assert_eq!(glyphs.epoch(), 0);
    }

    #[test]
    fn blank_glyph_is_cached_as_none() {
        let mut glyphs = cache();
        assert_eq!(glyphs.get(' ', Style::Regular, false), None);
        assert_eq!(glyphs.get(' ', Style::Regular, false), None);
    }

    #[test]
    fn builtin_box_drawing_uses_full_cell_width() {
        let mut glyphs = cache();
        let cell_w = glyphs.metrics().cell_w;
        let slot = glyphs.get('─', Style::Regular, false).expect("box drawing");
        assert_eq!(slot.page, Page::Mask);
        assert_eq!(slot.uv[2], cell_w);
    }

    #[test]
    fn wide_builtin_uses_two_cells() {
        let mut glyphs = cache();
        let cell_w = glyphs.metrics().cell_w;
        let slot = glyphs
            .get('─', Style::Regular, true)
            .expect("wide box drawing");
        assert_eq!(slot.uv[2], cell_w.saturating_mul(2));
    }

    #[test]
    fn reset_bumps_epoch_and_clears_atlas() {
        let mut glyphs = cache();
        let _ = glyphs.get('a', Style::Regular, false);
        assert!(glyphs.atlas_mut().has_pending());
        glyphs.reset();
        assert_eq!(glyphs.epoch(), 1);
        assert!(!glyphs.atlas_mut().has_pending());
    }

    #[test]
    fn set_font_resets_and_updates_metrics() {
        let mut glyphs = cache();
        let old = glyphs.metrics();
        glyphs.reset();
        assert_eq!(glyphs.epoch(), 1);
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/JetBrainsMono-Regular.ttf");
        let cfg = FontConfig {
            family: None,
            size_pt: 24.0,
            line_height: 1.0,
        };
        let font = Font::from_file(&path, &cfg, 1.0).expect("fixture font");
        glyphs.set_font(font);
        assert_eq!(glyphs.epoch(), 2);
        assert!(glyphs.metrics().cell_w > old.cell_w);
    }
}
