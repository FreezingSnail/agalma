//! Local (unconfined) process launcher implementing
//! [`agalma_contracts::ProcessApi`].
//!
//! Lifted from the Seatbelt backend
//! (`crates/agalma-sandbox/src/seatbelt.rs`) minus the confinement profile:
//!
//! 1. **Process-group ownership.** The child is made a process-group leader
//!    (`process_group(0)`), so `pgid == pid` and every descendant that does not
//!    call `setsid` shares the group.
//! 2. **Terminate the tree, not the pid.** A group signal is sent first
//!    (tolerating the macOS `EPERM` behaviour), then each descendant is
//!    signalled explicitly, then the direct child is reaped and the group is
//!    verified empty, escalating to `SIGKILL` on timeout.
//! 3. **No owned handle required.** [`LocalLauncher::terminate`] signals and
//!    verifies purely from `pid`/`pgid`, so a handle reconstructed after a
//!    conductor restart still tears the tree down.

use std::collections::HashMap;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::error::ContractError;
use agalma_contracts::harness::StopEvidence;
use agalma_contracts::{ProcessApi, ProcessChild, ProcessSpec};

/// Signal tool used for group/descendant signals.
pub const KILL: &str = "/bin/kill";
/// Process-group lister used to enumerate descendants.
pub const PGREP: &str = "/usr/bin/pgrep";

/// Grace period between `SIGTERM` and `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_millis(500);
/// Grace period after `SIGKILL` before declaring the group gone.
const KILL_GRACE: Duration = Duration::from_millis(2000);
/// Poll interval while waiting for processes to disappear.
const POLL: Duration = Duration::from_millis(25);

/// Local process launcher.
///
/// The launcher owns the [`tokio::process::Child`] handles it launched, keyed by
/// pid, so [`LocalLauncher::terminate`] can reap the direct child and
/// [`LocalLauncher::alive`] can reap an exited one instead of reporting a
/// zombie as alive. Termination still works for a handle whose child is not
/// owned by this instance (e.g. after a conductor restart): it signals and
/// verifies purely from `pid`/`pgid`.
///
/// The child map sits behind a [`Mutex`] because the contract's `alive(&self)`
/// must be able to reap.
#[derive(Default)]
pub struct LocalLauncher {
    children: Mutex<HashMap<u32, tokio::process::Child>>,
}

impl LocalLauncher {
    /// Construct a launcher with no live children.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run a command to completion, capturing its exit status and output.
    ///
    /// This is the one-shot counterpart to [`ProcessApi::launch`]: it runs the
    /// command as a process-group leader with piped stdout/stderr and blocks
    /// until it exits (or `timeout` elapses, in which case the process group is
    /// terminated and a `KnownFailure` is returned). Used by the conductor's
    /// verify phase, which needs the exit code as its verdict; the long-lived
    /// worker path keeps using `launch`.
    pub fn run(
        &mut self,
        spec: &ProcessSpec,
        timeout: Duration,
    ) -> Result<ProcessOutput, ContractError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.current_dir(&spec.cwd);
        cmd.env_clear().envs(&spec.env);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| {
            ContractError::KnownFailure(format!("failed to launch {}: {e}", spec.program))
        })?;
        let pid = child.id();
        let out_reader = child.stdout.take().map(drain);
        let err_reader = child.stderr.take().map(drain);

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => {
                    return Err(ContractError::KnownFailure(format!(
                        "waiting on pid {pid} failed: {e}"
                    )))
                }
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(POLL);
        };

        let Some(status) = status else {
            // Timed out: tear the group down, reap, then collect partial output.
            let _ = signal_group(pid, "TERM");
            let _ = signal_pid(pid, "TERM");
            std::thread::sleep(TERM_GRACE);
            let _ = signal_group(pid, "KILL");
            let _ = signal_pid(pid, "KILL");
            let _ = child.wait();
            let _ = out_reader.map(std::thread::JoinHandle::join);
            let _ = err_reader.map(std::thread::JoinHandle::join);
            return Err(ContractError::KnownFailure(format!(
                "command (pid {pid}) exceeded {timeout:?}"
            )));
        };

        let stdout = out_reader
            .and_then(|h| h.join().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let stderr = err_reader
            .and_then(|h| h.join().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        Ok(ProcessOutput {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }
}

/// Captured result of a one-shot run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessOutput {
    /// Process exit code, or `-1` when the child was signalled.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Spawn a thread that reads `reader` to EOF, returning the bytes.
fn drain<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    })
}

impl ProcessApi for LocalLauncher {
    fn launch(&mut self, spec: ProcessSpec) -> Result<ProcessChild, ContractError> {
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.current_dir(&spec.cwd);
        cmd.env_clear().envs(&spec.env);
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn().map_err(|e| {
            ContractError::KnownFailure(format!("failed to launch {}: {e}", spec.program))
        })?;

        let pid = child.id().ok_or_else(|| {
            ContractError::KnownFailure(
                "child exited before a handle could be recorded".to_string(),
            )
        })?;
        let started_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.children.lock().unwrap().insert(pid, child);

        Ok(ProcessChild {
            pid,
            pgid: pid,
            attempt: spec.attempt,
            started_at_unix_ms,
        })
    }

    fn alive(&self, child: &ProcessChild) -> Result<bool, ContractError> {
        // An owned child is reaped here: a live-but-unreaped zombie (which
        // `kill -0` still reports) must read as dead once it has exited.
        let mut children = self.children.lock().unwrap();
        let exited = match children.get_mut(&child.pid) {
            Some(owned) => matches!(owned.try_wait(), Ok(Some(_))),
            None => false,
        };
        if exited {
            children.remove(&child.pid);
        }
        drop(children);
        Ok(!exited && pid_alive(child.pid))
    }

    fn terminate(&mut self, child: &ProcessChild) -> Result<StopEvidence, ContractError> {
        let pgid = child.pgid;
        let self_pid = std::process::id();
        let mut actions: Vec<String> = Vec::new();

        // 1. Group signal first; macOS may reject it (EPERM) — tolerate and
        //    fall through to explicit descendant signalling.
        match signal_group(pgid, "TERM") {
            Ok(()) => actions.push(format!("SIGTERM -> pgid {pgid}")),
            Err(e) => actions.push(format!("SIGTERM -> pgid {pgid} rejected ({e}); continuing")),
        }

        // 2. Signal each descendant explicitly.
        let mut signalled = 0usize;
        for pid in pgrep_group(pgid) {
            if pid == self_pid {
                continue;
            }
            if signal_pid(pid, "TERM").is_ok() {
                signalled += 1;
            }
        }
        if signalled > 0 {
            actions.push(format!("SIGTERM -> {signalled} descendant(s)"));
        }

        // 3. Reap the direct child if this instance owns it.
        if let Some(mut owned) = self.children.lock().unwrap().remove(&child.pid) {
            if reap(&mut owned, TERM_GRACE) {
                actions.push(format!("reaped pid {}", child.pid));
            } else {
                actions.push(format!("pid {} did not exit within grace", child.pid));
            }
        } else {
            actions.push(format!(
                "pid {} not owned by this instance; verifying via group scan",
                child.pid
            ));
        }

        // 4. Verify the group is empty; escalate to SIGKILL and retry.
        let mut gone = wait_group_gone(pgid, TERM_GRACE);
        if !gone {
            let _ = signal_group(pgid, "KILL");
            for pid in pgrep_group(pgid) {
                if pid != self_pid {
                    let _ = signal_pid(pid, "KILL");
                }
            }
            gone = wait_group_gone(pgid, KILL_GRACE);
            actions.push(format!("escalated to SIGKILL; process_group_gone={gone}"));
        }

        Ok(StopEvidence {
            process_group_gone: gone,
            detail: actions.join("; "),
        })
    }
}

/// Send `sig` to a single pid. Returns `Err` when the signal could not be sent.
fn signal_pid(pid: u32, sig: &str) -> io::Result<()> {
    let status = Command::new(KILL)
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{KILL} -{sig} {pid} exited {status}"
        )))
    }
}

/// Send `sig` to a process group (`-pgid`). Callers tolerate the macOS `EPERM`.
fn signal_group(pgid: u32, sig: &str) -> io::Result<()> {
    let status = Command::new(KILL)
        .arg(format!("-{sig}"))
        .arg(format!("-{pgid}"))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{KILL} -{sig} -{pgid} exited {status}"
        )))
    }
}

/// List the pids currently in process group `pgid`.
fn pgrep_group(pgid: u32) -> Vec<u32> {
    let output = match Command::new(PGREP).arg("-g").arg(pgid.to_string()).output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

/// Whether `pid` names a live process (treating `EPERM` as alive).
fn pid_alive(pid: u32) -> bool {
    match Command::new(KILL).arg("-0").arg(pid.to_string()).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => String::from_utf8_lossy(&output.stderr).contains("Operation not permitted"),
        Err(_) => false,
    }
}

/// Wait up to `timeout` for the direct child to exit, reaping it.
fn reap(child: &mut tokio::process::Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Wait up to `timeout` for process group `pgid` to become empty.
fn wait_group_gone(pgid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pgrep_group(pgid).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_tracks_liveness() {
        assert!(pid_alive(std::process::id()));

        // A reaped process must be reported dead.
        let mut child = Command::new("/usr/bin/true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        assert!(!pid_alive(pid));
    }

    #[test]
    fn pgrep_group_of_unused_group_is_empty() {
        // A group id that cannot exist (max u32) has no members.
        assert!(pgrep_group(u32::MAX).is_empty());
    }
}
