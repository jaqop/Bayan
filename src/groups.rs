//! Tab groups: each group holds its own set of tabs, and the tab bar shows
//! one group at a time, so related sessions stay together and switching
//! groups never mixes unrelated tabs into the bar.
//!
//! The model is deliberately a TAG, not a container. Every tab carries its
//! group's id and stays where it is in the app's one flat tab list; a
//! group's tabs are the members with that id, in list order. Nothing is
//! moved between vectors or reordered, so the things that hold a tab index
//! stay valid: pane-id event routing (a DSR reply, an exit or a bell in a
//! background group still finds its pane), the MRU history, and the
//! index-based close cards. The active group is simply the active tab's.
//! A group with no tab left is dropped; there is always at least one.

/// One group: a stable id and the name the user gave it, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: u32,
    pub name: Option<String>,
}

/// The groups in their display order.
#[derive(Clone, Debug)]
pub struct Groups {
    list: Vec<Group>,
    next_id: u32,
}

impl Default for Groups {
    fn default() -> Self {
        Self::new()
    }
}

impl Groups {
    pub fn new() -> Self {
        Self {
            list: vec![Group { id: 0, name: None }],
            next_id: 1,
        }
    }

    /// The groups of a saved session, in their saved order: entry `i`
    /// becomes id `i`, which is what a saved tab's group index refers to.
    pub fn restored(names: Vec<Option<String>>) -> Self {
        if names.is_empty() {
            return Self::new();
        }
        let list: Vec<Group> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| Group { id: i as u32, name })
            .collect();
        Self {
            next_id: list.len() as u32,
            list,
        }
    }

    pub fn first(&self) -> u32 {
        self.list[0].id
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Group> {
        self.list.iter()
    }

    /// A new, empty group at the end of the order. It holds no tab yet:
    /// the caller gives it one before the next `prune`.
    pub fn add(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Group { id, name: None });
        id
    }

    pub fn position(&self, id: u32) -> Option<usize> {
        self.list.iter().position(|g| g.id == id)
    }

    pub fn contains(&self, id: u32) -> bool {
        self.position(id).is_some()
    }

    pub fn name(&self, id: u32) -> Option<&str> {
        self.list
            .iter()
            .find(|g| g.id == id)
            .and_then(|g| g.name.as_deref())
    }

    /// `name` must already be cleaned (`clean_tab_name`); `None` restores
    /// the numbered label.
    pub fn set_name(&mut self, id: u32, name: Option<String>) {
        if let Some(g) = self.list.iter_mut().find(|g| g.id == id) {
            g.name = name;
        }
    }

    /// What the chip, the palette and the cockpit call a group: its name,
    /// else a number that stays with the group while it lives (closing
    /// group 1 does not rename group 2).
    pub fn label(&self, id: u32) -> String {
        match self.name(id) {
            Some(n) => n.to_string(),
            None => format!("{} {}", crate::lang::tr("مجموعة", "Group"), id + 1),
        }
    }

    /// The group after (or before) `id` in the order, wrapping. `id` itself
    /// when it is the only group or not a group at all.
    pub fn neighbor(&self, id: u32, back: bool) -> u32 {
        let Some(p) = self.position(id) else {
            return id;
        };
        let n = self.list.len();
        let q = if back { (p + n - 1) % n } else { (p + 1) % n };
        self.list[q].id
    }

    /// Drop every group no live tab belongs to. `live` is each tab's
    /// group id. At least one group always remains.
    pub fn prune(&mut self, live: &[u32]) {
        self.list.retain(|g| live.contains(&g.id));
        if self.list.is_empty() {
            let id = self.next_id;
            self.next_id += 1;
            self.list.push(Group { id, name: None });
        }
    }
}

/// Indices (into the flat tab list) of group `gid`'s tabs, in list order.
pub fn members(tab_groups: &[u32], gid: u32) -> Vec<usize> {
    tab_groups
        .iter()
        .enumerate()
        .filter(|&(_, &g)| g == gid)
        .map(|(i, _)| i)
        .collect()
}

/// Which tab to show after a tab of group `removed_gid` closed.
/// `tab_groups` are the REMAINING tabs' groups (indices already shifted),
/// `shifted` is `active_after_remove`'s answer, and `was_active` says
/// whether the closed tab was the one on screen.
///
/// A background close keeps what is on screen. Closing the visible tab
/// stays in its group — the tab that slid into its place, else the
/// group's last — because jumping into another group's tabs is exactly
/// the mixing groups exist to prevent. Only when the group is gone does
/// the view leave it, for the most recent tab elsewhere.
pub fn after_close(
    was_active: bool,
    shifted: usize,
    removed_gid: u32,
    tab_groups: &[u32],
    mru: &crate::mru::Mru,
) -> usize {
    if !was_active {
        return shifted;
    }
    let same = members(tab_groups, removed_gid);
    if let Some(&last) = same.last() {
        return same.iter().copied().find(|&i| i >= shifted).unwrap_or(last);
    }
    let all: Vec<usize> = (0..tab_groups.len()).collect();
    mru.most_recent(&all).unwrap_or(shifted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mru::Mru;

    #[test]
    fn new_groups_append_and_labels_keep_their_number() {
        let mut g = Groups::new();
        let b = g.add();
        let c = g.add();
        assert_eq!((g.first(), b, c), (0, 1, 2));
        assert_eq!(g.label(c), "مجموعة 3");
        // group 2 closes: group 3 is still group 3
        g.prune(&[0, 2]);
        assert_eq!(g.len(), 2);
        assert_eq!(g.label(c), "مجموعة 3");
        g.set_name(c, Some("api".into()));
        assert_eq!(g.label(c), "api");
        g.set_name(c, None);
        assert_eq!(g.label(c), "مجموعة 3");
    }

    #[test]
    fn neighbor_wraps_both_ways_and_one_group_stays_put() {
        let mut g = Groups::new();
        let b = g.add();
        let c = g.add();
        assert_eq!(g.neighbor(0, false), b);
        assert_eq!(g.neighbor(c, false), 0, "wraps forward");
        assert_eq!(g.neighbor(0, true), c, "wraps back");
        let single = Groups::new();
        assert_eq!(single.neighbor(0, false), 0);
        assert_eq!(single.neighbor(0, true), 0);
        assert_eq!(g.neighbor(99, false), 99, "unknown id: no move");
    }

    #[test]
    fn prune_never_leaves_zero_groups_and_never_reuses_an_id() {
        let mut g = Groups::new();
        let b = g.add();
        g.prune(&[]);
        assert_eq!(g.len(), 1);
        assert!(!g.contains(0) && !g.contains(b));
        assert!(g.first() > b, "a fresh id, not a recycled one");
    }

    #[test]
    fn restored_groups_take_their_saved_order_as_ids() {
        let g = Groups::restored(vec![None, Some("docs".into())]);
        assert_eq!(g.len(), 2);
        assert_eq!(g.label(0), "مجموعة 1");
        assert_eq!(g.label(1), "docs");
        let mut g = g;
        assert_eq!(g.add(), 2, "new ids continue after the saved ones");
        assert_eq!(Groups::restored(Vec::new()).len(), 1);
    }

    #[test]
    fn members_are_in_list_order_and_may_interleave() {
        // tabs: A A B A B — a tab moved into a group keeps its list place
        let tabs = [0, 0, 1, 0, 1];
        assert_eq!(members(&tabs, 0), vec![0, 1, 3]);
        assert_eq!(members(&tabs, 1), vec![2, 4]);
        assert!(members(&tabs, 7).is_empty());
    }

    #[test]
    fn closing_the_visible_tab_stays_in_its_group() {
        let mru = Mru::new();
        // before: A0 B1 A2 B3, tab 2 (A) on screen closes -> A0 B1 B3
        // active_after_remove says index 2 (B3): the neighbour group's tab.
        // The view must stay in A.
        let after = [0, 1, 1];
        assert_eq!(after_close(true, 2, 0, &after, &mru), 0);
        // before: A0 A1 A2, tab 1 closes -> A0 A1: the one that slid in
        assert_eq!(after_close(true, 1, 0, &[0, 0], &mru), 1);
        // a background close never moves the view
        assert_eq!(after_close(false, 1, 0, &after, &mru), 1);
    }

    #[test]
    fn closing_a_groups_last_tab_goes_to_the_most_recent_elsewhere() {
        let mut mru = Mru::new();
        // most recent first: 0, 1
        mru.touch(1);
        mru.touch(0);
        // before: B0 B1 A2, A's only tab closes -> B0 B1
        assert_eq!(after_close(true, 1, 0, &[1, 1], &mru), 0);
        // no history at all: the shifted neighbour
        assert_eq!(after_close(true, 1, 0, &[1, 1], &Mru::new()), 1);
    }
}
