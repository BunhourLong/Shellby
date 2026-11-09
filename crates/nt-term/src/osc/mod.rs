//! OSC side-channel (D-T02-1 strategy B): a gated scanner that mirrors vte 0.15's OSC
//! boundaries, extracts OSC 7/9/99/777 bodies, and caps every OSC the core sees.

pub(crate) mod cwd;
pub(crate) mod notify;
pub(crate) mod sanitize;

/// Largest tracked (7/9/99/777) OSC body kept; larger ones are dropped whole.
pub(crate) const TRACKED_CAP: usize = 1 << 20;
/// Largest OSC payload the core may buffer; past it the core gets a CAN (vte's own
/// OSC buffer is unbounded with feature "std", D-T02-1 #3).
pub(crate) const CORE_CAP: usize = 2 << 20;

/// One scanner output, in stream order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Scanned<'a> {
    /// Bytes to feed to the core parser, in order. Concatenated, they equal the input
    /// except for over-cap OSC payloads, which are replaced by a single CAN.
    Core(&'a [u8]),
    /// A completed tracked OSC: the body between `ESC ]` and the terminator,
    /// e.g. `b"9;hello"`, with vte-ignored C0 bytes removed.
    Osc(&'a [u8]),
}

/// Scanner state carried across `advance` calls.
#[derive(Debug, Default)]
pub(crate) struct OscScanner {
    state: State,
    /// Body of the OSC being buffered; reused across calls and OSCs.
    buf: Vec<u8>,
    /// Payload bytes seen in the current OSC (the core-cap counter).
    pos: usize,
    /// True while the current body may still be a tracked command and is buffered.
    keep: bool,
    /// True once the current OSC exceeded `CORE_CAP`; its bytes are withheld.
    over_cap: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
}

/// True for the OSC numbers newTerm handles; `cmd` is the first field.
fn tracked_cmd(cmd: &[u8]) -> bool {
    matches!(cmd, b"7" | b"9" | b"99" | b"777")
}

/// The bytes before the first `';'`, or the whole body if there is none.
fn first_field(body: &[u8]) -> &[u8] {
    match body.iter().position(|&b| b == b';') {
        Some(p) => &body[..p],
        None => body,
    }
}

/// `false` once the first field is known not to be tracked (tracked commands are <= 3 bytes).
fn may_be_tracked(buf: &[u8]) -> bool {
    let head = &buf[..buf.len().min(4)];
    match head.iter().position(|&b| b == b';') {
        Some(p) => tracked_cmd(&head[..p]),
        None => head.len() < 4,
    }
}

impl OscScanner {
    /// Scans `bytes`, calling `out` with `Core` segments and completed `Osc` bodies in order.
    pub(crate) fn advance(&mut self, bytes: &[u8], out: &mut impl FnMut(Scanned<'_>)) {
        let mut i = 0;
        let mut core_start = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match memchr::memchr(0x1B, &bytes[i..]) {
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Escape;
                    }
                    None => i = bytes.len(),
                },
                State::Escape => {
                    match bytes[i] {
                        b']' => {
                            self.buf.clear();
                            self.pos = 0;
                            self.keep = true;
                            self.over_cap = false;
                            self.state = State::Osc;
                        }
                        0x00..=0x17 | 0x19 | 0x1B..=0x1F | 0x7F..=0xFF => {}
                        _ => self.state = State::Ground,
                    }
                    i += 1;
                }
                State::Osc => {
                    let rest = &bytes[i..];
                    let n = rest.iter().position(|&b| b < 0x20).unwrap_or(rest.len());
                    if n > 0 {
                        if self.keep {
                            if self.pos.saturating_add(n) > TRACKED_CAP {
                                self.keep = false;
                            } else {
                                self.buf.extend_from_slice(&rest[..n]);
                                if !may_be_tracked(&self.buf) {
                                    self.keep = false;
                                }
                            }
                        }
                        if !self.over_cap {
                            let room = CORE_CAP - self.pos;
                            if n > room {
                                let end = i + room;
                                if core_start < end {
                                    out(Scanned::Core(&bytes[core_start..end]));
                                }
                                out(Scanned::Core(&[0x18u8]));
                                self.over_cap = true;
                            }
                        }
                        self.pos = self.pos.saturating_add(n);
                        i += n;
                    }
                    let Some(&b) = bytes.get(i) else { break };
                    i += 1;
                    match b {
                        0x07 | 0x1B => {
                            if self.keep && tracked_cmd(first_field(&self.buf)) {
                                out(Scanned::Osc(&self.buf));
                            }
                            self.state = if b == 0x1B {
                                State::Escape
                            } else {
                                State::Ground
                            };
                        }
                        0x18 | 0x1A => self.state = State::Ground,
                        _ => continue,
                    }
                    if self.over_cap {
                        core_start = if b == 0x1B { i - 1 } else { i };
                        self.over_cap = false;
                    }
                }
            }
        }
        if !self.over_cap && core_start < bytes.len() {
            out(Scanned::Core(&bytes[core_start..]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_chunks(chunks: &[&[u8]]) -> Vec<(bool, Vec<u8>)> {
        let mut scanner = OscScanner::default();
        let mut seen = Vec::new();
        for chunk in chunks {
            scanner.advance(chunk, &mut |item| match item {
                Scanned::Core(b) => {
                    assert!(!b.is_empty(), "empty Core slice");
                    seen.push((true, b.to_vec()));
                }
                Scanned::Osc(b) => seen.push((false, b.to_vec())),
            });
        }
        seen
    }

    fn run(input: &[u8]) -> Vec<(bool, Vec<u8>)> {
        run_chunks(&[input])
    }

    fn cores(seen: &[(bool, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (is_core, bytes) in seen {
            if *is_core {
                out.extend_from_slice(bytes);
            }
        }
        out
    }

    fn oscs(seen: &[(bool, Vec<u8>)]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (is_core, bytes) in seen {
            if !*is_core {
                out.push(bytes.clone());
            }
        }
        out
    }

    fn osc(body: &[u8]) -> Vec<u8> {
        body.to_vec()
    }

    #[test]
    fn bel_and_st_terminators() {
        let input = b"\x1b]9;hi\x07\x1b]7;file:///x\x1b\\";
        let seen = run(input);
        assert_eq!(oscs(&seen), vec![osc(b"9;hi"), osc(b"7;file:///x")]);
        assert_eq!(cores(&seen), input.to_vec());
    }

    #[test]
    fn untracked_osc_not_emitted() {
        let input = b"\x1b]2;a title\x07";
        let seen = run(input);
        assert_eq!(oscs(&seen), Vec::<Vec<u8>>::new());
        assert_eq!(cores(&seen), input.to_vec());
    }

    #[test]
    fn semicolons_and_utf8_are_body() {
        let input = b"\x1b]9;a;b\xc3\xa9\x07";
        let seen = run(input);
        assert_eq!(oscs(&seen), vec![osc(b"9;a;b\xc3\xa9")]);
        assert_eq!(cores(&seen), input.to_vec());
    }

    #[test]
    fn c0_inside_body_dropped_but_7f_is_body() {
        let input = b"\x1b]9;\x01h\x02i\x7f\x07";
        let seen = run(input);
        assert_eq!(oscs(&seen), vec![osc(b"9;hi\x7f")]);
        assert_eq!(cores(&seen), input.to_vec());
    }

    #[test]
    fn can_and_sub_cancel() {
        for term in [0x18u8, 0x1A] {
            let mut input = b"\x1b]9;hi".to_vec();
            input.push(term);
            let seen = run(&input);
            assert!(oscs(&seen).is_empty(), "terminator {term:#x}");
            assert_eq!(cores(&seen), input);
        }
    }

    #[test]
    fn esc_terminates_and_next_esc_bracket_starts_new_osc() {
        let input = b"\x1b]9;a\x1b]9;b\x07";
        let seen = run(input);
        assert_eq!(oscs(&seen), vec![osc(b"9;a"), osc(b"9;b")]);
        assert_eq!(cores(&seen), input.to_vec());
    }

    #[test]
    fn double_esc_and_esc_c0_then_bracket_enter_osc() {
        assert_eq!(oscs(&run(b"\x1b\x1b]9;x\x07")), vec![osc(b"9;x")]);
        assert_eq!(oscs(&run(b"\x1b\x01]9;y\x07")), vec![osc(b"9;y")]);
    }

    #[test]
    fn csi_is_not_an_osc() {
        let input = b"\x1b[31m\x1b]9;x\x07";
        let seen = run(input);
        assert_eq!(oscs(&seen), vec![osc(b"9;x")]);
        assert_eq!(cores(&seen), input.to_vec());
    }

    #[test]
    fn every_split_gives_same_oscs_and_core_stream() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"ab\x1b]9;one\x07cd\x1b]99;two\x1b\\ef\x1b]777;three\x07gh");

        let whole = run(&stream);
        let want_oscs = oscs(&whole);
        let want_core = cores(&whole);
        assert_eq!(want_oscs.len(), 3);
        assert_eq!(want_core, stream);

        for split in 0..=stream.len() {
            let seen = run_chunks(&[&stream[..split], &stream[split..]]);
            assert_eq!(oscs(&seen), want_oscs, "split {split}");
            assert_eq!(cores(&seen), want_core, "split {split}");
        }

        let singles: Vec<&[u8]> = stream.chunks(1).collect();
        let seen = run_chunks(&singles);
        assert_eq!(oscs(&seen), want_oscs, "one-byte chunks");
        assert_eq!(cores(&seen), want_core, "one-byte chunks");
    }

    #[test]
    fn tracked_cap_exact_is_kept_and_plus_one_dropped() {
        let exact_body = {
            let mut v = b"9;".to_vec();
            v.extend(std::iter::repeat_n(b'x', TRACKED_CAP - 2));
            v
        };
        let mut exact = b"\x1b]".to_vec();
        exact.extend_from_slice(&exact_body);
        exact.push(0x07);
        let seen = run(&exact);
        assert_eq!(oscs(&seen), vec![exact_body.clone()]);
        assert_eq!(cores(&seen), exact);

        let mut over_body = b"9;".to_vec();
        over_body.extend(std::iter::repeat_n(b'x', TRACKED_CAP - 1));
        let mut over = b"\x1b]".to_vec();
        over.extend_from_slice(&over_body);
        over.push(0x07);
        let seen = run(&over);
        assert_eq!(oscs(&seen), Vec::<Vec<u8>>::new());
        assert_eq!(cores(&seen), over);
    }

    #[test]
    fn core_cap_bel_withholds_terminator_then_next_osc() {
        let mut input = b"\x1b]2;".to_vec();
        input.extend(std::iter::repeat_n(b'x', CORE_CAP + 10 - 2));
        input.push(0x07);
        input.extend_from_slice(b"ok");
        let next = b"\x1b]9;hi\x07";
        input.extend_from_slice(next);

        let seen = run(&input);
        let mut want = input[..2 + CORE_CAP].to_vec();
        want.push(0x18);
        want.extend_from_slice(b"ok");
        want.extend_from_slice(next);
        assert_eq!(cores(&seen), want);
        assert_eq!(oscs(&seen), vec![osc(b"9;hi")]);
    }

    #[test]
    fn core_cap_esc_gives_terminator_and_rest_then_next_osc() {
        let mut input = b"\x1b]2;".to_vec();
        input.extend(std::iter::repeat_n(b'x', CORE_CAP + 10 - 2));
        input.extend_from_slice(b"\x1b\\");
        input.extend_from_slice(b"ok");
        let next = b"\x1b]9;hi\x07";
        input.extend_from_slice(next);

        let seen = run(&input);
        let mut want = input[..2 + CORE_CAP].to_vec();
        want.push(0x18);
        want.extend_from_slice(b"\x1b\\ok");
        want.extend_from_slice(next);
        assert_eq!(cores(&seen), want);
        assert_eq!(oscs(&seen), vec![osc(b"9;hi")]);
    }
}
