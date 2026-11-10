//! GPU readback tests (T07 G-1..G-7). Ignored unless `NT_GPU_TESTS=1`; without a GPU the
//! helper panics instead of letting a test return early and pass.
#![allow(clippy::many_single_char_names, reason = "pixel coordinate math")]

use std::path::Path;

use nt_render::{CellSize, FontConfig, PaneDraw, PxRect, Renderer, Scene, UiLayer};
use nt_term::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};

const WHITE: Rgba = Rgba([255, 255, 255, 255]);
const BLACK: Rgba = Rgba([0, 0, 0, 255]);

fn renderer(w: u32, h: u32) -> Renderer {
    assert!(
        std::env::var("NT_GPU_TESTS").as_deref() == Ok("1"),
        "set NT_GPU_TESTS=1"
    );
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/JetBrainsMono-Regular.ttf");
    let font = FontConfig {
        family: None,
        size_pt: 12.0,
        line_height: 1.0,
    };
    Renderer::new_headless((w, h), 1.0, &font, Some(&path))
        .unwrap_or_else(|e| panic!("new_headless({w}x{h}) failed: {e}"))
}

fn renderer_for(cols: u16, rows: u16) -> (Renderer, CellSize, (u32, u32)) {
    let mut r = renderer(64, 64);
    let cell = r.cell_size();
    let size = (
        u32::from(cols) * u32::from(cell.w),
        u32::from(rows) * u32::from(cell.h),
    );
    r.resize(size);
    (r, cell, size)
}

fn blank_frame(cols: u16, rows: u16, bg: Rgba) -> Frame {
    let count = usize::from(cols) * usize::from(rows);
    let cells = vec![
        Cell {
            ch: ' ',
            fg: WHITE,
            bg,
            underline: WHITE,
            flags: CellFlags(0),
            extra: 0,
        };
        count
    ];
    Frame {
        cols,
        rows,
        cells,
        dirty_rows: vec![true; usize::from(rows)],
        zerowidth: vec![Vec::new(); usize::from(rows)],
        cursor: Cursor {
            col: 0,
            row: 0,
            shape: CursorShape::Hidden,
            color: BLACK,
            blinking: false,
        },
        default_bg: bg,
        generation: 0,
    }
}

fn set(frame: &mut Frame, col: u16, row: u16, ch: char, fg: Rgba, bg: Rgba) {
    let idx = usize::from(row) * usize::from(frame.cols) + usize::from(col);
    frame.cells[idx] = Cell {
        ch,
        fg,
        bg,
        underline: fg,
        flags: CellFlags(0),
        extra: 0,
    };
}

fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) as usize) * 4;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

fn expect(buf: &[u8], w: u32, x: u32, y: u32, want: Rgba, ctx: &str) {
    assert_eq!(px(buf, w, x, y), want.0, "{ctx}: pixel ({x}, {y})");
}

fn run(r: &mut Renderer, panes: &mut [PaneDraw<'_>], ui: &UiLayer, clear: Rgba) -> Vec<u8> {
    let mut scene = Scene { panes, ui, clear };
    r.render_offscreen(&mut scene)
        .unwrap_or_else(|e| panic!("render_offscreen failed: {e}"))
}

fn single(frame: &mut Frame, size: (u32, u32)) -> [PaneDraw<'_>; 1] {
    [PaneDraw {
        id: 1,
        frame,
        viewport: PxRect {
            x: 0,
            y: 0,
            w: size.0,
            h: size.1,
        },
        focused: true,
        cursor_visible: true,
    }]
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g1_default_cells_match_clear() {
    let bg = Rgba([10, 20, 30, 255]);
    let (mut r, _cell, (w, h)) = renderer_for(4, 3);
    let mut frame = blank_frame(4, 3, bg);
    let ui = UiLayer::default();
    let mut panes = single(&mut frame, (w, h));
    let buf = run(&mut r, &mut panes, &ui, bg);
    assert_eq!(buf.len(), (w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            expect(&buf, w, x, y, bg, "G-1 default bg");
        }
    }
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g2_one_cell_background_does_not_bleed() {
    let bg = Rgba([10, 20, 30, 255]);
    let red = Rgba([255, 0, 0, 255]);
    let (mut r, cell, (w, h)) = renderer_for(5, 4);
    let cw = u32::from(cell.w);
    let ch = u32::from(cell.h);
    let mut frame = blank_frame(5, 4, bg);
    set(&mut frame, 2, 1, ' ', bg, red);
    let ui = UiLayer::default();
    let mut panes = single(&mut frame, (w, h));
    let buf = run(&mut r, &mut panes, &ui, bg);
    for y in 0..h {
        for x in 0..w {
            let inside = (2 * cw..3 * cw).contains(&x) && (ch..2 * ch).contains(&y);
            let want = if inside { red } else { bg };
            expect(&buf, w, x, y, want, "G-2 cell rect");
        }
    }
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g3_block_elements_cover_their_cells() {
    let bg = Rgba([10, 20, 30, 255]);
    let (mut r, cell, (w, h)) = renderer_for(2, 1);
    let cw = u32::from(cell.w);
    let ch = u32::from(cell.h);
    let mut frame = blank_frame(2, 1, bg);
    set(&mut frame, 0, 0, '\u{2588}', WHITE, bg);
    set(&mut frame, 1, 0, '\u{2580}', WHITE, bg);
    let ui = UiLayer::default();
    let mut panes = single(&mut frame, (w, h));
    let buf = run(&mut r, &mut panes, &ui, bg);
    for y in 0..h {
        for x in 0..cw {
            expect(&buf, w, x, y, WHITE, "G-3 full block");
        }
    }
    let upper = ch / 2;
    let lower = ch.div_ceil(2);
    for y in 0..h {
        for x in cw..w {
            if y < upper {
                expect(&buf, w, x, y, WHITE, "G-3 upper half");
            } else if y >= lower {
                expect(&buf, w, x, y, bg, "G-3 lower half");
            }
        }
    }
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g4_block_cursor_uses_cursor_color_and_bg_ink() {
    let green = Rgba([0, 255, 0, 255]);
    let (mut r, _cell, (w, h)) = renderer_for(1, 1);
    let mut frame = blank_frame(1, 1, BLACK);
    set(&mut frame, 0, 0, 'A', WHITE, BLACK);
    frame.cursor = Cursor {
        col: 0,
        row: 0,
        shape: CursorShape::Block,
        color: green,
        blinking: false,
    };
    let ui = UiLayer::default();
    let mut panes = single(&mut frame, (w, h));
    let buf = run(&mut r, &mut panes, &ui, BLACK);

    let total = usize::try_from(w * h).expect("cell fits usize");
    let mut green_px = 0usize;
    let mut ink_px = 0usize;
    for y in 0..h {
        for x in 0..w {
            let [red, g, blue, alpha] = px(&buf, w, x, y);
            assert_eq!((red, blue, alpha), (0, 0, 255), "G-4 channel at ({x}, {y})");
            if [red, g, blue, alpha] == green.0 {
                green_px += 1;
            }
            if g < 128 {
                ink_px += 1;
            }
        }
    }
    assert!(
        green_px * 100 >= total * 60,
        "G-4 green coverage {green_px}/{total}"
    );
    assert!(ink_px >= 1, "G-4 cursor glyph ink drawn in cell bg");

    frame.cursor.shape = CursorShape::Hidden;
    let mut panes = single(&mut frame, (w, h));
    let buf = run(&mut r, &mut panes, &ui, BLACK);
    for y in 0..h {
        for x in 0..w {
            assert_ne!(px(&buf, w, x, y), green.0, "G-4 no green at ({x}, {y})");
        }
    }
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g5_damage_stays_inside_the_dirty_row_band() {
    let bg = BLACK;
    let (mut r, cell, (w, h)) = renderer_for(8, 5);
    let ch = u32::from(cell.h);
    let mut frame = blank_frame(8, 5, bg);
    let rows = ["abcdefgh", "ijklmnop", "qrstuvwx", "ABCDEFGH", "12345678"];
    for (row, text) in rows.iter().enumerate() {
        for (col, ch) in text.chars().enumerate() {
            set(&mut frame, col as u16, row as u16, ch, WHITE, bg);
        }
    }
    let ui = UiLayer::default();
    let mut panes = single(&mut frame, (w, h));
    let first = run(&mut r, &mut panes, &ui, bg);

    for (col, ch) in "ZYXWVUTS".chars().enumerate() {
        set(&mut frame, col as u16, 3, ch, WHITE, bg);
    }
    frame.dirty_rows[3] = true;
    let mut panes = single(&mut frame, (w, h));
    let second = run(&mut r, &mut panes, &ui, bg);

    let band = 3 * ch..4 * ch;
    let mut inside_diff = false;
    for y in 0..h {
        for x in 0..w {
            if px(&first, w, x, y) != px(&second, w, x, y) {
                assert!(band.contains(&y), "G-5 diff outside row 3 at ({x}, {y})");
                inside_diff = true;
            }
        }
    }
    assert!(inside_diff, "G-5 changed row must differ");

    let mut panes = single(&mut frame, (w, h));
    let third = run(&mut r, &mut panes, &ui, bg);
    assert_eq!(third, second, "G-5 unchanged re-render is identical");
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g6_panes_are_clipped_to_their_viewports() {
    let green = Rgba([0, 128, 0, 255]);
    let blue = Rgba([0, 0, 255, 255]);
    let mut r = renderer(64, 64);
    let cell = r.cell_size();
    let cw = u32::from(cell.w);
    let ch = u32::from(cell.h);
    let base_cols: u16 = 10;
    let rows: u16 = 3;
    let split = u32::from(base_cols) * cw;
    let pane_w = split + cw / 2;
    let pane_h = u32::from(rows) * ch;
    let target = (2 * pane_w, pane_h);
    r.resize(target);

    let mut left = blank_frame(base_cols + 1, rows, green);
    for row in 0..rows {
        set(&mut left, base_cols, row, '\u{2588}', WHITE, green);
    }
    let mut right = blank_frame(base_cols, rows, blue);

    let mut panes = [
        PaneDraw {
            id: 2,
            frame: &mut right,
            viewport: PxRect {
                x: pane_w,
                y: 0,
                w: pane_w,
                h: pane_h,
            },
            focused: false,
            cursor_visible: false,
        },
        PaneDraw {
            id: 1,
            frame: &mut left,
            viewport: PxRect {
                x: 0,
                y: 0,
                w: pane_w,
                h: pane_h,
            },
            focused: true,
            cursor_visible: false,
        },
    ];
    let ui = UiLayer::default();
    let buf = run(&mut r, &mut panes, &ui, blue);

    for y in 0..pane_h {
        for x in 0..split {
            expect(&buf, target.0, x, y, green, "G-6 pane 1 background");
        }
        for x in split..pane_w {
            expect(&buf, target.0, x, y, WHITE, "G-6 clipped full block");
        }
        for x in pane_w..target.0 {
            expect(&buf, target.0, x, y, blue, "G-6 pane 1 stays clipped");
        }
    }
}

#[test]
#[ignore = "GPU: NT_GPU_TESTS=1 cargo test -p nt-render --test gpu -- --ignored"]
fn g7_ui_rect_covers_exactly_its_pixels() {
    let bg = BLACK;
    let clear = Rgba([10, 20, 30, 255]);
    let mark = Rgba([1, 2, 3, 255]);
    let (mut r, _cell, (w, h)) = renderer_for(8, 6);
    let mut frame = blank_frame(8, 6, bg);
    let mut ui = UiLayer::default();
    let rect = PxRect {
        x: 5,
        y: 5,
        w: 3,
        h: 2,
    };
    ui.rect(rect, mark);
    let mut panes = single(&mut frame, (w, h));
    let buf = run(&mut r, &mut panes, &ui, clear);

    let mut hits = 0usize;
    for y in 0..h {
        for x in 0..w {
            if px(&buf, w, x, y) == mark.0 {
                assert!(
                    (5..8).contains(&x) && (5..7).contains(&y),
                    "G-7 stray UI pixel at ({x}, {y})"
                );
                hits += 1;
            }
        }
    }
    assert_eq!(hits, 6, "G-7 UI rect pixel count");
    for y in 5..7 {
        for x in 5..8 {
            expect(&buf, w, x, y, mark, "G-7 UI rect");
        }
    }
}
