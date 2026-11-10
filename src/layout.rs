//! A tab's pane layout as a tree of splits, so side-by-side and stacked
//! splits can be mixed in one tab: split the left pane down, the right one
//! across, and so on.
//!
//! Panes stay in the tab's flat `Vec<Pane>`; the tree only holds their
//! indices (`Leaf(i)`). Everything that worked on pane indices (focus,
//! broadcast, zoom, the close guard) keeps working. Leaves are numbered in
//! reading order of creation, and removing a pane renumbers the leaves above
//! it, the same way `Vec::remove` shifts the panes.
//!
//! Everything here is pure geometry and tree surgery, tested below; the
//! window only asks for rects, dividers and drags.

use crate::render::Rect;
use crate::Orientation;

/// Pixels between two panes when nothing else needs the seam. A scrollbar
/// widens the side-by-side seam; the stacked seam stays this hairline.
const GAP: i32 = 1;

/// How wide each seam is. `beside` is the vertical gap between side-by-side
/// panes (where a vertical scrollbar sits). `stacked` is the horizontal gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gaps {
    pub beside: i32,
    pub stacked: i32,
}

impl Gaps {
    pub const HAIRLINE: Self = Self {
        beside: GAP,
        stacked: GAP,
    };

    fn on(self, axis: Orientation) -> i32 {
        match axis {
            Orientation::Row => self.beside.max(1),
            Orientation::Column => self.stacked.max(1),
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Node {
    Leaf(usize),
    Split {
        axis: Orientation,
        children: Vec<Node>,
        /// relative sizes along `axis`, one per child
        weights: Vec<f32>,
    },
}

/// A draggable line between child `index` and `index + 1` of the split at
/// `path` (child indices from the root).
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub path: Vec<usize>,
    pub index: usize,
    pub axis: Orientation,
    /// the line's own rect: GAP wide across the axis, full length along it
    pub line: Rect,
}

impl Node {
    /// A single pane.
    pub fn single() -> Node {
        Node::Leaf(0)
    }

    /// The pre-tree layout: `n` panes along one axis (also how a session
    /// file from before nested splits is read back).
    pub fn flat(axis: Orientation, weights: &[f32], n: usize) -> Node {
        if n <= 1 {
            return Node::Leaf(0);
        }
        let weights = if usable_weights(weights, n) {
            weights.to_vec()
        } else {
            vec![1.0; n]
        };
        Node::Split {
            axis,
            children: (0..n).map(Node::Leaf).collect(),
            weights,
        }
    }

    /// A tree is usable for `n` panes when it holds each index 0..n exactly
    /// once and every split has matching, positive weights. A session file
    /// that fails this is replaced, not trusted.
    pub fn valid_for(&self, n: usize) -> bool {
        fn walk(node: &Node, seen: &mut Vec<bool>) -> bool {
            match node {
                Node::Leaf(i) => {
                    if *i >= seen.len() || seen[*i] {
                        return false;
                    }
                    seen[*i] = true;
                    true
                }
                Node::Split {
                    children, weights, ..
                } => {
                    children.len() >= 2
                        && usable_weights(weights, children.len())
                        && children.iter().all(|c| walk(c, seen))
                }
            }
        }
        let mut seen = vec![false; n];
        walk(self, &mut seen) && seen.iter().all(|s| *s)
    }

    /// Pane rects with explicit seams, indexed by pane. The cell rects
    /// exclude the seam, so a scrollbar drawn there does not cover a column
    /// and the column count stays `grid width / cell`.
    pub fn rects_spaced(&self, content: Rect, panes: usize, gaps: Gaps) -> Vec<Rect> {
        let mut out = vec![(content.0, content.1, 0, 0); panes];
        self.place(content, &mut out, gaps);
        out
    }

    fn place(&self, rect: Rect, out: &mut Vec<Rect>, gaps: Gaps) {
        match self {
            Node::Leaf(i) => {
                if let Some(slot) = out.get_mut(*i) {
                    *slot = rect;
                }
            }
            Node::Split {
                axis,
                children,
                weights,
            } => {
                for (child, r) in
                    children
                        .iter()
                        .zip(split_gap(rect, weights, *axis, gaps.on(*axis)))
                {
                    child.place(r, out, gaps);
                }
            }
        }
    }

    /// Split pane `target` along `axis`, adding pane `new` right after it.
    /// Inside a split that already runs along `axis`, the new pane becomes a
    /// sibling and the two share the target's old size; otherwise the
    /// target's slot becomes a new split of the two.
    pub fn split(&mut self, target: usize, axis: Orientation, new: usize) {
        if !self.split_in_parent(target, axis, new) {
            self.replace_leaf(target, axis, new);
        }
    }

    fn split_in_parent(&mut self, target: usize, axis: Orientation, new: usize) -> bool {
        let Node::Split {
            axis: a,
            children,
            weights,
        } = self
        else {
            return false;
        };
        if let Some(k) = children.iter().position(|c| *c == Node::Leaf(target)) {
            if *a == axis {
                let half = weights[k] / 2.0;
                weights[k] = half;
                children.insert(k + 1, Node::Leaf(new));
                weights.insert(k + 1, half);
                return true;
            }
            return false;
        }
        children
            .iter_mut()
            .any(|c| c.split_in_parent(target, axis, new))
    }

    fn replace_leaf(&mut self, target: usize, axis: Orientation, new: usize) -> bool {
        match self {
            Node::Leaf(i) if *i == target => {
                *self = Node::Split {
                    axis,
                    children: vec![Node::Leaf(target), Node::Leaf(new)],
                    weights: vec![1.0, 1.0],
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children
                .iter_mut()
                .any(|c| c.replace_leaf(target, axis, new)),
        }
    }

    /// Remove pane `id`: its leaf goes, panes above it shift down by one
    /// (matching `Vec::remove`), a split left with one child is replaced by
    /// that child, and the freed size goes to its neighbours.
    pub fn remove(&mut self, id: usize) {
        if let Some(n) = self.without(id) {
            *self = n;
        } else {
            *self = Node::Leaf(0);
        }
    }

    fn without(&self, id: usize) -> Option<Node> {
        match self {
            Node::Leaf(i) if *i == id => None,
            Node::Leaf(i) => Some(Node::Leaf(if *i > id { i - 1 } else { *i })),
            Node::Split {
                axis,
                children,
                weights,
            } => {
                let mut kids = Vec::new();
                let mut ws = Vec::new();
                for (c, w) in children.iter().zip(weights) {
                    if let Some(n) = c.without(id) {
                        kids.push(n);
                        ws.push(*w);
                    }
                }
                // A child split on the same axis is what a collapse leaves
                // behind (a column whose only remainder was itself a row,
                // inside a row). Fold it in so the tree doesn't nest an
                // axis with itself. Weights are scaled so the panes keep
                // the share they had.
                let mut flat_k = Vec::new();
                let mut flat_w = Vec::new();
                for (k, w) in kids.into_iter().zip(ws) {
                    if let Node::Split {
                        axis: child_axis,
                        children: inner,
                        weights: inner_w,
                    } = k
                    {
                        if child_axis == *axis {
                            let sum = inner_w.iter().sum::<f32>().max(f32::EPSILON);
                            for (c, cw) in inner.into_iter().zip(inner_w) {
                                flat_k.push(c);
                                flat_w.push(w * cw / sum);
                            }
                            continue;
                        }
                        flat_k.push(Node::Split {
                            axis: child_axis,
                            children: inner,
                            weights: inner_w,
                        });
                        flat_w.push(w);
                        continue;
                    }
                    flat_k.push(k);
                    flat_w.push(w);
                }
                match flat_k.len() {
                    0 => None,
                    1 => flat_k.pop(),
                    _ => Some(Node::Split {
                        axis: *axis,
                        children: flat_k,
                        weights: flat_w,
                    }),
                }
            }
        }
    }

    /// Every divider in `content`, outermost splits first.
    pub fn dividers_spaced(&self, content: Rect, gaps: Gaps) -> Vec<Divider> {
        let mut out = Vec::new();
        self.collect_dividers(content, &mut Vec::new(), &mut out, gaps);
        out
    }

    fn collect_dividers(
        &self,
        rect: Rect,
        path: &mut Vec<usize>,
        out: &mut Vec<Divider>,
        gaps: Gaps,
    ) {
        let Node::Split {
            axis,
            children,
            weights,
        } = self
        else {
            return;
        };
        let gap = gaps.on(*axis);
        let rs = split_gap(rect, weights, *axis, gap);
        for (k, r) in rs.iter().enumerate().take(rs.len().saturating_sub(1)) {
            let line = match axis {
                Orientation::Row => (r.0 + r.2, rect.1, gap, rect.3),
                Orientation::Column => (rect.0, r.1 + r.3, rect.2, gap),
            };
            out.push(Divider {
                path: path.clone(),
                index: k,
                axis: *axis,
                line,
            });
        }
        for (k, (c, r)) in children.iter().zip(rs).enumerate() {
            path.push(k);
            c.collect_dividers(r, path, out, gaps);
            path.pop();
        }
    }

    /// Move divider `d` to pointer `p` (x for a side-by-side split, y for a
    /// stacked one): the two children either side of it trade size, each
    /// keeping at least ~80px.
    pub fn drag_spaced(&mut self, d: &Divider, content: Rect, p: f64, gaps: Gaps) {
        let Some((node, rect)) = self.at_path(&d.path, content, gaps) else {
            return;
        };
        let Node::Split { axis, weights, .. } = node else {
            return;
        };
        let rs = split_gap(rect, weights, *axis, gaps.on(*axis));
        let i = d.index;
        if i + 1 >= rs.len() {
            return;
        }
        let (a, b) = (rs[i], rs[i + 1]);
        let (start, span) = match axis {
            Orientation::Row => (a.0 as f64, (b.0 + b.2 - a.0) as f64),
            Orientation::Column => (a.1 as f64, (b.1 + b.3 - a.1) as f64),
        };
        if span <= 0.0 {
            return;
        }
        let min = (80.0 / span).min(0.45);
        let rel = ((p - start) / span).clamp(min, 1.0 - min) as f32;
        let pair = weights[i] + weights[i + 1];
        weights[i] = pair * rel;
        weights[i + 1] = pair - weights[i];
    }

    fn at_path(&mut self, path: &[usize], rect: Rect, gaps: Gaps) -> Option<(&mut Node, Rect)> {
        let Some((&k, rest)) = path.split_first() else {
            return Some((self, rect));
        };
        let Node::Split {
            axis,
            children,
            weights,
        } = self
        else {
            return None;
        };
        let r = *split_gap(rect, weights, *axis, gaps.on(*axis)).get(k)?;
        children.get_mut(k)?.at_path(rest, r, gaps)
    }
}

/// The divider under `(x, y)`, as an index into `dividers`. The grab is
/// measured from the line's center and extends `grab` pixels past each
/// edge, so a wide seam (the scrollbar gutter) is held from both panes,
/// not only from the side where the line starts.
pub fn divider_at(dividers: &[Divider], x: f64, y: f64, grab: f64) -> Option<usize> {
    dividers.iter().position(|d| {
        let (lx, ly, lw, lh) = d.line;
        match d.axis {
            Orientation::Row => {
                let mid = lx as f64 + lw as f64 / 2.0;
                let reach = grab + lw as f64 / 2.0;
                (x - mid).abs() <= reach && y >= ly as f64 && y < (ly + lh) as f64
            }
            Orientation::Column => {
                let mid = ly as f64 + lh as f64 / 2.0;
                let reach = grab + lh as f64 / 2.0;
                (y - mid).abs() <= reach && x >= lx as f64 && x < (lx + lw) as f64
            }
        }
    })
}

/// `n` positive, finite weights whose sum is finite too: each weight can
/// pass and the total still overflow to infinity, which `split_rect` turns
/// into NaN rects.
fn usable_weights(weights: &[f32], n: usize) -> bool {
    weights.len() == n
        && weights.iter().all(|w| w.is_finite() && *w > 0.0)
        && weights.iter().sum::<f32>().is_finite()
}

/// `rect` cut along `axis` by `weights`, `gap` pixels between pieces; the
/// last piece absorbs the rounding.
fn split_gap(rect: Rect, weights: &[f32], axis: Orientation, gap: i32) -> Vec<Rect> {
    let gap = gap.max(0);
    let (x, y, w, h) = rect;
    let n = weights.len().max(1) as i32;
    let total: f32 = weights.iter().sum::<f32>().max(f32::EPSILON);
    let avail = match axis {
        Orientation::Row => w - gap * (n - 1),
        Orientation::Column => h - gap * (n - 1),
    }
    .max(0) as f32;
    let mut out = Vec::with_capacity(weights.len());
    let mut cursor = match axis {
        Orientation::Row => x,
        Orientation::Column => y,
    };
    for (i, wt) in weights.iter().enumerate() {
        let len = if i as i32 == n - 1 {
            match axis {
                Orientation::Row => x + w - cursor,
                Orientation::Column => y + h - cursor,
            }
        } else {
            (avail * wt / total) as i32
        }
        .max(0);
        out.push(match axis {
            Orientation::Row => (cursor, y, len, h),
            Orientation::Column => (x, cursor, w, len),
        });
        cursor += len + gap;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Orientation::{Column, Row};

    const C: Rect = (0, 0, 401, 201);

    #[test]
    fn side_by_side_then_stacked_mixes_in_one_tab() {
        // split pane 0 side by side, then split the RIGHT pane (1) stacked
        let mut t = Node::single();
        t.split(0, Row, 1);
        t.split(1, Column, 2);
        let r = t.rects_spaced(C, 3, Gaps::HAIRLINE);
        assert_eq!(r[0], (0, 0, 200, 201), "left pane keeps full height");
        assert_eq!(r[1], (201, 0, 200, 100), "right top");
        assert_eq!(r[2], (201, 101, 200, 100), "right bottom");
        assert!(t.valid_for(3));
    }

    #[test]
    fn splitting_along_the_parents_axis_adds_a_sibling() {
        // three side by side, not a split inside a split
        let mut t = Node::single();
        t.split(0, Row, 1);
        t.split(1, Row, 2);
        match &t {
            Node::Split { axis, children, .. } => {
                assert_eq!(*axis, Row);
                assert_eq!(children.len(), 3);
            }
            other => panic!("{other:?}"),
        }
        // the split pane and its new sibling share the split pane's size
        let r = t.rects_spaced(C, 3, Gaps::HAIRLINE);
        assert!((r[0].2 - 200).abs() <= 1, "{r:?}");
        assert!((r[1].2 - r[2].2).abs() <= 2, "{r:?}");
    }

    #[test]
    fn removing_a_pane_collapses_and_renumbers() {
        let mut t = Node::single();
        t.split(0, Row, 1);
        t.split(1, Column, 2);
        // close the right-top pane: the right column collapses to pane 2,
        // which becomes pane 1 (Vec::remove shifted it)
        t.remove(1);
        assert_eq!(t, Node::flat(Row, &[1.0, 1.0], 2));
        assert!(t.valid_for(2));
        t.remove(0);
        assert_eq!(t, Node::Leaf(0));
        t.remove(0);
        assert_eq!(t, Node::Leaf(0), "never an empty tree");
    }

    #[test]
    fn removing_a_pane_merges_a_same_axis_split_left_behind() {
        // Row [ leaf 0 | Column [ Row [1, 2] | leaf 3 ] ]. Closing 3
        // collapses the column onto that inner row, which is the same
        // axis as the outer split and has to become three siblings.
        let mut t = Node::Split {
            axis: Row,
            weights: vec![1.0, 1.0],
            children: vec![
                Node::Leaf(0),
                Node::Split {
                    axis: Column,
                    weights: vec![1.0, 1.0],
                    children: vec![
                        Node::Split {
                            axis: Row,
                            weights: vec![1.0, 1.0],
                            children: vec![Node::Leaf(1), Node::Leaf(2)],
                        },
                        Node::Leaf(3),
                    ],
                },
            ],
        };
        t.remove(3);
        match t {
            Node::Split {
                axis,
                children,
                weights,
            } => {
                assert_eq!(axis, Row);
                assert!(
                    children.iter().all(|c| matches!(c, Node::Leaf(_))),
                    "same-axis child was not folded: {children:?}"
                );
                assert_eq!(children, vec![Node::Leaf(0), Node::Leaf(1), Node::Leaf(2)]);
                assert!((weights[0] - 1.0).abs() < 1e-4, "{weights:?}");
                assert!((weights[1] - 0.5).abs() < 1e-4, "{weights:?}");
                assert!((weights[2] - 0.5).abs() < 1e-4, "{weights:?}");
            }
            other => panic!("expected a row, got {other:?}"),
        }
    }

    #[test]
    fn dividers_are_found_and_dragged_at_every_level() {
        let mut t = Node::single();
        t.split(0, Row, 1);
        t.split(1, Column, 2);
        let ds = t.dividers_spaced(C, Gaps::HAIRLINE);
        assert_eq!(ds.len(), 2);
        assert_eq!((ds[0].axis, ds[0].path.clone()), (Row, vec![]));
        assert_eq!((ds[1].axis, ds[1].path.clone()), (Column, vec![1]));
        // the vertical line between left and right
        assert_eq!(divider_at(&ds, 200.0, 150.0, 4.0), Some(0));
        // the horizontal line in the right column only
        assert_eq!(divider_at(&ds, 300.0, 100.0, 4.0), Some(1));
        assert_eq!(
            divider_at(&ds, 100.0, 100.0, 4.0),
            None,
            "left pane has none"
        );
        // drag the inner divider down: only the right column changes
        let d = ds[1].clone();
        t.drag_spaced(&d, C, 150.0, Gaps::HAIRLINE);
        let r = t.rects_spaced(C, 3, Gaps::HAIRLINE);
        assert_eq!(r[0], (0, 0, 200, 201));
        assert!(r[1].3 > r[2].3, "top grew: {r:?}");
        // drag the outer one: the left pane narrows, the column widens
        let d = t.dividers_spaced(C, Gaps::HAIRLINE)[0].clone();
        t.drag_spaced(&d, C, 120.0, Gaps::HAIRLINE);
        let r = t.rects_spaced(C, 3, Gaps::HAIRLINE);
        assert!(r[0].2 < r[1].2, "{r:?}");
        // the drag never makes a pane unusably small
        let d = t.dividers_spaced(C, Gaps::HAIRLINE)[0].clone();
        t.drag_spaced(&d, C, 0.0, Gaps::HAIRLINE);
        assert!(t.rects_spaced(C, 3, Gaps::HAIRLINE)[0].2 >= 79);
    }

    #[test]
    fn flat_reads_old_session_files_and_rejects_bad_weights() {
        let t = Node::flat(Column, &[3.0, 1.0], 2);
        assert_eq!(
            t.rects_spaced((0, 0, 100, 401), 2, Gaps::HAIRLINE)[0].3,
            300
        );
        assert_eq!(
            Node::flat(Row, &[f32::NAN, 1.0], 2),
            Node::flat(Row, &[1.0, 1.0], 2)
        );
        assert_eq!(Node::flat(Row, &[], 1), Node::Leaf(0));
    }

    #[test]
    fn a_tree_from_a_session_file_is_checked_before_use() {
        let mut t = Node::single();
        t.split(0, Row, 1);
        assert!(t.valid_for(2));
        assert!(!t.valid_for(3), "a pane with no slot");
        assert!(!Node::Leaf(5).valid_for(1));
        let dup = Node::Split {
            axis: Row,
            children: vec![Node::Leaf(0), Node::Leaf(0)],
            weights: vec![1.0, 1.0],
        };
        assert!(!dup.valid_for(2));
        let bad_w = Node::Split {
            axis: Row,
            children: vec![Node::Leaf(0), Node::Leaf(1)],
            weights: vec![1.0],
        };
        assert!(!bad_w.valid_for(2));
        // each weight is finite, but the sum overflows
        let overflow = Node::Split {
            axis: Row,
            children: vec![Node::Leaf(0), Node::Leaf(1)],
            weights: vec![f32::MAX, f32::MAX],
        };
        assert!(
            !overflow.valid_for(2),
            "a non-finite weight sum is unusable"
        );
        // and it round-trips through JSON
        let json = serde_json::to_string(&t).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn an_old_file_whose_weights_overflow_gets_equal_panes() {
        // an old session.json with "weights":[3.4e38,3.4e38] and no `layout`
        // reaches `flat`, not `valid_for`; an infinite total makes NaN rects
        let flat = Node::flat(Row, &[f32::MAX, f32::MAX], 2);
        assert_eq!(flat, Node::flat(Row, &[], 2));
        for (x, y, w, h) in flat.rects_spaced(C, 2, Gaps::HAIRLINE) {
            assert!(x >= 0 && y >= 0 && w > 0 && h > 0, "{:?}", (x, y, w, h));
        }
        // a large but finite sum is still the user's split
        assert_eq!(
            Node::flat(Column, &[3.0, 1.0], 2),
            Node::Split {
                axis: Column,
                children: vec![Node::Leaf(0), Node::Leaf(1)],
                weights: vec![3.0, 1.0],
            }
        );
    }

    #[test]
    fn a_side_by_side_seam_holds_a_scrollbar_between_the_grids() {
        let mut t = Node::single();
        t.split(0, Row, 1);
        let gaps = Gaps {
            beside: 8,
            stacked: 1,
        };
        let r = t.rects_spaced(C, 2, gaps);
        let seam = r[1].0 - (r[0].0 + r[0].2);
        assert_eq!(seam, 8);
        assert!(r[0].0 + r[0].2 <= r[1].0);
        // the hairline layout is unchanged, so a hidden bar does not move columns
        let hair = t.rects_spaced(C, 2, Gaps::HAIRLINE);
        assert_eq!(hair[1].0 - (hair[0].0 + hair[0].2), 1);
        let stacked = {
            let mut s = Node::single();
            s.split(0, Column, 1);
            s.rects_spaced((0, 0, 100, 401), 2, gaps)
        };
        assert_eq!(stacked[1].1 - (stacked[0].1 + stacked[0].3), 1);
    }

    #[test]
    fn a_wide_seam_is_grabbed_from_its_center() {
        let mut t = Node::single();
        t.split(0, Row, 1);
        let ds = t.dividers_spaced(
            C,
            Gaps {
                beside: 8,
                stacked: 1,
            },
        );
        let (x, _, w, _) = ds[0].line;
        let mid = x as f64 + w as f64 / 2.0;
        assert_eq!(divider_at(&ds, mid, 40.0, 4.0), Some(0));
        // 3px into each pane, past the seam: still the divider
        assert_eq!(divider_at(&ds, x as f64 - 3.0, 40.0, 4.0), Some(0));
        assert_eq!(divider_at(&ds, (x + w) as f64 + 3.0, 40.0, 4.0), Some(0));
        // 6px into a pane is past the 4px slop
        assert_eq!(divider_at(&ds, x as f64 - 6.0, 40.0, 4.0), None);
        assert_eq!(divider_at(&ds, (x + w) as f64 + 6.0, 40.0, 4.0), None);
    }
}
