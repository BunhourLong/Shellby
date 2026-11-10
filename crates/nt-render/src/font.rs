//! Font loading (fontdb discovery, swash raster/metrics), cell metrics and lazy fallback.
//! No wgpu types.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use fontdb::Style as DbStyle;
use fontdb::Weight as DbWeight;
use fontdb::{Database, Family, ID, Query, Stretch};
use swash::FontRef;
use swash::scale::image::{Content, Image};
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform};

use crate::FontConfig;
use crate::glyphs::Style;

/// Cell metrics in integer device pixels, all measured from the cell's top-left.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Metrics {
    /// Cell width.
    pub cell_w: u16,
    /// Cell height.
    pub cell_h: u16,
    /// Baseline row from the cell top.
    pub baseline: u16,
    /// Top row of the underline stroke.
    pub underline_y: u16,
    /// Top row of the strikeout stroke.
    pub strike_y: u16,
    /// Underline/strikeout/light-line thickness, ≥ 1.
    pub thickness: u16,
}

/// Font-unit-free inputs to `compute_metrics`, already scaled to px (f32).
/// `ascent`/`descent` are positive magnitudes; `underline_pos` is the distance of the
/// underline's centre below the baseline (positive = below).
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Raw {
    /// Px size (em).
    pub px: f32,
    /// Advance of '0' (or max ASCII advance if not monospace).
    pub advance: f32,
    /// Ascent, positive.
    pub ascent: f32,
    /// Descent, positive.
    pub descent: f32,
    /// Line gap.
    pub line_gap: f32,
    /// Underline centre below baseline, `None` if the font lacks it.
    pub underline_pos: Option<f32>,
    /// Underline thickness, `None` if the font lacks it.
    pub underline_thickness: Option<f32>,
    /// Strikeout centre above baseline, `None` if the font lacks it.
    pub strike_pos: Option<f32>,
    /// x-height, used when the strikeout metric is missing.
    pub x_height: f32,
}

#[allow(
    clippy::cast_sign_loss,
    reason = "the value is clamped to 1..=u16::MAX before the cast"
)]
fn round_positive(value: f32) -> u16 {
    value.round().clamp(1.0, f32::from(u16::MAX)) as u16
}

fn round_px(value: f32) -> i32 {
    value
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i32
}

/// Rounds raw px metrics to the integer cell metrics, per T07 "Metrics":
/// `cell_w = round(advance)`, `cell_h = round((ascent + descent + line_gap) × line_height)`,
/// extra line height split evenly above and below; underline fallback = descent / 2 below
/// the baseline, thickness fallback = max(1, round(px / 14)); strike fallback = `x_height` / 2.
/// Every result is clamped inside the cell.
#[allow(
    clippy::cast_sign_loss,
    reason = "baseline, underline_y and strike_y are clamped to non-negative rows first"
)]
pub fn compute_metrics(raw: &Raw, line_height: f32) -> Metrics {
    let cell_w = round_positive(raw.advance);
    let natural_h = raw.ascent + raw.descent + raw.line_gap;
    let cell_h = round_positive(natural_h * line_height);
    let cell_h_i = i32::from(cell_h);
    let baseline =
        round_px(raw.ascent + (f32::from(cell_h) - natural_h) / 2.0).clamp(0, cell_h_i) as u16;

    let thickness = round_positive(raw.underline_thickness.unwrap_or(raw.px / 14.0)).min(cell_h);

    let underline_centre =
        i32::from(baseline) + round_px(raw.underline_pos.unwrap_or(raw.descent / 2.0));
    let underline_y = (underline_centre - i32::from(thickness / 2))
        .clamp(0, cell_h_i - i32::from(thickness)) as u16;

    let strike_centre =
        i32::from(baseline) - round_px(raw.strike_pos.unwrap_or(raw.x_height / 2.0));
    let strike_y =
        (strike_centre - i32::from(thickness / 2)).clamp(0, cell_h_i - i32::from(thickness)) as u16;

    Metrics {
        cell_w,
        cell_h,
        baseline,
        underline_y,
        strike_y,
        thickness,
    }
}

/// Points to px: `pt × dpi / 72`, dpi = 96 × scale on Linux, 72 × scale on macOS.
pub fn px_size(size_pt: f32, scale: f64) -> f32 {
    #[cfg(target_os = "macos")]
    let dpi = 72.0 * scale;
    #[cfg(not(target_os = "macos"))]
    let dpi = 96.0 * scale;
    (f64::from(size_pt) * dpi / 72.0) as f32
}

/// A rasterised glyph in the caller's buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Raster {
    /// Bitmap width (0 = blank glyph, e.g. a space).
    pub w: u16,
    /// Bitmap height.
    pub h: u16,
    /// Left bearing: bitmap left minus pen x.
    pub left: i16,
    /// Bitmap top above the baseline (positive = up).
    pub top: i16,
    /// True = RGBA8 premultiplied (4 B/px), false = R8 coverage.
    pub color: bool,
}

#[derive(Clone)]
struct FontData(Arc<[u8]>);

impl std::fmt::Debug for FontData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FontData").field(&self.0.len()).finish()
    }
}

impl std::ops::Deref for FontData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Debug)]
struct Face {
    data: FontData,
    index: u32,
    synthetic_bold: bool,
    synthetic_italic: bool,
}

struct RenderState {
    ctx: ScaleContext,
    image: Image,
}

impl std::fmt::Debug for RenderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderState").finish_non_exhaustive()
    }
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            ctx: ScaleContext::new(),
            image: Image::new(),
        }
    }
}

/// The primary face (regular/bold/italic/bold-italic) plus lazily discovered fallbacks.
#[derive(Debug)]
pub struct Font {
    faces: [Face; 4],
    db: Option<Database>,
    size_pt: f32,
    line_height: f32,
    scale: f64,
    px: f32,
    metrics: Metrics,
    fallback_chars: HashMap<char, Option<ID>>,
    fallback_faces: HashMap<ID, Option<(FontData, u32)>>,
    render: RenderState,
}

impl Font {
    /// Loads `cfg.family` (or the default monospace) from the system at `scale`.
    /// Only the primary family is touched; fallback discovery is lazy.
    pub fn load(cfg: &FontConfig, scale: f64) -> Result<Self, String> {
        // shortcut: full system scan (~6 ms) on every load; T08 caches the primary face's
        // path and loads it directly, scanning only for fallback after first present.
        let mut db = Database::new();
        db.load_system_fonts();
        let family = match &cfg.family {
            Some(name) => name.clone(),
            None => default_family(&db).ok_or_else(|| "no monospace font installed".to_string())?,
        };
        let families = [Family::Name(family.as_str())];
        let regular_id = db
            .query(&Query {
                families: &families,
                weight: DbWeight::NORMAL,
                stretch: Stretch::Normal,
                style: DbStyle::Normal,
            })
            .ok_or_else(|| format!("font family not found: {family}"))?;
        let (regular_data, regular_index) = face_data(&db, regular_id)
            .ok_or_else(|| format!("could not read font data for {family}"))?;
        let regular = Face {
            data: regular_data.clone(),
            index: regular_index,
            synthetic_bold: false,
            synthetic_italic: false,
        };
        let synthetic = |synthetic_bold, synthetic_italic| Face {
            data: regular_data.clone(),
            index: regular_index,
            synthetic_bold,
            synthetic_italic,
        };
        let bold = variant(&db, &families, DbWeight::BOLD, DbStyle::Normal, true, false)
            .unwrap_or_else(|| synthetic(true, false));
        let italic = variant(
            &db,
            &families,
            DbWeight::NORMAL,
            DbStyle::Italic,
            false,
            true,
        )
        .unwrap_or_else(|| synthetic(false, true));
        let bold_italic = variant(&db, &families, DbWeight::BOLD, DbStyle::Italic, true, true)
            .unwrap_or_else(|| synthetic(true, true));
        Ok(Self::build(
            [regular, bold, italic, bold_italic],
            Some(db),
            cfg,
            scale,
        ))
    }

    /// Loads the regular face from a font file (tests, benches); styles fall back to it.
    pub fn from_file(path: &Path, cfg: &FontConfig, scale: f64) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let data = FontData(Arc::from(bytes.as_slice()));
        FontRef::from_index(&data, 0).ok_or_else(|| format!("not a font: {}", path.display()))?;
        let face = |synthetic_bold, synthetic_italic| Face {
            data: data.clone(),
            index: 0,
            synthetic_bold,
            synthetic_italic,
        };
        Ok(Self::build(
            [
                face(false, false),
                face(true, false),
                face(false, true),
                face(true, true),
            ],
            None,
            cfg,
            scale,
        ))
    }

    fn build(faces: [Face; 4], db: Option<Database>, cfg: &FontConfig, scale: f64) -> Self {
        let mut font = Self {
            faces,
            db,
            size_pt: cfg.size_pt,
            line_height: cfg.line_height,
            scale,
            px: 0.0,
            metrics: Metrics::default(),
            fallback_chars: HashMap::new(),
            fallback_faces: HashMap::new(),
            render: RenderState::default(),
        };
        font.recompute_metrics();
        font
    }

    fn recompute_metrics(&mut self) {
        let px = px_size(self.size_pt, self.scale).max(1.0);
        self.px = px;
        let face = &self.faces[0];
        let Some(font) = FontRef::from_index(&face.data, face.index as usize) else {
            return;
        };
        let metrics = font.metrics(&[]).scale(px);
        let glyphs = font.glyph_metrics(&[]).scale(px);
        let charmap = font.charmap();
        let advance = if metrics.is_monospace {
            glyphs.advance_width(charmap.map('0'))
        } else {
            let mut max = 0.0_f32;
            for byte in 0x21_u8..=0x7e {
                let width = glyphs.advance_width(charmap.map(char::from(byte)));
                if width > max {
                    max = width;
                }
            }
            max
        };
        let raw = Raw {
            px,
            advance,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.leading,
            underline_pos: (metrics.underline_offset != 0.0).then_some(-metrics.underline_offset),
            underline_thickness: (metrics.stroke_size != 0.0).then_some(metrics.stroke_size),
            strike_pos: (metrics.strikeout_offset != 0.0).then_some(metrics.strikeout_offset),
            x_height: metrics.x_height,
        };
        self.metrics = compute_metrics(&raw, self.line_height);
    }

    /// Changes size/line height/scale without reloading faces; metrics recomputed.
    pub fn rescale(&mut self, cfg: &FontConfig, scale: f64) {
        self.size_pt = cfg.size_pt;
        self.line_height = cfg.line_height;
        self.scale = scale;
        self.recompute_metrics();
    }

    /// Current cell metrics.
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Rasterises `ch` in `style` into `out` (cleared first). `None` = no face has the char
    /// (the caller draws tofu); `Some(w == 0)` = blank glyph. The first char the primary
    /// face lacks triggers the system fallback scan; results are cached per char.
    pub fn rasterize(&mut self, ch: char, style: Style, out: &mut Vec<u8>) -> Option<Raster> {
        out.clear();
        if ch == ' ' {
            return Some(Raster {
                w: 0,
                h: 0,
                left: 0,
                top: 0,
                color: false,
            });
        }
        let face = &self.faces[style as usize];
        let covered = FontRef::from_index(&face.data, face.index as usize)
            .is_some_and(|font| font.charmap().map(ch) != 0);
        let (data, index, bold, italic) = if covered {
            (
                face.data.clone(),
                face.index,
                face.synthetic_bold,
                face.synthetic_italic,
            )
        } else {
            let (data, index) = self.fallback_face(ch)?;
            (data, index, false, false)
        };
        self.render_face(&data, index, ch, out, bold, italic)
    }

    fn render_face(
        &mut self,
        data: &[u8],
        index: u32,
        ch: char,
        out: &mut Vec<u8>,
        bold: bool,
        italic: bool,
    ) -> Option<Raster> {
        let font = FontRef::from_index(data, index as usize)?;
        let gid = font.charmap().map(ch);
        if gid == 0 {
            return None;
        }
        let px = self.px;
        let state = &mut self.render;
        let rendered = {
            let mut scaler = state.ctx.builder(font).size(px).hint(true).build();
            let mut render = Render::new(&[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ]);
            render.format(Format::Alpha);
            if bold {
                render.embolden((px / 24.0).max(0.5));
            }
            if italic {
                render.transform(Some(Transform::skew(
                    Angle::from_degrees(14.0),
                    Angle::ZERO,
                )));
            }
            // swash resizes the reused buffer without zeroing it and accumulates coverage.
            state.image.data.clear();
            render.render_into(&mut scaler, gid, &mut state.image)
        };
        if !rendered {
            return None;
        }

        let image = &state.image;
        let w = image.placement.width.min(u32::from(u16::MAX)) as u16;
        let h = image.placement.height.min(u32::from(u16::MAX)) as u16;
        let left = image
            .placement
            .left
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let top = image
            .placement
            .top
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let color = image.content == Content::Color;
        if w == 0 || h == 0 {
            out.clear();
            return Some(Raster {
                w: 0,
                h: 0,
                left,
                top,
                color,
            });
        }
        match image.content {
            Content::Mask => out.extend_from_slice(&image.data),
            Content::Color => {
                out.extend_from_slice(&image.data);
                premultiply(out);
            }
            Content::SubpixelMask => {
                out.reserve(image.data.len() / 4);
                for pixel in image.data.as_chunks::<4>().0 {
                    let sum = u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2]);
                    out.push((sum / 3) as u8);
                }
            }
        }
        Some(Raster {
            w,
            h,
            left,
            top,
            color,
        })
    }

    fn fallback_face(&mut self, ch: char) -> Option<(FontData, u32)> {
        if let Some(entry) = self.fallback_chars.get(&ch) {
            return entry.and_then(|id| self.fallback_faces.get(&id).cloned().flatten());
        }
        if self.db.is_none() {
            // shortcut: the system scan is ~5.4 ms and only runs on the first primary-face
            // miss here; T08 moves it off the first-frame path with a cached primary path.
            let mut db = Database::new();
            db.load_system_fonts();
            self.db = Some(db);
        }
        let mut found = None;
        {
            let db = self.db.as_ref()?;
            'search: for mono_only in [true, false] {
                for face in db.faces() {
                    if mono_only && !face.monospaced {
                        continue;
                    }
                    if face_has(db, face.id, ch) {
                        found = Some(face.id);
                        break 'search;
                    }
                }
            }
        }
        let result = found.and_then(|id| {
            if let Some(cached) = self.fallback_faces.get(&id) {
                return cached.clone();
            }
            let data = self.db.as_ref().and_then(|db| {
                let index = db.face(id)?.index;
                db.with_face_data(id, |bytes, _| FontData(Arc::from(bytes)))
                    .map(|data| (data, index))
            });
            self.fallback_faces.insert(id, data.clone());
            data
        });
        self.fallback_chars.insert(ch, found);
        result
    }
}

fn default_family(db: &Database) -> Option<String> {
    const PREFERRED: [&str; 7] = [
        "JetBrains Mono",
        "DejaVu Sans Mono",
        "Noto Sans Mono",
        "Liberation Mono",
        "Menlo",
        "SF Mono",
        "Monaco",
    ];
    for name in PREFERRED {
        let families = [Family::Name(name)];
        let query = Query {
            families: &families,
            weight: DbWeight::NORMAL,
            stretch: Stretch::Normal,
            style: DbStyle::Normal,
        };
        if db.query(&query).is_some() {
            return Some(name.to_string());
        }
    }
    db.faces()
        .find(|face| face.monospaced)
        .and_then(|face| face.families.first())
        .map(|family| family.0.clone())
}

fn variant(
    db: &Database,
    families: &[Family<'_>],
    weight: DbWeight,
    style: DbStyle,
    bold: bool,
    italic: bool,
) -> Option<Face> {
    let id = db.query(&Query {
        families,
        weight,
        stretch: Stretch::Normal,
        style,
    })?;
    let info = db.face(id)?;
    let weight_ok = !bold || info.weight.0 >= 600;
    let style_ok = !italic || matches!(info.style, DbStyle::Italic | DbStyle::Oblique);
    if !weight_ok || !style_ok {
        return None;
    }
    let (data, index) = face_data(db, id)?;
    Some(Face {
        data,
        index,
        synthetic_bold: false,
        synthetic_italic: false,
    })
}

fn face_data(db: &Database, id: ID) -> Option<(FontData, u32)> {
    let index = db.face(id)?.index;
    db.with_face_data(id, |bytes, _| FontData(Arc::from(bytes)))
        .map(|data| (data, index))
}

fn face_has(db: &Database, id: ID, ch: char) -> bool {
    db.with_face_data(id, |bytes, index| {
        FontRef::from_index(bytes, index as usize).is_some_and(|font| font.charmap().map(ch) != 0)
    })
    .unwrap_or(false)
}

fn premultiply(data: &mut [u8]) {
    for pixel in data.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/JetBrainsMono-Regular.ttf")
    }

    #[test]
    fn rasterize_is_independent_of_the_previous_glyph() {
        // Regression: swash's render_into resizes the reused Image without zeroing it and
        // accumulates coverage, so each glyph carried the previous glyph's pixels.
        let mut font = Font::from_file(&fixture(), &cfg(), 1.0).unwrap();
        let mut first = Vec::new();
        font.rasterize('e', Style::Regular, &mut first).unwrap();
        let mut scratch = Vec::new();
        font.rasterize('W', Style::Regular, &mut scratch).unwrap();
        let mut again = Vec::new();
        font.rasterize('e', Style::Regular, &mut again).unwrap();
        assert_eq!(first, again);
    }

    fn cfg() -> FontConfig {
        FontConfig {
            family: None,
            size_pt: 12.0,
            line_height: 1.0,
        }
    }

    #[test]
    fn metrics_rounding_and_even_line_height_split() {
        let raw = Raw {
            px: 14.0,
            advance: 8.4,
            ascent: 10.0,
            descent: 3.0,
            line_gap: 0.0,
            underline_pos: Some(2.0),
            underline_thickness: Some(1.0),
            strike_pos: Some(4.0),
            x_height: 6.0,
        };
        let m = compute_metrics(&raw, 1.2);
        assert_eq!(m.cell_w, 8);
        assert_eq!(m.cell_h, 16);
        assert_eq!(m.baseline, 12);
        assert_eq!(m.thickness, 1);
        assert_eq!(m.underline_y, 14);
        assert_eq!(m.strike_y, 8);
    }

    #[test]
    fn metrics_missing_underline_and_strike_fallbacks() {
        let raw = Raw {
            px: 14.0,
            advance: 7.0,
            ascent: 10.0,
            descent: 4.0,
            line_gap: 0.0,
            underline_pos: None,
            underline_thickness: None,
            strike_pos: None,
            x_height: 6.0,
        };
        let m = compute_metrics(&raw, 1.0);
        assert_eq!(m.cell_h, 14);
        assert_eq!(m.baseline, 10);
        assert_eq!(m.thickness, 1);
        assert_eq!(m.underline_y, 12);
        assert_eq!(m.strike_y, 7);
    }

    #[test]
    fn metrics_clamp_inside_cell() {
        let raw = Raw {
            px: 14.0,
            advance: 7.0,
            ascent: 10.0,
            descent: 2.0,
            line_gap: 0.0,
            underline_pos: Some(50.0),
            underline_thickness: Some(40.0),
            strike_pos: Some(50.0),
            x_height: 6.0,
        };
        let m = compute_metrics(&raw, 1.0);
        assert!(m.thickness <= m.cell_h);
        assert!(u32::from(m.underline_y) + u32::from(m.thickness) <= u32::from(m.cell_h));
        assert!(u32::from(m.strike_y) + u32::from(m.thickness) <= u32::from(m.cell_h));
    }

    #[test]
    fn px_size_per_platform() {
        #[cfg(target_os = "macos")]
        let expected = 12.0;
        #[cfg(not(target_os = "macos"))]
        let expected = 16.0;
        assert!((px_size(12.0, 1.0) - expected).abs() < 1e-4);
        assert!((px_size(12.0, 2.0) - expected * 2.0).abs() < 1e-4);
    }

    #[test]
    fn from_file_metrics_and_rasters() {
        let mut font = Font::from_file(&fixture(), &cfg(), 1.0).expect("fixture font");
        let m = font.metrics();
        assert!(m.cell_w > 0);
        assert!(m.cell_h > m.cell_w);
        let mut out = Vec::new();
        let raster = font
            .rasterize('A', Style::Regular, &mut out)
            .expect("A raster");
        assert!(!raster.color);
        assert!(raster.w > 0 && raster.h > 0);
        assert_eq!(out.len(), usize::from(raster.w) * usize::from(raster.h));
        let blank = font
            .rasterize(' ', Style::Regular, &mut out)
            .expect("space");
        assert_eq!(blank.w, 0);
        assert_eq!(blank.h, 0);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn from_file_synthesises_bold_and_italic() {
        let mut font = Font::from_file(&fixture(), &cfg(), 1.0).expect("fixture font");
        assert!(font.faces[1].synthetic_bold);
        assert!(font.faces[2].synthetic_italic);
        assert!(font.faces[3].synthetic_bold && font.faces[3].synthetic_italic);
        let mut out = Vec::new();
        assert!(font.rasterize('A', Style::Bold, &mut out).is_some());
        assert!(font.rasterize('A', Style::Italic, &mut out).is_some());
        assert!(font.rasterize('A', Style::BoldItalic, &mut out).is_some());
    }

    #[test]
    fn rescale_recomputes_metrics() {
        let mut font = Font::from_file(&fixture(), &cfg(), 1.0).expect("fixture font");
        let before = font.metrics();
        let bigger = FontConfig {
            family: None,
            size_pt: 24.0,
            line_height: 1.0,
        };
        font.rescale(&bigger, 1.0);
        let after = font.metrics();
        assert!(after.cell_w > before.cell_w);
        assert!(after.cell_h > before.cell_h);
    }
}
