//! Is a drive path local all the way down, without following a link to find out?
//!
//! [`crate::term::accept_local_cwd`] reads a path as text, so it cannot see
//! a junction or symlink. `C:\proj` may be a junction to `\\server\share`,
//! and then any `exists`, `is_dir` or open of `C:\proj\a.rs` connects to that
//! server over SMB and can send the user's NTLM hash. A check on the opened
//! file's final path (`GetFinalPathNameByHandleW`, what Ghostty does for kitty
//! graphics) comes too late for that: the open has already connected.
//!
//! So the walk goes one component at a time from the drive root. A
//! component's attributes are read without following it, and every component
//! before it is already known to be a plain local directory, so no read here
//! can leave the machine. A symlink or junction is not refused outright,
//! because pnpm's `node_modules` and many dev trees are made of them: its
//! target is read from the reparse data, without following it, and the walk
//! restarts on the target when the target is a local drive path. A volume
//! mount point (a link to a volume's root) is walked through in place. A UNC
//! target, a folder inside a volume, an unknown link type, too many hops, or
//! a probe that fails for any reason other than "not found" is
//! [`Walk::NotLocal`].
//!
//! The drive itself is checked first: a mapped network drive is
//! `DRIVE_REMOTE` and is refused before any component is read.
//!
//! Reparse points that do not stand for another name (OneDrive and other
//! cloud placeholders, dedup, compression) are files on this volume, so the
//! walk goes on through them.
//!
//! Everything but the live probes is pure, so it is tested on every host.

/// Where a no-follow walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walk {
    /// Every component exists and is local, links included.
    Local,
    /// A component does not exist. Nothing past it was read.
    Missing,
    /// Remote, unreadable, or a link that leaves the local drives.
    NotLocal,
}

/// One component's attributes, read without following it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attr {
    /// Not a reparse point.
    Plain,
    /// A reparse point. Its target is read with the reparse probe.
    Reparse,
    /// The name does not exist.
    Missing,
    /// The read failed for another reason (access denied, a bad name…).
    Unknown,
}

/// What a reparse point stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reparse {
    /// A symlink or junction to this path, as stored. `relative` is a
    /// symlink whose target is relative to the link's own directory.
    Link { target: String, relative: bool },
    /// Not a name redirect (cloud placeholder, dedup, …): a file or
    /// directory on this volume.
    InPlace,
    /// A redirect this walk does not follow (WSL symlink, AF_UNIX, …).
    Other,
}

/// Symlinks and junctions are followed at most this many times. Windows
/// itself stops at 63; a path that needs more is not a path anyone clicks.
pub const MAX_HOPS: usize = 32;

/// `IO_REPARSE_TAG_MOUNT_POINT`: a junction or a volume mount point.
pub const TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `IO_REPARSE_TAG_SYMLINK`.
pub const TAG_SYMLINK: u32 = 0xA000_000C;
/// The name-surrogate bit (`IsReparseTagNameSurrogate`): the reparse point
/// stands for another named entity.
const NAME_SURROGATE: u32 = 0x2000_0000;
/// `SYMLINK_FLAG_RELATIVE`.
const SYMLINK_FLAG_RELATIVE: u32 = 1;

/// The prefixes of a normalized `X:\a\b` path, one per component:
/// `X:\a`, then `X:\a\b`. The drive root alone has none.
pub fn component_prefixes(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    if path.len() <= 3 {
        return out;
    }
    for (i, b) in path.bytes().enumerate().skip(3) {
        if b == b'\\' {
            out.push(&path[..i]);
        }
    }
    out.push(path);
    out
}

/// Decode `REPARSE_DATA_BUFFER` as `FSCTL_GET_REPARSE_POINT` returns it.
/// `None` is a buffer too short for what its header claims.
pub fn parse_reparse(buf: &[u8]) -> Option<Reparse> {
    let u16_at = |at: usize| -> Option<u16> {
        Some(u16::from_le_bytes(buf.get(at..at + 2)?.try_into().ok()?))
    };
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
    };
    let tag = u32_at(0)?;
    // ReparseDataLength counts the bytes after the 8-byte header. A buffer
    // shorter than that, or a name that reaches past it, is not trusted.
    let end = 8 + usize::from(u16_at(4)?);
    let buf = buf.get(..end)?;
    let (names_at, relative): (usize, bool) = match tag {
        // Header (8), four name offsets/lengths (8), Flags (4).
        TAG_SYMLINK => (20, u32_at(16)? & SYMLINK_FLAG_RELATIVE != 0),
        // Header (8), four name offsets/lengths (8).
        TAG_MOUNT_POINT => (16, false),
        t if t & NAME_SURROGATE == 0 => return Some(Reparse::InPlace),
        _ => return Some(Reparse::Other),
    };
    let off = usize::from(u16_at(8)?);
    let len = usize::from(u16_at(10)?);
    let start = names_at.checked_add(off)?;
    let bytes = buf.get(start..start.checked_add(len)?)?;
    if len % 2 != 0 {
        return None;
    }
    // `len` is even (checked above), so there is no odd byte left over.
    let (pairs, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|p| u16::from_le_bytes(*p)).collect();
    let target = String::from_utf16(&units).ok()?;
    Some(Reparse::Link { target, relative })
}

/// Where a link points, as a drive path to walk next, or `None` when it
/// leaves the local drives. `link` is the prefix the link was found at.
/// `\??\C:\x` is `C:\x`; `\??\UNC\…` and `\??\Volume{…}\` are `None` (a
/// volume root never gets here: [`walk`] passes it in place).
/// A relative target is joined to the link's own directory; the caller
/// normalizes the result (`..` included) with the same gate as any path.
pub fn link_target(link: &str, target: &str, relative: bool) -> Option<String> {
    if relative {
        if target.starts_with('\\') || target.starts_with('/') {
            // Rooted on the link's drive: `\x` under `C:\a\l` is `C:\x`.
            return Some(format!("{}{}", &link[..2], target));
        }
        let dir = match link.rfind('\\') {
            Some(i) if i >= 2 => &link[..i],
            _ => return None,
        };
        return Some(format!("{dir}\\{target}"));
    }
    let rest = target
        .strip_prefix(r"\??\")
        .or_else(|| target.strip_prefix(r"\\?\"))
        .unwrap_or(target);
    let b = rest.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        Some(rest.to_string())
    } else {
        None
    }
}

/// `\??\Volume{GUID}\` or `\\?\Volume{GUID}\`, a volume's root and nothing
/// below it: a folder inside the volume could itself be a link, and the walk
/// would not see it.
pub fn is_volume_root(target: &str) -> bool {
    let Some(rest) = target
        .strip_prefix(r"\??\")
        .or_else(|| target.strip_prefix(r"\\?\"))
    else {
        return false;
    };
    let Some(guid) = rest
        .strip_prefix("Volume{")
        .and_then(|r| r.strip_suffix('\\').unwrap_or(r).strip_suffix('}'))
    else {
        return false;
    };
    let groups: Vec<&str> = guid.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Walk `path` (already through [`crate::term::accept_local_cwd`]) with
/// injected probes. `norm` is that same gate, applied to every link target.
/// `drive_local` is asked once per drive the walk reaches.
pub fn walk<N, D, A, R>(path: &str, norm: N, drive_local: D, attr: A, reparse: R) -> Walk
where
    N: Fn(&str) -> Option<String>,
    D: Fn(char) -> bool,
    A: Fn(&str) -> Attr,
    R: Fn(&str) -> Option<Reparse>,
{
    let Some(mut cur) = norm(path) else {
        return Walk::NotLocal;
    };
    let mut hops = 0;
    'restart: loop {
        let Some(drive) = cur.chars().next() else {
            return Walk::NotLocal;
        };
        if !drive_local(drive) {
            return Walk::NotLocal;
        }
        for prefix in component_prefixes(&cur) {
            match attr(prefix) {
                Attr::Plain => {}
                Attr::Missing => return Walk::Missing,
                Attr::Unknown => return Walk::NotLocal,
                Attr::Reparse => match reparse(prefix) {
                    Some(Reparse::InPlace) => {}
                    // A volume mount point: the mount manager only mounts
                    // local volumes, and the components below it are still
                    // walked one by one through this same path.
                    Some(Reparse::Link {
                        target,
                        relative: false,
                    }) if is_volume_root(&target) => {}
                    Some(Reparse::Link { target, relative }) => {
                        hops += 1;
                        if hops > MAX_HOPS {
                            return Walk::NotLocal;
                        }
                        let Some(base) = link_target(prefix, &target, relative) else {
                            return Walk::NotLocal;
                        };
                        // `\??\Z:\` plus `\x.rs` must be `Z:\x.rs`: the
                        // gate refuses a `\\` anywhere.
                        let tail = &cur[prefix.len()..];
                        let joined = if tail.is_empty() {
                            base
                        } else {
                            format!("{}{tail}", base.trim_end_matches('\\'))
                        };
                        let Some(next) = norm(&joined) else {
                            return Walk::NotLocal;
                        };
                        cur = next;
                        continue 'restart;
                    }
                    Some(Reparse::Other) | None => return Walk::NotLocal,
                },
            }
        }
        return Walk::Local;
    }
}

/// The live walk: [`walk`] with the Win32 probes and the shared gate.
#[cfg(windows)]
pub fn walk_live(path: &str) -> Walk {
    walk(
        path,
        crate::term::accept_local_cwd,
        live::drive_is_local,
        live::attr,
        live::reparse,
    )
}

#[cfg(windows)]
mod live {
    use super::{parse_reparse, Attr, Reparse};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetDriveTypeW, GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// `DRIVE_REMOTE`. Its constant lives in a feature this crate does not
    /// enable; the value is the Win32 one (as in `term.rs`).
    const DRIVE_REMOTE: u32 = 4;
    /// `FSCTL_GET_REPARSE_POINT`, from `Win32_System_Ioctl`, which this
    /// crate does not enable either.
    const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
    /// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`.
    const MAX_REPARSE: usize = 16 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn drive_is_local(drive: char) -> bool {
        let root = wide(&format!("{drive}:\\"));
        // SAFETY: `root` is NUL-terminated.
        unsafe { GetDriveTypeW(root.as_ptr()) != DRIVE_REMOTE }
    }

    pub fn attr(path: &str) -> Attr {
        let w = wide(path);
        // SAFETY: `w` is NUL-terminated. GetFileAttributesW does not follow
        // a reparse point in the last component.
        let a = unsafe { GetFileAttributesW(w.as_ptr()) };
        if a == u32::MAX {
            // SAFETY: no other call sits between the failure and this read.
            let err = unsafe { GetLastError() };
            return if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
                Attr::Missing
            } else {
                Attr::Unknown
            };
        }
        if a & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Attr::Reparse
        } else {
            Attr::Plain
        }
    }

    pub fn reparse(path: &str) -> Option<Reparse> {
        let w = wide(path);
        // SAFETY: `w` is NUL-terminated. Access 0 asks for no data rights;
        // OPEN_REPARSE_POINT opens the link itself, and BACKUP_SEMANTICS is
        // what lets a directory (a junction) be opened at all.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut buf = vec![0u8; MAX_REPARSE];
        let mut got = 0u32;
        // SAFETY: `h` is a valid handle until CloseHandle below; the output
        // pointer and length are `buf`'s own, and `got` outlives the call.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_GET_REPARSE_POINT,
                std::ptr::null(),
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: `h` came from CreateFileW and is closed exactly once.
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return None;
        }
        parse_reparse(&buf[..got as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The shared gate's shape for these tests: `/` to `\`, UNC, device
    /// paths and a `\\` anywhere refused, `.`/`..` collapsed, a `..` past
    /// the drive refused. The real gate is `term::accept_local_cwd`;
    /// `walk_live` passes it.
    fn norm(p: &str) -> Option<String> {
        let p = p.replace('/', "\\");
        if p.starts_with(r"\??\") || p.contains(r"\\") {
            return None;
        }
        let b = p.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
            return None;
        }
        let mut stack: Vec<&str> = Vec::new();
        for comp in p[3..].split('\\').filter(|c| !c.is_empty()) {
            match comp {
                "." => {}
                ".." => {
                    stack.pop()?;
                }
                c => stack.push(c),
            }
        }
        Some(format!("{}\\{}", &p[..2], stack.join("\\")))
    }

    /// A fake volume: plain names, links, and everything else missing.
    struct Fs {
        plain: Vec<&'static str>,
        links: HashMap<&'static str, Reparse>,
        unreadable: Vec<&'static str>,
        remote_drives: Vec<char>,
    }

    impl Fs {
        fn new(plain: &[&'static str]) -> Self {
            Fs {
                plain: plain.to_vec(),
                links: HashMap::new(),
                unreadable: Vec::new(),
                remote_drives: Vec::new(),
            }
        }
        fn link(mut self, at: &'static str, target: &str, relative: bool) -> Self {
            let r = Reparse::Link {
                target: target.to_string(),
                relative,
            };
            self.links.insert(at, r);
            self
        }
        fn reparse(mut self, at: &'static str, r: Reparse) -> Self {
            self.links.insert(at, r);
            self
        }
        fn walk(&self, path: &str) -> Walk {
            walk(
                path,
                norm,
                |d| !self.remote_drives.contains(&d),
                |p| {
                    if self.unreadable.iter().any(|u| u.eq_ignore_ascii_case(p)) {
                        Attr::Unknown
                    } else if self.links.keys().any(|k| k.eq_ignore_ascii_case(p)) {
                        Attr::Reparse
                    } else if self.plain.iter().any(|k| k.eq_ignore_ascii_case(p)) {
                        Attr::Plain
                    } else {
                        Attr::Missing
                    }
                },
                |p| {
                    self.links
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(p))
                        .map(|(_, v)| v.clone())
                },
            )
        }
    }

    #[test]
    fn prefixes_are_one_per_component() {
        assert_eq!(component_prefixes(r"C:\"), Vec::<&str>::new());
        assert_eq!(component_prefixes(r"C:\a"), vec![r"C:\a"]);
        assert_eq!(
            component_prefixes(r"C:\a\b\c.rs"),
            vec![r"C:\a", r"C:\a\b", r"C:\a\b\c.rs"]
        );
    }

    #[test]
    fn plain_and_missing_paths() {
        let fs = Fs::new(&[r"C:\src", r"C:\src\a.rs"]);
        assert_eq!(fs.walk(r"C:\src\a.rs"), Walk::Local);
        assert_eq!(fs.walk(r"C:\"), Walk::Local);
        assert_eq!(fs.walk(r"C:\src\b.rs"), Walk::Missing);
        assert_eq!(fs.walk(r"C:\nope\b.rs"), Walk::Missing);
    }

    #[test]
    fn a_mapped_network_drive_is_refused_before_any_component() {
        let mut fs = Fs::new(&[r"Z:\a.rs"]);
        fs.remote_drives.push('Z');
        assert_eq!(fs.walk(r"Z:\a.rs"), Walk::NotLocal);
    }

    #[test]
    fn a_junction_to_a_share_in_a_parent_component_is_refused() {
        // The case a final-component check misses: the link is a parent.
        for target in [
            r"\??\UNC\server\share",
            r"\\?\UNC\server\share",
            r"\\server\share",
        ] {
            let fs = Fs::new(&[]).link(r"C:\proj", target, false);
            assert_eq!(fs.walk(r"C:\proj\src\main.rs"), Walk::NotLocal, "{target}");
        }
    }

    #[test]
    fn a_symlinked_file_to_a_share_is_refused() {
        let fs = Fs::new(&[r"C:\d"]).link(r"C:\d\a.txt", r"\??\UNC\evil\s\a.txt", false);
        assert_eq!(fs.walk(r"C:\d\a.txt"), Walk::NotLocal);
    }

    #[test]
    fn a_local_junction_is_followed_and_its_target_walked() {
        // pnpm: node_modules\pkg is a junction into the store.
        let fs = Fs::new(&[
            r"C:\p",
            r"C:\p\node_modules",
            r"C:\store",
            r"C:\store\pkg",
            r"C:\store\pkg\index.js",
        ])
        .link(r"C:\p\node_modules\pkg", r"\??\C:\store\pkg", false);
        assert_eq!(fs.walk(r"C:\p\node_modules\pkg\index.js"), Walk::Local);
        assert_eq!(fs.walk(r"C:\p\node_modules\pkg\gone.js"), Walk::Missing);
    }

    #[test]
    fn a_local_link_into_a_remote_link_is_refused() {
        let fs =
            Fs::new(&[])
                .link(r"C:\a", r"\??\D:\b", false)
                .link(r"D:\b", r"\??\UNC\srv\s", false);
        assert_eq!(fs.walk(r"C:\a\x.rs"), Walk::NotLocal);
    }

    #[test]
    fn a_junction_to_a_local_drive_root_is_followed() {
        let fs = Fs::new(&[r"D:\x.rs"]).link(r"C:\d", r"\??\D:\", false);
        assert_eq!(fs.walk(r"C:\d\x.rs"), Walk::Local);
        assert_eq!(fs.walk(r"C:\d"), Walk::Local);
    }

    #[test]
    fn a_link_onto_a_mapped_drive_is_refused() {
        let mut fs = Fs::new(&[r"Z:\x.rs"]).link(r"C:\z", r"\??\Z:\", false);
        fs.remote_drives.push('Z');
        assert_eq!(fs.walk(r"C:\z\x.rs"), Walk::NotLocal);
    }

    #[test]
    fn relative_symlinks_resolve_against_the_link_directory() {
        let fs = Fs::new(&[
            r"C:\a",
            r"C:\a\real",
            r"C:\a\real\f.rs",
            r"C:\top",
            r"C:\top\f.rs",
        ])
        .link(r"C:\a\l", r"real", true)
        .link(r"C:\a\up", r"..\top", true)
        .link(r"C:\a\rooted", r"\top", true);
        assert_eq!(fs.walk(r"C:\a\l\f.rs"), Walk::Local);
        assert_eq!(fs.walk(r"C:\a\up\f.rs"), Walk::Local);
        assert_eq!(fs.walk(r"C:\a\rooted\f.rs"), Walk::Local);
    }

    #[test]
    fn a_relative_symlink_that_climbs_past_the_drive_is_refused() {
        let fs = Fs::new(&[r"C:\a"]).link(r"C:\a\l", r"..\..\..\x", true);
        assert_eq!(fs.walk(r"C:\a\l"), Walk::NotLocal);
    }

    const VOLUME: &str = r"\??\Volume{1b2c3d4e-0000-0000-0000-100000000000}\";

    #[test]
    fn a_volume_mount_point_is_walked_in_place() {
        // #36: C:\mnt\data is a local disk, VHD or Dev Drive mounted there.
        let fs = Fs::new(&[r"C:\mnt", r"C:\mnt\data\sub", r"C:\mnt\data\notes.txt"]).link(
            r"C:\mnt\data",
            VOLUME,
            false,
        );
        assert_eq!(fs.walk(r"C:\mnt\data\notes.txt"), Walk::Local);
        assert_eq!(fs.walk(r"C:\mnt\data\sub"), Walk::Local);
        assert_eq!(fs.walk(r"C:\mnt\data"), Walk::Local);
        assert_eq!(fs.walk(r"C:\mnt\data\gone.txt"), Walk::Missing);
        let win32 = Fs::new(&[r"C:\v\a"]).link(r"C:\v", &VOLUME.replace(r"\??\", r"\\?\"), false);
        assert_eq!(win32.walk(r"C:\v\a"), Walk::Local);
    }

    #[test]
    fn a_link_below_a_volume_mount_point_is_still_checked() {
        let fs = Fs::new(&[r"C:\mnt"])
            .link(r"C:\mnt\data", VOLUME, false)
            .link(r"C:\mnt\data\share", r"\??\UNC\srv\s", false);
        assert_eq!(fs.walk(r"C:\mnt\data\share\x.rs"), Walk::NotLocal);
    }

    #[test]
    fn a_link_into_a_folder_inside_a_volume_is_refused() {
        // The walk cannot see whether `sub` on that volume is a link itself.
        let inside = format!("{VOLUME}sub");
        for (target, relative) in [
            (inside.as_str(), false),
            (r"\??\Volume{1b2c3d4e-0000-0000-0000-10000000000}\", false),
            (r"\??\Volume{1b2c3d4e-0000-0000-0000-10000000000g}\", false),
            (r"\??\Volume{1b2c3d4e00000000000000001000000000000}\", false),
            (r"\??\GLOBALROOT\Device\Mup\srv\s", false),
            (r"Volume{1b2c3d4e-0000-0000-0000-100000000000}\", false),
        ] {
            let fs = Fs::new(&[r"C:\mnt"]).link(r"C:\mnt\data", target, relative);
            assert_eq!(fs.walk(r"C:\mnt\data\a.txt"), Walk::NotLocal, "{target}");
        }
    }

    #[test]
    fn link_loops_stop() {
        let fs = Fs::new(&[])
            .link(r"C:\a", r"\??\C:\b", false)
            .link(r"C:\b", r"\??\C:\a", false);
        assert_eq!(fs.walk(r"C:\a\x"), Walk::NotLocal);
    }

    #[test]
    fn cloud_placeholders_pass_and_other_redirects_do_not() {
        let fs = Fs::new(&[r"C:\Users", r"C:\Users\me"])
            .reparse(r"C:\Users\me\OneDrive", Reparse::InPlace)
            .reparse(r"C:\Users\me\OneDrive\doc.md", Reparse::InPlace)
            .reparse(r"C:\wsl", Reparse::Other);
        assert_eq!(fs.walk(r"C:\Users\me\OneDrive\doc.md"), Walk::Local);
        assert_eq!(fs.walk(r"C:\wsl\x"), Walk::NotLocal);
    }

    #[test]
    fn an_unreadable_component_or_reparse_data_is_refused() {
        let mut fs = Fs::new(&[r"C:\a\b"]);
        fs.unreadable.push(r"C:\a");
        assert_eq!(fs.walk(r"C:\a\b"), Walk::NotLocal);
        // A reparse point whose data cannot be read.
        let fs = Fs::new(&[]).reparse(r"C:\r", Reparse::Other);
        assert_eq!(fs.walk(r"C:\r\x"), Walk::NotLocal);
    }

    #[test]
    fn a_path_the_gate_refuses_is_not_walked() {
        let fs = Fs::new(&[]);
        assert_eq!(fs.walk(r"\\server\share\a.rs"), Walk::NotLocal);
    }

    fn reparse_buf(tag: u32, flags: Option<u32>, sub: &str, print: &str) -> Vec<u8> {
        let sub: Vec<u8> = sub.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let print: Vec<u8> = print.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut body = Vec::new();
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&(sub.len() as u16).to_le_bytes());
        body.extend_from_slice(&(sub.len() as u16).to_le_bytes());
        body.extend_from_slice(&(print.len() as u16).to_le_bytes());
        if let Some(f) = flags {
            body.extend_from_slice(&f.to_le_bytes());
        }
        body.extend_from_slice(&sub);
        body.extend_from_slice(&print);
        let mut out = Vec::new();
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn reparse_buffers_decode() {
        let j = reparse_buf(TAG_MOUNT_POINT, None, r"\??\C:\store\pkg", r"C:\store\pkg");
        assert_eq!(
            parse_reparse(&j),
            Some(Reparse::Link {
                target: r"\??\C:\store\pkg".into(),
                relative: false
            })
        );
        let s = reparse_buf(TAG_SYMLINK, Some(0), r"\??\UNC\srv\s", r"\\srv\s");
        assert_eq!(
            parse_reparse(&s),
            Some(Reparse::Link {
                target: r"\??\UNC\srv\s".into(),
                relative: false
            })
        );
        let r = reparse_buf(TAG_SYMLINK, Some(1), r"..\x", r"..\x");
        assert_eq!(
            parse_reparse(&r),
            Some(Reparse::Link {
                target: r"..\x".into(),
                relative: true
            })
        );
        // IO_REPARSE_TAG_CLOUD_3: not a name surrogate.
        let cloud = [&0x9000_301Au32.to_le_bytes()[..], &[0, 0, 0, 0]].concat();
        assert_eq!(parse_reparse(&cloud), Some(Reparse::InPlace));
        // IO_REPARSE_TAG_LX_SYMLINK: a surrogate this walk does not decode.
        let lx = [&0xA000_001Du32.to_le_bytes()[..], &[0, 0, 0, 0]].concat();
        assert_eq!(parse_reparse(&lx), Some(Reparse::Other));
    }

    #[test]
    fn truncated_reparse_buffers_are_rejected() {
        let j = reparse_buf(TAG_MOUNT_POINT, None, r"\??\C:\x", r"C:\x");
        assert_eq!(parse_reparse(&j[..j.len() - 6]), None);
        assert_eq!(parse_reparse(&j[..3]), None);
        let s = reparse_buf(TAG_SYMLINK, Some(0), r"\??\C:\x", r"C:\x");
        assert_eq!(parse_reparse(&s[..18]), None);
    }

    #[test]
    fn link_targets() {
        assert_eq!(
            link_target(r"C:\a\l", r"\??\D:\x", false).as_deref(),
            Some(r"D:\x")
        );
        assert_eq!(
            link_target(r"C:\a\l", r"\\?\D:\x", false).as_deref(),
            Some(r"D:\x")
        );
        assert_eq!(
            link_target(r"C:\a\l", r"D:\x", false).as_deref(),
            Some(r"D:\x")
        );
        assert_eq!(link_target(r"C:\a\l", r"\??\UNC\s\x", false), None);
        assert_eq!(link_target(r"C:\a\l", r"\\s\x", false), None);
        assert_eq!(link_target(r"C:\a\l", r"\??\Volume{x}\", false), None);
        assert_eq!(
            link_target(r"C:\a\l", r"b\c", true).as_deref(),
            Some(r"C:\a\b\c")
        );
        assert_eq!(link_target(r"C:\l", r"b", true).as_deref(), Some(r"C:\b"));
        assert_eq!(
            link_target(r"C:\a\l", r"\x", true).as_deref(),
            Some(r"C:\x")
        );
    }
}
