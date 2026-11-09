//! crossfont stack (FreeType + fontconfig).

use std::time::Instant;

use crossfont::{
    BitmapBuffer, FontDesc, FontKey, GlyphKey, Rasterize, RasterizedGlyph, Rasterizer, Size, Slant,
    Style, Weight,
};

use crate::{
    Atlas, AtlasCell, AtlasGlyph, BOX_SAMPLE, BenchEntry, CJK_SAMPLE, EMOJI_SAMPLE, FontMetrics,
    Glyph, ascii_row, empty_glyph, fmt_f64, simple_row, util,
};

const FAMILY: &str = "JetBrains Mono";

fn desc(weight: Weight, slant: Slant) -> FontDesc {
    FontDesc::new(FAMILY, Style::Description { weight, slant })
}

fn metrics_of(r: &Rasterizer, key: FontKey, size: Size) -> FontMetrics {
    let m = r.metrics(key, size).expect("crossfont metrics");
    let ascent = (m.line_height + m.descent as f64).round() as i16;
    let descent = (-m.descent).round() as i16;
    FontMetrics {
        cell_w: m.average_advance.round().max(1.0) as u16,
        cell_h: m.line_height.round().max(1.0) as u16,
        ascent,
        descent,
        underline_position: m.underline_position,
        underline_thickness: m.underline_thickness,
    }
}

fn convert(g: RasterizedGlyph) -> (Glyph, f32) {
    let w = g.width.max(0) as u16;
    let h = g.height.max(0) as u16;
    let (color, data) = match g.buffer {
        BitmapBuffer::Rgb(buf) => (false, buf.chunks(3).map(|c| c[0]).collect()),
        BitmapBuffer::Rgba(buf) => (true, buf),
    };
    (
        Glyph {
            ch: g.character,
            w,
            h,
            left: g.left.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            top: g.top.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            color,
            data,
        },
        g.advance.0 as f32,
    )
}

fn raster_one(r: &mut Rasterizer, key: FontKey, size: Size, ch: char) -> Option<(Glyph, f32)> {
    match r.get_glyph(GlyphKey { character: ch, font_key: key, size }) {
        Ok(g) => Some(convert(g)),
        Err(_) => None,
    }
}

fn raster_95(r: &mut Rasterizer, key: FontKey, size: Size) -> Vec<Glyph> {
    (0x20u8..=0x7e)
        .map(|byte| {
            let ch = byte as char;
            raster_one(r, key, size, ch).map(|(g, _)| g).unwrap_or_else(|| empty_glyph(ch))
        })
        .collect()
}

fn elapsed_us(start: Instant) -> String {
    fmt_f64(start.elapsed().as_secs_f64() * 1e6)
}

fn face_kind(pattern: &str, needle: &str) -> &'static str {
    match util::fc_match_file(pattern) {
        Some(path) if path.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()) => "real",
        Some(_) => "synthetic",
        None => "unknown",
    }
}

pub fn rasterize_ascii(px: f32) -> (crate::Metrics, Vec<Glyph>) {
    let size = Size::from_px(px.max(1.0));
    let mut r = Rasterizer::new().expect("crossfont init");
    let key = r.load_font(&desc(Weight::Normal, Slant::Normal), size).expect("load JetBrains Mono");
    let fm = metrics_of(&r, key, size);
    (fm.to_metrics(), raster_95(&mut r, key, size))
}

pub fn bench() -> Vec<BenchEntry> {
    let mut out = Vec::new();
    let px = 18.0f32;
    let size = Size::from_px(px);

    let start = Instant::now();
    let mut r = Rasterizer::new().expect("crossfont init");
    out.push(BenchEntry::new("a_us", elapsed_us(start)));

    let start = Instant::now();
    let regular = r.load_font(&desc(Weight::Normal, Slant::Normal), size).expect("load regular");
    out.push(BenchEntry::new("b_us", elapsed_us(start)));
    out.push(BenchEntry::new(
        "a_b_note",
        "crossfont has no separate discovery API; family discovery is inside b_us",
    ));

    out.push(BenchEntry::new(
        "primary_font_family",
        util::fc_match_family(FAMILY).unwrap_or_else(|| FAMILY.to_string()),
    ));
    out.push(BenchEntry::new(
        "primary_font_path",
        util::fc_match_file(FAMILY).unwrap_or_else(|| "unknown".to_string()),
    ));

    let start = Instant::now();
    let glyphs = raster_95(&mut r, regular, size);
    out.push(BenchEntry::new("c_regular_us", elapsed_us(start)));
    out.push(BenchEntry::new("c_regular_glyphs", glyphs.len().to_string()));

    match r.load_font(&desc(Weight::Bold, Slant::Normal), size) {
        Ok(key) => {
            out.push(BenchEntry::new(
                "c_bold_face",
                face_kind("JetBrains Mono:weight=bold", "bold"),
            ));
            let start = Instant::now();
            let _ = raster_95(&mut r, key, size);
            out.push(BenchEntry::new("c_bold_us", elapsed_us(start)));
        }
        Err(_) => {
            out.push(BenchEntry::new("c_bold_face", "none"));
            out.push(BenchEntry::new("c_bold_us", "none"));
        }
    }

    match r.load_font(&desc(Weight::Normal, Slant::Italic), size) {
        Ok(key) => {
            out.push(BenchEntry::new(
                "c_italic_face",
                face_kind("JetBrains Mono:slant=italic", "italic"),
            ));
            let start = Instant::now();
            let _ = raster_95(&mut r, key, size);
            out.push(BenchEntry::new("c_italic_us", elapsed_us(start)));
        }
        Err(_) => {
            out.push(BenchEntry::new("c_italic_face", "none"));
            out.push(BenchEntry::new("c_italic_us", "none"));
        }
    }

    for ch in ['中', '😀', 'ﷺ', '⠿'] {
        let start = Instant::now();
        let ok = raster_one(&mut r, regular, size, ch).is_some();
        let us = elapsed_us(start);
        out.push(BenchEntry::new(format!("d_{ch}_us"), us));
        out.push(BenchEntry::new(
            format!("d_{ch}_font"),
            util::fc_match_family_for_char(ch).unwrap_or_else(|| {
                if ok { "fontconfig-auto".to_string() } else { "missing".to_string() }
            }),
        ));
    }

    let fm = metrics_of(&r, regular, size);
    out.push(BenchEntry::new("e_cell_w", fm.cell_w.to_string()));
    out.push(BenchEntry::new("e_cell_h", fm.cell_h.to_string()));
    out.push(BenchEntry::new("e_ascent", fm.ascent.to_string()));
    out.push(BenchEntry::new("e_descent", fm.descent.to_string()));
    out.push(BenchEntry::new("e_underline_pos", fmt_f64(fm.underline_position as f64)));
    out.push(BenchEntry::new(
        "e_underline_thickness",
        fmt_f64(fm.underline_thickness as f64),
    ));

    out.push(BenchEntry::new(
        "f_vmhwm_bytes",
        util::vmhwm_bytes().map_or_else(|| "unknown".to_string(), |v| v.to_string()),
    ));
    out
}

fn atlas_glyph(
    r: &mut Rasterizer,
    key: FontKey,
    size: Size,
    ch: char,
    fallback_advance: f32,
) -> AtlasGlyph {
    match raster_one(r, key, size, ch) {
        Some((g, adv)) => {
            let advance = if adv > 0.0 { adv } else { g.w as f32 };
            AtlasGlyph { glyph: g, advance }
        }
        None => AtlasGlyph { glyph: empty_glyph(ch), advance: fallback_advance },
    }
}

pub fn atlas(px: f32) -> Atlas {
    let px = px.max(1.0);
    let size = Size::from_px(px);
    let mut r = Rasterizer::new().expect("crossfont init");
    let regular = r.load_font(&desc(Weight::Normal, Slant::Normal), size).expect("load regular");
    let bold = r.load_font(&desc(Weight::Bold, Slant::Normal), size).ok();
    let italic = r.load_font(&desc(Weight::Normal, Slant::Italic), size).ok();
    let fm = metrics_of(&r, regular, size);
    let cw = fm.cell_w as f32;

    let mut rows = Vec::new();
    for key in [regular, bold.unwrap_or(regular), italic.unwrap_or(regular)] {
        rows.push(ascii_row(
            |ch| Some(atlas_glyph(&mut r, key, size, ch, cw)),
            cw,
        ));
    }

    rows.push(simple_row(
        BOX_SAMPLE.chars(),
        |ch| Some(atlas_glyph(&mut r, regular, size, ch, cw)),
        cw,
    ));
    rows.push(simple_row(
        CJK_SAMPLE.chars(),
        |ch| Some(atlas_glyph(&mut r, regular, size, ch, cw)),
        cw,
    ));

    let mut combining = AtlasCell {
        glyphs: vec![atlas_glyph(&mut r, regular, size, 'e', cw)],
    };
    if let Some((mark, _)) = raster_one(&mut r, regular, size, '\u{0301}') {
        combining.glyphs.push(AtlasGlyph { glyph: mark, advance: 0.0 });
    }

    rows.push(simple_row(
        EMOJI_SAMPLE.iter().copied(),
        |ch| Some(atlas_glyph(&mut r, regular, size, ch, cw)),
        cw,
    ));

    rows.insert(5, crate::AtlasRow { cells: vec![combining] });

    Atlas { px, ascent: fm.ascent as f32, cell_h: fm.cell_h as f32, rows }
}
