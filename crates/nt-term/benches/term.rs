//! nt-term benches, std only: `advance` vs core-only throughput per corpus
//! (`spikes/corpus/*.bin`, 64 KiB chunks, D-T02-1 method) and `fill_frame` latency.
//! Usage: `cargo bench -p nt-term --bench term [-- <corpus-name-filter>]`.
#![expect(clippy::print_stdout, reason = "bench report")]

use std::hint::black_box;
use std::time::{Duration, Instant};

use nt_term::{Frame, TermConfig, TermSize, Terminal};

const RUNS: usize = 5;
const CHUNK: usize = 64 * 1024;

fn term() -> Terminal {
    let size = TermSize {
        cols: 200,
        rows: 60,
        cell_w_px: 8,
        cell_h_px: 16,
    };
    Terminal::new(size, &TermConfig::default())
}

/// Median MB/s of `RUNS` fresh-terminal passes over `data`.
fn throughput(data: &[u8], full: bool) -> f64 {
    let mut rates: Vec<f64> = (0..RUNS)
        .map(|_| {
            let mut t = term();
            let mut events = Vec::new();
            let start = Instant::now();
            for chunk in data.chunks(CHUNK) {
                if full {
                    t.advance(chunk, &mut events);
                    events.clear();
                } else {
                    t.advance_core_only(chunk);
                }
            }
            black_box(&t);
            data.len() as f64 / start.elapsed().as_secs_f64() / 1e6
        })
        .collect();
    rates.sort_by(f64::total_cmp);
    rates[RUNS / 2]
}

/// (median, p99) of `n` timed `fill_frame` calls, each after an untimed `setup`.
fn fill_latency(n: usize, setup: &[u8]) -> (Duration, Duration) {
    let mut t = term();
    let mut events = Vec::new();
    let mut frame = Frame::default();
    t.fill_frame(&mut frame);
    let mut times: Vec<Duration> = (0..n)
        .map(|_| {
            t.advance(setup, &mut events);
            events.clear();
            frame.clear_dirty();
            let start = Instant::now();
            t.fill_frame(black_box(&mut frame));
            start.elapsed()
        })
        .collect();
    times.sort_unstable();
    (times[n / 2], times[n * 99 / 100])
}

fn main() {
    let filter = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .unwrap_or_default();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spikes/corpus");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|e| e == "bin"));
    files.retain(|p| {
        p.file_stem()
            .is_some_and(|s| s.to_string_lossy().contains(&filter))
    });
    files.sort();
    if files.is_empty() {
        println!("skip: no corpus in {dir}");
    }
    println!(
        "{:<30} {:>9} {:>9} {:>9}",
        "corpus", "core MB/s", "full MB/s", "full/core"
    );
    let (mut log_core, mut log_full) = (0.0, 0.0);
    for path in &files {
        let data = std::fs::read(path).expect("read corpus");
        let (core, full) = (throughput(&data, false), throughput(&data, true));
        // Gate (D-T02-1, T06): full ≥ 80% of core per corpus, ≥ 90% geomean.
        let ratio = full / core * 100.0;
        let verdict = if ratio >= 80.0 { "PASS" } else { "FAIL" };
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        println!("{name:<30} {core:>9.1} {full:>9.1} {ratio:>8.1}% {verdict}");
        log_core += core.ln();
        log_full += full.ln();
    }
    if !files.is_empty() {
        let n = files.len() as f64;
        let (core, full) = ((log_core / n).exp(), (log_full / n).exp());
        let ratio = full / core * 100.0;
        let verdict = if ratio >= 90.0 { "PASS" } else { "FAIL" };
        println!(
            "{:<30} {core:>9.1} {full:>9.1} {ratio:>8.1}% {verdict}",
            "geomean"
        );
    }

    // Full: every row rewritten; one row: a single cell changes.
    let mut screen = b"\x1b[H".to_vec();
    for row in 0..60usize {
        screen.extend((0..200usize).map(|c| b'a' + ((c + row) % 26) as u8));
    }
    for (label, n, setup, limit_us) in [
        ("fill full 200x60", 2_000, &screen[..], 100),
        ("fill 1 row", 20_000, b"\x1b[30;1Hx", 5),
    ] {
        let (median, p99) = fill_latency(n, setup);
        let verdict = if median.as_micros() <= limit_us {
            "PASS"
        } else {
            "FAIL"
        };
        println!("{label:<30} median {median:>9.2?} p99 {p99:>9.2?} (≤ {limit_us} µs) {verdict}");
    }
}
