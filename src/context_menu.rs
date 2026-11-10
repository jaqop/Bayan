//! Per-user Explorer "Open Bayan here".
//!
//! Two verbs, both under `HKEY_CURRENT_USER\Software\Classes`:
//! `Directory\shell\Bayan` (a folder icon) and
//! `Directory\Background\shell\Bayan` (the folder background). The command
//! is `"<exe>" -d "%V"`, which is the folder switch `#45` already accepts.
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

pub const MENU_LABEL: &str = "Open Bayan here";

pub const REGISTERED_NOTE: &str = "\
Registered \"Open Bayan here\" for the current user.
Windows 11 shows it under \"Show more options\" until Bayan has package identity (MSIX).";

pub const REMOVED_NOTE: &str = "Removed \"Open Bayan here\" from the current user's Explorer menu.";

const VERBS: [&str; 2] = [
    r"Software\Classes\Directory\shell\Bayan",
    r"Software\Classes\Directory\Background\shell\Bayan",
];

/// `name` empty is the key's default value. Keys are relative to HKCU.
pub trait RegistryStore {
    fn set_value(&mut self, key: &str, name: &str, value: &str) -> Result<(), String>;
    fn delete_tree(&mut self, key: &str) -> Result<(), String>;
}

/// `"<exe>" -d "%V"`. The exe is always quoted, including when it has no spaces.
pub fn context_menu_command(exe: &Path) -> Result<String, String> {
    let path = exe_display(exe)?;
    Ok(format!("\"{path}\" -d \"%V\""))
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

/// Installer and the settings file call this. `enabled` writes the two HKCU
/// verbs; `false` deletes them. The command points at this process's exe.
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
    fn the_command_string_quotes_spaces_and_arabic() {
        let spaced = context_menu_command(Path::new(r"C:\Program Files\Bayan\bayan.exe")).unwrap();
        assert_eq!(spaced, r#""C:\Program Files\Bayan\bayan.exe" -d "%V""#);
        let arabic = context_menu_command(Path::new(r"D:\مجلد بيان\bayan.exe")).unwrap();
        assert_eq!(arabic, "\"D:\\مجلد بيان\\bayan.exe\" -d \"%V\"");
        let plain = context_menu_command(Path::new(r"C:\bayan.exe")).unwrap();
        assert_eq!(plain, r#""C:\bayan.exe" -d "%V""#);
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
