//! Save a pane's output to a text file.
//!
//! The file goes to `Documents\Bayan\bayan-<date>-<time>.txt`, under the
//! Documents folder the shell reports (it may be redirected to OneDrive),
//! not `%USERPROFILE%\Documents`. No dialog: the name is minted here, an
//! existing file is never overwritten, and the window says where it went.
//! Naming, trimming and the no-overwrite write are pure and tested on a
//! temp directory; the two Win32 reads sit at the bottom.

use std::io::Write;
use std::path::{Path, PathBuf};

/// A wall-clock moment, local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

/// `bayan-20261113-142233.txt`, or `…-2.txt` for a second save that second.
pub fn file_name(at: Stamp, attempt: u32) -> String {
    let base = format!(
        "bayan-{:04}{:02}{:02}-{:02}{:02}{:02}",
        at.year, at.month, at.day, at.hour, at.minute, at.second
    );
    match attempt {
        0 | 1 => format!("{base}.txt"),
        n => format!("{base}-{n}.txt"),
    }
}

/// The text as it is written: trailing spaces off every line, the blank
/// rows below the last output dropped, one final newline, and Windows line
/// ends so Notepad on any Windows version shows lines. `None` is a pane
/// with nothing on it.
pub fn file_text(grid: &str) -> Option<String> {
    let lines: Vec<&str> = grid.lines().map(str::trim_end).collect();
    let used = lines.iter().rposition(|l| !l.is_empty())? + 1;
    let mut out = String::with_capacity(grid.len() + used);
    for line in &lines[..used] {
        out.push_str(line);
        out.push_str("\r\n");
    }
    Some(out)
}

/// How many names are tried before giving up on a crowded second.
const ATTEMPTS: u32 = 50;

/// Write `text` to a new file in `dir` (created if missing) and return its
/// path. `create_new` is the no-overwrite guarantee: a name that exists is
/// skipped, never truncated.
pub fn write_new(dir: &Path, at: Stamp, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for attempt in 1..=ATTEMPTS {
        let path = dir.join(file_name(at, attempt));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "every file name for this second is taken",
    ))
}

/// `<Documents>\Bayan`, from `FOLDERID_Documents`.
pub fn output_dir() -> Option<PathBuf> {
    crate::link::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents)
        .map(|docs| docs.join("Bayan"))
}

/// The local time now.
pub fn now() -> Stamp {
    // SAFETY: SYSTEMTIME is eight u16 fields, so all-zero is a valid value,
    // and GetLocalTime fills the struct it is pointed at. It cannot fail.
    let t = unsafe {
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        t
    };
    Stamp {
        year: t.wYear,
        month: t.wMonth,
        day: t.wDay,
        hour: t.wHour,
        minute: t.wMinute,
        second: t.wSecond,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: Stamp = Stamp {
        year: 2026,
        month: 3,
        day: 7,
        hour: 9,
        minute: 5,
        second: 2,
    };

    #[test]
    fn the_name_is_zero_padded_and_a_repeat_gets_a_suffix() {
        assert_eq!(file_name(AT, 1), "bayan-20260307-090502.txt");
        assert_eq!(file_name(AT, 2), "bayan-20260307-090502-2.txt");
    }

    #[test]
    fn the_text_drops_padding_and_the_empty_rows_below_the_output() {
        let grid = "PS C:\\> echo مرحبا   \nمرحبا\n\nPS C:\\>      \n\n\n";
        assert_eq!(
            file_text(grid).as_deref(),
            Some("PS C:\\> echo مرحبا\r\nمرحبا\r\n\r\nPS C:\\>\r\n"),
            "a blank line inside the output stays"
        );
        assert_eq!(file_text("   \n\n"), None, "an empty pane writes no file");
        assert_eq!(file_text(""), None);
    }

    #[test]
    fn a_second_save_in_the_same_second_does_not_overwrite_the_first() {
        let dir = std::env::temp_dir().join(format!(
            "bayan-export-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("Bayan");
        let first = write_new(&nested, AT, "one\r\n").expect("first save");
        let second = write_new(&nested, AT, "two\r\n").expect("second save");
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one\r\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two\r\n");
        assert_eq!(
            second.file_name().unwrap().to_str(),
            Some("bayan-20260307-090502-2.txt")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
