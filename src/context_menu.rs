//! Per-user Explorer "Open Bayan here".
//!
//! Two verbs, both under `HKEY_CURRENT_USER\Software\Classes`:
//! `Directory\shell\Bayan` (a folder icon) and
//! `Directory\Background\shell\Bayan` (the folder background). The command
//! is `"<exe>" -d "%V\."`. The `\.` keeps a drive root from ending in `\"`,
//! which Windows would hand the program as `C:"`.
//! Nothing here opens `HKEY_LOCAL_MACHINE` or asks for an administrator.
//!
//! Windows 11's top-level context menu only lists verbs from a package with
//! identity (MSIX, a later installer item). Until then this is the classic
//! entry under "Show more options".
//!
//! The writes go through [`RegistryStore`], so tests use a memory stand-in
//! and never touch the real registry. The live store refuses any key that
//! is not one of the two verbs before it calls Win32.

use std::path::Path;

use crate::cli::exe_display;

pub const MENU_LABEL: &str = "افتح بيان هنا";

pub const REGISTERED_NOTE: &str = "\
أُضيف «افتح بيان هنا» للمستخدم الحالي.
Windows 11 shows it under \"Show more options\" until Bayan has package identity (MSIX).";

pub const REMOVED_NOTE: &str = "أُزيل «افتح بيان هنا» من قائمة المستكشف للمستخدم الحالي.";

const VERBS: [&str; 2] = [
    r"Software\Classes\Directory\shell\Bayan",
    r"Software\Classes\Directory\Background\shell\Bayan",
];

/// `name` empty is the key's default value. Keys are relative to HKCU.
pub trait RegistryStore {
    fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String>;
    fn get_value(&self, key: &str, name: &str) -> Result<Option<String>, String>;
    fn delete_tree(&mut self, key: &str) -> Result<(), String>;
}

/// `"<exe>" -d "%V\."`. The exe is always quoted, including when it has no spaces.
/// `%V\.` is `C:\.` at a drive root and `C:\dir\.` anywhere else, never `"C:\"`.
pub fn context_menu_command(exe: &Path) -> Result<String, String> {
    let path = exe_display(exe)?;
    Ok(format!("\"{path}\" -d \"%V\\.\""))
}

/// UTF-16LE bytes of a `REG_SZ`, including the terminating NUL. This is the
/// buffer [`live_set`] passes to `RegSetValueExW`.
pub(crate) fn reg_sz_bytes(value: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(value.len() * 2 + 2);
    for unit in value.encode_utf16().chain(Some(0)) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The string `RegQueryValueExW` would hand back from [`reg_sz_bytes`].
pub(crate) fn reg_sz_decode(bytes: &[u8]) -> Result<String, String> {
    if !bytes.len().is_multiple_of(2) {
        return Err("REG_SZ length is not even".into());
    }
    let (chunks, _) = bytes.as_chunks::<2>();
    let mut units = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        units.push(u16::from_le_bytes(*chunk));
    }
    if units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16(&units).map_err(|_| "REG_SZ is not Unicode".into())
}

pub fn register_with(store: &mut impl RegistryStore, exe: &Path) -> Result<(), String> {
    let command = context_menu_command(exe)?;
    let icon = exe_display(exe)?;
    for verb in VERBS {
        store.set_value(verb, "", MENU_LABEL)?;
        store.set_value(verb, "Icon", &icon)?;
        let command_key = format!(r"{verb}\command");
        store.set_value(&command_key, "", &command)?;
    }
    Ok(())
}

pub fn unregister_with(store: &mut impl RegistryStore) -> Result<(), String> {
    for verb in VERBS {
        store.delete_tree(verb)?;
    }
    Ok(())
}

/// `true` when every verb already has this exe's label, icon and command.
fn menu_matches(store: &impl RegistryStore, exe: &Path) -> Result<bool, String> {
    let command = context_menu_command(exe)?;
    let icon = exe_display(exe)?;
    for verb in VERBS {
        if store.get_value(verb, "")?.as_deref() != Some(MENU_LABEL) {
            return Ok(false);
        }
        if store.get_value(verb, "Icon")?.as_deref() != Some(icon.as_str()) {
            return Ok(false);
        }
        let command_key = format!(r"{verb}\command");
        if store.get_value(&command_key, "")?.as_deref() != Some(command.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn menu_present(store: &impl RegistryStore) -> Result<bool, String> {
    for verb in VERBS {
        if store.get_value(verb, "")?.is_some() {
            return Ok(true);
        }
        let command_key = format!(r"{verb}\command");
        if store.get_value(&command_key, "")?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Write or remove the menu. Returns whether the registry changed. A second
/// call with the same exe and flag does not write again.
pub fn sync_with(
    store: &mut impl RegistryStore,
    exe: &Path,
    enabled: bool,
) -> Result<bool, String> {
    if enabled {
        if menu_matches(store, exe)? {
            return Ok(false);
        }
        register_with(store, exe)?;
        Ok(true)
    } else if menu_present(store)? {
        unregister_with(store)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Installer and the explicit CLI flags call this. `enabled` writes the two
/// HKCU verbs; `false` deletes them. The command points at this process's exe.
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

/// What startup does with `explorer_context_menu`. Skips the write when the
/// verbs already say what this exe would write, or are already absent.
pub fn sync_enabled(enabled: bool) -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("could not find bayan.exe: {error}"))?;
    let mut registry = LiveRegistry;
    sync_with(&mut registry, &exe, enabled).map(|_| ())
}

/// A value key we are willing to write: a verb, or that verb's `command` subkey.
fn allowed_value_key(key: &str) -> bool {
    VERBS.iter().any(|verb| {
        key == *verb
            || key
                .strip_prefix(verb)
                .is_some_and(|rest| rest == r"\command")
    })
}

/// A key we are willing to delete: the verb itself, which takes `command` with it.
fn allowed_delete_key(key: &str) -> bool {
    VERBS.contains(&key)
}

struct LiveRegistry;

impl RegistryStore for LiveRegistry {
    fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String> {
        if !allowed_value_key(key) {
            return Err(format!("refusing to write HKCU\\{key}"));
        }
        live_set(key, name, value)
    }

    fn get_value(&self, key: &str, name: &str) -> Result<Option<String>, String> {
        if !allowed_value_key(key) {
            return Err(format!("refusing to read HKCU\\{key}"));
        }
        live_get(key, name)
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
    let data = reg_sz_bytes(value);
    let bytes = data.len() as u32;
    // SAFETY: `name_w` is NUL-terminated UTF-16 and `data` is the REG_SZ
    // bytes from `reg_sz_bytes` (UTF-16LE plus a NUL). Both live for the call.
    // `opened` is the key just created.
    let rc = unsafe { RegSetValueExW(opened.0, name_w.as_ptr(), 0, REG_SZ, data.as_ptr(), bytes) };
    if rc != ERROR_SUCCESS {
        return Err(format!("could not set HKCU\\{key} ({rc})"));
    }
    Ok(())
}

fn live_get(key: &str, name: &str) -> Result<Option<String>, String> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_SZ,
    };

    let key_w = wide(key);
    let mut opened = std::ptr::null_mut();
    // SAFETY: `key_w` is NUL-terminated and lives for the call. The root is
    // HKEY_CURRENT_USER. On success `opened` is a key the guard closes.
    // This opens; it does not create.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut opened,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        return Err(format!("could not open HKCU\\{key} ({rc})"));
    }
    let opened = OpenKey(opened);
    let name_w = wide(name);
    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: `name_w` is NUL-terminated and lives for the call. A null data
    // pointer asks for the size only. `opened` is the key just opened.
    let rc = unsafe {
        RegQueryValueExW(
            opened.0,
            name_w.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS && rc != ERROR_MORE_DATA {
        return Err(format!("could not read HKCU\\{key} ({rc})"));
    }
    if kind != REG_SZ || size > 64 * 1024 {
        return Ok(None);
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: `buf` has `size` bytes and lives for the call. `name_w` is the
    // same NUL-terminated name. The API writes at most `size` bytes.
    let rc = unsafe {
        RegQueryValueExW(
            opened.0,
            name_w.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buf.as_mut_ptr(),
            &mut size,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        return Err(format!("could not read HKCU\\{key} ({rc})"));
    }
    let n = (size as usize).min(buf.len());
    reg_sz_decode(&buf[..n]).map(Some)
}

fn live_delete(key: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

    let key_w = wide(key);
    // SAFETY: `key_w` is NUL-terminated and lives for the call. The root is
    // HKEY_CURRENT_USER, a predefined key this function does not close.
    // RegDeleteTreeW removes `key` and everything under it.
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_w.as_ptr()) };
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("could not remove HKCU\\{key} ({rc})"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default, Debug)]
    struct MemoryRegistry {
        values: BTreeMap<(String, String), String>,
        writes: u32,
    }

    impl RegistryStore for MemoryRegistry {
        fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String> {
            self.writes += 1;
            self.values
                .insert((key.to_string(), name.to_string()), value.to_string());
            Ok(())
        }

        fn get_value(&self, key: &str, name: &str) -> Result<Option<String>, String> {
            Ok(self
                .values
                .get(&(key.to_string(), name.to_string()))
                .cloned())
        }

        fn delete_tree(&mut self, key: &str) -> Result<(), String> {
            self.writes += 1;
            let prefix = format!(r"{key}\");
            self.values
                .retain(|(stored, _), _| stored != key && !stored.starts_with(&prefix));
            Ok(())
        }
    }

    #[test]
    fn the_command_string_quotes_spaces_and_arabic() {
        let spaced = context_menu_command(Path::new(r"C:\Program Files\Bayan\bayan.exe")).unwrap();
        assert_eq!(spaced, r#""C:\Program Files\Bayan\bayan.exe" -d "%V\.""#);
        let arabic = context_menu_command(Path::new(r"D:\مجلد بيان\bayan.exe")).unwrap();
        assert_eq!(arabic, "\"D:\\مجلد بيان\\bayan.exe\" -d \"%V\\.\"");
        let plain = context_menu_command(Path::new(r"C:\bayan.exe")).unwrap();
        assert_eq!(plain, r#""C:\bayan.exe" -d "%V\.""#);
    }

    #[test]
    fn the_arabic_label_round_trips_through_reg_sz_bytes() {
        assert_eq!(MENU_LABEL, "افتح بيان هنا");
        let bytes = reg_sz_bytes(MENU_LABEL);
        assert_eq!(bytes.len() % 2, 0);
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0], "REG_SZ ends with a NUL");
        assert_eq!(reg_sz_decode(&bytes).unwrap(), MENU_LABEL);
        // U+0627 ALEF is the first letter, stored little-endian.
        assert_eq!(&bytes[..2], &('ا' as u16).to_le_bytes());
    }

    #[test]
    fn sync_skips_a_second_write_when_the_menu_already_matches() {
        let exe = Path::new(r"C:\Program Files\بيان\bayan.exe");
        let mut store = MemoryRegistry::default();
        assert!(!sync_with(&mut store, exe, false).unwrap());
        assert_eq!(store.writes, 0, "an absent menu is not deleted again");
        assert!(sync_with(&mut store, exe, true).unwrap());
        let writes = store.writes;
        assert!(writes > 0);
        assert!(!sync_with(&mut store, exe, true).unwrap());
        assert_eq!(store.writes, writes, "a matching menu is not rewritten");
        assert!(sync_with(&mut store, Path::new(r"D:\bayan.exe"), true).unwrap());
        assert!(store.writes > writes);
        assert!(sync_with(&mut store, exe, false).unwrap());
        let after_delete = store.writes;
        assert!(store.values.is_empty());
        assert!(!sync_with(&mut store, exe, false).unwrap());
        assert_eq!(store.writes, after_delete);
    }

    #[test]
    fn register_writes_both_hkcu_verbs_and_unregister_removes_them() {
        let exe = Path::new(r"C:\Program Files\بيان\bayan.exe");
        let command = context_menu_command(exe).unwrap();
        let mut store = MemoryRegistry::default();
        register_with(&mut store, exe).unwrap();

        for verb in VERBS {
            assert_eq!(
                store
                    .values
                    .get(&(verb.to_string(), String::new()))
                    .map(String::as_str),
                Some(MENU_LABEL),
                "{verb}"
            );
            assert_eq!(
                store
                    .values
                    .get(&(verb.to_string(), "Icon".to_string()))
                    .map(String::as_str),
                Some(r"C:\Program Files\بيان\bayan.exe"),
                "{verb}"
            );
            let command_key = format!(r"{verb}\command");
            assert_eq!(
                store
                    .values
                    .get(&(command_key, String::new()))
                    .map(String::as_str),
                Some(command.as_str()),
                "{verb}"
            );
        }
        assert!(
            store.values.keys().all(|(key, _)| {
                key.starts_with(r"Software\Classes\Directory\") && !key.contains("HKLM")
            }),
            "registration stays under the two HKCU directory verbs"
        );
        assert_eq!(store.values.len(), 6);

        unregister_with(&mut store).unwrap();
        assert!(store.values.is_empty());
        // removing an entry that is already gone is not an error
        unregister_with(&mut store).unwrap();
    }

    #[test]
    fn the_live_store_refuses_anything_outside_the_two_verbs() {
        let mut registry = LiveRegistry;
        for key in [
            r"HKLM\Software\Classes\Directory\shell\Bayan",
            r"Software\Classes\Directory\shell\Other",
            r"Software\Classes\*\shell\Bayan",
            r"Software\Classes\Directory\shell\Bayan\command\extra",
            r"SOFTWARE\Classes\Directory\shell\Bayan",
        ] {
            assert!(registry.set_value(key, "", "x").is_err(), "{key}");
            assert!(registry.get_value(key, "").is_err(), "{key}");
            assert!(registry.delete_tree(key).is_err(), "{key}");
        }
        // `command` is written, not deleted on its own: deleting the verb
        // takes it. This must fail before any real registry call.
        assert!(registry
            .delete_tree(r"Software\Classes\Directory\shell\Bayan\command")
            .is_err());
        assert!(allowed_value_key(r"Software\Classes\Directory\shell\Bayan"));
        assert!(allowed_value_key(
            r"Software\Classes\Directory\Background\shell\Bayan\command"
        ));
        assert!(allowed_delete_key(
            r"Software\Classes\Directory\Background\shell\Bayan"
        ));
        assert!(!allowed_delete_key(
            r"Software\Classes\Directory\shell\Bayan\command"
        ));
    }
}
