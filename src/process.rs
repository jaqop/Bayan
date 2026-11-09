//! Bounded background process probes shared by shell and theme discovery.

/// `Command::output()` with a deadline. std has no wait-with-timeout, so poll
/// `try_wait` and kill on expiry; the previews ran on the UI thread with no
/// bound at all, so one stuck `oh-my-posh` (a broken theme, a slow shell
/// plugin it shells out to) hung Bayan for good.
pub(crate) fn run_with_timeout(
    mut cmd: std::process::Command,
    limit: std::time::Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // previews and tests never open a console
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // drain stdout on its own thread so a chatty child never blocks on a
    // full pipe while we wait on it
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if t0.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(15))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    // The child is gone, but read_to_end returns only when EVERY write end
    // of the pipe closes — and oh-my-posh shells out, so a grandchild can
    // still be holding the one it inherited. Joining that thread outright
    // would hang the UI thread for as long as the grandchild lives, which
    // is the exact freeze the deadline above exists to prevent. Bound this
    // wait too; a preview we cannot collect in time is a preview we skip.
    let left = limit
        .saturating_sub(t0.elapsed())
        .max(std::time::Duration::from_millis(250));
    let stdout = rx.recv_timeout(left).ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}
