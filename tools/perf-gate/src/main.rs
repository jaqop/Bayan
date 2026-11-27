//! CLI mirror of `scripts/perf_gate.py`:
//!   perf-gate probe.log startup.log latency.log idle.log [--print] [--baseline path]

use std::path::PathBuf;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let print = argv.iter().any(|a| a == "--print");
    let baseline_arg = argv.windows(2).find(|w| w[0] == "--baseline").map(|w| w[1].clone());
    let logs: Vec<&String> = argv.iter()
        .filter(|a| !a.starts_with("--") && Some((*a).clone()) != baseline_arg)
        .collect();
    if logs.is_empty() {
        eprintln!("usage: perf-gate <log>... [--print] [--baseline path]");
        std::process::exit(2);
    }
    let mut text = String::new();
    for log in &logs {
        match std::fs::read_to_string(log) {
            Ok(t) => { text.push_str(&t); text.push('\n'); }
            Err(e) => {
                println!("::error::Invalid performance data: {e}");
                std::process::exit(2);
            }
        }
    }
    let measured = match perf_gate::parse(&text) {
        Ok(m) => m,
        Err(e) => {
            println!("::error::Invalid performance data: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = perf_gate::validate_measured(&measured) {
        println!("::error::Invalid performance data: {e}");
        std::process::exit(2);
    }
    if print {
        println!("{}", serde_json::to_string_pretty(&measured).unwrap());
        return;
    }
    let baseline_path = baseline_arg.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("perf/baseline.json"));
    let baseline: serde_json::Value = match std::fs::read_to_string(&baseline_path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            println!("::error::Invalid performance data: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = perf_gate::validate_baseline(&baseline) {
        println!("::error::Invalid performance data: {e}");
        std::process::exit(2);
    }
    let tolerance = baseline.get("tolerance").and_then(|v| v.as_f64()).unwrap_or(0.10);
    let (failures, gains, rows) = perf_gate::compare(&baseline, &measured, tolerance);
    println!("{}", perf_gate::table(&rows));
    for g in &gains {
        println!("note: {g}");
    }
    for f in &failures {
        println!("::error::{f}");
    }
    std::process::exit(if failures.is_empty() { 0 } else { 1 });
}
