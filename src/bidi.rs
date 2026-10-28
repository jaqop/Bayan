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
const LTR_PUNCT: &[char] = &[
    '.', '_', '-', '/', ':', '\\', '@', '~', '+', '=', '#', '&', '%',
];

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
/// `origin[i]` is the caller's index of `chars[i]`; it travels with the
/// char so a later hit-test can point a displayed glyph back at the cell
/// Claude actually stored.
fn rev_indexed(chars: &[char], origin: &[usize]) -> (Vec<char>, Vec<usize>) {
    let mut units: Vec<Vec<usize>> = Vec::new();
    for (i, ch) in chars.iter().enumerate() {
        if is_combining(*ch) {
            if let Some(last) = units.last_mut() {
                last.push(i);
                continue;
            }
        }
        units.push(vec![i]);
    }
    units.reverse();
    let mut out_c = Vec::with_capacity(chars.len());
    let mut out_o = Vec::with_capacity(chars.len());
    for unit in units {
        for i in unit {
            out_c.push(chars[i]);
            out_o.push(origin[i]);
        }
    }
    (out_c, out_o)
}

#[cfg(test)]
fn rev_clusters(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let origin: Vec<usize> = (0..chars.len()).collect();
    rev_indexed(&chars, &origin).0.into_iter().collect()
}

/// Is the line predominantly Arabic (so Claude reversed the whole line)?
pub fn line_is_rtl_visual(text: &str) -> bool {
    let (mut ar, mut lt) = (0usize, 0usize);
    let (mut first_arabic, mut last_arabic) = (None, None);
    for ch in text.chars() {
        if is_arabic_letter(ch) {
            ar += 1;
            first_arabic.get_or_insert(true);
            last_arabic = Some(true);
        } else if is_ltr_char(ch) && ch.is_alphabetic() {
            lt += 1;
            first_arabic.get_or_insert(false);
            last_arabic = Some(false);
        }
    }
    // A Latin island cannot change an Arabic paragraph into an English
    // paragraph merely by growing. Arabic at both strong-text edges
    // establishes the base even when the island contains most letters.
    (first_arabic == Some(true) && last_arabic == Some(true)) || (ar > 0 && ar >= lt)
}

/// Reverse L2 for an RTL-base line: reverse the whole line, then re-reverse
/// the LTR islands (Latin words, digits, paths). Punctuation joins an island
/// only when a Latin char follows, so "config.txt" survives intact but a
/// trailing "01." doesn't flip to ".01".
/// The second vec is the index of each output char in `chars`.
fn unbidi_indexed(chars: &[char]) -> (Vec<char>, Vec<usize>) {
    let origin: Vec<usize> = (0..chars.len()).collect();
    let (rev, rev_i) = rev_indexed(chars, &origin);
    let n = rev.len();
    let mut out = Vec::new();
    let mut out_i = Vec::new();
    let mut i = 0;
    while i < n {
        if is_ltr_char(rev[i]) {
            let mut j = i;
            while j < n {
                let cj = rev[j];
                let joins_island =
                    (LTR_PUNCT.contains(&cj) || cj == ' ') && j + 1 < n && is_ltr_char(rev[j + 1]);
                if is_ltr_char(cj) || joins_island {
                    j += 1;
                } else {
                    break;
                }
            }
            let (island, island_i) = rev_indexed(&rev[i..j], &rev_i[i..j]);
            out.extend(island);
            out_i.extend(island_i);
            i = j;
        } else {
            out.push(rev[i]);
            out_i.push(rev_i[i]);
            i += 1;
        }
    }
    (out, out_i)
}

#[cfg(test)]
fn unbidi_rtl_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    unbidi_indexed(&chars).0.into_iter().collect()
}

/// LTR-base line with Arabic islands (e.g. an English sentence quoting
/// Arabic): reverse each Arabic run in place, leaving the rest untouched.
fn reverse_arabic_indexed(chars: &[char]) -> (Vec<char>, Vec<usize>) {
    let n = chars.len();
    let mut out = Vec::new();
    let mut out_i = Vec::new();
    let mut i = 0;
    while i < n {
        if is_arabic_letter(chars[i]) {
            let mut j = i;
            while j < n
                && (is_arabic_letter(chars[j])
                    || is_combining(chars[j])
                    || (chars[j] == ' ' && j + 1 < n && is_arabic_letter(chars[j + 1])))
            {
                j += 1;
            }
            let origin: Vec<usize> = (i..j).collect();
            let (run, run_i) = rev_indexed(&chars[i..j], &origin);
            out.extend(run);
            out_i.extend(run_i);
            i = j;
        } else {
            out.push(chars[i]);
            out_i.push(i);
            i += 1;
        }
    }
    (out, out_i)
}

/// Convert Claude's visual line to logical, and record where each output
/// char sat in `text`. Mouse hit-testing needs the index: the shaped glyph
/// is in restored order, the grid cell is still Claude's visual column.
/// None = leave unchanged.
pub fn restore_bidi_map(text: &str) -> Option<(String, Vec<usize>)> {
    let chars: Vec<char> = text.chars().collect();
    if line_is_rtl_visual(text) {
        let (out, idx) = unbidi_indexed(&chars);
        return Some((out.into_iter().collect(), idx));
    }
    if chars.iter().copied().any(is_arabic_letter) {
        let (out, idx) = reverse_arabic_indexed(&chars);
        return Some((out.into_iter().collect(), idx));
    }
    None
}

/// Convert Claude's visual line to logical. None = leave unchanged.
pub fn restore_bidi_line(text: &str) -> Option<String> {
    restore_bidi_map(text).map(|(s, _)| s)
}

/// The same restoration with source-character identity retained, so repeated
/// letters and combining marks can still map back to terminal cells.
pub fn restore_bidi_line_with_order(text: &str) -> Option<(String, Vec<usize>)> {
    let chars: Vec<char> = text.chars().collect();
    if !chars.iter().copied().any(is_arabic_letter) {
        return None;
    }
    let reverse = |indices: &[usize]| {
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        for &i in indices {
            if is_combining(chars[i]) && !clusters.is_empty() {
                clusters.last_mut().unwrap().push(i);
            } else {
                clusters.push(vec![i]);
            }
        }
        clusters.into_iter().rev().flatten().collect::<Vec<_>>()
    };
    let indices: Vec<usize> = (0..chars.len()).collect();
    let mut order = Vec::with_capacity(chars.len());
    if line_is_rtl_visual(text) {
        let rev = reverse(&indices);
        let mut i = 0;
        while i < rev.len() {
            if is_ltr_char(chars[rev[i]]) {
                let mut j = i;
                while j < rev.len() {
                    let c = chars[rev[j]];
                    let joins = (LTR_PUNCT.contains(&c) || c == ' ')
                        && j + 1 < rev.len()
                        && is_ltr_char(chars[rev[j + 1]]);
                    if !is_ltr_char(c) && !joins {
                        break;
                    }
                    j += 1;
                }
                order.extend(reverse(&rev[i..j]));
                i = j;
            } else {
                order.push(rev[i]);
                i += 1;
            }
        }
    } else {
        let mut i = 0;
        while i < chars.len() {
            if is_arabic_letter(chars[i]) {
                let mut j = i;
                while j < chars.len()
                    && (is_arabic_letter(chars[j])
                        || is_combining(chars[j])
                        || (chars[j] == ' '
                            && j + 1 < chars.len()
                            && is_arabic_letter(chars[j + 1])))
                {
                    j += 1;
                }
                order.extend(reverse(&indices[i..j]));
                i = j;
            } else {
                order.push(i);
                i += 1;
            }
        }
    }
    Some((order.iter().map(|&i| chars[i]).collect(), order))
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
    fn restoration_keeps_a_permutation_for_repeated_letters_and_marks() {
        for text in [
            "ابحرم",
            "ملس ملس",
            "مَّلَس",
            "quoted ملس then ملس",
            "txt.1 رابتخا 123",
        ] {
            let (restored, order) = restore_bidi_line_with_order(text).unwrap();
            assert_eq!(Some(restored.clone()), restore_bidi_line(text));
            let chars: Vec<_> = text.chars().collect();
            assert_eq!(
                order.iter().map(|&i| chars[i]).collect::<String>(),
                restored
            );
            let mut indices = order;
            indices.sort_unstable();
            assert_eq!(indices, (0..chars.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn pure_arabic_round_trips() {
        // what Claude emits (visual) restores to logical
        assert_eq!(restore_bidi_line("ابحرم").as_deref(), Some("مرحبا"));
        // and reversing logical twice is identity
        assert_eq!(
            unbidi_rtl_line(&rev_clusters("مرحبا بالعالم")),
            "مرحبا بالعالم"
        );
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

    /// `tests/fixtures/bidi_cases.json` is published as a language-agnostic
    /// corpus (shared with other terminals) and was generated FROM this
    /// module. Nothing re-checked it after that: a change here would leave
    /// the corpus quietly wrong. Every pair must still round-trip.
    #[test]
    fn the_published_fixture_corpus_still_round_trips() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/bidi_cases.json"
        );
        let text = std::fs::read_to_string(path).expect("fixture corpus present");
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let cases = v["cases"].as_array().expect("cases array");
        assert!(cases.len() >= 6, "corpus shrank to {} cases", cases.len());
        for c in cases {
            let name = c["name"].as_str().unwrap_or("?");
            let logical = c["logical"].as_str().expect("logical");
            let visual = c["visual"].as_str().expect("visual");
            let got = restore_bidi_line(visual).unwrap_or_else(|| visual.to_string());
            assert_eq!(got, logical, "fixture {name}: visual -> logical drifted");
            let mapped = restore_bidi_line_with_order(visual)
                .map(|r| r.0)
                .unwrap_or_else(|| visual.to_string());
            assert_eq!(
                mapped, logical,
                "fixture {name}: mapped restoration drifted"
            );
            // the codepoint listings must describe the strings they sit beside
            let cps = |s: &str| {
                s.chars()
                    .map(|ch| format!("{:04X}", ch as u32))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            assert_eq!(
                cps(logical),
                c["logical_codepoints"].as_str().unwrap_or(""),
                "{name}"
            );
            assert_eq!(
                cps(visual),
                c["visual_codepoints"].as_str().unwrap_or(""),
                "{name}"
            );
        }
        let ml = &v["multiline_case"];
        assert_eq!(
            restore_block(ml["visual_block"].as_str().unwrap()),
            ml["logical_block"].as_str().unwrap()
        );
        let pg = &v["program_gating"];
        for m in pg["matches"].as_array().unwrap() {
            assert!(cmd_is_claude(m.as_str().unwrap()), "should match: {m}");
        }
        for m in pg["does_not_match"].as_array().unwrap() {
            assert!(!cmd_is_claude(m.as_str().unwrap()), "should not match: {m}");
        }
    }

    #[test]
    fn restore_map_points_each_output_char_at_its_source() {
        // visual "ابحرم" restores to logical "مرحبا"; every output char is
        // one of the input chars, not a new one, and the map round-trips.
        let visual = "ابحرم";
        let (out, idx) = restore_bidi_map(visual).unwrap();
        assert_eq!(out, "مرحبا");
        let src: Vec<char> = visual.chars().collect();
        let rebuilt: String = idx.iter().map(|&i| src[i]).collect();
        assert_eq!(rebuilt, out);
        // the displayed-left char after cosmic re-shapes this is ا, which
        // Claude stored at column 0
        assert_eq!(idx[idx.len() - 1], 0);
        // and م, stored at the last visual column, leads the logical line
        assert_eq!(idx[0], 4);

        // an LTR sentence with an Arabic tail: the English stays put
        let mixed = "What are you working on ؟كتدعاسم";
        let (out, idx) = restore_bidi_map(mixed).unwrap();
        assert_eq!(out, "What are you working on مساعدتك؟");
        let src: Vec<char> = mixed.chars().collect();
        let rebuilt: String = idx.iter().map(|&i| src[i]).collect();
        assert_eq!(rebuilt, out);
        assert!(idx.len() == src.len());
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

    #[test]
    fn arabic_base_survives_a_long_latin_island() {
        let visual = "ملاع Windows Terminal 123 ابحرم";
        assert_eq!(
            restore_bidi_line(visual).as_deref(),
            Some("مرحبا Windows Terminal 123 عالم")
        );
        assert_eq!(
            restore_bidi_line_with_order(visual).unwrap().0,
            "مرحبا Windows Terminal 123 عالم"
        );
        assert!(!line_is_rtl_visual("Hello ابحرم Windows Terminal"));
    }
}
