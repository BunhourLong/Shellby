//! Counting-allocator proof that nt-render's CPU hot paths do not allocate in steady state
//! (T07 R-12). One test only: the counters are thread-local and armed by it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as ThreadCell;
use std::path::Path;

use nt_render::__test::{Font, GlyphCache, GridLayer, encode_ui};
use nt_render::{FontConfig, PxRect, UiLayer};
use nt_term::{Cell, CellFlags, Cursor, CursorShape, Frame, Rgba};

struct Counters {
    armed: ThreadCell<bool>,
    allocations: ThreadCell<u64>,
}

thread_local! {
    static COUNTERS: Counters = const {
        Counters {
            armed: ThreadCell::new(false),
            allocations: ThreadCell::new(0),
        }
    };
}

fn record_allocation() {
    let _ = COUNTERS.try_with(|counters| {
        if counters.armed.get() {
            counters
                .allocations
                .set(counters.allocations.get().wrapping_add(1));
        }
    });
}

fn set_armed(armed: bool) {
    COUNTERS.with(|counters| counters.armed.set(armed));
}

fn allocations() -> u64 {
    COUNTERS.with(|counters| counters.allocations.get())
}

struct CountingAlloc;

// SAFETY: delegates every operation to `System`, so it satisfies GlobalAlloc under
// exactly the same preconditions as the system allocator.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: forwards the caller-provided valid layout to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: forwards the caller-provided valid layout to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: `ptr`/`layout` come from this allocator; `new_size` is the caller's request.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` come from this allocator and are freed exactly once.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

const COLS: u16 = 200;
const ROWS: u16 = 60;
// ASCII plus built-in box/block chars, all in the fixture font or `builtin::covers`.
const CHARS: [char; 16] = [
    'A', 'b', 'C', 'd', '0', '1', '-', '+', '#', '@', '─', '│', '┼', '█', '▀', '▄',
];

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u16) -> u16 {
        u16::try_from(self.next() % u32::from(n)).unwrap_or(0)
    }
}

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

fn make_frame() -> Frame {
    let bg = Rgba([0, 0, 0, 255]);
    let fg = Rgba([230, 230, 230, 255]);
    let len = usize::from(COLS) * usize::from(ROWS);
    let mut cells = Vec::with_capacity(len);
    for i in 0..len {
        cells.push(Cell {
            ch: CHARS[i % CHARS.len()],
            fg,
            bg,
            underline: fg,
            flags: CellFlags(0),
            extra: 0,
        });
    }
    Frame {
        cols: COLS,
        rows: ROWS,
        cells,
        dirty_rows: vec![true; usize::from(ROWS)],
        zerowidth: vec![Vec::new(); usize::from(ROWS)],
        cursor: Cursor {
            col: 0,
            row: 0,
            shape: CursorShape::Block,
            color: Rgba([255, 255, 255, 255]),
            blinking: false,
        },
        default_bg: bg,
        generation: 0,
    }
}

fn fill_ui(ui: &mut UiLayer) {
    ui.clear();
    for i in 0..50u32 {
        ui.rect(
            PxRect {
                x: i * 10,
                y: i * 5,
                w: 40,
                h: 20,
            },
            Rgba([1, 2, 3, 255]),
        );
    }
    for i in 0..10u32 {
        ui.text(
            i * 20,
            i * 10,
            "Hello 0123",
            Rgba([9, 9, 9, 255]),
            false,
            PxRect {
                x: 0,
                y: 0,
                w: 400,
                h: 200,
            },
        );
    }
}

#[test]
fn hot_paths_do_not_allocate() {
    let mut glyphs = cache();
    let mut frame = make_frame();
    let metrics = glyphs.metrics();
    let viewport = (
        u32::from(COLS) * u32::from(metrics.cell_w),
        u32::from(ROWS) * u32::from(metrics.cell_h),
    );
    let mut layer = GridLayer::new();

    let _ = layer.encode(&frame, viewport, true, true, &mut glyphs);
    frame.clear_dirty();
    frame.dirty_rows[0] = true;
    frame.cursor.col = 1;
    let _ = layer.encode(&frame, viewport, true, true, &mut glyphs);
    frame.clear_dirty();

    let mut ui = UiLayer::default();
    let mut ui_out = Vec::new();
    fill_ui(&mut ui);
    encode_ui(&ui, &mut glyphs, &mut ui_out);
    encode_ui(&ui, &mut glyphs, &mut ui_out);

    let mut rng = Rng(0x9E37_79B9);
    let start = allocations();
    set_armed(true);
    for _ in 0..100 {
        let count = rng.below(5) + 1;
        for _ in 0..count {
            let row = rng.below(ROWS);
            let col = rng.below(COLS);
            frame.dirty_rows[usize::from(row)] = true;
            let idx = usize::from(row) * usize::from(COLS) + usize::from(col);
            frame.cells[idx].ch = CHARS[usize::from(rng.below(16))];
        }
        frame.cursor.col = rng.below(COLS);
        frame.cursor.row = rng.below(ROWS);
        let _ = layer.encode(&frame, viewport, true, true, &mut glyphs);
        frame.clear_dirty();
    }
    let frame_allocs = allocations() - start;
    set_armed(false);
    assert_eq!(frame_allocs, 0, "frame encode allocated");

    let start = allocations();
    set_armed(true);
    fill_ui(&mut ui);
    encode_ui(&ui, &mut glyphs, &mut ui_out);
    let ui_allocs = allocations() - start;
    set_armed(false);
    assert_eq!(ui_allocs, 0, "UI encode allocated");
}
