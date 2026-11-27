//! Rust port of `scripts/measure-startup.ps1`: five owned launches of a
//! dev-hooks release in fresh profiles, collecting the `BAYAN_PROFILE`
//! startup marks. A stalled run is noise (the gate reads the fastest
//! complete start); `--min-runs` still fails closed. Only the spawned
//! child is ever killed.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
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

pub fn is_probe(args: &[String]) -> bool {
    args.first().map(|a| a == "startup").unwrap_or(false)
}
