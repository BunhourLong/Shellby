//! T02 Design §4/§5: fill_frame cost + reader/renderer lock contention.
//! Run pinned to two physical cores, e.g. `taskset -c 2,4 target/release/contention`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::*};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use t02_core::frame::{Frame, Rgba, fill_frame};

// ---- per-thread counting allocator ----

thread_local! {
    // const-initialized, no destructor: safe to touch from inside the allocator.
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}
fn bump() {
    let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
}
fn allocs() -> usize {
    ALLOCS.with(Cell::get)
}

struct Counting;
// SAFETY: every method forwards unchanged arguments to `System`, which upholds the
// GlobalAlloc contract; the only extra work is a thread-local counter bump that
// never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        bump();
        // SAFETY: caller's contract for `alloc` is passed straight through.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        bump();
        // SAFETY: caller's contract for `alloc_zeroed` is passed straight through.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        bump();
        // SAFETY: `p`/`l` came from this allocator, i.e. from `System`.
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p`/`l` came from this allocator, i.e. from `System`.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---- harness ----

type T = Term<VoidListener>;
const COLS: usize = 200;
const ROWS: usize = 60;
const RUN: Duration = Duration::from_secs(10);
const FRAME: Duration = Duration::from_nanos(8_333_333);

fn new_term() -> T {
    Term::new(Config::default(), &TermSize::new(COLS, ROWS), VoidListener)
}

trait Lock: Sync {
    fn new(t: T) -> Self;
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R;
}
impl Lock for FairMutex<T> {
    fn new(t: T) -> Self {
        FairMutex::new(t)
    }
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.lock())
    }
}
impl Lock for Mutex<T> {
    fn new(t: T) -> Self {
        Mutex::new(t)
    }
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.lock().unwrap())
    }
}

/// Architecture §2 reader: per chunk lock, advance, unlock, set wake. Returns MB/s.
fn reader(
    lock: &impl Lock,
    corpus: &[u8],
    chunk: usize,
    wake: &AtomicBool,
    stop: &AtomicBool,
) -> f64 {
    let mut parser = Processor::<StdSyncHandler>::new();
    let (start, mut bytes) = (Instant::now(), 0usize);
    'run: loop {
        for c in corpus.chunks(chunk) {
            if stop.load(Relaxed) {
                break 'run;
            }
            lock.with(|t| parser.advance(t, c));
            bytes += c.len();
            wake.store(true, Release);
        }
    }
    bytes as f64 / start.elapsed().as_secs_f64() / 1e6
}

fn pct(sorted: &[u32], p: usize) -> u32 {
    sorted[(sorted.len() - 1) * p / 100]
}

struct Row {
    reader_mbs: f64,
    alone_mbs: f64,
    wait: [u32; 3], // p50, p99, max (µs)
    hold_p99: u32,
    allocs: usize,       // inside fill_frame
    other_allocs: usize, // rest of the renderer loop (lock impl, etc.)
}

fn contention<L: Lock>(corpus: &[u8], chunk: usize, pal: &[Rgba; 256]) -> Row {
    let (wake, stop) = (AtomicBool::new(false), AtomicBool::new(false));

    // Reader alone.
    let lock = L::new(new_term());
    let alone_mbs = thread::scope(|s| {
        let h = s.spawn(|| reader(&lock, corpus, chunk, &wake, &stop));
        thread::sleep(RUN);
        stop.store(true, Relaxed);
        h.join().unwrap()
    });

    // Reader + 120 Hz renderer (this thread).
    stop.store(false, Relaxed);
    let lock = L::new(new_term());
    let mut frame = Frame::default();
    lock.with(|t| fill_frame(t, &mut frame, pal)); // warm-up: sizes the frame
    let cap = (RUN.as_nanos() / FRAME.as_nanos()) as usize * 2;
    let (mut waits, mut holds) = (Vec::with_capacity(cap), Vec::with_capacity(cap));
    let mut fill_allocs = 0;
    let (reader_mbs, loop_allocs) = thread::scope(|s| {
        let h = s.spawn(|| reader(&lock, corpus, chunk, &wake, &stop));
        let a0 = allocs();
        let start = Instant::now();
        let mut next = start;
        while next - start < RUN {
            next += FRAME;
            thread::sleep(next.saturating_duration_since(Instant::now()));
            if !wake.swap(false, Acquire) {
                continue;
            }
            let t0 = Instant::now();
            let t1 = lock.with(|t| {
                let t1 = Instant::now();
                let a = allocs();
                fill_frame(t, &mut frame, pal);
                fill_allocs += allocs() - a;
                t1
            });
            let t2 = Instant::now();
            assert!(waits.len() < cap, "sample buffer must not grow");
            waits.push((t1 - t0).as_micros() as u32);
            holds.push((t2 - t1).as_micros() as u32);
        }
        let allocs = allocs() - a0;
        stop.store(true, Relaxed);
        (h.join().unwrap(), allocs)
    });
    waits.sort_unstable();
    holds.sort_unstable();
    Row {
        reader_mbs,
        alone_mbs,
        wait: [pct(&waits, 50), pct(&waits, 99), *waits.last().unwrap()],
        hold_p99: pct(&holds, 99),
        allocs: fill_allocs,
        other_allocs: loop_allocs - fill_allocs,
    }
}

/// Median fill_frame time (µs) for full and one-line damage, plus allocs across both.
fn fill_cost(corpus: &[u8], pal: &[Rgba; 256]) -> (f64, f64, usize) {
    const N: usize = 2000;
    let mut term = new_term();
    let mut parser = Processor::<StdSyncHandler>::new();
    parser.advance(&mut term, &corpus[..corpus.len().min(4 << 20)]);
    let mut frame = Frame::default();
    fill_frame(&mut term, &mut frame, pal);
    let mut full = Vec::with_capacity(N);
    let mut line = Vec::with_capacity(N);
    let mut allocs_in_fill = 0;
    for i in 0..N {
        // Clear screen forces Full damage; then repaint with a slice of real content.
        // (dense_cells never scrolls, so scroll_display cannot force Full here.)
        let at = (i * (16 << 10)) % (corpus.len() - (16 << 10));
        parser.advance(&mut term, b"\x1b[2J");
        parser.advance(&mut term, &corpus[at..at + (16 << 10)]);
        frame.clear_dirty();
        let (t, a) = (Instant::now(), allocs());
        fill_frame(&mut term, &mut frame, pal);
        full.push(t.elapsed().as_nanos() as u64);
        allocs_in_fill += allocs() - a;
        assert!(frame.dirty_rows.iter().all(|&d| d), "expected full damage");
    }
    parser.advance(&mut term, b"\rX");
    fill_frame(&mut term, &mut frame, pal);
    for _ in 0..N {
        parser.advance(&mut term, b"\rX");
        frame.clear_dirty();
        let (t, a) = (Instant::now(), allocs());
        fill_frame(&mut term, &mut frame, pal);
        line.push(t.elapsed().as_nanos() as u64);
        allocs_in_fill += allocs() - a;
        assert_eq!(
            frame.dirty_rows.iter().filter(|&&d| d).count(),
            1,
            "expected one damaged line"
        );
    }
    let med = |v: &mut Vec<u64>| {
        v.sort_unstable();
        v[v.len() / 2] as f64 / 1e3
    };
    (med(&mut full), med(&mut line), allocs_in_fill)
}

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../corpus/dense_cells.bin");
    let corpus = std::fs::read(path).expect("read dense_cells.bin");
    // shortcut: synthetic palette (index in red channel); colors don't affect timing.
    let pal: [Rgba; 256] = std::array::from_fn(|i| Rgba([i as u8, 0, 0, 255]));

    let (fill_full_us, fill_one_line_us, fill_allocs) = fill_cost(&corpus, &pal);
    println!(
        "corpus_mib={:.1} cols={COLS} rows={ROWS} run_s={}",
        corpus.len() as f64 / (1 << 20) as f64,
        RUN.as_secs()
    );
    println!(
        "lock    chunk reader_mbs reader_mbs_without_renderer wait_p50_us wait_p99_us wait_max_us hold_p99_us  fill_full_us fill_one_line_us allocs_after_warmup verdict"
    );
    for chunk in [64 << 10, 16 << 10] {
        let mut all_pass = true;
        for (name, row) in [
            ("fair", contention::<FairMutex<T>>(&corpus, chunk, &pal)),
            ("std", contention::<Mutex<T>>(&corpus, chunk, &pal)),
        ] {
            let allocs = row.allocs + fill_allocs;
            let loss = 1.0 - row.reader_mbs / row.alone_mbs;
            // Design §6 lock rule: wait p99 <= 1 ms and reader loss <= 10%.
            let pass = row.wait[1] <= 1000 && loss <= 0.10;
            all_pass &= pass;
            println!(
                "{name:<6} {:>5}K {:>10.1} {:>27.1} {:>11} {:>11} {:>11} {:>11} {:>13.1} {:>16.2} {:>19} {} (loss {:.1}%, renderer_other_allocs {})",
                chunk >> 10,
                row.reader_mbs,
                row.alone_mbs,
                row.wait[0],
                row.wait[1],
                row.wait[2],
                row.hold_p99,
                fill_full_us,
                fill_one_line_us,
                allocs,
                if pass { "PASS" } else { "MISS" },
                loss * 100.0,
                row.other_allocs
            );
            assert_eq!(allocs, 0, "fill_frame allocated after warm-up");
        }
        // Design §6: re-measure at 16 KiB if any lock missed at 64 KiB.
        if all_pass || chunk == 16 << 10 {
            break;
        }
    }
}
