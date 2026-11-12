//! Resolve ConPTY from System32 `kernel32.dll`, and optionally from a
//! `conpty.dll` the owner placed next to the executable.
//!
//! portable-pty 0.9's `load_conpty` called `LoadLibraryW` on the bare name
//! `conpty.dll` (via the `shared_library` crate) and preferred that image
//! over kernel32. The search order for a bare name includes the directory
//! containing the executable, the current directory, and `PATH`, so a
//! planted `conpty.dll` runs as the user. Bayan does not ship a ConPTY
//! redistributable. The three exports have lived in kernel32 since Windows
//! 10 1809.
//!
//! A bundled pair is a different load: the absolute path
//! `<exe_dir>\conpty.dll`, and only when `<exe_dir>\OpenConsole.exe` is also
//! a file and the DLL's file version is 1.22 or newer (the ConPTY rewrite,
//! microsoft/terminal#17510). The current directory and `PATH` are not
//! searched, and a `conpty.dll` with no `OpenConsole.exe` beside it is
//! ignored. Inbox kernel32 stays the fallback, including when the bundled
//! `CreatePseudoConsole` fails.
//!
//! This file is compiled into the vendored crate (the production call) and,
//! under `cfg(test)`, into the bayan harness so the unit test observes the
//! exact `load_library_ex` arguments.

use std::ffi::c_void;

/// `LOAD_LIBRARY_SEARCH_SYSTEM32` from winbase.h. Restricts this one load
/// to `%SystemRoot%\System32` and does not search the application directory,
/// the current directory, or `PATH`.
pub(crate) const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

/// Combined with [`LOAD_LIBRARY_SEARCH_SYSTEM32`] when the file name is an
/// absolute path. Adds the directory that contains that DLL and nothing
/// else: not the current directory, not `PATH`.
pub(crate) const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;

/// Flags for an absolute-path load of a bundled `conpty.dll`.
pub(crate) const BUNDLED_LOAD_FLAGS: u32 =
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32;

/// `CreatePseudoConsole` bits. `0x1` is the only one Learn documents
/// (`PSEUDOCONSOLE_INHERIT_CURSOR`). `0x2` and `0x4` are the resize quirk
/// and Win32 input mode that inbox conhost still honours. On a 1.22+
/// `conpty.dll` those two are ignored.
///
/// `0x8` was `PSEUDOCONSOLE_PASSTHROUGH_MODE` (`--passthrough`) on
/// OpenConsole before microsoft/terminal#16954. The rewrite (#17510) passes
/// VT through by default and reused the bit as
/// `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` (`src/inc/conpty-static.h`). Do not
/// set it on that host.
pub(crate) const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;
pub(crate) const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
pub(crate) const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;
pub(crate) const PSEUDOCONSOLE_PASSTHROUGH_MODE: u32 = 0x8;

/// Inherit-cursor, resize quirk, and Win32 input. Not bit `0x8`.
pub(crate) const CONPTY_BASE_FLAGS: u32 =
    PSEUDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;

/// First Windows Terminal minor that ships the #17510 rewrite. File
/// versions are `1.<minor>.<build>.<revision>` (1.22.x, 1.24.2604.02001).
pub(crate) const BUNDLED_REWRITE_MINOR: u16 = 22;

const KERNEL32: &str = "kernel32.dll";

/// What the resolver asks a loader to open, and the symbols it then binds.
pub(crate) struct ResolvedConpty<M, S> {
    pub module: M,
    pub create_pseudo_console: S,
    pub resize_pseudo_console: S,
    pub close_pseudo_console: S,
}

/// One `LoadLibraryExW`-shaped open plus `GetProcAddress`. There is no
/// bare `LoadLibraryW` method on purpose: a caller cannot ask for a search
/// of the current directory through this trait.
pub(crate) trait ConptyLoader {
    type Module: Copy;
    type Symbol;

    fn load_library_ex(&self, file_name: &str, flags: u32) -> Result<Self::Module, String>;
    fn get_proc_address(&self, module: Self::Module, name: &str) -> Result<Self::Symbol, String>;
}

/// Bind `CreatePseudoConsole`, `ResizePseudoConsole`, and `ClosePseudoConsole`.
///
/// The only library open is `LoadLibraryExW(L"kernel32.dll", NULL,
/// LOAD_LIBRARY_SEARCH_SYSTEM32)`. `conpty.dll` is never requested.
pub(crate) fn resolve_conpty<L: ConptyLoader>(
    loader: &L,
) -> Result<ResolvedConpty<L::Module, L::Symbol>, String> {
    let module = loader.load_library_ex(KERNEL32, LOAD_LIBRARY_SEARCH_SYSTEM32)?;
    let create_pseudo_console = loader.get_proc_address(module, "CreatePseudoConsole")?;
    let resize_pseudo_console = loader.get_proc_address(module, "ResizePseudoConsole")?;
    let close_pseudo_console = loader.get_proc_address(module, "ClosePseudoConsole")?;
    Ok(ResolvedConpty {
        module,
        create_pseudo_console,
        resize_pseudo_console,
        close_pseudo_console,
    })
}

/// Which host `CreatePseudoConsole` will talk to, decided before the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConptyCapability {
    /// System32 `kernel32`. Inbox conhost through Windows 11 25H2 strips
    /// DCS and APC. Bit `0x8` is not requested: on build 26100 it is a
    /// measured no-op (microsoft/terminal#7019), so a successful call would
    /// not mean Sixel arrives.
    Inbox,
    /// Pre-rewrite OpenConsole that still maps bit `0x8` to `--passthrough`.
    /// The probe does not select this. A 1.22+ DLL reused the bit, and the
    /// older experimental mode was removed because it broke PowerShell
    /// (microsoft/terminal#16954). The arm exists so the flag is requested
    /// only for a host that still implements it.
    LegacyPassthrough,
    /// `conpty.dll` + `OpenConsole.exe` from Windows Terminal 1.22+ (#17510).
    /// VT, including DCS and APC, is passed through without bit `0x8`.
    BundledRewrite,
}

/// `CreatePseudoConsole` flags, and whether DCS/APC actually reach the
/// terminal. `passthrough` is what primary DA may advertise as Sixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConptyCreate {
    pub flags: u32,
    pub passthrough: bool,
}

/// Flags for one capability. Inbox and the 1.22+ bundle both use
/// [`CONPTY_BASE_FLAGS`]. Only [`ConptyCapability::LegacyPassthrough`] sets
/// bit `0x8`, and only that host and the bundle report passthrough.
pub(crate) fn create_flags(capability: ConptyCapability) -> ConptyCreate {
    match capability {
        ConptyCapability::Inbox => ConptyCreate {
            flags: CONPTY_BASE_FLAGS,
            passthrough: false,
        },
        ConptyCapability::LegacyPassthrough => ConptyCreate {
            flags: CONPTY_BASE_FLAGS | PSEUDOCONSOLE_PASSTHROUGH_MODE,
            passthrough: true,
        },
        ConptyCapability::BundledRewrite => ConptyCreate {
            flags: CONPTY_BASE_FLAGS,
            passthrough: true,
        },
    }
}

/// What a directory listing and a version resource said about a possible
/// bundled pair. The Win32 calls that fill this stay in the production
/// loader; tests build it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BundleProbe {
    pub dll_is_file: bool,
    pub open_console_is_file: bool,
    /// `None` when the image has no `VS_FIXEDFILEINFO`.
    pub version: Option<(u16, u16, u16, u16)>,
}

/// Inbox unless both files are present and the DLL version is 1.22 or
/// newer. An unversioned image, a DLL alone, and a pre-rewrite pair are
/// not loaded: passthrough would be a guess, and DA1 would advertise
/// Sixel that conhost then strips.
pub(crate) fn capability_from_probe(probe: BundleProbe) -> ConptyCapability {
    if !probe.dll_is_file || !probe.open_console_is_file {
        return ConptyCapability::Inbox;
    }
    match probe.version {
        Some((1, minor, _, _)) if minor >= BUNDLED_REWRITE_MINOR => {
            ConptyCapability::BundledRewrite
        }
        _ => ConptyCapability::Inbox,
    }
}

/// `VS_FIXEDFILEINFO` at the start of the version block. `dwSignature` is
/// `0xFEEF04BD`; file version is the high and low words of
/// `dwFileVersionMS` then `dwFileVersionLS`.
pub(crate) fn file_version_from_fixed(info: &[u8]) -> Option<(u16, u16, u16, u16)> {
    if info.len() < 16 {
        return None;
    }
    let sig = u32::from_le_bytes([info[0], info[1], info[2], info[3]]);
    if sig != 0xFEEF_04BD {
        return None;
    }
    let ms = u32::from_le_bytes([info[8], info[9], info[10], info[11]]);
    let ls = u32::from_le_bytes([info[12], info[13], info[14], info[15]]);
    Some((high_word(ms), low_word(ms), high_word(ls), low_word(ls)))
}

fn low_word(v: u32) -> u16 {
    (v & 0xffff) as u16
}

fn high_word(v: u32) -> u16 {
    (v >> 16) as u16
}

/// File version of `path`, or `None` when the resource is missing.
/// Does not map the image as a module.
pub(crate) fn read_file_version(path: &str) -> Option<(u16, u16, u16, u16)> {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let mut handle = 0u32;
    // SAFETY: `wide` is NUL-terminated and outlives the call. The handle
    // out-parameter is unused by the API and is a live local.
    let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle) };
    // A missing resource is zero. A huge reported size is not a version
    // block we are willing to allocate.
    if size == 0 || size > 1_048_576 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: `block` is `size` bytes, the length we just asked for, and
    // `wide` is still NUL-terminated. `dwHandle` is documented as ignored.
    let ok = unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, block.as_mut_ptr()) };
    if ok == 0 {
        return None;
    }
    let mut ptr = std::ptr::null_mut();
    let mut len = 0u32;
    // The root sub-block is a single backslash. VerQueryValueW writes a
    // pointer into `block` and the byte count of the fixed info.
    let root = [u16::from(b'\\'), 0];
    // SAFETY: `block` stays alive for the copy below. `root` is
    // NUL-terminated. `ptr` and `len` are live locals the API fills.
    let found = unsafe { VerQueryValueW(block.as_ptr(), root.as_ptr(), &mut ptr, &mut len) };
    if found == 0 || ptr.is_null() || len < 16 {
        return None;
    }
    let n = std::cmp::min(len as usize, 64);
    let mut fixed = [0u8; 64];
    // SAFETY: `ptr` points at `len` bytes inside `block` for this call,
    // and `n` is at most `len` and at most `fixed`'s length.
    unsafe {
        std::ptr::copy_nonoverlapping(ptr, fixed.as_mut_ptr(), n);
    }
    file_version_from_fixed(&fixed[..n])
}

#[link(name = "version")]
extern "system" {
    fn GetFileVersionInfoSizeW(file_name: *const u16, handle: *mut u32) -> u32;
    fn GetFileVersionInfoW(file_name: *const u16, handle: u32, len: u32, data: *mut u8) -> i32;
    fn VerQueryValueW(
        block: *const u8,
        sub_block: *const u16,
        buffer: *mut *mut u8,
        len: *mut u32,
    ) -> i32;
}

/// Win32 `LoadLibraryExW` / `GetProcAddress`. Unused in the bayan test
/// harness (that copy of this file is only here so the recording test can
/// call [`resolve_conpty`]); portable-pty's copy is the production caller.
#[allow(dead_code)]
mod system_loader {
    use super::{c_void, ConptyLoader};

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(lib_file_name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
    }

    pub(crate) struct SystemLoader;

    impl ConptyLoader for SystemLoader {
        type Module = *mut c_void;
        type Symbol = *mut c_void;

        fn load_library_ex(&self, file_name: &str, flags: u32) -> Result<Self::Module, String> {
            let mut wide: Vec<u16> = file_name.encode_utf16().collect();
            wide.push(0);
            // `hFile` is reserved and must be NULL. The flag, not a path
            // prefix, is what pins the lookup to System32.
            let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), flags) };
            if module.is_null() {
                Err(format!(
                    "LoadLibraryExW({file_name}) failed: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(module)
            }
        }

        fn get_proc_address(
            &self,
            module: Self::Module,
            name: &str,
        ) -> Result<Self::Symbol, String> {
            let mut bytes = Vec::with_capacity(name.len() + 1);
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            let sym = unsafe { GetProcAddress(module, bytes.as_ptr()) };
            if sym.is_null() {
                Err(format!(
                    "GetProcAddress({name}) failed: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(sym)
            }
        }
    }
}

#[allow(dead_code, unused_imports)]
pub(crate) use system_loader::SystemLoader;
