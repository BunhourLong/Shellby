//! cosmic-text stack (fontdb discovery + swash rasteriser via SwashCache).

use std::time::Instant;

use cosmic_text::fontdb::{
    Database, Family, ID, Query, Stretch, Style as DbStyle, Weight as DbWeight,
};
use cosmic_text::skrifa::FontRef as SkrifaFont;
use cosmic_text::skrifa::MetadataProvider as _;
use cosmic_text::{CacheKey, CacheKeyFlags, FontSystem, SwashCache, SwashContent};

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

fn face_family(db: &Database, id: ID) -> String {
    db.face(id)
        .and_then(|f| f.families.first().map(|f| f.0.clone()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn face_path(db: &Database, id: ID) -> Option<String> {
    db.face(id).and_then(|f| util::cosine_source_path(&f.source))
}

fn face_weight(db: &Database, id: ID) -> DbWeight {
    db.face(id).map_or(DbWeight::NORMAL, |f| f.weight)
}

fn covers(db: &Database, id: ID, ch: char) -> bool {
    db.with_face_data(id, |data, index| {
        SkrifaFont::from_index(data, index)
            .ok()
            .and_then(|font| font.charmap().map(ch))
            .is_some()
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
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    id: ID,
    weight: DbWeight,
    px: f32,
    ch: char,
) -> Option<(Glyph, f32)> {
    let font = fs.get_font(id, weight)?;
    let (gid, advance) = {
        let swash = font.as_swash();
        let gid = swash.charmap().map(ch);
        if gid == 0 && ch != ' ' {
            return None;
        }
        let advance = swash.glyph_metrics(&[]).scale(px).advance_width(gid);
        (gid, advance)
    };
    drop(font);

    let key = CacheKey::new(id, gid, px, (0.0, 0.0), weight, CacheKeyFlags::empty()).0;
    let image = cache.get_image_uncached(fs, key)?;

    let w = image.placement.width as u16;
    let h = image.placement.height as u16;
    let (color, data) = match image.content {
        SwashContent::Mask => (false, image.data),
        SwashContent::Color => (true, premultiply(image.data)),
        SwashContent::SubpixelMask => {
            let mut mask = Vec::with_capacity(w as usize * h as usize);
            for px in image.data.chunks_exact(4) {
                mask.push(((px[0] as u32 + px[1] as u32 + px[2] as u32) / 3) as u8);
            }
            (false, mask)
        }
    };

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

fn metrics_of(fs: &mut FontSystem, id: ID, px: f32) -> FontMetrics {
    let font = fs.get_font(id, DbWeight::NORMAL).expect("regular font");
    let m = *font.metrics();
    let scale = if m.units_per_em == 0 { 1.0 } else { px / m.units_per_em as f32 };
    let advance = {
        let swash = font.as_swash();
        swash.glyph_metrics(&[]).scale(px).advance_width(swash.charmap().map('0'))
    };
    let (underline_position, underline_thickness) = match m.underline {
        Some(decoration) => (decoration.offset * scale, decoration.thickness * scale),
        None => (0.0, 0.0),
    };
    let ascent = (m.ascent * scale).round() as i16;
    let descent = (-m.descent * scale).round() as i16;
    FontMetrics {
        cell_w: advance.round().max(1.0) as u16,
        cell_h: (m.ascent * scale + -m.descent * scale + m.leading * scale).round().max(1.0) as u16,
        ascent,
        descent,
        underline_position,
        underline_thickness,
    }
}

fn raster_95(
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    id: ID,
    weight: DbWeight,
    px: f32,
) -> Vec<Glyph> {
    (0x20u8..=0x7e)
        .map(|byte| {
            let ch = byte as char;
            render_glyph(fs, cache, id, weight, px, ch)
                .map(|(g, _)| g)
                .unwrap_or_else(|| empty_glyph(ch))
        })
        .collect()
}

fn us(start: Instant) -> String {
    fmt_f64(start.elapsed().as_secs_f64() * 1e6)
}

fn real_face(
    db: &Database,
    weight: DbWeight,
    style: DbStyle,
    bold: bool,
    italic: bool,
) -> Option<ID> {
    let id = query_face(db, weight, style)?;
    let face = db.face(id)?;
    let weight_ok = !bold || face.weight.0 >= 600;
    let style_ok = !italic || matches!(face.style, DbStyle::Italic | DbStyle::Oblique);
    (weight_ok && style_ok).then_some(id)
}

pub fn rasterize_ascii(px: f32) -> (crate::Metrics, Vec<Glyph>) {
    let px = px.max(1.0);
    let mut fs = FontSystem::new();
    let regular = query_face(fs.db(), DbWeight::NORMAL, DbStyle::Normal).expect("JetBrains Mono");
    let fm = metrics_of(&mut fs, regular, px);
    let mut cache = SwashCache::new();
    let glyphs = raster_95(&mut fs, &mut cache, regular, DbWeight::NORMAL, px);
    (fm.to_metrics(), glyphs)
}

pub fn bench() -> Vec<BenchEntry> {
    let mut out = Vec::new();
    let px = 18.0f32;

    let start = Instant::now();
    let mut fs = FontSystem::new();
    let regular = query_face(fs.db(), DbWeight::NORMAL, DbStyle::Normal).expect("JetBrains Mono");
    let a_scan = us(start);
    out.push(BenchEntry::new("a_us", a_scan.clone()));
    out.push(BenchEntry::new("a_scan_us", a_scan));

    let cached_path = util::fc_match_file("JetBrains Mono:style=Regular");
    let start = Instant::now();
    let path_ok = cached_path
        .as_deref()
        .map(|path| {
            let mut db = Database::new();
            db.load_font_file(path).is_ok()
                && query_face(&db, DbWeight::NORMAL, DbStyle::Normal).is_some()
                && FontSystem::new_with_locale_and_db("en-US".to_string(), db).db().faces().count() > 0
        })
        .unwrap_or(false);
    out.push(BenchEntry::new("a_path_us", us(start)));
    out.push(BenchEntry::new("a_path_ok", path_ok.to_string()));

    out.push(BenchEntry::new("primary_font_family", face_family(fs.db(), regular)));
    out.push(BenchEntry::new(
        "primary_font_path",
        face_path(fs.db(), regular)
            .or(cached_path)
            .unwrap_or_else(|| "unknown".to_string()),
    ));

    let start = Instant::now();
    let _ = fs.get_font(regular, DbWeight::NORMAL).expect("load face at 18px");
    out.push(BenchEntry::new("b_us", us(start)));

    let mut cache = SwashCache::new();
    let start = Instant::now();
    let glyphs = raster_95(&mut fs, &mut cache, regular, DbWeight::NORMAL, px);
    out.push(BenchEntry::new("c_regular_us", us(start)));
    out.push(BenchEntry::new("c_regular_glyphs", glyphs.len().to_string()));

    let bold_id = real_face(fs.db(), DbWeight::BOLD, DbStyle::Normal, true, false);
    match bold_id {
        Some(id) => {
            let weight = face_weight(fs.db(), id);
            out.push(BenchEntry::new("c_bold_face", face_family(fs.db(), id)));
            let _ = fs.get_font(id, weight);
            let start = Instant::now();
            let _ = raster_95(&mut fs, &mut cache, id, weight, px);
            out.push(BenchEntry::new("c_bold_us", us(start)));
        }
        None => {
            out.push(BenchEntry::new("c_bold_face", "none"));
            out.push(BenchEntry::new("c_bold_us", "none"));
        }
    }

    let italic_id = real_face(fs.db(), DbWeight::NORMAL, DbStyle::Italic, false, true);
    match italic_id {
        Some(id) => {
            let weight = face_weight(fs.db(), id);
            out.push(BenchEntry::new("c_italic_face", face_family(fs.db(), id)));
            let _ = fs.get_font(id, weight);
            let start = Instant::now();
            let _ = raster_95(&mut fs, &mut cache, id, weight, px);
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
        for id in candidates(fs.db(), regular, ch) {
            let weight = face_weight(fs.db(), id);
            if let Some((glyph, _)) = render_glyph(&mut fs, &mut cache, id, weight, px, ch) {
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
            chosen.map_or_else(|| "missing".to_string(), |id| face_family(fs.db(), id)),
        ));
        out.push(BenchEntry::new(format!("d_{ch}_ok"), ok.to_string()));
    }

    let fm = metrics_of(&mut fs, regular, px);
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

fn raster_any(
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    primary: ID,
    px: f32,
    ch: char,
) -> Option<AtlasGlyph> {
    for id in candidates(fs.db(), primary, ch) {
        let weight = face_weight(fs.db(), id);
        if let Some((glyph, advance)) = render_glyph(fs, cache, id, weight, px, ch) {
            if glyph.w == 0 && glyph.h == 0 && ch != ' ' {
                continue;
            }
            let advance = if advance > 0.0 { advance } else { glyph.w as f32 };
            return Some(AtlasGlyph { glyph, advance });
        }
    }
    None
}

pub fn atlas(px: f32) -> Atlas {
    let px = px.max(1.0);
    let mut fs = FontSystem::new();
    let regular = query_face(fs.db(), DbWeight::NORMAL, DbStyle::Normal).expect("JetBrains Mono");
    let bold = real_face(fs.db(), DbWeight::BOLD, DbStyle::Normal, true, false);
    let italic = real_face(fs.db(), DbWeight::NORMAL, DbStyle::Italic, false, true);
    let fm = metrics_of(&mut fs, regular, px);
    let cw = fm.cell_w as f32;
    let mut cache = SwashCache::new();

    let mut rows = Vec::new();
    {
        let weight = face_weight(fs.db(), regular);
        rows.push(ascii_row(
            |ch| {
                render_glyph(&mut fs, &mut cache, regular, weight, px, ch)
                    .map(|(g, _)| AtlasGlyph { glyph: g, advance: cw })
            },
            cw,
        ));
    }
    for id in [bold, italic].into_iter().flatten() {
        let weight = face_weight(fs.db(), id);
        rows.push(ascii_row(
            |ch| {
                render_glyph(&mut fs, &mut cache, id, weight, px, ch)
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
        |ch| raster_any(&mut fs, &mut cache, regular, px, ch),
        cw,
    ));
    rows.push(simple_row(
        CJK_SAMPLE.chars(),
        |ch| raster_any(&mut fs, &mut cache, regular, px, ch),
        cw,
    ));

    let weight = face_weight(fs.db(), regular);
    let mut combining = AtlasCell { glyphs: Vec::new() };
    if let Some((glyph, advance)) = render_glyph(&mut fs, &mut cache, regular, weight, px, 'e') {
        combining
            .glyphs
            .push(AtlasGlyph { glyph, advance: if advance > 0.0 { advance } else { cw } });
    }
    if let Some((glyph, _)) = render_glyph(&mut fs, &mut cache, regular, weight, px, '\u{0301}') {
        combining.glyphs.push(AtlasGlyph { glyph, advance: 0.0 });
    }
    rows.push(crate::AtlasRow { cells: vec![combining] });

    rows.push(simple_row(
        EMOJI_SAMPLE.iter().copied(),
        |ch| raster_any(&mut fs, &mut cache, regular, px, ch),
        cw,
    ));

    Atlas { px, ascent: fm.ascent as f32, cell_h: fm.cell_h as f32, rows }
}
