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
#[test]
fn probe_loads_fake_conpty_dll() {
    let Some(dll) = std::env::var_os("BAYAN_PROBE_CONPTY") else {
        return;
    };
    let code = load_bait(&dll);
    if code == 0 {
        let marker = Path::new(&dll).with_file_name("probe.ok");
        std::fs::write(&marker, b"ok").expect("probe marker");
    }
    std::process::exit(code);
}

fn load_bait(dll: &std::ffi::OsStr) -> i32 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    // Don't let a bad image pop a dialog and stall CI.
    unsafe { SetErrorMode(0x0001 | 0x0002 | 0x8000) };
    let wide: Vec<u16> = std::ffi::OsStr::new(dll)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
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

#[test]
fn planted_conpty_dll_is_not_loaded() {
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
    std::fs::write(&exe_dll, BAIT).unwrap();
    let _restore = Restore {
        prev: std::env::current_dir().unwrap(),
        exe_dll: exe_dll.clone(),
        temp: temp.clone(),
    };

    assert_bait_loads_in_a_child(&cwd_dll);

    std::env::set_current_dir(&temp).unwrap();
    // Same call main() makes first. CWD is the directory that holds the bait.
    crate::harden_dll_search();
    exercise_pty();

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

fn assert_bait_loads_in_a_child(dll: &Path) {
    let marker = dll.with_file_name("probe.ok");
    let _ = std::fs::remove_file(&marker);
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(PROBE_TEST)
        .env("BAYAN_PROBE_CONPTY", dll)
        .output()
        .expect("re-exec the test harness");
    assert!(
        marker.is_file(),
        "bait conpty.dll did not LoadLibraryW in a child process (status {:?}).\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exercise_pty() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.arg("/c");
    cmd.arg("exit");
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    let status = child.wait().expect("wait for cmd");
    drop(child);
    drop(pair.master);
    drain.join().expect("pty drain");
    assert!(status.success(), "cmd /c exit -> {status:?}");
}

fn system32_dir() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
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

    let process = unsafe { GetCurrentProcess() };
    let mut modules = vec![std::ptr::null_mut::<c_void>(); 1024];
    let bytes = u32::try_from(modules.len() * std::mem::size_of::<*mut c_void>()).unwrap();
    let mut needed = 0u32;
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
    prev: PathBuf,
    exe_dll: PathBuf,
    temp: PathBuf,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
        let _ = std::fs::remove_file(&self.exe_dll);
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}
