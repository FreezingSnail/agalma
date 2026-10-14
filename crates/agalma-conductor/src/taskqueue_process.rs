//! Process-bound task-queue supervisor (B1.5, decisions D2/D3).
//!
//! The task-queue component server (`agalma-taskqueue-server`) hosts the `bd`
//! backlog adapter. The conductor supervises it exactly like the
//! workspace/sandbox servers: spawn as a process-group leader, wait for the
//! `READY {json}` handshake, connect a [`TaskQueueClient`], and tear the group
//! down with `SIGTERM`/`SIGKILL` evidence on drop. Supervision is shared with
//! [`crate::harness_process`] (one `spawn_supervised`/`stop_supervised` pair),
//! not duplicated.
//!
//! Transport selection is [`TaskQueueTransport`] (`AGALMA_TASKQUEUE_TRANSPORT`):
//! `process` (default) or `in-process`. [`TaskQueueHandle`] exposes the full
//! task-queue operation surface ([`TaskQueueOps`] + `TaskQueueApi`) by delegating
//! to whichever backend is bound, so work/reconcile logic is unchanged (D5) —
//! only the construction sites change. The process wire defaults to
//! [`Wire::Connect`] (IPCX.5); `AGALMA_IPC_WIRE=framed` restores framed for
//! parity runs.
//!
//! The socket lives under the run state dir (`taskqueue.sock`);
//! [`TaskQueueProcess::spawn`] bounds the addressed path to `SUN_LEN` via
//! [`crate::phases::bounded_socket_path`] while the socket file still lands in
//! the state dir.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::{ClaimEvidence, ContractError, ReadyTasks, Task, TaskId, TaskQueueApi};
use agalma_ipc::{CommandSpec, IssueProjection, TaskQueueClient, TaskQueueOps, Wire, WIRE_ENV};
use agalma_taskqueue::server::server_command;
use agalma_taskqueue::BdTaskQueue;

use crate::harness_process::{
    resolve_server_binary, spawn_supervised, stop_supervised, READY_TIMEOUT,
};

/// Environment variable selecting the task-queue transport.
pub const TRANSPORT_ENV: &str = "AGALMA_TASKQUEUE_TRANSPORT";
/// Socket file name under the run state dir.
pub const TASKQUEUE_SOCKET_NAME: &str = "taskqueue.sock";

/// The wire transport the task-queue process boundary uses (IPCX.5).
///
/// The task-queue boundary migrated to [`Wire::Connect`]; a parity run may set
/// [`agalma_ipc::WIRE_ENV`] to `framed` to compare the two wires against the
/// same scenario. The selected wire is also exported to the child process (see
/// [`TaskQueueProcess::spawn_with_spec_wire`]) so its server serves the matching
/// transport.
pub fn taskqueue_wire() -> Wire {
    Wire::from_env(Wire::Connect)
}

/// How the conductor reaches a task-queue implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskQueueTransport {
    /// Direct in-process trait call (unchanged work/reconcile logic).
    InProcess,
    /// Supervised component server over the selected wire transport. This is the
    /// default.
    Process,
}

impl TaskQueueTransport {
    /// Read the transport from [`TRANSPORT_ENV`]; default `Process`, with an
    /// explicit `in-process` override (unknown/empty keeps the default).
    ///
    /// The default flipped to `Process` once the bounded socket scheme
    /// ([`crate::phases::bounded_socket_path`]) covered this boundary and a live
    /// process smoke proved the production spawn path (`agalma-fez`).
    /// `in-process` remains the opt-out for tests and direct in-process embedding.
    pub fn from_env() -> Self {
        match std::env::var(TRANSPORT_ENV).ok().as_deref() {
            Some("in-process") => TaskQueueTransport::InProcess,
            _ => TaskQueueTransport::Process,
        }
    }

    /// Stable selector string.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskQueueTransport::InProcess => "in-process",
            TaskQueueTransport::Process => "process",
        }
    }
}

/// Launch spec for the task-queue server bound to `socket`, serving the backlog
/// in `repo`.
pub fn taskqueue_server_spec(socket: &Path, repo: &Path) -> CommandSpec {
    server_command()
        .arg("--socket")
        .arg(socket.to_string_lossy().into_owned())
        .arg("--repo")
        .arg(repo.to_string_lossy().into_owned())
}

/// A supervised task-queue component-server process.
pub struct TaskQueueProcess {
    socket: PathBuf,
    child: Child,
    wire: Wire,
    stopped: bool,
}

impl TaskQueueProcess {
    /// Spawn a task-queue server serving `repo`'s backlog and listen on
    /// `<state_dir>/taskqueue.sock` (over [`taskqueue_wire`]); wait for `READY`.
    ///
    /// The addressed socket path is bounded to `SUN_LEN` for long `state_dir`
    /// values; the socket file still lands under `state_dir`
    /// ([`crate::phases::bounded_socket_path`]).
    pub fn spawn(
        state_dir: &Path,
        repo: &Path,
        timeout: Duration,
    ) -> Result<TaskQueueProcess, String> {
        let socket = crate::phases::bounded_socket_path(state_dir, TASKQUEUE_SOCKET_NAME);
        let spec = taskqueue_server_spec(&socket, repo);
        let program = resolve_server_binary(&spec.program)?;
        TaskQueueProcess::spawn_with_spec_wire(spec, program, socket, taskqueue_wire(), timeout)
    }

    /// Spawn a task-queue server from an explicit spec/program over
    /// [`taskqueue_wire`] (used by tests that re-exec the test binary as the
    /// server when the real binary is absent).
    pub fn spawn_with_spec(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        timeout: Duration,
    ) -> Result<TaskQueueProcess, String> {
        TaskQueueProcess::spawn_with_spec_wire(spec, program, socket, taskqueue_wire(), timeout)
    }

    /// Spawn a task-queue server from an explicit spec/program over `wire`. The
    /// selected wire is exported to the child in [`WIRE_ENV`], so its server
    /// serves the matching transport.
    pub fn spawn_with_spec_wire(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        wire: Wire,
        timeout: Duration,
    ) -> Result<TaskQueueProcess, String> {
        let spec = spec.env(WIRE_ENV, wire.as_str());
        let (guard, _info) = spawn_supervised(&spec, &program, &socket, timeout)?;
        Ok(TaskQueueProcess {
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
    pub fn client(&self) -> Result<TaskQueueClient, String> {
        TaskQueueClient::connect_wire(&self.socket, self.wire).map_err(|e| e.to_string())
    }

    /// Stop the server with `SIGTERM`/`SIGKILL` evidence. Idempotent.
    pub fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "taskqueue server already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }
}

impl Drop for TaskQueueProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The operation backend: an in-process `BdTaskQueue` or a client to a
/// supervised server.
enum Backend {
    InProcess(BdTaskQueue),
    Process(TaskQueueClient),
}

/// A task queue bound either in-process or across the process boundary.
///
/// The work/reconcile call sites are unchanged because the handle exposes the
/// same surface as `BdTaskQueue`; only the constructed type differs at the
/// construction sites (D5). The optional `_process` field keeps the supervised
/// server alive for the handle's lifetime (and stops it on drop).
pub struct TaskQueueHandle {
    backend: Backend,
    _process: Option<Rc<TaskQueueProcess>>,
}

impl TaskQueueHandle {
    /// Bind the in-process `bd` backend rooted at `repo`.
    pub fn in_process(repo: &Path) -> TaskQueueHandle {
        TaskQueueHandle {
            backend: Backend::InProcess(BdTaskQueue::new(repo)),
            _process: None,
        }
    }

    /// Spawn a supervised task-queue server serving `repo`, listening on
    /// `<state_dir>/taskqueue.sock`, and connect.
    pub fn process(state_dir: &Path, repo: &Path) -> Result<TaskQueueHandle, ContractError> {
        let process = TaskQueueProcess::spawn(state_dir, repo, READY_TIMEOUT)
            .map_err(|e| ContractError::KnownFailure(format!("start taskqueue server: {e}")))?;
        let client = process
            .client()
            .map_err(|e| ContractError::KnownFailure(format!("connect taskqueue server: {e}")))?;
        Ok(TaskQueueHandle {
            backend: Backend::Process(client),
            _process: Some(Rc::new(process)),
        })
    }

    /// Build a handle for the selected `transport`.
    pub fn from_transport(
        state_dir: &Path,
        repo: &Path,
        transport: TaskQueueTransport,
    ) -> Result<TaskQueueHandle, ContractError> {
        match transport {
            TaskQueueTransport::InProcess => Ok(TaskQueueHandle::in_process(repo)),
            TaskQueueTransport::Process => TaskQueueHandle::process(state_dir, repo),
        }
    }

    fn backend(&self) -> &(dyn TaskQueueOps + 'static) {
        match &self.backend {
            Backend::InProcess(queue) => queue,
            Backend::Process(client) => client,
        }
    }

    fn backend_mut(&mut self) -> &mut (dyn TaskQueueOps + 'static) {
        match &mut self.backend {
            Backend::InProcess(queue) => queue,
            Backend::Process(client) => client,
        }
    }
}

impl TaskQueueApi for TaskQueueHandle {
    fn ready_tasks(&self) -> Result<ReadyTasks, ContractError> {
        self.backend().ready_tasks()
    }

    fn get(&self, id: &TaskId) -> Result<Task, ContractError> {
        self.backend().get(id)
    }

    fn claim(&mut self, id: &TaskId) -> Result<ClaimEvidence, ContractError> {
        self.backend_mut().claim(id)
    }

    fn comment(&mut self, id: &TaskId, text: &str) -> Result<(), ContractError> {
        self.backend_mut().comment(id, text)
    }

    fn close_task(&mut self, id: &TaskId, reason: &str) -> Result<(), ContractError> {
        self.backend_mut().close_task(id, reason)
    }

    fn add_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        self.backend_mut().add_label(id, label)
    }
}

impl TaskQueueOps for TaskQueueHandle {
    fn projection(&self, id: &TaskId) -> Result<IssueProjection, ContractError> {
        self.backend().projection(id)
    }

    fn set_status(&mut self, id: &TaskId, status: &str) -> Result<(), ContractError> {
        self.backend_mut().set_status(id, status)
    }

    fn remove_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        self.backend_mut().remove_label(id, label)
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
            TaskQueueTransport::from_env(),
            TaskQueueTransport::Process
        );
        std::env::set_var(TRANSPORT_ENV, "in-process");
        assert_eq!(
            TaskQueueTransport::from_env(),
            TaskQueueTransport::InProcess
        );
        std::env::set_var(TRANSPORT_ENV, "bogus");
        assert_eq!(
            TaskQueueTransport::from_env(),
            TaskQueueTransport::Process
        );
        std::env::remove_var(TRANSPORT_ENV);
    }

    #[test]
    fn spec_includes_socket_and_repo() {
        let spec = taskqueue_server_spec(Path::new("/run/s.sock"), Path::new("/run/repo"));
        assert_eq!(spec.program, agalma_taskqueue::server::SERVER_BIN);
        assert_eq!(
            spec.args,
            vec!["--socket", "/run/s.sock", "--repo", "/run/repo"]
        );
    }

    #[test]
    fn in_process_handle_is_constructed_without_a_server() {
        let handle = TaskQueueHandle::in_process(Path::new("/run/repo"));
        assert!(handle._process.is_none());
    }

    #[test]
    fn wire_defaults_to_connect_and_env_restores_framed() {
        // Absent -> Connect (IPCX.5 migration default); `framed` for parity.
        std::env::remove_var(WIRE_ENV);
        assert_eq!(taskqueue_wire(), Wire::Connect);
        std::env::set_var(WIRE_ENV, "framed");
        assert_eq!(taskqueue_wire(), Wire::Framed);
        std::env::set_var(WIRE_ENV, "connect");
        assert_eq!(taskqueue_wire(), Wire::Connect);
        std::env::remove_var(WIRE_ENV);
    }
}
