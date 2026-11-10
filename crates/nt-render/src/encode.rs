//! CPU encoding of frames and the UI layer into GPU instance arrays. Pure CPU, no wgpu types.
//!
//! One instance type and one pipeline draw everything: a quad of `uv[2] × uv[3]` px at
//! `origin + xy`, filled solid, from the mask page, or from the color page (by `flags`).
//! Zero-size quads (`uv[2] == 0 || uv[3] == 0`) draw nothing.
//!
//! Per-pane instance layout (`n = cols × rows`, `i = row × cols + col`), drawn in order:
//! ```text
//! [0]                         pane background: solid, viewport size, frame.default_bg
//! [1 + i]                     cell background (zero-size when bg == default_bg)
//! [1 + n + i]                 cell glyph
//! [1 + 2n + 2i + k]           decorations: k = 0 underline (any style), k = 1 strikeout
//! [1 + 4n, 1 + 4n + OVERLAY_CAP)  overlay: cursor rects, cursor glyph, zero-width marks
//! ```
//! A dirty row run maps to three contiguous ranges (bg, glyph, deco), so a partial upload
//! is three `write_buffer` calls per run.

use std::ops::Range;

use crate::UiLayer;
use crate::atlas::Page;
use crate::builtin::Deco;
use crate::font::Metrics;
use crate::glyphs::{GlyphCache, GlyphSlot, Style};
use nt_term::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};

/// Draw from the R8 mask page: `out = premul(color) × coverage`.
pub const FLAG_MASK: u32 = 0;
/// Draw from the RGBA8 premultiplied color page: `out = texel`.
pub const FLAG_COLOR: u32 = 1;
/// Solid fill: `out = premul(color)`; `uv[0..2]` ignored.
pub const FLAG_SOLID: u32 = 2;

/// Overlay capacity per pane: up to 4 cursor rects + 1 cursor glyph + zero-width marks.
/// `// shortcut:` marks past capacity are dropped.
pub const OVERLAY_CAP: usize = 72;

/// One quad. Pane-local px for grid layers, window px for the UI layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Inst {
    /// Top-left, px.
    pub xy: [i16; 2],
    /// Atlas texel rect `[x, y, w, h]`; `w × h` is also the quad size in px.
    pub uv: [u16; 4],
    /// sRGB bytes, straight alpha (the shader premultiplies).
    pub color: [u8; 4],
    /// `FLAG_MASK` | `FLAG_COLOR` | `FLAG_SOLID`.
    pub flags: u32,
}

const _: () = assert!(size_of::<Inst>() == 20);

/// Up to 16 half-open dirty row ranges `(start, end)`; more collapse to one `min..max`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct DirtyRuns {
    runs: [(u16, u16); 16],
    len: u8,
}

impl DirtyRuns {
    /// The runs, ascending, non-overlapping.
    pub fn as_slice(&self) -> &[(u16, u16)] {
        &self.runs[..self.len as usize]
    }
}

/// What one `GridLayer::encode` changed, so `gpu.rs` uploads just that.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Encoded {
    /// Everything changed (size, metrics, epoch, first encode): upload all instances.
    /// When set, the other fields may be anything.
    pub full: bool,
    /// Instance 0 (pane background) changed.
    pub head: bool,
    /// Dirty row runs whose bg/glyph/deco ranges changed.
    pub runs: DirtyRuns,
    /// The overlay range changed.
    pub overlay: bool,
}

impl Encoded {
    /// True if nothing needs uploading.
    pub fn is_empty(&self) -> bool {
        !self.full && !self.head && self.runs.len == 0 && !self.overlay
    }
}

/// One pane's retained instances (layout in the module doc).
#[derive(Debug)]
pub struct GridLayer {
    inst: Vec<Inst>,
    cols: u16,
    rows: u16,
    filled: bool,
    metrics: Metrics,
    epoch: u32,
    default_bg: Rgba,
    viewport: (u32, u32),
    cursor: Cursor,
    focused: bool,
    cursor_visible: bool,
}

impl Default for GridLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl GridLayer {
    /// Empty layer; the first `encode` is full.
    pub fn new() -> Self {
        Self {
            inst: vec![Inst::default(); 1 + OVERLAY_CAP],
            cols: 0,
            rows: 0,
            filled: false,
            metrics: Metrics::default(),
            epoch: 0,
            default_bg: Rgba::default(),
            viewport: (0, 0),
            cursor: Cursor {
                col: 0,
                row: 0,
                shape: CursorShape::Hidden,
                color: Rgba::default(),
                blinking: false,
            },
            focused: false,
            cursor_visible: false,
        }
    }

    /// All instances, layout per the module doc; `len == 1 + 4n + OVERLAY_CAP`.
    pub fn instances(&self) -> &[Inst] {
        &self.inst
    }

    /// Encodes `frame`'s dirty rows (all rows if the grid size, metrics or glyph epoch
    /// changed) and, when needed, the overlay (cursor moved/changed shape or color, focus
    /// or visibility changed, the cursor's row or a row with zero-width marks is dirty).
    /// `viewport_wh` sizes the pane background. Does not clear `frame`'s dirty rows.
    /// If the glyph epoch changes during the call (atlas cleared), re-encodes fully once.
    /// No allocation unless the grid size changed or a glyph cache miss occurs.
    pub fn encode(
        &mut self,
        frame: &Frame,
        viewport_wh: (u32, u32),
        focused: bool,
        cursor_visible: bool,
        glyphs: &mut GlyphCache,
    ) -> Encoded {
        let epoch_before = glyphs.epoch();
        let full = self.needs_full(frame, glyphs.metrics(), epoch_before);
        let mut encoded =
            self.encode_pass(frame, viewport_wh, focused, cursor_visible, glyphs, full);
        if glyphs.epoch() != epoch_before {
            encoded = self.encode_pass(frame, viewport_wh, focused, cursor_visible, glyphs, true);
        }
        encoded
    }

    /// Instance index ranges `[bg, glyph, deco]` for the half-open row run `rows`.
    pub fn row_ranges(&self, rows: (u16, u16)) -> [Range<usize>; 3] {
        let cols = usize::from(self.cols);
        let n = cols * usize::from(self.rows);
        let start = usize::from(rows.0) * cols;
        let end = usize::from(rows.1) * cols;
        [
            1 + start..1 + end,
            1 + n + start..1 + n + end,
            1 + 2 * n + 2 * start..1 + 2 * n + 2 * end,
        ]
    }

    /// Instance index range of the overlay.
    pub fn overlay_range(&self) -> Range<usize> {
        let n = usize::from(self.cols) * usize::from(self.rows);
        let start = 1 + 4 * n;
        start..start + OVERLAY_CAP
    }

    fn needs_full(&self, frame: &Frame, metrics: Metrics, epoch: u32) -> bool {
        !self.filled
            || self.cols != frame.cols
            || self.rows != frame.rows
            || self.metrics != metrics
            || self.epoch != epoch
            || self.default_bg != frame.default_bg
    }

    fn encode_pass(
        &mut self,
        frame: &Frame,
        viewport_wh: (u32, u32),
        focused: bool,
        cursor_visible: bool,
        glyphs: &mut GlyphCache,
        full: bool,
    ) -> Encoded {
        let cols = frame.cols;
        let rows = frame.rows;
        let n = usize::from(cols) * usize::from(rows);
        let metrics = glyphs.metrics();
        self.cols = cols;
        self.rows = rows;

        let need = 1 + 4 * n + OVERLAY_CAP;
        if full {
            if self.inst.len() != need {
                self.inst.clear();
                self.inst.resize(need, Inst::default());
            }
        } else {
            debug_assert_eq!(self.inst.len(), need);
        }

        let runs = if full {
            DirtyRuns::default()
        } else {
            build_runs(&frame.dirty_rows)
        };

        let head = full || self.viewport != viewport_wh || self.default_bg != frame.default_bg;
        self.inst[0] = Inst {
            xy: [0, 0],
            uv: [
                0,
                0,
                clamp_u16_from_u32(viewport_wh.0),
                clamp_u16_from_u32(viewport_wh.1),
            ],
            color: frame.default_bg.0,
            flags: FLAG_SOLID,
        };

        if full {
            for row in 0..rows {
                self.encode_row(frame, row, metrics, glyphs);
            }
        } else {
            for &(start, end) in runs.as_slice() {
                for row in start..end {
                    self.encode_row(frame, row, metrics, glyphs);
                }
            }
        }

        let any_dirty = full || !runs.as_slice().is_empty();
        let overlay = any_dirty
            || frame.cursor != self.cursor
            || focused != self.focused
            || cursor_visible != self.cursor_visible;
        if overlay {
            let range = self.overlay_range();
            for inst in &mut self.inst[range] {
                *inst = Inst::default();
            }
            self.encode_overlay(frame, focused, cursor_visible, metrics, glyphs);
        }

        self.metrics = metrics;
        self.epoch = glyphs.epoch();
        self.default_bg = frame.default_bg;
        self.viewport = viewport_wh;
        self.cursor = frame.cursor;
        self.focused = focused;
        self.cursor_visible = cursor_visible;
        self.filled = true;

        Encoded {
            full,
            head,
            runs: if full { DirtyRuns::default() } else { runs },
            overlay,
        }
    }

    fn encode_row(&mut self, frame: &Frame, row: u16, metrics: Metrics, glyphs: &mut GlyphCache) {
        let cols = self.cols;
        let n = usize::from(cols) * usize::from(self.rows);
        let cw = i32::from(metrics.cell_w);
        let ch = i32::from(metrics.cell_h);
        let underline_y = i32::from(metrics.underline_y);
        let strike_y = i32::from(metrics.strike_y);
        let thickness = i32::from(metrics.thickness);
        let y = i32::from(row).saturating_mul(ch);
        let base = usize::from(row) * usize::from(cols);
        let cells = frame.row(row);

        for (col, cell) in cells.iter().enumerate() {
            let i = base + col;
            let x = i32::try_from(col).unwrap_or(i32::MAX).saturating_mul(cw);

            self.inst[1 + i] = if cell.bg == frame.default_bg {
                Inst::default()
            } else {
                solid_inst(x, y, cw, ch, cell.bg)
            };

            let flags = cell.flags;
            self.inst[1 + n + i] = if cell.ch == ' '
                || flags.contains(CellFlags::WIDE_SPACER)
                || flags.contains(CellFlags::HIDDEN)
            {
                Inst::default()
            } else {
                glyph_inst(
                    glyphs,
                    cell.ch,
                    Style::from_flags(flags),
                    flags.contains(CellFlags::WIDE),
                    x,
                    y,
                    cell.fg,
                )
                .unwrap_or_default()
            };

            self.inst[1 + 2 * n + 2 * i] = if flags.contains(CellFlags::HIDDEN) {
                Inst::default()
            } else if flags.contains(CellFlags::UNDERLINE) {
                solid_inst(
                    x,
                    y.saturating_add(underline_y),
                    cw,
                    thickness,
                    cell.underline,
                )
            } else {
                let kind = if flags.contains(CellFlags::DOUBLE_UNDERLINE) {
                    Some(Deco::Double)
                } else if flags.contains(CellFlags::UNDERCURL) {
                    Some(Deco::Undercurl)
                } else if flags.contains(CellFlags::DOTTED_UNDERLINE) {
                    Some(Deco::Dotted)
                } else if flags.contains(CellFlags::DASHED_UNDERLINE) {
                    Some(Deco::Dashed)
                } else {
                    None
                };
                match kind {
                    Some(kind) => glyphs.deco(kind).map_or_else(Inst::default, |slot| {
                        glyph_from_slot(&slot, x, y, cell.underline)
                    }),
                    None => Inst::default(),
                }
            };

            self.inst[1 + 2 * n + 2 * i + 1] =
                if flags.contains(CellFlags::STRIKEOUT) && !flags.contains(CellFlags::HIDDEN) {
                    solid_inst(x, y.saturating_add(strike_y), cw, thickness, cell.fg)
                } else {
                    Inst::default()
                };
        }
    }

    fn encode_overlay(
        &mut self,
        frame: &Frame,
        focused: bool,
        cursor_visible: bool,
        metrics: Metrics,
        glyphs: &mut GlyphCache,
    ) {
        let idx = self.encode_cursor(frame, focused, cursor_visible, metrics, glyphs);
        self.encode_marks(frame, glyphs, idx, metrics);
    }

    fn encode_cursor(
        &mut self,
        frame: &Frame,
        focused: bool,
        cursor_visible: bool,
        metrics: Metrics,
        glyphs: &mut GlyphCache,
    ) -> usize {
        let cols = self.cols;
        let n = usize::from(cols) * usize::from(self.rows);
        let start = 1 + 4 * n;
        let end = start + OVERLAY_CAP;
        let mut idx = start;
        let cw = i32::from(metrics.cell_w);
        let ch = i32::from(metrics.cell_h);
        let cursor = frame.cursor;

        if !cursor_visible
            || cursor.shape == CursorShape::Hidden
            || cursor.col >= cols
            || cursor.row >= self.rows
        {
            return idx;
        }

        let x = i32::from(cursor.col).saturating_mul(cw);
        let y = i32::from(cursor.row).saturating_mul(ch);
        let cell_idx = usize::from(cursor.row) * usize::from(cols) + usize::from(cursor.col);
        let cell = frame.cells[cell_idx];
        let cw2 = if cell.flags.contains(CellFlags::WIDE) {
            cw.saturating_mul(2)
        } else {
            cw
        };

        if cursor.shape == CursorShape::HollowBlock || !focused {
            let t = (cw.saturating_add(6) / 12).max(1);
            put(
                &mut self.inst,
                &mut idx,
                end,
                solid_inst(x, y, cw2, t, cursor.color),
            );
            put(
                &mut self.inst,
                &mut idx,
                end,
                solid_inst(
                    x,
                    y.saturating_add(ch).saturating_sub(t),
                    cw2,
                    t,
                    cursor.color,
                ),
            );
            put(
                &mut self.inst,
                &mut idx,
                end,
                solid_inst(x, y, t, ch, cursor.color),
            );
            put(
                &mut self.inst,
                &mut idx,
                end,
                solid_inst(
                    x.saturating_add(cw2).saturating_sub(t),
                    y,
                    t,
                    ch,
                    cursor.color,
                ),
            );
            return idx;
        }

        match cursor.shape {
            CursorShape::Block => {
                put(
                    &mut self.inst,
                    &mut idx,
                    end,
                    solid_inst(x, y, cw2, ch, cursor.color),
                );
                if let Some(glyph) = cursor_glyph(glyphs, &cell, x, y, cell.bg) {
                    put(&mut self.inst, &mut idx, end, glyph);
                }
            }
            CursorShape::Beam => {
                let w = (cw.saturating_add(4) / 8).max(1);
                put(
                    &mut self.inst,
                    &mut idx,
                    end,
                    solid_inst(x, y, w, ch, cursor.color),
                );
            }
            CursorShape::Underline => {
                let underline_y = i32::from(metrics.underline_y);
                let h = (i32::from(metrics.thickness) * 2).min(ch.saturating_sub(underline_y));
                put(
                    &mut self.inst,
                    &mut idx,
                    end,
                    solid_inst(x, y.saturating_add(underline_y), cw2, h, cursor.color),
                );
            }
            CursorShape::HollowBlock | CursorShape::Hidden => {}
        }
        idx
    }

    fn encode_marks(
        &mut self,
        frame: &Frame,
        glyphs: &mut GlyphCache,
        mut idx: usize,
        metrics: Metrics,
    ) {
        let cols = self.cols;
        let rows = self.rows;
        let end = self.overlay_range().end;
        let cw = i32::from(metrics.cell_w);
        let ch = i32::from(metrics.cell_h);

        for row in 0..rows {
            let marks = &frame.zerowidth[usize::from(row)];
            if marks.is_empty() {
                continue;
            }
            let y = i32::from(row).saturating_mul(ch);
            for &(col, ref s) in marks {
                if col >= cols {
                    continue;
                }
                let cell_idx = usize::from(row) * usize::from(cols) + usize::from(col);
                let cell = frame.cells[cell_idx];
                let style = Style::from_flags(cell.flags);
                let x = i32::from(col).saturating_mul(cw);
                for mark in s.chars() {
                    if let Some(glyph) = glyph_inst(glyphs, mark, style, false, x, y, cell.fg) {
                        put(&mut self.inst, &mut idx, end, glyph);
                    }
                }
            }
        }
    }
}

fn build_runs(dirty: &[bool]) -> DirtyRuns {
    let mut runs = [(0u16, 0u16); 16];
    let mut len = 0usize;
    let mut first = 0u16;
    let mut last = 0u16;
    let mut seen = false;
    let mut i = 0usize;
    while i < dirty.len() {
        if !dirty[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < dirty.len() && dirty[i] {
            i += 1;
        }
        let start = u16::try_from(start).unwrap_or(u16::MAX);
        let end = u16::try_from(i).unwrap_or(u16::MAX);
        if !seen {
            first = start;
            seen = true;
        }
        last = end;
        if len < runs.len() {
            runs[len] = (start, end);
        }
        len += 1;
    }

    let mut out = DirtyRuns::default();
    if seen {
        if len > runs.len() {
            out.runs[0] = (first, last);
            out.len = 1;
        } else {
            out.runs = runs;
            out.len = u8::try_from(len).unwrap_or(16);
        }
    }
    out
}

fn put(insts: &mut [Inst], idx: &mut usize, end: usize, inst: Inst) {
    if *idx >= end {
        return;
    }
    insts[*idx] = inst;
    *idx += 1;
}

fn solid_inst(x: i32, y: i32, w: i32, h: i32, color: Rgba) -> Inst {
    Inst {
        xy: [clamp_i16(x), clamp_i16(y)],
        uv: [0, 0, clamp_u16(w), clamp_u16(h)],
        color: color.0,
        flags: FLAG_SOLID,
    }
}

fn glyph_inst(
    glyphs: &mut GlyphCache,
    ch: char,
    style: Style,
    wide: bool,
    x: i32,
    y: i32,
    color: Rgba,
) -> Option<Inst> {
    glyphs
        .get(ch, style, wide)
        .map(|slot| glyph_from_slot(&slot, x, y, color))
}

fn glyph_from_slot(slot: &GlyphSlot, x: i32, y: i32, color: Rgba) -> Inst {
    Inst {
        xy: [
            clamp_i16(x.saturating_add(i32::from(slot.offset[0]))),
            clamp_i16(y.saturating_add(i32::from(slot.offset[1]))),
        ],
        uv: slot.uv,
        color: color.0,
        flags: if slot.page == Page::Color {
            FLAG_COLOR
        } else {
            FLAG_MASK
        },
    }
}

fn cursor_glyph(glyphs: &mut GlyphCache, cell: &Cell, x: i32, y: i32, color: Rgba) -> Option<Inst> {
    let flags = cell.flags;
    if cell.ch == ' ' || flags.contains(CellFlags::WIDE_SPACER) || flags.contains(CellFlags::HIDDEN)
    {
        return None;
    }
    glyph_inst(
        glyphs,
        cell.ch,
        Style::from_flags(flags),
        flags.contains(CellFlags::WIDE),
        x,
        y,
        color,
    )
}

// shortcut: pane-local px are i16, so content past 32767 px is clamped (8K at huge fonts).
fn clamp_i16(v: i32) -> i16 {
    i16::try_from(v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0)
}

fn clamp_u16(v: i32) -> u16 {
    u16::try_from(v.clamp(0, i32::from(u16::MAX))).unwrap_or(0)
}

fn clamp_u16_from_u32(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

fn clamp_i32_from_u32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// Encodes the UI layer into `out` (cleared first, capacity reused): rects in order, then
/// text glyphs (style Regular or Bold, one cell advance per char; `// shortcut:` no width table, so
/// wide chars get a 1-cell glyph), each
/// glyph quad trimmed to its run's `clip` by shifting `xy`/`uv`. Window px coordinates.
pub fn encode_ui(ui: &UiLayer, glyphs: &mut GlyphCache, out: &mut Vec<Inst>) {
    out.clear();

    for rect in &ui.rects {
        out.push(solid_inst(
            clamp_i32_from_u32(rect.rect.x),
            clamp_i32_from_u32(rect.rect.y),
            clamp_i32_from_u32(rect.rect.w),
            clamp_i32_from_u32(rect.rect.h),
            rect.color,
        ));
    }

    let cw = i32::from(glyphs.metrics().cell_w);
    for run in &ui.runs {
        let style = if run.bold {
            Style::Bold
        } else {
            Style::Regular
        };
        let text = &ui.text[run.start as usize..run.end as usize];
        let y = clamp_i32_from_u32(run.y);
        let clip_left = clamp_i32_from_u32(run.clip.x);
        let clip_top = clamp_i32_from_u32(run.clip.y);
        let clip_right = clip_left.saturating_add(clamp_i32_from_u32(run.clip.w));
        let clip_bottom = clip_top.saturating_add(clamp_i32_from_u32(run.clip.h));
        let mut pen = clamp_i32_from_u32(run.x);

        for ch in text.chars() {
            if let Some(slot) = glyphs.get(ch, style, false) {
                let qx = pen.saturating_add(i32::from(slot.offset[0]));
                let qy = y.saturating_add(i32::from(slot.offset[1]));
                let qw = i32::from(slot.uv[2]);
                let qh = i32::from(slot.uv[3]);
                let x0 = qx.max(clip_left);
                let y0 = qy.max(clip_top);
                let x1 = qx.saturating_add(qw).min(clip_right);
                let y1 = qy.saturating_add(qh).min(clip_bottom);
                if x1 > x0 && y1 > y0 {
                    let left = clamp_u16(x0 - qx);
                    let top = clamp_u16(y0 - qy);
                    out.push(Inst {
                        xy: [clamp_i16(x0), clamp_i16(y0)],
                        uv: [
                            slot.uv[0].saturating_add(left),
                            slot.uv[1].saturating_add(top),
                            clamp_u16(x1 - x0),
                            clamp_u16(y1 - y0),
                        ],
                        color: run.color.0,
                        flags: if slot.page == Page::Color {
                            FLAG_COLOR
                        } else {
                            FLAG_MASK
                        },
                    });
                }
            }
            pen = pen.saturating_add(cw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GridLayer, OVERLAY_CAP, build_runs};

    #[test]
    fn dirty_runs_empty() {
        assert_eq!(build_runs(&[]).as_slice().len(), 0);
        assert_eq!(build_runs(&[false, false, false]).as_slice().len(), 0);
    }

    #[test]
    fn dirty_runs_consecutive_and_separate() {
        assert_eq!(build_runs(&[true, true, true]).as_slice(), &[(0, 3)]);
        assert_eq!(
            build_runs(&[true, true, false, true]).as_slice(),
            &[(0, 2), (3, 4)]
        );
        assert_eq!(
            build_runs(&[false, true, false, true, true]).as_slice(),
            &[(1, 2), (3, 5)]
        );
    }

    #[test]
    fn dirty_runs_sixteen_stay_sixteen() {
        let mut dirty = vec![false; 32];
        for row in (0..32).step_by(2) {
            dirty[row] = true;
        }
        let runs = build_runs(&dirty);
        assert_eq!(runs.as_slice().len(), 16);
        assert_eq!(runs.as_slice()[0], (0, 1));
        assert_eq!(runs.as_slice()[15], (30, 31));
    }

    #[test]
    fn dirty_runs_more_than_sixteen_collapse() {
        let mut dirty = vec![false; 34];
        for row in (0..34).step_by(2) {
            dirty[row] = true;
        }
        let runs = build_runs(&dirty);
        assert_eq!(runs.as_slice(), &[(0, 33)]);
    }

    #[test]
    fn row_ranges_3x2() {
        let mut layer = GridLayer::new();
        layer.cols = 3;
        layer.rows = 2;

        assert_eq!(layer.row_ranges((0, 2)), [1..7, 7..13, 13..25]);
        assert_eq!(layer.row_ranges((1, 2)), [4..7, 10..13, 19..25]);
        assert_eq!(layer.overlay_range(), 25..25 + OVERLAY_CAP);
    }

    #[test]
    fn new_layer_has_head_and_overlay_slots() {
        let layer = GridLayer::new();
        assert_eq!(layer.instances().len(), 1 + OVERLAY_CAP);
    }
}
