//! OSC 9 / 777 notifications (pure) and the OSC 99 (kitty) chunk state machine.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use crate::osc::sanitize::sanitize;
use crate::{NotifySource, TermEvent};

/// Largest number of pending OSC 99 ids kept; the oldest insertion is evicted.
const MAX_PENDING_IDS: usize = 16;
/// Largest accumulated payload kept per OSC 99 id.
const MAX_PENDING_BYTES: usize = 64 * 1024;

/// Standard-alphabet base64 that accepts padding on any chunk (kitty `e=1`).
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// `body` is everything after `9;`. `ConEmu` subcommands (first field 1..=12) → `None`.
pub(crate) fn parse_osc9(body: &[u8]) -> Option<TermEvent> {
    let first_end = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    // shortcut: ConEmu owns `9;1;`..`9;12;`, so a real notification whose text is
    // exactly "4" or starts with "4;" is swallowed (WezTerm makes the same trade-off).
    if is_conemu(&body[..first_end]) {
        return None;
    }
    let body = sanitize(&String::from_utf8_lossy(body), 4096);
    if body.is_empty() {
        return None;
    }
    Some(TermEvent::Notify {
        id: None,
        title: None,
        body,
        source: NotifySource::Osc9,
    })
}

/// True when `field` is 1–2 ASCII digits with a value in `1..=12` (`ConEmu` subcommand).
fn is_conemu(field: &[u8]) -> bool {
    if field.is_empty() || field.len() > 2 || !field.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let mut value = 0u32;
    for &b in field {
        value = value * 10 + u32::from(b - b'0');
    }
    (1..=12).contains(&value)
}

/// `body` is everything after `777;` (e.g. `notify;title;body`).
pub(crate) fn parse_osc777(body: &[u8]) -> Option<TermEvent> {
    let mut fields = body.splitn(3, |&b| b == b';');
    if fields.next()? != b"notify" {
        return None;
    }
    let title = String::from_utf8_lossy(fields.next().unwrap_or_default());
    let body = String::from_utf8_lossy(fields.next().unwrap_or_default());
    notify(None, &title, &body, NotifySource::Osc777)
}

/// Sanitised notification; title only → body = title; `None` when both are empty.
fn notify(id: Option<String>, title: &str, body: &str, source: NotifySource) -> Option<TermEvent> {
    let title = sanitize(title, 256);
    let body = sanitize(body, 4096);
    let (title, body) = match (title.is_empty(), body.is_empty()) {
        (true, true) => return None,
        (false, true) => (None, title),
        (true, false) => (None, body),
        (false, false) => (Some(title), body),
    };
    Some(TermEvent::Notify {
        id,
        title,
        body,
        source,
    })
}

/// Result of one OSC 99 sequence.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum K99Out {
    /// Nothing to emit (chunk buffered, ignored or invalid).
    None,
    /// A complete notification (`TermEvent::Notify` with `source: Osc99`).
    Event(TermEvent),
    /// Reply bytes for the PTY (`p=?` query).
    Reply(Vec<u8>),
}

/// Payload type from the `p` metadata key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PKind {
    /// `p=title` (the default).
    Title,
    /// `p=body`.
    Body,
    /// `p=close`.
    Close,
    /// `p=?`.
    Query,
    /// Any other value: payload ignored, `d` still honoured.
    Other,
}

/// Parsed OSC 99 metadata (keys we act on).
#[derive(Debug)]
struct Meta {
    /// Validated `i=` id.
    id: Option<String>,
    /// `d=0` (more chunks follow).
    more: bool,
    /// `e=1` (base64 payload).
    b64: bool,
    /// Payload type.
    kind: PKind,
}

/// Parses `:`-separated `key=value` items; unknown keys and malformed items are ignored.
fn parse_meta(meta: &[u8]) -> Meta {
    let mut out = Meta {
        id: None,
        more: false,
        b64: false,
        kind: PKind::Title,
    };
    for item in meta.split(|&b| b == b':') {
        let Some(eq) = item.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = item.split_at(eq);
        let value = &value[1..];
        match key {
            b"i" => out.id = valid_id(value),
            b"d" => out.more = value == b"0",
            b"e" => out.b64 = value == b"1",
            b"p" => {
                out.kind = match value {
                    b"title" => PKind::Title,
                    b"body" => PKind::Body,
                    b"close" => PKind::Close,
                    b"?" => PKind::Query,
                    _ => PKind::Other,
                };
            }
            _ => {}
        }
    }
    out
}

/// `[A-Za-z0-9_+.-]{1,64}`, else `None`.
fn valid_id(value: &[u8]) -> Option<String> {
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    if !value
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'.' | b'-'))
    {
        return None;
    }
    Some(String::from_utf8_lossy(value).into_owned())
}

/// Decodes a payload; invalid base64 yields `None`.
fn decode_payload(payload: &[u8], b64: bool) -> Option<String> {
    if !b64 {
        return Some(String::from_utf8_lossy(payload).into_owned());
    }
    let decoded = B64.decode(payload).ok()?;
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

/// Builds the `p=?` capability reply, or nothing without a valid id.
fn reply_query(id: Option<&str>) -> K99Out {
    let Some(id) = id else {
        return K99Out::None;
    };
    let mut out = Vec::with_capacity(id.len() + 48);
    out.extend_from_slice(b"\x1b]99;i=");
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(b":p=?;p=title,body,close:o=always\x1b\\");
    K99Out::Reply(out)
}

/// An OSC 99 notification event, or `K99Out::None` when empty.
fn emit_notify(id: Option<String>, title: &str, body: &str) -> K99Out {
    notify(id, title, body, NotifySource::Osc99).map_or(K99Out::None, K99Out::Event)
}

/// One accumulated OSC 99 notification.
#[derive(Debug)]
struct Pending {
    /// Notification id (`i=`).
    id: String,
    /// Accumulated title chunks.
    title: String,
    /// Accumulated body chunks.
    body: String,
    /// Accumulated payload bytes (UTF-8).
    bytes: usize,
    /// Over the per-id cap: ignore payloads until a `d!=0` chunk ends it.
    poisoned: bool,
}

/// Pending OSC 99 chunks keyed by id; bounded (≤ 16 ids, ≤ 64 KiB each).
#[derive(Debug, Default)]
pub(crate) struct Kitty99 {
    /// Pending entries, oldest insertion first.
    pending: Vec<Pending>,
}

impl Kitty99 {
    /// `body` is everything after `99;` (`<metadata>;<payload>`).
    pub(crate) fn feed(&mut self, body: &[u8]) -> K99Out {
        let (meta_bytes, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &body[body.len()..]),
        };
        let meta = parse_meta(meta_bytes);
        match meta.kind {
            PKind::Query => return reply_query(meta.id.as_deref()),
            PKind::Close => {
                let Some(id) = meta.id else {
                    return K99Out::None;
                };
                self.drop_pending(&id);
                return K99Out::Event(TermEvent::NotifyClose { id });
            }
            PKind::Title | PKind::Body | PKind::Other => {}
        }

        let text = decode_payload(payload, meta.b64);
        match meta.kind {
            PKind::Title | PKind::Body => self.chunk(meta, text),
            _ => self.ignore_chunk(meta.id, meta.more),
        }
    }

    /// Index of the pending entry for `id`.
    fn find(&self, id: &str) -> Option<usize> {
        self.pending.iter().position(|p| p.id == id)
    }

    /// Drops any pending entry for `id`.
    fn drop_pending(&mut self, id: &str) {
        if let Some(i) = self.find(id) {
            self.pending.remove(i);
        }
    }

    /// Returns the index for `id`, creating it (evicting the oldest id) if needed.
    fn ensure(&mut self, id: &str) -> usize {
        if let Some(i) = self.find(id) {
            return i;
        }
        if self.pending.len() >= MAX_PENDING_IDS {
            self.pending.remove(0);
        }
        self.pending.push(Pending {
            id: id.to_owned(),
            title: String::new(),
            body: String::new(),
            bytes: 0,
            poisoned: false,
        });
        self.pending.len() - 1
    }

    /// Accumulates a title/body chunk, or emits a standalone notification without an id.
    fn chunk(&mut self, meta: Meta, text: Option<String>) -> K99Out {
        let Some(text) = text else {
            return self.ignore_chunk(meta.id, meta.more);
        };
        let standalone = |id| match meta.kind {
            PKind::Body => emit_notify(id, "", &text),
            _ => emit_notify(id, &text, ""),
        };
        let Some(id) = meta.id else {
            return standalone(None);
        };
        if !meta.more && self.find(&id).is_none() {
            return standalone(Some(id)); // single final chunk: never evicts a pending id
        }

        if let Some(i) = self.find(&id)
            && self.pending[i].poisoned
        {
            if !meta.more {
                self.pending.remove(i);
            }
            return K99Out::None;
        }

        let index = self.ensure(&id);
        let total = self.pending[index].bytes.saturating_add(text.len());
        if total > MAX_PENDING_BYTES {
            self.pending[index].poisoned = true;
            self.pending[index].title.clear();
            self.pending[index].body.clear();
            if !meta.more {
                self.pending.remove(index);
            }
            return K99Out::None;
        }

        {
            let entry = &mut self.pending[index];
            entry.bytes = total;
            match meta.kind {
                PKind::Body => entry.body.push_str(&text),
                _ => entry.title.push_str(&text),
            }
        }
        if meta.more {
            return K99Out::None;
        }
        let entry = self.pending.remove(index);
        emit_notify(Some(entry.id), &entry.title, &entry.body)
    }

    /// An ignored payload (`p=icon`, invalid base64, …) still honours `d`.
    fn ignore_chunk(&mut self, id: Option<String>, more: bool) -> K99Out {
        let Some(id) = id else {
            return K99Out::None;
        };
        let Some(i) = self.find(&id) else {
            return K99Out::None;
        };
        if more {
            return K99Out::None;
        }
        let entry = self.pending.remove(i);
        if entry.poisoned {
            return K99Out::None;
        }
        emit_notify(Some(entry.id), &entry.title, &entry.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(
        id: Option<&str>,
        title: Option<&str>,
        body: &str,
        source: NotifySource,
    ) -> TermEvent {
        TermEvent::Notify {
            id: id.map(str::to_owned),
            title: title.map(str::to_owned),
            body: body.to_owned(),
            source,
        }
    }

    #[test]
    fn osc9_plain() {
        assert_eq!(
            parse_osc9(b"hello"),
            Some(notify(None, None, "hello", NotifySource::Osc9))
        );
    }

    #[test]
    fn osc9_keeps_semicolons() {
        assert_eq!(
            parse_osc9(b"hello;world"),
            Some(notify(None, None, "hello;world", NotifySource::Osc9))
        );
    }

    #[test]
    fn osc9_conemu_subcommands_ignored() {
        assert_eq!(parse_osc9(b"4;1;50"), None);
        assert_eq!(parse_osc9(b"4"), None);
        assert_eq!(parse_osc9(b"12;x"), None);
    }

    #[test]
    fn osc9_numeric_non_conemu_accepted() {
        assert_eq!(
            parse_osc9(b"13;hi"),
            Some(notify(None, None, "13;hi", NotifySource::Osc9))
        );
        assert_eq!(
            parse_osc9(b"0;x"),
            Some(notify(None, None, "0;x", NotifySource::Osc9))
        );
    }

    #[test]
    fn osc9_empty_ignored() {
        assert_eq!(parse_osc9(b""), None);
    }

    #[test]
    fn osc777_title_and_body() {
        assert_eq!(
            parse_osc777(b"notify;Title;Body"),
            Some(notify(None, Some("Title"), "Body", NotifySource::Osc777))
        );
    }

    #[test]
    fn osc777_body_keeps_semicolons() {
        assert_eq!(
            parse_osc777(b"notify;T;B;C"),
            Some(notify(None, Some("T"), "B;C", NotifySource::Osc777))
        );
    }

    #[test]
    fn osc777_title_only_becomes_body() {
        assert_eq!(
            parse_osc777(b"notify;T"),
            Some(notify(None, None, "T", NotifySource::Osc777))
        );
    }

    #[test]
    fn osc777_non_notify_ignored() {
        assert_eq!(parse_osc777(b"other;T;B"), None);
        assert_eq!(
            parse_osc777(b"notify;;B"),
            Some(notify(None, None, "B", NotifySource::Osc777))
        );
        assert_eq!(parse_osc777(b"notify"), None);
    }

    #[test]
    fn k99_simple_title() {
        let mut k = Kitty99::default();
        assert_eq!(
            k.feed(b"p=title;hello"),
            K99Out::Event(notify(None, None, "hello", NotifySource::Osc99))
        );
    }

    #[test]
    fn k99_title_then_body_chunks() {
        let mut k = Kitty99::default();
        assert_eq!(k.feed(b"i=a:d=0:p=title;Hello"), K99Out::None);
        assert_eq!(
            k.feed(b"i=a:d=1:p=body;World"),
            K99Out::Event(notify(
                Some("a"),
                Some("Hello"),
                "World",
                NotifySource::Osc99
            ))
        );
        assert!(k.pending.is_empty());
    }

    #[test]
    fn k99_base64_padding_optional() {
        let mut padded = Kitty99::default();
        assert_eq!(
            padded.feed(b"e=1:p=body;aGVsbG8="),
            K99Out::Event(notify(None, None, "hello", NotifySource::Osc99))
        );
        let mut unpadded = Kitty99::default();
        assert_eq!(
            unpadded.feed(b"e=1:p=body;aGVsbG8"),
            K99Out::Event(notify(None, None, "hello", NotifySource::Osc99))
        );
    }

    #[test]
    fn k99_invalid_base64_dropped() {
        let mut k = Kitty99::default();
        assert_eq!(k.feed(b"e=1:p=body;!!!!"), K99Out::None);
        assert!(k.pending.is_empty());
    }

    #[test]
    fn k99_close() {
        let mut k = Kitty99::default();
        assert_eq!(k.feed(b"i=z:d=0:p=title;T"), K99Out::None);
        assert_eq!(
            k.feed(b"i=z:p=close"),
            K99Out::Event(TermEvent::NotifyClose { id: "z".to_owned() })
        );
        assert!(k.pending.is_empty());
        assert_eq!(k.feed(b"p=close"), K99Out::None);
    }

    #[test]
    fn k99_query_reply() {
        let mut k = Kitty99::default();
        assert_eq!(
            k.feed(b"i=abc:p=?"),
            K99Out::Reply(b"\x1b]99;i=abc:p=?;p=title,body,close:o=always\x1b\\".to_vec())
        );
        assert_eq!(k.feed(b"p=?"), K99Out::None);
        assert_eq!(k.feed(b"i=bad id:p=?"), K99Out::None);
    }

    #[test]
    fn k99_invalid_id_is_no_id() {
        let mut k = Kitty99::default();
        assert_eq!(
            k.feed(b"i=bad id:d=0:p=body;hi"),
            K99Out::Event(notify(None, None, "hi", NotifySource::Osc99))
        );
    }

    #[test]
    fn k99_seventeen_ids_evict_oldest() {
        let mut k = Kitty99::default();
        for i in 0..17 {
            let seq = format!("i=id{i}:d=0:p=title;T{i}");
            assert_eq!(k.feed(seq.as_bytes()), K99Out::None);
        }
        assert_eq!(k.pending.len(), 16);
        assert_eq!(
            k.feed(b"i=id1:d=1:p=body;B"),
            K99Out::Event(notify(Some("id1"), Some("T1"), "B", NotifySource::Osc99))
        );
        assert_eq!(
            k.feed(b"i=id0:d=1:p=body;B"),
            K99Out::Event(notify(Some("id0"), None, "B", NotifySource::Osc99))
        );
    }

    #[test]
    fn k99_overflow_drops_until_done() {
        let mut k = Kitty99::default();
        let mut seq = b"i=big:d=0:p=body;".to_vec();
        seq.extend_from_slice(&vec![b'a'; MAX_PENDING_BYTES]);
        assert_eq!(k.feed(&seq), K99Out::None);
        assert_eq!(k.feed(b"i=big:d=0:p=body;x"), K99Out::None);
        assert_eq!(k.feed(b"i=big:d=1:p=body;x"), K99Out::None);
        assert!(k.pending.iter().all(|p| p.id != "big"));
    }

    #[test]
    fn k99_unknown_and_malformed_ignored() {
        let mut k = Kitty99::default();
        assert_eq!(
            k.feed(b"z=1:malformed:i=a:p=body;hi"),
            K99Out::Event(notify(Some("a"), None, "hi", NotifySource::Osc99))
        );
    }

    #[test]
    fn k99_duplicate_key_last_wins() {
        let mut k = Kitty99::default();
        assert_eq!(
            k.feed(b"i=one:i=two:p=body;hi"),
            K99Out::Event(notify(Some("two"), None, "hi", NotifySource::Osc99))
        );
        let mut d = Kitty99::default();
        assert_eq!(d.feed(b"i=a:d=1:d=0:p=body;hi"), K99Out::None);
        assert_eq!(
            d.feed(b"i=a:d=1:p=body;there"),
            K99Out::Event(notify(Some("a"), None, "hithere", NotifySource::Osc99))
        );
    }
}
