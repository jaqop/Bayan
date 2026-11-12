//! The tab-bar shell menu.
//!
//! A small arrow beside the tabs opens a menu of the shells
//! [`crate::shells::detect`] found, plus any extra profile that resolves
//! through the same lookup `-p` uses (`find_profile`, and
//! `find_profile_by_name` when the window is elevated). A profile with an
//! empty name, a control or bidi character, or a command that is not one
//! of those installed shells is skipped and reported. It never becomes a
//! row, so it cannot be spawned and it cannot stop the rest of the menu.
//!
//! SSH and the mux are offered only when the caller says they exist and
//! the window is not elevated. The elevated bit is the one
//! [`crate::elevate::process_is_elevated`] already computed for this
//! process; this module does not ask the token again. Choosing a shell
//! row names that shell's command. The tab is started by `Session::spawn`,
//! which is the path that pins an elevated Git Bash `HOME` and sets its
//! integration environment.

use crate::lang::tr;
use crate::shells::{self, Shell};

/// What a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A new tab running `command`.
    Shell,
    /// Open the SSH host menu. Never present when the window is elevated.
    Ssh,
    /// Open the detached-mux menu. Never present when the window is elevated.
    Mux,
}

/// One menu row. `announce` is the shell name a screen reader says.
/// `label` is what the menu draws, with the default mark when it applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub label: String,
    pub announce: String,
    pub command: String,
}

/// A profile the menu could not show, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skip {
    pub name: String,
    pub reason: &'static str,
}

/// SSH and mux rows the caller would offer in an ordinary window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offers {
    pub ssh: bool,
    pub mux: bool,
}

/// The built menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Menu {
    pub rows: Vec<Row>,
    pub skipped: Vec<Skip>,
}

/// Where the arrow sits in the tab bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrowBox {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A key the menu handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
}

/// What that key does. `Move` is in list order: positive is the next row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Move(i32),
    Pick,
    Close,
}

/// Where focus goes when the menu closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusAfter {
    /// Esc: the arrow button, so the menu can be opened again.
    Arrow,
    /// Enter on a row: the new tab.
    Tab,
}

/// The button's accessible name.
pub fn button_label() -> &'static str {
    tr("الطرفيات", "Shells")
}

/// The menu's accessible name.
pub fn menu_label() -> &'static str {
    tr("اختر الطرفية", "Choose a shell")
}

/// Alt+Down with no Ctrl and no Shift. The physical Down key, so an
/// Arabic layout still opens the menu.
pub fn alt_down_opens(alt: bool, ctrl: bool, shift: bool, down: bool) -> bool {
    alt && !ctrl && !shift && down
}

/// Up and Down move through the list in both languages: the rows are
/// stacked, and Down is the next row below. Left and Right follow the
/// reading direction, the same swap copy mode uses on an RTL cell.
/// Enter picks the highlighted row. Esc closes.
pub fn menu_action(key: MenuKey, rtl: bool) -> MenuAction {
    let forward = if rtl { -1 } else { 1 };
    match key {
        MenuKey::Down => MenuAction::Move(1),
        MenuKey::Up => MenuAction::Move(-1),
        MenuKey::Right => MenuAction::Move(forward),
        MenuKey::Left => MenuAction::Move(-forward),
        MenuKey::Enter => MenuAction::Pick,
        MenuKey::Escape => MenuAction::Close,
    }
}

/// Esc returns to the arrow. A pick moves to the tab that opened.
pub fn focus_after(picked: bool) -> FocusAfter {
    if picked {
        FocusAfter::Tab
    } else {
        FocusAfter::Arrow
    }
}

/// The arrow beside the tab run, just after the last tab.
///
/// The tab strip is not mirrored: caption buttons stay on the right and
/// the group chip owns the left, and the first tab starts at `tabs_x0`,
/// so there is never a free gap before it. Both languages use the gap
/// after the last tab. The arrow never covers a tab or the ‹ › + cluster.
pub fn arrow_box(
    last_end: i32,
    controls_x: i32,
    size: i32,
    gap: i32,
    bar_h: i32,
) -> Option<ArrowBox> {
    if size <= 0 || bar_h <= 4 || controls_x <= 0 {
        return None;
    }
    let after = last_end.saturating_add(gap);
    let x = if after.saturating_add(size) <= controls_x {
        after
    } else {
        let squeezed = controls_x - size;
        if squeezed < last_end {
            return None;
        }
        squeezed
    };
    if x < last_end || x.saturating_add(size) > controls_x {
        return None;
    }
    let h = size.min(bar_h - 4).max(1);
    let y = ((bar_h - h) / 2).max(0);
    Some(ArrowBox { x, y, w: size, h })
}

/// The menu. `installed` is what detection found. `profiles` is any extra
/// profile record; a bad one is listed in [`Menu::skipped`] and omitted.
/// Until detection has reported (both slices empty) the two shells every
/// Windows has stand in, the same stand-in the right-click menu used.
pub fn build(
    installed: &[Shell],
    profiles: &[Shell],
    default_command: &str,
    elevated: bool,
    offers: Offers,
) -> Menu {
    let mut skipped = Vec::new();
    let mut rows = Vec::new();
    if installed.is_empty() && profiles.is_empty() {
        push_shell(
            &mut rows,
            "PowerShell",
            "PowerShell",
            "powershell.exe",
            default_command,
        );
        push_shell(
            &mut rows,
            "Command Prompt",
            "Command Prompt",
            "cmd.exe",
            default_command,
        );
    } else {
        let mut accepted: Vec<Shell> = Vec::new();
        for shell in installed {
            match fault(shell) {
                Some(reason) => skipped.push(skip_of(shell, reason)),
                None => {
                    if !rows.iter().any(|row| row.command == shell.command) {
                        let name = shell.label();
                        push_shell(&mut rows, &name, &name, &shell.command, default_command);
                    }
                    accepted.push(shell.clone());
                }
            }
        }
        for profile in profiles {
            if let Some(reason) = fault(profile) {
                skipped.push(skip_of(profile, reason));
                continue;
            }
            let resolved = if elevated {
                shells::find_profile_by_name(&accepted, profile.name.trim())
            } else {
                shells::find_profile(&accepted, profile.name.trim())
                    .or_else(|| shells::find_profile(&accepted, profile.command.trim()))
            };
            let Some(shell) = resolved else {
                let reason = if elevated {
                    "not an installed profile name"
                } else {
                    "not installed"
                };
                skipped.push(skip_of(profile, reason));
                continue;
            };
            if rows.iter().any(|row| row.command == shell.command) {
                continue;
            }
            let name = if profile.name.trim().is_empty() {
                shell.label()
            } else {
                profile.name.trim().to_string()
            };
            push_shell(
                &mut rows,
                &name,
                &shell.label(),
                &shell.command,
                default_command,
            );
        }
    }
    // An elevated window does not offer SSH or the mux. The caller may
    // still say they exist; the bit drops them.
    if !elevated && offers.ssh {
        rows.push(Row {
            kind: Kind::Ssh,
            label: crate::ssh::label_open().to_string(),
            announce: crate::ssh::label_open().to_string(),
            command: String::new(),
        });
    }
    if !elevated && offers.mux {
        rows.push(Row {
            kind: Kind::Mux,
            label: crate::mux::label_reattach().to_string(),
            announce: crate::mux::label_reattach().to_string(),
            command: String::new(),
        });
    }
    Menu { rows, skipped }
}

fn push_shell(rows: &mut Vec<Row>, label: &str, announce: &str, command: &str, default: &str) {
    let mark = tr("الافتراضية", "default");
    let marked = if command == default {
        format!("{label} · {mark}")
    } else {
        label.to_string()
    };
    rows.push(Row {
        kind: Kind::Shell,
        label: marked,
        announce: announce.to_string(),
        command: command.to_string(),
    });
}

fn skip_of(shell: &Shell, reason: &'static str) -> Skip {
    let name = shell.name.trim();
    let name = if name.is_empty() {
        shell.command.trim()
    } else {
        name
    };
    let name = if name.is_empty() {
        "(empty)".to_string()
    } else {
        name.to_string()
    };
    Skip { name, reason }
}

/// Why this record cannot be a row. `None` means it is well-formed; it
/// may still fail to match an installed shell.
fn fault(shell: &Shell) -> Option<&'static str> {
    let name = shell.name.trim();
    let command = shell.command.trim();
    if name.is_empty() && command.is_empty() {
        return Some("empty profile");
    }
    if name.is_empty() || command.is_empty() {
        return Some("empty name or command");
    }
    if name.chars().any(|c| c.is_control()) || command.chars().any(|c| c.is_control()) {
        return Some("control character");
    }
    if has_bidi(name) || has_bidi(command) {
        return Some("bidi override");
    }
    None
}

fn has_bidi(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'
                | '\u{202b}'
                | '\u{202c}'
                | '\u{202d}'
                | '\u{202e}'
                | '\u{2066}'
                | '\u{2067}'
                | '\u{2068}'
                | '\u{2069}'
        )
    })
}

/// A tab the menu opened. SSH and mux rows do not open a shell tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedTab {
    pub command: String,
    pub announce: String,
}

/// Choosing `index` appends a tab that runs that row's shell. The command
/// is the installed shell's command, copied, not rebuilt.
pub fn open_tab(rows: &[Row], index: usize, tabs: &mut Vec<OpenedTab>) -> bool {
    let Some(row) = rows.get(index) else {
        return false;
    };
    if row.kind != Kind::Shell || row.command.is_empty() {
        return false;
    }
    tabs.push(OpenedTab {
        command: row.command.clone(),
        announce: row.announce.clone(),
    });
    true
}

/// Write the skips once, when the menu opens. Building the rows for a
/// frame must not log them again.
pub fn log_skips(skipped: &[Skip]) {
    for skip in skipped {
        let name = &skip.name;
        let reason = skip.reason;
        eprintln!("bayan: shell menu skipped profile {name:?}: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{self, Lang};

    fn shell(name: &str, command: &str) -> Shell {
        Shell {
            name: name.to_string(),
            command: command.to_string(),
        }
    }

    fn installed() -> Vec<Shell> {
        vec![
            shell("PowerShell", "powershell.exe"),
            shell("Command Prompt", "cmd.exe"),
            shell("Git Bash", r"C:\Git\usr\bin\bash.exe --login -i"),
            shell("WSL · Ubuntu", r"C:\Windows\System32\wsl.exe -d Ubuntu"),
        ]
    }

    #[test]
    fn the_menu_lists_installed_shells_skips_a_bad_profile_and_hides_ssh_when_elevated() {
        let _lang = lang_guard(Lang::Ar);
        let profiles = vec![
            shell("PowerShell", "powershell.exe"),
            shell("عمل", "cmd.exe"),
            shell("calc", "calc.exe"),
            shell("bad\nname", "cmd.exe"),
            shell("override", "cmd.exe\u{202e}"),
            shell("", ""),
            shell("powershell.exe", "powershell.exe"),
        ];
        let plain = build(
            &installed(),
            &profiles,
            "cmd.exe",
            false,
            Offers {
                ssh: true,
                mux: true,
            },
        );
        let commands: Vec<&str> = plain.rows.iter().map(|r| r.command.as_str()).collect();
        assert!(commands.contains(&"powershell.exe"));
        assert!(commands.contains(&"cmd.exe"));
        assert!(commands.contains(&r"C:\Git\usr\bin\bash.exe --login -i"));
        assert!(commands.contains(&r"C:\Windows\System32\wsl.exe -d Ubuntu"));
        assert!(!commands.iter().any(|c| c.contains("calc")));
        assert_eq!(
            commands.iter().filter(|c| **c == "cmd.exe").count(),
            1,
            "a profile of an installed shell is not a second row"
        );
        assert!(plain.rows.iter().any(|r| r.kind == Kind::Ssh));
        assert!(plain.rows.iter().any(|r| r.kind == Kind::Mux));
        assert!(plain
            .rows
            .iter()
            .any(|r| r.announce == "Command Prompt" && r.label.contains("الافتراضية")));
        assert_eq!(button_label(), "الطرفيات");
        assert_eq!(menu_label(), "اختر الطرفية");
        let reasons: Vec<&str> = plain.skipped.iter().map(|s| s.reason).collect();
        assert!(reasons.contains(&"not installed"), "{:?}", plain.skipped);
        assert!(
            reasons.contains(&"control character"),
            "{:?}",
            plain.skipped
        );
        assert!(reasons.contains(&"bidi override"), "{:?}", plain.skipped);
        assert!(reasons.contains(&"empty profile"), "{:?}", plain.skipped);
        // Git Bash is absent when it is not installed.
        let no_bash: Vec<Shell> = installed()
            .into_iter()
            .filter(|s| s.name != "Git Bash")
            .collect();
        let without = build(
            &no_bash,
            &[],
            "powershell.exe",
            false,
            Offers {
                ssh: false,
                mux: false,
            },
        );
        assert!(without
            .rows
            .iter()
            .all(|r| !r.announce.contains("Git Bash")));
        assert!(without.rows.iter().any(|r| r.command == "powershell.exe"));

        let elevated = build(
            &installed(),
            &profiles,
            "powershell.exe",
            true,
            Offers {
                ssh: true,
                mux: true,
            },
        );
        assert!(elevated.rows.iter().all(|r| r.kind == Kind::Shell));
        assert!(elevated
            .rows
            .iter()
            .all(|r| r.kind != Kind::Ssh && r.kind != Kind::Mux));
        assert!(elevated
            .skipped
            .iter()
            .any(|s| s.name == "powershell.exe" && s.reason == "not an installed profile name"));
        assert!(elevated.rows.iter().any(|r| r.announce == "PowerShell"));
        // A corrupt installed entry does not remove the others.
        let mut broken = installed();
        broken.push(shell("x\u{202e}y", "cmd.exe /c nope"));
        let still = build(
            &broken,
            &[],
            "powershell.exe",
            true,
            Offers {
                ssh: true,
                mux: true,
            },
        );
        assert!(still.rows.len() >= 4);
        assert!(still.skipped.iter().any(|s| s.reason == "bidi override"));
        assert!(still.rows.iter().all(|r| r.kind == Kind::Shell));

        // The strip is not mirrored, so the arrow follows the last tab
        // in both languages. A gap that cannot hold it hides the arrow.
        let beside = arrow_box(200, 800, 24, 6, 40).unwrap();
        assert_eq!(beside.x, 206);
        assert!(beside.x >= 200 && beside.x + beside.w <= 800);
        assert!(arrow_box(790, 800, 24, 6, 40).is_none());

        assert!(alt_down_opens(true, false, false, true));
        assert!(!alt_down_opens(true, true, false, true));
        assert!(!alt_down_opens(false, false, false, true));
        assert_eq!(menu_action(MenuKey::Down, true), MenuAction::Move(1));
        assert_eq!(menu_action(MenuKey::Up, true), MenuAction::Move(-1));
        assert_eq!(menu_action(MenuKey::Left, true), MenuAction::Move(1));
        assert_eq!(menu_action(MenuKey::Right, true), MenuAction::Move(-1));
        assert_eq!(menu_action(MenuKey::Left, false), MenuAction::Move(-1));
        assert_eq!(menu_action(MenuKey::Right, false), MenuAction::Move(1));
        assert_eq!(menu_action(MenuKey::Down, false), MenuAction::Move(1));
        assert_eq!(menu_action(MenuKey::Enter, true), MenuAction::Pick);
        assert_eq!(menu_action(MenuKey::Escape, true), MenuAction::Close);
        assert_eq!(focus_after(false), FocusAfter::Arrow);
        lang::set(Lang::En);
        assert_eq!(button_label(), "Shells");
        assert_eq!(menu_label(), "Choose a shell");
        let en = build(
            &installed(),
            &[],
            "cmd.exe",
            false,
            Offers {
                ssh: false,
                mux: false,
            },
        );
        assert!(en.rows.iter().any(|r| r.label.contains("default")));
        lang::set(Lang::Ar);
    }

    #[test]
    fn choosing_a_row_opens_a_tab_with_that_shell() {
        let _lang = lang_guard(Lang::Ar);
        let menu = build(
            &installed(),
            &[shell("planted", "calc.exe")],
            "powershell.exe",
            true,
            Offers {
                ssh: true,
                mux: true,
            },
        );
        assert!(menu.rows.iter().all(|r| r.kind == Kind::Shell));
        assert!(menu.skipped.iter().any(|s| s.name == "planted"));
        let index = menu
            .rows
            .iter()
            .position(|r| r.announce == "Git Bash")
            .expect("Git Bash is installed");
        // RTL: Left moves to the next row, then we step back onto Git Bash
        // with Right and pick it. Down would also move forward.
        let mut sel = index;
        match menu_action(MenuKey::Left, true) {
            MenuAction::Move(step) => {
                sel = crate::keyroute::cycle(sel, step, menu.rows.len());
            }
            other => panic!("Left should move, got {other:?}"),
        }
        assert_ne!(sel, index);
        match menu_action(MenuKey::Right, true) {
            MenuAction::Move(step) => {
                sel = crate::keyroute::cycle(sel, step, menu.rows.len());
            }
            other => panic!("Right should move, got {other:?}"),
        }
        assert_eq!(sel, index);
        let mut tabs = Vec::new();
        assert!(open_tab(&menu.rows, sel, &mut tabs));
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].announce, "Git Bash");
        assert_eq!(tabs[0].command, r"C:\Git\usr\bin\bash.exe --login -i");
        assert!(!tabs[0].command.contains("calc"));
        // The command the tab stores is the one Session::spawn hands to
        // shell integration. Elevated Git Bash pins HOME and sets the
        // readline environment there; this menu does not build its own.
        let plan = crate::shellint::plan(&tabs[0].command, true, true, "nonce-1").unwrap();
        assert!(plan.elevated);
        assert_eq!(
            plan.path_env,
            vec![("HOME".to_string(), "home".to_string())]
        );
        assert_eq!(
            plan.env
                .iter()
                .find(|(k, _)| k == "INPUTRC")
                .map(|(_, v)| v.as_str()),
            Some("/dev/null")
        );
        assert_eq!(focus_after(true), FocusAfter::Tab);
        assert!(!open_tab(&menu.rows, menu.rows.len(), &mut tabs));
        assert_eq!(tabs.len(), 1);
    }

    struct LangGuard(Lang);
    impl Drop for LangGuard {
        fn drop(&mut self) {
            lang::set(self.0);
        }
    }
    fn lang_guard(lang: Lang) -> LangGuard {
        let prev = lang::get();
        lang::set(lang);
        LangGuard(prev)
    }
}
