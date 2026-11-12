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
fn create_flags_follow_the_host() {
    use crate::conpty_resolve::{
        create_flags, ConptyCapability, ConptyCreate, BUNDLED_LOAD_FLAGS, CONPTY_BASE_FLAGS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
        PSEUDOCONSOLE_PASSTHROUGH_MODE,
    };

    let inbox = create_flags(ConptyCapability::Inbox);
    let legacy = create_flags(ConptyCapability::LegacyPassthrough);
    let bundled = create_flags(ConptyCapability::BundledRewrite);
    assert_eq!(
        inbox,
        ConptyCreate {
            flags: CONPTY_BASE_FLAGS,
            passthrough: false,
        }
    );
    assert_eq!(inbox.flags & PSEUDOCONSOLE_PASSTHROUGH_MODE, 0);
    // The pre-rewrite host is the only one that is asked for bit 0x8.
    // On a 1.22+ conpty.dll that bit is glyph width, and passthrough is
    // the default, so the flags stay the base set.
    assert_eq!(
        legacy,
        ConptyCreate {
            flags: CONPTY_BASE_FLAGS | PSEUDOCONSOLE_PASSTHROUGH_MODE,
            passthrough: true,
        }
    );
    assert_eq!(legacy.flags, 0x1 | 0x2 | 0x4 | 0x8);
    assert_eq!(
        bundled,
        ConptyCreate {
            flags: CONPTY_BASE_FLAGS,
            passthrough: true,
        }
    );
    assert_eq!(bundled.flags, inbox.flags);
    assert_ne!(bundled.passthrough, inbox.passthrough);
    // A bundled load is an absolute path with both search flags. Flags 0
    // would search the exe directory, the current directory, and PATH.
    assert_eq!(
        BUNDLED_LOAD_FLAGS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32
    );
    assert_ne!(BUNDLED_LOAD_FLAGS, 0);
    assert_ne!(BUNDLED_LOAD_FLAGS, LOAD_LIBRARY_SEARCH_SYSTEM32);
}

#[test]
fn bundled_pair_is_passthrough_only_at_1_22_with_both_files() {
    use crate::conpty_resolve::{
        capability_from_probe, file_version_from_fixed, BundleProbe, ConptyCapability,
    };

    let rewrite = Some((1, 24, 2604, 2001));
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: true,
            open_console_is_file: true,
            version: rewrite,
        }),
        ConptyCapability::BundledRewrite
    );
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: true,
            open_console_is_file: true,
            version: Some((1, 22, 0, 0)),
        }),
        ConptyCapability::BundledRewrite
    );
    // The planted-DLL case: conpty.dll beside the exe, no OpenConsole.exe.
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: true,
            open_console_is_file: false,
            version: rewrite,
        }),
        ConptyCapability::Inbox
    );
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: false,
            open_console_is_file: true,
            version: rewrite,
        }),
        ConptyCapability::Inbox
    );
    // 1.21 still has the old engine. Do not advertise passthrough.
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: true,
            open_console_is_file: true,
            version: Some((1, 21, 0, 0)),
        }),
        ConptyCapability::Inbox
    );
    // No version resource: do not guess.
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: true,
            open_console_is_file: true,
            version: None,
        }),
        ConptyCapability::Inbox
    );
    // Inbox conhost is 10.0.x. A major other than 1 is not the NuGet pair.
    assert_eq!(
        capability_from_probe(BundleProbe {
            dll_is_file: true,
            open_console_is_file: true,
            version: Some((10, 0, 26100, 1)),
        }),
        ConptyCapability::Inbox
    );

    let mut fixed = [0u8; 16];
    fixed[0..4].copy_from_slice(&0xFEEF_04BDu32.to_le_bytes());
    // 1.24.2604.2001
    let ms: u32 = (1u32 << 16) | 24;
    let ls: u32 = (2604u32 << 16) | 2001;
    fixed[8..12].copy_from_slice(&ms.to_le_bytes());
    fixed[12..16].copy_from_slice(&ls.to_le_bytes());
    assert_eq!(file_version_from_fixed(&fixed), Some((1, 24, 2604, 2001)));
    assert!(file_version_from_fixed(&fixed[..15]).is_none());
    fixed[0] = 0;
    assert!(file_version_from_fixed(&fixed).is_none());
}

/// Supported (1.22+), unsupported (older or a non-1.x file version), and
/// unknown (no version resource) all return a plan. Only supported turns
/// passthrough on. The other two keep the inbox flags Bayan already uses.
#[test]
fn passthrough_follows_conpty_version_and_unknown_stays_inbox() {
    use crate::conpty_resolve::{negotiate, CONPTY_BASE_FLAGS};

    let supported = negotiate(Some((1, 22, 0, 1)));
    assert!(supported.passthrough);
    assert_eq!(supported.flags, CONPTY_BASE_FLAGS);
    assert_eq!(negotiate(Some((1, 24, 2604, 2001))), supported);

    let unsupported = negotiate(Some((1, 21, 0, 0)));
    assert!(!unsupported.passthrough);
    assert_eq!(unsupported.flags, CONPTY_BASE_FLAGS);
    let numbered_like_inbox = negotiate(Some((10, 0, 26100, 1)));
    assert_eq!(numbered_like_inbox, unsupported);

    let unknown = negotiate(None);
    assert!(!unknown.passthrough);
    assert_eq!(unknown, unsupported);
}

/// The only OpenConsole.exe Bayan will run is the absolute path beside the
/// executable. A bare name (PATH), the current directory, and a copy in
/// System32 are not that file.
#[test]
fn open_console_is_loaded_only_from_the_exe_directory() {
    use crate::conpty_resolve::{bundled_host_path, path_is_beside_exe};

    let exe = Path::new(r"C:\Program Files\Bayan\bayan.exe");
    let cwd = Path::new(r"C:\Users\me\Downloads");
    let beside = bundled_host_path(exe, "OpenConsole.exe").expect("exe-dir path");
    assert!(beside.is_absolute(), "{beside:?}");
    assert_eq!(
        beside.file_name().and_then(|n| n.to_str()),
        Some("OpenConsole.exe")
    );
    assert_eq!(beside.parent(), exe.parent());
    assert!(path_is_beside_exe(exe, &beside));
    assert!(path_is_beside_exe(
        exe,
        &bundled_host_path(exe, "conpty.dll").unwrap()
    ));

    assert!(bundled_host_path(exe, r"OpenConsole.exe").is_some());
    assert!(bundled_host_path(exe, r"..\OpenConsole.exe").is_none());
    assert!(bundled_host_path(exe, r"C:\OpenConsole.exe").is_none());
    assert!(bundled_host_path(Path::new("bayan.exe"), "OpenConsole.exe").is_none());

    let from_cwd = cwd.join("OpenConsole.exe");
    let from_path = Path::new("OpenConsole.exe");
    let from_system = Path::new(r"C:\Windows\System32\OpenConsole.exe");
    assert!(!path_is_beside_exe(exe, &from_cwd));
    assert!(!path_is_beside_exe(exe, from_path));
    assert!(!path_is_beside_exe(exe, from_system));
}

/// Missing, unsigned, and a host that did not start all stay on inbox.
/// The decision is a plan, not an error, so spawn does not fail closed.
#[test]
fn rejected_or_unstarted_open_console_falls_back_to_inbox() {
    use crate::conpty_resolve::{
        accept_bundled_host, capability_after_launch, hash_matches_pin, host_file_ok,
        ConptyCapability, HostFacts, OPEN_CONSOLE_SHA256,
    };

    let signed = HostFacts {
        exists: true,
        reparse: false,
        signature_ok: true,
        hash_ok: false,
        elevated: false,
        file_user_writable: false,
        dir_user_writable: false,
        acl_known: true,
    };
    assert!(accept_bundled_host(signed));
    assert!(accept_bundled_host(HostFacts {
        signature_ok: false,
        hash_ok: true,
        ..signed
    }));
    assert!(!accept_bundled_host(HostFacts {
        exists: false,
        ..signed
    }));
    assert!(!accept_bundled_host(HostFacts {
        reparse: true,
        ..signed
    }));
    assert!(!accept_bundled_host(HostFacts {
        signature_ok: false,
        hash_ok: false,
        ..signed
    }));
    assert!(!hash_matches_pin("abc", ""));
    assert!(hash_matches_pin("abc", "ABC"));
    assert_eq!(OPEN_CONSOLE_SHA256, "");
    assert_eq!(
        capability_after_launch(true, false),
        ConptyCapability::Inbox,
        "CreatePseudoConsole failed"
    );
    assert_eq!(
        capability_after_launch(false, false),
        ConptyCapability::Inbox,
        "verification failed"
    );
    assert_eq!(
        capability_after_launch(true, true),
        ConptyCapability::BundledRewrite
    );

    let dir = std::env::temp_dir().join(format!("bayan-oc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let unsigned = dir.join("OpenConsole.exe");
    std::fs::write(&unsigned, b"MZ not a signed image").unwrap();
    let unsigned_str = unsigned.to_str().expect("temp path");
    let unsigned_status = crate::conpty_resolve::signature_status(unsigned_str);
    assert_ne!(
        unsigned_status, 0,
        "an unsigned file must not verify ({unsigned_status:#x})"
    );
    assert!(!host_file_ok(unsigned_str, false));
    let _ = std::fs::remove_dir_all(&dir);

    // A real PE is not the same answer as the junk file. Catalog-signed
    // System32 binaries come back TRUST_E_NOSIGNATURE (0x800B0100) here;
    // an embedded Authenticode signature, which is what the ConPTY package's
    // OpenConsole.exe carries, is the zero status `host_file_ok` accepts.
    let system_ps = system32_dir()
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let system_ps = system_ps.to_str().expect("powershell path");
    let pe_status = crate::conpty_resolve::signature_status(system_ps);
    assert_ne!(
        pe_status, unsigned_status,
        "WinVerifyTrust did not distinguish a real PE ({pe_status:#x}) from the junk file ({unsigned_status:#x})"
    );
}

/// Elevated Bayan refuses OpenConsole.exe when the file or its directory
/// grants write to a standard user. A locked-down signed file is accepted.
#[test]
fn elevated_window_refuses_a_user_writable_open_console() {
    use crate::conpty_resolve::{
        accept_bundled_host, path_user_writable, user_can_write, HostFacts,
    };

    let sid = "S-1-5-21-1";
    assert!(user_can_write("D:(A;;FA;;;WD)", sid));
    assert!(user_can_write("D:(A;;FW;;;BU)", sid));
    assert!(user_can_write("D:(A;;0x120116;;;AU)", sid));
    assert!(user_can_write("D:(A;;FA;;;S-1-5-21-1)", sid));
    assert!(
        !user_can_write("D:(A;;FA;;;BA)(A;;0x1200a9;;;BU)", sid),
        "read for Users and full control for Administrators is not user-writable"
    );
    assert!(
        !user_can_write("D:(A;;FA;;;S-1-5-21-1)S:(ML;;NW;;;HI)", sid),
        "no-write-up at high integrity blocks a medium token"
    );

    let signed = HostFacts {
        exists: true,
        reparse: false,
        signature_ok: true,
        hash_ok: false,
        elevated: true,
        file_user_writable: false,
        dir_user_writable: false,
        acl_known: true,
    };
    assert!(accept_bundled_host(signed));
    assert!(!accept_bundled_host(HostFacts {
        file_user_writable: true,
        ..signed
    }));
    assert!(!accept_bundled_host(HostFacts {
        dir_user_writable: true,
        ..signed
    }));
    assert!(
        !accept_bundled_host(HostFacts {
            acl_known: false,
            ..signed
        }),
        "an unreadable ACL is a refusal while elevated"
    );
    assert!(accept_bundled_host(HostFacts {
        elevated: false,
        file_user_writable: true,
        dir_user_writable: true,
        ..signed
    }));

    let dir = std::env::temp_dir().join(format!("bayan-acl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("OpenConsole.exe");
    std::fs::write(&file, b"x").unwrap();
    let file_str = file.to_str().expect("temp path");
    let sddl = crate::conpty_resolve::security_sddl(file_str);
    assert_eq!(
        path_user_writable(file_str),
        Some(true),
        "a file this user just created must read as user-writable: {sddl:?}"
    );
    let _ = crate::conpty_resolve::running_elevated();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kernel32_file_version_is_readable() {
    let path = system32_dir().join("kernel32.dll");
    let text = path.to_str().expect("system32 path is unicode");
    let version = conpty_resolve::read_file_version(text);
    assert!(
        version.is_some(),
        "GetFileVersionInfoW(kernel32.dll) returned nothing for {}",
        path.display()
    );
}

#[test]
fn search_flag_matches_windows_sys() {
    assert_eq!(
        LOAD_LIBRARY_SEARCH_SYSTEM32,
        windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32
    );
    assert_eq!(
        crate::conpty_resolve::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
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
