//! Chunking invariance and robustness properties (C-1..C-4), seeded, no proptest.

use nt_term::{Frame, TermConfig, TermEvent, TermSize, Terminal};

const TOKENS: &[&[u8]] = &[
    b"\x1b]",
    b";",
    b"9",
    b"7",
    b"99",
    b"777",
    b"\x07",
    b"\x1b\\",
    b"\x18",
    "é".as_bytes(),
    "漢".as_bytes(),
    b"\r\n",
    b"\x1b[31m",
];

const SEED_C1: u64 = 0x9E37_79B9_7F4A_7C15;
const SEED_C2: u64 = 0xDEAD_BEEF_1234_5678;
const SEED_C4: u64 = 0xCAFE_F00D_DEAD_BEEF;

/// A tiny deterministic xorshift64 generator.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

fn term() -> Terminal {
    Terminal::new(
        TermSize {
            cols: 80,
            rows: 24,
            cell_w_px: 8,
            cell_h_px: 16,
        },
        &TermConfig {
            hostname: "testhost".into(),
            ..TermConfig::default()
        },
    )
}

/// Up to 4 KiB, biased towards OSC-ish tokens, ending at a random point.
fn gen_stream(rng: &mut Rng) -> Vec<u8> {
    let mut stream = Vec::new();
    while stream.len() < 4096 {
        if rng.below(100) < 30 {
            let token = TOKENS[rng.below(TOKENS.len())];
            if stream.len() + token.len() > 4096 {
                break;
            }
            stream.extend_from_slice(token);
        } else {
            stream.push((rng.next_u64() & 0xff) as u8);
        }
        if rng.below(1024) == 0 {
            break;
        }
    }
    if stream.is_empty() {
        stream.push(b'a');
    }
    stream
}

fn gen_splits(rng: &mut Rng, len: usize) -> Vec<usize> {
    let count = 1 + rng.below(20);
    let mut splits = Vec::with_capacity(count);
    for _ in 0..count {
        splits.push(rng.below(len + 1));
    }
    splits.sort_unstable();
    splits
}

/// Per-call coalescing makes raw event lists differ, so compare the stable projection.
#[derive(Debug, PartialEq, Eq)]
struct Norm {
    semantic: Vec<TermEvent>,
    pty: Vec<u8>,
    bell: bool,
    last_title: Option<TermEvent>,
}

fn normalize(events: Vec<TermEvent>) -> (Norm, usize) {
    let mut norm = Norm {
        semantic: Vec::new(),
        pty: Vec::new(),
        bell: false,
        last_title: None,
    };
    let mut notify_count = 0usize;
    for event in events {
        match event {
            TermEvent::PtyWrite(bytes) => norm.pty.extend_from_slice(&bytes),
            TermEvent::Bell => norm.bell = true,
            title @ (TermEvent::Title(_) | TermEvent::ResetTitle) => {
                norm.last_title = Some(title);
            }
            other => {
                if matches!(&other, TermEvent::Notify { .. }) {
                    notify_count += 1;
                }
                if matches!(
                    &other,
                    TermEvent::Notify { .. }
                        | TermEvent::Cwd(_)
                        | TermEvent::NotifyClose { .. }
                        | TermEvent::ClipboardStore { .. }
                        | TermEvent::ClipboardLoad { .. }
                ) {
                    norm.semantic.push(other);
                }
            }
        }
    }
    (norm, notify_count)
}

fn feed_split(terminal: &mut Terminal, stream: &[u8], splits: &[usize]) -> Vec<TermEvent> {
    let mut events = Vec::new();
    let mut previous = 0usize;
    for &point in splits {
        terminal.advance(&stream[previous..point], &mut events);
        previous = point;
    }
    terminal.advance(&stream[previous..], &mut events);
    events
}

#[test]
fn c1_chunking_invariance() {
    let mut rng = Rng::new(SEED_C1);
    let mut cases = 0usize;
    let mut attempts = 0usize;
    while cases < 2000 {
        attempts += 1;
        assert!(
            attempts < 50_000,
            "generator did not produce 2000 usable cases"
        );
        let seed = rng.0;
        let stream = gen_stream(&mut rng);
        let splits = gen_splits(&mut rng, stream.len());

        let mut unsplit = term();
        let mut unsplit_events = Vec::new();
        unsplit.advance(&stream, &mut unsplit_events);
        let mut unsplit_frame = Frame::default();
        unsplit.fill_frame(&mut unsplit_frame);
        let (unsplit_norm, notify_count) = normalize(unsplit_events);
        if notify_count >= 64 {
            continue;
        }

        let mut split = term();
        let split_events = feed_split(&mut split, &stream, &splits);
        let mut split_frame = Frame::default();
        split.fill_frame(&mut split_frame);
        let (split_norm, _) = normalize(split_events);

        let len = stream.len();
        let cells_equal = unsplit_frame.cells == split_frame.cells;
        assert!(
            unsplit_norm == split_norm && cells_equal,
            "C-1 chunking mismatch seed={seed:#018x} len={len} splits={splits:?}\n\
             unsplit={unsplit_norm:?}\nsplit={split_norm:?}\ncells_equal={cells_equal}"
        );
        cases += 1;
    }
}

#[test]
fn c2_random_bytes_never_panic() {
    let mut rng = Rng::new(SEED_C2);
    for _ in 0..10_000 {
        let len = 1 + rng.below(1024);
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push((rng.next_u64() & 0xff) as u8);
        }
        let mut terminal = term();
        let mut events = Vec::new();
        terminal.advance(&bytes, &mut events);
        let mut frame = Frame::default();
        terminal.fill_frame(&mut frame);
    }
}

#[test]
fn c3_per_advance_event_bounds() {
    for terminator in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
        let mut terminal = term();
        let mut seq = Vec::new();
        for i in 0..100 {
            seq.extend_from_slice(b"\x1b]9;n");
            seq.extend_from_slice(i.to_string().as_bytes());
            seq.extend_from_slice(terminator);
        }
        seq.extend(std::iter::repeat_n(0x07, 100));
        for i in 0..50 {
            seq.extend_from_slice(b"\x1b]2;t");
            seq.extend_from_slice(i.to_string().as_bytes());
            seq.extend_from_slice(terminator);
        }

        let mut events = Vec::new();
        terminal.advance(&seq, &mut events);
        let notifications = events
            .iter()
            .filter(|event| matches!(event, TermEvent::Notify { .. }))
            .count();
        let bells = events
            .iter()
            .filter(|event| matches!(event, TermEvent::Bell))
            .count();
        let titles = events
            .iter()
            .filter(|event| matches!(event, TermEvent::Title(_) | TermEvent::ResetTitle))
            .count();
        let pty_bytes: usize = events
            .iter()
            .filter_map(|event| match event {
                TermEvent::PtyWrite(bytes) => Some(bytes.len()),
                _ => None,
            })
            .sum();
        assert!(
            notifications <= 64,
            "too many notifications: {notifications}"
        );
        assert!(bells <= 1, "too many Bell events: {bells}");
        assert!(titles <= 1, "too many Title/ResetTitle events: {titles}");
        assert!(pty_bytes <= 1 << 20, "too many PtyWrite bytes: {pty_bytes}");
    }
}

fn is_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

fn assert_clean(text: &str, what: &str) {
    assert!(
        text.chars().all(|c| !c.is_control()),
        "{what} has a control char: {text:?}"
    );
    assert!(
        !text.chars().any(is_bidi),
        "{what} has a bidi control: {text:?}"
    );
}

#[test]
fn c4_event_payload_invariants() {
    let mut rng = Rng::new(SEED_C4);
    for _ in 0..300 {
        let stream = gen_stream(&mut rng);
        let mut terminal = term();
        let mut events = Vec::new();
        terminal.advance(&stream, &mut events);
        for event in &events {
            match event {
                TermEvent::Cwd(report) => {
                    assert!(
                        report.path.is_absolute(),
                        "Cwd path is not absolute: {report:?}"
                    );
                    assert!(
                        !report.path.to_string_lossy().contains('\0'),
                        "Cwd path contains NUL: {report:?}"
                    );
                }
                TermEvent::Title(title) => assert_clean(title, "title"),
                TermEvent::Notify { title, body, .. } => {
                    assert!(!body.is_empty(), "Notify body is empty");
                    assert_clean(body, "notify body");
                    if let Some(title) = title {
                        assert_clean(title, "notify title");
                    }
                }
                _ => {}
            }
        }
    }
}

/// vte 0.15 drops valid text after a UTF-8 char split across `advance` calls when an
/// invalid byte follows within 3 bytes (found by C-1, seed 0x6e83fbe6d047500c).
#[test]
fn c5_split_utf8_then_invalid_byte_keeps_text() {
    let row0 = |chunks: &[&[u8]]| {
        let mut terminal = term();
        let mut events = Vec::new();
        for chunk in chunks {
            terminal.advance(chunk, &mut events);
        }
        let mut frame = Frame::default();
        terminal.fill_frame(&mut frame);
        frame
            .row(0)
            .iter()
            .take(4)
            .map(|cell| cell.ch)
            .collect::<String>()
    };
    assert!(row0(&[b"\xC3\xA90\x81"]).starts_with("é0"));
    for (whole, split) in [
        (&b"\xC3\xA90\x81"[..], &[&b"\xC3"[..], b"\xA90\x81"][..]),
        (b"\xE6\xBC\xA2a\x81", &[b"\xE6", b"\xBC\xA2a\x81"]),
        (
            b"\xF0\x9F\x98\x80b\xFF",
            &[b"\xF0\x9F", b"\x98", b"\x80b\xFF"],
        ),
        (b"\xC3x\xA9", &[b"\xC3", b"x\xA9"]),
    ] {
        assert_eq!(row0(split), row0(&[whole]), "split {split:02x?}");
    }
}
