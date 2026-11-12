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
//! `<exe_dir>\conpty.dll`, and only when `<exe_dir>\OpenConsole.exe` is that
//! same directory's file (never a PATH or current-directory lookup), the
//! DLL version is 1.22 or newer, and `OpenConsole.exe` has a trusted
//! Authenticode signature or matches [`OPEN_CONSOLE_SHA256`]. An elevated
//! process also refuses the pair when the file or its directory is writable
//! by a standard user. Any miss, a bad signature, or a failed
//! `CreatePseudoConsole` keeps inbox kernel32. Nothing here panics for a
//! bad bundle.
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
    /// only for a host that still implements it. The bayan tests construct
    /// it; this crate's loader does not.
    #[allow(dead_code)]
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

/// Inbox unless both files are present and [`negotiate`] says the DLL
/// version supports passthrough. An unversioned image, a DLL alone, and a
/// pre-rewrite pair are not loaded: passthrough would be a guess, and DA1
/// would advertise Sixel that conhost then strips.
pub(crate) fn capability_from_probe(probe: BundleProbe) -> ConptyCapability {
    if !probe.dll_is_file || !probe.open_console_is_file {
        return ConptyCapability::Inbox;
    }
    if negotiate(probe.version).passthrough {
        ConptyCapability::BundledRewrite
    } else {
        ConptyCapability::Inbox
    }
}

/// SHA-256 of `OpenConsole.exe`, lowercase hex, or empty. Empty means a
/// trusted Authenticode signature is required. A non-empty pin also
/// accepts that exact digest when the signature check does not.
pub(crate) const OPEN_CONSOLE_SHA256: &str = "";

/// The only host file Bayan will consider: `file_name` joined onto the
/// directory that contains `exe`. `file_name` is a single component, so
/// this cannot name a PATH entry, the current directory, or another folder.
pub(crate) fn bundled_host_path(
    exe: &std::path::Path,
    file_name: &str,
) -> Option<std::path::PathBuf> {
    if file_name != "OpenConsole.exe" && file_name != "conpty.dll" {
        return None;
    }
    let dir = exe.parent()?;
    if !dir.is_absolute() {
        return None;
    }
    let path = dir.join(file_name);
    if path.parent() != Some(dir) || !path.is_absolute() {
        return None;
    }
    Some(path)
}

/// `candidate` is the absolute `OpenConsole.exe` or `conpty.dll` beside `exe`.
pub(crate) fn path_is_beside_exe(exe: &std::path::Path, candidate: &std::path::Path) -> bool {
    let Some(name) = candidate.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    bundled_host_path(exe, name).as_deref() == Some(candidate)
}

/// What the loader learned about one candidate file before mapping it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostFacts {
    pub exists: bool,
    pub reparse: bool,
    pub signature_ok: bool,
    pub hash_ok: bool,
    pub elevated: bool,
    pub file_user_writable: bool,
    pub dir_user_writable: bool,
    /// `false` when the ACL could not be read. Elevated, that is a refusal.
    pub acl_known: bool,
}

/// Use the bundled host only when the file is a real, non-reparse image in
/// the exe directory, it is signed or hash-pinned, and an elevated token
/// is not about to run a copy a standard user can replace.
pub(crate) fn accept_bundled_host(facts: HostFacts) -> bool {
    if !facts.exists || facts.reparse {
        return false;
    }
    if !(facts.signature_ok || facts.hash_ok) {
        return false;
    }
    if facts.elevated && (!facts.acl_known || facts.file_user_writable || facts.dir_user_writable) {
        return false;
    }
    true
}

/// A bundle that was not accepted, or whose `CreatePseudoConsole` did not
/// start, is inbox. The caller still opens kernel32; this is not an error.
pub(crate) fn capability_after_launch(accepted: bool, started: bool) -> ConptyCapability {
    if accepted && started {
        ConptyCapability::BundledRewrite
    } else {
        ConptyCapability::Inbox
    }
}

/// `true` when `got_hex` is the configured pin. An empty pin never matches,
/// so a missing signature is not waved through by accident.
pub(crate) fn hash_matches_pin(got_hex: &str, pin: &str) -> bool {
    !pin.is_empty() && got_hex.eq_ignore_ascii_case(pin)
}

/// Version gate for passthrough. Always a plan: a supported 1.22+ pair
/// passes VT through, and an older or missing version keeps the inbox
/// flags. Neither case is an error, so spawn does not fail closed.
pub(crate) fn negotiate(version: Option<(u16, u16, u16, u16)>) -> ConptyCreate {
    match version {
        Some((1, minor, _, _)) if minor >= BUNDLED_REWRITE_MINOR => {
            create_flags(ConptyCapability::BundledRewrite)
        }
        _ => create_flags(ConptyCapability::Inbox),
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

/// Allow-write ACEs for Everyone, Users, Authenticated Users, or `user_sid`
/// mean a standard user can replace the file. A high or system integrity
/// label with no-write-up blocks a medium token even when such an ACE exists.
pub(crate) fn user_can_write(sddl: &str, user_sid: &str) -> bool {
    if label_blocks_medium(sddl) {
        return false;
    }
    for ace in sddl_section(sddl, 'D').split('(').skip(1) {
        let body = ace.split(')').next().unwrap_or("");
        let mut parts = body.split(';');
        let ace_type = parts.next().unwrap_or("");
        if !ace_type.eq_ignore_ascii_case("A") {
            continue;
        }
        let _flags = parts.next();
        let rights = parts.next().unwrap_or("");
        let _object = parts.next();
        let _inherit = parts.next();
        let trustee = parts.next().unwrap_or("");
        if trustee_is_standard_user(trustee, user_sid) && rights_allow_write(rights) {
            return true;
        }
    }
    false
}

fn sddl_section(sddl: &str, kind: char) -> &str {
    let tag = match kind {
        'D' => "D:",
        'S' => "S:",
        _ => return "",
    };
    let Some(at) = sddl.find(tag) else {
        return "";
    };
    let rest = &sddl[at + 2..];
    if kind == 'D' {
        if let Some(s) = rest.find("S:") {
            return &rest[..s];
        }
    }
    rest
}

fn label_blocks_medium(sddl: &str) -> bool {
    let label = sddl_section(sddl, 'S');
    label.contains("NW") && (label.contains(";;;HI") || label.contains(";;;SI"))
}

fn trustee_is_standard_user(trustee: &str, user_sid: &str) -> bool {
    let trustee = trustee.trim();
    trustee.eq_ignore_ascii_case("WD")
        || trustee.eq_ignore_ascii_case("AU")
        || trustee.eq_ignore_ascii_case("BU")
        || trustee.eq_ignore_ascii_case("S-1-1-0")
        || trustee.eq_ignore_ascii_case("S-1-5-11")
        || trustee.eq_ignore_ascii_case("S-1-5-32-545")
        || (!user_sid.is_empty() && trustee.eq_ignore_ascii_case(user_sid))
}

fn rights_allow_write(rights: &str) -> bool {
    if rights.contains("FA")
        || rights.contains("FW")
        || rights.contains("GW")
        || rights.contains("GA")
    {
        return true;
    }
    let hex = rights
        .strip_prefix("0x")
        .or_else(|| rights.strip_prefix("0X"));
    let Some(hex) = hex else {
        return false;
    };
    let Ok(mask) = u32::from_str_radix(hex, 16) else {
        return false;
    };
    // FILE_WRITE_DATA, FILE_APPEND_DATA, FILE_DELETE_CHILD, DELETE,
    // WRITE_DAC, WRITE_OWNER, GENERIC_ALL, GENERIC_WRITE.
    const WRITE: u32 =
        0x2 | 0x4 | 0x40 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x1000_0000 | 0x4000_0000;
    mask & WRITE != 0
}

/// Exists, is a file, and is not a reparse point. A junction would let the
/// signature check and `conpty.dll` open different targets.
pub(crate) fn plain_host_file(path: &str) -> bool {
    match file_attributes(path) {
        None => false,
        Some(attr) => {
            const DIRECTORY: u32 = 0x10;
            const REPARSE: u32 = 0x400;
            attr & DIRECTORY == 0 && attr & REPARSE == 0
        }
    }
}

/// Trusted Authenticode signature, including catalog signatures of system
/// files. Unsigned, untrusted, and missing files are `false`. No UI.
pub(crate) fn signature_is_trusted(path: &str) -> bool {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let mut file = WintrustFileInfo {
        cb_struct: std::mem::size_of::<WintrustFileInfo>() as u32,
        file_path: wide.as_ptr(),
        file: std::ptr::null_mut(),
        known_subject: std::ptr::null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let mut data = WintrustData {
        cb_struct: std::mem::size_of::<WintrustData>() as u32,
        policy_callback: std::ptr::null_mut(),
        sip_client: std::ptr::null_mut(),
        ui_choice: 2,    // WTD_UI_NONE
        revocation: 0,   // WTD_REVOKE_NONE: offline must not turn a signed file into a failure
        union_choice: 1, // WTD_CHOICE_FILE
        file_info: &mut file,
        state_action: 1, // WTD_STATEACTION_VERIFY
        state_data: std::ptr::null_mut(),
        url_reference: std::ptr::null_mut(),
        prov_flags: 0x1000, // WTD_CACHE_ONLY_URL_RETRIEVAL
        ui_context: 0,
        signature_settings: std::ptr::null_mut(),
    };
    // SAFETY: `wide` is NUL-terminated and outlives both calls. `file` and
    // `data` are live. The first call verifies; the second releases the
    // state `hWVTStateData` the first call stored in `data`.
    let status = unsafe { WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data) };
    data.state_action = 2; // WTD_STATEACTION_CLOSE
    unsafe { WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data) };
    status == 0
}

/// `true` when a standard user's medium token could write `path`. `None`
/// when the ACL could not be read; the elevated caller treats that as writable.
pub(crate) fn path_user_writable(path: &str) -> Option<bool> {
    let sddl = security_sddl(path)?;
    let sid = current_user_sid().unwrap_or_default();
    Some(user_can_write(&sddl, &sid))
}

/// One file of the bundled pair. Missing, reparse, unsigned (and not the
/// pin), or — when `elevated` — user-writable, is `false`.
pub(crate) fn host_file_ok(path: &str, elevated: bool) -> bool {
    if !plain_host_file(path) {
        return false;
    }
    let (acl_known, file_user_writable) = if elevated {
        match path_user_writable(path) {
            Some(writable) => (true, writable),
            None => (false, true),
        }
    } else {
        (true, false)
    };
    accept_bundled_host(HostFacts {
        exists: true,
        reparse: false,
        signature_ok: signature_is_trusted(path),
        hash_ok: hash_matches_pin("", OPEN_CONSOLE_SHA256),
        elevated,
        file_user_writable,
        dir_user_writable: false,
        acl_known,
    })
}

/// This process's elevated bit. A failed query is elevated, so the ACL
/// refusal stays on when the token cannot be proved limited.
pub(crate) fn running_elevated() -> bool {
    token_is_elevated().unwrap_or(true)
}

fn file_attributes(path: &str) -> Option<u32> {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let attr = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attr == 0xFFFF_FFFF {
        None
    } else {
        Some(attr)
    }
}

pub(crate) fn security_sddl(path: &str) -> Option<String> {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    // DACL plus the mandatory label. Label is optional on older calls.
    const DACL: u32 = 0x4;
    const LABEL: u32 = 0x10;
    read_sddl(&wide, DACL | LABEL).or_else(|| read_sddl(&wide, DACL))
}

fn read_sddl(path: &[u16], info: u32) -> Option<String> {
    let mut sd = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated. On success `sd` owns the descriptor
    // and is released with LocalFree. The DACL and SACL pointers are inside
    // that block and are not freed on their own.
    let err = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            1, // SE_FILE_OBJECT
            info,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 || sd.is_null() {
        return None;
    }
    let mut text = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `sd` is the descriptor just returned. `text` is allocated by
    // the call and freed with LocalFree before return.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, info, &mut text, &mut len)
    };
    let sddl = if ok != 0 && !text.is_null() {
        wide_to_string(text)
    } else {
        None
    };
    unsafe {
        if !text.is_null() {
            LocalFree(text.cast());
        }
        LocalFree(sd.cast());
    }
    sddl
}

fn current_user_sid() -> Option<String> {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER: i32 = 1;
    // SAFETY: GetCurrentProcess is a pseudo-handle. The token is closed
    // before return. The first query sizes the buffer; the second writes
    // into `buf`. The SID pointer addresses memory inside `buf` until
    // ConvertSidToStringSidW copies it, and that copy is LocalFree'd.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TOKEN_USER,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 || buf.len() < std::mem::size_of::<SidAndAttributes>() {
            return None;
        }
        let sid = std::ptr::read_unaligned(buf.as_ptr().cast::<SidAndAttributes>()).sid;
        if sid.is_null() {
            return None;
        }
        let mut out = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut out) == 0 || out.is_null() {
            return None;
        }
        let text = wide_to_string(out);
        LocalFree(out.cast());
        text.filter(|s| s.starts_with("S-1-"))
    }
}

fn token_is_elevated() -> Option<bool> {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: i32 = 20;
    // SAFETY: same token rules as `current_user_sid`. The elevation struct
    // is a live local whose size is passed explicitly.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut elevation = 0u32;
        let mut written = 0u32;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION,
            &mut elevation as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
            &mut written,
        );
        CloseHandle(token);
        if ok == 0 {
            None
        } else {
            Some(elevation != 0)
        }
    }
}

fn wide_to_string(wide: *const u16) -> Option<String> {
    if wide.is_null() {
        return None;
    }
    let mut n = 0usize;
    // SAFETY: `wide` is a NUL-terminated string just returned by the API,
    // and this walks only until that NUL.
    unsafe {
        while *wide.add(n) != 0 {
            n += 1;
            if n > 64 * 1024 {
                return None;
            }
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(
            wide, n,
        )))
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[repr(C)]
struct WintrustFileInfo {
    cb_struct: u32,
    file_path: *const u16,
    file: *mut c_void,
    known_subject: *mut c_void,
}

#[repr(C)]
struct WintrustData {
    cb_struct: u32,
    policy_callback: *mut c_void,
    sip_client: *mut c_void,
    ui_choice: u32,
    revocation: u32,
    union_choice: u32,
    file_info: *mut WintrustFileInfo,
    state_action: u32,
    state_data: *mut c_void,
    url_reference: *mut u16,
    prov_flags: u32,
    ui_context: u32,
    signature_settings: *mut c_void,
}

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const WINTRUST_ACTION_GENERIC_VERIFY_V2: Guid = Guid {
    data1: 0x00aa_c56b,
    data2: 0xcd44,
    data3: 0x11d0,
    data4: [0x8c, 0xc2, 0x00, 0xc0, 0x4f, 0xc2, 0x95, 0xee],
};

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

#[link(name = "kernel32")]
extern "system" {
    fn GetFileAttributesW(file_name: *const u16) -> u32;
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: i32,
        info: *mut c_void,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
    fn GetNamedSecurityInfoW(
        name: *const u16,
        object_type: i32,
        info: u32,
        owner: *mut *mut c_void,
        group: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        sd: *mut *mut c_void,
    ) -> u32;
    fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
        sd: *mut c_void,
        revision: u32,
        info: u32,
        out: *mut *mut u16,
        len: *mut u32,
    ) -> i32;
}

#[link(name = "wintrust")]
extern "system" {
    fn WinVerifyTrust(hwnd: *mut c_void, action: *mut Guid, data: *mut WintrustData) -> i32;
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
