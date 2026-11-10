//! CPU encode benches, std harness: full grid, one row, cursor only.
//! Usage: `cargo bench -p nt-render --bench encode`.
#![expect(clippy::print_stdout, reason = "bench report")]

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use nt_render::__test::{Font, GlyphCache, GridLayer};
use nt_render::FontConfig;
use nt_term::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};

const COLS: u16 = 200;
const ROWS: u16 = 60;
const ITERS: usize = 2_000;

fn frame(font: &mut GlyphCache) -> (Frame, (u32, u32)) {
    let white = Rgba([220, 220, 220, 255]);
    let default_bg = Rgba([0, 0, 0, 255]);
    let colored_bg = Rgba([40, 40, 80, 255]);
    let mut cells = Vec::with_capacity(usize::from(COLS) * usize::from(ROWS));

    for row in 0..ROWS {
        let mut col = 0u16;
        while col < COLS {
            if col % 40 == 35 && col + 1 < COLS {
                cells.push(Cell {
                    ch: '─',
                    fg: white,
                    bg: default_bg,
                    underline: white,
                    flags: CellFlags(CellFlags::WIDE),
                    extra: 0,
                });
                cells.push(Cell {
                    ch: ' ',
                    fg: white,
                    bg: default_bg,
                    underline: white,
                    flags: CellFlags(CellFlags::WIDE_SPACER),
                    extra: 0,
                });
                col += 2;
                continue;
            }

            let byte = b'a' + ((row.wrapping_add(col)) % 26) as u8;
            let mut cell = Cell {
                ch: byte as char,
                fg: white,
                bg: default_bg,
                underline: white,
                flags: CellFlags(0),
                extra: 0,
            };
            if (u32::from(col) * 7 + u32::from(row) * 3) % 23 == 0 {
                cell.bg = colored_bg;
            }
            if (col + row).is_multiple_of(17) {
                cell.flags = CellFlags(CellFlags::UNDERLINE);
            }
            cells.push(cell);
            col += 1;
        }
    }

    let metrics = font.metrics();
    let viewport = (
        u32::from(metrics.cell_w) * u32::from(COLS),
        u32::from(metrics.cell_h) * u32::from(ROWS),
    );
    let frame = Frame {
        cols: COLS,
        rows: ROWS,
        cells,
        dirty_rows: vec![true; usize::from(ROWS)],
        zerowidth: vec![Vec::new(); usize::from(ROWS)],
        cursor: Cursor {
            col: 10,
            row: 5,
            shape: CursorShape::Block,
            color: Rgba([255, 255, 255, 255]),
            blinking: false,
        },
        default_bg,
        generation: 0,
    };
    (frame, viewport)
}

fn stats(mut times: Vec<Duration>) -> (f64, f64, f64) {
    times.sort_unstable();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let min = us(times[0]);
    let median = us(times[times.len() / 2]);
    let p99 = us(times[times.len() * 99 / 100]);
    (min, median, p99)
}

fn main() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/JetBrainsMono-Regular.ttf");
    let cfg = FontConfig {
        family: None,
        size_pt: 12.0,
        line_height: 1.0,
    };
    let font = Font::from_file(&path, &cfg, 1.0).expect("load fixture font");
    let mut glyphs = GlyphCache::new(font, 8192);
    let (mut frame, viewport) = frame(&mut glyphs);
    let mut layer = GridLayer::new();

    let _ = layer.encode(&frame, viewport, true, true, &mut glyphs);
    frame.clear_dirty();
    let _ = layer.encode(&frame, viewport, true, true, &mut glyphs);
    frame.clear_dirty();

    let mut full = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        frame.dirty_rows.fill(true);
        let start = Instant::now();
        let encoded = layer.encode(
            black_box(&frame),
            viewport,
            true,
            true,
            black_box(&mut glyphs),
        );
        full.push(start.elapsed());
        black_box(encoded);
        frame.clear_dirty();
    }

    let mut one_row = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        frame.dirty_rows[30] = true;
        let start = Instant::now();
        let encoded = layer.encode(
            black_box(&frame),
            viewport,
            true,
            true,
            black_box(&mut glyphs),
        );
        one_row.push(start.elapsed());
        black_box(encoded);
        frame.clear_dirty();
    }

    let mut cursor_only = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        frame.cursor.col = if i % 2 == 0 { 11 } else { 10 };
        let start = Instant::now();
        let encoded = layer.encode(
            black_box(&frame),
            viewport,
            true,
            true,
            black_box(&mut glyphs),
        );
        cursor_only.push(start.elapsed());
        black_box(encoded);
        frame.clear_dirty();
    }

    println!(
        "{:<16} {:>10} {:>10} {:>10}",
        "case", "min µs", "median µs", "p99 µs"
    );
    for (name, times) in [
        ("full_200x60", full),
        ("one_row", one_row),
        ("cursor_only", cursor_only),
    ] {
        let (min, median, p99) = stats(times);
        println!("{name:<16} {min:>10.2} {median:>10.2} {p99:>10.2}");
    }
}
