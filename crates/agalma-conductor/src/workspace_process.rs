//! Process-bound workspace supervisor (B1.4, decisions D2/D3).
//!
//! The workspace component server (`agalma-workspace-server`) hosts the
//! git-backed `Workspace` backend. The conductor supervises it exactly like the
//! sandbox server: spawn as a process-group leader, wait for the `READY {json}`
//! handshake, connect a [`WorkspaceClient`], and tear the group down with
//! `SIGTERM`/`SIGKILL` evidence on drop. Supervision is shared with
//! [`crate::harness_process`] (one `spawn_supervised`/`stop_supervised` pair),
//! not duplicated.
//!
//! Transport selection is [`WorkspaceTransport`] (`AGALMA_WORKSPACE_TRANSPORT`):
//! `process` (default) or `in-process`. [`WorkspaceHandle`] exposes the full
//! workspace operation surface ([`WorkspaceOps`] + `WorkspaceApi`) by
//! delegating to whichever backend is bound, so phase/work logic is unchanged
//! (D5) — only the construction sites change. When bound to a process, the
//! boundary speaks [`workspace_wire`] (Connect by default; IPCX.4).
//!
//! The socket lives under the run state dir (`workspace.sock`);
//! [`WorkspaceProcess::spawn`] bounds the addressed path to `SUN_LEN` via
//! [`crate::phases::bounded_socket_path`] while the socket file still lands in
//! the state dir.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::{Checkout, ContractError, ExecutionId, MergeReceipt, WorkspaceApi};
use agalma_ipc::{CommandSpec, Wire, WorkspaceClient, WorkspaceOps, WIRE_ENV};
use agalma_workspace::server::server_command;
use agalma_workspace::Workspace;

use crate::harness_process::{
    resolve_server_binary, spawn_supervised, stop_supervised, READY_TIMEOUT,
};

/// Environment variable selecting the workspace transport.
pub const TRANSPORT_ENV: &str = "AGALMA_WORKSPACE_TRANSPORT";
/// Socket file name under the run state dir.
pub const WORKSPACE_SOCKET_NAME: &str = "workspace.sock";

/// The wire transport the workspace process boundary uses (IPCX.4).
///
/// The workspace boundary migrated to [`Wire::Connect`]; a parity run may set
/// [`agalma_ipc::WIRE_ENV`] to `framed` to compare the two wires against the
/// same scenario. The selected wire is also exported to the child process (see
/// [`WorkspaceProcess::spawn_with_spec_wire`]) so its server serves the matching
/// transport.
pub fn workspace_wire() -> Wire {
    Wire::from_env(Wire::Connect)
}

/// How the conductor reaches a workspace implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceTransport {
    /// Direct in-process trait call (unchanged phase logic).
    InProcess,
    /// Supervised component server over the selected wire transport
    /// ([`workspace_wire`], Connect by default since IPCX.4). This is the
    /// default.
    Process,
}

impl WorkspaceTransport {
    /// Read the transport from [`TRANSPORT_ENV`]; default `Process`, with an
    /// explicit `in-process` override (unknown/empty keeps the default).
    ///
    /// The default flipped to `Process` once the bounded socket scheme
    /// ([`crate::phases::bounded_socket_path`]) covered this boundary and a live
    /// process smoke proved the production spawn path (`agalma-fez`).
    /// `in-process` remains the opt-out for tests and direct in-process embedding.
    pub fn from_env() -> Self {
        match std::env::var(TRANSPORT_ENV).ok().as_deref() {
            Some("in-process") => WorkspaceTransport::InProcess,
            _ => WorkspaceTransport::Process,
        }
    }

    /// Stable selector string.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceTransport::InProcess => "in-process",
            WorkspaceTransport::Process => "process",
        }
    }
}

/// Launch spec for the workspace server bound to `socket`, rooted at `base_dir`.
pub fn workspace_server_spec(socket: &Path, base_dir: &Path) -> CommandSpec {
    server_command()
        .arg("--socket")
        .arg(socket.to_string_lossy().into_owned())
        .arg("--base-dir")
        .arg(base_dir.to_string_lossy().into_owned())
}

/// A supervised workspace component-server process.
pub struct WorkspaceProcess {
    socket: PathBuf,
    child: Child,
    wire: Wire,
    stopped: bool,
}

impl WorkspaceProcess {
    /// Spawn a workspace server rooted at `base_dir` and listen on
    /// `<base_dir>/workspace.sock` (over [`workspace_wire`]); wait for `READY`.
    ///
    /// The addressed socket path is bounded to `SUN_LEN` for long `base_dir`
    /// values; the socket file still lands under `base_dir`
    /// ([`crate::phases::bounded_socket_path`]).
    pub fn spawn(base_dir: &Path, timeout: Duration) -> Result<WorkspaceProcess, String> {
        let socket = crate::phases::bounded_socket_path(base_dir, WORKSPACE_SOCKET_NAME);
        let spec = workspace_server_spec(&socket, base_dir);
        let program = resolve_server_binary(&spec.program)?;
        WorkspaceProcess::spawn_with_spec_wire(spec, program, socket, workspace_wire(), timeout)
    }

    /// Spawn a workspace server from an explicit spec/program over
    /// [`workspace_wire`] (used by tests that re-exec the test binary as the
    /// server when the real binary is absent).
    pub fn spawn_with_spec(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        timeout: Duration,
    ) -> Result<WorkspaceProcess, String> {
        WorkspaceProcess::spawn_with_spec_wire(spec, program, socket, workspace_wire(), timeout)
    }

    /// Spawn a workspace server from an explicit spec/program over `wire`. The
    /// selected wire is exported to the child in [`WIRE_ENV`], so its server
    /// serves the matching transport.
    pub fn spawn_with_spec_wire(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        wire: Wire,
        timeout: Duration,
    ) -> Result<WorkspaceProcess, String> {
        let spec = spec.env(WIRE_ENV, wire.as_str());
        let (guard, _info) = spawn_supervised(&spec, &program, &socket, timeout)?;
        Ok(WorkspaceProcess {
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
    pub fn client(&self) -> Result<WorkspaceClient, String> {
        WorkspaceClient::connect_wire(&self.socket, self.wire).map_err(|e| e.to_string())
    }

    /// Stop the server with `SIGTERM`/`SIGKILL` evidence. Idempotent.
    pub fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "workspace server already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }
}

impl Drop for WorkspaceProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The operation backend: an in-process `Workspace` or a client to a supervised
/// server.
enum Backend {
    InProcess(Workspace),
    Process(WorkspaceClient),
}

/// A workspace bound either in-process or across the process boundary.
///
/// The phase/work call sites are unchanged because the handle exposes the same
/// surface as `Workspace`; only the constructed type differs at the construction
/// sites (D5). The optional `_process` field keeps the supervised server alive
/// for the handle's lifetime (and stops it on drop).
pub struct WorkspaceHandle {
    backend: Backend,
    _process: Option<Rc<WorkspaceProcess>>,
}

impl WorkspaceHandle {
    /// Bind the in-process git backend rooted at `base_dir`.
    pub fn in_process(base_dir: &Path) -> WorkspaceHandle {
        WorkspaceHandle {
            backend: Backend::InProcess(Workspace::new(base_dir)),
            _process: None,
        }
    }

    /// Spawn a supervised workspace server rooted at `base_dir` and connect.
    pub fn process(base_dir: &Path) -> Result<WorkspaceHandle, ContractError> {
        let process = WorkspaceProcess::spawn(base_dir, READY_TIMEOUT)
            .map_err(|e| ContractError::KnownFailure(format!("start workspace server: {e}")))?;
        let client = process
            .client()
            .map_err(|e| ContractError::KnownFailure(format!("connect workspace server: {e}")))?;
        Ok(WorkspaceHandle {
            backend: Backend::Process(client),
            _process: Some(Rc::new(process)),
        })
    }

    /// Build a handle for the selected `transport`.
    pub fn from_transport(
        base_dir: &Path,
        transport: WorkspaceTransport,
    ) -> Result<WorkspaceHandle, ContractError> {
        match transport {
            WorkspaceTransport::InProcess => Ok(WorkspaceHandle::in_process(base_dir)),
            WorkspaceTransport::Process => WorkspaceHandle::process(base_dir),
        }
    }

    fn backend(&self) -> &(dyn WorkspaceOps + 'static) {
        match &self.backend {
            Backend::InProcess(workspace) => workspace,
            Backend::Process(client) => client,
        }
    }

    fn backend_mut(&mut self) -> &mut (dyn WorkspaceOps + 'static) {
        match &mut self.backend {
            Backend::InProcess(workspace) => workspace,
            Backend::Process(client) => client,
        }
    }
}

impl WorkspaceApi for WorkspaceHandle {
    fn prepare(
        &mut self,
        template: &str,
        execution: &ExecutionId,
    ) -> Result<Checkout, ContractError> {
        self.backend_mut().prepare(template, execution)
    }

    fn diff(&self, checkout: &Checkout) -> Result<String, ContractError> {
        self.backend().diff(checkout)
    }

    fn integrate(
        &mut self,
        checkout: &Checkout,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        self.backend_mut().integrate(checkout, expected_main_sha)
    }

    fn tag(&mut self, name: &str, sha: &str) -> Result<(), ContractError> {
        self.backend_mut().tag(name, sha)
    }

    fn reconcile(&mut self, checkout: &Checkout) -> Result<bool, ContractError> {
        self.backend_mut().reconcile(checkout)
    }

    fn revert(&mut self, sha: &str) -> Result<(), ContractError> {
        self.backend_mut().revert(sha)
    }
}

impl WorkspaceOps for WorkspaceHandle {
    fn prepare_repo(
        &mut self,
        origin_path: &Path,
        base_ref: &str,
        execution: &ExecutionId,
        base_dir: &Path,
    ) -> Result<Checkout, ContractError> {
        self.backend_mut()
            .prepare_repo(origin_path, base_ref, execution, base_dir)
    }

    fn integrate_origin(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        self.backend()
            .integrate_origin(checkout, origin_path, expected_main_sha)
    }

    fn candidate_sha(&self, checkout: &Checkout) -> Result<String, ContractError> {
        self.backend().candidate_sha(checkout)
    }

    fn commit_candidate(&self, checkout: &Checkout) -> Result<String, ContractError> {
        self.backend().commit_candidate(checkout)
    }

    fn prepare_verify_checkout(
        &self,
        checkout: &Checkout,
        base_dir: &Path,
        execution: &ExecutionId,
        sha: &str,
    ) -> Result<PathBuf, ContractError> {
        self.backend()
            .prepare_verify_checkout(checkout, base_dir, execution, sha)
    }

    fn origin_receipt(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        self.backend().origin_receipt(checkout, origin_path)
    }

    fn revert_origin(
        &self,
        origin_path: &Path,
        to_sha: &str,
        from_sha: &str,
    ) -> Result<(), ContractError> {
        self.backend().revert_origin(origin_path, to_sha, from_sha)
    }

    fn integrated_receipt(
        &mut self,
        checkout: &Checkout,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        self.backend_mut().integrated_receipt(checkout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_from_env_defaults_to_process() {
        // Absent -> Process (post-B1-F3 default); explicit `in-process` overrides;
        // an unknown value keeps the default.
        std::env::remove_var(TRANSPORT_ENV);
        assert_eq!(
            WorkspaceTransport::from_env(),
            WorkspaceTransport::Process
        );
        std::env::set_var(TRANSPORT_ENV, "in-process");
        assert_eq!(
            WorkspaceTransport::from_env(),
            WorkspaceTransport::InProcess
        );
        std::env::set_var(TRANSPORT_ENV, "bogus");
        assert_eq!(
            WorkspaceTransport::from_env(),
            WorkspaceTransport::Process
        );
        std::env::remove_var(TRANSPORT_ENV);
    }

    #[test]
    fn spec_includes_socket_and_base_dir() {
        let spec = workspace_server_spec(Path::new("/run/s.sock"), Path::new("/run/state"));
        assert_eq!(spec.program, agalma_workspace::server::SERVER_BIN);
        assert_eq!(
            spec.args,
            vec!["--socket", "/run/s.sock", "--base-dir", "/run/state"]
        );
    }

    #[test]
    fn in_process_handle_is_constructed_without_a_server() {
        let handle = WorkspaceHandle::in_process(Path::new("/run/state"));
        assert!(handle._process.is_none());
    }

    #[test]
    fn wire_defaults_to_connect_and_env_restores_framed() {
        // Absent -> Connect (IPCX.4 migration default); `framed` for parity.
        std::env::remove_var(WIRE_ENV);
        assert_eq!(workspace_wire(), Wire::Connect);
        std::env::set_var(WIRE_ENV, "framed");
        assert_eq!(workspace_wire(), Wire::Framed);
        std::env::set_var(WIRE_ENV, "connect");
        assert_eq!(workspace_wire(), Wire::Connect);
        std::env::remove_var(WIRE_ENV);
    }
}
