//! Word boundaries for presentation-only Arabic prose wrapping.

use super::{is_arabic, tui_row, CellInfo};

/// Keep the same number of screen rows, moving only their soft break points.
/// Fully visible paragraphs only: an unseen head/tail must not change the
/// positions of the visible rows as the viewport scrolls. TUIs and explicit
/// newlines retain their exact grid layout.
pub(super) fn reflow(lines: &mut [Vec<CellInfo>], continued: &[bool], cols: usize) -> Vec<usize> {
    let mut changed = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        while end < lines.len() && continued[end] {
            end += 1;
        }
        let complete = !continued[start] && !continued[end];
        let split_arabic = (start + 1..end).any(|row| {
            lines[row - 1]
                .last()
                .is_some_and(|c| is_arabic(c.c) && c.c.is_alphabetic())
                && lines[row]
                    .first()
                    .is_some_and(|c| is_arabic(c.c) && c.c.is_alphabetic())
        });
        let application_direction = lines[start..end].iter().flatten().any(|cell| {
            std::iter::once(cell.c)
                .chain(cell.zw.iter().flatten().copied())
                .any(|c| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        });
        if complete
            && split_arabic
            && !application_direction
            && !lines[start..end].iter().enumerate().any(|(offset, row)| {
                // A continuation beginning with `>=` is still prose, not a
                // fresh shell prompt. Grid gaps/boxes remain TUI indicators.
                tui_row(row.len(), |i| (row[i].col, row[i].w, row[i].c), offset == 0)
            })
        {
            // Count word-end boundaries in the original grid. Never create
            // empty display rows or split an unbroken token to fill a row.
            let cells: Vec<_> = lines[start..end].iter().flatten().collect();
            let source_ends: Vec<_> = lines
                .iter()
                .enumerate()
                .take(end)
                .skip(start)
                .flat_map(|(row, cells)| cells.iter().map(move |c| row * cols + c.col + c.w))
                .collect();
            let boundaries: Vec<_> = (1..cells.len())
                .filter(|&i| cells[i - 1].c.is_whitespace() && !cells[i].c.is_whitespace())
                .collect();
            let count = end - start;
            if boundaries.len() >= count - 1 {
                let mut cuts = vec![0];
                for n in 1..count {
                    let target = (start + n) * cols;
                    let remaining = count - n - 1;
                    let candidates: Vec<_> = boundaries
                        .iter()
                        .copied()
                        .filter(|&i| i > *cuts.last().unwrap())
                        .collect();
                    let best = candidates[..candidates.len() - remaining]
                        .iter()
                        .copied()
                        .min_by_key(|&i| source_ends[i - 1].abs_diff(target))
                        .unwrap();
                    cuts.push(best);
                }
                cuts.push(cells.len());
                let mut owned = Vec::with_capacity(cells.len());
                for (row, line) in lines.iter_mut().enumerate().take(end).skip(start) {
                    for mut cell in std::mem::take(line) {
                        cell.col += row * cols;
                        owned.push(cell);
                    }
                }
                let mut iter = owned.into_iter();
                for (offset, pair) in cuts.windows(2).enumerate() {
                    lines[start + offset].extend(iter.by_ref().take(pair[1] - pair[0]));
                    changed.push(start + offset);
                }
            }
        }
        start = end;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, cols: usize) -> Vec<Vec<CellInfo>> {
        text.chars()
            .collect::<Vec<_>>()
            .chunks(cols)
            .map(|part| {
                part.iter()
                    .enumerate()
                    .map(|(col, &c)| CellInfo {
                        col,
                        c,
                        w: 1,
                        zw: None,
                        fg: super::super::FG,
                        bg: super::super::BG,
                        style: 0,
                        ul: super::super::FG,
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn incomplete_paragraphs_explicit_lines_tuis_and_app_direction_keep_the_grid() {
        for text in [
            "هذه الإنجليزية ثم العربية",
            "│هذه الإنجليزية ثم العربية",
            "هذه \u{2067}الإنجليزية ثم العربية\u{2069}",
            "الإنجليزيةالإنجليزيةالإنجليزية",
        ] {
            let mut lines = rows(text, 8);
            let mut continued = vec![true; lines.len() + 1];
            continued[0] = false;
            // A paragraph clipped at the bottom is not reflowed.
            assert!(reflow(&mut lines, &continued, 8).is_empty());
            continued.fill(false);
            assert!(
                reflow(&mut lines, &continued, 8).is_empty(),
                "hard newlines stay hard"
            );
            continued.fill(true);
            continued[0] = false;
            *continued.last_mut().unwrap() = false;
            if text.starts_with('│') || text.contains('\u{2067}') || !text.contains(' ') {
                assert!(reflow(&mut lines, &continued, 8).is_empty());
            } else {
                assert!(!reflow(&mut lines, &continued, 8).is_empty());
            }
        }
    }
}
