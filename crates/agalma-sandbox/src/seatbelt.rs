//! macOS Seatbelt (`sandbox-exec`) implementation of [`SandboxApi`].
//!
//! Responsibilities and S0b conditions (see `docs/spikes/s0b-confinement.md`):
//!
//! 1. **Fail closed.** The profile is validated before any process is created;
//!    the launcher never falls back to unconfined execution. `sandbox-exec`
//!    returning `65` for the rendered profile is a `KnownFailure`.
//! 2. **Process-group ownership.** The confined leader is made a process-group
//!    leader (`process_group(0)`), so `pgid == pid` and every descendant that
//!    does not call `setsid` shares the group.
//! 3. **Terminate the tree, not the pid.** A group signal is sent first
//!    (tolerating the macOS `EPERM` behaviour), then each descendant is
//!    signalled explicitly, then the direct child is reaped and the group is
//!    verified empty, escalating to `SIGKILL` on timeout.
//!
//! Attempt-scoped `TMPDIR`/`XDG_*` is the caller's responsibility: they arrive
//! in [`LaunchSpec::env`] and must point inside `attempt_dir`.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::error::ContractError;
use agalma_contracts::harness::StopEvidence;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};

use crate::profile::{self, PARAM_ATTEMPT, PARAM_EXTRA_RO, PARAM_PROTECTED, PARAM_SOCKDIR};

/// Seatbelt launch tool. Deprecated but the only local confinement mechanism;
/// compatibility is re-verified on macOS upgrades (S0b open item).
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
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

/// Seatbelt-backed [`SandboxApi`].
///
/// The backend owns the [`tokio::process::Child`] handles it launched, keyed by
/// pid, so [`SeatbeltSandbox::terminate`] can reap the direct child. Termination
/// still works for a handle whose child is not owned by this instance (e.g. after
/// a conductor restart): it signals and verifies purely from `pid`/`pgid`.
#[derive(Default)]
pub struct SeatbeltSandbox {
    children: HashMap<u32, tokio::process::Child>,
}

impl SeatbeltSandbox {
    /// Construct a Seatbelt backend with no live children.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SandboxApi for SeatbeltSandbox {
    fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
        // 1. Read + structurally validate the rendered profile (fail closed).
        let source = std::fs::read_to_string(&spec.profile).map_err(|e| {
            ContractError::KnownFailure(format!(
                "sandbox profile {:?} is unreadable: {e}",
                spec.profile
            ))
        })?;
        profile::validate(&source).map_err(|e| {
            ContractError::KnownFailure(format!("sandbox profile {:?} rejected: {e}", spec.profile))
        })?;
        let profile_abs = std::fs::canonicalize(&spec.profile).map_err(|e| {
            ContractError::KnownFailure(format!(
                "sandbox profile {:?} cannot be resolved: {e}",
                spec.profile
            ))
        })?;

        // 2. Compile-check the profile with a benign command before creating
        //    the real child. sandbox-exec exits 65 on any profile error and never
        //    execs the command, so this is a fail-closed gate, not a fallback.
        preflight(&profile_abs, &spec)?;

        // 3. Spawn the confined command as a process-group leader.
        let child = spawn_confined(&profile_abs, &spec)?;

        let pid = child.id().ok_or_else(|| {
            ContractError::KnownFailure(
                "confined child exited before a handle could be recorded".to_string(),
            )
        })?;
        let started_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.children.insert(pid, child);

        Ok(SandboxChild {
            pid,
            pgid: pid,
            attempt: spec.attempt,
            started_at_unix_ms,
        })
    }

    fn alive(&self, child: &SandboxChild) -> Result<bool, ContractError> {
        Ok(pid_alive(child.pid))
    }

    fn terminate(&mut self, child: &SandboxChild) -> Result<StopEvidence, ContractError> {
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
        if let Some(mut owned) = self.children.remove(&child.pid) {
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

/// Compile the profile by running a benign command under it. Any non-zero exit
/// (in particular `65`) means the profile is unusable and the launch must fail.
fn preflight(profile: &Path, spec: &LaunchSpec) -> Result<(), ContractError> {
    let output = Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(profile)
        .args(param_args(spec))
        .arg("/usr/bin/true")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("cannot run {SANDBOX_EXEC}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ContractError::KnownFailure(format!(
            "sandbox profile rejected by {SANDBOX_EXEC} (rc {:?}): {}",
            output.status.code(),
            stderr.trim()
        )));
    }
    Ok(())
}

/// `-D NAME=value` arguments for the four profile parameters.
fn param_args(spec: &LaunchSpec) -> Vec<String> {
    let pairs = [
        (PARAM_ATTEMPT, spec.attempt_dir.as_str()),
        (PARAM_PROTECTED, spec.protected_dir.as_str()),
        (PARAM_SOCKDIR, spec.sock_dir.as_str()),
        (PARAM_EXTRA_RO, spec.extra_ro.as_str()),
    ];
    let mut args = Vec::with_capacity(pairs.len() * 2);
    for (name, value) in pairs {
        args.push("-D".to_string());
        args.push(format!("{name}={value}"));
    }
    args
}

/// Spawn the confined command as a new process-group leader.
fn spawn_confined(
    profile: &Path,
    spec: &LaunchSpec,
) -> Result<tokio::process::Child, ContractError> {
    let mut cmd = tokio::process::Command::new(SANDBOX_EXEC);
    cmd.arg("-f").arg(profile);
    cmd.args(param_args(spec));
    cmd.arg(&spec.program).args(&spec.args);
    cmd.current_dir(&spec.cwd);
    cmd.env_clear().envs(&spec.env);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn()
        .map_err(|e| ContractError::KnownFailure(format!("failed to launch {SANDBOX_EXEC}: {e}")))
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
    fn param_args_are_paired_and_ordered() {
        let spec = LaunchSpec {
            program: "/bin/true".to_string(),
            args: vec![],
            cwd: "/".to_string(),
            env: Default::default(),
            profile: "/p".to_string(),
            attempt: agalma_contracts::ids::AttemptId::derive(1),
            attempt_dir: "/a".to_string(),
            protected_dir: "/p".to_string(),
            sock_dir: "/s".to_string(),
            extra_ro: "/e".to_string(),
        };
        assert_eq!(
            param_args(&spec),
            vec![
                "-D".to_string(),
                "ATTEMPT=/a".to_string(),
                "-D".to_string(),
                "PROTECTED=/p".to_string(),
                "-D".to_string(),
                "SOCKDIR=/s".to_string(),
                "-D".to_string(),
                "EXTRA_RO=/e".to_string(),
            ]
        );
    }

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
