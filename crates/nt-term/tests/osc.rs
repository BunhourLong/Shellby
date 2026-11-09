//! Black-box OSC tests through `Terminal::advance` / `Terminal::fill_frame` (O-1..O-16).

use nt_term::{
    ClipboardTarget, CwdReport, Frame, NotifySource, TermConfig, TermEvent, TermSize, Terminal,
};
use std::path::PathBuf;

const BEL: &[u8] = b"\x07";
const ST: &[u8] = b"\x1b\\";

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

fn feed(terminal: &mut Terminal, bytes: &[u8]) -> Vec<TermEvent> {
    let mut events = Vec::new();
    terminal.advance(bytes, &mut events);
    events
}

/// Feed `ESC ] <body> <terminator>`.
fn osc(terminal: &mut Terminal, body: &[u8], terminator: &[u8]) -> Vec<TermEvent> {
    let mut seq = Vec::with_capacity(body.len() + terminator.len() + 2);
    seq.extend_from_slice(b"\x1b]");
    seq.extend_from_slice(body);
    seq.extend_from_slice(terminator);
    feed(terminal, &seq)
}

fn titles(events: &[TermEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::Title(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn count_bell(events: &[TermEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, TermEvent::Bell))
        .count()
}

/// A borrowed view of one `TermEvent::Notify`.
#[derive(Debug, PartialEq, Eq)]
struct Notify<'a> {
    id: Option<&'a str>,
    title: Option<&'a str>,
    body: &'a str,
    source: NotifySource,
}

fn notifies(events: &[TermEvent]) -> Vec<Notify<'_>> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::Notify {
                id,
                title,
                body,
                source,
            } => Some(Notify {
                id: id.as_deref(),
                title: title.as_deref(),
                body: body.as_str(),
                source: *source,
            }),
            _ => None,
        })
        .collect()
}

fn close_ids(events: &[TermEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::NotifyClose { id } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn stores(events: &[TermEvent]) -> Vec<(ClipboardTarget, &[u8])> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::ClipboardStore { target, data } => Some((*target, data.as_slice())),
            _ => None,
        })
        .collect()
}

fn loads(events: &[TermEvent]) -> Vec<ClipboardTarget> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::ClipboardLoad { target } => Some(*target),
            _ => None,
        })
        .collect()
}

fn cwds(events: &[TermEvent]) -> Vec<&CwdReport> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::Cwd(report) => Some(report),
            _ => None,
        })
        .collect()
}

fn pty_writes(events: &[TermEvent]) -> Vec<&[u8]> {
    events
        .iter()
        .filter_map(|event| match event {
            TermEvent::PtyWrite(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .collect()
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

/// The first `n` characters of visible row 0 after a fill.
fn row0_text(terminal: &mut Terminal, n: usize) -> String {
    let mut frame = Frame::default();
    terminal.fill_frame(&mut frame);
    frame.cells[..frame.cols as usize]
        .iter()
        .take(n)
        .map(|cell| cell.ch)
        .collect()
}

#[test]
fn o1_title_osc_0_and_2() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        assert_eq!(
            titles(&osc(&mut terminal, b"0;hello", terminator)),
            vec!["hello"]
        );

        let mut terminal = term();
        assert_eq!(
            titles(&osc(&mut terminal, b"2;hello", terminator)),
            vec!["hello"]
        );

        let mut terminal = term();
        assert_eq!(
            titles(&osc(&mut terminal, b"1;icon", terminator)),
            Vec::<&str>::new()
        );
    }
}

#[test]
fn o2_title_sanitised_and_capped() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let mut body = b"2;".to_vec();
        body.extend_from_slice(b"\x01\x08\x7f");
        body.extend_from_slice("\u{202e}".as_bytes());
        body.extend_from_slice("\u{2066}".as_bytes());
        body.resize(body.len() + 2000, b'x');
        let events = osc(&mut terminal, &body, terminator);
        let titles = titles(&events);
        assert_eq!(titles.len(), 1);
        let title = titles[0];
        let len = title.chars().count();
        assert!(len <= 1024, "title longer than 1024 chars: {len}");
        assert!(len >= 1000, "title not truncated as expected: {len}");
        assert!(
            title.chars().all(|c| !c.is_control()),
            "control char survived: {title:?}"
        );
        assert!(
            !title.chars().any(is_bidi),
            "bidi control survived: {title:?}"
        );
        assert!(
            title.ends_with('\u{2026}'),
            "missing truncation marker: {title:?}"
        );
    }
}

#[test]
fn o3_titles_coalesced_per_advance() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let mut seq = Vec::new();
        for body in [b"0;first".as_slice(), b"2;second", b"0;third"] {
            seq.extend_from_slice(b"\x1b]");
            seq.extend_from_slice(body);
            seq.extend_from_slice(terminator);
        }
        let events = feed(&mut terminal, &seq);
        assert_eq!(titles(&events), vec!["third"]);
    }
}

#[test]
fn o4_bells_coalesced_per_advance() {
    let mut terminal = term();
    assert_eq!(count_bell(&feed(&mut terminal, &[0x07u8; 1000])), 1);
    assert_eq!(count_bell(&feed(&mut terminal, &[0x07u8])), 1);
}

#[test]
fn o5_osc7_accepted() {
    for terminator in [BEL, ST] {
        let cases: &[(&[u8], &str)] = &[
            (b"7;file:///home/u", "/home/u"),
            (b"7;file://testhost/a%20b", "/a b"),
            (b"7;file://localhost/x;y", "/x;y"),
            (b"7;FILE://TESTHOST/z", "/z"),
        ];
        for (body, path) in cases {
            let mut terminal = term();
            let events = osc(&mut terminal, body, terminator);
            let reports = cwds(&events);
            assert_eq!(reports.len(), 1, "body={body:?} terminator={terminator:?}");
            assert_eq!(reports[0].host, "");
            assert_eq!(reports[0].path, PathBuf::from(path));
        }
    }
}

#[test]
fn o6_osc7_rejected() {
    let long_path = format!("7;file://testhost/{}", "a".repeat(4097));
    let mut cases: Vec<Vec<u8>> = vec![
        b"7;file://evilhost/x".to_vec(),
        b"7;file://testhost".to_vec(),
        b"7;file:/x".to_vec(),
        b"7;http://testhost/x".to_vec(),
        b"7;file:///x%00y".to_vec(),
        b"7;".to_vec(),
        long_path.into_bytes(),
    ];
    cases.push(b"7;file://".to_vec());
    for terminator in [BEL, ST] {
        for body in &cases {
            let mut terminal = term();
            let events = osc(&mut terminal, body, terminator);
            assert!(cwds(&events).is_empty(), "accepted OSC 7 body {body:?}");
        }
    }
}

#[test]
fn o7_osc9_conemu_disambiguation() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let events = osc(&mut terminal, b"9;hello", terminator);
        assert_eq!(
            notifies(&events),
            vec![Notify {
                id: None,
                title: None,
                body: "hello",
                source: NotifySource::Osc9
            }]
        );

        for body in [b"9;4;1;50".as_slice(), b"9;4", b"9;12;x", b"9;1"] {
            let mut terminal = term();
            let events = osc(&mut terminal, body, terminator);
            assert!(
                notifies(&events).is_empty(),
                "ConEmu subcommand produced a Notify: {body:?}"
            );
        }

        let mut terminal = term();
        let events = osc(&mut terminal, b"9;13;hi", terminator);
        assert_eq!(notifies(&events)[0].body, "13;hi");

        let mut terminal = term();
        let events = osc(&mut terminal, b"9;0;hi", terminator);
        assert_eq!(notifies(&events)[0].body, "0;hi");
    }
}

#[test]
fn o8_osc777_notify() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let events = osc(&mut terminal, b"777;notify;T;B;C", terminator);
        assert_eq!(
            notifies(&events),
            vec![Notify {
                id: None,
                title: Some("T"),
                body: "B;C",
                source: NotifySource::Osc777,
            }]
        );

        let mut terminal = term();
        assert_eq!(
            notifies(&osc(&mut terminal, b"777;other;x", terminator)),
            Vec::<Notify<'_>>::new()
        );

        let mut terminal = term();
        let events = osc(&mut terminal, b"777;notify;;body", terminator);
        assert_eq!(
            notifies(&events),
            vec![Notify {
                id: None,
                title: None,
                body: "body",
                source: NotifySource::Osc777,
            }]
        );
    }
}

#[test]
fn o9_osc99_notifications_and_replies() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let events = osc(&mut terminal, b"99;;hello", terminator);
        assert_eq!(
            notifies(&events),
            vec![Notify {
                id: None,
                title: None,
                body: "hello",
                source: NotifySource::Osc99,
            }]
        );

        let mut terminal = term();
        assert_eq!(
            notifies(&osc(&mut terminal, b"99;i=a:d=0;Ti", terminator)),
            Vec::<Notify<'_>>::new()
        );
        let events = osc(&mut terminal, b"99;i=a:p=body:d=1;Bo", terminator);
        assert_eq!(
            notifies(&events),
            vec![Notify {
                id: Some("a"),
                title: Some("Ti"),
                body: "Bo",
                source: NotifySource::Osc99,
            }]
        );

        let mut terminal = term();
        let events = osc(&mut terminal, b"99;e=1;aGVsbG8=", terminator);
        assert_eq!(notifies(&events)[0].body, "hello");

        let mut terminal = term();
        let events = osc(&mut terminal, b"99;e=1;!!!!", terminator);
        assert!(
            notifies(&events).is_empty(),
            "invalid base64 produced a notification"
        );

        let mut terminal = term();
        let events = osc(&mut terminal, b"99;i=a:p=close:o=always", terminator);
        assert_eq!(close_ids(&events), vec!["a"]);

        let mut terminal = term();
        let events = osc(&mut terminal, b"99;i=a:p=?", terminator);
        assert_eq!(
            pty_writes(&events),
            vec![b"\x1b]99;i=a:p=?;p=title,body,close:o=always\x1b\\".as_slice()]
        );

        let mut terminal = term();
        let events = osc(&mut terminal, b"99;p=?", terminator);
        assert!(
            pty_writes(&events).is_empty(),
            "p=? without an id must stay silent"
        );
    }
}

#[test]
fn o10_osc99_id_eviction() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        for i in 0..17 {
            let body = format!("99;i=id{i}:d=0;t{i}");
            let events = osc(&mut terminal, body.as_bytes(), terminator);
            assert!(
                notifies(&events).is_empty(),
                "chunk before d=1 produced a notification"
            );
        }

        let events = osc(&mut terminal, b"99;i=id0:d=1;tail", terminator);
        if let Some(notify) = notifies(&events).first() {
            assert_eq!(notify.id, Some("id0"));
            assert_eq!(
                notify.title, None,
                "evicted title leaked into the notification"
            );
            assert_eq!(notify.body, "tail");
        }

        // No `p=` means title: "t1" + "tail" is a title-only notification → body.
        let events = osc(&mut terminal, b"99;i=id1:d=1;tail", terminator);
        let notify = &notifies(&events)[0];
        assert_eq!(notify.id, Some("id1"));
        assert_eq!(notify.title, None);
        assert_eq!(notify.body, "t1tail");
    }
}

#[test]
fn o11_notification_cap_per_advance() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let mut seq = Vec::new();
        for i in 0..65 {
            seq.extend_from_slice(b"\x1b]9;n");
            seq.extend_from_slice(i.to_string().as_bytes());
            seq.extend_from_slice(terminator);
        }
        assert_eq!(notifies(&feed(&mut terminal, &seq)).len(), 64);

        let events = osc(&mut terminal, b"9;again", terminator);
        assert_eq!(notifies(&events).len(), 1);
    }
}

#[test]
fn o12_osc52_clipboard() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let events = osc(&mut terminal, b"52;c;aGVsbG8=", terminator);
        assert_eq!(
            stores(&events),
            vec![(ClipboardTarget::Clipboard, b"hello".as_slice())]
        );

        let mut terminal = term();
        let events = osc(&mut terminal, b"52;p;aGVsbG8=", terminator);
        assert_eq!(
            stores(&events),
            vec![(ClipboardTarget::Primary, b"hello".as_slice())]
        );

        let mut terminal = term();
        let events = osc(&mut terminal, b"52;c;?", terminator);
        assert_eq!(loads(&events), vec![ClipboardTarget::Clipboard]);

        let mut terminal = term();
        let events = osc(&mut terminal, b"52;c;!!!!", terminator);
        assert!(
            stores(&events).is_empty(),
            "invalid base64 stored clipboard data"
        );
        assert!(loads(&events).is_empty(), "invalid base64 produced a load");
    }
}

#[test]
fn o13_osc777_many_fields() {
    for terminator in [BEL, ST] {
        let mut body = b"777;notify;Title".to_vec();
        let mut expected = String::new();
        for i in 0..20 {
            body.push(b';');
            if i > 0 {
                expected.push(';');
            }
            let field = format!("field{i}");
            body.extend_from_slice(field.as_bytes());
            expected.push_str(&field);
        }
        let mut terminal = term();
        let events = osc(&mut terminal, &body, terminator);
        let notifies = notifies(&events);
        assert_eq!(notifies.len(), 1);
        assert_eq!(notifies[0].title, Some("Title"));
        assert_eq!(
            notifies[0].body, expected,
            "20 ';'-separated fields were truncated"
        );
    }
}

#[test]
fn o14_oversized_osc9_dropped_and_recovers() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let mut seq = b"\x1b]9;".to_vec();
        seq.resize(seq.len() + (2 << 20), b'a');
        seq.extend_from_slice(terminator);
        assert!(
            notifies(&feed(&mut terminal, &seq)).is_empty(),
            "2 MiB OSC 9 was emitted"
        );

        let events = osc(&mut terminal, b"9;after", terminator);
        assert_eq!(
            notifies(&events).len(),
            1,
            "parser did not recover after an oversized OSC 9"
        );
    }
}

#[test]
fn o14b_oversized_title_capped_and_recovers() {
    for terminator in [BEL, ST] {
        let mut terminal = term();
        let mut seq = b"\x1b]2;".to_vec();
        seq.resize(seq.len() + (3 << 20), b'a');
        seq.extend_from_slice(terminator);
        seq.extend_from_slice(b"ok");
        let events = feed(&mut terminal, &seq);
        let titles = titles(&events);
        assert!(
            titles.len() <= 1,
            "oversized title produced {} Title events",
            titles.len()
        );
        if let Some(title) = titles.first() {
            assert!(
                title.chars().count() <= 1024,
                "oversized title was not capped"
            );
        }
        assert_eq!(
            row0_text(&mut terminal, 2),
            "ok",
            "'ok' did not render after the abort"
        );
    }
}

#[test]
fn o15_osc_aborted_by_can_or_sub() {
    let bodies: &[&[u8]] = &[b"9;hello", b"7;file:///x", b"99;i=a;hi", b"777;notify;t;b"];
    for abort in [0x18u8, 0x1a] {
        for body in bodies {
            let mut terminal = term();
            let mut seq = b"\x1b]".to_vec();
            seq.extend_from_slice(body);
            seq.push(abort);
            seq.extend_from_slice(b"ok");
            let events = feed(&mut terminal, &seq);
            assert!(
                cwds(&events).is_empty()
                    && notifies(&events).is_empty()
                    && close_ids(&events).is_empty(),
                "aborted OSC {body:?} produced an event"
            );
            assert_eq!(
                row0_text(&mut terminal, 2),
                "ok",
                "text after abort did not render"
            );
        }
    }
}

#[test]
fn o16_device_queries_reply() {
    let mut terminal = term();
    let events = feed(&mut terminal, b"\x1b[c");
    let writes = pty_writes(&events);
    assert!(
        writes.iter().any(|write| write.starts_with(b"\x1b[?")),
        "DA1 reply missing: {writes:?}"
    );

    for terminator in [BEL, ST] {
        let mut terminal = term();
        let events = osc(&mut terminal, b"10;?", terminator);
        let writes = pty_writes(&events);
        assert!(
            writes
                .iter()
                .any(|write| write.windows(4).any(|part| part == b"rgb:")),
            "OSC 10 reply missing rgb: color: {writes:?}"
        );
    }
}
