//! Process-bound sandbox supervisor (B1.3, decisions D2/D3).
//!
//! The sandbox component server (`agalma-sandbox-server`) hosts the Seatbelt
//! [`agalma_contracts::sandbox::SandboxApi`] backend. The conductor supervises it
//! exactly like the harness server: spawn as a process-group leader, wait for
//! the `READY {json}` handshake, connect a [`SandboxClient`], and tear the group
//! down with `SIGTERM`/`SIGKILL` evidence on drop. Supervision is shared with
//! [`crate::harness_process`] (one `spawn_supervised`/`stop_supervised` pair),
//! not duplicated.
//!
//! [`SandboxSource`] owns the supervisor lazily: the server is started on first
//! use and kept alive for the conductor process. Component sockets live under
//! the run state dir; the socket path must stay within `SUN_LEN`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::ContractError;
use agalma_ipc::{CommandSpec, SandboxClient, Wire, WIRE_ENV};
use agalma_sandbox::server_command;

use crate::harness_process::{
    resolve_server_binary, spawn_supervised, stop_supervised, READY_TIMEOUT,
};

/// Socket file name under the run state dir.
pub const SANDBOX_SOCKET_NAME: &str = "sandbox.sock";

/// The wire transport the sandbox process boundary uses (IPCX.3).
///
/// The sandbox boundary migrated to [`Wire::Connect`]; a parity run may set
/// [`agalma_ipc::WIRE_ENV`] to `framed` to compare the two wires against the
/// same scenario. The selected wire is also exported to the child process (see
/// [`SandboxProcess::spawn_with_spec_wire`]) so its server serves the matching
/// transport.
pub fn sandbox_wire() -> Wire {
    Wire::from_env(Wire::Connect)
}

/// Launch spec for the sandbox server bound to `socket`.
pub fn sandbox_server_spec(socket: &Path) -> CommandSpec {
    server_command()
        .arg("--socket")
        .arg(socket.to_string_lossy().into_owned())
}

/// A supervised sandbox component-server process.
pub struct SandboxProcess {
    socket: PathBuf,
    child: Child,
    wire: Wire,
    stopped: bool,
}

impl SandboxProcess {
    /// Spawn the sandbox server listening on `socket` (over [`sandbox_wire`])
    /// and wait for `READY`.
    pub fn spawn(socket: PathBuf, timeout: Duration) -> Result<SandboxProcess, String> {
        let spec = sandbox_server_spec(&socket);
        let program = resolve_server_binary(&spec.program)?;
        SandboxProcess::spawn_with_spec_wire(spec, program, socket, sandbox_wire(), timeout)
    }

    /// Spawn a sandbox server from an explicit spec/program over
    /// [`sandbox_wire`] (used by tests that re-exec the test binary as the
    /// server when the real binary is absent).
    pub fn spawn_with_spec(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        timeout: Duration,
    ) -> Result<SandboxProcess, String> {
        SandboxProcess::spawn_with_spec_wire(spec, program, socket, sandbox_wire(), timeout)
    }

    /// Spawn a sandbox server from an explicit spec/program over `wire`. The
    /// selected wire is exported to the child in [`WIRE_ENV`], so its server
    /// serves the matching transport.
    pub fn spawn_with_spec_wire(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        wire: Wire,
        timeout: Duration,
    ) -> Result<SandboxProcess, String> {
        let spec = spec.env(WIRE_ENV, wire.as_str());
        let (guard, _info) = spawn_supervised(&spec, &program, &socket, timeout)?;
        Ok(SandboxProcess {
            socket,
            child: guard.into_child(),
            wire,
            stopped: false,
        })
    }

    /// The socket the server is listening on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The server child pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The wire transport this process (and its server) uses.
    pub fn wire(&self) -> Wire {
        self.wire
    }

    /// Connect a fresh client to the running server over its wire.
    pub fn client(&self) -> Result<SandboxClient, String> {
        SandboxClient::connect_wire(&self.socket, self.wire).map_err(|e| e.to_string())
    }

    /// Stop the server with `SIGTERM`/`SIGKILL` evidence. Idempotent.
    pub fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "sandbox server already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }
}

impl Drop for SandboxProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Lazily-started sandbox supervisor shared by the phase activities.
///
/// The server is started on first [`SandboxSource::ensure`] and kept alive by
/// the shared [`Rc`]; when the last clone drops, the server is stopped.
#[derive(Clone)]
pub struct SandboxSource {
    state_dir: PathBuf,
    process: Rc<RefCell<Option<Rc<SandboxProcess>>>>,
}

impl SandboxSource {
    /// Create a source rooted at `state_dir` (server not yet started).
    pub fn new(state_dir: PathBuf) -> SandboxSource {
        SandboxSource {
            state_dir,
            process: Rc::new(RefCell::new(None)),
        }
    }

    /// The sandbox socket path under the state dir.
    pub fn socket(&self) -> PathBuf {
        self.state_dir.join(SANDBOX_SOCKET_NAME)
    }

    /// Start the sandbox server if needed and return its socket path.
    pub fn ensure(&self) -> Result<PathBuf, ContractError> {
        if self.process.borrow().is_none() {
            let socket = self.socket();
            let process = SandboxProcess::spawn(socket.clone(), READY_TIMEOUT)
                .map_err(|e| ContractError::KnownFailure(format!("start sandbox server: {e}")))?;
            *self.process.borrow_mut() = Some(Rc::new(process));
        }
        Ok(self.socket())
    }

    /// Ensure the server is running and connect a fresh client to it over
    /// [`sandbox_wire`].
    pub fn client(&self) -> Result<SandboxClient, ContractError> {
        let socket = self.ensure()?;
        SandboxClient::connect_wire(&socket, sandbox_wire())
            .map_err(|e| ContractError::KnownFailure(format!("connect sandbox server: {e}")))
    }

    /// The running server pid, if started.
    pub fn pid(&self) -> Option<u32> {
        self.process.borrow().as_ref().map(|process| process.pid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_includes_socket_argument() {
        let spec = sandbox_server_spec(Path::new("/run/s.sock"));
        assert_eq!(spec.program, agalma_sandbox::SERVER_BIN);
        assert_eq!(spec.args, vec!["--socket", "/run/s.sock"]);
    }

    #[test]
    fn socket_lives_under_state_dir() {
        let source = SandboxSource::new(PathBuf::from("/state"));
        assert_eq!(source.socket(), PathBuf::from("/state/sandbox.sock"));
        assert!(source.pid().is_none(), "not started until ensure()");
    }

    #[test]
    fn wire_defaults_to_connect_and_env_restores_framed() {
        // Absent -> Connect (IPCX.3 migration default); `framed` for parity.
        std::env::remove_var(WIRE_ENV);
        assert_eq!(sandbox_wire(), Wire::Connect);
        std::env::set_var(WIRE_ENV, "framed");
        assert_eq!(sandbox_wire(), Wire::Framed);
        std::env::set_var(WIRE_ENV, "connect");
        assert_eq!(sandbox_wire(), Wire::Connect);
        std::env::remove_var(WIRE_ENV);
    }
}
