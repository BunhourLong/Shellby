//! Render input (00-architecture §3.2): resolved colors only, reused across frames.

/// An opaque 8-bit RGBA color; alpha is always 255 when filled by `nt-term`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Rgba(pub [u8; 4]);

/// Cell attribute bits; plain `u16` constants (no bitflags crate).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct CellFlags(pub u16);

impl CellFlags {
    /// Bold weight.
    pub const BOLD: u16 = 1 << 0;
    /// Italic style.
    pub const ITALIC: u16 = 1 << 1;
    /// Single underline.
    pub const UNDERLINE: u16 = 1 << 2;
    /// Double underline.
    pub const DOUBLE_UNDERLINE: u16 = 1 << 3;
    /// Curly underline.
    pub const UNDERCURL: u16 = 1 << 4;
    /// Strike-through.
    pub const STRIKEOUT: u16 = 1 << 5;
    /// First half of a 2-column char.
    pub const WIDE: u16 = 1 << 6;
    /// Second half of a 2-column char; the renderer draws no glyph.
    pub const WIDE_SPACER: u16 = 1 << 7;
    /// Concealed text (fg already equals bg).
    pub const HIDDEN: u16 = 1 << 8;
    /// Dotted underline (D-T02-2).
    pub const DOTTED_UNDERLINE: u16 = 1 << 9;
    /// Dashed underline (D-T02-2).
    pub const DASHED_UNDERLINE: u16 = 1 << 10;

    /// True if every bit of `bits` is set.
    pub fn contains(self, bits: u16) -> bool {
        self.0 & bits == bits
    }
}

/// One grid cell, fully resolved. 20 bytes, `Copy`, no heap.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Cell {
    /// The base character; `' '` for empty.
    pub ch: char,
    /// Foreground color.
    pub fg: Rgba,
    /// Background color; equals `Frame::default_bg` when the renderer may skip the bg quad.
    pub bg: Rgba,
    /// Underline color (SGR 58); equals `fg` when unset.
    pub underline: Rgba,
    /// `CellFlags` bits.
    pub flags: CellFlags,
    /// 1 = this cell has combining marks in `Frame::zerowidth[row]`; 0 = none.
    pub extra: u16,
}

/// How the cursor is drawn.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CursorShape {
    /// Filled block.
    Block,
    /// Underline bar.
    Underline,
    /// Vertical beam.
    Beam,
    /// Outline block (unfocused).
    HollowBlock,
    /// Not drawn.
    Hidden,
}

/// Cursor state of a frame, in visible-screen coordinates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Column, 0-based.
    pub col: u16,
    /// Visible row, 0-based.
    pub row: u16,
    /// Shape; `Hidden` when not on screen or hidden by the program.
    pub shape: CursorShape,
    /// Cursor color.
    pub color: Rgba,
    /// Whether the program asked for a blinking cursor.
    pub blinking: bool,
}

/// What is on screen, filled by `Terminal::fill_frame` and owned by the renderer side.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Row-major cells, `len == cols * rows`, reused across frames.
    pub cells: Vec<Cell>,
    /// `len == rows`; true = row changed since the renderer last called `clear_dirty`.
    pub dirty_rows: Vec<bool>,
    /// Per row: `(col, combining marks)` for cells with `extra != 0`.
    pub zerowidth: Vec<Vec<(u16, String)>>,
    /// Cursor.
    pub cursor: Cursor,
    /// Background of empty cells.
    pub default_bg: Rgba,
    /// Bumps on every fill.
    pub generation: u64,
}

impl Default for Frame {
    /// An empty 0x0 frame; the first `fill_frame` sizes it.
    fn default() -> Self {
        let black = Rgba([0, 0, 0, 255]);
        Self {
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

impl Frame {
    /// The cells of visible row `r`. Panics if `r >= rows`.
    pub fn row(&self, r: u16) -> &[Cell] {
        let cols = self.cols as usize;
        let start = r as usize * cols;
        &self.cells[start..start + cols]
    }

    /// True if any row is dirty.
    pub fn is_dirty(&self) -> bool {
        self.dirty_rows.contains(&true)
    }

    /// Marks every row clean; the renderer calls this after uploading.
    pub fn clear_dirty(&mut self) {
        self.dirty_rows.fill(false);
    }
}
