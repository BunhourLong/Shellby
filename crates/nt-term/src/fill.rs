//! Core grid → `Frame` (damage, cursor, scroll offset, wide chars, zero-width marks).

use crate::Terminal;
use crate::color;
use crate::frame::{Cell, CellFlags, Cursor, CursorShape, Frame};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::TermDamage;
use alacritty_terminal::term::cell::{Cell as CoreCell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::CursorShape as CoreCursorShape;

fn map_flags(flags: Flags) -> u16 {
    let mut bits = 0;
    for (source, target) in [
        (Flags::BOLD, CellFlags::BOLD),
        (Flags::ITALIC, CellFlags::ITALIC),
        (Flags::UNDERLINE, CellFlags::UNDERLINE),
        (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
        (Flags::UNDERCURL, CellFlags::UNDERCURL),
        (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
        (Flags::WIDE_CHAR, CellFlags::WIDE),
        (Flags::WIDE_CHAR_SPACER, CellFlags::WIDE_SPACER),
        (Flags::HIDDEN, CellFlags::HIDDEN),
        (Flags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
        (Flags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
    ] {
        if flags.contains(source) {
            bits |= target;
        }
    }
    bits
}

fn convert(
    source: &CoreCell,
    colors: &Colors,
    palette: &crate::Palette,
    bold_is_bright: bool,
) -> Cell {
    let bold = source.flags.contains(Flags::BOLD);
    let dimmed = source.flags.contains(Flags::DIM);
    let inverse = source.flags.contains(Flags::INVERSE);
    let hidden = source.flags.contains(Flags::HIDDEN);
    let (fg, bg) = color::resolve(
        colors,
        palette,
        source.fg,
        source.bg,
        bold,
        dimmed,
        inverse,
        hidden,
        bold_is_bright,
    );
    let underline = match source.underline_color() {
        Some(value) => color::resolve_plain(colors, palette, value),
        None => fg,
    };
    let leading_wide = source.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
    let flags = if leading_wide {
        0
    } else {
        map_flags(source.flags)
    };
    let ch = if source.c == '\t' || leading_wide {
        ' '
    } else {
        source.c
    };
    Cell {
        ch,
        fg,
        bg,
        underline,
        flags: CellFlags(flags),
        extra: 0,
    }
}

fn map_cursor_shape(shape: CoreCursorShape) -> CursorShape {
    match shape {
        CoreCursorShape::Block => CursorShape::Block,
        CoreCursorShape::Underline => CursorShape::Underline,
        CoreCursorShape::Beam => CursorShape::Beam,
        CoreCursorShape::HollowBlock => CursorShape::HollowBlock,
        CoreCursorShape::Hidden => CursorShape::Hidden,
    }
}

/// The only allocating path: size `frame` to the terminal, all cells blank.
fn resize_frame(frame: &mut Frame, term: &Terminal) {
    let (cols, rows) = (term.size.cols as usize, term.size.rows as usize);
    frame.cols = term.size.cols;
    frame.rows = term.size.rows;
    let blank = Cell {
        ch: ' ',
        fg: term.palette.fg,
        bg: term.palette.bg,
        underline: term.palette.fg,
        flags: CellFlags(0),
        extra: 0,
    };
    frame.cells.clear();
    frame.cells.resize(cols * rows, blank);
    frame.dirty_rows.clear();
    frame.dirty_rows.resize(rows, false);
    frame.zerowidth.clear();
    frame.zerowidth.resize_with(rows, Vec::new);
}

/// Copy damaged viewport rows into `frame`. Allocation-free unless the size
/// changed or a cell carries combining marks.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "rows/columns are u16-bounded grid dimensions"
)]
pub(crate) fn fill_frame(term: &mut Terminal, frame: &mut Frame) {
    let cols = term.size.cols as usize;
    let rows = term.size.rows as usize;

    if frame.cols as usize != cols || frame.rows as usize != rows {
        term.force_full = true;
        resize_frame(frame, term);
    }

    let mut force_all = term.force_full;
    match term.core.damage() {
        TermDamage::Full => force_all = true,
        TermDamage::Partial(lines) => {
            for bounds in lines {
                if let Some(slot) = frame.dirty_rows.get_mut(bounds.line) {
                    *slot = true;
                }
            }
        }
    }
    term.core.reset_damage();

    let (cursor_point, cursor_shape, display_offset) = {
        let content = term.core.renderable_content();
        (
            content.cursor.point,
            content.cursor.shape,
            content.display_offset,
        )
    };
    if display_offset != term.last_display_offset {
        force_all = true;
    }
    if force_all {
        frame.dirty_rows.fill(true);
    }

    let cursor_row = cursor_point.line.0 + display_offset as i32;
    if (0..rows as i32).contains(&cursor_row) {
        frame.dirty_rows[cursor_row as usize] = true;
    }
    if let Some(previous) = term.last_cursor_row
        && (previous as usize) < rows
    {
        frame.dirty_rows[previous as usize] = true;
    }

    {
        let colors = term.core.colors();
        let palette = &term.palette;
        let bold_is_bright = term.bold_is_bright;
        for row in 0..rows {
            if !frame.dirty_rows[row] {
                continue;
            }
            let line = Line(row as i32 - display_offset as i32);
            let source = &term.core.grid()[line][..];
            let start = row * cols;
            let destination = &mut frame.cells[start..start + cols];
            let zerowidth = &mut frame.zerowidth[row];
            zerowidth.clear();
            for (column, (target, origin)) in destination.iter_mut().zip(source.iter()).enumerate()
            {
                *target = convert(origin, colors, palette, bold_is_bright);
                if let Some(marks) = origin.zerowidth() {
                    zerowidth.push((column as u16, marks.iter().collect()));
                    target.extra = 1;
                }
            }
        }
    }

    let style = term.core.cursor_style();
    let visible = (0..rows as i32).contains(&cursor_row);
    let shape = if visible {
        map_cursor_shape(cursor_shape)
    } else {
        CursorShape::Hidden
    };
    let color = term.palette.cursor.unwrap_or_else(|| {
        let index = (cursor_row.max(0) as usize) * cols + cursor_point.column.0.min(cols - 1);
        frame
            .cells
            .get(index)
            .map_or(term.palette.fg, |cell| cell.fg)
    });
    frame.cursor = Cursor {
        col: if visible {
            cursor_point.column.0 as u16
        } else {
            0
        },
        row: if visible { cursor_row as u16 } else { 0 },
        shape,
        color,
        blinking: style.blinking,
    };

    term.last_cursor_row = if visible {
        Some(cursor_row as u16)
    } else {
        None
    };
    term.last_display_offset = display_offset;
    term.force_full = false;
    term.generation += 1;
    frame.generation = term.generation;
    frame.default_bg = term.palette.bg;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TermConfig, TermSize};

    #[test]
    fn red_a_resolves_and_damages_row() {
        let mut terminal = Terminal::new(
            TermSize {
                cols: 10,
                rows: 3,
                cell_w_px: 8,
                cell_h_px: 16,
            },
            &TermConfig::default(),
        );
        terminal.advance_core_only(b"\x1b[31mA");
        let mut frame = Frame::default();
        terminal.fill_frame(&mut frame);
        let cell = frame.row(0)[0];
        assert_eq!(cell.ch, 'A');
        assert_eq!(cell.fg, crate::Palette::default().ansi[1]);
        assert!(frame.dirty_rows[0]);
    }
}
