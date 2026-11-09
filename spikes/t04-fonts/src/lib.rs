//! T04 spike: font stack comparison. One stack per cargo feature (crossfont | swash | cosmic).

/// One rasterised glyph. `data` is R8 coverage (len w*h) when `color == false`, RGBA8 premultiplied (len w*h*4) when true.
#[derive(Debug, Clone)]
pub struct Glyph {
    pub ch: char,
    pub w: u16,
    pub h: u16,
    /// Bearing from the pen position (cell origin on the baseline) to the bitmap's left edge, px.
    pub left: i16,
    /// Bearing from the baseline up to the bitmap's top edge, px (positive = above baseline).
    pub top: i16,
    pub color: bool,
    pub data: Vec<u8>,
}

/// Integer cell metrics in physical px.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub cell_w: u16,
    pub cell_h: u16,
    pub ascent: i16,
    pub descent: i16,
}

/// Full metrics used by the bench (the frozen [`Metrics`] plus decorations).
#[derive(Debug, Clone, Copy)]
pub struct FontMetrics {
    pub cell_w: u16,
    pub cell_h: u16,
    pub ascent: i16,
    /// Positive distance from the baseline down to the bottom of the line box.
    pub descent: i16,
    /// Native underline offset (negative = below the baseline).
    pub underline_position: f32,
    pub underline_thickness: f32,
}

impl FontMetrics {
    pub fn to_metrics(self) -> Metrics {
        Metrics {
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            ascent: self.ascent,
            descent: self.descent,
        }
    }
}

/// One `metric` / `value` pair produced by a single `--once` run.
#[derive(Debug, Clone)]
pub struct BenchEntry {
    pub metric: String,
    pub value: String,
}

impl BenchEntry {
    pub fn new(metric: impl Into<String>, value: impl Into<String>) -> Self {
        Self { metric: metric.into(), value: value.into() }
    }
}

/// A glyph placed in an atlas cell, with the pen advance to use after the cell.
#[derive(Debug, Clone)]
pub struct AtlasGlyph {
    pub glyph: Glyph,
    pub advance: f32,
}

/// One atlas cell. More than one glyph means they are overlaid (combining marks).
#[derive(Debug, Clone, Default)]
pub struct AtlasCell {
    pub glyphs: Vec<AtlasGlyph>,
}

impl AtlasCell {
    pub fn single(glyph: Glyph, advance: f32) -> Self {
        Self { glyphs: vec![AtlasGlyph { glyph, advance }] }
    }

    pub fn advance(&self) -> f32 {
        self.glyphs.iter().map(|g| g.advance).sum()
    }
}

#[derive(Debug, Clone, Default)]
pub struct AtlasRow {
    pub cells: Vec<AtlasCell>,
}

/// Atlas geometry plus rows at one pixel size.
#[derive(Debug, Clone)]
pub struct Atlas {
    pub px: f32,
    pub ascent: f32,
    pub cell_h: f32,
    pub rows: Vec<AtlasRow>,
}

pub fn empty_glyph(ch: char) -> Glyph {
    Glyph { ch, w: 0, h: 0, left: 0, top: 0, color: false, data: Vec::new() }
}

pub fn fmt_f64(x: f64) -> String {
    if (x - x.round()).abs() < 1e-9 {
        format!("{}", x.round() as i64)
    } else {
        let s = format!("{x:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

pub mod png;
pub(crate) mod util;

#[cfg(feature = "crossfont")]
mod crossfont_stack;
#[cfg(feature = "swash")]
mod swash_stack;
#[cfg(feature = "cosmic")]
mod cosmic_stack;

pub fn stack_name() -> &'static str {
    #[cfg(feature = "crossfont")]
    {
        return "crossfont";
    }
    #[cfg(all(not(feature = "crossfont"), feature = "swash"))]
    {
        return "swash";
    }
    #[cfg(all(not(feature = "crossfont"), not(feature = "swash"), feature = "cosmic"))]
    {
        return "cosmic";
    }
    #[cfg(not(any(feature = "crossfont", feature = "swash", feature = "cosmic")))]
    {
        "none"
    }
}

/// Rasterise printable ASCII (0x20..=0x7E) of the primary monospace face at `px` physical pixels.
pub fn rasterize_ascii(px: f32) -> (Metrics, Vec<Glyph>) {
    #[cfg(feature = "crossfont")]
    {
        return crossfont_stack::rasterize_ascii(px);
    }
    #[cfg(all(not(feature = "crossfont"), feature = "swash"))]
    {
        return swash_stack::rasterize_ascii(px);
    }
    #[cfg(all(not(feature = "crossfont"), not(feature = "swash"), feature = "cosmic"))]
    {
        return cosmic_stack::rasterize_ascii(px);
    }
    #[cfg(not(any(feature = "crossfont", feature = "swash", feature = "cosmic")))]
    {
        let _ = px;
        panic!("enable one of the crossfont / swash / cosmic features")
    }
}

pub fn bench() -> Vec<BenchEntry> {
    #[cfg(feature = "crossfont")]
    {
        return crossfont_stack::bench();
    }
    #[cfg(all(not(feature = "crossfont"), feature = "swash"))]
    {
        return swash_stack::bench();
    }
    #[cfg(all(not(feature = "crossfont"), not(feature = "swash"), feature = "cosmic"))]
    {
        return cosmic_stack::bench();
    }
    #[cfg(not(any(feature = "crossfont", feature = "swash", feature = "cosmic")))]
    {
        panic!("enable one of the crossfont / swash / cosmic features")
    }
}

pub fn atlas(px: f32) -> Atlas {
    #[cfg(feature = "crossfont")]
    {
        return crossfont_stack::atlas(px);
    }
    #[cfg(all(not(feature = "crossfont"), feature = "swash"))]
    {
        return swash_stack::atlas(px);
    }
    #[cfg(all(not(feature = "crossfont"), not(feature = "swash"), feature = "cosmic"))]
    {
        return cosmic_stack::atlas(px);
    }
    #[cfg(not(any(feature = "crossfont", feature = "swash", feature = "cosmic")))]
    {
        let _ = px;
        panic!("enable one of the crossfont / swash / cosmic features")
    }
}

/// Fixed sample rows used by the atlas dumps.
pub(crate) const BOX_SAMPLE: &str = "─│┌┐└┘├┤┬┴┼═║╔╗╚╝█▀▄░▒▓\u{e0b0}\u{e0b1}\u{e0b2}\u{e0b3}";
pub(crate) const CJK_SAMPLE: &str = "中文字体測試";
pub(crate) const EMOJI_SAMPLE: &[char] = &['😀', '🚀', '❤', '👍'];

pub(crate) fn ascii_row(mut make: impl FnMut(char) -> Option<AtlasGlyph>, advance: f32) -> AtlasRow {
    let mut cells = Vec::with_capacity(95);
    for byte in 0x20u8..=0x7e {
        let ch = byte as char;
        let g = make(ch).unwrap_or(AtlasGlyph { glyph: empty_glyph(ch), advance });
        cells.push(AtlasCell { glyphs: vec![g] });
    }
    AtlasRow { cells }
}

pub(crate) fn simple_row(
    chars: impl IntoIterator<Item = char>,
    mut make: impl FnMut(char) -> Option<AtlasGlyph>,
    fallback_advance: f32,
) -> AtlasRow {
    let mut cells = Vec::new();
    for ch in chars {
        let g = make(ch).unwrap_or(AtlasGlyph { glyph: empty_glyph(ch), advance: fallback_advance });
        cells.push(AtlasCell { glyphs: vec![g] });
    }
    AtlasRow { cells }
}

#[allow(dead_code)]
pub(crate) fn is_emoji_char(ch: char) -> bool {
    let c = ch as u32;
    (0x1f000..=0x1faff).contains(&c)
        || (0x2600..=0x27bf).contains(&c)
        || (0x2b00..=0x2bff).contains(&c)
}
