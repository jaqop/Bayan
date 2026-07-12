//! Claude mode: reverse UAX#9 rule L2 — visual back to logical.
//!
//! Claude Code (Ink) applies BiDi itself on Windows and emits Arabic in
//! REVERSED, VISUAL order. Feeding that to a shaping engine that applies
//! BiDi again produces mangled text. These functions restore logical order
//! so cosmic-text's own BiDi then renders correctly. Ported line-for-line
//! from EasyTer, where this logic carried daily Arabic Claude sessions.
//! (PowerShell itself emits logical order — this only runs for Claude.)

/// Arabic LETTERS only: Arabic-Indic and Persian digits are numbers — like
/// 0-9 they form weak LTR runs and must keep digit order (١٧٥ stays ١٧٥).
pub fn is_arabic_letter(c: char) -> bool {
    let o = c as u32;
    if (0x0660..=0x0669).contains(&o) || (0x06F0..=0x06F9).contains(&o) {
        return false;
    }
    matches!(o,
        0x0600..=0x06FF | 0x0750..=0x077F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

fn is_ltr_char(c: char) -> bool {
    let o = c as u32;
    matches!(o,
        0x41..=0x5A | 0x61..=0x7A | 0x30..=0x39 | 0xC0..=0x2AF
        | 0x0660..=0x0669 | 0x06F0..=0x06F9)
}

/// Punctuation that stays inside an LTR island (paths, filenames, versions).
const LTR_PUNCT: &[char] = &['.', '_', '-', '/', ':', '\\', '@', '~', '+', '=', '#', '&', '%'];

/// Combining marks (Arabic tashkeel + generic): must travel WITH their base
/// character when reversing, or shadda/tanwin land on the wrong letter.
fn is_combining(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670
        | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
        | 0x08D3..=0x08FF)
}

/// Reverse character order keeping each base char glued to its combining
/// marks — a plain reverse would put marks before their base.
fn rev_clusters(s: &str) -> String {
    let mut units: Vec<String> = Vec::new();
    for ch in s.chars() {
        match units.last_mut() {
            Some(last) if is_combining(ch) => last.push(ch),
            _ => units.push(ch.to_string()),
        }
    }
    units.reverse();
    units.concat()
}

/// Is the line predominantly Arabic (so Claude reversed the whole line)?
pub fn line_is_rtl_visual(text: &str) -> bool {
    let (mut ar, mut lt) = (0usize, 0usize);
    for ch in text.chars() {
        if is_arabic_letter(ch) {
            ar += 1;
        } else if is_ltr_char(ch) && ch.is_alphabetic() {
            lt += 1;
        }
    }
    ar > 0 && ar >= lt
}

/// Reverse L2 for an RTL-base line: reverse the whole line, then re-reverse
/// the LTR islands (Latin words, digits, paths). Punctuation joins an island
/// only when a Latin char follows, so "config.txt" survives intact but a
/// trailing "01." doesn't flip to ".01".
fn unbidi_rtl_line(line: &str) -> String {
    let rev: Vec<char> = rev_clusters(line).chars().collect();
    let n = rev.len();
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        if is_ltr_char(rev[i]) {
            let mut j = i;
            while j < n {
                let cj = rev[j];
                if is_ltr_char(cj) {
                    j += 1;
                } else if (LTR_PUNCT.contains(&cj) || cj == ' ')
                    && j + 1 < n
                    && is_ltr_char(rev[j + 1])
                {
                    j += 1;
                } else {
                    break;
                }
            }
            let island: String = rev[i..j].iter().collect();
            out.push_str(&rev_clusters(&island));
            i = j;
        } else {
            out.push(rev[i]);
            i += 1;
        }
    }
    out
}

/// LTR-base line with Arabic islands (e.g. an English sentence quoting
/// Arabic): reverse each Arabic run in place, leaving the rest untouched.
fn reverse_arabic_runs(line: &str) -> String {
    let ch: Vec<char> = line.chars().collect();
    let n = ch.len();
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        if is_arabic_letter(ch[i]) {
            let mut j = i;
            while j < n
                && (is_arabic_letter(ch[j])
                    || is_combining(ch[j])
                    || (ch[j] == ' ' && j + 1 < n && is_arabic_letter(ch[j + 1])))
            {
                j += 1;
            }
            let run: String = ch[i..j].iter().collect();
            out.push_str(&rev_clusters(&run));
            i = j;
        } else {
            out.push(ch[i]);
            i += 1;
        }
    }
    out
}

/// Convert Claude's visual line to logical. None = leave unchanged.
pub fn restore_bidi_line(text: &str) -> Option<String> {
    if line_is_rtl_visual(text) {
        return Some(unbidi_rtl_line(text));
    }
    if text.chars().any(is_arabic_letter) {
        return Some(reverse_arabic_runs(text));
    }
    None
}

/// A whole copied block (selection in Claude mode): restore each line, so
/// pasted text is logical-order Arabic, not Claude's visual order.
pub fn restore_block(text: &str) -> String {
    text.split('\n')
        .map(|l| restore_bidi_line(l).unwrap_or_else(|| l.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Is the command that entered the alternate screen Claude itself? Other
/// full-screen tools (vim, less, htop) emit LOGICAL Arabic and must NOT be
/// reversed. Matches the INVOKED PROGRAM (basename, extension stripped),
/// allowing runners: `claude`, `npx claude`, `C:\...\claude.exe` match;
/// `vim claude.py` and `git log claude` do not.
pub fn cmd_is_claude(cmd: &str) -> bool {
    fn prog(tok: &str) -> String {
        let base = tok
            .trim_matches(|c| c == '"' || c == '\'')
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_lowercase();
        for ext in [".exe", ".cmd", ".bat", ".ps1"] {
            if let Some(s) = base.strip_suffix(ext) {
                return s.to_string();
            }
        }
        base
    }
    let mut toks = cmd.split_whitespace();
    let Some(first) = toks.next() else {
        return false;
    };
    let first = prog(first);
    if first == "claude" {
        return true;
    }
    const RUNNERS: &[&str] = &[
        "&", "npx", "npm", "pnpm", "bun", "node", "py", "python", "python3", "uv", "uvx",
    ];
    RUNNERS.contains(&first.as_str()) && toks.any(|t| prog(t) == "claude")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_arabic_round_trips() {
        // what Claude emits (visual) restores to logical
        assert_eq!(restore_bidi_line("ابحرم").as_deref(), Some("مرحبا"));
        // and reversing logical twice is identity
        assert_eq!(unbidi_rtl_line(&rev_clusters("مرحبا بالعالم")), "مرحبا بالعالم");
    }

    #[test]
    fn digits_keep_their_order() {
        // logical "رقم ١٧٥": digits are a weak-LTR island inside the RTL line
        assert_eq!(unbidi_rtl_line("١٧٥ مقر"), "رقم ١٧٥");
    }

    #[test]
    fn ltr_islands_survive() {
        // an RTL-base line quoting a filename: the island must not flip
        let logical = "افتح ملف config.txt رجاء";
        // construct Claude's visual output: reverse all, islands stay LTR
        let visual = {
            let rev = rev_clusters(logical);
            // config.txt got reversed with everything; Claude keeps it LTR,
            // so un-reverse it in the constructed input
            rev.replace("txt.gifnoc", "config.txt")
        };
        assert_eq!(restore_bidi_line(&visual).as_deref(), Some(logical));
    }

    #[test]
    fn ltr_base_reverses_arabic_runs_in_place() {
        // EasyTer's canonical example: English line with a reversed Arabic tail
        assert_eq!(
            restore_bidi_line("What are you working on ؟كتدعاسم").as_deref(),
            Some("What are you working on مساعدتك؟")
        );
        assert_eq!(restore_bidi_line("pure english"), None);
    }

    #[test]
    fn combining_marks_travel_with_their_base() {
        // معاً = م ع ا + tanwin on the ا: reversal keeps the mark attached
        let logical = "\u{645}\u{639}\u{627}\u{64b}";
        let visual = rev_clusters(logical);
        assert_eq!(restore_bidi_line(&visual).as_deref(), Some(logical));
    }

    #[test]
    fn copied_blocks_restore_line_by_line() {
        let block = "ابحرم\nplain english\nWhat are you working on ؟كتدعاسم";
        assert_eq!(
            restore_block(block),
            "مرحبا\nplain english\nWhat are you working on مساعدتك؟"
        );
    }

    #[test]
    fn claude_detection_matches_easyter_spec() {
        assert!(cmd_is_claude("claude"));
        assert!(cmd_is_claude("claude --continue"));
        assert!(cmd_is_claude("npx claude"));
        assert!(cmd_is_claude(r#""C:\tools\claude.exe" chat"#));
        assert!(cmd_is_claude("& C:\\Users\\x\\claude.cmd"));
        assert!(!cmd_is_claude("vim claude.py"));
        assert!(!cmd_is_claude("less claude.txt"));
        assert!(!cmd_is_claude("git log claude"));
        assert!(!cmd_is_claude(""));
        assert!(!cmd_is_claude("htop"));
    }
}
