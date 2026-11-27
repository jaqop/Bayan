//! Rust port of `scripts/perf_gate.py`: parse the `PROBE` lines the ignored
//! `measure_` tests print, validate them against `perf/baseline.json`, and
//! fail a change that makes the reader slower or the scrollback heavier.
//!
//! Semantics mirror the Python gate: best throughput, lowest scrollback and
//! startup, worst latency/idle; per-group tolerance overrides; idle_redraw
//! baseline 0 with any increase failing; minimized redraws report-only.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

pub type Group = BTreeMap<String, f64>;
pub type Measured = BTreeMap<String, Group>;

pub const MIN_LATENCY_SAMPLES: u64 = 40;
/// One-line switch, mirroring the Python gate. Flip once minimized redraws
/// are stopped; until then a non-zero minimized window is report-only.
pub const MINIMIZED_REDRAW_REQUIRED: bool = false;

const NUMBER: &str = r"[-+]?(?:\d+(?:\.\d+)?|nan|inf(?:inity)?)";

macro_rules! regex {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).unwrap())
        }
    };
}

regex!(throughput_re, &format!(
    r"(?i)(?m)PROBE throughput (\w+)\s+\d+ MB: process_chunk\s+({N}) MB/s, vte alone\s+({N}) MB/s \(({N})% of vte\)\s*$", N = NUMBER));
regex!(scrollback_re, &format!(
    r"(?i)(?m)PROBE scrollback\s+(\d+) lines x \d+ cols: \+\s*({N}) MB once full, \+\s*({N}) MB after as much again \(history (\d+)\)\s*$", N = NUMBER));
// The Python gate uses a lookbehind (?<![\w.+-]); the regex crate has no
// lookaround, so matches are post-filtered by their preceding character.
regex!(startup_re, &format!(
    r"(?i)(?m)({N}) ms  ([a-z][a-z ]*[a-z])\s*$", N = NUMBER));
regex!(startup_shape_re, r"(?i)\s+ms  [a-z][a-z ]*[a-z]\s*$");
regex!(latency_re, &format!(
    r"(?i)(?m)PROBE latency key->frame p50=({N}) p95=({N}) n=(\d+)\s*$", N = NUMBER));
regex!(latency_render_re, &format!(
    r"(?i)(?m)PROBE latency render p50=({N}) p95=({N}) n=(\d+)\s*$", N = NUMBER));
regex!(idle_memory_re, &format!(
    r"(?i)(?m)PROBE idle_memory private=({N}) MB working_set=({N}) MB\s*$", N = NUMBER));
regex!(idle_redraw_re, r"(?i)(?m)PROBE idle_redraw (visible|minimized|tabs10) presents=(\d+) redraws=(\d+) window_s=(\d+)\s*$");
regex!(tabs_memory_re, &format!(
    r"(?i)(?m)PROBE tabs_memory tabs=(\d+) private=({N}) MB working_set=({N}) MB\s*$", N = NUMBER));
regex!(cat_re, &format!(
    r"(?i)(?m)PROBE cat mb=(\d+) seconds=({N}) peak_private=({N}) MB\s*$", N = NUMBER));

pub fn required() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::from([
        ("throughput", vec!["ascii", "sgr", "arabic"]),
        ("scrollback", vec!["10000", "100000"]),
        ("startup", vec!["first frame presented", "renderer ready"]),
        ("startup_ratio", vec!["first frame / gpu ready"]),
        ("latency", vec!["key->frame p95"]),
        ("idle_memory", vec!["private"]),
        ("idle_redraw", vec![
            "visible presents", "visible redraws",
            "minimized presents", "minimized redraws",
            "tabs10 presents", "tabs10 redraws",
        ]),
        ("tabs_memory", vec!["private 10 tabs"]),
        ("cat_time", vec!["16 MB seconds"]),
        ("cat_memory", vec!["16 MB peak private"]),
    ])
}

fn allows_zero(group: &str) -> bool {
    group == "idle_redraw"
}

fn report_only(group: &str, name: &str) -> bool {
    !MINIMIZED_REDRAW_REQUIRED && group == "idle_redraw" && name.starts_with("minimized")
}

fn number(value: f64, label: &str, allow_zero: bool) -> Result<f64, String> {
    if !value.is_finite() {
        return Err(format!("{label} must be a finite number"));
    }
    if value < 0.0 || (value == 0.0 && !allow_zero) {
        return Err(format!(
            "{label} must be {}",
            if allow_zero { "nonnegative" } else { "positive" }
        ));
    }
    Ok(value)
}

fn json_number(v: &serde_json::Value, label: &str, allow_zero: bool) -> Result<f64, String> {
    match v {
        serde_json::Value::Number(n) => {
            let f = n.as_f64().ok_or_else(|| format!("{label} must be a finite number"))?;
            number(f, label, allow_zero)
        }
        _ => Err(format!("{label} must be a finite number")),
    }
}

pub fn validate_baseline(b: &serde_json::Value) -> Result<(), String> {
    let obj = b.as_object().ok_or("baseline must be an object")?;
    let req = required();
    for key in obj.keys() {
        if key != "tolerance" && key != "tolerances" && !req.contains_key(key.as_str()) {
            return Err("baseline contains unknown policy keys".into());
        }
    }
    for (group, names) in &req {
        let values = obj.get(*group).and_then(|v| v.as_object())
            .ok_or_else(|| format!("baseline {group} is missing required metrics"))?;
        for name in names {
            if !values.contains_key(*name) {
                return Err(format!("baseline {group} is missing required metrics"));
            }
        }
        for (name, value) in values {
            json_number(value, &format!("baseline {group} {name}"), allows_zero(group))?;
        }
    }
    json_number(obj.get("tolerance").unwrap_or(&serde_json::json!(0.10)), "tolerance", true)?;
    if let Some(t) = obj.get("tolerances") {
        let t = t.as_object().ok_or("tolerances must name known metric groups")?;
        for (group, value) in t {
            if !req.contains_key(group.as_str()) {
                return Err("tolerances must name known metric groups".into());
            }
            json_number(value, &format!("tolerance {group}"), true)?;
        }
    }
    Ok(())
}

pub fn validate_measured(m: &Measured) -> Result<(), String> {
    for (group, names) in required() {
        let values = m.get(group);
        for name in names {
            let v = values.and_then(|g| g.get(name))
                .ok_or_else(|| format!("measured {group} is missing required metrics"))?;
            number(*v, &format!("measured {group} {name}"), allows_zero(group))?;
        }
    }
    Ok(())
}

fn preceded_by_wordish(text: &str, start: usize) -> bool {
    text[..start].chars().next_back().is_some_and(|c| {
        c.is_alphanumeric() || c == '_' || c == '.' || c == '+' || c == '-'
    })
}

fn startup_matches(text: &str) -> Vec<(f64, String)> {
    startup_re().captures_iter(text)
        .filter(|c| {
            let m = c.get(0).unwrap();
            !preceded_by_wordish(text, m.start())
        })
        .map(|c| (c[1].parse::<f64>().unwrap(), c[2].to_lowercase()))
        .collect()
}

pub fn parse(text: &str) -> Result<Measured, String> {
    // Per-line malformed checks, same order as the Python gate.
    for line in text.lines() {
        for (marker, ok) in [
            ("PROBE throughput", throughput_re().is_match(line)),
            ("PROBE scrollback", scrollback_re().is_match(line)),
        ] {
            if line.contains(marker) && !ok {
                return Err(format!("malformed {marker} line"));
            }
        }
        if line.contains("PROBE latency") && !latency_re().is_match(line) && !latency_render_re().is_match(line) {
            return Err("malformed PROBE latency line".into());
        }
        if line.contains("PROBE idle_memory") && !idle_memory_re().is_match(line) {
            return Err("malformed PROBE idle_memory line".into());
        }
        if line.contains("PROBE idle_redraw") && !idle_redraw_re().is_match(line) {
            return Err("malformed PROBE idle_redraw line".into());
        }
        if line.contains("PROBE tabs_memory") && !tabs_memory_re().is_match(line) {
            return Err("malformed PROBE tabs_memory line".into());
        }
        if line.contains("PROBE cat") && !cat_re().is_match(line) {
            return Err("malformed PROBE cat line".into());
        }
        if startup_shape_re().is_match(line) && startup_matches(line).is_empty() {
            return Err("malformed startup mark".into());
        }
    }

    let mut out: Measured = BTreeMap::new();
    fn put(out: &mut Measured, group: &str, name: String, value: f64, best: fn(f64, f64) -> f64) {
        let g = out.entry(group.to_string()).or_default();
        let e = g.entry(name).or_insert(value);
        *e = best(*e, value);
    }

    for c in throughput_re().captures_iter(text) {
        let kind = c[1].to_lowercase();
        number(c[2].parse::<f64>().unwrap(), &format!("process_chunk rate {kind}"), false)?;
        number(c[3].parse::<f64>().unwrap(), &format!("vte rate {kind}"), false)?;
        let v = number(c[4].parse::<f64>().unwrap(), &format!("throughput {kind}"), false)?;
        put(&mut out, "throughput", kind, v, f64::max);
    }
    for c in scrollback_re().captures_iter(text) {
        let lines = c[1].to_string();
        number(c[3].parse::<f64>().unwrap(), &format!("scrollback after refill {lines}"), false)?;
        if c[4] != c[1] {
            return Err("scrollback history does not match the requested capacity".into());
        }
        let v = number(c[2].parse::<f64>().unwrap(), &format!("scrollback {lines}"), false)?;
        put(&mut out, "scrollback", lines, v, f64::min);
    }
    for (ms, mark) in startup_matches(text) {
        let v = number(ms, &format!("startup {mark}"), mark == "main start")?;
        put(&mut out, "startup", mark, v, f64::min);
    }
    // Ratio from the fastest start; a cold start's lower ratio would hide
    // work added after the GPU came up.
    let mut fastest = f64::INFINITY;
    let mut ratio = f64::NAN;
    for block in text.split("--- startup run").skip(1) {
        let mut marks: BTreeMap<String, f64> = BTreeMap::new();
        for (ms, mark) in startup_matches(block) {
            if marks.insert(mark.clone(), ms).is_some() {
                return Err(format!("duplicate startup mark: {mark}"));
            }
        }
        let gpu = *marks.get("gpu ready").ok_or("startup run is missing required marks")?;
        let frame = *marks.get("first frame presented").ok_or("startup run is missing required marks")?;
        let ready = *marks.get("renderer ready").ok_or("startup run is missing required marks")?;
        if !(0.0 < gpu && gpu <= frame && frame <= ready) {
            return Err("startup marks must satisfy 0 < gpu <= frame <= renderer".into());
        }
        if frame < fastest {
            fastest = frame;
            ratio = (frame / gpu * 1000.0).round() / 1000.0;
        }
    }
    if !ratio.is_nan() {
        out.entry("startup_ratio".into()).or_default()
            .insert("first frame / gpu ready".into(), ratio);
    }
    for c in latency_re().captures_iter(text) {
        let n: u64 = c[3].parse().unwrap();
        if n < MIN_LATENCY_SAMPLES {
            return Err(format!("latency n={n} is below {MIN_LATENCY_SAMPLES}"));
        }
        number(c[1].parse::<f64>().unwrap(), "latency p50", false)?;
        let v = number(c[2].parse::<f64>().unwrap(), "latency p95", false)?;
        put(&mut out, "latency", "key->frame p95".into(), v, f64::max);
    }
    for c in latency_render_re().captures_iter(text) {
        let n: u64 = c[3].parse().unwrap();
        if n < MIN_LATENCY_SAMPLES {
            return Err(format!("latency render n={n} is below {MIN_LATENCY_SAMPLES}"));
        }
        let p50 = number(c[1].parse::<f64>().unwrap(), "latency render p50", true)?;
        let p95 = number(c[2].parse::<f64>().unwrap(), "latency render p95", true)?;
        put(&mut out, "latency_render", "p50".into(), p50, f64::max);
        put(&mut out, "latency_render", "p95".into(), p95, f64::max);
    }
    for c in idle_memory_re().captures_iter(text) {
        number(c[2].parse::<f64>().unwrap(), "idle_memory working_set", false)?;
        let v = number(c[1].parse::<f64>().unwrap(), "idle_memory private", false)?;
        put(&mut out, "idle_memory", "private".into(), v, f64::max);
    }
    for c in idle_redraw_re().captures_iter(text) {
        if c[4].parse::<u64>().unwrap() != 5 {
            return Err("idle_redraw window_s must be 5".into());
        }
        let which = c[1].to_lowercase();
        for (suffix, raw) in [("presents", &c[2]), ("redraws", &c[3])] {
            let v = raw.parse::<f64>().unwrap();
            put(&mut out, "idle_redraw", format!("{which} {suffix}"), v, f64::max);
        }
    }
    for c in tabs_memory_re().captures_iter(text) {
        number(c[3].parse::<f64>().unwrap(), "tabs_memory working_set", false)?;
        let count: u64 = c[1].parse().unwrap();
        let v = number(c[2].parse::<f64>().unwrap(), "tabs_memory private", false)?;
        put(&mut out, "tabs_memory", format!("private {count} tabs"), v, f64::max);
    }
    for c in cat_re().captures_iter(text) {
        let mb: u64 = c[1].parse().unwrap();
        let secs = number(c[2].parse::<f64>().unwrap(), "cat seconds", false)?;
        let peak = number(c[3].parse::<f64>().unwrap(), "cat peak_private", false)?;
        put(&mut out, "cat_time", format!("{mb} MB seconds"), secs, f64::max);
        put(&mut out, "cat_memory", format!("{mb} MB peak private"), peak, f64::max);
    }
    // Ensure every group key exists (Python returns all groups, possibly empty).
    for g in ["throughput", "scrollback", "startup", "startup_ratio", "latency",
              "latency_render", "idle_memory", "idle_redraw", "tabs_memory",
              "cat_time", "cat_memory"] {
        out.entry(g.to_string()).or_default();
    }
    Ok(out)
}

pub struct Row {
    pub label: String,
    pub base: f64,
    pub got: Option<f64>,
    pub unit: &'static str,
    pub verdict: String,
}

pub fn compare(
    baseline: &serde_json::Value,
    measured: &Measured,
    tolerance: f64,
) -> (Vec<String>, Vec<String>, Vec<Row>) {
    let mut failures = Vec::new();
    let mut gains = Vec::new();
    let mut rows = Vec::new();
    let checks: &[(&str, &str, bool, bool)] = &[
        ("throughput", "% of vte", true, true),
        ("scrollback", "MB", false, true),
        ("startup", "ms", false, false),
        ("startup_ratio", "x", false, false),
        ("latency", "ms", false, false),
        ("idle_memory", "MB", false, true),
        ("idle_redraw", "count", false, false),
        ("tabs_memory", "MB", false, true),
        ("cat_time", "s", false, false),
        ("cat_memory", "MB", false, true),
    ];
    let tolerances = baseline.get("tolerances").and_then(|v| v.as_object());
    for (group, unit, higher_is_better, report_gains) in checks {
        let group_tolerance = tolerances
            .and_then(|t| t.get(*group))
            .and_then(|v| v.as_f64())
            .unwrap_or(tolerance);
        let base_group = baseline.get(*group).and_then(|v| v.as_object());
        let got_group = measured.get(*group);
        if let Some(bg) = base_group {
            for (name, base_v) in bg {
                let base = base_v.as_f64().unwrap_or(f64::NAN);
                let label = format!("{group} {name}");
                let got = got_group.and_then(|g| g.get(name)).copied();
                let Some(got) = got else {
                    failures.push(format!("{label}: the probe printed no value"));
                    rows.push(Row { label, base, got: None, unit, verdict: "missing".into() });
                    continue;
                };
                let worse = if *group == "idle_redraw" {
                    if got > base { f64::INFINITY } else { 0.0 }
                } else {
                    let change = if base != 0.0 { (got - base) / base } else { 0.0 };
                    if *higher_is_better { -change } else { change }
                };
                let verdict;
                if report_only(group, name) {
                    verdict = "report-only".to_string();
                    if worse > 0.0 {
                        gains.push(format!(
                            "{label}: {got} {unit} is report-only (set MINIMIZED_REDRAW_REQUIRED = True to require it)"
                        ));
                    }
                } else if *group == "idle_redraw" && got > base {
                    verdict = "FAIL".to_string();
                    failures.push(format!("{label}: {got} {unit} against a baseline of {base} (must be {base})"));
                } else if worse > group_tolerance {
                    verdict = "FAIL".to_string();
                    failures.push(format!(
                        "{label}: {got} {unit} against a baseline of {base} ({:.0}% worse, limit {:.0}%)",
                        worse * 100.0, group_tolerance * 100.0
                    ));
                } else if *report_gains && -worse > group_tolerance {
                    verdict = "better".to_string();
                    gains.push(format!("{label}: {got} {unit} against {base}; raise the baseline"));
                } else {
                    verdict = "ok".to_string();
                }
                rows.push(Row { label, base, got: Some(got), unit, verdict });
            }
        }
    }
    (failures, gains, rows)
}

pub fn table(rows: &[Row]) -> String {
    let mut out = String::from("| probe | baseline | measured | |\n|---|---|---|---|\n");
    for r in rows {
        let shown = match r.got {
            None => "—".to_string(),
            Some(g) => format!("{g} {}", r.unit),
        };
        out.push_str(&format!("| {} | {} {} | {} | {} |\n", r.label, r.base, r.unit, shown, r.verdict));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
PROBE throughput ascii  64 MB: process_chunk   117.0 MB/s, vte alone   148.1 MB/s (79% of vte)
PROBE throughput ascii  64 MB: process_chunk   101.0 MB/s, vte alone   148.1 MB/s (68% of vte)
PROBE throughput sgr    64 MB: process_chunk    72.5 MB/s, vte alone   172.5 MB/s (42% of vte)
PROBE throughput arabic 64 MB: process_chunk   157.2 MB/s, vte alone   206.8 MB/s (76% of vte)
PROBE scrollback  10000 lines x 120 cols: + 33.0 MB once full, + 1.0 MB after as much again (history 10000)
PROBE scrollback 100000 lines x 120 cols: + 303.0 MB once full, + 1.0 MB after as much again (history 100000)
PROBE latency key->frame p50=20.0 p95=30.0 n=50
PROBE idle_memory private=291.3 MB working_set=124.7 MB
PROBE idle_redraw visible presents=0 redraws=0 window_s=5
PROBE idle_redraw minimized presents=0 redraws=0 window_s=5
PROBE idle_redraw tabs10 presents=0 redraws=0 window_s=5
PROBE tabs_memory tabs=10 private=314.0 MB working_set=128.9 MB
PROBE cat mb=16 seconds=7.7 peak_private=120.0 MB
";

    const STARTUP: &str = "\
--- startup run 1
0.4 ms  main start
73.3 ms  gpu ready
80.0 ms  first frame presented
100.7 ms  renderer ready
--- startup run 2
0.4 ms  main start
131.2 ms  gpu ready
140.7 ms  first frame presented
200.7 ms  renderer ready
";

    #[test]
    fn best_throughput_lowest_scrollback_and_startup() {
        let m = parse(&format!("{LOG}{STARTUP}")).unwrap();
        assert_eq!(m["throughput"]["ascii"], 79.0);
        assert_eq!(m["scrollback"]["10000"], 33.0);
        assert_eq!(m["startup"]["first frame presented"], 80.0);
        // ratio from the FASTEST start (80/73.3), not the cold one
        assert_eq!(m["startup_ratio"]["first frame / gpu ready"], 1.091);
    }

    #[test]
    fn malformed_throughput_line_fails() {
        let bad = LOG.replace("(79% of vte)", "(79% of vte");
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn short_latency_sample_fails() {
        let bad = LOG.replace("n=50", "n=39");
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn startup_mark_order_is_enforced() {
        let bad = STARTUP.replace("80.0 ms  first frame presented", "50.0 ms  first frame presented");
        assert!(parse(&format!("{LOG}{bad}")).is_err());
    }

    fn baseline() -> serde_json::Value {
        serde_json::json!({
            "tolerance": 0.10,
            "throughput": {"ascii": 79, "sgr": 42, "arabic": 76},
            "scrollback": {"10000": 33, "100000": 303},
            "startup": {"first frame presented": 140, "renderer ready": 200},
            "startup_ratio": {"first frame / gpu ready": 1.08},
            "latency": {"key->frame p95": 155},
            "idle_memory": {"private": 291.3},
            "idle_redraw": {"visible presents": 0, "visible redraws": 0,
                            "minimized presents": 0, "minimized redraws": 0,
                            "tabs10 presents": 0, "tabs10 redraws": 0},
            "tabs_memory": {"private 10 tabs": 323.9},
            "cat_time": {"16 MB seconds": 8.1},
            "cat_memory": {"16 MB peak private": 126.7}
        })
    }

    #[test]
    fn the_baseline_itself_passes() {
        let m = parse(&format!("{LOG}{STARTUP}")).unwrap();
        let (failures, gains, _) = compare(&baseline(), &m, 0.10);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(gains.is_empty(), "{gains:?}");
    }

    #[test]
    fn slower_reader_fails_and_ten_percent_is_the_edge() {
        let mut m = parse(&format!("{LOG}{STARTUP}")).unwrap();
        m.get_mut("throughput").unwrap().insert("ascii".into(), 70.0);
        let (failures, _, _) = compare(&baseline(), &m, 0.10);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("throughput ascii"));
        m.get_mut("throughput").unwrap().insert("ascii".into(), 71.2);
        assert!(compare(&baseline(), &m, 0.10).0.is_empty());
    }

    #[test]
    fn idle_redraw_increase_fails_but_minimized_is_report_only() {
        let mut m = parse(&format!("{LOG}{STARTUP}")).unwrap();
        m.get_mut("idle_redraw").unwrap().insert("visible redraws".into(), 1.0);
        let (failures, _, _) = compare(&baseline(), &m, 0.10);
        assert_eq!(failures.len(), 1);
        m.get_mut("idle_redraw").unwrap().insert("visible redraws".into(), 0.0);
        m.get_mut("idle_redraw").unwrap().insert("minimized redraws".into(), 9.0);
        let (failures, gains, _) = compare(&baseline(), &m, 0.10);
        assert!(failures.is_empty());
        assert_eq!(gains.len(), 1);
        assert!(gains[0].contains("report-only"));
    }

    #[test]
    fn per_group_tolerance_overrides() {
        let mut b = baseline();
        b["tolerances"] = serde_json::json!({"throughput": 0.15});
        let mut m = parse(&format!("{LOG}{STARTUP}")).unwrap();
        m.get_mut("throughput").unwrap().insert("sgr".into(), 37.0);
        // 37 vs 42 is 12% worse: fails at 10%, passes at 15%
        assert_eq!(compare(&baseline(), &m, 0.10).0.len(), 1);
        assert!(compare(&b, &m, 0.10).0.is_empty());
    }
}
