//! Clickable things in terminal output: http(s) URLs and file paths.
//!
//! Detection is pure text → spans, so it is testable without a grid. The
//! app maps a pointer position to a row's text, asks for the span under the
//! column, and (for paths) confirms the file exists before offering it —
//! only what can actually be opened gets an underline and a hand cursor.
//! This is the essential half of the "hints"/quick-select mode users ask
//! every terminal for; Ctrl+click is the gesture EasyTer taught its users.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Url(String),
    /// a path as written in the output (resolved against the cwd on use)
    Path(String),
}

/// Characters that end a URL or a path token.
fn is_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '|')
}

/// Trailing punctuation that belongs to the sentence, not the link:
/// "see https://x.y/z." or "(https://x.y/z)". A ')' is kept when the link
/// itself opened a '(' that it closes (Wikipedia-style URLs).
fn trim_trailing(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start {
        let c = chars[end - 1];
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' | ']' | '}' => {
                let (open, close) = match c {
                    ')' => ('(', ')'),
                    ']' => ('[', ']'),
                    _ => ('{', '}'),
                };
                let seg = &chars[start..end];
                seg.iter().filter(|&&x| x == open).count()
                    < seg.iter().filter(|&&x| x == close).count()
            }
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

/// Does a token look like a file reference? Either it carries a directory
/// separator, or it is `name.ext` with a short extension (compiler, pytest
/// and grep output). Existence is the caller's check.
fn looks_like_path(tok: &str) -> bool {
    if tok.contains('\\') || tok.contains('/') {
        return tok.len() > 1;
    }
    match tok.rfind('.') {
        Some(i) if i > 0 => {
            let ext = &tok[i + 1..];
            // a real extension has a letter in it: "3.14" and "v1.2.3" don't
            (1..=10).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        }
        _ => false,
    }
}

/// Every link on a line, as (char_start, char_end, link) — end exclusive.
/// Char indices equal grid columns when the caller emits one char per cell.
pub fn find_links(line: &str) -> Vec<(usize, usize, Link)> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if is_break(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && !is_break(chars[i]) {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        let lower = raw.to_ascii_lowercase();
        if let Some(p) = lower.find("http://").or_else(|| lower.find("https://")) {
            // a URL may be glued to a prefix like "url=" or "(" — it starts at
            // the scheme, and the paren balance is judged from there
            let s = start + lower[..p].chars().count();
            let end = trim_trailing(&chars, s, i);
            if end > s + 8 {
                out.push((s, end, Link::Url(chars[s..end].iter().collect())));
            }
        } else {
            let end = trim_trailing(&chars, start, i);
            if end > start {
                let tok: String = chars[start..end].iter().collect();
                if looks_like_path(&tok) {
                    out.push((start, end, Link::Path(tok)));
                }
            }
        }
    }
    out
}

/// The link whose span covers `col`, if any.
pub fn link_at(line: &str, col: usize) -> Option<(usize, usize, Link)> {
    find_links(line).into_iter().find(|(s, e, _)| *s <= col && col < *e)
}

/// Turn a path token into an existing file: strip a `:line` suffix, expand
/// `~`, resolve a relative path against the shell's cwd. None unless the
/// file is really there — nothing that cannot open is offered as a link.
pub fn resolve_path(tok: &str, cwd: Option<&str>) -> Option<PathBuf> {
    // "src\main.rs:42" → "src\main.rs" (compiler / grep / pytest style)
    let base = match tok.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => tok,
    };
    let expanded = match base.strip_prefix('~') {
        Some(rest) => {
            let home = std::env::var_os("USERPROFILE")?;
            format!("{}{}", home.to_string_lossy(), rest)
        }
        None => base.to_string(),
    };
    let p = Path::new(&expanded);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd?).join(p)
    };
    full.is_file().then_some(full)
}

/// Open a URL or file with whatever the system associates with it.
/// ShellExecuteW rather than `cmd /c start`: no console flash, and the
/// `windows-sys` Shell feature is already linked for the tray icon.
#[cfg(windows)]
pub fn open_external(target: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
    };
    let verb = wide("open");
    let what = wide(target);
    // SAFETY: both buffers are NUL-terminated and outlive the call; the
    // remaining pointers are documented as optional (null).
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            what.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    r as usize > 32 // the documented "success" threshold
}

#[cfg(not(windows))]
pub fn open_external(_target: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(line: &str) -> Vec<String> {
        find_links(line)
            .into_iter()
            .filter_map(|(_, _, l)| match l {
                Link::Url(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn urls_are_found_with_sentence_punctuation_stripped() {
        assert_eq!(urls("see https://example.com/a/b. then"), ["https://example.com/a/b"]);
        assert_eq!(urls("(http://x.y/z)"), ["http://x.y/z"]);
        assert_eq!(urls("url=https://a.b/c?d=1&e=2,"), ["https://a.b/c?d=1&e=2"]);
        // a URL that opens and closes its own paren keeps the ')'
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Foo_(bar)"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        // quotes and angle brackets end it
        assert_eq!(urls("<https://a.b/c> \"https://d.e/f\""), ["https://a.b/c", "https://d.e/f"]);
        assert_eq!(urls("no links here"), Vec::<String>::new());
        assert_eq!(urls("https://"), Vec::<String>::new(), "a bare scheme is not a link");
    }

    #[test]
    fn spans_map_to_columns_and_link_at_picks_by_column() {
        let line = "ok https://a.b/c and src\\main.rs:42 done";
        let links = find_links(line);
        assert_eq!(links[0].0, 3);
        assert_eq!(links[0].1, 3 + "https://a.b/c".len());
        assert_eq!(link_at(line, 5).unwrap().2, Link::Url("https://a.b/c".into()));
        assert_eq!(link_at(line, 2), None, "a space is not a link");
        assert_eq!(link_at(line, 25).unwrap().2, Link::Path("src\\main.rs:42".into()));
        assert_eq!(link_at(line, 38), None);
    }

    #[test]
    fn path_tokens_need_a_separator_or_an_extension() {
        let paths = |l: &str| -> Vec<String> {
            find_links(l)
                .into_iter()
                .filter_map(|(_, _, x)| match x {
                    Link::Path(p) => Some(p),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(paths("error in main.rs at line 3"), ["main.rs"]);
        assert_eq!(paths(r"C:\Users\Admin\x.txt and .\rel\dir"), [r"C:\Users\Admin\x.txt", r".\rel\dir"]);
        // plain words, numbers and version-like tokens are not paths
        assert_eq!(paths("total 3.14 items done"), Vec::<String>::new());
        assert_eq!(paths("v1.2.3 and 10.0.26100"), Vec::<String>::new(), "numeric tails are not extensions");
        assert_eq!(paths("a.b1"), ["a.b1"]);
    }

    #[test]
    fn resolving_checks_the_disk_and_understands_line_suffixes() {
        let dir = std::env::temp_dir().join("bayan_links_test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("note.txt");
        std::fs::write(&f, "x").unwrap();
        let cwd = dir.to_str().unwrap();
        assert_eq!(resolve_path("note.txt", Some(cwd)), Some(f.clone()));
        assert_eq!(resolve_path("note.txt:12", Some(cwd)), Some(f.clone()));
        assert_eq!(resolve_path(f.to_str().unwrap(), None), Some(f.clone()));
        assert_eq!(resolve_path("missing.txt", Some(cwd)), None);
        assert_eq!(resolve_path("note.txt", None), None, "relative with no cwd cannot resolve");
        assert_eq!(resolve_path(cwd, None), None, "a directory is not a file link");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
