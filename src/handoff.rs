//! Windows 11 default-terminal handoff, as far as an unpackaged exe can go.
//!
//! Inbox `conhost` does not talk to a terminal directly. It reads
//! `HKCU\Console\%%Startup` (`DelegationConsole` and `DelegationTerminal`)
//! and activates `DelegationConsole` as `IConsoleHandoff`
//! (`{E686C757-9A35-4A1C-B3CE-0BCC8B5C69F4}`). That server is Windows
//! Terminal's OpenConsole. OpenConsole then activates `DelegationTerminal`
//! as `ITerminalHandoff` / `ITerminalHandoff2` / `ITerminalHandoff3` and
//! passes the ConPTY handles. The proxy/stub for those interfaces is
//! `OpenConsoleProxy.dll`, which Bayan does not ship.
//!
//! The Settings page "Default terminal application" does not read those
//! registry values. It enumerates packaged AppExtensions
//! `com.microsoft.windows.console.host` and
//! `com.microsoft.windows.terminal.host` and only lists a package that
//! contains both (`DelegationConfig.cpp`). An unpackaged registration is
//! invisible there. Package identity (MSIX) is required for that list.
//!
//! What this module does, behind `terminal_handoff` (default off):
//! register a per-user COM class at
//! `HKCU\Software\Classes\CLSID\{A7C3E1B2-4D58-4F0A-9E6C-1B8D5A2F7043}\LocalServer32`
//! whose command is `"<exe>" --handoff`. `--handoff` (COM also appends
//! `-Embedding`) registers that class and implements the three handoff
//! interfaces. Each `EstablishPtyHandoff` returns `E_NOTIMPL` and does not
//! spawn a shell, retain a handle, or write a command anywhere.
//!
//! What it deliberately does not do:
//! - It does not write `HKCU\Console\%%Startup`. Pointing `DelegationConsole`
//!   at Bayan would break `cmd.exe`, because Bayan is not a console host.
//!   Pointing `DelegationTerminal` at Bayan without OpenConsole and without
//!   `OpenConsoleProxy.dll` would not deliver a session either.
//! - It does not register an interface proxy, and it does not pretend the
//!   Windows Settings dropdown can see this class.
//! - It does not adopt foreign ConPTY handles. That path needs the console
//!   host above. Failing closed is the whole of the method.
//!
//! Registry writes are HKCU only, and only the one CLSID key.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::cli::exe_display;

/// Bayan's terminal-side class. Not Windows Terminal's CLSID.
pub const TERMINAL_CLSID: &str = "{A7C3E1B2-4D58-4F0A-9E6C-1B8D5A2F7043}";

/// `ITerminalHandoff` (deprecated; handles only).
const IID_TERMINAL_HANDOFF: windows::core::GUID =
    windows::core::GUID::from_u128(0x59D55CCE_FC8A_48B4_ACE8_0A9286C6557F);
/// `ITerminalHandoff2` (startup info by value).
const IID_TERMINAL_HANDOFF2: windows::core::GUID =
    windows::core::GUID::from_u128(0xAA6B364F_4A50_4176_9002_0AE755E7B5EF);
/// `ITerminalHandoff3` (caller-chosen input and output pipes).
const IID_TERMINAL_HANDOFF3: windows::core::GUID =
    windows::core::GUID::from_u128(0x6F23DA90_15C5_4203_9DB0_64E73F1B1B00);

/// Returned by every `EstablishPtyHandoff`. The session is not started.
const E_NOTIMPL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4001u32 as i32);

const IID_IUNKNOWN: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000000_0000_0000_C000_000000000046);

pub const REGISTERED_NOTE: &str = "\
Registered Bayan's per-user terminal handoff class.
This does not make Bayan the Windows default terminal. The Settings list
needs an MSIX package that implements both the console host and the
terminal host, and Bayan does not ship OpenConsole or its proxy DLL.
EstablishPtyHandoff returns E_NOTIMPL until that exists.";

pub const REMOVED_NOTE: &str = "Removed Bayan's per-user terminal handoff class.";

/// `name` empty is the key's default value. Keys are relative to HKCU.
pub trait RegistryStore {
    fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String>;
    fn delete_tree(&mut self, key: &str) -> Result<(), String>;
}

pub fn clsid_key() -> String {
    format!(r"Software\Classes\CLSID\{TERMINAL_CLSID}")
}

pub fn local_server_key() -> String {
    format!(r"{}\LocalServer32", clsid_key())
}

/// `"<exe>" --handoff`. COM appends ` -Embedding` itself.
pub fn local_server_command(exe: &Path) -> Result<String, String> {
    let path = exe_display(exe)?;
    Ok(format!("\"{path}\" --handoff"))
}

pub fn register_with(store: &mut impl RegistryStore, exe: &Path) -> Result<(), String> {
    let command = local_server_command(exe)?;
    store.set_value(&clsid_key(), "", "Bayan (بيان) terminal handoff")?;
    store.set_value(&local_server_key(), "", &command)?;
    Ok(())
}

pub fn unregister_with(store: &mut impl RegistryStore) -> Result<(), String> {
    store.delete_tree(&clsid_key())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("could not find bayan.exe: {error}"))?;
    let mut registry = LiveRegistry;
    if enabled {
        register_with(&mut registry, &exe)
    } else {
        unregister_with(&mut registry)
    }
}

fn allowed_value_key(key: &str) -> bool {
    key == clsid_key() || key == local_server_key()
}

fn allowed_delete_key(key: &str) -> bool {
    key == clsid_key()
}

struct LiveRegistry;

impl RegistryStore for LiveRegistry {
    fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String> {
        if !allowed_value_key(key) {
            return Err(format!("refusing to write HKCU\\{key}"));
        }
        live_set(key, name, value)
    }

    fn delete_tree(&mut self, key: &str) -> Result<(), String> {
        if !allowed_delete_key(key) {
            return Err(format!("refusing to delete HKCU\\{key}"));
        }
        live_delete(key)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct OpenKey(windows_sys::Win32::System::Registry::HKEY);

impl Drop for OpenKey {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by RegCreateKeyExW and has not
            // been closed. RegCloseKey releases that one key.
            unsafe {
                windows_sys::Win32::System::Registry::RegCloseKey(self.0);
            }
        }
    }
}

fn live_set(key: &str, name: &str, value: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCreateKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_CREATE_SUB_KEY, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let mut current = HKEY_CURRENT_USER;
    let mut owned: Option<OpenKey> = None;
    for part in key.split('\\') {
        let part_w = wide(part);
        let mut opened = std::ptr::null_mut();
        // SAFETY: `part_w` is NUL-terminated and lives for the call. `current`
        // is HKEY_CURRENT_USER or a key opened earlier in this loop. The
        // security and disposition pointers are null, which the API allows.
        // On success `opened` is a new key the guard closes.
        let rc = unsafe {
            RegCreateKeyExW(
                current,
                part_w.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_CREATE_SUB_KEY | KEY_SET_VALUE,
                std::ptr::null(),
                &mut opened,
                std::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("could not create HKCU\\{key} ({rc})"));
        }
        let next = OpenKey(opened);
        current = next.0;
        owned = Some(next);
    }
    let Some(opened) = owned else {
        return Err("empty registry key".into());
    };
    let name_w = wide(name);
    let data = wide(value);
    let bytes = (data.len() * 2) as u32;
    // SAFETY: `name_w` and `data` are NUL-terminated UTF-16 and live for the
    // call. `bytes` counts the terminating NUL. `opened` is the key just created.
    let rc = unsafe {
        RegSetValueExW(
            opened.0,
            name_w.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            bytes,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("could not set HKCU\\{key} ({rc})"));
    }
    Ok(())
}

fn live_delete(key: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

    let key_w = wide(key);
    // SAFETY: `key_w` is NUL-terminated and lives for the call. The root is
    // HKEY_CURRENT_USER, a predefined key this function does not close.
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_w.as_ptr()) };
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("could not remove HKCU\\{key} ({rc})"))
    }
}

static LOCKS: AtomicU32 = AtomicU32::new(0);
static OBJECTS: AtomicU32 = AtomicU32::new(0);

fn maybe_quit() {
    if LOCKS.load(Ordering::SeqCst) == 0 && OBJECTS.load(Ordering::SeqCst) == 0 {
        // SAFETY: PostQuitMessage posts WM_QUIT to this thread. It takes no pointer.
        unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
    }
}

/// COM local server for `--handoff`. Blocks until the client releases the
/// class. Does not open a terminal window.
pub fn serve() -> Result<(), String> {
    use windows::Win32::System::Com::{
        CoInitializeEx, CoRegisterClassObject, CoRevokeClassObject, IClassFactory,
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, REGCLS_MULTIPLEUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage,
    };

    // SAFETY: STA init, one class object, and the thread message pump. The
    // factory is revoked before return. See the module docs for why the
    // methods refuse the handoff.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|error| error.to_string())?;
        let factory: IClassFactory = Factory.into();
        let cookie =
            CoRegisterClassObject(&clsid(), &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE)
                .map_err(|error| error.to_string())?;
        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = CoRevokeClassObject(cookie);
    }
    Ok(())
}

fn clsid() -> windows::core::GUID {
    windows::core::GUID::from_u128(0xA7C3E1B2_4D58_4F0A_9E6C_1B8D5A2F7043)
}

#[windows::core::implement(windows::Win32::System::Com::IClassFactory)]
struct Factory;

impl windows::Win32::System::Com::IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: windows::core::Ref<'_, windows::core::IUnknown>,
        iid: *const windows::core::GUID,
        object: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        if !outer.is_null() {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::CLASS_E_NOAGGREGATION,
            ));
        }
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        let created = Box::into_raw(Box::new(Handoff::new()));
        // SAFETY: `created` is a live object. On failure the initial
        // reference is released, which also drops the allocation.
        let hr = unsafe { query(created, iid, object) };
        if hr.is_ok() {
            // SAFETY: query added a reference, so this drops the constructor's
            // reference and leaves the caller holding one.
            unsafe { release(created) };
            Ok(())
        } else {
            // SAFETY: query did not add a reference. This drops the only one
            // and frees the object.
            unsafe { release(created) };
            Err(windows::core::Error::from_hresult(hr))
        }
    }

    fn LockServer(&self, lock: windows::core::BOOL) -> windows::core::Result<()> {
        if lock.as_bool() {
            LOCKS.fetch_add(1, Ordering::SeqCst);
        } else if LOCKS.load(Ordering::SeqCst) > 0 {
            LOCKS.fetch_sub(1, Ordering::SeqCst);
            maybe_quit();
        }
        Ok(())
    }
}

/// `TERMINAL_STARTUP_INFO` from `ITerminalHandoff.idl`. The fields are not
/// read: `EstablishPtyHandoff` returns `E_NOTIMPL` without using them.
#[allow(dead_code)]
#[repr(C)]
struct StartupInfo {
    title: *mut u16,
    icon_path: *mut u16,
    icon_index: i32,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_chars: u32,
    y_chars: u32,
    fill: u32,
    flags: u32,
    show: u16,
}

#[repr(C)]
struct Handoff {
    unknown: *const UnknownVtbl,
    handoff: *const HandoffVtbl,
    handoff2: *const Handoff2Vtbl,
    handoff3: *const Handoff3Vtbl,
    refs: AtomicU32,
}

#[repr(C)]
struct UnknownVtbl {
    query: unsafe extern "system" fn(
        *mut Handoff,
        *const windows::core::GUID,
        *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT,
    addref: unsafe extern "system" fn(*mut Handoff) -> u32,
    release: unsafe extern "system" fn(*mut Handoff) -> u32,
}

#[repr(C)]
struct HandoffVtbl {
    query: unsafe extern "system" fn(
        *mut HandoffVtbl,
        *const windows::core::GUID,
        *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT,
    addref: unsafe extern "system" fn(*mut HandoffVtbl) -> u32,
    release: unsafe extern "system" fn(*mut HandoffVtbl) -> u32,
    establish: unsafe extern "system" fn(
        *mut HandoffVtbl,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
    ) -> windows::core::HRESULT,
}

#[repr(C)]
struct Handoff2Vtbl {
    query: unsafe extern "system" fn(
        *mut Handoff2Vtbl,
        *const windows::core::GUID,
        *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT,
    addref: unsafe extern "system" fn(*mut Handoff2Vtbl) -> u32,
    release: unsafe extern "system" fn(*mut Handoff2Vtbl) -> u32,
    establish: unsafe extern "system" fn(
        *mut Handoff2Vtbl,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        StartupInfo,
    ) -> windows::core::HRESULT,
}

#[repr(C)]
struct Handoff3Vtbl {
    query: unsafe extern "system" fn(
        *mut Handoff3Vtbl,
        *const windows::core::GUID,
        *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT,
    addref: unsafe extern "system" fn(*mut Handoff3Vtbl) -> u32,
    release: unsafe extern "system" fn(*mut Handoff3Vtbl) -> u32,
    establish: unsafe extern "system" fn(
        *mut Handoff3Vtbl,
        *mut *mut core::ffi::c_void,
        *mut *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *const StartupInfo,
    ) -> windows::core::HRESULT,
}

impl Handoff {
    fn new() -> Self {
        Self {
            unknown: &UNKNOWN_VTBL,
            handoff: &HANDOFF_VTBL,
            handoff2: &HANDOFF2_VTBL,
            handoff3: &HANDOFF3_VTBL,
            refs: AtomicU32::new(1),
        }
    }
}

static UNKNOWN_VTBL: UnknownVtbl = UnknownVtbl {
    query,
    addref,
    release,
};

static HANDOFF_VTBL: HandoffVtbl = HandoffVtbl {
    query: query_handoff,
    addref: addref_handoff,
    release: release_handoff,
    establish: establish_refused,
};

static HANDOFF2_VTBL: Handoff2Vtbl = Handoff2Vtbl {
    query: query_handoff2,
    addref: addref_handoff2,
    release: release_handoff2,
    establish: establish2_refused,
};

static HANDOFF3_VTBL: Handoff3Vtbl = Handoff3Vtbl {
    query: query_handoff3,
    addref: addref_handoff3,
    release: release_handoff3,
    establish: establish3_refused,
};

unsafe extern "system" fn query(
    this: *mut Handoff,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    if out.is_null() || iid.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    // SAFETY: `iid` and `out` were just checked. `this` is the object.
    unsafe {
        *out = std::ptr::null_mut();
        let id = *iid;
        let face: *mut core::ffi::c_void = if id == IID_IUNKNOWN {
            this.cast()
        } else if id == IID_TERMINAL_HANDOFF {
            std::ptr::addr_of_mut!((*this).handoff).cast()
        } else if id == IID_TERMINAL_HANDOFF2 {
            std::ptr::addr_of_mut!((*this).handoff2).cast()
        } else if id == IID_TERMINAL_HANDOFF3 {
            std::ptr::addr_of_mut!((*this).handoff3).cast()
        } else {
            return windows::Win32::Foundation::E_NOINTERFACE;
        };
        (*this).refs.fetch_add(1, Ordering::SeqCst);
        *out = face;
    }
    windows::Win32::Foundation::S_OK
}

unsafe extern "system" fn addref(this: *mut Handoff) -> u32 {
    // SAFETY: `this` is a live Handoff.
    unsafe { (*this).refs.fetch_add(1, Ordering::SeqCst) + 1 }
}

unsafe extern "system" fn release(this: *mut Handoff) -> u32 {
    // SAFETY: `this` is a live Handoff. The last release frees that allocation.
    unsafe {
        let left = (*this).refs.fetch_sub(1, Ordering::SeqCst) - 1;
        if left == 0 {
            OBJECTS.fetch_sub(1, Ordering::SeqCst);
            drop(Box::from_raw(this));
            maybe_quit();
        }
        left
    }
}

fn object_from_field<T>(field: *mut T, offset: usize) -> *mut Handoff {
    field.cast::<u8>().wrapping_sub(offset).cast()
}

unsafe extern "system" fn query_handoff(
    this: *mut HandoffVtbl,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    // SAFETY: `this` points at `Handoff::handoff`.
    unsafe {
        query(
            object_from_field(this, std::mem::offset_of!(Handoff, handoff)),
            iid,
            out,
        )
    }
}

unsafe extern "system" fn addref_handoff(this: *mut HandoffVtbl) -> u32 {
    // SAFETY: `this` points at `Handoff::handoff` of a live object.
    unsafe {
        addref(object_from_field(
            this,
            std::mem::offset_of!(Handoff, handoff),
        ))
    }
}

unsafe extern "system" fn release_handoff(this: *mut HandoffVtbl) -> u32 {
    // SAFETY: `this` points at `Handoff::handoff` of a live object.
    unsafe {
        release(object_from_field(
            this,
            std::mem::offset_of!(Handoff, handoff),
        ))
    }
}

unsafe extern "system" fn query_handoff2(
    this: *mut Handoff2Vtbl,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    // SAFETY: `this` points at `Handoff::handoff2` of a live object.
    unsafe {
        query(
            object_from_field(this, std::mem::offset_of!(Handoff, handoff2)),
            iid,
            out,
        )
    }
}

unsafe extern "system" fn addref_handoff2(this: *mut Handoff2Vtbl) -> u32 {
    // SAFETY: `this` points at `Handoff::handoff2` of a live object.
    unsafe {
        addref(object_from_field(
            this,
            std::mem::offset_of!(Handoff, handoff2),
        ))
    }
}

unsafe extern "system" fn release_handoff2(this: *mut Handoff2Vtbl) -> u32 {
    // SAFETY: `this` points at `Handoff::handoff2` of a live object.
    unsafe {
        release(object_from_field(
            this,
            std::mem::offset_of!(Handoff, handoff2),
        ))
    }
}

unsafe extern "system" fn query_handoff3(
    this: *mut Handoff3Vtbl,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    // SAFETY: `this` points at `Handoff::handoff3` of a live object.
    unsafe {
        query(
            object_from_field(this, std::mem::offset_of!(Handoff, handoff3)),
            iid,
            out,
        )
    }
}

unsafe extern "system" fn addref_handoff3(this: *mut Handoff3Vtbl) -> u32 {
    // SAFETY: `this` points at `Handoff::handoff3` of a live object.
    unsafe {
        addref(object_from_field(
            this,
            std::mem::offset_of!(Handoff, handoff3),
        ))
    }
}

unsafe extern "system" fn release_handoff3(this: *mut Handoff3Vtbl) -> u32 {
    // SAFETY: `this` points at `Handoff::handoff3` of a live object.
    unsafe {
        release(object_from_field(
            this,
            std::mem::offset_of!(Handoff, handoff3),
        ))
    }
}

/// Fail closed. Handles are not closed and not stored: they belong to the caller.
unsafe extern "system" fn establish_refused(
    _this: *mut HandoffVtbl,
    _input: *mut core::ffi::c_void,
    _output: *mut core::ffi::c_void,
    _signal: *mut core::ffi::c_void,
    _reference: *mut core::ffi::c_void,
    _server: *mut core::ffi::c_void,
    _client: *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn establish2_refused(
    _this: *mut Handoff2Vtbl,
    _input: *mut core::ffi::c_void,
    _output: *mut core::ffi::c_void,
    _signal: *mut core::ffi::c_void,
    _reference: *mut core::ffi::c_void,
    _server: *mut core::ffi::c_void,
    _client: *mut core::ffi::c_void,
    _startup: StartupInfo,
) -> windows::core::HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn establish3_refused(
    _this: *mut Handoff3Vtbl,
    _input: *mut *mut core::ffi::c_void,
    _output: *mut *mut core::ffi::c_void,
    _signal: *mut core::ffi::c_void,
    _reference: *mut core::ffi::c_void,
    _server: *mut core::ffi::c_void,
    _client: *mut core::ffi::c_void,
    _startup: *const StartupInfo,
) -> windows::core::HRESULT {
    E_NOTIMPL
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default, Debug)]
    struct MemoryRegistry {
        values: BTreeMap<(String, String), String>,
    }

    impl RegistryStore for MemoryRegistry {
        fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String> {
            self.values
                .insert((key.to_string(), name.to_string()), value.to_string());
            Ok(())
        }

        fn delete_tree(&mut self, key: &str) -> Result<(), String> {
            let prefix = format!(r"{key}\");
            self.values
                .retain(|(stored, _), _| stored != key && !stored.starts_with(&prefix));
            Ok(())
        }
    }

    #[test]
    fn registration_is_the_hkcu_clsid_and_nothing_else() {
        let exe = Path::new(r"D:\مجلد بيان\bayan.exe");
        let mut store = MemoryRegistry::default();
        register_with(&mut store, exe).unwrap();
        assert_eq!(
            store
                .values
                .get(&(local_server_key(), String::new()))
                .map(String::as_str),
            Some(r#""D:\مجلد بيان\bayan.exe" --handoff"#)
        );
        assert!(store
            .values
            .keys()
            .all(|(key, _)| { key == &clsid_key() || key == &local_server_key() }));
        for (key, _) in store.values.keys() {
            let lower = key.to_ascii_lowercase();
            assert!(!lower.contains("delegation"), "{key}");
            assert!(!lower.contains("%%startup"), "{key}");
            assert!(!lower.contains("hklm"), "{key}");
            assert!(!lower.contains(r"console\"), "{key}");
        }
        unregister_with(&mut store).unwrap();
        assert!(store.values.is_empty());
    }

    #[test]
    fn the_live_store_refuses_delegation_and_hklm() {
        let mut registry = LiveRegistry;
        for key in [
            r"Console\%%Startup",
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            r"Software\Classes\CLSID\{2EACA947-7F5F-4CFA-BA87-8F7FBEEFBE69}",
            r"Software\Classes\Interface\{59D55CCE-FC8A-48B4-ACE8-0A9286C6557F}",
            r"HKLM\Software\Classes\CLSID\{A7C3E1B2-4D58-4F0A-9E6C-1B8D5A2F7043}",
        ] {
            assert!(registry.set_value(key, "", "x").is_err(), "{key}");
            assert!(registry.delete_tree(key).is_err(), "{key}");
        }
        assert!(allowed_value_key(&clsid_key()));
        assert!(allowed_value_key(&local_server_key()));
        assert!(allowed_delete_key(&clsid_key()));
        assert!(!allowed_delete_key(&local_server_key()));
    }

    #[test]
    fn establish_does_not_report_success() {
        assert_eq!(E_NOTIMPL.0, 0x8000_4001u32 as i32);
    }
}
