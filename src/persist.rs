//! Replace JSON files without exposing a partial write to the next launch.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

struct StagingFile(std::path::PathBuf);

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(path, |file| file.write_all(bytes))
}

/// Copy `path` to `path` + `.bayan.bak` the first time, and never again.
/// `$PROFILE` is the user's file; the backup is the copy from before Bayan
/// rewrote the managed line.
pub fn backup_once(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let Some(name) = path.file_name() else {
        return Ok(());
    };
    let mut bak_name = name.to_os_string();
    bak_name.push(".bayan.bak");
    let bak = path.with_file_name(bak_name);
    if bak.exists() {
        return Ok(());
    }
    write(&bak, &std::fs::read(path)?)
}

fn replace_with(path: &Path, fill: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
    std::fs::create_dir_all(parent)?;
    // Same directory means the final rename stays on the same filesystem.
    // create_new refuses collisions (including planted symlinks).
    let (staging, mut file) = loop {
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = parent.join(temp_name);
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (StagingFile(temp), file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    fill(&mut file)?;
    file.sync_all()?;
    // Windows must be able to rename the staging file.
    drop(file);
    // Never remove the old file first: a failed replacement must preserve it.
    std::fs::rename(&staging.0, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "bayan-persist-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn failed_partial_write_preserves_previous_json_and_cleans_up() {
        let fixture = Fixture::new();
        let path = fixture.0.join("session.json");
        std::fs::write(&path, br#"{"tabs":["old"]}"#).unwrap();
        let result = replace_with(&path, |file| {
            file.write_all(b"{\"tabs\":")?;
            Err(io::Error::other("simulated disk full"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"tabs":["old"]}"#);
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn saves_new_files_and_replaces_existing_files() {
        let fixture = Fixture::new();
        let path = fixture.0.join("config.json");
        write(&path, b"old content").unwrap();
        write(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn replacement_failure_leaves_destination_and_no_staging_file() {
        let fixture = Fixture::new();
        let path = fixture.0.join("directory");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"keep").unwrap();
        assert!(write(&path, b"new").is_err());
        assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn atomic_replace_keeps_a_finished_file_and_backs_up_once() {
        // N-06: temp dir, never ~/.bayan
        let fixture = Fixture::new();
        let dir = &fixture.0;
        let path = dir.join("session.json");
        write(&path, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
        // the second write is the one that has to replace, not truncate
        write(&path, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":2}");
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.ends_with(".tmp")),
            "temp left behind: {names:?}"
        );

        let profile = dir.join("Microsoft.PowerShell_profile.ps1");
        backup_once(&profile).unwrap();
        assert!(!dir
            .join("Microsoft.PowerShell_profile.ps1.bayan.bak")
            .exists());
        std::fs::write(&profile, b"user-line\n").unwrap();
        backup_once(&profile).unwrap();
        let bak = dir.join("Microsoft.PowerShell_profile.ps1.bayan.bak");
        assert_eq!(std::fs::read(&bak).unwrap(), b"user-line\n");
        std::fs::write(&profile, b"changed\n").unwrap();
        backup_once(&profile).unwrap();
        assert_eq!(
            std::fs::read(&bak).unwrap(),
            b"user-line\n",
            "the backup is taken once"
        );
    }
}
