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
}
