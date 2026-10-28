//! ConPTY loader tests. The harness compiles this; `main` does not.
//!
//! The bait DLL in `tests/fixtures/fake_conpty.dll` is a real x64 image with
//! no imported DLLs. `CreatePseudoConsole` and `ResizePseudoConsole` return
//! `E_FAIL`. Its source and the mingw command that produced it are
//! `tests/fixtures/fake_conpty.c`.

use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::conpty_resolve::{self, ConptyLoader, LOAD_LIBRARY_SEARCH_SYSTEM32};

const BAIT: &[u8] = include_bytes!("../tests/fixtures/fake_conpty.dll");
/// libtest's fully qualified name. `--exact` fails closed if this drifts:
/// the parent requires the probe marker, so a filter that matches nothing
/// does not look like a successful load.
const PROBE_TEST: &str = "conpty_load::probe_loads_fake_conpty_dll";

struct RecordingLoader {
    loads: RefCell<Vec<(String, u32)>>,
    procs: RefCell<Vec<String>>,
}

impl ConptyLoader for RecordingLoader {
    type Module = usize;
    type Symbol = usize;

    fn load_library_ex(&self, file_name: &str, flags: u32) -> Result<Self::Module, String> {
        self.loads.borrow_mut().push((file_name.to_string(), flags));
        Ok(1)
    }

    fn get_proc_address(&self, module: Self::Module, name: &str) -> Result<Self::Symbol, String> {
        assert_eq!(module, 1);
        let mut procs = self.procs.borrow_mut();
        procs.push(name.to_string());
        Ok(procs.len())
    }
}

#[test]
fn resolver_never_calls_a_bare_name_load() {
    let loader = RecordingLoader {
        loads: RefCell::new(Vec::new()),
        procs: RefCell::new(Vec::new()),
    };
    let resolved = conpty_resolve::resolve_conpty(&loader).expect("kernel32 load");
    let loads = loader.loads.borrow();
    assert_eq!(loads.len(), 1, "the resolver opens exactly one library");
    let (name, flags) = &loads[0];
    // A bare-name load is LoadLibraryW, or LoadLibraryExW with flags 0,
    // which searches the exe directory, the CWD, and PATH.
    assert_eq!(name, "kernel32.dll");
    assert_eq!(*flags, LOAD_LIBRARY_SEARCH_SYSTEM32);
    assert_ne!(*flags, 0);
    assert!(!name.eq_ignore_ascii_case("conpty.dll"));
    assert!(!name.contains('\\') && !name.contains('/'));
    assert_eq!(
        loader.procs.borrow().as_slice(),
        [
            "CreatePseudoConsole",
            "ResizePseudoConsole",
            "ClosePseudoConsole",
        ]
    );
    assert_eq!(resolved.module, 1);
    assert_eq!(resolved.create_pseudo_console, 1);
    assert_eq!(resolved.resize_pseudo_console, 2);
    assert_eq!(resolved.close_pseudo_console, 3);
}

#[test]
fn search_flag_matches_windows_sys() {
    assert_eq!(
        LOAD_LIBRARY_SEARCH_SYSTEM32,
        windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32
    );
}

/// Child entry. With `BAYAN_PROBE_CONPTY` unset this is a no-op so the
/// normal `cargo test` run stays green. The planted-DLL test re-execs the
/// harness and sets the variable; a full-path `LoadLibraryW` in that other
/// process proves the bait is an image Windows will map, which is what
/// makes a still-present bare-name load visible in the parent.
///
/// Return instead of `process::exit`. Exiting from a libtest worker on
/// Windows can deadlock the harness stdio lock, and the parent then waits
/// forever.
#[test]
fn probe_loads_fake_conpty_dll() {
    let Some(dll) = std::env::var_os("BAYAN_PROBE_CONPTY") else {
        return;
    };
    let code = load_bait(&dll);
    let marker = Path::new(&dll).with_file_name(if code == 0 { "probe.ok" } else { "probe.fail" });
    std::fs::write(&marker, code.to_string()).expect("probe marker");
}

fn load_bait(dll: &std::ffi::OsStr) -> i32 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    // Don't let a bad image pop a dialog and stall CI.
    // SAFETY: SetErrorMode takes a flag word and touches no caller memory.
    unsafe { SetErrorMode(0x0001 | 0x0002 | 0x8000) };
    let wide: Vec<u16> = std::ffi::OsStr::new(dll)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return 2;
    }
    for name in [
        "CreatePseudoConsole",
        "ResizePseudoConsole",
        "ClosePseudoConsole",
    ] {
        let mut c_name = Vec::with_capacity(name.len() + 1);
        c_name.extend_from_slice(name.as_bytes());
        c_name.push(0);
        // SAFETY: `module` is a live handle from LoadLibraryW above, and
        // `c_name` is a NUL-terminated ANSI name that outlives the call.
        let sym = unsafe { GetProcAddress(module, c_name.as_ptr()) };
        if sym.is_none() {
            return 3;
        }
    }
    0
}

#[link(name = "kernel32")]
extern "system" {
    fn SetErrorMode(mode: u32) -> u32;
}

const PTY_TEST: &str = "conpty_load::pty_under_planted_conpty_dll";

#[test]
fn planted_conpty_dll_is_not_loaded() {
    // The re-execed harness sets one of these. Returning keeps a missed
    // `--exact` filter from spawning another copy of this test.
    if std::env::var_os("BAYAN_PROBE_CONPTY").is_some()
        || std::env::var_os("BAYAN_CONPTY_EXERCISE").is_some()
    {
        return;
    }
    for needle in [
        "CreatePseudoConsole",
        "ResizePseudoConsole",
        "ClosePseudoConsole",
    ] {
        assert!(
            BAIT.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "bait DLL is missing export name {needle}"
        );
    }
    let temp = std::env::temp_dir().join(format!("bayan-conpty-{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();
    let cwd_dll = temp.join("conpty.dll");
    let exe_dll = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("conpty.dll");
    std::fs::write(&cwd_dll, BAIT).unwrap();
    let _restore = Restore {
        exe_dll: exe_dll.clone(),
        temp: temp.clone(),
    };

    // Probe before the exe-adjacent plant. A conpty.dll beside this harness
    // is on the child process's application-directory search path from the
    // moment it starts.
    assert_bait_loads_in_a_child(&cwd_dll);
    std::fs::write(&exe_dll, BAIT).unwrap();
    assert_pty_child_rejects_bait(&temp);
}

/// Child entry for the PTY half. Unset in the normal suite, so this is a
/// no-op there. The parent sets `BAYAN_CONPTY_EXERCISE` to the temp
/// directory that holds the bait and has already planted the other copy
/// next to the test exe. Doing the spawn here lets the parent
/// `TerminateProcess` this harness if ConPTY or `cmd.exe` does not return.
#[test]
fn pty_under_planted_conpty_dll() {
    let Some(temp) = std::env::var_os("BAYAN_CONPTY_EXERCISE") else {
        return;
    };
    let temp = PathBuf::from(temp);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_planted_pty(&temp);
    }));
    let text = match &result {
        Ok(()) => "ok\n".to_string(),
        Err(payload) => format!("panic: {}\n", panic_message(payload)),
    };
    std::fs::write(temp.join("pty-report.txt"), &text).expect("pty report");
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_planted_pty(temp: &Path) {
    note(temp, "start");
    std::env::set_current_dir(temp).expect("cwd");
    note(temp, "cwd");
    // Same call main() makes first. CWD is the directory that holds the bait.
    crate::harden_dll_search();
    note(temp, "hardened");
    exercise_pty(temp);
    note(temp, "pty done");
    assert_modules_clean(temp);
    note(temp, "modules ok");
}

fn assert_bait_loads_in_a_child(dll: &Path) {
    let marker = dll.with_file_name("probe.ok");
    let fail = dll.with_file_name("probe.fail");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&fail);
    let mut child = reexec(PROBE_TEST, "BAYAN_PROBE_CONPTY", dll.as_os_str());
    let status = wait_for(&mut child.child, std::time::Duration::from_secs(30));
    let (stdout, stderr) = child.output();
    let fail_body = std::fs::read_to_string(&fail).unwrap_or_default();
    assert!(
        marker.is_file(),
        "bait conpty.dll did not LoadLibraryW in a child process (status {status:?}, fail {fail_body:?}).\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
}

fn assert_pty_child_rejects_bait(temp: &Path) {
    let report = temp.join("pty-report.txt");
    let _ = std::fs::remove_file(&report);
    let mut child = reexec(PTY_TEST, "BAYAN_CONPTY_EXERCISE", temp.as_os_str());
    let status = wait_for(&mut child.child, std::time::Duration::from_secs(45));
    let (stdout, stderr) = child.output();
    let body = std::fs::read_to_string(&report).unwrap_or_default();
    let trace = std::fs::read_to_string(temp.join("pty-trace.txt")).unwrap_or_default();
    assert!(
        status.as_ref().is_some_and(|s| s.success()) && body == "ok\n",
        "planted conpty.dll was loaded or the pty child did not finish (status {status:?}).\nreport:\n{body}\ntrace:\n{trace}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
}

struct Reexec {
    child: std::process::Child,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
}

impl Reexec {
    fn output(self) -> (Vec<u8>, Vec<u8>) {
        let stdout = self.stdout.join().unwrap_or_else(|_| Vec::new());
        let stderr = self.stderr.join().unwrap_or_else(|_| Vec::new());
        (stdout, stderr)
    }
}

fn reexec(test: &str, key: &str, value: &std::ffi::OsStr) -> Reexec {
    let mut child = Command::new(std::env::current_exe().expect("current_exe"))
        .arg("--exact")
        .arg(test)
        .arg("--test-threads")
        .arg("1")
        .env_remove("BAYAN_PROBE_CONPTY")
        .env_remove("BAYAN_CONPTY_EXERCISE")
        .env(key, value)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("re-exec the test harness");
    let stdout = drain(child.stdout.take().expect("stdout"));
    let stderr = drain(child.stderr.take().expect("stderr"));
    Reexec {
        child,
        stdout,
        stderr,
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn wait_for(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn exercise_pty(temp: &Path) {
    note(temp, "openpty");
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    note(temp, "openpty ok");
    // Absolute path: CommandBuilder still walks PATH for a bare `cmd.exe`,
    // and a dead PATH entry can block `exists` for a long time.
    let comspec =
        std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string());
    let mut cmd = CommandBuilder::new(comspec);
    cmd.arg("/c");
    cmd.arg("exit");
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut input = pair.master.take_writer().expect("pty input");
    let drain = std::thread::spawn(move || {
        use std::io::Write;
        let mut buf = [0u8; 4096];
        let mut cursor_query = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // INHERIT_CURSOR asks the host for its cursor position.
                    // Keep the input open and answer even across read boundaries.
                    cursor_query.extend_from_slice(&buf[..n]);
                    if cursor_query.windows(4).any(|w| w == b"\x1b[6n") {
                        input.write_all(b"\x1b[1;1R").expect("cursor reply");
                        input.flush().expect("flush cursor reply");
                        cursor_query.clear();
                    }
                    if cursor_query.len() > 3 {
                        cursor_query.drain(..cursor_query.len() - 3);
                    }
                }
            }
        }
    });
    note(temp, "spawn");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    note(temp, "spawned");
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < std::time::Duration::from_secs(20) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                note(temp, "cmd timeout");
                panic!("cmd.exe did not exit within 20s");
            }
            Err(err) => panic!("try_wait: {err}"),
        }
    };
    note(temp, "cmd exited");
    drop(child);
    drop(pair.master);
    drain.join().expect("pty drain");
    assert!(status.success(), "cmd /c exit -> {status:?}");
}

fn note(temp: &Path, msg: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(temp.join("pty-trace.txt"))
        .expect("pty trace");
    writeln!(file, "{msg}").expect("pty trace");
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else if let Some(msg) = payload.downcast_ref::<&str>() {
        (*msg).to_string()
    } else {
        "unknown panic".into()
    }
}

fn assert_modules_clean(temp: &Path) {
    let exe_dll = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("exe dir")
        .join("conpty.dll");
    let cwd_dll = temp.join("conpty.dll");
    let system32 = system32_dir();
    assert!(
        norm(&system32).ends_with("\\system32"),
        "GetSystemDirectoryW returned {}",
        system32.display()
    );
    let loaded = loaded_module_paths();
    assert!(
        loaded.iter().any(|p| file_name_is(p, "kernel32.dll")),
        "kernel32.dll was not in the module list"
    );
    for path in &loaded {
        if file_name_is(path, "kernel32.dll") || file_name_is(path, "conpty.dll") {
            assert!(
                is_under(path, &system32),
                "{} loaded from {}, not under {}",
                path.file_name().unwrap().to_string_lossy(),
                path.display(),
                system32.display()
            );
        }
    }
    for planted in [&exe_dll, &cwd_dll] {
        let planted_norm = norm(planted);
        assert!(
            loaded.iter().all(|p| norm(p) != planted_norm),
            "planted {} was mapped into the process",
            planted.display()
        );
    }
}

fn system32_dir() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the length passed is `buf`'s own, so the write stays in bounds.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    assert!(
        n > 0 && (n as usize) < buf.len(),
        "GetSystemDirectoryW failed"
    );
    PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]))
}

fn loaded_module_paths() -> Vec<PathBuf> {
    use std::ffi::c_void;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows_sys::Win32::System::ProcessStatus::EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: returns a pseudo-handle; no arguments, nothing to free.
    let process = unsafe { GetCurrentProcess() };
    let mut modules = vec![std::ptr::null_mut::<c_void>(); 1024];
    let bytes = u32::try_from(modules.len() * std::mem::size_of::<*mut c_void>()).unwrap();
    let mut needed = 0u32;
    // SAFETY: `bytes` is exactly the size of `modules`, so the write stays in
    // bounds, and `needed` is a live local.
    let ok = unsafe { EnumProcessModules(process, modules.as_mut_ptr(), bytes, &mut needed) };
    assert_ne!(
        ok,
        0,
        "EnumProcessModules: {}",
        std::io::Error::last_os_error()
    );
    let count = needed as usize / std::mem::size_of::<*mut c_void>();
    assert!(
        count <= modules.len(),
        "module list truncated ({needed} bytes)"
    );
    let mut paths = Vec::with_capacity(count);
    for module in &modules[..count] {
        if module.is_null() {
            continue;
        }
        let mut buf = [0u16; 1024];
        // SAFETY: `module` came from EnumProcessModules for this process, and
        // the length passed is `buf`'s own.
        let len = unsafe { GetModuleFileNameW(*module, buf.as_mut_ptr(), buf.len() as u32) };
        if len == 0 || len as usize >= buf.len() {
            continue;
        }
        paths.push(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )));
    }
    paths
}

fn norm(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(raw.as_ref());
    trimmed.replace('/', "\\").to_ascii_lowercase()
}

fn is_under(path: &Path, dir: &Path) -> bool {
    let path = norm(path);
    let dir = norm(dir).trim_end_matches('\\').to_string();
    path == dir || path.starts_with(&format!("{dir}\\"))
}

fn file_name_is(path: &Path, name: &str) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
}

struct Restore {
    exe_dll: PathBuf,
    temp: PathBuf,
}

impl Drop for Restore {
    fn drop(&mut self) {
        // The pty child may still be releasing the image for a moment after
        // TerminateProcess. Retry so deps/conpty.dll does not survive the test.
        for _ in 0..50 {
            if std::fs::remove_file(&self.exe_dll).is_ok() || !self.exe_dll.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}
