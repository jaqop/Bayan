//! Key-to-frame latency for `BAYAN_PROFILE` runs.
//!
//! Three timestamps per sample: the key event reaching Bayan, the first PTY
//! read after Bayan wrote that key (normally its echo), and the next frame
//! handed to the GPU after that read. Only times are kept, never which key.
//!
//! What it does not see: the OS input queue before winit, and the
//! compositor and display after `present`. An end-to-end figure needs a
//! camera or a photodiode; this one splits Bayan + ConPTY + shell echo
//! (`to_output_ms`) from Bayan's own wake, draw and submit (`to_frame_ms`).

use std::time::{Duration, Instant};

/// A key with no echo (an arrow at the end of the prompt) is replaced by the
/// next key after this long, so later unrelated output cannot pair with it.
const STALE: Duration = Duration::from_secs(1);

/// A summary line every this many samples.
pub const SUMMARY_EVERY: usize = 25;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub to_output_ms: f64,
    pub to_frame_ms: f64,
}

pub struct Latency {
    key: Option<Instant>,
    out: Option<Instant>,
    samples: Vec<Sample>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Latency {
    pub const fn new() -> Self {
        Self {
            key: None,
            out: None,
            samples: Vec::new(),
        }
    }

    /// A key that reached the PTY, timed from its arrival. While an earlier
    /// key still waits for its echo, that one is kept: fast typing measures
    /// the first key of the burst.
    pub fn key_sent(&mut self, arrived: Instant) {
        if self
            .key
            .is_some_and(|k| arrived.saturating_duration_since(k) < STALE)
        {
            return;
        }
        self.key = Some(arrived);
        self.out = None;
    }

    pub fn output_read(&mut self, at: Instant) {
        if self.out.is_none() && self.key.is_some_and(|k| at >= k) {
            self.out = Some(at);
        }
    }

    /// Closes a sample if a key and its output are both waiting.
    pub fn frame_presented(&mut self, at: Instant) -> Option<Sample> {
        let (key, out) = (self.key?, self.out?);
        self.key = None;
        self.out = None;
        let s = Sample {
            to_output_ms: ms(out.saturating_duration_since(key)),
            to_frame_ms: ms(at.saturating_duration_since(key)),
        };
        self.samples.push(s);
        Some(s)
    }

    /// p50 / p95 / max over every sample so far, once per [`SUMMARY_EVERY`].
    pub fn summary(&self) -> Option<String> {
        let n = self.samples.len();
        if n == 0 || !n.is_multiple_of(SUMMARY_EVERY) {
            return None;
        }
        let stats = |pick: fn(&Sample) -> f64| {
            let mut v: Vec<f64> = self.samples.iter().map(pick).collect();
            v.sort_by(f64::total_cmp);
            format!(
                "p50={:.1} p95={:.1} max={:.1}",
                percentile(&v, 50.0),
                percentile(&v, 95.0),
                v[v.len() - 1]
            )
        };
        Some(format!(
            "latency summary n={n} key->frame {} | key->pty-output {}",
            stats(|s| s.to_frame_ms),
            stats(|s| s.to_output_ms)
        ))
    }
}

/// Nearest-rank percentile of a sorted, non-empty slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(t: Instant, ms: u64) -> Instant {
        t + Duration::from_millis(ms)
    }

    #[test]
    fn a_key_its_echo_and_the_next_frame_make_one_sample() {
        let t = Instant::now();
        let mut l = Latency::new();
        assert_eq!(l.frame_presented(t), None, "no key yet");
        l.key_sent(t);
        assert_eq!(l.frame_presented(after(t, 5)), None, "no echo yet");
        l.output_read(after(t, 12));
        l.output_read(after(t, 14)); // a second read keeps the first time
        let s = l.frame_presented(after(t, 20)).expect("one sample");
        assert!((s.to_output_ms - 12.0).abs() < 0.01, "{s:?}");
        assert!((s.to_frame_ms - 20.0).abs() < 0.01, "{s:?}");
        assert_eq!(l.frame_presented(after(t, 30)), None, "closed once");
    }

    #[test]
    fn a_burst_measures_its_first_key_and_a_silent_key_goes_stale() {
        let t = Instant::now();
        let mut l = Latency::new();
        l.key_sent(t);
        l.key_sent(after(t, 30)); // typed before the first echo
        l.output_read(after(t, 40));
        let s = l.frame_presented(after(t, 45)).unwrap();
        assert!((s.to_frame_ms - 45.0).abs() < 0.01, "{s:?}");

        // a key with no echo, then a key 2 s later: the later one is timed
        l.key_sent(after(t, 100));
        l.key_sent(after(t, 2100));
        l.output_read(after(t, 2110));
        let s = l.frame_presented(after(t, 2115)).unwrap();
        assert!((s.to_frame_ms - 15.0).abs() < 0.01, "{s:?}");
    }

    #[test]
    fn the_summary_reports_nearest_rank_percentiles() {
        assert_eq!(percentile(&[1.0], 95.0), 1.0);
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), 10.0);
        assert_eq!(percentile(&v, 95.0), 19.0);

        let t = Instant::now();
        let mut l = Latency::new();
        for i in 0..SUMMARY_EVERY as u64 {
            let k = after(t, i * 100);
            l.key_sent(k);
            l.output_read(after(k, 1 + i));
            l.frame_presented(after(k, 2 + i));
            let done = i as usize + 1;
            assert_eq!(l.summary().is_some(), done == SUMMARY_EVERY, "{done}");
        }
        let s = l.summary().unwrap();
        assert!(s.contains("n=25"), "{s}");
        assert!(s.contains("key->frame p50=14.0 p95=25.0 max=26.0"), "{s}");
    }
}
