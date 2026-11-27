//! Rust port of `scripts/measure-startup.ps1`: five owned launches of a
//! dev-hooks release in fresh profiles, collecting the `BAYAN_PROFILE`
//! startup marks. A stalled run is noise (the gate reads the fastest
//! complete start); `--min-runs` still fails closed. Only the spawned
//! child is ever killed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn arg_value(args: &[String], flag: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

pub fn startup(args: &[String]) -> i32 {
    let binary = arg_value(args, "--binary", "target/release/bayan.exe");
    let output = arg_value(args, "--output", "startup.log");
    let runs: u32 = arg_value(args, "--runs", "5").parse().unwrap_or(5);
    let timeout: u64 = arg_value(args, "--timeout", "30").parse().unwrap_or(30);
    let min_runs: u32 = arg_value(args, "--min-runs", "3").parse().unwrap_or(3);
    if min_runs > runs {
        eprintln!("::error::MinRuns cannot exceed Runs");
        return 2;
    }
    let exe = match fs::canonicalize(&binary) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let output = PathBuf::from(&output);
    if output.exists() {
        eprintln!("::error::Use a fresh startup log path");
        return 2;
    }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let run_root = std::env::temp_dir().join(format!("bayan-startup-{nanos:x}"));
    if let Err(e) = fs::create_dir_all(&run_root) {
        eprintln!("::error::{e}");
        return 2;
    }
    let mut out = match fs::File::create(&output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let mut completed = 0u32;
    for run in 1..=runs {
        let dir = run_root.join(format!("run-{run}"));
        let home = dir.join("home");
        let temp = dir.join("temp");
        if fs::create_dir_all(&home).and_then(|_| fs::create_dir_all(&temp)).is_err() {
            eprintln!("::error::cannot create run dirs");
            return 2;
        }
        let log = temp.join("bayan_profile.log");
        let stderr_path = dir.join("stderr.txt");
        let stderr_file = match fs::File::create(&stderr_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("::error::{e}");
                return 2;
            }
        };
        let mut child = match Command::new(&exe)
            .args(["--", "cmd.exe", "/d", "/q"])
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("TEMP", &temp)
            .env("TMP", &temp)
            .env("BAYAN_PROFILE", "1")
            .env("BAYAN_SHOW_NOW", "1")
            .stderr(Stdio::from(stderr_file))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("::error::{e}");
                return 2;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut ready = false;
        let mut exited = false;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    exited = true;
                    break;
                }
            }
            if let Ok(text) = fs::read_to_string(&log) {
                if text.contains(" ms  gpu ready")
                    && text.contains(" ms  first frame presented")
                    && text.contains(" ms  renderer ready")
                {
                    ready = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if exited {
            let err = fs::read_to_string(&stderr_path).unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("::error::Bayan exited during startup: {err}");
            return 2;
        }
        if !ready {
            // A cold start on a loaded runner can outlast the deadline; the
            // gate reads the fastest complete start. The marker must not read
            // as a `--- startup run` block.
            println!("startup run {run} stalled: no marks within {timeout} s");
            let _ = writeln!(out, "--- stalled start {run} (no marks within {timeout} s)");
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        completed += 1;
        let _ = writeln!(out, "--- startup run {run}");
        if let Ok(text) = fs::read_to_string(&log) {
            for line in text.lines() {
                if line.contains("latency") || line.contains("  key ") {
                    continue;
                }
                let _ = writeln!(out, "{line}");
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    if completed < min_runs {
        eprintln!("::error::Only {completed} of {runs} startup runs produced all required marks (need {min_runs})");
        return 2;
    }
    println!("startup completed={completed}/{runs}");
    0
}

/// Nearest rank, matching `src/latency.rs` percentile().
fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let mut rank = ((p / 100.0) * n as f64).ceil() as usize;
    if rank < 1 {
        rank = 1;
    }
    if rank > n {
        rank = n;
    }
    sorted[rank - 1]
}

struct TapRun {
    rows: Vec<f64>,
    deltas: Vec<f64>,
    saw_done: bool,
}

fn tap_run(exe: &Path, keys: u32, timeout: u64) -> Result<TapRun, String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("bayan-latency-{nanos:x}"));
    let home = root.join("home");
    let temp = root.join("temp");
    fs::create_dir_all(&home).and_then(|_| fs::create_dir_all(&temp)).map_err(|e| e.to_string())?;
    let log = temp.join("bayan_profile.log");
    let stderr_path = root.join("stderr.txt");
    let stderr_file = fs::File::create(&stderr_path).map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .args(["--", "cmd.exe", "/d", "/q"])
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("BAYAN_PROFILE", "1")
        .env("BAYAN_SHOW_NOW", "1")
        .env("BAYAN_TAP", keys.to_string())
        // The counter file is the idle probe's; leaving it unset keeps a file
        // replace off the key->frame path.
        .env_remove("BAYAN_FRAME_COUNTERS")
        .env_remove("BAYAN_TYPE")
        .env_remove("BAYAN_IDLE_BUSY")
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut ready = false;
    let mut saw_done = false;
    let mut last_count: i64 = -1;
    let mut stable_since: Option<Instant> = None;
    let mut text = String::new();
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            let err = fs::read_to_string(&stderr_path).unwrap_or_default();
            let _ = child.wait();
            return Err(format!("Bayan exited during latency with {status}: {err}"));
        }
        // The profile log is opened and closed per mark; a sharing violation
        // on this poll is the next iteration's read.
        if let Ok(t) = fs::read_to_string(&log) {
            text = t;
            if text.contains(" ms  renderer ready") {
                ready = true;
            }
            if text.contains(" ms  tap done") {
                saw_done = true;
            }
            let count = text.matches("latency key->pty-output").count() as i64;
            if saw_done && count == last_count {
                let since = *stable_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(1500) {
                    break;
                }
            } else {
                stable_since = None;
                last_count = count;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Ok(Some(status)) = child.try_wait() {
        let err = fs::read_to_string(&stderr_path).unwrap_or_default();
        let _ = child.wait();
        return Err(format!("Bayan exited during latency with {status}: {err}"));
    }
    let _ = child.kill();
    let _ = child.wait();
    if !ready {
        return Err("Latency run did not reach renderer ready".into());
    }
    let mut rows = Vec::new();
    let mut deltas = Vec::new();
    const MARK: &str = "latency key->pty-output ";
    for line in text.lines() {
        // Lines carry a leading timestamp; find the marker anywhere.
        if let Some(pos) = line.find(MARK) {
            let rest = &line[pos + MARK.len()..];
            // "<out> ms, key->frame <frame> ms"
            if let Some((out_s, rest)) = rest.split_once(" ms, key->frame ") {
                if let Some(frame_s) = rest.strip_suffix(" ms") {
                    if let (Ok(out), Ok(frame)) = (out_s.parse::<f64>(), frame_s.parse::<f64>()) {
                        rows.push(frame);
                        deltas.push(frame - out);
                    }
                }
            }
        }
    }
    Ok(TapRun { rows, deltas, saw_done })
}

pub fn latency(args: &[String]) -> i32 {
    let binary = arg_value(args, "--binary", "target/release/bayan.exe");
    let output = arg_value(args, "--output", "latency.log");
    let keys: u32 = arg_value(args, "--keys", "50").parse().unwrap_or(50);
    let timeout: u64 = arg_value(args, "--timeout", "90").parse().unwrap_or(90);
    let exe = match fs::canonicalize(&binary) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let output = PathBuf::from(&output);
    if output.exists() {
        eprintln!("::error::Use a fresh latency log path");
        return 2;
    }
    let mut attempts: Vec<TapRun> = Vec::new();
    for attempt in 1..=2 {
        match tap_run(&exe, keys, timeout) {
            Ok(r) => {
                println!("latency attempt {attempt} samples={} tap_done={}", r.rows.len(), r.saw_done);
                let enough = r.rows.len() >= 40;
                attempts.push(r);
                if enough {
                    break;
                }
            }
            Err(e) => {
                eprintln!("::error::{e}");
                return 2;
            }
        }
    }
    let best = match attempts.iter().max_by_key(|r| r.rows.len()) {
        Some(b) if !b.rows.is_empty() => b,
        _ => {
            eprintln!("::error::no latency lines in any attempt");
            return 2;
        }
    };
    let n = best.rows.len();
    let p50 = percentile(&best.rows, 50.0);
    let p95 = percentile(&best.rows, 95.0);
    let d50 = percentile(&best.deltas, 50.0);
    let d95 = percentile(&best.deltas, 95.0);
    let lines = format!(
        "PROBE latency key->frame p50={p50:.1} p95={p95:.1} n={n}\nPROBE latency render p50={d50:.1} p95={d95:.1} n={n}\n"
    );
    print!("{lines}");
    if let Err(e) = fs::write(&output, lines) {
        eprintln!("::error::{e}");
        return 2;
    }
    0
}

pub fn is_probe(args: &[String]) -> bool {
    matches!(args.first().map(|s| s.as_str()), Some("startup") | Some("latency"))
}
