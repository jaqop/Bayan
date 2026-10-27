//! Resolve ConPTY from System32 `kernel32.dll`.
//!
//! portable-pty 0.9's `load_conpty` called `LoadLibraryW` on the bare name
//! `conpty.dll` (via the `shared_library` crate) and preferred that image
//! over kernel32. The search order for a bare name includes the directory
//! containing the executable, the current directory, and `PATH`, so a
//! planted `conpty.dll` runs as the user. Bayan does not ship a ConPTY
//! redistributable. The three exports have lived in kernel32 since Windows
//! 10 1809.
//!
//! This file is compiled into the vendored crate (the production call) and,
//! under `cfg(test)`, into the bayan harness so the unit test observes the
//! exact `load_library_ex` arguments.

use std::ffi::c_void;

/// `LOAD_LIBRARY_SEARCH_SYSTEM32` from winbase.h. Restricts this one load
/// to `%SystemRoot%\System32` and does not search the application directory,
/// the current directory, or `PATH`.
pub(crate) const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

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
