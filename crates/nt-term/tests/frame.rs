//! Black-box tests for `Terminal::fill_frame` semantics (T06 F-1..F-14).

use nt_term::{
    Cell, CellFlags, CursorShape, Frame, Palette, Rgba, TermConfig, TermEvent, TermSize, Terminal,
};

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn term_size() -> TermSize {
    TermSize {
        cols: COLS,
        rows: ROWS,
        cell_w_px: 8,
        cell_h_px: 16,
    }
}

fn term_config() -> TermConfig {
    TermConfig {
        hostname: "testhost".into(),
        ..TermConfig::default()
    }
}

fn new_term() -> Terminal {
    Terminal::new(term_size(), &term_config())
}

fn new_term_with(config: &TermConfig) -> Terminal {
    Terminal::new(term_size(), config)
}

fn feed(term: &mut Terminal, bytes: &[u8]) -> Vec<TermEvent> {
    let mut events = Vec::new();
    term.advance(bytes, &mut events);
    events
}

fn fill(term: &mut Terminal) -> Frame {
    let mut frame = Frame::default();
    term.fill_frame(&mut frame);
    frame
}

fn cell(frame: &Frame, row: u16, col: u16) -> Cell {
    frame.row(row)[usize::from(col)]
}

fn text_cell(frame: &Frame, ch: char) -> Cell {
    frame
        .cells
        .iter()
        .copied()
        .find(|cell| cell.ch == ch)
        .expect("cell with the expected glyph is visible")
}

fn single_cell(term: &mut Terminal, bytes: &[u8]) -> Cell {
    feed(term, bytes);
    let frame = fill(term);
    text_cell(&frame, 'x')
}

fn row_text(frame: &Frame, row: u16) -> String {
    let text: String = frame.row(row).iter().map(|cell| cell.ch).collect();
    text.trim_end().to_owned()
}

fn dim(color: Rgba) -> Rgba {
    let Rgba([r, g, b, a]) = color;
    let third = |v: u8| (u16::from(v) * 2 / 3) as u8;
    Rgba([third(r), third(g), third(b), a])
}

#[test]
fn f01_fresh_frame_is_blank_and_fully_dirty() {
    let mut term = new_term();
    let frame = fill(&mut term);
    let palette = Palette::default();

    assert_eq!(frame.cols, COLS);
    assert_eq!(frame.rows, ROWS);
    assert_eq!(frame.cells.len(), usize::from(COLS) * usize::from(ROWS));
    assert_eq!(frame.dirty_rows.len(), usize::from(ROWS));
    assert!(frame.dirty_rows.iter().all(|&dirty| dirty));
    assert_eq!(frame.default_bg, palette.bg);
    assert_eq!(frame.generation, 1);

    for cell in &frame.cells {
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.fg, palette.fg);
        assert_eq!(cell.bg, palette.bg);
        assert_eq!(cell.underline, palette.fg);
        assert_eq!(cell.flags, CellFlags::default());
        assert_eq!(cell.extra, 0);
    }
}

#[test]
fn f02_only_damaged_and_cursor_rows_are_dirty() {
    let mut term = new_term();
    let mut frame = fill(&mut term);
    frame.clear_dirty();

    feed(&mut term, b"\x1b[5;1Hx");
    term.fill_frame(&mut frame);

    for row in 0..ROWS {
        let expected = row == 0 || row == 4;
        assert_eq!(
            frame.dirty_rows[usize::from(row)],
            expected,
            "row {row} dirtiness"
        );
    }
    assert_eq!(cell(&frame, 4, 0).ch, 'x');
}

#[test]
fn f03_dirty_rows_accumulate_without_clear() {
    let mut term = new_term();
    let mut frame = fill(&mut term);
    frame.clear_dirty();

    feed(&mut term, b"a");
    term.fill_frame(&mut frame);
    assert!(frame.dirty_rows[0]);

    feed(&mut term, b"\x1b[3;1Hb");
    term.fill_frame(&mut frame);
    assert!(frame.dirty_rows[0], "earlier damage survives a refill");
    assert!(frame.dirty_rows[2], "newly damaged row is dirty");
}

#[test]
fn f04_ansi_and_bold_as_bright() {
    let palette = Palette::default();
    let mut term = new_term();
    assert_eq!(single_cell(&mut term, b"\x1b[31mx").fg, palette.ansi[1]);

    let mut term = new_term();
    assert_eq!(single_cell(&mut term, b"\x1b[1;31mx").fg, palette.ansi[9]);

    let config = TermConfig {
        bold_is_bright: false,
        ..term_config()
    };
    let mut term = new_term_with(&config);
    let bold = single_cell(&mut term, b"\x1b[1;31mx");
    assert_eq!(bold.fg, palette.ansi[1]);
    assert!(bold.flags.contains(CellFlags::BOLD));
}

#[test]
fn f05_indexed_and_truecolor() {
    let mut term = new_term();
    assert_eq!(
        single_cell(&mut term, b"\x1b[38;5;196mx").fg,
        Rgba([255, 0, 0, 255])
    );

    let mut term = new_term();
    assert_eq!(
        single_cell(&mut term, b"\x1b[38;5;232mx").fg,
        Rgba([8, 8, 8, 255])
    );

    let mut term = new_term();
    assert_eq!(
        single_cell(&mut term, b"\x1b[38;5;21mx").fg,
        Rgba([0, 0, 255, 255])
    );

    let mut term = new_term();
    assert_eq!(
        single_cell(&mut term, b"\x1b[38;2;1;2;3mx").fg,
        Rgba([1, 2, 3, 255])
    );

    let mut term = new_term();
    assert_eq!(
        single_cell(&mut term, b"\x1b[1;38;2;1;2;3mx").fg,
        Rgba([1, 2, 3, 255]),
        "bold does not brighten truecolor"
    );
}

#[test]
fn f06_inverse_hidden_and_dim() {
    let palette = Palette::default();

    let mut term = new_term();
    let inverse = single_cell(&mut term, b"\x1b[31;7mx");
    assert_eq!(inverse.fg, palette.bg);
    assert_eq!(inverse.bg, palette.ansi[1]);

    let mut term = new_term();
    let hidden = single_cell(&mut term, b"\x1b[31;8mx");
    assert_eq!(hidden.fg, hidden.bg);
    assert!(hidden.flags.contains(CellFlags::HIDDEN));

    let mut term = new_term();
    assert_eq!(
        single_cell(&mut term, b"\x1b[2;31mx").fg,
        dim(palette.ansi[1])
    );
}

#[test]
fn f07_wide_chars_and_leading_spacer() {
    let mut term = new_term();
    let frame = {
        feed(&mut term, "漢".as_bytes());
        fill(&mut term)
    };
    let wide = cell(&frame, 0, 0);
    assert_eq!(wide.ch, '漢');
    assert!(wide.flags.contains(CellFlags::WIDE));
    assert!(cell(&frame, 0, 1).flags.contains(CellFlags::WIDE_SPACER));

    let mut term = new_term();
    feed(&mut term, "\x1b[1;80H漢".as_bytes());
    let frame = fill(&mut term);

    let spacer = cell(&frame, 0, COLS - 1);
    assert_eq!(spacer.ch, ' ');
    assert_eq!(spacer.flags, CellFlags::default());

    let wrapped = cell(&frame, 1, 0);
    assert_eq!(wrapped.ch, '漢');
    assert!(wrapped.flags.contains(CellFlags::WIDE));
    assert!(cell(&frame, 1, 1).flags.contains(CellFlags::WIDE_SPACER));
}

#[test]
fn f08_combining_marks_survive_other_row_refills() {
    let mut term = new_term();
    let mut frame = fill(&mut term);
    frame.clear_dirty();

    feed(&mut term, "e\u{301}".as_bytes());
    term.fill_frame(&mut frame);
    assert_ne!(cell(&frame, 0, 0).extra, 0);
    assert!(
        frame.zerowidth[0]
            .iter()
            .any(|(col, marks)| *col == 0 && marks == "\u{301}")
    );
    frame.clear_dirty();

    feed(&mut term, b"\x1b[2;1Hz");
    term.fill_frame(&mut frame);
    frame.clear_dirty();

    feed(&mut term, b"\x1b[5;1Hq");
    term.fill_frame(&mut frame);

    assert!(!frame.dirty_rows[0], "row 0 was not refilled");
    assert_ne!(cell(&frame, 0, 0).extra, 0);
    assert!(
        frame.zerowidth[0]
            .iter()
            .any(|(col, marks)| *col == 0 && marks == "\u{301}")
    );
}

#[test]
fn f09_cursor_position_shape_and_visibility() {
    let mut term = new_term();
    feed(&mut term, b"\x1b[10;5H");
    let frame = fill(&mut term);
    assert_eq!(frame.cursor.col, 4);
    assert_eq!(frame.cursor.row, 9);
    assert_ne!(frame.cursor.shape, CursorShape::Hidden);

    feed(&mut term, b"\x1b[?25l");
    let frame = fill(&mut term);
    assert_eq!(frame.cursor.shape, CursorShape::Hidden);

    feed(&mut term, b"\x1b[?25h\x1b[5 q");
    let frame = fill(&mut term);
    assert_eq!(frame.cursor.shape, CursorShape::Beam);
    assert!(frame.cursor.blinking);
}

#[test]
fn f10_scrollback_shifts_viewport_and_hides_cursor() {
    let mut term = new_term();
    for line in 0..100 {
        feed(&mut term, format!("line{line:03}\r\n").as_bytes());
    }
    let mut frame = fill(&mut term);
    let before = row_text(&frame, 0);
    frame.clear_dirty();
    term.scroll_display(10);
    term.fill_frame(&mut frame);
    let after = row_text(&frame, 0);

    assert_ne!(before, after, "older scrollback lines are shown");
    assert!(frame.dirty_rows.iter().all(|&dirty| dirty));
    assert_eq!(frame.cursor.shape, CursorShape::Hidden);
}

#[test]
fn f11_resize_refits_frame_and_dirties_everything() {
    let mut term = new_term();
    let mut frame = fill(&mut term);
    frame.clear_dirty();

    term.resize(TermSize {
        cols: 100,
        rows: 30,
        cell_w_px: 8,
        cell_h_px: 16,
    });
    term.fill_frame(&mut frame);

    assert_eq!(frame.cols, 100);
    assert_eq!(frame.rows, 30);
    assert_eq!(frame.cells.len(), 100 * 30);
    assert_eq!(frame.dirty_rows.len(), 30);
    assert!(frame.dirty_rows.iter().all(|&dirty| dirty));
}

#[test]
fn f12_osc4_palette_override_wins() {
    let mut term = new_term();
    let red = single_cell(&mut term, b"\x1b]4;1;rgb:12/34/56\x07\x1b[31mx");
    assert_eq!(red.fg, Rgba([0x12, 0x34, 0x56, 255]));
}

#[test]
fn f13_alt_screen_switches_and_restores() {
    let mut term = new_term();
    feed(&mut term, b"primary\r\n");
    let mut frame = fill(&mut term);
    frame.clear_dirty();

    feed(&mut term, b"\x1b[?1049h\x1b[Halt");
    term.fill_frame(&mut frame);
    assert!(row_text(&frame, 0).starts_with("alt"));
    frame.clear_dirty();

    feed(&mut term, b"\x1b[?1049l");
    term.fill_frame(&mut frame);
    assert!(row_text(&frame, 0).starts_with("primary"));
    assert!(frame.dirty_rows.iter().all(|&dirty| dirty));
}

#[test]
fn f14_generation_strictly_increases() {
    let mut term = new_term();
    let mut frame = Frame::default();

    term.fill_frame(&mut frame);
    let first = frame.generation;
    term.fill_frame(&mut frame);
    let second = frame.generation;
    feed(&mut term, b"x");
    term.fill_frame(&mut frame);
    let third = frame.generation;

    assert!(second > first, "{second} > {first}");
    assert!(third > second, "{third} > {second}");
}
