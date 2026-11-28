//! Output triggers: user patterns matched against NEW pane output that fire a
//! notify-only action (a toast or the bell). Like iTerm2 Triggers, but bounded
//! and safe: the trigger is untrusted output, so it may only raise a
//! notification — never run a command, write to the PTY, or touch the
//! clipboard. Each trigger has a cooldown, and toasts also pass the global
//! rate limit, so a matching flood cannot spam.

use crate::config::TriggerConfig;
use std::time::{Duration, Instant};

/// What a matched trigger does. Notify-only by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerAction {
    Toast,
    Bell,
}

impl TriggerAction {
    fn parse(s: &str) -> Self {
        match s {
            "bell" => TriggerAction::Bell,
            _ => TriggerAction::Toast,
        }
    }
}

struct Meta {
    action: TriggerAction,
    cooldown: Duration,
    last_fired: Option<Instant>,
}

/// Compiled triggers. Patterns that do not compile are dropped at compile
/// time, so a bad pattern never costs anything at match time.
pub struct TriggerSet {
    regexes: Vec<regex::Regex>,
    meta: Vec<Meta>,
}

impl TriggerSet {
    pub fn compile(configs: &[TriggerConfig]) -> Self {
        let mut regexes = Vec::new();
        let mut meta = Vec::new();
        for c in configs {
            if c.pattern.is_empty() {
                continue;
            }
            let Ok(re) = regex::Regex::new(&c.pattern) else {
                continue;
            };
            regexes.push(re);
            meta.push(Meta {
                action: TriggerAction::parse(&c.action),
                cooldown: Duration::from_millis(c.cooldown_ms),
                last_fired: None,
            });
        }
        Self { regexes, meta }
    }

    pub fn is_empty(&self) -> bool {
        self.regexes.is_empty()
    }

    /// Scan the new output since `since` and return the actions whose cooldown
    /// has elapsed, plus the new cursor. The text extraction stays under the
    /// session's lock; the matching and cooldown bookkeeping are here.
    pub fn fire(
        &mut self,
        session: &crate::term::Session,
        since: i32,
        now: Instant,
    ) -> (Vec<TriggerAction>, i32) {
        let (text, cursor) = session.output_text_since(since);
        let hits: Vec<bool> = self.regexes.iter().map(|re| re.is_match(&text)).collect();
        let fired = self.cooldown_filter(&hits, now);
        (fired, cursor)
    }

    /// Pure cooldown logic, testable without a terminal. A hit fires only when
    /// its cooldown has elapsed; firing stamps the cooldown.
    fn cooldown_filter(&mut self, hits: &[bool], now: Instant) -> Vec<TriggerAction> {
        let mut fired = Vec::new();
        for (i, &hit) in hits.iter().enumerate() {
            if !hit {
                continue;
            }
            let m = &mut self.meta[i];
            if m.last_fired
                .is_some_and(|t| now.duration_since(t) < m.cooldown)
            {
                continue;
            }
            m.last_fired = Some(now);
            fired.push(m.action);
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pattern: &str, action: &str, cooldown_ms: u64) -> TriggerConfig {
        TriggerConfig {
            pattern: pattern.into(),
            action: action.into(),
            cooldown_ms,
        }
    }

    #[test]
    fn compile_drops_empty_and_invalid_patterns() {
        let set = TriggerSet::compile(&[
            cfg("ERROR", "toast", 5000),
            cfg("", "toast", 5000),
            cfg("[invalid", "toast", 5000),
            cfg("FAIL", "bell", 5000),
        ]);
        assert_eq!(set.regexes.len(), 2, "only the two valid patterns survive");
        assert_eq!(set.meta[0].action, TriggerAction::Toast);
        assert_eq!(set.meta[1].action, TriggerAction::Bell);
        assert!(!set.is_empty());
        assert!(TriggerSet::compile(&[]).is_empty());
    }

    #[test]
    fn action_defaults_to_toast_and_bell_is_opt_in() {
        assert_eq!(TriggerAction::parse("bell"), TriggerAction::Bell);
        assert_eq!(TriggerAction::parse("toast"), TriggerAction::Toast);
        assert_eq!(TriggerAction::parse("banana"), TriggerAction::Toast);
    }

    #[test]
    fn a_hit_fires_once_then_the_cooldown_suppresses_repeats() {
        let mut set = TriggerSet::compile(&[cfg("ERROR", "toast", 5000)]);
        let t0 = Instant::now();
        assert_eq!(set.cooldown_filter(&[true], t0), vec![TriggerAction::Toast]);
        // 1s later: still inside the 5s cooldown
        let t1 = t0 + Duration::from_secs(1);
        assert!(set.cooldown_filter(&[true], t1).is_empty());
        // 6s after the first fire: cooldown elapsed, fires again
        let t2 = t0 + Duration::from_secs(6);
        assert_eq!(set.cooldown_filter(&[true], t2), vec![TriggerAction::Toast]);
    }

    #[test]
    fn a_miss_never_fires_and_triggers_are_independent() {
        let mut set =
            TriggerSet::compile(&[cfg("ERROR", "toast", 5000), cfg("FAIL", "bell", 5000)]);
        let t0 = Instant::now();
        assert!(set.cooldown_filter(&[false, false], t0).is_empty());
        // only the second matches
        assert_eq!(
            set.cooldown_filter(&[false, true], t0),
            vec![TriggerAction::Bell]
        );
        // the first still fires independently afterwards
        assert_eq!(
            set.cooldown_filter(&[true, false], t0 + Duration::from_millis(1)),
            vec![TriggerAction::Toast]
        );
    }

    #[test]
    fn zero_cooldown_fires_every_hit() {
        let mut set = TriggerSet::compile(&[cfg("X", "bell", 0)]);
        let t0 = Instant::now();
        assert_eq!(set.cooldown_filter(&[true], t0), vec![TriggerAction::Bell]);
        assert_eq!(
            set.cooldown_filter(&[true], t0 + Duration::from_millis(1)),
            vec![TriggerAction::Bell]
        );
    }

    #[test]
    fn a_pattern_matches_anywhere_in_the_new_output_text() {
        let set = TriggerSet::compile(&[cfg(r"ERROR|FAIL", "toast", 5000)]);
        assert!(set.regexes[0].is_match("build failed: ERROR here"));
        assert!(set.regexes[0].is_match("FAIL"));
        assert!(!set.regexes[0].is_match("all good"));
    }
}
