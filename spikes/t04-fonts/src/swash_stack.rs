//! swash + fontdb stack.

use std::time::Instant;

use fontdb::{Database, Family, ID, Query, Stretch, Style as DbStyle, Weight as DbWeight};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Scaler, Source, StrikeWith};
use swash::zeno::Format;
use swash::FontRef;

use crate::{
    Atlas, AtlasCell, AtlasGlyph, BOX_SAMPLE, BenchEntry, CJK_SAMPLE, EMOJI_SAMPLE, FontMetrics,
    Glyph, ascii_row, empty_glyph, fmt_f64, is_emoji_char, simple_row, util,
};

const FAMILY: &str = "JetBrains Mono";

fn query_face(db: &Database, weight: DbWeight, style: DbStyle) -> Option<ID> {
    db.query(&Query {
        families: &[Family::Name(FAMILY)],
        weight,
        stretch: Stretch::Normal,
        style,
    })
}

fn face_data(db: &Database, id: ID) -> Option<(Vec<u8>, usize)> {
    let index = db.face(id)?.index as usize;
    let bytes = db.with_face_data(id, |data, _| data.to_vec())?;
    Some((bytes, index))
}

fn face_family(db: &Database, id: ID) -> String {
    db.face(id)
        .and_then(|f| f.families.first().map(|f| f.0.clone()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn face_path(db: &Database, id: ID) -> Option<String> {
    db.face(id).and_then(|f| util::source_path(&f.source))
}

fn premultiply(mut data: Vec<u8>) -> Vec<u8> {
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        for channel in &mut px[..3] {
            *channel = ((*channel as u32 * a + 127) / 255) as u8;
        }
    }
    data
}

fn render_glyph(
    font: &FontRef,
    scaler: &mut Scaler,
    px: f32,
    ch: char,
) -> Option<(Glyph, f32)> {
    let gid = font.charmap().map(ch);
    if gid == 0 && ch != ' ' {
        return None;
    }

    let mut render = Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::Outline,
    ]);
    render.format(Format::Alpha);
    let image = render.render(scaler, gid)?;

    let w = image.placement.width as u16;
    let h = image.placement.height as u16;
    let (color, data) = match image.content {
        Content::Mask => (false, image.data),
        Content::Color => (true, premultiply(image.data)),
        Content::SubpixelMask => {
            let mut mask = Vec::with_capacity(w as usize * h as usize);
            for px in image.data.chunks_exact(4) {
                mask.push(((px[0] as u32 + px[1] as u32 + px[2] as u32) / 3) as u8);
            }
            (false, mask)
        }
    };
    let advance = font.glyph_metrics(&[]).scale(px).advance_width(gid);

    Some((
        Glyph {
            ch,
            w,
            h,
            left: image.placement.left.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            top: image.placement.top.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            color,
            data,
        },
        advance,
    ))
}

fn metrics_of(font: &FontRef, px: f32) -> FontMetrics {
    let m = font.metrics(&[]).scale(px);
    let advance = font.glyph_metrics(&[]).scale(px).advance_width(font.charmap().map('0'));
    FontMetrics {
        cell_w: advance.round().max(1.0) as u16,
        cell_h: (m.ascent + m.descent + m.leading).round().max(1.0) as u16,
        ascent: m.ascent.round() as i16,
        descent: m.descent.round() as i16,
        underline_position: m.underline_offset,
        underline_thickness: m.stroke_size,
    }
}

fn covers(db: &Database, id: ID, ch: char) -> bool {
    db.with_face_data(id, |data, index| {
        FontRef::from_index(data, index as usize)
            .map(|font| font.charmap().map(ch) != 0)
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

fn candidates(db: &Database, primary: ID, ch: char) -> Vec<ID> {
    let mut out = Vec::new();
    if covers(db, primary, ch) {
        out.push(primary);
    }
    let prefer_color = is_emoji_char(ch);
    let mut scored: Vec<(u8, ID)> = Vec::new();
    for face in db.faces() {
        if face.id == primary || !covers(db, face.id, ch) {
            continue;
        }
        let name = face.families.first().map(|f| f.0.as_str()).unwrap_or("");
        let post = face.post_script_name.as_str();
        let color = name.contains("Color Emoji") || post.contains("Color Emoji");
        let emoji = name.contains("Emoji") || post.contains("Emoji");
        let score = if prefer_color {
            if color { 0 } else if emoji { 1 } else { 2 }
        } else if emoji {
            3
        } else {
            0
        };
        scored.push((score, face.id));
    }
    scored.sort_by_key(|(score, _)| *score);
    out.extend(scored.into_iter().map(|(_, id)| id));
    out
}

fn render_face(db: &Database, id: ID, px: f32, ch: char) -> Option<(Glyph, f32)> {
    let (bytes, index) = face_data(db, id)?;
    let font = FontRef::from_index(&bytes, index)?;
    let mut context = ScaleContext::new();
    let mut scaler = context.builder(font).size(px).hint(true).build();
    render_glyph(&font, &mut scaler, px, ch)
}

fn raster_any(db: &Database, primary: ID, px: f32, ch: char) -> Option<AtlasGlyph> {
    for id in candidates(db, primary, ch) {
        if let Some((glyph, advance)) = render_face(db, id, px, ch) {
            if glyph.w == 0 && glyph.h == 0 && ch != ' ' {
                continue;
            }
            let advance = if advance > 0.0 { advance } else { glyph.w as f32 };
            return Some(AtlasGlyph { glyph, advance });
        }
    }
    None
}

fn raster_95(font: &FontRef, scaler: &mut Scaler, px: f32) -> Vec<Glyph> {
    (0x20u8..=0x7e)
        .map(|byte| {
            let ch = byte as char;
            render_glyph(font, scaler, px, ch).map(|(g, _)| g).unwrap_or_else(|| empty_glyph(ch))
        })
        .collect()
}

fn us(start: Instant) -> String {
    fmt_f64(start.elapsed().as_secs_f64() * 1e6)
}

fn real_face(db: &Database, weight: DbWeight, style: DbStyle, bold: bool, italic: bool) -> Option<ID> {
    let id = query_face(db, weight, style)?;
    let face = db.face(id)?;
    let weight_ok = !bold || face.weight.0 >= 600;
    let style_ok = !italic || matches!(face.style, DbStyle::Italic | DbStyle::Oblique);
    (weight_ok && style_ok).then_some(id)
}

pub fn rasterize_ascii(px: f32) -> (crate::Metrics, Vec<Glyph>) {
    let px = px.max(1.0);
    let mut db = Database::new();
    db.load_system_fonts();
    let id = query_face(&db, DbWeight::NORMAL, DbStyle::Normal).expect("JetBrains Mono Regular");
    let (bytes, index) = face_data(&db, id).expect("font bytes");
    let font = FontRef::from_index(&bytes, index).expect("font parse");
    let mut context = ScaleContext::new();
    let mut scaler = context.builder(font).size(px).hint(true).build();
    (metrics_of(&font, px).to_metrics(), raster_95(&font, &mut scaler, px))
}

pub fn bench() -> Vec<BenchEntry> {
    let mut out = Vec::new();
    let px = 18.0f32;

    let start = Instant::now();
    let mut db = Database::new();
    db.load_system_fonts();
    let regular = query_face(&db, DbWeight::NORMAL, DbStyle::Normal).expect("JetBrains Mono Regular");
    let a_scan = us(start);
    out.push(BenchEntry::new("a_us", a_scan.clone()));
    out.push(BenchEntry::new("a_scan_us", a_scan));

    let cached_path = util::fc_match_file("JetBrains Mono:style=Regular");
    let start = Instant::now();
    let path_ok = cached_path
        .as_deref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| FontRef::from_index(&bytes, 0).map(|_| ()))
        .is_some();
    out.push(BenchEntry::new("a_path_us", us(start)));
    out.push(BenchEntry::new("a_path_ok", path_ok.to_string()));

    out.push(BenchEntry::new("primary_font_family", face_family(&db, regular)));
    out.push(BenchEntry::new(
        "primary_font_path",
        face_path(&db, regular)
            .or(cached_path)
            .unwrap_or_else(|| "unknown".to_string()),
    ));

    let start = Instant::now();
    let (bytes, index) = face_data(&db, regular).expect("regular bytes");
    let regular_font = FontRef::from_index(&bytes, index).expect("regular parse");
    let mut context = ScaleContext::new();
    let _ = context.builder(regular_font).size(px).hint(true).build();
    out.push(BenchEntry::new("b_us", us(start)));

    let mut context = ScaleContext::new();
    let mut scaler = context.builder(regular_font).size(px).hint(true).build();
    let start = Instant::now();
    let glyphs = raster_95(&regular_font, &mut scaler, px);
    out.push(BenchEntry::new("c_regular_us", us(start)));
    out.push(BenchEntry::new("c_regular_glyphs", glyphs.len().to_string()));
    let fm = metrics_of(&regular_font, px);
    drop(scaler);

    let bold_id = real_face(&db, DbWeight::BOLD, DbStyle::Normal, true, false);
    match bold_id.and_then(|id| face_data(&db, id).map(|data| (id, data))) {
        Some((id, (bold_bytes, bold_index))) => {
            out.push(BenchEntry::new("c_bold_face", face_family(&db, id)));
            let font = FontRef::from_index(&bold_bytes, bold_index).expect("bold parse");
            let mut context = ScaleContext::new();
            let mut scaler = context.builder(font).size(px).hint(true).build();
            let start = Instant::now();
            let _ = raster_95(&font, &mut scaler, px);
            out.push(BenchEntry::new("c_bold_us", us(start)));
        }
        None => {
            out.push(BenchEntry::new("c_bold_face", "none"));
            out.push(BenchEntry::new("c_bold_us", "none"));
        }
    }

    let italic_id = real_face(&db, DbWeight::NORMAL, DbStyle::Italic, false, true);
    match italic_id.and_then(|id| face_data(&db, id).map(|data| (id, data))) {
        Some((id, (italic_bytes, italic_index))) => {
            out.push(BenchEntry::new("c_italic_face", face_family(&db, id)));
            let font = FontRef::from_index(&italic_bytes, italic_index).expect("italic parse");
            let mut context = ScaleContext::new();
            let mut scaler = context.builder(font).size(px).hint(true).build();
            let start = Instant::now();
            let _ = raster_95(&font, &mut scaler, px);
            out.push(BenchEntry::new("c_italic_us", us(start)));
        }
        None => {
            out.push(BenchEntry::new("c_italic_face", "none"));
            out.push(BenchEntry::new("c_italic_us", "none"));
        }
    }

    for ch in ['中', '😀', 'ﷺ', '⠿'] {
        let start = Instant::now();
        let mut chosen = None;
        let mut ok = false;
        for id in candidates(&db, regular, ch) {
            if let Some((glyph, _)) = render_face(&db, id, px, ch) {
                if glyph.w == 0 && glyph.h == 0 && ch != ' ' {
                    continue;
                }
                chosen = Some(id);
                ok = true;
                break;
            }
        }
        out.push(BenchEntry::new(format!("d_{ch}_us"), us(start)));
        out.push(BenchEntry::new(
            format!("d_{ch}_font"),
            chosen.map_or_else(|| "missing".to_string(), |id| face_family(&db, id)),
        ));
        out.push(BenchEntry::new(format!("d_{ch}_ok"), ok.to_string()));
    }

    out.push(BenchEntry::new("e_cell_w", fm.cell_w.to_string()));
    out.push(BenchEntry::new("e_cell_h", fm.cell_h.to_string()));
    out.push(BenchEntry::new("e_ascent", fm.ascent.to_string()));
    out.push(BenchEntry::new("e_descent", fm.descent.to_string()));
    out.push(BenchEntry::new("e_underline_pos", fmt_f64(fm.underline_position as f64)));
    out.push(BenchEntry::new("e_underline_thickness", fmt_f64(fm.underline_thickness as f64)));

    out.push(BenchEntry::new(
        "f_vmhwm_bytes",
        util::vmhwm_bytes().map_or_else(|| "unknown".to_string(), |v| v.to_string()),
    ));
    out
}

pub fn atlas(px: f32) -> Atlas {
    let px = px.max(1.0);
    let mut db = Database::new();
    db.load_system_fonts();
    let regular = query_face(&db, DbWeight::NORMAL, DbStyle::Normal).expect("JetBrains Mono Regular");
    let bold = real_face(&db, DbWeight::BOLD, DbStyle::Normal, true, false);
    let italic = real_face(&db, DbWeight::NORMAL, DbStyle::Italic, false, true);

    let (bytes, index) = face_data(&db, regular).expect("regular bytes");
    let font = FontRef::from_index(&bytes, index).expect("regular parse");
    let fm = metrics_of(&font, px);
    let cw = fm.cell_w as f32;

    let mut rows = Vec::new();
    {
        let mut context = ScaleContext::new();
        let mut scaler = context.builder(font).size(px).hint(true).build();
        rows.push(ascii_row(
            |ch| {
                render_glyph(&font, &mut scaler, px, ch)
                    .map(|(g, _)| AtlasGlyph { glyph: g, advance: cw })
            },
            cw,
        ));
    }
    for id in [bold, italic].into_iter().flatten() {
        let Some((extra_bytes, extra_index)) = face_data(&db, id) else { continue };
        let extra = FontRef::from_index(&extra_bytes, extra_index).expect("variant parse");
        let mut context = ScaleContext::new();
        let mut scaler = context.builder(extra).size(px).hint(true).build();
        rows.push(ascii_row(
            |ch| {
                render_glyph(&extra, &mut scaler, px, ch)
                    .map(|(g, _)| AtlasGlyph { glyph: g, advance: cw })
            },
            cw,
        ));
    }
    while rows.len() < 3 {
        rows.push(ascii_row(|_| None, cw));
    }

    rows.push(simple_row(
        BOX_SAMPLE.chars(),
        |ch| raster_any(&db, regular, px, ch),
        cw,
    ));
    rows.push(simple_row(
        CJK_SAMPLE.chars(),
        |ch| raster_any(&db, regular, px, ch),
        cw,
    ));

    let mut combining = AtlasCell { glyphs: Vec::new() };
    {
        let mut context = ScaleContext::new();
        let mut scaler = context.builder(font).size(px).hint(true).build();
        if let Some((glyph, advance)) = render_glyph(&font, &mut scaler, px, 'e') {
            combining
                .glyphs
                .push(AtlasGlyph { glyph, advance: if advance > 0.0 { advance } else { cw } });
        }
        if let Some((glyph, _)) = render_glyph(&font, &mut scaler, px, '\u{0301}') {
            combining.glyphs.push(AtlasGlyph { glyph, advance: 0.0 });
        }
    }
    rows.push(crate::AtlasRow { cells: vec![combining] });

    rows.push(simple_row(
        EMOJI_SAMPLE.iter().copied(),
        |ch| raster_any(&db, regular, px, ch),
        cw,
    ));

    Atlas { px, ascent: fm.ascent as f32, cell_h: fm.cell_h as f32, rows }
}
