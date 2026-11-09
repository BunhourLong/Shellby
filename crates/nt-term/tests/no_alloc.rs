//! Counting-allocator proof that the terminal hot paths do not allocate in steady
//! state (T06 A-1..A-3). One test only: the counters are thread-local and armed by it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use nt_term::{Frame, TermConfig, TermEvent, TermSize, Terminal};

struct Counters {
    armed: Cell<bool>,
    allocations: Cell<u64>,
}

thread_local! {
    static COUNTERS: Counters = const {
        Counters {
            armed: Cell::new(false),
            allocations: Cell::new(0),
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

const WORKLOAD: &[u8] = b"plain \x1b[1;32mgreen\x1b[0m text\r\n\
\x1b[10;5Hpositioned\r\n\
scroll a\r\nscroll b\r\nscroll c\r\nscroll d\r\n\
\x1b[3A\x1b[4B\x1b[0m\r\n";

fn term_size() -> TermSize {
    TermSize {
        cols: 80,
        rows: 24,
        cell_w_px: 8,
        cell_h_px: 16,
    }
}

fn term_config() -> TermConfig {
    TermConfig {
        scrollback_lines: 1000,
        hostname: "testhost".into(),
        ..TermConfig::default()
    }
}

#[test]
fn hot_paths_do_not_allocate() {
    let mut term = Terminal::new(term_size(), &term_config());
    let mut events: Vec<TermEvent> = Vec::new();
    let mut frame = Frame::default();

    for _ in 0..1_200 {
        term.advance(b"warm up line\r\n", &mut events);
        events.clear();
    }
    term.fill_frame(&mut frame);

    term.advance(WORKLOAD, &mut events);
    events.clear();
    term.fill_frame(&mut frame);

    events.reserve(64);

    let start = allocations();
    set_armed(true);

    for _ in 0..100 {
        term.advance(WORKLOAD, &mut events);
        events.clear();
    }
    let after_a1 = allocations();

    term.fill_frame(&mut frame);
    let after_a2 = allocations();

    for _ in 0..100 {
        term.advance(WORKLOAD, &mut events);
        events.clear();
    }
    term.fill_frame(&mut frame);
    let after_a3 = allocations();

    set_armed(false);

    assert_eq!(after_a1 - start, 0, "A-1: advance allocated");
    assert_eq!(after_a2 - after_a1, 0, "A-2: fill_frame allocated");
    assert_eq!(after_a3 - after_a2, 0, "A-3: reused events/frame allocated");
}
