//! T02-S3: parser throughput harness + synthetic OSC corpus generator.
//!
//! Modes: vte-noop, vte-osc, alac, alac+A, alac+B, diff-ab. See .tasks/T02-spike-terminal-core.md §2/§3.

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte;
use alacritty_terminal::vte::Perform;
use t02_core::oscscan::OscScanner;

const MIB: usize = 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const SPLIT_CHUNK: usize = 4096;
const WARMUPS: usize = 2;
const MEASURED: usize = 10;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: wraps the system allocator and forwards every call unchanged; the only
// added work is a relaxed atomic increment, which is safe from any thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding the same layout to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: pointer/layout came from System via this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding the same layout to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: pointer/layout came from System via this allocator; new_size is valid.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---------------------------------------------------------------------------
// vte performers
// ---------------------------------------------------------------------------

struct NoopPerformer;

impl Perform for NoopPerformer {}

#[derive(Default)]
struct OscPerformer {
    events: u64,
    max_bytes: usize,
}

fn tracked_cmd(cmd: &[u8]) -> bool {
    cmd == &b"7"[..] || cmd == &b"9"[..] || cmd == &b"99"[..] || cmd == &b"777"[..]
}

impl Perform for OscPerformer {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(cmd) = params.first().copied() else {
            return;
        };
        if !tracked_cmd(cmd) {
            return;
        }
        self.events += 1;
        let mut len = 0usize;
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                len += 1;
            }
            len += p.len();
        }
        if len > self.max_bytes {
            self.max_bytes = len;
        }
    }
}

// ---------------------------------------------------------------------------
// Runners (fresh parser/Term built outside each timed region)
// ---------------------------------------------------------------------------

fn run_vte_noop(data: &[u8], chunk: usize) -> (f64, u64) {
    let mut parser: vte::Parser = vte::Parser::new();
    let mut performer = NoopPerformer;
    ALLOCS.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for c in data.chunks(chunk) {
        parser.advance(&mut performer, black_box(c));
    }
    let secs = start.elapsed().as_secs_f64();
    (secs, ALLOCS.load(Ordering::Relaxed) as u64)
}

fn run_vte_osc(data: &[u8], chunk: usize) -> (f64, u64, u64, usize) {
    let mut parser: vte::Parser = vte::Parser::new();
    let mut performer = OscPerformer::default();
    ALLOCS.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for c in data.chunks(chunk) {
        parser.advance(&mut performer, black_box(c));
    }
    let secs = start.elapsed().as_secs_f64();
    let allocs = ALLOCS.load(Ordering::Relaxed) as u64;
    (secs, allocs, performer.events, performer.max_bytes)
}

fn new_term() -> Term<VoidListener> {
    let config = Config {
        scrolling_history: 10000,
        ..Config::default()
    };
    let size = TermSize::new(200, 60);
    Term::new(config, &size, VoidListener)
}

/// Length vte reports for a tracked OSC body: the first MAX_OSC_PARAMS=16 ';'-fields rejoined
/// (vte lib.rs:529-530 drops the 17th field onward).
fn vte_view(raw: &[u8]) -> &[u8] {
    let end = raw
        .iter()
        .enumerate()
        .filter(|&(_, &b)| b == b';')
        .nth(15)
        .map_or(raw.len(), |(i, _)| i);
    &raw[..end]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OscMode {
    None,
    A,
    B,
}

fn run_alac(data: &[u8], chunk: usize, osc: OscMode) -> (f64, u64, Term<VoidListener>, u64, usize) {
    let mut term = new_term();
    let mut processor: vte::ansi::Processor = vte::ansi::Processor::new();
    let mut osc_parser: vte::Parser = vte::Parser::new();
    let mut osc_performer = OscPerformer::default();
    let mut scanner = OscScanner::new();
    let mut b_counts = OscPerformer::default();
    let mut on_osc = |raw: &[u8]| {
        b_counts.events += 1;
        b_counts.max_bytes = b_counts.max_bytes.max(vte_view(raw).len());
    };

    ALLOCS.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for c in data.chunks(chunk) {
        processor.advance(&mut term, c);
        if let Some(deadline) = processor.sync_timeout().sync_timeout() {
            if deadline <= Instant::now() {
                processor.stop_sync(&mut term);
            }
        }
        match osc {
            OscMode::None => {}
            OscMode::A => osc_parser.advance(&mut osc_performer, black_box(c)),
            OscMode::B => scanner.advance(black_box(c), &mut on_osc),
        }
    }
    if processor.sync_timeout().sync_timeout().is_some() {
        processor.stop_sync(&mut term);
    }
    let secs = start.elapsed().as_secs_f64();
    let allocs = ALLOCS.load(Ordering::Relaxed) as u64;
    let counts = if osc == OscMode::B {
        b_counts
    } else {
        osc_performer
    };
    (secs, allocs, term, counts.events, counts.max_bytes)
}

// ---------------------------------------------------------------------------
// diff-ab: A (vte Parser) and B (OscScanner) must report identical tracked OSCs
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OscRecorder(Vec<Vec<u8>>);

impl Perform for OscRecorder {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.first().is_some_and(|c| tracked_cmd(c)) {
            self.0.push(params.join(&b';'));
        }
    }
}

/// Feeds `data` in chunks of the given sizes (cycled) to A and B; Err describes the first divergence.
fn diff_ab(data: &[u8], sizes: &[usize]) -> Result<usize, String> {
    let mut parser: vte::Parser = vte::Parser::new();
    let mut a = OscRecorder::default();
    let mut scanner = OscScanner::new();
    let mut b: Vec<Vec<u8>> = Vec::new();
    let (mut off, mut k) = (0, 0);
    while off < data.len() {
        let end = (off + sizes[k % sizes.len()]).min(data.len());
        parser.advance(&mut a, &data[off..end]);
        scanner.advance(&data[off..end], &mut |raw: &[u8]| {
            b.push(vte_view(raw).to_vec())
        });
        off = end;
        k += 1;
    }
    match a.0.iter().zip(&b).position(|(x, y)| x != y) {
        _ if a.0 == b => Ok(a.0.len()),
        Some(i) => Err(format!("event {i}: A={:?} B={:?}", a.0[i], b[i])),
        None => Err(format!("count A={} B={}", a.0.len(), b.len())),
    }
}

/// Hand-written edge cases plus a seeded random stream over an escape-heavy alphabet.
fn adversarial() -> Vec<u8> {
    let mut v: Vec<u8> = Vec::new();
    let cases: &[&[u8]] = &[
        b"\x1b[1;2\x1b]7;csi-abort\x07",
        b"\x1bP1$q\x1b]9;dcs-abort\x1b\\",
        b"\x1bPq#0;2;0;0;0abc\x1b]99;passthrough\x07",
        b"\x1b_apc\x1b]777;notify;t;b\x07",
        b"\x1bXsos\x1b]7;sos\x07\x1b^pm\x1b]7;pm\x07",
        b"\x1b\x1b]7;double-esc\x07\x1b\x1b\x1b]9;triple\x07",
        b"\x1b\x05\x7f\x80\xff]7;esc-ignored-bytes\x07",
        b"\x1b\x18]7;can-after-esc\x07\x1b\x1a]7;sub-after-esc\x07",
        b"\x1b(]7;esc-intermediate\x07",
        b"\x1b]7;can\x18mid\x07\x1b]9;sub\x1amid\x07",
        b"\x1b]9;has\x9c\x9d\x9b;c1\x07\x9d7;c1-osc\x07\x9b\x1b]7;x\x9c",
        b"\x1b]7;c0\x01\x02\x0a\x0d\x19\x1c\x1fdropped\x07",
        b"\x1b]7;esc-other\x1b[m\x1b]9;esc-esc\x1b\x1b]7;y\x07",
        b"\xe2\x1b]7;partial-utf8-1\x07\xe2\x82\x1b]7;partial-utf8-2\x07",
        b"\xf0\x9f\x98\x1b]9;partial-utf8-3\x07\xc3\xa9\xff\xfe\x1b]9;invalid-utf8\x07",
        b"\x1b]7\x07\x1b]\x07\x1b];7\x07\x1b]77;no\x07\x1b]7777;no\x07\x1b]07;no\x07",
        b"\x1b]99;1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18\x07",
        b"\x1b]9;\x00\x00;;;\x07\x1b]777\x07\x1b]0;title\x07\x1b]9",
    ];
    for c in cases {
        v.extend_from_slice(c);
    }
    let alphabet: &[u8] =
        b"\x1b\x1b\x1b]]];;79\x07\x18\x1a\x9c\x9d\xe2\x82\xf0a[P_X^\\(\x00\x7f\x05 ";
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..400_000 {
        v.push(alphabet[(xorshift(&mut state) % alphabet.len() as u64) as usize]);
    }
    v
}

fn run_diff_ab(path: &str) -> ExitCode {
    let (data, size_sets): (Vec<u8>, Vec<Vec<usize>>) = if path == "--adversarial" {
        let sets = vec![
            vec![1],
            vec![2],
            vec![3],
            vec![7, 1, 13],
            vec![4096],
            vec![usize::MAX],
        ];
        (adversarial(), sets)
    } else {
        match fs::read(path) {
            Ok(d) => (d, vec![vec![CHUNK], vec![SPLIT_CHUNK], vec![4093, 1, 17]]),
            Err(err) => {
                eprintln!("read {path}: {err}");
                return ExitCode::FAILURE;
            }
        }
    };
    for sizes in &size_sets {
        match diff_ab(&data, sizes) {
            Ok(n) => println!("diff-ab\t{path}\tchunks={sizes:?}\tok\tevents={n}"),
            Err(e) => {
                println!("diff-ab\t{path}\tchunks={sizes:?}\tMISMATCH\t{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

fn report(
    mode: &str,
    stem: &str,
    bytes: usize,
    mbs: &mut [f64],
    total_allocs: u64,
    events: u64,
    max_bytes: usize,
) {
    let mib = bytes as f64 / MIB as f64;
    let median_mbs = median(mbs);
    let min_mbs = mbs.iter().copied().fold(f64::INFINITY, f64::min);
    let allocs_per_mib = total_allocs as f64 / (mib * MEASURED as f64);
    println!(
        "{mode}\t{stem}\t{mib:.2}\t{median_mbs:.2}\t{min_mbs:.2}\t{allocs_per_mib:.2}\t{events}\t{max_bytes}"
    );
}

// ---------------------------------------------------------------------------
// Synthetic corpus generation
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct OscStats {
    events: u64,
    max_payload: usize,
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn make_base64(n: usize) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut raw = Vec::with_capacity(n);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    while raw.len() < n {
        let word = xorshift(&mut state);
        for b in word.to_le_bytes() {
            if raw.len() < n {
                raw.push(b);
            }
        }
    }
    let mut out = Vec::with_capacity(n.div_ceil(3) * 4);
    for triple in raw.chunks_exact(3) {
        let v = ((triple[0] as u32) << 16) | ((triple[1] as u32) << 8) | triple[2] as u32;
        out.push(ALPHABET[((v >> 18) & 63) as usize]);
        out.push(ALPHABET[((v >> 12) & 63) as usize]);
        out.push(ALPHABET[((v >> 6) & 63) as usize]);
        out.push(ALPHABET[(v & 63) as usize]);
    }
    out
}

fn append_text(out: &mut Vec<u8>, len: usize) {
    const LINE: &[u8] = b"the quick brown fox jumps over the lazy dog 0123456789\r\n";
    let mut left = len;
    while left > 0 {
        let take = left.min(LINE.len());
        out.extend_from_slice(&LINE[..take]);
        left -= take;
    }
}

fn write_osc(out: &mut Vec<u8>, stats: &mut OscStats, payload: &[u8], bell: bool) {
    let cmd = payload.split(|&b| b == b';').next().unwrap_or(payload);
    if tracked_cmd(cmd) {
        stats.events += 1;
        if payload.len() > stats.max_payload {
            stats.max_payload = payload.len();
        }
    }
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(payload);
    if bell {
        out.push(0x07);
    } else {
        out.extend_from_slice(b"\x1b\\");
    }
}

fn osc_steps(big: &[u8]) -> Vec<(Vec<u8>, bool)> {
    vec![
        (b"7;file://localhost/home/user/project".to_vec(), true),
        (b"9;Build finished successfully".to_vec(), false),
        (b"9;4;1;50".to_vec(), true),
        (b"99;i=123;d=0;aGVsbG8=".to_vec(), true),
        (b"99;i=123;d=1;d29ybGQ=".to_vec(), false),
        (big.to_vec(), true),
        (b"777;notify;Build;Done".to_vec(), false),
    ]
}

const OSC_TARGET: usize = 32 * MIB + 4096;
const ASCII_TARGET: usize = 64 * MIB;

fn build_osc_heavy(big: &[u8]) -> (Vec<u8>, OscStats) {
    let steps = osc_steps(big);
    let text = {
        let mut v = Vec::new();
        append_text(&mut v, 200);
        v
    };
    let mut out = Vec::with_capacity(OSC_TARGET + 16 * 1024);
    let mut stats = OscStats::default();
    let mut i = 0usize;
    while out.len() < OSC_TARGET {
        out.extend_from_slice(&text);
        let (payload, bell) = &steps[i % steps.len()];
        write_osc(&mut out, &mut stats, payload, *bell);
        i += 1;
    }
    (out, stats)
}

fn build_osc_split(big: &[u8], chunk: usize) -> (Vec<u8>, OscStats) {
    let steps = osc_steps(big);
    let mut out = Vec::with_capacity(OSC_TARGET + 16 * 1024);
    let mut stats = OscStats::default();
    let mut idx = 0usize;
    'outer: while out.len() < OSC_TARGET {
        for (payload, bell) in &steps {
            let seq_len = 2 + payload.len() + if *bell { 1 } else { 2 };
            let off = (idx % (seq_len - 1)) + 1;
            let boundary = (out.len() / chunk + 1) * chunk;
            let mut start = boundary - off;
            if start < out.len() {
                start += chunk;
            }
            let pad = start - out.len();
            append_text(&mut out, pad);
            write_osc(&mut out, &mut stats, payload, *bell);
            idx += 1;
            if out.len() >= OSC_TARGET {
                break 'outer;
            }
        }
    }
    (out, stats)
}

fn build_ascii_plain() -> Vec<u8> {
    const LINE: &[u8] = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. 0123456789\r\n";
    let mut out = Vec::with_capacity(ASCII_TARGET + LINE.len());
    while out.len() < ASCII_TARGET {
        out.extend_from_slice(LINE);
    }
    out
}

fn generate(outdir: &str) -> std::io::Result<()> {
    fs::create_dir_all(outdir)?;
    let big_payload = {
        let b64 = make_base64(4095);
        let mut p = b"99;i=42;e=1;".to_vec();
        p.extend_from_slice(&b64);
        p
    };

    let (heavy, heavy_stats) = build_osc_heavy(&big_payload);
    let (split, split_stats) = build_osc_split(&big_payload, SPLIT_CHUNK);
    let ascii = build_ascii_plain();

    let files = [
        ("osc_heavy.bin", &heavy, heavy_stats),
        ("osc_split.bin", &split, split_stats),
        ("ascii_plain.bin", &ascii, OscStats::default()),
    ];
    for (name, data, stats) in files {
        let path = Path::new(outdir).join(name);
        fs::write(&path, data)?;
        println!(
            "{name}\t{}\t{}\t{}",
            data.len(),
            stats.events,
            stats.max_payload
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() == 3 && args[1] == "gen" {
        if let Err(err) = generate(&args[2]) {
            eprintln!("gen failed: {err}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }
    if args.len() != 3 {
        eprintln!("usage: throughput <vte-noop|vte-osc|alac|alac+A|alac+B> <corpus.bin>");
        eprintln!("       throughput diff-ab <corpus.bin|--adversarial>");
        eprintln!("       throughput gen <outdir>");
        return ExitCode::FAILURE;
    }

    let mode = args[1].as_str();
    let path = &args[2];
    if mode == "diff-ab" {
        return run_diff_ab(path);
    }
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("corpus");
    let chunk = if stem == "osc_split" {
        SPLIT_CHUNK
    } else {
        CHUNK
    };
    let bytes = data.len();

    match mode {
        "vte-noop" => {
            for _ in 0..WARMUPS {
                black_box(run_vte_noop(&data, chunk));
            }
            let mut mbs = Vec::with_capacity(MEASURED);
            let mut allocs = 0u64;
            for _ in 0..MEASURED {
                let (secs, a) = run_vte_noop(&data, chunk);
                allocs += a;
                mbs.push(bytes as f64 / 1e6 / secs);
            }
            report(mode, stem, bytes, &mut mbs, allocs, 0, 0);
        }
        "vte-osc" => {
            for _ in 0..WARMUPS {
                black_box(run_vte_osc(&data, chunk));
            }
            let mut mbs = Vec::with_capacity(MEASURED);
            let mut allocs = 0u64;
            let (mut events, mut max_bytes) = (0u64, 0usize);
            for _ in 0..MEASURED {
                let (secs, a, e, m) = run_vte_osc(&data, chunk);
                allocs += a;
                events = e;
                max_bytes = m;
                mbs.push(bytes as f64 / 1e6 / secs);
            }
            report(mode, stem, bytes, &mut mbs, allocs, events, max_bytes);
        }
        "alac" | "alac+A" | "alac+B" => {
            let osc = match mode {
                "alac+A" => OscMode::A,
                "alac+B" => OscMode::B,
                _ => OscMode::None,
            };
            for _ in 0..WARMUPS {
                black_box(run_alac(&data, chunk, osc));
            }
            let mut mbs = Vec::with_capacity(MEASURED);
            let mut allocs = 0u64;
            let (mut events, mut max_bytes) = (0u64, 0usize);
            let mut last_term = None;
            for _ in 0..MEASURED {
                let (secs, a, term, e, m) = run_alac(&data, chunk, osc);
                allocs += a;
                events = e;
                max_bytes = m;
                mbs.push(bytes as f64 / 1e6 / secs);
                last_term = Some(term);
            }
            report(mode, stem, bytes, &mut mbs, allocs, events, max_bytes);

            if let Some(term) = last_term {
                let mut content = term.renderable_content();
                let point = content.cursor.point;
                let nonspace = content.display_iter.any(|cell| cell.cell.c != ' ');
                eprintln!(
                    "sanity cursor=({},{}) nonspace={}",
                    point.line.0, point.column.0, nonspace
                );
            }
        }
        other => {
            eprintln!("unknown mode: {other}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
