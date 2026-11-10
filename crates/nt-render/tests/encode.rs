//! CPU instance encoding: damage runs, bg/glyph/deco slots, cursor overlay and UI (T07
//! R-5..R-11 plus `encode_ui`).

use std::path::Path;

use nt_render::__test::{
    Deco, FLAG_MASK, FLAG_SOLID, Font, GlyphCache, GridLayer, Inst, OVERLAY_CAP, Style, encode_ui,
};
use nt_render::{FontConfig, PxRect, UiLayer};
use nt_term::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};

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

fn white() -> Rgba {
    Rgba([255, 255, 255, 255])
}

fn plain_cell(ch: char) -> Cell {
    Cell {
        ch,
        fg: white(),
        bg: Rgba([0, 0, 0, 255]),
        underline: white(),
        flags: CellFlags(0),
        extra: 0,
    }
}

fn make_frame(cols: u16, rows: u16) -> Frame {
    let bg = Rgba([0, 0, 0, 255]);
    Frame {
        cols,
        rows,
        cells: vec![plain_cell(' '); usize::from(cols) * usize::from(rows)],
        dirty_rows: vec![true; usize::from(rows)],
        zerowidth: vec![Vec::new(); usize::from(rows)],
        cursor: Cursor {
            col: 0,
            row: 0,
            shape: CursorShape::Hidden,
            color: Rgba([0, 0, 0, 255]),
            blinking: false,
        },
        default_bg: bg,
        generation: 0,
    }
}

fn c(value: i32) -> i16 {
    i16::try_from(value).expect("coordinate fits i16")
}

fn overlay_for(
    glyphs: &mut GlyphCache,
    frame: &Frame,
    focused: bool,
    visible: bool,
    viewport: (u32, u32),
) -> Vec<Inst> {
    let mut layer = GridLayer::new();
    let encoded = layer.encode(frame, viewport, focused, visible, glyphs);
    assert!(encoded.full);
    let range = layer.overlay_range();
    layer.instances()[range].to_vec()
}

/// R-5: a fresh frame is full; one changed row produces exactly one run and touches nothing
/// outside that row's ranges or the overlay.
#[test]
fn r5_dirty_row_partial_upload() {
    let mut glyphs = cache();
    let mut layer = GridLayer::new();
    let mut frame = make_frame(80, 24);
    for (i, cell) in frame.cells.iter_mut().enumerate() {
        cell.ch = if i % 2 == 0 { 'a' } else { 'b' };
    }

    let encoded = layer.encode(&frame, (640, 384), true, true, &mut glyphs);
    assert!(encoded.full);

    let before = layer.instances().to_vec();
    frame.clear_dirty();
    let row = 10u16;
    frame.dirty_rows[usize::from(row)] = true;
    frame.cells[usize::from(row) * 80 + 3].ch = 'Z';

    let encoded = layer.encode(&frame, (640, 384), true, true, &mut glyphs);
    assert!(!encoded.full);
    assert_eq!(encoded.runs.as_slice(), &[(row, row + 1)]);

    let after = layer.instances();
    assert_eq!(before.len(), after.len());
    let ranges = layer.row_ranges((row, row + 1));
    let overlay = layer.overlay_range();
    for (i, (old, new)) in before.iter().zip(after.iter()).enumerate() {
        if i == 0 || overlay.contains(&i) || ranges.iter().any(|range| range.contains(&i)) {
            continue;
        }
        assert_eq!(old, new, "instance {i} changed outside row {row}");
    }
}

/// R-6: default-bg cells get a zero instance; others get a solid cell-sized rect.
#[test]
fn r6_bg_instance_rules() {
    let mut glyphs = cache();
    let mut layer = GridLayer::new();
    let mut frame = make_frame(4, 3);
    let cw = glyphs.metrics().cell_w;
    let ch = glyphs.metrics().cell_h;
    let color = Rgba([10, 20, 30, 255]);
    let col = 2usize;
    let row = 1usize;
    frame.cells[row * 4 + col].bg = color;

    let encoded = layer.encode(
        &frame,
        (u32::from(cw) * 4, u32::from(ch) * 3),
        true,
        true,
        &mut glyphs,
    );
    assert!(encoded.full);

    let inst = layer.instances();
    assert_eq!(inst[1], Inst::default(), "default bg is zero-size");

    let bg = inst[1 + row * 4 + col];
    assert_eq!(bg.flags, FLAG_SOLID);
    assert_eq!(bg.uv, [0, 0, cw, ch]);
    assert_eq!(bg.xy, [c(2 * i32::from(cw)), c(i32::from(ch))]);
    assert_eq!(bg.color, color.0);
}

/// R-7: a WIDE cell uses the wide glyph (2 cells) and its spacer draws nothing.
#[test]
fn r7_wide_glyph() {
    let mut glyphs = cache();
    let mut layer = GridLayer::new();
    let mut frame = make_frame(8, 2);
    let cw = glyphs.metrics().cell_w;
    let ch = glyphs.metrics().cell_h;
    let expected = glyphs
        .get('─', Style::Regular, true)
        .expect("wide box-drawing glyph");

    let wide_col = 3usize;
    let mut cell = plain_cell('─');
    cell.flags = CellFlags(CellFlags::WIDE);
    frame.cells[wide_col] = cell;
    let mut spacer = plain_cell(' ');
    spacer.flags = CellFlags(CellFlags::WIDE_SPACER);
    frame.cells[wide_col + 1] = spacer;

    let encoded = layer.encode(
        &frame,
        (u32::from(cw) * 8, u32::from(ch) * 2),
        true,
        true,
        &mut glyphs,
    );
    assert!(encoded.full);

    let n = 8 * 2;
    let inst = layer.instances();
    let wide = inst[1 + n + wide_col];
    assert_eq!(wide.uv, expected.uv);
    assert_eq!(
        wide.uv[2],
        cw.saturating_mul(2),
        "wide glyph spans two cells"
    );
    assert_eq!(wide.uv[3], ch);
    assert_eq!(wide.flags, FLAG_MASK);
    assert!(wide.uv[2] > 0 && wide.uv[3] > 0);
    assert_eq!(
        wide.xy,
        [
            c(i32::from(cw) * 3 + i32::from(expected.offset[0])),
            c(i32::from(expected.offset[1])),
        ]
    );

    assert_eq!(
        inst[1 + n + wide_col + 1],
        Inst::default(),
        "spacer glyph is zero"
    );
}

/// R-8: underline/strikeout are solid rects at the metric rows; sprite decos are mask quads.
#[test]
fn r8_decorations() {
    let mut glyphs = cache();
    let mut layer = GridLayer::new();
    let mut frame = make_frame(4, 3);
    let metrics = glyphs.metrics();
    let cw = i32::from(metrics.cell_w);
    let underline = Rgba([1, 2, 3, 255]);
    let strike = Rgba([4, 5, 6, 255]);
    let undercurl_color = Rgba([7, 8, 9, 255]);
    let double_color = Rgba([10, 11, 12, 255]);

    let undercurl = glyphs.deco(Deco::Undercurl).expect("undercurl sprite");
    let double = glyphs.deco(Deco::Double).expect("double sprite");

    let mut under = plain_cell(' ');
    under.flags = CellFlags(CellFlags::UNDERLINE);
    under.underline = underline;
    frame.cells[0] = under;

    let mut strike_cell = plain_cell(' ');
    strike_cell.flags = CellFlags(CellFlags::STRIKEOUT);
    strike_cell.fg = strike;
    frame.cells[1] = strike_cell;

    let mut curl = plain_cell(' ');
    curl.flags = CellFlags(CellFlags::UNDERCURL);
    curl.underline = undercurl_color;
    frame.cells[2] = curl;

    let mut dbl = plain_cell(' ');
    dbl.flags = CellFlags(CellFlags::DOUBLE_UNDERLINE);
    dbl.underline = double_color;
    frame.cells[3] = dbl;

    let encoded = layer.encode(
        &frame,
        (u32::from(metrics.cell_w) * 4, u32::from(metrics.cell_h) * 3),
        true,
        true,
        &mut glyphs,
    );
    assert!(encoded.full);

    let n = 4 * 3;
    let base = 1 + 2 * n;
    let inst = layer.instances();

    let k0 = inst[base];
    assert_eq!(k0.flags, FLAG_SOLID);
    assert_eq!(k0.xy, [0, c(i32::from(metrics.underline_y))]);
    assert_eq!(k0.uv, [0, 0, metrics.cell_w, metrics.thickness]);
    assert_eq!(k0.color, underline.0);
    assert_eq!(
        inst[base + 1],
        Inst::default(),
        "no strikeout on underline cell"
    );

    let k1 = inst[base + 2 + 1];
    assert_eq!(k1.flags, FLAG_SOLID);
    assert_eq!(k1.xy, [c(cw), c(i32::from(metrics.strike_y))]);
    assert_eq!(k1.uv, [0, 0, metrics.cell_w, metrics.thickness]);
    assert_eq!(k1.color, strike.0);

    let curl_inst = inst[base + 2 * 2];
    assert_eq!(curl_inst.flags, FLAG_MASK, "undercurl is a mask sprite");
    assert_eq!(curl_inst.uv, undercurl.uv);
    assert_eq!(
        curl_inst.xy,
        [
            c(cw * 2 + i32::from(undercurl.offset[0])),
            c(i32::from(undercurl.offset[1])),
        ]
    );
    assert_eq!(curl_inst.color, undercurl_color.0);

    let double_inst = inst[base + 2 * 3];
    assert_eq!(
        double_inst.flags, FLAG_MASK,
        "double underline is a mask sprite"
    );
    assert_eq!(double_inst.uv, double.uv);
    assert_eq!(
        double_inst.xy,
        [
            c(cw * 3 + i32::from(double.offset[0])),
            c(i32::from(double.offset[1])),
        ]
    );
    assert_eq!(double_inst.color, double_color.0);
}

/// R-9: focused Block draws a cursor rect plus the cell glyph in the cell bg color; Beam and
/// Underline draw one solid rect of the expected size.
#[test]
fn r9_cursor_block_beam_underline() {
    let mut glyphs = cache();
    let metrics = glyphs.metrics();
    let cw = i32::from(metrics.cell_w);
    let ch = i32::from(metrics.cell_h);
    let cursor_color = Rgba([200, 100, 50, 255]);
    let cell_bg = Rgba([9, 9, 9, 255]);
    let a = glyphs.get('A', Style::Regular, false).expect("A glyph");

    let mut frame = make_frame(6, 2);
    let mut cell = plain_cell('A');
    cell.bg = cell_bg;
    frame.cells[2] = cell;
    frame.cursor = Cursor {
        col: 2,
        row: 0,
        shape: CursorShape::Block,
        color: cursor_color,
        blinking: false,
    };
    let viewport = (u32::from(metrics.cell_w) * 6, u32::from(metrics.cell_h) * 2);

    let overlay = overlay_for(&mut glyphs, &frame, true, true, viewport);
    assert_eq!(overlay[0].flags, FLAG_SOLID);
    assert_eq!(overlay[0].xy, [c(cw * 2), 0]);
    assert_eq!(overlay[0].uv, [0, 0, metrics.cell_w, metrics.cell_h]);
    assert_eq!(overlay[0].color, cursor_color.0);
    assert_eq!(overlay[1].uv, a.uv);
    assert_eq!(
        overlay[1].xy,
        [
            c(cw * 2 + i32::from(a.offset[0])),
            c(i32::from(a.offset[1])),
        ]
    );
    assert_eq!(overlay[1].color, cell_bg.0);
    assert_eq!(overlay[1].flags, FLAG_MASK);
    assert_eq!(overlay[2], Inst::default());

    frame.cursor.shape = CursorShape::Beam;
    let overlay = overlay_for(&mut glyphs, &frame, true, true, viewport);
    let beam_w = u16::try_from(((cw + 4) / 8).max(1)).expect("beam width fits");
    assert_eq!(overlay[0].flags, FLAG_SOLID);
    assert_eq!(overlay[0].xy, [c(cw * 2), 0]);
    assert_eq!(overlay[0].uv, [0, 0, beam_w, metrics.cell_h]);
    assert_eq!(overlay[0].color, cursor_color.0);
    assert_eq!(overlay[1], Inst::default());

    frame.cursor.shape = CursorShape::Underline;
    let overlay = overlay_for(&mut glyphs, &frame, true, true, viewport);
    let h = (i32::from(metrics.thickness) * 2).min(ch - i32::from(metrics.underline_y));
    let underline_h = u16::try_from(h).expect("underline height fits");
    assert_eq!(overlay[0].flags, FLAG_SOLID);
    assert_eq!(
        overlay[0].xy,
        [c(cw * 2), c(i32::from(metrics.underline_y))]
    );
    assert_eq!(overlay[0].uv, [0, 0, metrics.cell_w, underline_h]);
    assert_eq!(overlay[0].color, cursor_color.0);
    assert_eq!(overlay[1], Inst::default());
}

/// R-9: `HollowBlock` and an unfocused Block draw exactly four solid edge rects; Hidden and an
/// invisible cursor draw nothing.
#[test]
fn r9_cursor_hollow_unfocused_hidden() {
    let mut glyphs = cache();
    let metrics = glyphs.metrics();
    let cw = i32::from(metrics.cell_w);
    let ch = i32::from(metrics.cell_h);
    let cursor_color = Rgba([200, 100, 50, 255]);
    let viewport = (u32::from(metrics.cell_w) * 6, u32::from(metrics.cell_h) * 2);

    let mut frame = make_frame(6, 2);
    frame.cells[2] = plain_cell('A');
    frame.cursor = Cursor {
        col: 2,
        row: 0,
        shape: CursorShape::HollowBlock,
        color: cursor_color,
        blinking: false,
    };

    let overlay = overlay_for(&mut glyphs, &frame, true, true, viewport);
    let t = u16::try_from(((cw + 6) / 12).max(1)).expect("stroke fits");
    assert_eq!(
        overlay
            .iter()
            .filter(|inst| inst.flags == FLAG_SOLID)
            .count(),
        4
    );
    assert_eq!(overlay[0].xy, [c(cw * 2), 0]);
    assert_eq!(overlay[0].uv, [0, 0, metrics.cell_w, t]);
    assert_eq!(overlay[1].xy, [c(cw * 2), c(ch - i32::from(t))]);
    assert_eq!(overlay[1].uv, [0, 0, metrics.cell_w, t]);
    assert_eq!(overlay[2].xy, [c(cw * 2), 0]);
    assert_eq!(overlay[2].uv, [0, 0, t, metrics.cell_h]);
    assert_eq!(overlay[3].xy, [c(cw * 2 + cw - i32::from(t)), 0]);
    assert_eq!(overlay[3].uv, [0, 0, t, metrics.cell_h]);
    for inst in &overlay[..4] {
        assert_eq!(inst.color, cursor_color.0);
    }
    assert_eq!(overlay[4], Inst::default());

    frame.cursor.shape = CursorShape::Block;
    let overlay = overlay_for(&mut glyphs, &frame, false, true, viewport);
    assert_eq!(
        overlay
            .iter()
            .filter(|inst| inst.flags == FLAG_SOLID)
            .count(),
        4
    );
    assert_eq!(overlay[4], Inst::default());

    frame.cursor.shape = CursorShape::Hidden;
    let overlay = overlay_for(&mut glyphs, &frame, true, true, viewport);
    assert!(overlay.iter().all(|inst| *inst == Inst::default()));

    frame.cursor.shape = CursorShape::Block;
    let overlay = overlay_for(&mut glyphs, &frame, true, false, viewport);
    assert!(overlay.iter().all(|inst| *inst == Inst::default()));
}

/// R-9: a cursor on a WIDE cell is two cells wide, including its glyph.
#[test]
fn r9_cursor_wide() {
    let mut glyphs = cache();
    let metrics = glyphs.metrics();
    let cw = metrics.cell_w;
    let cursor_color = Rgba([200, 100, 50, 255]);

    let mut frame = make_frame(6, 2);
    let mut wide = plain_cell('─');
    wide.flags = CellFlags(CellFlags::WIDE);
    frame.cells[2] = wide;
    let mut spacer = plain_cell(' ');
    spacer.flags = CellFlags(CellFlags::WIDE_SPACER);
    frame.cells[3] = spacer;
    frame.cursor = Cursor {
        col: 2,
        row: 0,
        shape: CursorShape::Block,
        color: cursor_color,
        blinking: false,
    };
    let viewport = (u32::from(metrics.cell_w) * 6, u32::from(metrics.cell_h) * 2);

    let overlay = overlay_for(&mut glyphs, &frame, true, true, viewport);
    assert_eq!(overlay[0].flags, FLAG_SOLID);
    assert_eq!(overlay[0].xy, [c(i32::from(cw) * 2), 0]);
    assert_eq!(overlay[0].uv[2], cw.saturating_mul(2));
    assert_eq!(overlay[0].uv[3], metrics.cell_h);
    assert_eq!(
        overlay[1].uv[2],
        cw.saturating_mul(2),
        "cursor glyph is wide too"
    );
}

/// R-10: an epoch bump or a grid resize forces a full encode; resizing grows the arrays.
#[test]
fn r10_epoch_and_resize_full() {
    let mut glyphs = cache();
    let mut layer = GridLayer::new();
    let mut frame = make_frame(4, 3);
    let metrics = glyphs.metrics();
    let viewport = (u32::from(metrics.cell_w) * 4, u32::from(metrics.cell_h) * 3);

    let encoded = layer.encode(&frame, viewport, true, true, &mut glyphs);
    assert!(encoded.full);
    frame.clear_dirty();
    let encoded = layer.encode(&frame, viewport, true, true, &mut glyphs);
    assert!(!encoded.full);

    let epoch = glyphs.epoch();
    glyphs.reset();
    assert_eq!(glyphs.epoch(), epoch + 1);
    let encoded = layer.encode(&frame, viewport, true, true, &mut glyphs);
    assert!(encoded.full, "epoch bump re-encodes everything");

    let mut bigger = make_frame(5, 4);
    bigger.dirty_rows.fill(false);
    let encoded = layer.encode(&bigger, viewport, true, true, &mut glyphs);
    assert!(encoded.full, "grid resize re-encodes everything");
    let n: usize = 5 * 4;
    assert_eq!(layer.instances().len(), 1 + 4 * n + OVERLAY_CAP);
}

/// R-11: more than 16 dirty runs collapse to one range; a viewport change only sets head; an
/// unchanged encode is empty.
#[test]
fn r11_dirty_runs_head_and_empty() {
    let mut glyphs = cache();
    let mut layer = GridLayer::new();
    let mut frame = make_frame(80, 40);
    let metrics = glyphs.metrics();
    let viewport = (
        u32::from(metrics.cell_w) * 80,
        u32::from(metrics.cell_h) * 40,
    );

    let encoded = layer.encode(&frame, viewport, true, true, &mut glyphs);
    assert!(encoded.full);
    frame.clear_dirty();

    for row in (0..40).step_by(2) {
        frame.dirty_rows[row] = true;
    }
    let encoded = layer.encode(&frame, viewport, true, true, &mut glyphs);
    assert!(!encoded.full);
    assert_eq!(encoded.runs.as_slice(), &[(0, 39)]);

    frame.clear_dirty();
    let moved = (viewport.0 + 1, viewport.1 + 1);
    let encoded = layer.encode(&frame, moved, true, true, &mut glyphs);
    assert!(encoded.head, "viewport change updates the pane background");
    assert!(!encoded.full);
    assert_eq!(encoded.runs.as_slice().len(), 0);

    let encoded = layer.encode(&frame, moved, true, true, &mut glyphs);
    assert!(encoded.is_empty(), "nothing changed");
}

/// `encode_ui`: rects come first in order; glyph quads are trimmed inside the run's clip.
#[test]
fn ui_rects_first_and_text_clipped() {
    let mut glyphs = cache();
    let cw = u32::from(glyphs.metrics().cell_w);
    let ch = u32::from(glyphs.metrics().cell_h);
    let mut ui = UiLayer::default();
    ui.clear();
    ui.rect(
        PxRect {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        },
        Rgba([1, 2, 3, 255]),
    );
    ui.rect(
        PxRect {
            x: 5,
            y: 6,
            w: 7,
            h: 8,
        },
        Rgba([4, 5, 6, 255]),
    );
    let clip = PxRect {
        x: 0,
        y: 0,
        w: 3 * cw,
        h: 2 * ch,
    };
    ui.text(0, 0, "ABCDE", Rgba([9, 9, 9, 255]), false, clip);

    let mut out = Vec::new();
    encode_ui(&ui, &mut glyphs, &mut out);

    assert!(out.len() >= 3);
    assert_eq!(out[0].flags, FLAG_SOLID);
    assert_eq!(out[0].xy, [1, 2]);
    assert_eq!(out[0].uv, [0, 0, 3, 4]);
    assert_eq!(out[0].color, [1, 2, 3, 255]);
    assert_eq!(out[1].flags, FLAG_SOLID);
    assert_eq!(out[1].xy, [5, 6]);
    assert_eq!(out[1].uv, [0, 0, 7, 8]);
    assert_eq!(out[1].color, [4, 5, 6, 255]);

    let mut drawn = 0usize;
    let clip_left = i32::try_from(clip.x).expect("clip x fits");
    let clip_top = i32::try_from(clip.y).expect("clip y fits");
    let clip_right = clip_left + i32::try_from(clip.w).expect("clip w fits");
    let clip_bottom = clip_top + i32::try_from(clip.h).expect("clip h fits");
    for inst in &out[2..] {
        assert_ne!(inst.flags, FLAG_SOLID);
        let x0 = i32::from(inst.xy[0]);
        let y0 = i32::from(inst.xy[1]);
        let x1 = x0 + i32::from(inst.uv[2]);
        let y1 = y0 + i32::from(inst.uv[3]);
        assert!(x0 >= clip_left);
        assert!(y0 >= clip_top);
        assert!(x1 <= clip_right);
        assert!(y1 <= clip_bottom);
        drawn += 1;
    }
    assert!(drawn < 5, "the run is clipped");
}
