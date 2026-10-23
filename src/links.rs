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

/// What activating a link is allowed to do. Deciding this once, here, is
/// what keeps an arbitrary string out of `ShellExecuteW`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// an http(s) URL: hand it to the browser
    Browse(String),
    /// a document or an image: association-open is safe
    Open(PathBuf),
    /// anything else that exists: show it in Explorer, never run it
    Reveal(PathBuf),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Browse(u) => f.write_str(u),
            Target::Open(p) | Target::Reveal(p) => write!(f, "{}", p.display()),
        }
    }
}

/// Extensions `ShellExecuteW` "open" may receive. "Open" RUNS an
/// executable, and terminal output is not trusted: without this gate a
/// hostile `setup.cmd` in the cwd is one Ctrl+click from running, under
/// an underline that advertises it as a safe link. Documents and images
/// only — everything else is revealed in Explorer instead.
const ALLOWED_EXTS: &[&str] = &[
    ".txt", ".md", ".log", ".json", ".pdf", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp",
];

/// The filename the extension check runs on. Windows ignores trailing
/// dots and spaces, so `x.exe ` is still an executable. A colon in the
/// leaf is an alternate data stream (`a.txt:evil.exe`), which names no
/// allowlisted extension at all.
fn leaf_for_extension(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let file = file.trim_end_matches([' ', '.']);
    if file.contains(':') {
        return String::new();
    }
    file.to_ascii_lowercase()
}

fn is_allowlisted(path: &str) -> bool {
    let leaf = leaf_for_extension(path);
    ALLOWED_EXTS.iter().any(|ext| leaf.len() > ext.len() && leaf.ends_with(ext))
}

/// A UNC or NT device path. Stat'ing a UNC path opens an SMB connection
/// to a host named by program output: the user's credentials go to it,
/// and the pointer move that triggered it freezes the window until the
/// connect times out. Nothing a link can offer is worth that.
fn unc_or_device(s: &str) -> bool {
    let norm: String = s.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
    let lower = norm.to_ascii_lowercase();
    lower.starts_with("\\\\") || lower.starts_with("\\??\\")
}

/// Characters a link path may not contain. The reveal argument is
/// `/select,"<path>"`, so a quote or a comma would break out of it; a
/// BiDi override makes `payload.exe` read as `payload.txt` on screen,
/// which is the oldest trick there is for earning a click.
fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '"' | ',')
        || matches!(c as u32, 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
}

/// The real existence check. Named so the injected one in tests and this
/// one cannot drift apart.
fn on_disk(p: &Path) -> bool {
    p.is_file()
}

/// A detected link, resolved into something that can be activated.
pub fn resolve(link: &Link, cwd: Option<&str>) -> Option<Target> {
    resolve_with(link, cwd, &on_disk)
}

fn resolve_with(
    link: &Link,
    cwd: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<Target> {
    match link {
        // find_links only ever builds http(s) URLs. Re-checking here means
        // a later caller cannot route file:// or javascript: to the shell.
        Link::Url(u) => {
            let lower = u.to_ascii_lowercase();
            (lower.starts_with("http://") || lower.starts_with("https://"))
                .then(|| Target::Browse(u.clone()))
        }
        Link::Path(p) => {
            let full = resolve_path_with(p, cwd, exists)?;
            Some(if is_allowlisted(full.to_str()?) {
                Target::Open(full)
            } else {
                Target::Reveal(full)
            })
        }
    }
}

/// Turn a path token into an existing file: strip a `:line` suffix, expand
/// `~`, resolve a relative path against the shell's cwd. None unless the
/// file is really there — nothing that cannot open is offered as a link.
/// The gates run BEFORE `exists`, so a refused path is never stat'd.
fn resolve_path_with(
    tok: &str,
    cwd: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // "src\main.rs:42" → "src\main.rs" (compiler / grep / pytest style)
    let base = match tok.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => tok,
    };
    if base.is_empty() || base.chars().any(is_forbidden) || unc_or_device(base) {
        return None;
    }
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
    // the cwd, or an expanded `~`, can itself be a UNC path: gate the
    // joined result too, still before the disk is touched
    let text = full.to_str()?;
    if text.chars().any(is_forbidden) || unc_or_device(text) {
        return None;
    }
    exists(&full).then_some(full)
}

/// The path half of [`resolve`] against the real filesystem — kept so the
/// disk-facing behaviour can be exercised on a real file.
#[cfg(test)]
fn resolve_path(tok: &str, cwd: Option<&str>) -> Option<PathBuf> {
    resolve_path_with(tok, cwd, &on_disk)
}

/// Act on a resolved target: the browser or the associated app for the
/// first two, Explorer's "show me this file" for the third.
#[cfg(windows)]
pub fn activate(target: &Target) -> bool {
    match target {
        Target::Browse(u) => shell_open(u),
        Target::Open(p) => p.to_str().is_some_and(shell_open),
        Target::Reveal(p) => reveal(p),
    }
}

#[cfg(not(windows))]
pub fn activate(_target: &Target) -> bool {
    false
}

/// Hand a target to whatever the system associates with it. ShellExecuteW
/// rather than `cmd /c start`: no console flash, and the `windows-sys`
/// Shell feature is already linked for the tray icon.
#[cfg(windows)]
fn shell_open(target: &str) -> bool {
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

/// The full path to explorer.exe, never a bare "explorer" — that would
/// search the working directory first, and the working directory is
/// wherever the user's shell happens to be.
#[cfg(windows)]
fn explorer_program() -> PathBuf {
    const FALLBACK: &str = r"C:\Windows\explorer.exe";
    let dir = std::env::var("WINDIR").unwrap_or_default();
    let p = Path::new(&dir);
    let usable = !dir.chars().any(is_forbidden)
        && !unc_or_device(&dir)
        && p.is_absolute()
        && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Windows"));
    if usable {
        p.join("explorer.exe")
    } else {
        PathBuf::from(FALLBACK)
    }
}

/// Show a file in Explorer with it selected, without opening it: what a
/// link to something Bayan will not hand to the shell does instead.
#[cfg(windows)]
fn reveal(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let Some(p) = path.to_str() else { return false };
    // the path has already refused quote and comma, so it cannot break
    // out of the quotes around /select's argument
    std::process::Command::new(explorer_program())
        .raw_arg(format!(r#"/select,"{p}""#))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .is_ok()
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

    /// ShellExecuteW "open" RUNS an executable. Terminal output is not
    /// trusted, and the hover underline advertises a link as safe, so the
    /// association-open set is an allowlist of documents and images;
    /// everything else is revealed in Explorer instead.
    #[test]
    fn an_executable_is_never_association_opened() {
        for bad in [
            "payload.exe", "invoice.pdf.exe", "setup.cmd", "run.bat", "x.ps1",
            "x.lnk", "x.hta", "x.scr", "x.js", "x.vbs", "x.msi", "x.reg",
            // Windows ignores trailing dots and spaces: this is still x.exe
            "x.exe ", "x.exe.", "x.exe. .",
            // a colon in the leaf is an alternate data stream, not an extension
            "notes.txt:payload.exe",
            // no extension to vouch for it
            "Makefile",
        ] {
            assert!(!is_allowlisted(bad), "{bad} must not association-open");
        }
        assert!(!is_allowlisted(".txt"), "a bare extension is not a filename");
        for ok in [
            "notes.txt", "README.md", "build.log", "a.json", "doc.pdf",
            "shot.PNG", "p.jpg", "p.jpeg", "a.gif", "a.webp", "a.bmp",
        ] {
            assert!(is_allowlisted(ok), "{ok} should association-open");
        }
    }

    /// A UNC stat opens an SMB connection to a host named by program
    /// output — the user's credentials, and a multi-second UI freeze, for
    /// a pointer move. The gate must come BEFORE the disk check.
    #[test]
    fn a_unc_path_is_refused_without_ever_touching_the_disk() {
        use std::cell::RefCell;
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        // "everything exists", so the gate is the only thing that can refuse
        let exists = |p: &Path| {
            seen.borrow_mut().push(p.display().to_string());
            true
        };
        for unc in [
            r"\\attacker\share\doc.txt",
            r"//attacker/share/doc.txt",
            r"\\?\UNC\attacker\share\doc.txt",
            r"\\.\pipe\doc.txt",
            r"\??\UNC\attacker\share\doc.txt",
        ] {
            assert_eq!(resolve_path_with(unc, None, &exists), None, "{unc}");
        }
        // a relative path under a UNC cwd is still a UNC path
        assert_eq!(resolve_path_with("doc.txt", Some(r"\\attacker\share"), &exists), None);
        assert!(seen.borrow().is_empty(), "stat'd a UNC path: {:?}", seen.borrow());
        // a local path still resolves, and is the only thing stat'd
        assert_eq!(
            resolve_path_with(r"C:\x\doc.txt", None, &exists),
            Some(PathBuf::from(r"C:\x\doc.txt"))
        );
        assert_eq!(seen.borrow().len(), 1);
    }

    /// The reveal argument is `/select,"<path>"`, and an RLO mark makes
    /// `payload.exe` read as `payload.txt` on screen. Neither may survive
    /// into a path that gets handed to the shell.
    #[test]
    fn a_quote_comma_or_bidi_mark_in_a_path_is_refused() {
        let exists = |_: &Path| true;
        for bad in [
            "C:\\x\\a,b.txt",
            "C:\\x\\a\"b.txt",
            // RLO: renders as "payloadtxt.exe" reversed -> "payload.txt"
            "C:\\x\\payload\u{202e}txt.exe",
            "C:\\x\\a\u{200f}b.txt",
            "C:\\x\\a\u{2066}b.txt",
            "C:\\x\\a\u{1}b.txt",
        ] {
            assert_eq!(resolve_path_with(bad, None, &exists), None, "{bad:?}");
        }
        assert_eq!(
            resolve_path_with(r"C:\x\a-b.txt", None, &exists),
            Some(PathBuf::from(r"C:\x\a-b.txt")),
            "an ordinary path beside them still resolves"
        );
    }

    /// What activation is allowed to do is decided here, once, so no
    /// caller can hand an arbitrary string to the shell.
    #[test]
    fn resolve_decides_what_activation_may_do() {
        let exists = |_: &Path| true;
        let path = |tok: &str| resolve_with(&Link::Path(tok.into()), Some(r"C:\w"), &exists);
        assert_eq!(path("notes.txt"), Some(Target::Open(PathBuf::from(r"C:\w\notes.txt"))));
        assert_eq!(path("payload.exe"), Some(Target::Reveal(PathBuf::from(r"C:\w\payload.exe"))));
        assert_eq!(path(r"\\host\share\notes.txt"), None);
        let url = |u: &str| resolve_with(&Link::Url(u.into()), None, &exists);
        assert_eq!(url("https://a.b/c"), Some(Target::Browse("https://a.b/c".into())));
        assert_eq!(url("HTTP://a.b/c"), Some(Target::Browse("HTTP://a.b/c".into())));
        // find_links only ever builds http(s) Urls; the check is here anyway
        // so that a future caller cannot route a scheme past the shell
        assert_eq!(url("javascript:alert(1)"), None);
        assert_eq!(url("file:///C:/x"), None);
    }
}
