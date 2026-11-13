// LOCAL ADDITION — Bayan's pinned Microsoft ConPTY pair; see docs/VENDORED_PATCHES.md.
use super::conpty_resolve::{ConptyLoader, SystemLoader, LOAD_LIBRARY_SEARCH_SYSTEM32};
use std::convert::TryInto;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};

const PINS: &str = include_str!("../../../../resources/conpty.json");
const MAX_HOST_BYTES: u64 = 16 * 1024 * 1024;

/// Keeping both files open without WRITE/DELETE sharing prevents replacing
/// a verified image before LoadLibrary or the later OpenConsole launch.
pub(crate) struct VerifiedPair {
    pub paths: [PathBuf; 2],
    _files: [File; 2],
    _directories: Vec<File>,
}

pub(crate) fn trust_pair(signature_ok: bool, digest_ok: bool) -> bool {
    signature_ok && digest_ok
}

pub(crate) fn digest_matches(bytes: &[u8], expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    expected.len() == 64 && format!("{:x}", Sha256::digest(bytes)).eq_ignore_ascii_case(expected)
}

/// No PATH/CWD lookup and no privileged bundle loading. The exact hashes
/// identify Microsoft's inspected package, not merely any trusted signer.
pub(crate) fn verify_pair(exe: &Path, elevated: bool) -> Option<VerifiedPair> {
    if elevated {
        return None;
    }
    match exe.components().next()? {
        Component::Prefix(p) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => (),
        _ => return None,
    }
    let dir = exe.parent()?;
    let mut directories = Vec::new();
    let ancestors: Vec<_> = dir.ancestors().collect();
    for parent in ancestors.into_iter().rev() {
        // BACKUP_SEMANTICS opens a directory. Keep every ancestor pinned so
        // a junction or renamed directory cannot change a subsequent lookup.
        let directory = File::options()
            .read(true)
            .share_mode(3)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(parent)
            .ok()?;
        let meta = directory.metadata().ok()?;
        if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
            return None;
        }
        directories.push(directory);
    }
    let pins: serde_json::Value = serde_json::from_str(PINS).ok()?;
    let arch = pins.get("architectures")?.get(std::env::consts::ARCH)?;
    let paths = [dir.join("conpty.dll"), dir.join("OpenConsole.exe")];
    let mut files = Vec::new();
    for path in &paths {
        let mut file = File::options()
            .read(true)
            .share_mode(1)
            .custom_flags(0x0020_0000)
            .open(path)
            .ok()?; // OPEN_REPARSE_POINT
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.file_attributes() & 0x400 != 0 || meta.len() > MAX_HOST_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_HOST_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        let pin = arch.get(path.file_name()?.to_str()?)?.as_str()?;
        if !trust_pair(signature_valid(path, &file), digest_matches(&bytes, pin)) {
            return None;
        }
        files.push(file);
    }
    Some(VerifiedPair {
        paths,
        _files: files.try_into().ok()?,
        _directories: directories,
    })
}

pub(crate) fn running_elevated() -> bool {
    let mut token = std::ptr::null_mut();
    let mut elevated = 0u32;
    let mut length = 0;
    // SAFETY: these calls fill the live token/output buffers. A failed query
    // denies bundle loading. The owned token handle is always closed.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), 0x8, &mut token) == 0 {
            return true;
        }
        let ok = GetTokenInformation(
            token,
            20,
            (&mut elevated as *mut u32).cast(),
            4,
            &mut length,
        );
        CloseHandle(token);
        ok == 0 || elevated != 0
    }
}

fn signature_valid(path: &Path, file: &File) -> bool {
    verify_signature(path, file).unwrap_or(false)
}

fn verify_signature(path: &Path, file: &File) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    let trust = SystemLibrary(
        SystemLoader
            .load_library_ex("wintrust.dll", LOAD_LIBRARY_SEARCH_SYSTEM32)
            .ok()?,
    );
    let crypt = SystemLibrary(
        SystemLoader
            .load_library_ex("crypt32.dll", LOAD_LIBRARY_SEARCH_SYSTEM32)
            .ok()?,
    );
    // SAFETY: fixed symbol names from the System32 modules, with the Win32 ABI.
    let (verify, provider, signer, certificate, name): (
        Verify,
        Provider,
        Signer,
        Certificate,
        CertificateName,
    ) = unsafe {
        (
            std::mem::transmute::<*mut c_void, Verify>(
                SystemLoader
                    .get_proc_address(trust.0, "WinVerifyTrust")
                    .ok()?,
            ),
            std::mem::transmute::<*mut c_void, Provider>(
                SystemLoader
                    .get_proc_address(trust.0, "WTHelperProvDataFromStateData")
                    .ok()?,
            ),
            std::mem::transmute::<*mut c_void, Signer>(
                SystemLoader
                    .get_proc_address(trust.0, "WTHelperGetProvSignerFromChain")
                    .ok()?,
            ),
            std::mem::transmute::<*mut c_void, Certificate>(
                SystemLoader
                    .get_proc_address(trust.0, "WTHelperGetProvCertFromChain")
                    .ok()?,
            ),
            std::mem::transmute::<*mut c_void, CertificateName>(
                SystemLoader
                    .get_proc_address(crypt.0, "CertGetNameStringW")
                    .ok()?,
            ),
        )
    };
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut info = WintrustFileInfo {
        size: std::mem::size_of::<WintrustFileInfo>() as u32,
        path: wide.as_ptr(),
        file: file.as_raw_handle(),
        subject: std::ptr::null_mut(),
    };
    let mut data = WintrustData {
        size: std::mem::size_of::<WintrustData>() as u32,
        policy: std::ptr::null_mut(),
        sip: std::ptr::null_mut(),
        ui: 2,
        revocation: 1,
        choice: 1,
        file: &mut info,
        action: 1,
        state: std::ptr::null_mut(),
        url: std::ptr::null_mut(),
        flags: 0x1080,
        context: 0,
        settings: std::ptr::null_mut(),
    };
    let mut action = Guid {
        a: 0x00aa_c56b,
        b: 0xcd44,
        c: 0x11d0,
        d: [0x8c, 0xc2, 0x00, 0xc0, 0x4f, 0xc2, 0x95, 0xee],
    };
    // SAFETY: structures have WinTrust's documented ABI and all backing
    // buffers/handles stay alive through VERIFY and CLOSE. No UI or network.
    let status = unsafe { verify(std::ptr::null_mut(), &mut action, &mut data) };
    let microsoft =
        status == 0 && microsoft_publisher(data.state, provider, signer, certificate, name);
    data.action = 2;
    // SAFETY: CLOSE releases the verification state from the preceding call.
    unsafe { verify(std::ptr::null_mut(), &mut action, &mut data) };
    Some(microsoft)
}

fn microsoft_publisher(
    state: *mut c_void,
    provider_fn: Provider,
    signer_fn: Signer,
    certificate_fn: Certificate,
    name_fn: CertificateName,
) -> bool {
    if state.is_null() {
        return false;
    }
    // SAFETY: this state belongs to a successful WinVerifyTrust call and
    // remains open. Helpers return pointers owned by that state.
    unsafe {
        let provider = provider_fn(state);
        if provider.is_null() {
            return false;
        }
        let signer = signer_fn(provider, 0, 0, 0);
        if signer.is_null() {
            return false;
        }
        let certificate = certificate_fn(signer, 0);
        if certificate.is_null() || (*certificate).certificate.is_null() {
            return false;
        }
        let mut name = [0u16; 256];
        let n = name_fn(
            (*certificate).certificate,
            4,
            0,
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        ) as usize;
        n > 1
            && n <= name.len()
            && String::from_utf16_lossy(&name[..n - 1]) == "Microsoft Corporation"
    }
}

#[repr(C)]
struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}
#[repr(C)]
struct WintrustFileInfo {
    size: u32,
    path: *const u16,
    file: *mut c_void,
    subject: *mut c_void,
}
#[repr(C)]
struct WintrustData {
    size: u32,
    policy: *mut c_void,
    sip: *mut c_void,
    ui: u32,
    revocation: u32,
    choice: u32,
    file: *mut WintrustFileInfo,
    action: u32,
    state: *mut c_void,
    url: *mut u16,
    flags: u32,
    context: u32,
    settings: *mut c_void,
}
// Only the ABI's first two fields are read; the provider owns the rest.
#[repr(C)]
struct ProviderCertificate {
    size: u32,
    certificate: *const c_void,
}
type Verify = unsafe extern "system" fn(*mut c_void, *mut Guid, *mut WintrustData) -> i32;
type Provider = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
type Signer = unsafe extern "system" fn(*mut c_void, u32, i32, u32) -> *mut c_void;
type Certificate = unsafe extern "system" fn(*mut c_void, u32) -> *const ProviderCertificate;
type CertificateName =
    unsafe extern "system" fn(*const c_void, u32, u32, *mut c_void, *mut u16, u32) -> u32;
struct SystemLibrary(*mut c_void);
impl Drop for SystemLibrary {
    fn drop(&mut self) {
        // SAFETY: this module came from our System32 LoadLibraryExW call.
        unsafe { FreeLibrary(self.0) };
    }
}
#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        data: *mut c_void,
        size: u32,
        length: *mut u32,
    ) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
}
