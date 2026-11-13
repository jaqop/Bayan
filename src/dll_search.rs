//! Which graphics DLLs this process may load, and from where.
//!
//! Windows searches the directory that contains `bayan.exe` before
//! System32. A `dxcompiler.dll`, `d3dcompiler_47.dll`, `opengl32.dll`, or
//! `vulkan-1.dll` planted beside the executable (Downloads, a writable
//! `target\release`) would otherwise run as the user. wgpu's default
//! `Dx12Compiler::Auto` is that search: it loads the bare name
//! `dxcompiler.dll` and only then falls back to FXC.
//!
//! Decision:
//!
//! - Shaders use FXC (`d3dcompiler_47.dll`). DXC is not loaded from the
//!   application directory or from System32. Bayan does not ship
//!   `dxcompiler.dll` or `dxil.dll`. When they are absent, which is the
//!   normal install, FXC still compiles the WGSL pipeline and the window
//!   renders. Asking wgpu for dynamic DXC would not fall back if an old
//!   DLL loaded but then refused to compile.
//! - The Windows build does not enable wgpu's `vulkan` feature, so
//!   `vulkan-1.dll` is not linked. Non-Windows targets enable that feature
//!   for a later Linux port; this process still does not request Vulkan.
//!   OpenGL stays compiled, as the fallback when DX12 cannot make a device.
//!   An ICD path that cannot be read or expanded is skipped. DirectX 12
//!   does not need it, so a bad registry value is not a black window.
//! - `SetDllDirectoryW("")` drops the current directory.
//!   `SetDefaultDllDirectories` is called with
//!   `LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_USER_DIRS`, not
//!   with `LOAD_LIBRARY_SEARCH_SYSTEM32` alone. The flag that only names
//!   System32 also hides `System32\D3D12` (the inbox loader reads
//!   `D3D12Core.dll` from there) and the OpenGL ICD's sibling DLLs under
//!   `System32\DriverStore\FileRepository`. A missed dependency is a black
//!   window, and a WARP device in CI never loads a vendor ICD, so that
//!   probe cannot prove the narrower flag. `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR`
//!   cannot be set for the whole process. The only directories added
//!   afterwards are `System32\D3D12` and an ICD folder
//!   [`icd_dependency_directory`] accepts. Those user directories are
//!   searched before System32, so the inbox graphics DLLs are mapped with
//!   `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)` before the extra
//!   directories are added. A later bare-name load returns the module
//!   already in memory.
//! - `opengl32.dll` (and `dxgi.dll`, when wgpu imports it) are raw-dylib
//!   imports, so the loader would resolve them before `main`. The link
//!   delay-loads them, so this function still runs first. A copy beside
//!   the executable is not on the search path by then.
//!
//! The optional ConPTY pair is a different load. `conpty.dll` is opened
//! only after its hash and Microsoft signature match, and only by that
//! full path. It is not a graphics DLL and it is not on this allowlist.
//! If that pair is missing or rejected, the inbox `kernel32` console is
//! used and the window still runs.

use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Once;

/// Graphics DLLs wgpu loads by bare name on the DX12 and GL paths.
/// `dxcompiler.dll`, `dxil.dll`, `vulkan-1.dll`, and `libEGL.dll` are
/// absent on purpose: this process does not load them.
pub const SYSTEM_GRAPHICS_DLLS: &[&str] = &[
    "d3d12.dll",
    "dxgi.dll",
    "dcomp.dll",
    "d3dcompiler_47.dll",
    "opengl32.dll",
];

/// Shader compiler wgpu is asked for. There is no DXC variant: a path
/// under the executable, or even a System32 copy, is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dx12ShaderCompiler {
    /// Inbox `d3dcompiler_47.dll`, already mapped from System32.
    Fxc,
}

/// Always FXC. See the module note for why dynamic DXC is not selected
/// even when `dxcompiler.dll` exists.
pub fn dx12_shader_compiler() -> Dx12ShaderCompiler {
    Dx12ShaderCompiler::Fxc
}

/// `system_dir` plus one allowlisted file name, or `None` when that is
/// not a graphics DLL this process is willing to map from System32.
pub fn system32_graphics_dll(system_dir: &Path, name: &str) -> Option<PathBuf> {
    if normalize_disk(system_dir)
        .as_deref()
        .is_none_or(|n| !is_windows_system(n))
    {
        return None;
    }
    let canonical = SYSTEM_GRAPHICS_DLLS
        .iter()
        .copied()
        .find(|allowed| allowed.eq_ignore_ascii_case(name))?;
    if !bare_dll_name(canonical) {
        return None;
    }
    let joined = system_dir.join(canonical);
    // One extra component, still the same system directory.
    if joined.parent() != Some(system_dir)
        || joined.file_name().and_then(|s| s.to_str()) != Some(canonical)
    {
        return None;
    }
    Some(joined)
}

/// `System32\D3D12`, where the inbox DirectX 12 loader keeps `D3D12Core.dll`.
pub fn d3d12_core_directory(system_dir: &Path) -> Option<PathBuf> {
    let norm = normalize_disk(system_dir)?;
    if !is_windows_system(&norm) {
        return None;
    }
    let joined = system_dir.join("D3D12");
    let child = normalize_disk(&joined)?;
    if child != format!(r"{norm}\d3d12") {
        return None;
    }
    Some(joined)
}

/// Parent directory of an OpenGL ICD, when that DLL sits in a driver-store
/// package under `system_dir` and nowhere else.
pub fn icd_dependency_directory(system_dir: &Path, icd_dll: &Path) -> Option<PathBuf> {
    let system = normalize_disk(system_dir)?;
    if !is_windows_system(&system) {
        return None;
    }
    let dll = normalize_disk(icd_dll)?;
    let parent = icd_dll.parent()?;
    let parent_norm = normalize_disk(parent)?;
    let package_root = format!(r"{system}\driverstore\filerepository\");
    // A package directory under FileRepository, not FileRepository itself
    // and not a path that merely shares a prefix (`FileRepository-evil`).
    if !parent_norm.starts_with(&package_root) || parent_norm == system {
        return None;
    }
    if dll != format!(r"{parent_norm}\{}", file_name_lower(icd_dll)?) {
        return None;
    }
    Some(parent.to_path_buf())
}

/// Directories `AddDllDirectory` may receive: D3D12's core folder and
/// trusted ICD folders, minus the executable's own directory.
pub fn search_directories<'a>(
    system_dir: &Path,
    icd_dlls: impl IntoIterator<Item = &'a Path>,
    exe_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let exe = exe_dir.and_then(normalize_disk);
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let mut push = |dir: PathBuf| {
        if out.len() >= 8 {
            return;
        }
        let Some(norm) = normalize_disk(&dir) else {
            return;
        };
        if exe.as_deref() == Some(norm.as_str()) || seen.iter().any(|s| s == &norm) {
            return;
        }
        seen.push(norm);
        out.push(dir);
    };
    if let Some(dir) = d3d12_core_directory(system_dir) {
        push(dir);
    }
    for icd in icd_dlls {
        if let Some(dir) = icd_dependency_directory(system_dir, icd) {
            push(dir);
        }
    }
    out
}

/// Drop the current directory and the application directory from DLL
/// search, then map the inbox graphics DLLs from System32.
pub(crate) fn harden_dll_search() {
    static ONCE: Once = Once::new();
    ONCE.call_once(harden_dll_search_once);
}

fn harden_dll_search_once() {
    use windows_sys::Win32::System::LibraryLoader::{
        AddDllDirectory, SetDefaultDllDirectories, SetDllDirectoryW, LOAD_LIBRARY_SEARCH_SYSTEM32,
        LOAD_LIBRARY_SEARCH_USER_DIRS,
    };

    // An empty string removes the current directory. NULL would put it back.
    let empty = [0u16];
    // SAFETY: `empty` is a NUL-terminated UTF-16 string that outlives the
    // call, and SetDllDirectoryW copies it rather than keeping the pointer.
    unsafe {
        let _ = SetDllDirectoryW(empty.as_ptr());
    }

    // SAFETY: the flags are the documented pair. Failure leaves the
    // standard search in place; the explicit System32 loads below still run.
    let restricted = unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_USER_DIRS) != 0
    };
    if !restricted {
        eprintln!("bayan: could not restrict the DLL search path");
    }

    let Some(system_dir) = system_directory() else {
        eprintln!("bayan: GetSystemDirectoryW failed; graphics DLLs were not pinned");
        return;
    };
    // Map System32 copies before any user directory is added. User
    // directories are searched first, so this order is what makes a
    // same-named file in a driver folder lose.
    for name in SYSTEM_GRAPHICS_DLLS {
        // The path check is the allowlist. The load itself is that bare
        // name plus LOAD_LIBRARY_SEARCH_SYSTEM32: an absolute path combined
        // with that flag is rejected on some Windows builds, and the flag
        // is what keeps the application directory out of this search.
        if system32_graphics_dll(&system_dir, name).is_some() {
            load_system32_name(name);
        }
    }
    if !restricted {
        return;
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let icds = registry_icd_dlls();
    let mut noted_missing_dir = false;
    for dir in search_directories(
        &system_dir,
        icds.iter().map(PathBuf::as_path),
        exe_dir.as_deref(),
    ) {
        let wide = wide_path(&dir);
        // SAFETY: `wide` is NUL-terminated and outlives the call. A null
        // cookie means the directory was not added; the System32 modules
        // already mapped stay mapped. The cookie is intentionally not
        // passed to RemoveDllDirectory.
        let cookie = unsafe { AddDllDirectory(wide.as_ptr()) };
        if cookie.is_null() && !noted_missing_dir {
            noted_missing_dir = true;
            eprintln!("bayan: a graphics dependency directory was not added");
        }
    }
}

fn load_system32_name(name: &str) {
    use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    let wide = wide(name);
    // SAFETY: `wide` is a NUL-terminated allowlisted file name, not a path,
    // and it outlives the call. The file handle must be null. The flag
    // searches System32 only. The module is left loaded so a later
    // bare-name load returns this copy. The handle is not freed.
    unsafe {
        let _ = LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
    }
}

fn system_directory() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the length passed is `buf`'s own, so the write stays inside
    // it. Zero, or a length that does not fit, is "no system directory".
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize]));
    system32_graphics_dll(&path, "d3d12.dll").map(|_| path)
}

fn registry_icd_dlls() -> Vec<PathBuf> {
    let mut out = Vec::new();
    khronos_icd_names(&mut out);
    display_icd_names(&mut out);
    out.truncate(32);
    out
}

fn khronos_icd_names(out: &mut Vec<PathBuf>) {
    use windows_sys::Win32::System::Registry::{RegEnumValueW, HKEY_LOCAL_MACHINE, REG_DWORD};
    let Some(key) = open_key(HKEY_LOCAL_MACHINE, r"SOFTWARE\Khronos\OpenGL\Drivers") else {
        return;
    };
    let mut index = 0u32;
    while out.len() < 32 {
        let mut name = [0u16; 1024];
        let mut name_chars = name.len() as u32;
        let mut kind = 0u32;
        // SAFETY: `key` is open. `name` is the buffer whose character count
        // is passed in `name_chars`. The data pointer is null because only
        // the value name (the ICD path) is read, and only when the type is
        // a DWORD, which is what the Khronos key stores.
        let err = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut name_chars,
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if err == 259 {
            break; // ERROR_NO_MORE_ITEMS
        }
        index = index.saturating_add(1);
        if err != 0 || kind != REG_DWORD || name_chars as usize >= name.len() {
            continue;
        }
        if let Some(path) = path_from_wide(&name[..name_chars as usize]) {
            out.push(path);
        }
    }
}

fn display_icd_names(out: &mut Vec<PathBuf>) {
    use windows_sys::Win32::System::Registry::{RegEnumKeyExW, HKEY_LOCAL_MACHINE};
    let Some(class) = open_key(
        HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
    ) else {
        return;
    };
    for index in 0..32u32 {
        if out.len() >= 32 {
            break;
        }
        let mut sub = [0u16; 64];
        let mut sub_chars = sub.len() as u32;
        // SAFETY: `class` is open. `sub` is the name buffer and `sub_chars`
        // its capacity in UTF-16 code units. The class and file-time
        // outputs are unused.
        let err = unsafe {
            RegEnumKeyExW(
                class.0,
                index,
                sub.as_mut_ptr(),
                &mut sub_chars,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if err == 259 {
            break;
        }
        if err != 0 || sub_chars as usize >= sub.len() {
            continue;
        }
        let Some(sub_name) = wide_to_string(&sub[..sub_chars as usize]) else {
            continue;
        };
        let Some(adapter) = open_key(class.0, &sub_name) else {
            continue;
        };
        let Some((kind_raw, units)) = registry_string(&adapter, "OpenGLDriverName") else {
            continue;
        };
        // A missing value, an unknown type, or an expansion that fails is
        // skipped. DirectX 12 still starts; this read must not abort it.
        let Some(kind) = icd_kind(kind_raw) else {
            continue;
        };
        if let Some(path) = icd_path_from_registry(kind, &units, expand_environment_string) {
            out.push(path);
        }
    }
}

fn icd_kind(raw: u32) -> Option<IcdRegKind> {
    use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_MULTI_SZ, REG_SZ};
    match raw {
        REG_SZ => Some(IcdRegKind::Sz),
        REG_EXPAND_SZ => Some(IcdRegKind::ExpandSz),
        REG_MULTI_SZ => Some(IcdRegKind::MultiSz),
        _ => None,
    }
}

fn registry_string(key: &RegKey, name: &str) -> Option<(u32, Vec<u16>)> {
    use windows_sys::Win32::System::Registry::RegQueryValueExW;
    let name = wide(name);
    let mut bytes = 8192u32;
    for _ in 0..4 {
        let mut data = vec![0u8; bytes as usize];
        let mut kind = 0u32;
        let mut size = bytes;
        // SAFETY: `key` is open, `name` is NUL-terminated, and `data` is
        // the buffer `size` describes. The call does not retain them.
        // ERROR_MORE_DATA (234) reports the size to retry with.
        let queried = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut size,
            )
        };
        if queried == 234 && size > bytes && size <= 1_048_576 {
            bytes = size;
            continue;
        }
        if queried != 0 || size == 0 || size as usize > data.len() || !size.is_multiple_of(2) {
            return None;
        }
        let (pairs, _) = data[..size as usize].as_chunks::<2>();
        let units = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        return Some((kind, units));
    }
    None
}

/// `ExpandEnvironmentStringsW`. `None` if the API fails, so the caller
/// skips this ICD instead of failing startup.
fn expand_environment_string(value: &str) -> Option<String> {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let src = wide(value);
    let mut cap = 1024u32;
    for _ in 0..4 {
        let mut buf = vec![0u16; cap as usize];
        // SAFETY: `src` is NUL-terminated and outlives the call. `buf` has
        // `cap` code units. A return of 0 is failure. A return above `cap`
        // is the required size, including the NUL, and the buffer is not read.
        let written = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), cap) };
        if written == 0 {
            return None;
        }
        if written <= cap {
            let end = (written as usize).saturating_sub(1);
            return String::from_utf16(&buf[..end.min(buf.len())]).ok();
        }
        cap = written;
    }
    None
}

struct RegKey(windows_sys::Win32::System::Registry::HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle was opened by `open_key` and is closed once.
            unsafe {
                windows_sys::Win32::System::Registry::RegCloseKey(self.0);
            }
        }
    }
}

fn open_key(parent: windows_sys::Win32::System::Registry::HKEY, subkey: &str) -> Option<RegKey> {
    use windows_sys::Win32::System::Registry::{RegOpenKeyExW, KEY_READ, KEY_WOW64_64KEY};
    let name = wide(subkey);
    let mut key = std::ptr::null_mut();
    // SAFETY: `parent` is a predefined key or one `open_key` returned.
    // `name` is NUL-terminated and outlives the call. On success `key` is
    // owned by `RegKey` and closed on drop.
    let err = unsafe {
        RegOpenKeyExW(
            parent,
            name.as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if err != 0 || key.is_null() {
        None
    } else {
        Some(RegKey(key))
    }
}

/// Registry string kinds that can name an OpenGL ICD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcdRegKind {
    /// REG_SZ. One string, used as written.
    Sz,
    /// REG_EXPAND_SZ. One string. The caller expands it after the read.
    ExpandSz,
    /// REG_MULTI_SZ. The first non-empty string. Later strings are ignored.
    MultiSz,
}

/// The ICD string selected from a registry value.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IcdRegString {
    path: String,
    /// Set only for [`IcdRegKind::ExpandSz`].
    expand: bool,
}

/// Select the ICD string from registry bytes. No Win32 calls: environment
/// expansion is the `expand` argument, applied only for REG_EXPAND_SZ and
/// only after the string has been read. `None` (unknown shape, empty, or
/// an expander that fails) means "skip this value".
fn icd_path_from_registry(
    kind: IcdRegKind,
    units: &[u16],
    expand: impl FnOnce(&str) -> Option<String>,
) -> Option<PathBuf> {
    let parsed = parse_icd_registry(kind, units)?;
    let text = if parsed.expand {
        let expanded = expand(parsed.path.as_str())?;
        clean_icd_text(&expanded)?
    } else {
        parsed.path
    };
    Some(PathBuf::from(text))
}

fn parse_icd_registry(kind: IcdRegKind, units: &[u16]) -> Option<IcdRegString> {
    let path = match kind {
        IcdRegKind::Sz | IcdRegKind::ExpandSz => one_reg_string(units)?,
        IcdRegKind::MultiSz => first_nonempty_multi(units)?,
    };
    Some(IcdRegString {
        expand: matches!(kind, IcdRegKind::ExpandSz),
        path,
    })
}

fn one_reg_string(units: &[u16]) -> Option<String> {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    clean_icd_text(&String::from_utf16(&units[..end]).ok()?)
}

/// NUL-separated strings. An empty entry, including a leading NUL, is not
/// a path. The list ends at the extra trailing NUL or at the buffer.
fn first_nonempty_multi(units: &[u16]) -> Option<String> {
    let mut start = 0usize;
    while start < units.len() {
        let end = start
            + units[start..]
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(units.len() - start);
        if let Ok(text) = String::from_utf16(&units[start..end]) {
            if let Some(clean) = clean_icd_text(&text) {
                return Some(clean);
            }
        }
        if end >= units.len() {
            break;
        }
        start = end + 1;
    }
    None
}

fn clean_icd_text(text: &str) -> Option<String> {
    let text = text.trim().trim_matches('"');
    if text.is_empty() || text.len() > 1024 {
        None
    } else {
        Some(text.to_string())
    }
}

fn path_from_wide(buf: &[u16]) -> Option<PathBuf> {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    let text = wide_to_string(&buf[..end])?;
    let text = text.trim().trim_matches('"');
    if text.is_empty() || text.len() > 1024 {
        return None;
    }
    Some(PathBuf::from(text))
}

fn wide_to_string(buf: &[u16]) -> Option<String> {
    String::from_utf16(buf).ok()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn bare_dll_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
        && name.matches('.').count() == 1
        && !name.starts_with('.')
        && name.ends_with(".dll")
}

fn file_name_lower(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

/// `c:\windows\system32` for a drive path with no `.` or `..`. UNC,
/// device, and verbatim-UNC paths are `None`.
fn normalize_disk(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut saw_prefix = false;
    let mut saw_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                if saw_prefix {
                    return None;
                }
                let disk = match prefix.kind() {
                    Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
                    _ => return None,
                };
                if !disk.is_ascii_alphabetic() {
                    return None;
                }
                parts.push(format!("{}:", (disk as char).to_ascii_lowercase()));
                saw_prefix = true;
            }
            Component::RootDir => {
                if !saw_prefix || saw_root {
                    return None;
                }
                saw_root = true;
            }
            Component::Normal(part) => {
                if !saw_root {
                    return None;
                }
                let text = part.to_str()?;
                if text.eq_ignore_ascii_case(".") || text.eq_ignore_ascii_case("..") {
                    return None;
                }
                parts.push(text.to_ascii_lowercase());
            }
            Component::CurDir | Component::ParentDir => return None,
        }
    }
    if !saw_root || parts.len() < 2 {
        return None;
    }
    Some(parts.join(r"\"))
}

fn is_windows_system(normalized: &str) -> bool {
    let mut pieces = normalized.split('\\');
    let drive = pieces.next().unwrap_or("");
    let windows = pieces.next().unwrap_or("");
    let leaf = pieces.next().unwrap_or("");
    let bytes = drive.as_bytes();
    pieces.next().is_none()
        && bytes.len() == 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && windows == "windows"
        && (leaf == "system32" || leaf == "syswow64")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system() -> &'static Path {
        Path::new(r"C:\Windows\System32")
    }

    /// Line comments removed, so a note that names a refused API is not a use of it.
    fn rust_code(src: &str) -> String {
        let mut out = String::new();
        let mut chars = src.chars().peekable();
        let mut in_string = false;
        let mut escape = false;
        while let Some(c) = chars.next() {
            if in_string {
                out.push(c);
                if escape {
                    escape = false;
                } else if c == '\\' {
                    escape = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            if c == '"' {
                in_string = true;
                out.push(c);
                continue;
            }
            if c == '/' && chars.peek() == Some(&'/') {
                chars.next();
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn system32_graphics_dll_accepts_only_the_allowlist() {
        let got = system32_graphics_dll(system(), "D3D12.DLL").unwrap();
        assert_eq!(got, PathBuf::from(r"C:\Windows\System32\d3d12.dll"));
        for name in [
            "dxcompiler.dll",
            "dxil.dll",
            "vulkan-1.dll",
            "libEGL.dll",
            "conpty.dll",
            r"d3d12.dll\..\evil.dll",
            r"..\d3d12.dll",
            "d3d12.dll ",
            "",
        ] {
            assert!(
                system32_graphics_dll(system(), name).is_none(),
                "{name} was accepted"
            );
        }
    }

    #[test]
    fn system_directory_shape_rejects_the_places_a_plant_lives() {
        assert!(system32_graphics_dll(Path::new(r"C:\Users\a\Downloads"), "d3d12.dll").is_none());
        assert!(
            system32_graphics_dll(Path::new(r"C:\Windows\System32\D3D12"), "d3d12.dll").is_none()
        );
        assert!(
            system32_graphics_dll(Path::new(r"\\server\share\System32"), "d3d12.dll").is_none()
        );
        assert!(
            system32_graphics_dll(Path::new(r"C:\Windows\System32\..\System32"), "d3d12.dll")
                .is_none()
        );
        assert!(system32_graphics_dll(Path::new(r"\Windows\System32"), "d3d12.dll").is_none());
        assert!(system32_graphics_dll(Path::new(r"D:\Windows\SysWOW64"), "opengl32.dll").is_some());
    }

    #[test]
    fn d3d12_core_and_icd_directories_stay_under_the_driver_store() {
        assert_eq!(
            d3d12_core_directory(system()).unwrap(),
            PathBuf::from(r"C:\Windows\System32\D3D12")
        );
        let icd =
            Path::new(r"C:\Windows\System32\DriverStore\FileRepository\nv.inf_amd64\nvoglv64.dll");
        assert_eq!(
            icd_dependency_directory(system(), icd).unwrap(),
            Path::new(r"C:\Windows\System32\DriverStore\FileRepository\nv.inf_amd64")
        );
        // The repository root itself is not one package directory.
        assert!(icd_dependency_directory(
            system(),
            Path::new(r"C:\Windows\System32\DriverStore\FileRepository\nvoglv64.dll"),
        )
        .is_none());
        assert!(icd_dependency_directory(
            system(),
            Path::new(r"C:\Users\a\Downloads\nvoglv64.dll"),
        )
        .is_none());
        assert!(icd_dependency_directory(
            system(),
            Path::new(r"C:\Windows\System32\DriverStore\FileRepository\..\evil\nvoglv64.dll"),
        )
        .is_none());
        assert!(icd_dependency_directory(system(), Path::new(r"nvoglv64.dll")).is_none());
    }

    #[test]
    fn search_directories_skip_the_executable_and_repeat_nothing() {
        let icd =
            Path::new(r"C:\Windows\System32\DriverStore\FileRepository\nv.inf_amd64\nvoglv64.dll");
        let package = PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\nv.inf_amd64");
        let dirs = search_directories(system(), [icd, icd], Some(Path::new(r"C:\Bayan")));
        assert_eq!(
            dirs,
            vec![PathBuf::from(r"C:\Windows\System32\D3D12"), package.clone(),]
        );
        // Running the exe from the D3D12 folder must not add that folder
        // a second time as "the application directory".
        let dirs = search_directories(
            system(),
            [icd],
            Some(Path::new(r"C:\Windows\System32\D3D12")),
        );
        assert_eq!(dirs, vec![package]);
        let beside = Path::new(r"C:\Bayan\dxcompiler.dll");
        assert!(search_directories(system(), [beside], None)
            .iter()
            .all(|dir| dir.ends_with("D3D12")));
    }

    fn utf16_z(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn reg_sz_is_the_string_itself() {
        let units = utf16_z(r"C:\Windows\System32\DriverStore\FileRepository\pkg\nvoglv64.dll");
        let got =
            icd_path_from_registry(IcdRegKind::Sz, &units, |_| panic!("REG_SZ is not expanded"))
                .unwrap();
        assert_eq!(
            got,
            PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\pkg\nvoglv64.dll")
        );
    }

    #[test]
    fn reg_multi_sz_uses_the_first_non_empty_string() {
        let mut units = utf16_z(r"C:\Windows\System32\DriverStore\FileRepository\a\nvoglv64.dll");
        units.extend(utf16_z(
            r"C:\Windows\System32\DriverStore\FileRepository\b\nvoglv64.dll",
        ));
        units.push(0);
        let got = icd_path_from_registry(IcdRegKind::MultiSz, &units, |_| {
            panic!("REG_MULTI_SZ is not expanded")
        })
        .unwrap();
        assert_eq!(
            got,
            PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\a\nvoglv64.dll")
        );
    }

    #[test]
    fn reg_multi_sz_skips_an_empty_first_string() {
        let mut units = vec![0u16];
        units.extend(utf16_z(
            r"C:\Windows\System32\DriverStore\FileRepository\nv\nvoglv64.dll",
        ));
        units.push(0);
        let got = icd_path_from_registry(IcdRegKind::MultiSz, &units, |_| {
            panic!("REG_MULTI_SZ is not expanded")
        })
        .unwrap();
        assert_eq!(
            got,
            PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\nv\nvoglv64.dll")
        );
    }

    #[test]
    fn reg_expand_sz_is_expanded_after_the_string_is_read() {
        let units = utf16_z(r"%SystemRoot%\System32\DriverStore\FileRepository\pkg\nvoglv64.dll");
        let got = icd_path_from_registry(IcdRegKind::ExpandSz, &units, |raw| {
            assert!(
                raw.starts_with("%SystemRoot%"),
                "expansion runs on the string that was read"
            );
            Some(raw.replace("%SystemRoot%", r"C:\Windows"))
        })
        .unwrap();
        assert_eq!(
            got,
            PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\pkg\nvoglv64.dll")
        );
        let units = utf16_z(r"%SystemRoot%\System32\foo.dll");
        assert!(
            icd_path_from_registry(IcdRegKind::ExpandSz, &units, |_| None).is_none(),
            "a failed expansion skips the value"
        );
    }

    #[test]
    fn shaders_use_fxc_even_when_a_dxc_dll_is_sitting_there() {
        assert_eq!(dx12_shader_compiler(), Dx12ShaderCompiler::Fxc);
        // Comments may name the default we refused. Only the code is checked.
        let gpu = rust_code(include_str!("gpu.rs"));
        assert!(
            gpu.contains("Dx12Compiler::Fxc"),
            "the instance must ask for FXC"
        );
        assert!(
            !gpu.contains("Dx12Compiler::Auto") && !gpu.contains("default_dynamic_dxc"),
            "Auto loads the bare name dxcompiler.dll"
        );
        let main = include_str!("main.rs");
        let start = main.find("fn main()").expect("main");
        let body = &main[start..];
        let harden = body.find("harden_dll_search()").expect("harden call");
        let dispatch = body.find("dispatch()").expect("dispatch");
        assert!(
            harden < dispatch,
            "DLL search is restricted before dispatch"
        );
    }

    #[test]
    fn a_crafted_import_table_distinguishes_delay_load_from_startup() {
        let bytes = crafted_pe(&["OPENGL32.dll", "KERNEL32.dll"], &["dxgi.dll"]);
        let imports = pe_dll_imports(&bytes).expect("crafted pe");
        assert!(imports.startup.iter().any(|n| n == "opengl32.dll"));
        assert!(imports.startup.iter().any(|n| n == "kernel32.dll"));
        assert!(imports.delay.iter().any(|n| n == "dxgi.dll"));
        assert!(!imports.startup.iter().any(|n| n == "dxgi.dll"));
        assert!(pe_dll_imports(&[]).is_none());
        assert!(pe_dll_imports(b"MZ").is_none());
    }

    #[test]
    fn this_binary_does_not_import_graphics_dlls_at_startup() {
        // Referencing the entry keeps wgpu's raw-dylib imports in this binary.
        let _anchor = crate::gpu::Gpu::new as *const ();
        let path = std::env::current_exe().expect("current exe");
        let bytes = std::fs::read(&path).expect("read exe");
        let imports = pe_dll_imports(&bytes).expect("parse exe");
        for name in [
            "opengl32.dll",
            "dxgi.dll",
            "d3d12.dll",
            "d3dcompiler_47.dll",
            "dcomp.dll",
            "dxcompiler.dll",
            "dxil.dll",
            "vulkan-1.dll",
            "libegl.dll",
        ] {
            assert!(
                !imports.startup.iter().any(|n| n == name),
                "{name} is a startup import of {path:?}; a copy beside the exe would load before main"
            );
        }
        for name in ["dxcompiler.dll", "dxil.dll", "vulkan-1.dll", "libegl.dll"] {
            assert!(
                !imports.delay.iter().any(|n| n == name),
                "{name} is delay-loaded; it would still be searched by bare name"
            );
        }
        // The MSVC link delay-loads opengl32. A gnu link does not, and this
        // crate is not shipped that way.
        #[cfg(target_env = "msvc")]
        assert!(
            imports.delay.iter().any(|n| n == "opengl32.dll"),
            "opengl32.dll was not delay-loaded (startup {:?}, delay {:?})",
            imports.startup,
            imports.delay
        );
    }

    struct Imports {
        startup: Vec<String>,
        delay: Vec<String>,
    }

    struct Section {
        va: u32,
        vsize: u32,
        raw_ptr: u32,
        raw_size: u32,
    }

    fn pe_dll_imports(bytes: &[u8]) -> Option<Imports> {
        if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
            return None;
        }
        let lfanew = u32_at(bytes, 0x3C)? as usize;
        if lfanew.checked_add(24).is_none_or(|end| end > bytes.len())
            || &bytes[lfanew..lfanew + 4] != b"PE\0\0"
        {
            return None;
        }
        let section_count = u16_at(bytes, lfanew + 6)? as usize;
        let opt_size = u16_at(bytes, lfanew + 20)? as usize;
        let opt = lfanew + 24;
        if opt
            .checked_add(opt_size)
            .is_none_or(|end| end > bytes.len())
        {
            return None;
        }
        let magic = u16_at(bytes, opt)?;
        let (dir_off, dir_count_off) = match magic {
            0x20B => (112usize, 108usize), // PE32+
            0x10B => (96usize, 92usize),   // PE32
            _ => return None,
        };
        if dir_count_off + 4 > opt_size {
            return None;
        }
        let dir_count = u32_at(bytes, opt + dir_count_off)? as usize;
        let sections = section_headers(bytes, opt + opt_size, section_count)?;
        let startup = dll_names(
            bytes,
            &sections,
            directory(bytes, opt, opt_size, dir_off, dir_count, 1)?,
            20,
            12,
        )?;
        let delay = dll_names(
            bytes,
            &sections,
            directory(bytes, opt, opt_size, dir_off, dir_count, 13)?,
            32,
            4,
        )?;
        Some(Imports { startup, delay })
    }

    fn directory(
        bytes: &[u8],
        opt: usize,
        opt_size: usize,
        dir_off: usize,
        dir_count: usize,
        index: usize,
    ) -> Option<(u32, u32)> {
        if index >= dir_count || dir_off + (index + 1) * 8 > opt_size {
            return Some((0, 0));
        }
        let at = opt + dir_off + index * 8;
        Some((u32_at(bytes, at)?, u32_at(bytes, at + 4)?))
    }

    fn section_headers(bytes: &[u8], at: usize, count: usize) -> Option<Vec<Section>> {
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let base = at.checked_add(i * 40)?;
            if base + 40 > bytes.len() {
                return None;
            }
            out.push(Section {
                vsize: u32_at(bytes, base + 8)?,
                va: u32_at(bytes, base + 12)?,
                raw_size: u32_at(bytes, base + 16)?,
                raw_ptr: u32_at(bytes, base + 20)?,
            });
        }
        Some(out)
    }

    fn dll_names(
        bytes: &[u8],
        sections: &[Section],
        dir: (u32, u32),
        stride: usize,
        name_at: usize,
    ) -> Option<Vec<String>> {
        let (rva, size) = dir;
        if rva == 0 || size == 0 {
            return Some(Vec::new());
        }
        let mut names = Vec::new();
        let mut offset = rva_to_offset(sections, rva)?;
        let end = offset.saturating_add(size as usize).min(bytes.len());
        while offset + stride <= end && names.len() < 128 {
            // MSVC's delay descriptor stores an RVA in this field (attribute
            // bit 0 set). Import descriptors are RVAs as well. The attribute
            // dword is not the name, so delay entries read four bytes in.
            let name_rva = u32_at(bytes, offset + name_at)?;
            if name_rva == 0 {
                break;
            }
            names.push(dll_string(bytes, sections, name_rva)?);
            offset += stride;
        }
        Some(names)
    }

    fn dll_string(bytes: &[u8], sections: &[Section], rva: u32) -> Option<String> {
        let mut at = rva_to_offset(sections, rva)?;
        let mut raw = Vec::new();
        while at < bytes.len() && raw.len() < 128 {
            let byte = bytes[at];
            if byte == 0 {
                let text = std::str::from_utf8(&raw).ok()?;
                if text.is_empty() {
                    return None;
                }
                return Some(text.to_ascii_lowercase());
            }
            if !byte.is_ascii() {
                return None;
            }
            raw.push(byte);
            at += 1;
        }
        None
    }

    fn rva_to_offset(sections: &[Section], rva: u32) -> Option<usize> {
        for section in sections {
            let span = section.vsize.max(section.raw_size);
            if rva >= section.va && rva - section.va < span && (rva - section.va) < section.raw_size
            {
                let delta = (rva - section.va) as usize;
                return Some(section.raw_ptr as usize + delta);
            }
        }
        None
    }

    fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
    }

    fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }

    fn crafted_pe(startup: &[&str], delay: &[&str]) -> Vec<u8> {
        let mut content = Vec::new();
        let mut startup_at = Vec::new();
        let mut delay_at = Vec::new();
        for name in startup {
            startup_at.push(content.len());
            content.extend_from_slice(name.as_bytes());
            content.push(0);
        }
        for name in delay {
            delay_at.push(content.len());
            content.extend_from_slice(name.as_bytes());
            content.push(0);
        }
        while content.len() % 4 != 0 {
            content.push(0);
        }
        let import_at = content.len();
        for at in &startup_at {
            content.extend_from_slice(&0u32.to_le_bytes());
            content.extend_from_slice(&0u32.to_le_bytes());
            content.extend_from_slice(&0u32.to_le_bytes());
            content.extend_from_slice(&(0x1000 + *at as u32).to_le_bytes());
            content.extend_from_slice(&0u32.to_le_bytes());
        }
        content.extend_from_slice(&[0u8; 20]);
        let import_size = content.len() - import_at;
        let delay_desc = content.len();
        for at in &delay_at {
            // Attribute bit 0 set: the following fields are RVAs.
            content.extend_from_slice(&1u32.to_le_bytes());
            content.extend_from_slice(&(0x1000 + *at as u32).to_le_bytes());
            content.extend_from_slice(&[0u8; 24]);
        }
        content.extend_from_slice(&[0u8; 32]);
        let delay_size = content.len() - delay_desc;

        let mut file = vec![0u8; 0x200];
        file[0] = b'M';
        file[1] = b'Z';
        file[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        file[0x80..0x84].copy_from_slice(b"PE\0\0");
        file[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        file[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        file[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        file[0x96..0x98].copy_from_slice(&0x22u16.to_le_bytes());
        file[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        file[0x104..0x108].copy_from_slice(&16u32.to_le_bytes());
        file[0x110..0x114].copy_from_slice(&(0x1000 + import_at as u32).to_le_bytes());
        file[0x114..0x118].copy_from_slice(&(import_size as u32).to_le_bytes());
        file[0x170..0x174].copy_from_slice(&(0x1000 + delay_desc as u32).to_le_bytes());
        file[0x174..0x178].copy_from_slice(&(delay_size as u32).to_le_bytes());
        file[0x188..0x190].copy_from_slice(b".rdata\0\0");
        let raw_size = content.len().next_multiple_of(0x200) as u32;
        file[0x190..0x194].copy_from_slice(&(content.len() as u32).to_le_bytes());
        file[0x194..0x198].copy_from_slice(&0x1000u32.to_le_bytes());
        file[0x198..0x19C].copy_from_slice(&raw_size.to_le_bytes());
        file[0x19C..0x1A0].copy_from_slice(&0x200u32.to_le_bytes());
        file.extend_from_slice(&content);
        while !file.len().is_multiple_of(0x200) {
            file.push(0);
        }
        file
    }
}
