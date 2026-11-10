//! Taskbar Jump List: one task per detected shell, plus a new window and
//! "تشغيل كمسؤول".
//!
//! Tasks are `IShellLink`s handed to `ICustomDestinationList::AddUserTasks`.
//! The link's path is this executable and its arguments are only
//! `-p <profile name>`, `--new-window`, or `--elevated`. A profile's command
//! line is not an argument: the new process resolves `-p` itself.
//!
//! The COM call is behind [`JumpListStore`], so tests commit into a `Vec`
//! and never load shell32. The list is rewritten on startup when the
//! generated tasks differ from the last list this user committed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::{self, quote_windows_arg};
use crate::elevate;
use crate::shells::Shell;

/// Explicit AppUserModelID prefix. The exe path is hashed onto it so two
/// copies of Bayan do not share one taskbar button or one jump list.
pub const APP_USER_MODEL_PREFIX: &str = "Bayan.Terminal";

/// One jump-list task. `arguments` is the `SetArguments` string, never empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JumpTask {
    pub title: String,
    pub arguments: String,
}

/// A detected profile as the jump list needs it: the stable name (`-p`)
/// and the words the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTask {
    pub name: String,
    pub title: String,
}

impl ProfileTask {
    pub fn from_shell(shell: &Shell) -> Self {
        Self {
            name: shell.name.clone(),
            title: shell.label(),
        }
    }
}

/// `Bayan.Terminal.` plus 16 hex digits. No spaces: an AppUserModelID rejects them.
pub fn app_user_model_id(exe: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for unit in exe.encode_utf16() {
        hash ^= u64::from(unit);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{APP_USER_MODEL_PREFIX}.{hash:016x}")
}

/// Tasks in order: each safe profile, then "نافذة جديدة", then "تشغيل كمسؤول".
/// A name [`elevate::elevation_profile_name`] refuses is left out. The admin
/// task is `--elevated` alone, so it cannot carry a folder or a command.
pub fn jump_tasks(profiles: &[ProfileTask]) -> Vec<JumpTask> {
    let mut tasks = Vec::new();
    let mut seen = Vec::new();
    for profile in profiles {
        if seen.iter().any(|name: &String| name == &profile.name) {
            continue;
        }
        let Some(arguments) = profile_arguments(&profile.name) else {
            continue;
        };
        seen.push(profile.name.clone());
        let title = if profile.title.trim().is_empty() {
            profile.name.clone()
        } else {
            profile.title.clone()
        };
        tasks.push(JumpTask { title, arguments });
    }
    tasks.push(JumpTask {
        title: crate::lang::tr("نافذة جديدة", "New window").to_string(),
        arguments: "--new-window".to_string(),
    });
    tasks.push(JumpTask {
        title: crate::lang::tr("تشغيل كمسؤول", "Run as administrator").to_string(),
        arguments: "--elevated".to_string(),
    });
    tasks
}

/// `-p <name>` quoted for `CommandLineToArgvW`. `None` when the name must
/// not be an argument (a path, a quote, a flag).
pub fn profile_arguments(name: &str) -> Option<String> {
    let name = elevate::elevation_profile_name(name).ok()?;
    Some(format!("-p {}", quote_windows_arg(name)))
}

/// The command line a task stands for. The exe is always quoted.
#[cfg(test)]
fn task_command_line(exe: &str, task: &JumpTask) -> String {
    format!("\"{exe}\" {}", task.arguments)
}

/// Where a generated list is remembered so the next startup can tell that
/// the profiles changed. The file is a cache of what we last committed. It
/// is never the source of the tasks that get committed.
pub fn tasks_changed(previous: &[JumpTask], next: &[JumpTask]) -> bool {
    previous != next
}

/// Replace the task list when `next` differs from `previous`. Returns
/// whether a commit happened. `store` receives `next` only.
pub fn apply(
    store: &mut impl JumpListStore,
    previous: &[JumpTask],
    next: &[JumpTask],
    app_id: &str,
) -> Result<bool, String> {
    if !tasks_changed(previous, next) {
        return Ok(false);
    }
    store.commit(app_id, next)?;
    Ok(true)
}

pub trait JumpListStore {
    fn commit(&mut self, app_id: &str, tasks: &[JumpTask]) -> Result<(), String>;
}

/// Startup entry. Profiles come from this process's own `shells::detect`.
/// A failure is logged by the caller; the terminal still opens.
pub fn refresh(profiles: &[Shell], persist_stamp: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = cli::exe_display(&exe)?;
    let next = jump_tasks(
        &profiles
            .iter()
            .map(ProfileTask::from_shell)
            .collect::<Vec<_>>(),
    );
    let path = crate::config::bayan_dir().join("jumplist.stamp");
    // An elevated process does not create or replace anything under ~/.bayan.
    // The comparison file is skipped; the task list is still committed.
    let previous = if persist_stamp {
        read_stamp(&path)
    } else {
        Vec::new()
    };
    let app_id = app_user_model_id(&exe);
    let mut live = LiveDestinations { exe };
    if apply(&mut live, &previous, &next, &app_id)? && persist_stamp {
        let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
        crate::persist::write(&path, &bytes).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn read_stamp(path: &Path) -> Vec<JumpTask> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if text.len() > 64 * 1024 {
        return Vec::new();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

/// Set the process AppUserModelID before any window exists.
pub fn set_process_app_id(app_id: &str) -> Result<(), String> {
    let wide = wide(app_id);
    // SAFETY: `wide` is NUL-terminated and lives for the call.
    unsafe {
        windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::PCWSTR(
            wide.as_ptr(),
        ))
    }
    .map_err(|error| error.to_string())
}

pub fn set_window_app_id(
    hwnd: windows_sys::Win32::Foundation::HWND,
    app_id: &str,
) -> Result<(), String> {
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyStore, SHGetPropertyStoreForWindow,
    };

    // SAFETY: `hwnd` is the window winit just created. The string variant is
    // CoTaskMemAlloc'd and PropVariantClear'd. The store drops here.
    unsafe {
        let store: IPropertyStore =
            SHGetPropertyStoreForWindow(windows::Win32::Foundation::HWND(hwnd as _))
                .map_err(|error| error.to_string())?;
        let mut value = lpwstr_variant(app_id)?;
        let set = store.SetValue(&PKEY_AppUserModel_ID, &value);
        let _ = PropVariantClear(&mut value);
        set.map_err(|error| error.to_string())?;
        store.Commit().map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct LiveDestinations {
    exe: String,
}

impl JumpListStore for LiveDestinations {
    fn commit(&mut self, app_id: &str, tasks: &[JumpTask]) -> Result<(), String> {
        commit_destinations(&self.exe, app_id, tasks)
    }
}

fn commit_destinations(exe: &str, app_id: &str, tasks: &[JumpTask]) -> Result<(), String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_Title};
    use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, ShellLink,
    };

    // SAFETY: a second STA init on this thread returns S_FALSE, which is
    // success. The apartment stays initialized for the process.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|error| error.to_string())?;
    }

    let exe_w = wide(exe);
    let app_w = wide(app_id);
    // SAFETY: the COM objects are released on drop. Every wide buffer is
    // NUL-terminated and outlives the call that reads it. String variants
    // are CoTaskMemAlloc'd and PropVariantClear'd.
    unsafe {
        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| error.to_string())?;
        list.SetAppID(PCWSTR(app_w.as_ptr()))
            .map_err(|error| error.to_string())?;
        let mut slots = 0u32;
        let removed: IObjectArray = list
            .BeginList(&mut slots)
            .map_err(|error| error.to_string())?;
        drop(removed);

        let collection: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| error.to_string())?;
        for task in tasks {
            if task.arguments.is_empty() {
                return Err("a jump list task needs arguments".into());
            }
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| error.to_string())?;
            link.SetPath(PCWSTR(exe_w.as_ptr()))
                .map_err(|error| error.to_string())?;
            let args = wide(&task.arguments);
            link.SetArguments(PCWSTR(args.as_ptr()))
                .map_err(|error| error.to_string())?;
            link.SetIconLocation(PCWSTR(exe_w.as_ptr()), 0)
                .map_err(|error| error.to_string())?;
            let store: IPropertyStore = link.cast().map_err(|error| error.to_string())?;
            let mut title_value = lpwstr_variant(&task.title)?;
            let title_set = store.SetValue(&PKEY_Title, &title_value);
            let _ = PropVariantClear(&mut title_value);
            title_set.map_err(|error| error.to_string())?;
            let mut id_value = lpwstr_variant(app_id)?;
            let id_set = store.SetValue(&PKEY_AppUserModel_ID, &id_value);
            let _ = PropVariantClear(&mut id_value);
            id_set.map_err(|error| error.to_string())?;
            store.Commit().map_err(|error| error.to_string())?;
            collection
                .AddObject(&link)
                .map_err(|error| error.to_string())?;
        }
        let array: IObjectArray = collection.cast().map_err(|error| error.to_string())?;
        list.AddUserTasks(&array)
            .map_err(|error| error.to_string())?;
        list.CommitList().map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// `VT_LPWSTR` allocated with `CoTaskMemAlloc`, so `PropVariantClear` can free it.
fn lpwstr_variant(
    text: &str,
) -> Result<windows::Win32::System::Com::StructuredStorage::PROPVARIANT, String> {
    use windows::Win32::System::Com::CoTaskMemAlloc;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_LPWSTR;

    let units = wide(text);
    let bytes = units.len() * std::mem::size_of::<u16>();
    // SAFETY: CoTaskMemAlloc returns a block of `bytes` or null. The copy
    // stays inside that block. The variant points at it; the caller frees it
    // with PropVariantClear, which is the matching free.
    unsafe {
        let mem = CoTaskMemAlloc(bytes).cast::<u16>();
        if mem.is_null() {
            return Err("out of memory for a jump list string".into());
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), mem, units.len());
        let mut prop = PROPVARIANT::default();
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = VT_LPWSTR;
        inner.Anonymous.pwszVal = windows::core::PWSTR(mem);
        Ok(prop)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Memory {
        commits: Vec<(String, Vec<JumpTask>)>,
    }

    impl JumpListStore for Memory {
        fn commit(&mut self, app_id: &str, tasks: &[JumpTask]) -> Result<(), String> {
            self.commits.push((app_id.to_string(), tasks.to_vec()));
            Ok(())
        }
    }

    fn profiles() -> Vec<ProfileTask> {
        vec![
            ProfileTask {
                name: "PowerShell سريع".into(),
                title: "PowerShell سريع".into(),
            },
            ProfileTask {
                name: "Command Prompt".into(),
                title: "Command Prompt".into(),
            },
            ProfileTask {
                name: "WSL · Ubuntu".into(),
                title: "WSL · Ubuntu".into(),
            },
            ProfileTask {
                name: r"C:\Windows\System32\cmd.exe".into(),
                title: "not a profile".into(),
            },
        ]
    }

    #[test]
    fn tasks_quote_arabic_and_spaces_and_never_carry_a_command() {
        let tasks = jump_tasks(&profiles());
        assert_eq!(tasks[0].arguments, "-p \"PowerShell سريع\"");
        assert_eq!(tasks[1].arguments, "-p \"Command Prompt\"");
        assert_eq!(tasks[2].arguments, "-p \"WSL · Ubuntu\"");
        assert!(tasks.iter().any(|task| task.arguments == "--new-window"));
        assert!(tasks.iter().any(|task| task.arguments == "--elevated"));
        assert_eq!(
            tasks.last().map(|task| task.arguments.as_str()),
            Some("--elevated")
        );
        for task in &tasks {
            assert!(!task.arguments.contains("cmd.exe"), "{}", task.arguments);
            assert!(!task.arguments.contains("System32"), "{}", task.arguments);
            assert!(!task.arguments.contains("-- "), "{}", task.arguments);
            assert!(!task.arguments.is_empty());
        }
        let line = task_command_line(r"D:\مجلد بيان\bayan.exe", &tasks[0]);
        assert_eq!(line, "\"D:\\مجلد بيان\\bayan.exe\" -p \"PowerShell سريع\"");
    }

    #[test]
    fn the_list_is_committed_only_when_profiles_change() {
        let first = jump_tasks(&profiles()[..2]);
        let again = jump_tasks(&profiles()[..2]);
        let changed = jump_tasks(&profiles()[..3]);
        let mut store = Memory::default();
        let id = app_user_model_id(r"D:\مجلد بيان\bayan.exe");
        assert!(id.starts_with("Bayan.Terminal."));
        assert!(!id.contains(' '));
        assert_ne!(id, app_user_model_id(r"C:\bayan.exe"));
        assert!(apply(&mut store, &[], &first, &id).unwrap());
        assert!(!apply(&mut store, &first, &again, &id).unwrap());
        assert!(apply(&mut store, &first, &changed, &id).unwrap());
        assert_eq!(store.commits.len(), 2);
        assert_eq!(store.commits[1].1, changed);
    }

    #[test]
    fn a_stamp_is_never_what_gets_committed() {
        let planted = r#"[{"title":"evil","arguments":"-- cmd.exe /c whoami"}]"#;
        let previous: Vec<JumpTask> = serde_json::from_str(planted).unwrap();
        let next = jump_tasks(&profiles()[..1]);
        let mut store = Memory::default();
        apply(&mut store, &previous, &next, "Bayan.Terminal.test").unwrap();
        let committed = &store.commits[0].1;
        assert_eq!(committed, &next);
        assert!(committed
            .iter()
            .all(|task| !task.arguments.contains("whoami")));
        assert!(committed
            .iter()
            .all(|task| !task.arguments.contains("cmd.exe")));
    }
}
