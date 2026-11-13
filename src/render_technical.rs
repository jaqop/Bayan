//! Presentation-only direction isolation for technical tokens in prose.

use super::Seg;
use std::collections::BTreeSet;

/// Add controls only to the shaped view, never to the terminal grid. This
/// preserves copy/export bytes and keeps every real character's cell map.
pub(super) fn isolate(segs: &mut Vec<Seg>) -> Option<ByteMap> {
    let text: String = segs.iter().map(|s| s.text.as_str()).collect();
    // Applications supplying explicit direction controls own their policy.
    if text
        .chars()
        .any(|c| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
    {
        return None;
    }
    let mut starts = BTreeSet::new();
    let mut ends = BTreeSet::new();
    let mut offset = 0;
    for token in text.split_inclusive(char::is_whitespace) {
        let leading = token.trim_start_matches(['"', '\'', '(', '{']);
        let value = leading.trim_end_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | ')' | '}' | '.' | ',' | '،' | '؛' | '!' | '?'
                )
        });
        let drive = value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|b| matches!(b, b'/' | b'\\'))
            && value.as_bytes()[0].is_ascii_alphabetic();
        let path = drive
            || value.starts_with(['/', '\\'])
            || value.starts_with("./")
            || value.starts_with("../")
            || value.starts_with("~/")
            || value.contains("://");
        let number = value.bytes().any(|b| b.is_ascii_digit())
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || b"+-#$%.,:[]".contains(&b))
            && value.bytes().any(|b| !b.is_ascii_digit());
        if !value.is_empty() && (path || number) {
            let start = offset + token.len() - leading.len();
            starts.insert(start);
            ends.insert(start + value.len());
        }
        offset += token.len();
    }
    if starts.is_empty() {
        return None;
    }
    fn push(out: &mut Vec<Seg>, template: &Seg, c: char) {
        if let Some(last) = out
            .last_mut()
            .filter(|s| s.fg == template.fg && s.style == template.style)
        {
            last.text.push(c);
        } else {
            out.push(Seg {
                text: c.to_string(),
                fg: template.fg,
                style: template.style,
            });
        }
    }
    let mut out = Vec::new();
    let mut mapped = Vec::new();
    let (mut old, mut new) = (0, 0);
    for seg in segs.iter() {
        for c in seg.text.chars() {
            if ends.contains(&old) {
                push(&mut out, seg, '\u{2069}');
                new += 3;
            }
            if starts.contains(&old) {
                push(&mut out, seg, '\u{2066}');
                new += 3;
            }
            push(&mut out, seg, c);
            mapped.push((old, new, new + c.len_utf8()));
            old += c.len_utf8();
            new += c.len_utf8();
        }
    }
    if ends.contains(&old) {
        if let Some(seg) = segs.last() {
            push(&mut out, seg, '\u{2069}');
        }
    }
    *segs = out;
    Some(ByteMap(mapped))
}

/// Original character boundaries -> shaped boundaries, including multi-scalar
/// terminal cells and decoration ranges spanning more than one character.
pub(super) struct ByteMap(Vec<(usize, usize, usize)>);

impl ByteMap {
    pub(super) fn range(&self, start: &mut usize, end: &mut usize) {
        let first = self
            .0
            .binary_search_by_key(start, |&(old, _, _)| old)
            .expect("original range begins at a character");
        let last = self.0.partition_point(|&(old, _, _)| old < *end) - 1;
        (*start, *end) = (self.0[first].1, self.0[last].2);
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn cells(text: &str) -> Vec<CellInfo> {
        text.chars()
            .enumerate()
            .map(|(col, c)| CellInfo {
                col,
                c,
                w: 1,
                zw: None,
                fg: FG,
                bg: BG,
                style: 0,
                ul: FG,
            })
            .collect()
    }

    fn position(text: &str, needle: &str, r: &mut Renderer) -> f32 {
        let source_byte = text.find(needle).unwrap();
        let col = text[..source_byte].chars().count();
        let plan = bidi_plan(&cells(text), false, FG, Some(col), ParagraphDir::Rtl).unwrap();
        let pane = (0, 0, 4000, 100);
        r.draw_line_bidi(
            &mut Vec::new(),
            pane,
            0,
            0,
            pane.2,
            &cells(text),
            false,
            Some(col),
            ParagraphDir::Rtl,
            None,
            true,
        );
        // Use only the run that the real drawing function cached. Calling
        // ensure_shaped here would hide a missing live-path integration.
        let run = r
            .cache_hot
            .iter()
            .find(|(key, _)| key.0 == plan.segs)
            .map(|(_, run)| run)
            .expect("drawing must use the same mapped text as hit-testing");
        let byte = plan
            .byte_cols
            .iter()
            .find(|&&(_, _, c)| c == col)
            .unwrap()
            .0;
        run.buffer
            .layout_runs()
            .flat_map(|line| line.glyphs.iter())
            .find(|g| g.start <= byte && byte < g.end)
            .unwrap()
            .x
    }

    #[test]
    fn realistic_corpus_preserves_every_cell_and_cursor_mapping() {
        let corpus = include_str!("../tests/fixtures/mixed-arabic-english.txt");
        assert!(corpus.split_whitespace().count() >= 100);
        for line in corpus.lines().filter(|line| !line.is_empty()) {
            let cells = cells(line);
            for cursor in [0, cells.len() / 2, cells.len() - 1] {
                let plan = bidi_plan(&cells, false, FG, Some(cursor), ParagraphDir::Rtl).unwrap();
                let view: String = plan.segs.iter().map(|s| s.text.as_str()).collect();
                assert_eq!(
                    view.chars()
                        .filter(|c| !matches!(c, '\u{2066}' | '\u{2069}' | '\u{200f}'))
                        .collect::<String>(),
                    line
                );
                assert_eq!(
                    plan.byte_cols
                        .iter()
                        .filter(|&&(a, b, _)| &view[a..b] != "\u{200f}")
                        .count(),
                    cells.len()
                );
                for &(start, end, col) in &plan.byte_cols {
                    if &view[start..end] == "\u{200f}" {
                        continue;
                    }
                    assert_eq!(&view[start..end], cells[col].c.to_string());
                }
                let (start, end) = plan.cursor_bytes.unwrap();
                assert_eq!(&view[start..end], cells[cursor].c.to_string());
            }
        }
    }

    #[test]
    fn application_direction_controls_and_styles_are_preserved() {
        let text = "افتح \u{2066}/home/أحمد/a.txt\u{2069} الآن";
        let plan = bidi_plan(&cells(text), false, FG, None, ParagraphDir::Default).unwrap();
        assert_eq!(
            plan.segs
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>(),
            text
        );
        let mut source = cells("افتح /home/أحمد/a.txt الآن");
        for cell in &mut source {
            cell.style = if cell.col % 2 == 0 { ST_BOLD } else { 0 };
        }
        let plan = bidi_plan(&source, false, FG, None, ParagraphDir::Default).unwrap();
        let mut offset = 0;
        for seg in plan.segs {
            for c in seg
                .text
                .chars()
                .filter(|c| !matches!(c, '\u{2066}' | '\u{2069}'))
            {
                assert_eq!(c, source[offset].c);
                assert_eq!(seg.style, source[offset].style);
                offset += 1;
            }
        }
        assert_eq!(offset, source.len());
    }

    #[test]
    fn rtl_prose_keeps_path_components_dates_and_numeric_signs_in_reading_order() {
        let mut r = Renderer::new(1.0, &crate::config::UserConfig::default(), 0.0);
        for (text, first, last) in [
            (
                "افتح /home/أحمد/project-7/README.md الآن.",
                "/home",
                "project-7",
            ),
            (
                r"افتح C:\Users\أحمد\Documents\Bayan الآن.",
                "C:",
                "Documents",
            ),
            ("التاريخ 2026-11-13 والساعة 15:42:07.", "2026", "13"),
            ("القيمة -17 والخصم 15% والمرجع #80.", "-", "17"),
            ("القيمة -17 والخصم 15% والمرجع #80.", "#", "80"),
            ("السعر $49.95 للحزمة.", "$", "49"),
        ] {
            let left = position(text, first, &mut r);
            let right = position(text, last, &mut r);
            assert!(
                left < right,
                "{text}: {first} at {left} must precede {last} at {right}"
            );
        }
    }
}
