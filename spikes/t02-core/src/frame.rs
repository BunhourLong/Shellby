//! Architecture §3.2 render input, filled from an `alacritty_terminal::Term`.
//! Spike code: T06 re-implements this with tests; do not copy verbatim.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{self, Color, NamedColor, Rgb};

// ---- §3.2 types, verbatim ----

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Rgba(pub [u8; 4]);

// No bitflags crate: plain u16 constants.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct CellFlags(pub u16);
impl CellFlags {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 3;
    pub const UNDERCURL: u16 = 1 << 4;
    pub const STRIKEOUT: u16 = 1 << 5;
    pub const WIDE: u16 = 1 << 6; // first half of a 2-column char
    pub const WIDE_SPACER: u16 = 1 << 7; // second half; renderer skips glyph
    pub const HIDDEN: u16 = 1 << 8;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Cell {
    // 16 bytes, Copy, no heap
    pub ch: char, // ' ' for empty
    pub fg: Rgba,
    pub bg: Rgba,
    pub flags: CellFlags,
    pub extra: u16, // 1 = this cell has combining marks in Frame::zerowidth[row]; 0 = none
}

pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgba,
    pub blinking: bool,
}

pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>, // row-major, len == cols*rows, reused across frames
    pub dirty_rows: Vec<bool>, // len == rows; true = row changed since last take
    pub zerowidth: Vec<Vec<(u16, String)>>, // per row: (col, combining marks); rare path
    pub cursor: Cursor,
    pub default_bg: Rgba,
    pub generation: u64, // bumps on every fill; renderer uses it to skip work
}
impl Frame {
    pub fn row(&self, r: u16) -> &[Cell] {
        let start = r as usize * self.cols as usize;
        &self.cells[start..start + self.cols as usize]
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty_rows.contains(&true)
    }
    pub fn clear_dirty(&mut self) {
        self.dirty_rows.fill(false);
    }
}

// ---- spike additions ----

impl Default for Frame {
    /// Empty frame; the first `fill_frame` sizes it.
    fn default() -> Self {
        let black = Rgba([0, 0, 0, 255]);
        Frame {
            cols: 0,
            rows: 0,
            cells: Vec::new(),
            dirty_rows: Vec::new(),
            zerowidth: Vec::new(),
            cursor: Cursor {
                col: 0,
                row: 0,
                shape: CursorShape::Hidden,
                color: black,
                blinking: false,
            },
            default_bg: black,
            generation: 0,
        }
    }
}

fn rgba(c: Rgb) -> Rgba {
    Rgba([c.r, c.g, c.b, 255])
}

/// Alacritty's dim factor (2/3), used when no explicit dim color is set.
fn dim(c: Rgba) -> Rgba {
    let d = |v: u8| (v as u16 * 2 / 3) as u8;
    Rgba([d(c.0[0]), d(c.0[1]), d(c.0[2]), c.0[3]])
}

// shortcut: the palette carries no fg/bg/cursor slots, so default fg = palette[7],
// bg = palette[0], bright fg = palette[15]. T06 adds real defaults from config.
fn named(colors: &Colors, palette: &[Rgba; 256], n: NamedColor) -> Rgba {
    if let Some(c) = colors[n] {
        return rgba(c);
    }
    let i = n as usize;
    match n {
        _ if i < 16 => palette[i],
        NamedColor::Foreground | NamedColor::Cursor => palette[7],
        NamedColor::Background => palette[0],
        NamedColor::BrightForeground => palette[15],
        NamedColor::DimForeground => dim(palette[7]),
        // DimBlack..=DimWhite: dim of the matching 0-7 color.
        _ => dim(named(
            colors,
            palette,
            base(i - NamedColor::DimBlack as usize),
        )),
    }
}

/// NamedColor for ANSI index 0-7.
fn base(i: usize) -> NamedColor {
    use NamedColor::*;
    [Black, Red, Green, Yellow, Blue, Magenta, Cyan, White][i]
}

/// Resolve a cell color. `bold`/`dim` only apply to fg (alacritty semantics).
fn resolve(colors: &Colors, palette: &[Rgba; 256], c: Color, bold: bool, dimmed: bool) -> Rgba {
    match c {
        Color::Spec(rgb) if dimmed => dim(rgba(rgb)),
        Color::Spec(rgb) => rgba(rgb),
        Color::Named(n) if bold && !dimmed => named(colors, palette, n.to_bright()),
        Color::Named(n) if dimmed && !bold => named(colors, palette, n.to_dim()),
        Color::Named(n) => named(colors, palette, n),
        Color::Indexed(i) if i < 8 && bold && !dimmed => resolve_indexed(colors, palette, i + 8),
        Color::Indexed(i) if i < 8 && dimmed && !bold => {
            named(colors, palette, base(i as usize).to_dim())
        }
        Color::Indexed(i) if dimmed && !bold => dim(resolve_indexed(colors, palette, i)),
        Color::Indexed(i) => resolve_indexed(colors, palette, i),
    }
}

fn resolve_indexed(colors: &Colors, palette: &[Rgba; 256], i: u8) -> Rgba {
    colors[i as usize].map_or(palette[i as usize], rgba)
}

fn fill_row<T>(term: &Term<T>, frame: &mut Frame, palette: &[Rgba; 256], r: usize) {
    let offset = term.grid().display_offset();
    let src = &term.grid()[Line(r as i32 - offset as i32)][..];
    let cols = frame.cols as usize;
    let dst = &mut frame.cells[r * cols..(r + 1) * cols];
    let colors = term.colors();
    frame.zerowidth[r].clear();
    for (col, (d, s)) in dst.iter_mut().zip(src).enumerate() {
        let f = s.flags;
        let bold = f.contains(Flags::BOLD);
        let dimmed = f.contains(Flags::DIM);
        let mut fg = resolve(colors, palette, s.fg, bold, dimmed);
        let mut bg = resolve(colors, palette, s.bg, false, false);
        if f.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let mut flags = 0;
        for (from, to) in [
            (Flags::BOLD, CellFlags::BOLD),
            (Flags::ITALIC, CellFlags::ITALIC),
            // §3.2 has no dotted/dashed: degrade to plain underline.
            (
                Flags::UNDERLINE | Flags::DOTTED_UNDERLINE | Flags::DASHED_UNDERLINE,
                CellFlags::UNDERLINE,
            ),
            (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
            (Flags::UNDERCURL, CellFlags::UNDERCURL),
            (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
            (Flags::WIDE_CHAR, CellFlags::WIDE),
            (Flags::WIDE_CHAR_SPACER, CellFlags::WIDE_SPACER),
            (Flags::HIDDEN, CellFlags::HIDDEN),
        ] {
            if f.intersects(from) {
                flags |= to;
            }
        }
        let mut extra = 0;
        if let Some(zw) = s.zerowidth() {
            // shortcut: allocates a String per combining-mark cell; rare. Upgrade to a
            // per-frame char arena if T11 shows combining-heavy output allocating.
            frame.zerowidth[r].push((col as u16, zw.iter().collect()));
            extra = 1;
        }
        // Tabs are stored as '\t' for copy; leading-wide spacers render blank.
        let ch = if s.c == '\t' || f.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            ' '
        } else {
            s.c
        };
        *d = Cell {
            ch,
            fg,
            bg,
            flags: CellFlags(flags),
            extra,
        };
    }
    frame.dirty_rows[r] = true;
}

/// Copy damaged viewport rows of `term` into `frame`. Allocation-free unless the
/// grid size changed or a cell carries combining marks.
pub fn fill_frame<T: EventListener>(term: &mut Term<T>, frame: &mut Frame, palette: &[Rgba; 256]) {
    let (cols, rows) = (term.columns(), term.screen_lines());
    let resized = frame.cols as usize != cols || frame.rows as usize != rows;
    if resized {
        let blank = Cell {
            ch: ' ',
            fg: palette[7],
            bg: palette[0],
            flags: CellFlags(0),
            extra: 0,
        };
        frame.cols = cols as u16;
        frame.rows = rows as u16;
        frame.cells = vec![blank; cols * rows];
        frame.dirty_rows = vec![false; rows];
        frame.zerowidth = vec![Vec::new(); rows];
    }
    // Collect damage into dirty_rows first: the iterator borrows `term` mutably.
    match term.damage() {
        TermDamage::Full => frame.dirty_rows.fill(true),
        TermDamage::Partial(_) if resized => frame.dirty_rows.fill(true),
        TermDamage::Partial(lines) => lines.for_each(|l| frame.dirty_rows[l.line] = true),
    }
    term.reset_damage();
    for r in 0..rows {
        if frame.dirty_rows[r] {
            fill_row(term, frame, palette, r);
        }
    }

    let colors = term.colors();
    frame.default_bg = named(colors, palette, NamedColor::Background);
    let style = term.cursor_style();
    let content = term.renderable_content();
    let row = content.cursor.point.line.0 + content.display_offset as i32;
    let visible = term.mode().contains(TermMode::SHOW_CURSOR) && (0..rows as i32).contains(&row);
    frame.cursor = Cursor {
        col: content.cursor.point.column.0 as u16,
        row: row.max(0) as u16,
        shape: match content.cursor.shape {
            _ if !visible => CursorShape::Hidden,
            ansi::CursorShape::Block => CursorShape::Block,
            ansi::CursorShape::Underline => CursorShape::Underline,
            ansi::CursorShape::Beam => CursorShape::Beam,
            ansi::CursorShape::HollowBlock => CursorShape::HollowBlock,
            ansi::CursorShape::Hidden => CursorShape::Hidden,
        },
        color: named(colors, palette, NamedColor::Cursor),
        blinking: style.blinking,
    };
    frame.generation += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    fn palette() -> [Rgba; 256] {
        std::array::from_fn(|i| Rgba([i as u8, 0, 0, 255]))
    }

    fn feed(bytes: &[u8]) -> Frame {
        let mut term = Term::new(Config::default(), &TermSize::new(10, 3), VoidListener);
        Processor::<ansi::StdSyncHandler>::new().advance(&mut term, bytes);
        let mut frame = Frame::default();
        fill_frame(&mut term, &mut frame, &palette());
        frame
    }

    #[test]
    fn red_a_uses_palette_1() {
        let f = feed(b"\x1b[31mA\x1b[0m");
        let c = f.row(0)[0];
        assert_eq!(
            (c.ch, c.fg, c.flags, c.extra),
            ('A', palette()[1], CellFlags(0), 0)
        );
    }

    #[test]
    fn cjk_is_wide_plus_spacer() {
        let f = feed("中".as_bytes());
        assert_eq!(
            (f.row(0)[0].ch, f.row(0)[0].flags),
            ('中', CellFlags(CellFlags::WIDE))
        );
        assert_eq!(f.row(0)[1].flags, CellFlags(CellFlags::WIDE_SPACER));
    }

    #[test]
    fn bold_red_is_bright_and_inverse_swaps() {
        let f = feed(b"\x1b[1;31mA\x1b[0;7;32mB");
        assert_eq!(f.row(0)[0].fg, palette()[9]);
        assert_eq!(
            (f.row(0)[1].fg, f.row(0)[1].bg),
            (palette()[0], palette()[2])
        );
    }

    #[test]
    fn combining_mark_goes_to_zerowidth() {
        let f = feed("e\u{301}".as_bytes());
        assert_eq!(f.row(0)[0].extra, 1);
        assert_eq!(f.zerowidth[0], vec![(0, "\u{301}".to_string())]);
    }

    #[test]
    fn display_offset_shows_scrollback() {
        let mut term = Term::new(Config::default(), &TermSize::new(10, 3), VoidListener);
        Processor::<ansi::StdSyncHandler>::new().advance(&mut term, b"1\r\n2\r\n3\r\n4");
        let mut frame = Frame::default();
        fill_frame(&mut term, &mut frame, &palette());
        assert_eq!(frame.row(0)[0].ch, '2');
        term.scroll_display(alacritty_terminal::grid::Scroll::Delta(1));
        fill_frame(&mut term, &mut frame, &palette());
        assert_eq!((frame.row(0)[0].ch, frame.row(2)[0].ch), ('1', '3'));
        assert!(
            matches!(frame.cursor.shape, CursorShape::Hidden),
            "cursor scrolled off"
        );
    }
}
