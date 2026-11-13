//! Bounded background process probes shared by shell and theme discovery.
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;

/// Every exit path reaps the direct child. Closing the job then terminates
/// descendants too, including those still holding a copy of stdout.
struct Probe {
    child: Child,
    _job: Option<JobGuard>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A bounded `Command::output()`: one deadline, at most 1 MiB of stdout,
/// and on Windows a job assigned before the first instruction executes.
/// A probe that cannot be contained is skipped, not run without a bound.
pub(crate) fn run_with_timeout(mut cmd: Command, limit: Duration) -> Option<Output> {
    let started = Instant::now();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0004 | 0x0800_0000); // suspended, no console
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut probe = Probe { child, _job: None };
    #[cfg(windows)]
    {
        probe._job = Some(tie_to_job(&probe.child)?);
        if !resume_process(&probe.child) {
            return None;
        }
    }
    let stdout = probe.child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("bayan-probe-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take((MAX_PROBE_OUTPUT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .and_then(|_| {
                    if bytes.len() > MAX_PROBE_OUTPUT_BYTES {
                        Err(std::io::Error::other("probe output exceeded 1 MiB"))
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = tx.send(result);
        })
        .ok()?;
    let mut output = None;
    let status = loop {
        if output.is_none() {
            match rx.try_recv() {
                Ok(Ok(bytes)) => output = Some(bytes),
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if started.elapsed() >= limit {
            return None;
        }
        match probe.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => return None,
        }
    };
    let stdout = match output {
        Some(bytes) => bytes,
        None => rx
            .recv_timeout(limit.saturating_sub(started.elapsed()))
            .ok()?
            .ok()?,
    };
    Some(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// A job that kills the process and anything it spawned when the handle
/// closes. Absent off Windows, and best-effort if the job cannot be made.
/// Called while the process is still suspended, so the job owns it before
/// the first instruction runs.
pub(crate) fn tie_to_job(_child: &std::process::Child) -> Option<JobGuard> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: CreateJobObjectW returns a handle this function either
        // closes or wraps in OwnedHandle. AssignProcessToJobObject borrows
        // the process handle owned by `Child`. The information pointer is a
        // local struct that outlives SetInformationJobObject.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
            {
                let _ = CloseHandle(job);
                return None;
            }
            if AssignProcessToJobObject(job, _child.as_raw_handle()) == 0 {
                let _ = CloseHandle(job);
                return None;
            }
            Some(JobGuard(OwnedHandle::from_raw_handle(job)))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = _child;
        None
    }
}

/// Std closes the primary thread when CREATE_SUSPENDED is set and does not
/// resume it. Resume the whole process after the job assignment.
#[cfg(windows)]
pub(crate) fn resume_process(child: &std::process::Child) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
    }
    // SAFETY: the handle is the live process from Command::spawn.
    unsafe { NtResumeProcess(child.as_raw_handle()) >= 0 }
}

#[cfg(windows)]
pub(crate) struct JobGuard(std::os::windows::io::OwnedHandle);
#[cfg(not(windows))]
pub(crate) struct JobGuard;

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        // The field is the owned job handle. Dropping it after this function
        // closes the handle, and that close kills the process tree.
        let _ = self.0.as_raw_handle();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn probe_preserves_the_output_limit_and_the_child_exit_status() {
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile", "-NonInteractive", "-Command",
            "$b=[byte[]]::new(1048576); [Console]::OpenStandardOutput().Write($b,0,$b.Length); exit 7",
        ]);
        let output =
            run_with_timeout(cmd, Duration::from_secs(5)).expect("bounded output is allowed");
        assert_eq!(output.stdout.len(), MAX_PROBE_OUTPUT_BYTES);
        assert!(output.stdout.iter().all(|&byte| byte == 0));
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn probe_rejects_output_larger_than_one_mebibyte() {
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$b=[byte[]]::new(1048577); [Console]::OpenStandardOutput().Write($b,0,$b.Length)",
        ]);
        assert!(run_with_timeout(cmd, Duration::from_secs(5)).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn probe_deadline_terminates_a_grandchild_that_keeps_stdout_open() {
        const FIXTURE_PID: &str = "BAYAN_TEST_PROBE_CHILD_PID";
        // Re-enter only this test in a fresh process. Its child inherits the
        // actual stdout pipe; no PowerShell startup competes with the deadline.
        if let Some(path) = std::env::var_os(FIXTURE_PID) {
            #[expect(
                clippy::zombie_processes,
                reason = "this Windows fixture must exit before its child; the probe job owns cleanup"
            )]
            let child = Command::new("cmd.exe")
                .args(["/c", "ping", "-n", "30", "127.0.0.1"])
                .spawn()
                .expect("spawn the fixture child");
            std::fs::write(path, child.id().to_string()).expect("record the fixture child");
            return;
        }
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_TERMINATE,
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("bayan-probe-{}-{stamp}.pid", std::process::id()));
        let mut cmd = Command::new(std::env::current_exe().expect("test executable"));
        cmd.args([
            "--exact",
            "process::tests::probe_deadline_terminates_a_grandchild_that_keeps_stdout_open",
            "--nocapture",
        ])
        .env(FIXTURE_PID, &path);
        let started = std::time::Instant::now();
        let result = run_with_timeout(cmd, Duration::from_secs(3));
        let pid: u32 = std::fs::read_to_string(&path)
            .expect("the parent must launch and record its child")
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        // SAFETY: this PID is our fixture's process. OpenProcess owns the
        // returned handle; it is closed, and a failing test kills its fixture.
        let dead = unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | 0x0010_0000,
                0,
                pid,
            );
            if handle.is_null() {
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
                true
            } else {
                let exited = WaitForSingleObject(handle, 2000) == WAIT_OBJECT_0;
                if !exited {
                    let _ = TerminateProcess(handle, 1);
                }
                let _ = CloseHandle(handle);
                exited
            }
        };
        assert!(result.is_none(), "the inherited pipe must hit the deadline");
        assert!(dead, "the timed-out probe left its child process running");
        assert!(started.elapsed() < Duration::from_secs(7));
    }
}
