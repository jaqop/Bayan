//! Rust port of `scripts/measure-idle.ps1`: quiet-window private memory and
//! redraws, the minimized busy-tab window, ten quiet tabs, and the 16 MB
//! `type` cat. Only the spawned child is ever killed. Win32 is reached with
//! a small manual FFI (no extra dependency).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type HWND = isize;
type HANDLE = isize;
type BOOL = i32;

#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[repr(C)]
struct ProcessEntry32W {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u16; 260],
}

#[link(name = "user32")]
extern "system" {
    fn ShowWindow(h: HWND, cmd: i32) -> BOOL;
    fn IsIconic(h: HWND) -> BOOL;
    fn IsWindowVisible(h: HWND) -> BOOL;
    fn GetWindow(h: HWND, cmd: u32) -> HWND;
    fn EnumWindows(cb: extern "system" fn(HWND, isize) -> BOOL, l: isize) -> BOOL;
    fn GetWindowThreadProcessId(h: HWND, pid: *mut u32) -> u32;
}
extern "system" {
    fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    fn CloseHandle(h: HANDLE) -> BOOL;
    fn K32GetProcessMemoryInfo(h: HANDLE, pmc: *mut ProcessMemoryCounters, cb: u32) -> BOOL;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    fn Process32FirstW(h: HANDLE, pe: *mut ProcessEntry32W) -> BOOL;
    fn Process32NextW(h: HANDLE, pe: *mut ProcessEntry32W) -> BOOL;
}

fn mb(bytes: usize) -> String {
    format!("{:.1}", bytes as f64 / 1048576.0)
}

fn private_working(pid: u32) -> Result<(String, String), String> {
    unsafe {
        let h = OpenProcess(0x0410, 0, pid); // QUERY_INFORMATION | VM_READ
        if h == 0 {
            return Err("OpenProcess failed".into());
        }
        let mut pmc: ProcessMemoryCounters = std::mem::zeroed();
        pmc.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        let ok = K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb);
        CloseHandle(h);
        if ok == 0 {
            return Err("GetProcessMemoryInfo failed".into());
        }
        Ok((mb(pmc.pagefile_usage), mb(pmc.working_set_size)))
    }
}

struct FindCtx {
    pid: u32,
    found: HWND,
}
extern "system" fn find_cb(h: HWND, l: isize) -> BOOL {
    unsafe {
        let ctx = &mut *(l as *mut FindCtx);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(h, &mut pid);
        if pid == ctx.pid && IsWindowVisible(h) != 0 && GetWindow(h, 4) == 0 {
            ctx.found = h;
            return 0;
        }
    }
    1
}

fn find_window(pid: u32) -> HWND {
    let mut ctx = FindCtx { pid, found: 0 };
    unsafe {
        EnumWindows(find_cb, &mut ctx as *mut FindCtx as isize);
    }
    ctx.found
}

fn has_cmd_child(pid: u32) -> bool {
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x2, 0); // TH32CS_SNAPPROCESS
        if snap == -1 || snap == 0 {
            return false;
        }
        let mut pe: ProcessEntry32W = std::mem::zeroed();
        pe.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut any = false;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                if pe.th32_parent_process_id == pid {
                    let name: String = String::from_utf16_lossy(
                        &pe.sz_exe_file[..pe.sz_exe_file.iter().position(|&c| c == 0).unwrap_or(260)],
                    );
                    if name.eq_ignore_ascii_case("cmd.exe") {
                        any = true;
                        break;
                    }
                }
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        any
    }
}

#[derive(Clone, Copy)]
struct Counters {
    redraws: i64,
    presents: i64,
}

fn read_counters(path: &Path) -> Result<Counters, String> {
    for _ in 0..20 {
        if let Ok(text) = fs::read_to_string(path) {
            if let (Some(r), Some(p)) = (
                text.find("redraws=").map(|i| &text[i + 8..]),
                text.find("presents=").map(|i| &text[i + 9..]),
            ) {
                let r: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
                let p: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let (Ok(r), Ok(p)) = (r.parse::<i64>(), p.parse::<i64>()) {
                    return Ok(Counters { redraws: r, presents: p });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("frame counters missing or malformed: {}", path.display()))
}

fn wait_mark(child: &mut std::process::Child, log: &Path, stderr: &Path, mark: &str, deadline: Instant) -> Result<(), String> {
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            let err = fs::read_to_string(stderr).unwrap_or_default();
            return Err(format!("Bayan exited while waiting for {mark}: {status} {err}"));
        }
        if let Ok(text) = fs::read_to_string(log) {
            if text.contains(mark) {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for {mark}"))
}

fn wait_stable(child: &mut std::process::Child, counters: &Path, deadline: Instant) -> Result<Counters, String> {
    let mut prev: Option<Counters> = None;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return Err("Bayan exited while frame counters were settling".into());
        }
        let now = read_counters(counters)?;
        if let Some(p) = prev {
            if p.redraws == now.redraws && p.presents == now.presents {
                return Ok(now);
            }
        }
        prev = Some(now);
        std::thread::sleep(Duration::from_millis(1500));
    }
    Err("frame counters did not settle for a quiet window".into())
}

struct Run {
    root: PathBuf,
    log: PathBuf,
    counters: PathBuf,
    stderr: PathBuf,
    child: std::process::Child,
}

fn spawn_run(exe: &Path, prefix: &str, config: &str, extra: &[(&str, &str)], remove: &[&str], args: &[&str]) -> Result<Run, String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("{prefix}{nanos:x}"));
    let home = root.join("home");
    let temp = root.join("temp");
    fs::create_dir_all(home.join(".bayan")).and_then(|_| fs::create_dir_all(&temp)).map_err(|e| e.to_string())?;
    fs::write(home.join(".bayan").join("config.json"), config).map_err(|e| e.to_string())?;
    let log = temp.join("bayan_profile.log");
    let counters = temp.join("bayan_frame_counters.txt");
    let stderr = root.join("stderr.txt");
    let stderr_file = fs::File::create(&stderr).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("BAYAN_PROFILE", "1")
        .env("BAYAN_SHOW_NOW", "1")
        .env("BAYAN_FRAME_COUNTERS", "1")
        .stderr(Stdio::from(stderr_file));
    for (k, v) in extra {
        cmd.env(k, v);
    }
    for k in remove {
        cmd.env_remove(k);
    }
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    Ok(Run { root, log, counters, stderr, child })
}

fn stop(run: &mut Run) {
    let _ = run.child.kill();
    let _ = run.child.wait();
    let _ = fs::remove_dir_all(&run.root);
}

const WINDOW_S: u64 = 5;

pub fn idle(args: &[String]) -> i32 {
    let binary = args.windows(2).find(|w| w[0] == "--binary").map(|w| w[1].clone()).unwrap_or_else(|| "target/release/bayan.exe".into());
    let output = args.windows(2).find(|w| w[0] == "--output").map(|w| w[1].clone()).unwrap_or_else(|| "idle.log".into());
    let timeout: u64 = args.windows(2).find(|w| w[0] == "--timeout").map(|w| w[1].clone()).unwrap_or_else(|| "60".into()).parse().unwrap_or(60);
    let exe = match fs::canonicalize(&binary) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let output = PathBuf::from(&output);
    if output.exists() {
        eprintln!("::error::Use a fresh idle log path");
        return 2;
    }
    let mut out = match fs::File::create(&output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let mut emit = |s: String| {
        println!("{s}");
        let _ = writeln!(out, "{s}");
    };
    let deadline = || Instant::now() + Duration::from_secs(timeout);
    let cfg = "{\"cursor_blink\":false}";

    // Quiet visible window: private memory + zero redraws over 5s.
    let mut r = match spawn_run(&exe, "bayan-idle-", cfg, &[], &["BAYAN_IDLE_BUSY", "BAYAN_TAP", "BAYAN_TYPE"], &["--", "cmd.exe", "/d", "/q"]) {
        Ok(r) => r,
        Err(e) => { eprintln!("::error::{e}"); return 2; }
    };
    let res = (|| -> Result<(), String> {
        wait_mark(&mut r.child, &r.log, &r.stderr, " ms  renderer ready", deadline())?;
        wait_mark(&mut r.child, &r.log, &r.stderr, " ms  first pty output", deadline())?;
        let start = wait_stable(&mut r.child, &r.counters, deadline())?;
        std::thread::sleep(Duration::from_secs(WINDOW_S));
        if let Ok(Some(_)) = r.child.try_wait() { return Err("Bayan exited during the quiet window".into()); }
        let end = read_counters(&r.counters)?;
        let (private, working) = private_working(r.child.id())?;
        let presents = end.presents - start.presents;
        let redraws = end.redraws - start.redraws;
        if presents < 0 || redraws < 0 { return Err("frame counters went backwards".into()); }
        emit(format!("PROBE idle_memory private={private} MB working_set={working} MB"));
        emit(format!("PROBE idle_redraw visible presents={presents} redraws={redraws} window_s={WINDOW_S}"));
        Ok(())
    })();
    stop(&mut r);
    if let Err(e) = res { eprintln!("::error::idle visible: {e}"); return 2; }

    // Minimized busy tab: report-only redraws over 5s.
    let mut r = match spawn_run(&exe, "bayan-idle-min-", cfg, &[("BAYAN_IDLE_BUSY", "1")], &["BAYAN_TAP", "BAYAN_TYPE"], &["--", "cmd.exe", "/d", "/q"]) {
        Ok(r) => r,
        Err(e) => { eprintln!("::error::{e}"); return 2; }
    };
    let res = (|| -> Result<(), String> {
        wait_mark(&mut r.child, &r.log, &r.stderr, " ms  renderer ready", deadline())?;
        let before = read_counters(&r.counters)?;
        std::thread::sleep(Duration::from_secs(1));
        let after = read_counters(&r.counters)?;
        if after.redraws <= before.redraws && after.presents <= before.presents {
            return Err("busy tab did not move frame counters before minimize".into());
        }
        let mut hwnd = 0;
        for _ in 0..50 {
            hwnd = find_window(r.child.id());
            if hwnd != 0 { break; }
            std::thread::sleep(Duration::from_millis(100));
        }
        if hwnd == 0 { return Err("minimized probe found no window handle".into()); }
        unsafe { ShowWindow(hwnd, 6); }
        std::thread::sleep(Duration::from_millis(300));
        if unsafe { IsIconic(hwnd) } == 0 { return Err("ShowWindow did not minimize the window".into()); }
        std::thread::sleep(Duration::from_millis(750));
        let start = read_counters(&r.counters)?;
        std::thread::sleep(Duration::from_secs(WINDOW_S));
        if let Ok(Some(_)) = r.child.try_wait() { return Err("Bayan exited during the minimized window".into()); }
        let end = read_counters(&r.counters)?;
        let presents = end.presents - start.presents;
        let redraws = end.redraws - start.redraws;
        if presents < 0 || redraws < 0 { return Err("frame counters went backwards".into()); }
        emit(format!("PROBE idle_redraw minimized presents={presents} redraws={redraws} window_s={WINDOW_S}"));
        Ok(())
    })();
    stop(&mut r);
    if let Err(e) = res { eprintln!("::error::idle minimized: {e}"); return 2; }

    // Ten quiet tabs: private memory + zero redraws over 5s.
    let mut r = match spawn_run(&exe, "bayan-idle-tabs-", cfg, &[("BAYAN_TABS", "10")], &["BAYAN_IDLE_BUSY", "BAYAN_TAP", "BAYAN_TYPE"], &["--", "cmd.exe", "/d", "/q"]) {
        Ok(r) => r,
        Err(e) => { eprintln!("::error::{e}"); return 2; }
    };
    let res = (|| -> Result<(), String> {
        wait_mark(&mut r.child, &r.log, &r.stderr, " ms  renderer ready", deadline())?;
        wait_mark(&mut r.child, &r.log, &r.stderr, " ms  first pty output", deadline())?;
        let start = wait_stable(&mut r.child, &r.counters, deadline())?;
        std::thread::sleep(Duration::from_secs(WINDOW_S));
        if let Ok(Some(_)) = r.child.try_wait() { return Err("Bayan exited during the ten-tab window".into()); }
        let end = read_counters(&r.counters)?;
        let (private, working) = private_working(r.child.id())?;
        let presents = end.presents - start.presents;
        let redraws = end.redraws - start.redraws;
        if presents < 0 || redraws < 0 { return Err("frame counters went backwards".into()); }
        emit(format!("PROBE tabs_memory tabs=10 private={private} MB working_set={working} MB"));
        emit(format!("PROBE idle_redraw tabs10 presents={presents} redraws={redraws} window_s={WINDOW_S}"));
        Ok(())
    })();
    stop(&mut r);
    if let Err(e) = res { eprintln!("::error::ten tabs: {e}"); return 2; }

    // 16 MB cat through the real ConPTY path: seconds to last frame, peak private.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let catroot = std::env::temp_dir().join(format!("bayan-idle-cat-{nanos:x}"));
    let _ = fs::create_dir_all(&catroot);
    let file = catroot.join("cat-16mb.txt");
    let filler = "the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnop";
    {
        let mut f = match fs::File::create(&file) { Ok(f) => f, Err(e) => { eprintln!("::error::{e}"); return 2; } };
        for i in 0..200000u32 {
            let _ = writeln!(f, "{i:06} {filler}");
        }
    }
    let cfg_cat = "{\"cursor_blink\":false,\"scrollbar\":{\"autohide\":false}}";
    let type_cmd = format!("type \"{}\" & exit\r", file.display());
    let mut r = match spawn_run(&exe, "bayan-idle-catrun-", cfg_cat, &[("BAYAN_TYPE", &type_cmd)], &["BAYAN_IDLE_BUSY", "BAYAN_TAP", "BAYAN_TABS"], &["-p", "Command Prompt"]) {
        Ok(r) => r,
        Err(e) => { eprintln!("::error::{e}"); return 2; }
    };
    let res = (|| -> Result<(), String> {
        let cat_deadline = Instant::now() + Duration::from_secs(180);
        wait_mark(&mut r.child, &r.log, &r.stderr, " ms  first pty output", cat_deadline)?;
        let started = Instant::now();
        let (base, _) = private_working(r.child.id())?;
        let base_bytes = (base.parse::<f64>().unwrap_or(0.0) * 1048576.0) as usize;
        let mut peak = base_bytes;
        let mut last: Option<Counters> = None;
        let mut changed = started;
        let mut done_at: Option<Instant> = None;
        loop {
            if Instant::now() >= cat_deadline { return Err("large cat did not settle within 180s".into()); }
            if let Ok(Some(_)) = r.child.try_wait() {
                if done_at.is_some() { break; }
                return Err("Bayan exited during the cat".into());
            }
            if let Ok((p, _)) = private_working(r.child.id()) {
                let bytes = (p.parse::<f64>().unwrap_or(0.0) * 1048576.0) as usize;
                if bytes > peak { peak = bytes; }
            }
            let now = read_counters(&r.counters)?;
            if last.map(|l| l.redraws != now.redraws || l.presents != now.presents).unwrap_or(true) {
                changed = Instant::now();
            }
            last = Some(now);
            if done_at.is_none() && !has_cmd_child(r.child.id()) {
                done_at = Some(Instant::now());
            }
            if let Some(d) = done_at {
                if changed.elapsed() >= Duration::from_secs(2) && d.elapsed() >= Duration::from_secs(2) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let end = changed.max(done_at.unwrap_or(changed));
        let seconds = end.duration_since(started).as_secs_f64();
        if peak.saturating_sub(base_bytes) < 16 * 1048576 {
            return Err("large cat: private memory rose too little; the file's output did not arrive".into());
        }
        emit(format!("PROBE cat mb=16 seconds={seconds:.2} peak_private={} MB", mb(peak)));
        Ok(())
    })();
    stop(&mut r);
    let _ = fs::remove_dir_all(&catroot);
    if let Err(e) = res { eprintln!("::error::large cat: {e}"); return 2; }
    0
}
