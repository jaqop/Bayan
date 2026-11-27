//! Rust port of `scripts/assert-no-planted-dxc.ps1`: plant the marker
//! `dxcompiler.dll` next to the binary, run Bayan to renderer-ready, and
//! assert the planted DLL was never mapped (module list + marker file).
//! Only the spawned child is killed; only the two planted names are removed.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

type HANDLE = isize;
type HMODULE = isize;
type BOOL = i32;

extern "system" {
    fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    fn CloseHandle(h: HANDLE) -> BOOL;
    fn K32EnumProcessModules(h: HANDLE, mods: *mut HMODULE, cb: u32, needed: *mut u32) -> BOOL;
    fn K32GetModuleFileNameExW(h: HANDLE, m: HMODULE, buf: *mut u16, n: u32) -> u32;
}

const EXPECTED: &str = "4327247952c12c9809fd034fc38610709b84e2da74b40517edd9d2174aae8a2e";

fn mapped_dxcompiler(pid: u32) -> Result<Option<String>, String> {
    unsafe {
        let h = OpenProcess(0x0410, 0, pid);
        if h == 0 {
            return Err("OpenProcess failed".into());
        }
        let mut mods = [0isize; 1024];
        let mut needed: u32 = 0;
        let ok = K32EnumProcessModules(
            h,
            mods.as_mut_ptr(),
            (mods.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
        );
        if ok == 0 {
            CloseHandle(h);
            return Err("EnumProcessModules failed".into());
        }
        let count = (needed as usize) / std::mem::size_of::<HMODULE>();
        let mut found = None;
        for &m in mods.iter().take(count.min(mods.len())) {
            let mut buf = [0u16; 1024];
            let n = K32GetModuleFileNameExW(h, m, buf.as_mut_ptr(), buf.len() as u32);
            if n == 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&buf[..n as usize]);
            if name.rsplit(['\\', '/']).next().unwrap_or("").eq_ignore_ascii_case("dxcompiler.dll") {
                found = Some(name);
                break;
            }
        }
        CloseHandle(h);
        Ok(found)
    }
}

pub fn plant(args: &[String]) -> i32 {
    let binary = args.windows(2).find(|w| w[0] == "--binary").map(|w| w[1].clone());
    let source = args.windows(2).find(|w| w[0] == "--source").map(|w| w[1].clone());
    let (binary, source) = match (binary, source) {
        (Some(b), Some(s)) => (b, s),
        _ => {
            eprintln!("usage: perf-gate plant --binary <exe> --source <marker.dll>");
            return 2;
        }
    };
    let exe = match fs::canonicalize(&binary) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let source = match fs::canonicalize(&source) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let bytes = match fs::read(&source) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let hash = format!("{:x}", Sha256::digest(&bytes));
    if hash != EXPECTED {
        eprintln!("::error::marker DLL hash mismatch: {hash}");
        return 2;
    }
    let dir = match exe.parent() {
        Some(d) => d.to_path_buf(),
        None => {
            eprintln!("::error::The executable has no directory");
            return 2;
        }
    };
    let planted = dir.join("dxcompiler.dll");
    let marker = dir.join("dxcompiler.loaded");
    if planted.file_name().and_then(|n| n.to_str()) != Some("dxcompiler.dll") {
        eprintln!("::error::Refusing to plant an unexpected name");
        return 2;
    }
    if let Err(e) = fs::copy(&source, &planted) {
        eprintln!("::error::{e}");
        return 2;
    }
    let _ = fs::remove_file(&marker);

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let run_root = std::env::temp_dir().join(format!("bayan-plant-{nanos:x}"));
    let result = (|| -> Result<(), String> {
        fs::create_dir_all(&run_root).map_err(|e| e.to_string())?;
        let log = run_root.join("bayan_profile.log");
        let stderr_path = run_root.join("stderr.txt");
        let stderr_file = fs::File::create(&stderr_path).map_err(|e| e.to_string())?;
        let mut child = Command::new(&exe)
            .args(["--", "cmd.exe", "/d", "/q"])
            .env("TEMP", &run_root)
            .env("TMP", &run_root)
            .env("BAYAN_PROFILE", "1")
            .env("BAYAN_SHOW_NOW", "1")
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut ready = false;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                let detail = fs::read_to_string(&stderr_path).unwrap_or_default();
                return Err(format!("Bayan exited before the renderer was ready ({status}): {detail}"));
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
            std::thread::sleep(Duration::from_millis(200));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Startup did not reach renderer ready".into());
        }
        let pid = child.id();
        let mapped = mapped_dxcompiler(pid)?;
        let _ = child.kill();
        let _ = child.wait();
        if let Some(name) = mapped {
            return Err(format!("dxcompiler.dll was mapped: {name}"));
        }
        if marker.exists() {
            return Err(format!("The planted dxcompiler.dll ran (marker {})", marker.display()));
        }
        println!("Planted dxcompiler.dll was not loaded; renderer ready.");
        Ok(())
    })();
    let _ = fs::remove_file(&planted);
    let _ = fs::remove_file(&marker);
    let _ = fs::remove_dir_all(&run_root);
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("::error::{e}");
            2
        }
    }
}
