//! Native notifications without WinRT: a Shell_NotifyIcon tray icon whose
//! NIF_INFO balloons render as real Windows 10/11 toast notifications
//! (Action Center included, quiet-hours respected). Zero COM machinery —
//! everything stays on windows-sys, which Bayan already carries for the
//! quake hotkey and window alpha. The tray icon doubles as a summon
//! target: clicking it (or a toast) raises the window.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP,
    NIIF_INFO, NIIF_RESPECT_QUIET_TIME, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};

/// The tray callback message (WM_APP+2); the msg hook watches it so a
/// toast/tray click can summon the window.
pub const TRAY_MSG: u32 = 0x8000 + 2;
/// Callback events that mean "the user clicked us".
pub const NIN_BALLOONUSERCLICK: u32 = 0x0405;
pub const WM_LBUTTONUP: u32 = 0x0202;
const TRAY_UID: u32 = 1;

fn has_arabic(s: &str) -> bool {
    s.chars().any(|c| matches!(c as u32,
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF
        | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF))
}

/// The balloon/toast text renderer lays runs out in LOGICAL order,
/// left-to-right — no BiDi run reordering, and it ignores RLM/RLO
/// direction marks (verified empirically on Windows 11: word order came
/// out reversed while letter shaping stayed correct, with or without a
/// U+200F prefix). So for Arabic-bearing strings we hand it the VISUAL
/// word order — reverse the token sequence — and it comes out readable.
/// The same visual-order philosophy as Claude mode, aimed the other way.
fn visual_order(s: &str) -> String {
    if !has_arabic(s) {
        return s.to_string();
    }
    s.split(' ').rev().collect::<Vec<_>>().join(" ")
}

/// &str -> fixed UTF-16 buffer, truncated with a NUL guaranteed.
fn wstr<const N: usize>(s: &str) -> [u16; N] {
    let mut out = [0u16; N];
    for (slot, ch) in out.iter_mut().zip(s.encode_utf16().take(N - 1)) {
        *slot = ch;
    }
    out
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = TRAY_UID;
    d
}

/// Add the tray icon once at startup (idempotent enough: a second NIM_ADD
/// for the same (hwnd, uid) just fails quietly). True when the shell
/// accepted the icon.
pub fn init(hwnd: HWND) -> bool {
    let mut d = base(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    d.uCallbackMessage = TRAY_MSG;
    d.hIcon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
    d.szTip = wstr("Bayan — بيان");
    unsafe {
        let ok = Shell_NotifyIconW(NIM_ADD, &d) != 0;
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &d);
        ok
    }
}

/// Show a toast. NIIF_RESPECT_QUIET_TIME defers to focus assist — a
/// notification that interrupts a presentation is worse than none. True
/// when the shell accepted it (do-not-disturb can still hold the banner
/// and file it in the notification center instead).
pub fn show(hwnd: HWND, title: &str, body: &str) -> bool {
    let mut d = base(hwnd);
    d.uFlags = NIF_INFO;
    d.dwInfoFlags = NIIF_INFO | NIIF_RESPECT_QUIET_TIME;
    d.szInfoTitle = wstr(&visual_order(title));
    d.szInfo = wstr(&visual_order(body));
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) != 0 }
}

/// Remove the tray icon before exit — orphaned icons ghost in the tray
/// until the next hover sweeps them.
pub fn remove(hwnd: HWND) {
    let d = base(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &d) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balloon_text_gets_visual_word_order() {
        // Arabic: token sequence reverses (the renderer lays them LTR)
        assert_eq!(visual_order("اكتمل الأمر"), "الأمر اكتمل");
        // mixed: the LTR token rides along, its inside untouched
        assert_eq!(visual_order("إشعار تجريبي (M19)"), "(M19) تجريبي إشعار");
        // pure LTR passes through — reversing "cargo build" would wrong it
        assert_eq!(visual_order("cargo build"), "cargo build");
        // and the reversal is an involution on the Arabic strings we send
        assert_eq!(visual_order(&visual_order("اكتمل الأمر")), "اكتمل الأمر");
    }

    #[test]
    fn wstr_truncates_and_terminates() {
        let w: [u16; 8] = wstr("hello");
        assert_eq!(&w[..6], &[104, 101, 108, 108, 111, 0]);
        // longer than the buffer: truncated, last slot stays NUL
        let w: [u16; 4] = wstr("abcdefgh");
        assert_eq!(w, [97, 98, 99, 0]);
        // Arabic survives the UTF-16 trip
        let w: [u16; 8] = wstr("بيان");
        assert_eq!(&w[..5], &[0x628, 0x64a, 0x627, 0x646, 0]);
    }
}
