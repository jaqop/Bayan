//! Most-recently-used tab order for Ctrl+Tab — the browser/IDE "stack"
//! behaviour (a top Ghostty request): one Ctrl+Tab returns to the tab you
//! were just on, and holding Ctrl while pressing Tab again walks further
//! down the history. Releasing Ctrl commits wherever the walk stopped as
//! the new most-recent tab. Ctrl+Shift+Tab walks the other way.
//!
//! Pure data: tab INDICES, kept in sync by the app on switch/spawn/close.
//! The classic left-to-right cycle is still available via the
//! `tab_switch = "order"` config.

#[derive(Debug, Default)]
pub struct Mru {
    /// front = most recently active
    order: Vec<usize>,
    /// how far a Ctrl-held walk has gone (0 = not walking)
    walk: usize,
}

impl Mru {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tab `idx` became active by the user's choice (click, cockpit, a new
    /// tab, a finished Ctrl walk): it is now the most recent.
    pub fn touch(&mut self, idx: usize) {
        self.order.retain(|&i| i != idx);
        self.order.insert(0, idx);
        self.walk = 0;
    }

    /// Tab `idx` closed: drop it and slide the higher indices down.
    pub fn removed(&mut self, idx: usize) {
        self.order.retain(|&i| i != idx);
        for i in &mut self.order {
            if *i > idx {
                *i -= 1;
            }
        }
        self.walk = 0;
    }

    /// One Ctrl+Tab (forward) or Ctrl+Shift+Tab (back) with Ctrl held:
    /// the tab to show next. `len` is the live tab count; indices the
    /// order has not seen yet (restored tabs, say) count as least recent.
    #[cfg(test)]
    pub fn step(&mut self, forward: bool, len: usize) -> Option<usize> {
        let all: Vec<usize> = (0..len).collect();
        self.step_among(forward, len, &all)
    }

    /// `step`, with the walk confined to `members` — the active group's
    /// tabs — so Ctrl+Tab never lands in another group. The history of
    /// the other groups is kept, just not walked.
    pub fn step_among(&mut self, forward: bool, len: usize, members: &[usize]) -> Option<usize> {
        if members.iter().filter(|&&i| i < len).count() < 2 {
            return None;
        }
        // make sure every live tab is in the ring, oldest last
        for i in 0..len {
            if !self.order.contains(&i) {
                self.order.push(i);
            }
        }
        self.order.retain(|&i| i < len);
        let ring: Vec<usize> = self
            .order
            .iter()
            .copied()
            .filter(|i| members.contains(i))
            .collect();
        let n = ring.len();
        self.walk = if forward {
            (self.walk + 1) % n
        } else {
            (self.walk + n - 1) % n
        };
        Some(ring[self.walk])
    }

    /// The most recently active of `members`, if the history has seen one
    /// — where switching back to a group lands.
    pub fn most_recent(&self, members: &[usize]) -> Option<usize> {
        self.order.iter().copied().find(|i| members.contains(i))
    }

    pub fn walking(&self) -> bool {
        self.walk != 0
    }

    /// Ctrl released after a walk: the tab it stopped on is the most recent.
    pub fn release(&mut self, current: usize) {
        if self.walk != 0 {
            self.touch(current);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_ctrl_tab_returns_to_the_previous_tab() {
        let mut m = Mru::new();
        m.touch(0);
        m.touch(1);
        m.touch(2); // history: 2 (now), 1, 0
        assert_eq!(m.step(true, 3), Some(1));
        m.release(1); // Ctrl up: 1 is now the most recent, 2 before it
        assert_eq!(m.step(true, 3), Some(2));
        m.release(2);
        // alternating Ctrl+Tab flips between the two most recent, like a browser
        assert_eq!(m.step(true, 3), Some(1));
        m.release(1);
    }

    #[test]
    fn holding_ctrl_walks_deeper_and_wraps() {
        let mut m = Mru::new();
        for i in 0..4 {
            m.touch(i);
        } // 3, 2, 1, 0
        assert_eq!(m.step(true, 4), Some(2));
        assert!(m.walking());
        assert_eq!(m.step(true, 4), Some(1));
        assert_eq!(m.step(true, 4), Some(0));
        assert_eq!(m.step(true, 4), Some(3), "wraps to the start of the ring");
        // Shift walks back up
        assert_eq!(m.step(false, 4), Some(0));
        m.release(0);
        assert!(!m.walking());
        assert_eq!(
            m.step(true, 4),
            Some(3),
            "0 is now first, 3 (where the walk began) next"
        );
    }

    #[test]
    fn closing_a_tab_keeps_the_rest_in_order() {
        let mut m = Mru::new();
        for i in 0..4 {
            m.touch(i);
        } // 3, 2, 1, 0
        m.removed(1); // tabs 2,3 slide to 1,2 -> history 2, 1, 0
        assert_eq!(m.order, vec![2, 1, 0]);
        m.removed(2); // the current one closed -> 1, 0
        assert_eq!(m.order, vec![1, 0]);
        assert_eq!(m.step(true, 2), Some(0));
    }

    #[test]
    fn unseen_tabs_join_as_least_recent_and_one_tab_never_steps() {
        let mut m = Mru::new();
        m.touch(0);
        // three tabs restored from a session: 1 and 2 were never touched
        assert_eq!(m.step(true, 3), Some(1));
        assert_eq!(m.step(true, 3), Some(2));
        let mut single = Mru::new();
        single.touch(0);
        assert_eq!(single.step(true, 1), None);
        assert!(!single.walking());
    }

    #[test]
    fn a_group_walk_skips_tabs_of_other_groups() {
        let mut m = Mru::new();
        for i in 0..5 {
            m.touch(i);
        } // 4, 3, 2, 1, 0 — say 0, 2, 4 are one group
        let group = [0, 2, 4];
        assert_eq!(m.step_among(true, 5, &group), Some(2));
        assert_eq!(m.step_among(true, 5, &group), Some(0));
        assert_eq!(m.step_among(true, 5, &group), Some(4), "wraps in the group");
        m.release(4);
        // a group of one tab never steps, even with other tabs open
        assert_eq!(m.step_among(true, 5, &[3]), None);
        assert!(!m.walking());
        // the group's most recent, ignoring newer tabs elsewhere
        m.touch(1);
        assert_eq!(m.most_recent(&group), Some(4));
        assert_eq!(m.most_recent(&[]), None);
    }
}
