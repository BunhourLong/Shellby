//! OSC 7 working-directory report: parse and validate (pure).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use crate::CwdReport;

/// Largest accepted decoded path, in bytes.
const MAX_PATH_BYTES: usize = 4096;

/// `body` is everything after `7;` (e.g. `file://host/a%20b`). `None` = reject.
pub(crate) fn parse(body: &[u8], hostname: &str) -> Option<CwdReport> {
    const SCHEME: &[u8] = b"file://";
    if body.len() < SCHEME.len() || !body[..SCHEME.len()].eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let rest = &body[SCHEME.len()..];
    let slash = rest.iter().position(|&b| b == b'/')?;
    let host = std::str::from_utf8(&rest[..slash]).ok()?;
    if !host.is_empty()
        && !host.eq_ignore_ascii_case("localhost")
        && !host.eq_ignore_ascii_case(hostname)
    {
        return None;
    }
    let path = percent_decode(&rest[slash..]);
    if path.len() > MAX_PATH_BYTES || path.first() != Some(&b'/') || path.contains(&0) {
        return None;
    }
    Some(CwdReport {
        host: String::new(),
        path: PathBuf::from(OsString::from_vec(path)),
    })
}

/// Lenient percent-decoding: `%XX` hex pairs become bytes, every other `%`
/// (malformed escape, bad digit, trailing `%`) stays literal.
fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_val);
            let lo = bytes.get(i + 2).copied().and_then(hex_val);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Hex digit value of `b`, or `None` for anything else.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    /// Hostname a test config would supply.
    const HOST: &str = "mybox";

    /// Decoded path bytes of a body expected to parse.
    fn path_bytes(body: &[u8]) -> Vec<u8> {
        parse(body, HOST)
            .expect("body should parse")
            .path
            .into_os_string()
            .into_vec()
    }

    #[test]
    fn encoded_and_unencoded_space() {
        assert_eq!(path_bytes(b"file://localhost/a%20b c"), b"/a b c");
    }

    #[test]
    fn encoded_slash_decodes_to_separator() {
        assert_eq!(path_bytes(b"file://localhost/a%2Fb"), b"/a/b");
    }

    #[test]
    fn bad_percent_and_trailing_percent_are_literal() {
        assert_eq!(path_bytes(b"file://localhost/a%ZZb%c%"), b"/a%ZZb%c%");
    }

    #[test]
    fn uppercase_scheme_and_host_accepted() {
        assert_eq!(path_bytes(b"FILE://LOCALHOST/x"), b"/x");
    }

    #[test]
    fn localhost_and_own_host_accepted() {
        assert!(parse(b"file://LoCaLhOsT/x", HOST).is_some());
        assert!(parse(b"file://MyBox/x", HOST).is_some());
    }

    #[test]
    fn foreign_host_rejected() {
        assert!(parse(b"file://other/x", HOST).is_none());
    }

    #[test]
    fn empty_host_accepted() {
        assert_eq!(path_bytes(b"file:///home/x"), b"/home/x");
    }

    #[test]
    fn relative_path_rejected() {
        assert!(parse(b"home/x", HOST).is_none());
        assert!(parse(b"file://localhost", HOST).is_none());
    }

    #[test]
    fn nul_via_percent_rejected() {
        assert!(parse(b"file://localhost/a%00b", HOST).is_none());
    }

    #[test]
    fn path_of_exactly_4096_bytes_ok() {
        let mut body = b"file://localhost/".to_vec();
        body.resize(body.len() + 4095, b'a');
        assert_eq!(path_bytes(&body).len(), 4096);
    }

    #[test]
    fn path_of_4097_bytes_rejected() {
        let mut body = b"file://localhost/".to_vec();
        body.resize(body.len() + 4096, b'a');
        assert!(parse(&body, HOST).is_none());
    }

    #[test]
    fn semicolon_inside_path() {
        assert_eq!(path_bytes(b"file://localhost/a;b/c;d"), b"/a;b/c;d");
    }

    #[test]
    fn invalid_utf8_path_bytes_accepted() {
        assert_eq!(path_bytes(b"file://localhost/a%FFb"), b"/a\xffb");
    }

    #[test]
    fn non_file_scheme_rejected() {
        assert!(parse(b"kitty-shell-cwd:///x", HOST).is_none());
        assert!(parse(b"https://localhost/x", HOST).is_none());
    }

    #[test]
    fn empty_body_rejected() {
        assert!(parse(b"", HOST).is_none());
    }

    #[test]
    fn reported_host_is_always_empty() {
        let report = parse(b"file://MyBox/x", HOST).expect("parses");
        assert_eq!(report.host, "");
    }

    #[test]
    fn percent_decoding_round_trips_via_from_vec() {
        let report = parse(b"file://localhost/a%20b", HOST).expect("parses");
        assert_eq!(
            report.path.into_os_string().into_vec(),
            OsString::from_vec(b"/a b".to_vec()).into_vec()
        );
    }
}
