//! Pure window-pixel to cell-grid math.

/// Terminal grid size in cells.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct GridSize {
    /// Columns, ≥ 1.
    pub(crate) cols: u16,
    /// Rows, ≥ 1.
    pub(crate) rows: u16,
}

/// Inner padding on each side, in physical pixels (already scaled).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Padding {
    /// Left and right, each.
    pub(crate) x: u32,
    /// Top and bottom, each.
    pub(crate) y: u32,
}

/// Cells that fit fully in the window. Never returns 0 in either dimension (min 1x1)
/// so the PTY and the grid never see a zero size; clamps to `u16::MAX`.
/// `cell_w` and `cell_h` must be ≥ 1 (debug-asserted; the renderer guarantees it).
pub(crate) fn grid_from_pixels(
    win_w: u32,
    win_h: u32,
    cell_w: u32,
    cell_h: u32,
    pad: Padding,
) -> GridSize {
    debug_assert!(cell_w >= 1 && cell_h >= 1);
    let cell_w = cell_w.max(1);
    let cell_h = cell_h.max(1);
    let cols = win_w.saturating_sub(pad.x.saturating_mul(2)) / cell_w;
    let rows = win_h.saturating_sub(pad.y.saturating_mul(2)) / cell_h;
    GridSize {
        cols: cols.clamp(1, u32::from(u16::MAX)) as u16,
        rows: rows.clamp(1, u32::from(u16::MAX)) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_PAD: Padding = Padding { x: 0, y: 0 };

    #[test]
    fn exact_fit_uses_whole_window() {
        assert_eq!(
            grid_from_pixels(800, 600, 8, 16, NO_PAD),
            GridSize {
                cols: 100,
                rows: 37
            }
        );
    }

    #[test]
    fn remainder_is_floored() {
        assert_eq!(
            grid_from_pixels(805, 603, 8, 16, NO_PAD),
            GridSize {
                cols: 100,
                rows: 37
            }
        );
    }

    #[test]
    fn padding_is_subtracted_on_both_sides() {
        let pad = Padding { x: 8, y: 8 };
        assert_eq!(
            grid_from_pixels(816, 632, 8, 16, pad),
            GridSize {
                cols: 100,
                rows: 38
            }
        );
    }

    #[test]
    fn padding_larger_than_window_clamps_to_one_cell() {
        let pad = Padding { x: 100, y: 100 };
        assert_eq!(
            grid_from_pixels(10, 10, 8, 16, pad),
            GridSize { cols: 1, rows: 1 }
        );
    }

    #[test]
    fn huge_window_clamps_to_u16_max() {
        assert_eq!(
            grid_from_pixels(u32::MAX, u32::MAX, 1, 1, NO_PAD),
            GridSize {
                cols: u16::MAX,
                rows: u16::MAX,
            }
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "cell_w >= 1")]
    fn zero_cell_size_panics_in_debug() {
        let _ = grid_from_pixels(800, 600, 0, 16, NO_PAD);
    }
}
