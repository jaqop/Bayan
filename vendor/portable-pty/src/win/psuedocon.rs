use super::conpty_resolve::{self, ConptyLoader, SystemLoader};
use super::WinChild;
use crate::cmdbuilder::CommandBuilder;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::{bail, ensure, Error};
use filedescriptor::{FileDescriptor, OwnedHandle};
use lazy_static::lazy_static;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;

pub type HPCON = HANDLE;

pub const PSUEDOCONSOLE_INHERIT_CURSOR: DWORD = conpty_resolve::PSEUDOCONSOLE_INHERIT_CURSOR;
pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = conpty_resolve::PSEUDOCONSOLE_RESIZE_QUIRK;
pub const PSEUDOCONSOLE_WIN32_INPUT_MODE: DWORD = conpty_resolve::PSEUDOCONSOLE_WIN32_INPUT_MODE;
/// Historical `--passthrough` bit. Not passed to a 1.22+ conpty.dll: there
/// it means glyph width. See [`conpty_resolve::create_flags`].
#[allow(dead_code)]
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = conpty_resolve::PSEUDOCONSOLE_PASSTHROUGH_MODE;

// The upstream struct came from the `shared_library!` macro, whose `open`
// is `LoadLibraryW` of whatever path it is given — including the bare name
// `conpty.dll`. The field names stay so the call sites below are unchanged.
// `_module` holds the kernel32 `HMODULE` so the load is not `FreeLibrary`'d.
#[allow(non_snake_case)]
struct ConPtyFuncs {
    CreatePseudoConsole:
        unsafe extern "system" fn(COORD, HANDLE, HANDLE, DWORD, *mut HPCON) -> HRESULT,
    ResizePseudoConsole: unsafe extern "system" fn(HPCON, COORD) -> HRESULT,
    ClosePseudoConsole: unsafe extern "system" fn(HPCON),
    _module: usize,
    flags: u32,
    passthrough: bool,
}

fn bind_api(
    resolved: conpty_resolve::ResolvedConpty<*mut std::ffi::c_void, *mut std::ffi::c_void>,
    flags: u32,
    passthrough: bool,
) -> ConPtyFuncs {
    // SAFETY: the three pointers came from GetProcAddress on a module this
    // process keeps mapped. Each symbol has the stdcall/C signature of the
    // ConPTY export it was looked up by.
    unsafe {
        ConPtyFuncs {
            CreatePseudoConsole: mem::transmute(resolved.create_pseudo_console),
            ResizePseudoConsole: mem::transmute(resolved.resize_pseudo_console),
            ClosePseudoConsole: mem::transmute(resolved.close_pseudo_console),
            _module: resolved.module as usize,
            flags,
            passthrough,
        }
    }
}

fn load_kernel32() -> ConPtyFuncs {
    let resolved = conpty_resolve::resolve_conpty(&SystemLoader).unwrap_or_else(|err| {
        panic!(
            "this system does not support conpty. Windows 10 October 2018 or newer is required: {}",
            err
        )
    });
    let plan = conpty_resolve::create_flags(conpty_resolve::ConptyCapability::Inbox);
    bind_api(resolved, plan.flags, plan.passthrough)
}

/// Absolute `<exe>\conpty.dll` when `<exe>\OpenConsole.exe` is that same
/// directory, both files verify, and an elevated token is not about to run
/// a user-writable copy. Anything else is `None`: kernel32 is the host.
/// A bare name, PATH, and the current directory are never consulted.
fn load_bundled() -> Option<ConPtyFuncs> {
    let exe = std::env::current_exe().ok()?;
    let dll = conpty_resolve::bundled_host_path(&exe, "conpty.dll")?;
    let console = conpty_resolve::bundled_host_path(&exe, "OpenConsole.exe")?;
    let dll_str = dll.to_str()?;
    let console_str = console.to_str()?;
    let elevated = conpty_resolve::running_elevated();
    if !conpty_resolve::host_file_ok(console_str, elevated)
        || !conpty_resolve::host_file_ok(dll_str, elevated)
    {
        log::debug!("conpty: bundled OpenConsole was not accepted; using kernel32");
        return None;
    }
    if elevated {
        let dir = exe.parent()?.to_str()?;
        if conpty_resolve::path_user_writable(dir).unwrap_or(true) {
            log::debug!("conpty: exe directory is user-writable; using kernel32");
            return None;
        }
    }
    let probe = conpty_resolve::BundleProbe {
        dll_is_file: true,
        open_console_is_file: true,
        version: conpty_resolve::read_file_version(dll_str),
    };
    let capability = conpty_resolve::capability_from_probe(probe);
    if capability != conpty_resolve::ConptyCapability::BundledRewrite {
        return None;
    }
    // Full path plus DLL_LOAD_DIR | SYSTEM32. Not a bare name, so the
    // current directory and PATH are not searched for this load.
    let module = SystemLoader
        .load_library_ex(dll_str, conpty_resolve::BUNDLED_LOAD_FLAGS)
        .ok()?;
    let create = SystemLoader.get_proc_address(module, "CreatePseudoConsole");
    let resize = SystemLoader.get_proc_address(module, "ResizePseudoConsole");
    let close = SystemLoader.get_proc_address(module, "ClosePseudoConsole");
    let (Ok(create), Ok(resize), Ok(close)) = (create, resize, close) else {
        // SAFETY: `module` is the handle LoadLibraryExW just returned, and
        // we are not going to call through it.
        unsafe { FreeLibrary(module) };
        return None;
    };
    let plan = conpty_resolve::create_flags(capability);
    log::info!(
        "conpty: using bundled {} (version {:?}), passthrough {}",
        dll_str,
        probe.version,
        plan.passthrough
    );
    Some(bind_api(
        conpty_resolve::ResolvedConpty {
            module,
            create_pseudo_console: create,
            resize_pseudo_console: resize,
            close_pseudo_console: close,
        },
        plan.flags,
        plan.passthrough,
    ))
}

struct ConPtyHosts {
    bundle: Option<ConPtyFuncs>,
    kernel: ConPtyFuncs,
}

fn load_hosts() -> ConPtyHosts {
    ConPtyHosts {
        bundle: load_bundled(),
        kernel: load_kernel32(),
    }
}

lazy_static! {
    static ref CONPTY: ConPtyHosts = load_hosts();
}

/// Set once a bundled 1.22+ pseudoconsole has been created. Inbox sessions
/// leave it false, which is what primary DA reads before advertising Sixel.
static PASSTHROUGH_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn passthrough_active() -> bool {
    PASSTHROUGH_ACTIVE.load(Ordering::Acquire)
}

#[link(name = "kernel32")]
extern "system" {
    fn FreeLibrary(module: *mut std::ffi::c_void) -> i32;
}

pub struct PsuedoCon {
    con: HPCON,
    resize: unsafe extern "system" fn(HPCON, COORD) -> HRESULT,
    close: unsafe extern "system" fn(HPCON),
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        // SAFETY: `con` was returned by this host's CreatePseudoConsole, and
        // `close` is that host's ClosePseudoConsole.
        unsafe { (self.close)(self.con) };
    }
}

fn open_with(
    api: &ConPtyFuncs,
    size: COORD,
    input: &FileDescriptor,
    output: &FileDescriptor,
) -> Result<PsuedoCon, HRESULT> {
    let mut con: HPCON = INVALID_HANDLE_VALUE;
    // SAFETY: the handles are the read end of the input pipe and the write
    // end of the output pipe, both live for this call. `api.flags` is the
    // plan for this host. On failure `con` is left untouched and not closed.
    let result = unsafe {
        (api.CreatePseudoConsole)(
            size,
            input.as_raw_handle() as _,
            output.as_raw_handle() as _,
            api.flags,
            &mut con,
        )
    };
    if result != S_OK {
        return Err(result);
    }
    if api.passthrough {
        PASSTHROUGH_ACTIVE.store(true, Ordering::Release);
    }
    Ok(PsuedoCon {
        con,
        resize: api.ResizePseudoConsole,
        close: api.ClosePseudoConsole,
    })
}

impl PsuedoCon {
    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let hosts = &*CONPTY;
        debug_assert_eq!(
            hosts.kernel.flags,
            PSUEDOCONSOLE_INHERIT_CURSOR
                | PSEUDOCONSOLE_RESIZE_QUIRK
                | PSEUDOCONSOLE_WIN32_INPUT_MODE
        );
        if let Some(bundle) = &hosts.bundle {
            match open_with(bundle, size, &input, &output) {
                Ok(con) => return Ok(con),
                Err(hr) => {
                    // A host that did not start is inbox. No dialog, no panic.
                    let fallback = conpty_resolve::capability_after_launch(true, false);
                    log::debug!(
                        "bundled CreatePseudoConsole failed (HRESULT {hr:#x}); {fallback:?}; using kernel32"
                    );
                }
            }
        }
        match open_with(&hosts.kernel, size, &input, &output) {
            Ok(con) => Ok(con),
            Err(hr) => {
                bail!("failed to create psuedo console: HRESULT {}", hr)
            }
        }
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        // SAFETY: `con` is this host's pseudoconsole and `resize` is its
        // ResizePseudoConsole.
        let result = unsafe { (self.resize)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        // Explicitly set the stdio handles as invalid handles otherwise
        // we can end up with a weird state where the spawned process can
        // inherit the explicitly redirected output handles from its parent.
        // For example, when daemonizing wezterm-mux-server, the stdio handles
        // are redirected to a log file and the spawned process would end up
        // writing its output there instead of to the pty we just created.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut attrs = ProcThreadAttributeList::with_capacity(1)?;
        attrs.set_pty(self.con)?;
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = cmd.cmdline()?;
        let cmd_os = OsString::from_wide(&cmdline);

        let cwd = cmd.current_directory();

        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
                cwd.as_ref()
                    .map(|c| c.as_slice().as_ptr())
                    .unwrap_or(ptr::null()),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            let msg = format!(
                "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
                cmd_os,
                cwd.as_ref().map(|c| OsString::from_wide(c)),
                err
            );
            log::error!("{}", msg);
            bail!("{}", msg);
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        Ok(WinChild {
            proc: Mutex::new(proc),
        })
    }
}
