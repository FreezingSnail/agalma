//! Process-bound execution/kernel supervisor (B1.7, decisions D2/D3).
//!
//! The **candidate** replacement for the M1 seed executor is the B0 durable
//! kernel. This module is the conductor-side wiring that lets a test or binary
//! *select* the kernel binding and drive it across a process boundary: it
//! supervises the `agalma-kernel-server` child (which itself connects a
//! `LedgerClient` to the extracted ledger server), waits for the `READY`
//! handshake, and connects an [`ExecutionClient`].
//!
//! The production default remains the in-process seed executor: nothing here is
//! switched on by default. [`ExecutionTransport::from_env`] reads
//! `AGALMA_EXECUTION_TRANSPORT` (`in-process` default, `process` optional); the
//! kernel process binding is selected only by a caller/test that asks for it.
//!
//! The socket lives directly under the run state dir (`execution.sock`) to stay
//! within `SUN_LEN`.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_ipc::{CommandSpec, ExecutionClient};
use agalma_kernel::server::{LEDGER_SOCKET_ENV, SANDBOX_SOCKET_ENV};

use crate::harness_process::{
    resolve_server_binary, spawn_supervised, stop_supervised, READY_TIMEOUT,
};

/// Environment variable selecting the execution transport (candidate only).
pub const TRANSPORT_ENV: &str = "AGALMA_EXECUTION_TRANSPORT";
/// Socket file name under the run state dir.
pub const EXECUTION_SOCKET_NAME: &str = "execution.sock";

/// How a caller reaches the kernel execution candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionTransport {
    /// Direct in-process `KernelEngine` call (the candidate's in-process form).
    InProcess,
    /// Supervised kernel component server over the framed transport.
    Process,
}

impl ExecutionTransport {
    /// Read the transport from [`TRANSPORT_ENV`]; unknown/empty is `InProcess`.
    pub fn from_env() -> Self {
        match std::env::var(TRANSPORT_ENV).ok().as_deref() {
            Some("process") => ExecutionTransport::Process,
            _ => ExecutionTransport::InProcess,
        }
    }

    /// Stable selector string.
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionTransport::InProcess => "in-process",
            ExecutionTransport::Process => "process",
        }
    }
}

/// The socket a kernel server binds under `state_dir`.
pub fn execution_socket(state_dir: &Path) -> PathBuf {
    state_dir.join(EXECUTION_SOCKET_NAME)
}

/// Launch spec for the kernel server bound to `socket`, hosted under `state_dir`,
/// reaching the ledger server at `ledger_socket` and (optionally) the sandbox
/// server at `sandbox_socket`.
pub fn execution_server_spec(
    socket: &Path,
    state_dir: &Path,
    ledger_socket: &Path,
    sandbox_socket: Option<&Path>,
) -> CommandSpec {
    let mut spec = agalma_kernel::server::server_command()
        .arg("--socket")
        .arg(socket.to_string_lossy().into_owned())
        .arg("--state-dir")
        .arg(state_dir.to_string_lossy().into_owned())
        .env(
            LEDGER_SOCKET_ENV,
            ledger_socket.to_string_lossy().into_owned(),
        );
    if let Some(sandbox_socket) = sandbox_socket {
        spec = spec.env(
            SANDBOX_SOCKET_ENV,
            sandbox_socket.to_string_lossy().into_owned(),
        );
    }
    spec
}

/// A supervised kernel component-server process.
pub struct ExecutionProcess {
    socket: PathBuf,
    state_dir: PathBuf,
    child: Child,
    stopped: bool,
}

impl ExecutionProcess {
    /// Spawn a kernel server listening on `<state_dir>/execution.sock`; wait for
    /// `READY`.
    pub fn spawn(
        state_dir: &Path,
        ledger_socket: &Path,
        sandbox_socket: Option<&Path>,
        timeout: Duration,
    ) -> Result<ExecutionProcess, String> {
        let socket = execution_socket(state_dir);
        let spec = execution_server_spec(&socket, state_dir, ledger_socket, sandbox_socket);
        let program = resolve_server_binary(&spec.program)?;
        ExecutionProcess::spawn_with_spec(spec, program, socket, state_dir.to_path_buf(), timeout)
    }

    /// Spawn a kernel server from an explicit spec/program (used by tests that
    /// re-exec the test binary as the server when the real binary is absent).
    pub fn spawn_with_spec(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        state_dir: PathBuf,
        timeout: Duration,
    ) -> Result<ExecutionProcess, String> {
        let (guard, _info) = spawn_supervised(&spec, &program, &socket, timeout)?;
        Ok(ExecutionProcess {
            socket,
            state_dir,
            child: guard.into_child(),
            stopped: false,
        })
    }

    /// The socket the server is listening on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The state dir the server hosts under.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The server child pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Connect a fresh execution client to the running server.
    pub fn client(&self) -> Result<ExecutionClient, String> {
        ExecutionClient::connect(&self.socket).map_err(|e| e.to_string())
    }

    /// Stop the server with `SIGTERM`/`SIGKILL` evidence. Idempotent.
    pub fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "kernel server already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }
}

impl Drop for ExecutionProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The default handshake timeout (re-exported from the shared supervisor).
pub const DEFAULT_READY_TIMEOUT: Duration = READY_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_from_env_defaults_to_in_process() {
        std::env::remove_var(TRANSPORT_ENV);
        assert_eq!(
            ExecutionTransport::from_env(),
            ExecutionTransport::InProcess
        );
    }

    #[test]
    fn spec_includes_socket_state_dir_and_ledger_env() {
        let spec = execution_server_spec(
            Path::new("/run/e.sock"),
            Path::new("/run/state"),
            Path::new("/run/ledger.sock"),
            None,
        );
        assert_eq!(spec.program, agalma_kernel::server::SERVER_BIN);
        assert_eq!(
            spec.args,
            vec!["--socket", "/run/e.sock", "--state-dir", "/run/state"]
        );
        assert_eq!(
            spec.env.get(LEDGER_SOCKET_ENV).map(String::as_str),
            Some("/run/ledger.sock")
        );
        assert!(!spec.env.contains_key(SANDBOX_SOCKET_ENV));
    }

    #[test]
    fn spec_includes_sandbox_env_when_provided() {
        let spec = execution_server_spec(
            Path::new("/run/e.sock"),
            Path::new("/run/state"),
            Path::new("/run/ledger.sock"),
            Some(Path::new("/run/sandbox.sock")),
        );
        assert_eq!(
            spec.env.get(SANDBOX_SOCKET_ENV).map(String::as_str),
            Some("/run/sandbox.sock")
        );
    }

    #[test]
    fn socket_lives_under_the_state_dir() {
        assert_eq!(
            execution_socket(Path::new("/run/state")),
            PathBuf::from("/run/state/execution.sock")
        );
    }
}
