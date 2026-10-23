//! OSC 7 current-directory reports: `ESC ] 7 ; file://host/path ST`.
//!
//! The shell tells the terminal where it is. New tabs and splits inherit
//! that directory, and an OSC 8 hyperlink resolver can join a relative
//! `file:line` target against [`crate::term::SessionMeta::reported_cwd`].
//!
//! The URL is parsed here (scheme, host, percent-decoding). The path that
//! comes out is then [`crate::term::accept_local_cwd`]: drive letter only,
//! and UNC, `\\?\`, `\\.\` and `\??\` are rejected before anything probes
//! the disk.

/// Payload cap, bytes between the semicolon and the terminator. Matches the
/// length PR #2 uses for OSC 9;9 so the two gates reject the same class of
/// oversized report.
pub const OSC7_MAX_PAYLOAD: usize = 4096;

/// BiDi controls that can spoof a path when a UI draws it. Overrides,
/// embeddings, isolates, plus the implicit marks (LRM/RLM/ALM).
pub fn is_bidi_control(c: char) -> bool {
    matches!(
        c as u32,
        0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
    )
}

/// Names that count as "this machine" besides `localhost` and the loopback
/// addresses, which are always accepted. Windows sets `COMPUTERNAME`.
pub fn local_host_names() -> Vec<String> {
    ["COMPUTERNAME", "HOSTNAME", "HOST"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok())
        .filter(|s| !s.is_empty() && !s.chars().any(forbidden_char))
        .collect()
}

pub fn cached_local_names() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(local_host_names)
}

fn forbidden_char(c: char) -> bool {
    c.is_control() || is_bidi_control(c)
}

/// Parse one OSC 7 payload (no `ESC ] 7 ;` prefix, no BEL/ST) into a
/// Windows drive path (`C:\…`). `None` rejects the report: the caller must
/// not replace a previously accepted directory.
///
/// `local_names` are extra hosts treated as this machine (tests pass a
/// fixed list; production passes [`local_host_names`]). `localhost`,
/// `127.0.0.1`, `::1`, and an empty host are always local.
pub fn parse_osc7<S: AsRef<str>>(payload: &str, local_names: &[S]) -> Option<String> {
    if payload.len() > OSC7_MAX_PAYLOAD {
        return None;
    }
    // Surrounding spaces are the `ESC ] 7 ; file://…` spelling. Any other
    // whitespace, and every control or bidi character, rejects the report
    // before we look at the scheme.
    let payload = payload.trim_matches(' ');
    // A raw space belongs in the path as `%20`. Anything else that is
    // whitespace, a control, a bidi override, or a backslash is rejected
    // before the scheme is even read.
    if payload.is_empty()
        || payload
            .chars()
            .any(|c| forbidden_char(c) || c.is_whitespace())
        || payload.contains('\\')
    {
        return None;
    }
    let rest = payload
        .get(5..)
        .filter(|_| payload[..5].eq_ignore_ascii_case("file:"))?
        .strip_prefix("//")?;
    // Query, fragment: a file URL we act on has neither. A `#` or `?` that
    // belongs in the path has to arrive percent-encoded.
    if rest.contains(['?', '#']) {
        return None;
    }
    let (host, path) = split_host(rest)?;
    if !host_allowed(host, local_names) {
        return None;
    }
    let decoded = percent_decode(path)?;
    if decoded.chars().any(forbidden_char) || decoded.chars().count() > OSC7_MAX_PAYLOAD {
        return None;
    }
    windows_drive_path(&decoded)
}

/// The spawn gate. Parsing accepts a well-formed drive path that does not
/// exist (the shell can report a directory we race with); spawning must not.
/// `Session::spawn` goes further and rejects a remote drive or a reparse
/// point before this check.
pub fn directory_exists(path: &str) -> bool {
    !path.is_empty() && std::path::Path::new(path).is_dir()
}

/// `file://host/path` with `rest` starting at `host`. A leading slash is an
/// empty host (`file:///C:/x`).
fn split_host(rest: &str) -> Option<(&str, &str)> {
    if rest.starts_with('/') {
        return Some(("", rest));
    }
    let slash = rest.find('/')?;
    let host = &rest[..slash];
    if host.is_empty() {
        return None;
    }
    Some((host, &rest[slash..]))
}

fn host_allowed<S: AsRef<str>>(host: &str, local_names: &[S]) -> bool {
    if host.is_empty() {
        return true;
    }
    // userinfo (`user@host`) and a percent-encoded host are not local names.
    if host.contains(['@', '%']) {
        return false;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if bare.eq_ignore_ascii_case("::1") {
        return true;
    }
    // A colon here is a port, or an IPv6 address we did not recognise.
    if bare.contains(':') {
        return false;
    }
    const ALWAYS: &[&str] = &["localhost", "127.0.0.1"];
    if ALWAYS.iter().any(|n| bare.eq_ignore_ascii_case(n)) {
        return true;
    }
    local_names
        .iter()
        .any(|n| bare.eq_ignore_ascii_case(n.as_ref()))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return None;
            }
            let h = hex_val(b[i + 1])?;
            let l = hex_val(b[i + 2])?;
            out.push((h << 4) | l);
            i += 3;
        } else if b[i] < 0x20 || b[i] == 0x7f {
            return None;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `/C:/x` → `C:\x` through [`crate::term::accept_local_cwd`].
///
/// A file-URL path starts with `/`. Stripping that one slash leaves a
/// drive path, or a UNC / device path (`//server/share`, `/\??\…`) which
/// the shared gate rejects. Nothing here calls `is_dir`.
fn windows_drive_path(decoded: &str) -> Option<String> {
    let rest = decoded.strip_prefix('/')?;
    crate::term::accept_local_cwd(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    use crate::term::{process_chunk, SessionMeta, TermSize};

    fn parse(payload: &str) -> Option<String> {
        parse_osc7(payload, &["BayanPC"])
    }

    fn fresh() -> (Term<VoidListener>, Processor, SessionMeta, Vec<u8>) {
        let size = TermSize { cols: 40, rows: 6 };
        (
            Term::new(Config::default(), &size, VoidListener),
            Processor::new(),
            SessionMeta::default(),
            Vec::new(),
        )
    }

    #[test]
    fn osc7_bel_and_st_terminators() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        // Split across reads, BEL terminator.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]7;file:///C:/Users/Admin",
        );
        assert!(
            meta.cwd.is_none(),
            "unterminated OSC 7 is carried, not stored"
        );
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"/Bayan\x07");
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\Users\Admin\Bayan"));
        assert_eq!(meta.reported_cwd(), Some(r"C:\Users\Admin\Bayan"));

        // ST (ESC \) terminator, in one read. The space after the semicolon
        // is the spelling in the recommendation.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]7 ; file:///D:/work\x1b\\",
        );
        assert_eq!(meta.cwd.as_deref(), Some(r"D:\work"));

        // A rejected report must not wipe a directory we already trusted.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]7;file://evil.example/C:/Windows\x07",
        );
        assert_eq!(meta.cwd.as_deref(), Some(r"D:\work"));

        // OSC 9;9 after OSC 7 wins (it is the later report). OSC 7 after
        // OSC 9;9 wins the other way. The two scanners have to agree on order.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]7;file:///C:/from-osc7\x07\x1b]9;9;C:\\from-osc99\x07",
        );
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\from-osc99"));
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]9;9;C:\\old\x07\x1b]7;file:///E:/new\x07",
        );
        assert_eq!(meta.cwd.as_deref(), Some(r"E:\new"));
    }

    #[test]
    fn osc7_percent_decoding_windows_drive_and_arabic() {
        assert_eq!(
            parse("file:///C:/Program%20Files"),
            Some(r"C:\Program Files".to_string())
        );
        assert_eq!(
            parse("file:///C:/Program%20Files/"),
            Some(r"C:\Program Files".to_string()),
            "a trailing slash is the directory, not part of the name"
        );
        // بيان
        assert_eq!(
            parse("file:///D:/%D8%A8%D9%8A%D8%A7%D9%86"),
            Some("D:\\بيان".to_string())
        );
        assert_eq!(
            parse("file://localhost/C:/a%20b"),
            Some(r"C:\a b".to_string())
        );
        assert_eq!(parse("file://127.0.0.1/C:/x"), Some(r"C:\x".to_string()));
        assert_eq!(parse("file://[::1]/C:/x"), Some(r"C:\x".to_string()));
        assert_eq!(
            parse("file://BayanPC/C:/src"),
            Some(r"C:\src".to_string()),
            "the machine name is a local host"
        );
        assert_eq!(parse("file://bayanpc/c:/src"), Some(r"c:\src".to_string()));
        assert_eq!(parse("file:///C:/"), Some(r"C:\".to_string()));
        assert_eq!(parse("FILE:///C:/x"), Some(r"C:\x".to_string()));
        // lower-case hex digits
        assert_eq!(parse("file:///C:/a%2fb"), Some(r"C:\a\b".to_string()));
    }

    #[test]
    fn osc7_rejects_remote_unc_schemes_controls_and_overlong() {
        let rejected = [
            "file://evil.example/C:/Windows",
            "file://server/share/secret",
            "file:////server/share",
            "http://localhost/C:/Windows",
            "https://127.0.0.1/C:/x",
            "javascript:alert(1)",
            "file:///C:/x y",
            "file:///etc/passwd",
            "file://user@localhost/C:/x",
            "file://localhost:445/C:/x",
            // UNC and device paths, percent-encoded so they survive the URL.
            "file:///%5C%5Cserver%5Cshare",
            "file:///%5C%5C%3F%5CC%3A%5CWindows",
            "file:///%5C%3F%3F%5CC%3A%5CWindows",
            // raw and encoded controls, and an RLO (U+202E) both ways
            "file:///C:/a\nb",
            "file:///C:/a%0Ab",
            "file:///C:/a%00b",
            "file:///C:/a\u{202e}b",
            "file:///C:/a%E2%80%AEb",
            "file:///C:/a%E2%81%A6b", // U+2066 LRI
            "file:///C:/a?b",
            "file:///C:/a#b",
            "file:///C:/..\\..\\Windows",
        ];
        for payload in rejected {
            assert_eq!(parse(payload), None, "accepted {payload:?}");
        }
        let mut huge = String::from("file:///C:/");
        huge.extend(std::iter::repeat_n('a', OSC7_MAX_PAYLOAD));
        assert!(huge.len() > OSC7_MAX_PAYLOAD);
        assert_eq!(parse(&huge), None, "over-long payload");

        // A good report survives a later hostile one at the parse layer too,
        // not only through process_chunk (covered above).
        assert_eq!(parse("file://other/C:/src"), None);
    }

    #[test]
    fn spawn_cwd_uses_reported_directory_only_if_it_exists() {
        let mut meta = SessionMeta {
            cwd: Some(r"C:\this\does\not\exist\bayan-osc7".to_string()),
            ..SessionMeta::default()
        };
        assert_eq!(
            meta.reported_cwd(),
            Some(r"C:\this\does\not\exist\bayan-osc7"),
            "hyperlink resolution sees the report even when spawn will not use it"
        );
        assert!(meta.spawn_cwd().is_none());

        let dir = std::env::temp_dir();
        let text = dir.to_str().expect("temp dir is unicode");
        meta.cwd = Some(text.to_string());
        // accept_local_cwd drops a trailing slash before the existence check.
        let expected = crate::term::accept_local_cwd(text).expect("temp is a drive path");
        assert!(crate::osc7::directory_exists(&expected));
        assert_eq!(meta.spawn_cwd().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn powershell_prompt_emits_osc7() {
        let s = crate::term::shell_integration_ps("abc123");
        assert!(
            s.contains("]7;file://"),
            "PowerShell prompt must emit OSC 7"
        );
        assert!(s.contains("]9;9;"), "the ConEmu OSC 9;9 report stays");
        assert!(
            s.contains("]133;A;abc123"),
            "prompt start carries the session nonce"
        );
        assert!(
            s.contains("]133;D;$c;abc123"),
            "command end carries the exit code and the nonce"
        );
    }
}
