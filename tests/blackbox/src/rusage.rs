//! What the kernel counted against a child process, read when it is reaped.
//!
//! `wait4` hands back the child's CPU time and peak resident set together
//! with those of every descendant it waited for, so a tool command that lived
//! a few milliseconds is in the total — which a poller would miss. The cost is
//! that the caller reaps the child itself: a child handed to tokio's `Child`
//! must not also be waited for there.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub struct Spent {
    pub user: Duration,
    pub system: Duration,
    /// The largest resident set among the child and its waited descendants —
    /// the largest one process, not their sum.
    pub max_rss_bytes: u64,
}

impl Spent {
    pub fn cpu(&self) -> Duration {
        self.user + self.system
    }

    fn from_raw(ru: &libc::rusage) -> Self {
        let time = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        // Bytes on macOS, kilobytes on Linux.
        let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
        Self {
            user: time(ru.ru_utime),
            system: time(ru.ru_stime),
            max_rss_bytes: ru.ru_maxrss as u64 * unit,
        }
    }
}

/// How a reaped child ended.
#[derive(Debug)]
pub struct Reaped {
    pub status: ExitStatus,
    pub spent: Spent,
    /// It was still running after the grace period and was killed.
    pub killed: bool,
}

/// Wait for `pid` to exit, `SIGKILL` it once `grace` has passed, and reap
/// it. Blocks: run it on a blocking thread.
pub fn reap(pid: u32, grace: Duration) -> std::io::Result<Reaped> {
    let pid = pid as libc::pid_t;
    let deadline = Instant::now() + grace;
    let mut killed = false;
    loop {
        let mut status = 0;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let got = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut ru) };
        if got == pid {
            return Ok(Reaped {
                status: ExitStatus::from_raw(status),
                spent: Spent::from_raw(&ru),
                killed,
            });
        }
        if got < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if !killed && Instant::now() >= deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            killed = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
