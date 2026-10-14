//! Process-bound ledger supervisor (B1.6, decisions D2/D3).
//!
//! The durable-state component server (`agalma-ledger-server`) hosts the
//! SQLite-backed [`SqliteLedger`]. The conductor supervises it exactly like the
//! workspace/sandbox/task-queue servers: spawn as a process-group leader, wait
//! for the `READY {json}` handshake, connect a [`LedgerClient`], and tear the
//! group down with `SIGTERM`/`SIGKILL` evidence on drop. Supervision is shared
//! with [`crate::harness_process`] (one `spawn_supervised`/`stop_supervised`
//! pair), not duplicated.
//!
//! Transport selection is [`LedgerTransport`] (`AGALMA_LEDGER_TRANSPORT`):
//! `process` (default) or `in-process`. When a process binding is selected, its
//! wire is [`ledger_wire`] (`AGALMA_IPC_WIRE`): `Connect` (default, IPCX.6) or
//! `framed` for parity runs. [`LedgerHandle`] exposes the full
//! durable-state surface ([`LedgerApi`] + [`LedgerOps`]) by delegating to
//! whichever backend is bound, so executor/kernel construction is unchanged (D5)
//! — only the construction sites change. The handle is cheaply cloneable and
//! shares one backend, so every per-task executor in a work loop uses the same
//! durable-state owner (single writer per state generation).
//!
//! The socket lives under the run state dir (`ledger.sock`); [`ledger_socket`]
//! bounds the addressed path to `SUN_LEN` via [`crate::phases::bounded_socket_path`]
//! while the socket file still lands in the state dir.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::{
    BindingRecord, CommitBatch, CommitReceipt, ContractError, ControlReceipt, DecisionOutcome,
    DecisionRequest, DigestRecord, DirectiveRow, DispatchIntent, ExecutionEvent, ExecutionId,
    ExecutionRecord, LedgerApi, OperationId, OperationReceipt, StateSnapshot, TaskId, TimerRecord,
};
use agalma_ipc::{CommandSpec, LedgerClient, LedgerOps, Wire, WIRE_ENV};
use agalma_ledger::server::server_command;
use agalma_ledger::SqliteLedger;

use crate::harness_process::{
    resolve_server_binary, spawn_supervised, stop_supervised, READY_TIMEOUT,
};

/// Environment variable selecting the ledger transport.
pub const TRANSPORT_ENV: &str = "AGALMA_LEDGER_TRANSPORT";
/// Socket file name under the run state dir.
pub const LEDGER_SOCKET_NAME: &str = "ledger.sock";

/// How the conductor reaches a ledger implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerTransport {
    /// Direct in-process trait call (unchanged executor/kernel logic).
    InProcess,
    /// Supervised component server over the selected wire transport
    /// ([`ledger_wire`], Connect by default since IPCX.6). This is the default.
    Process,
}

impl LedgerTransport {
    /// Read the transport from [`TRANSPORT_ENV`]; default `Process`, with an
    /// explicit `in-process` override (unknown/empty keeps the default).
    ///
    /// The default flipped to `Process` once the bounded socket scheme
    /// ([`crate::phases::bounded_socket_path`]) covered this boundary and a live
    /// process smoke proved the production spawn path (`agalma-fez`).
    /// `in-process` remains the opt-out for tests and direct in-process embedding.
    pub fn from_env() -> Self {
        match std::env::var(TRANSPORT_ENV).ok().as_deref() {
            Some("in-process") => LedgerTransport::InProcess,
            _ => LedgerTransport::Process,
        }
    }

    /// Stable selector string.
    pub fn as_str(self) -> &'static str {
        match self {
            LedgerTransport::InProcess => "in-process",
            LedgerTransport::Process => "process",
        }
    }
}

/// The socket the conductor's ledger server binds under `state_dir`.
///
/// Bounded to `SUN_LEN` for long state dirs: when `<state_dir>/ledger.sock` is
/// too long the addressed path goes through a short alias, but the socket file
/// still lands under `state_dir` ([`crate::phases::bounded_socket_path`]).
pub fn ledger_socket(state_dir: &Path) -> PathBuf {
    crate::phases::bounded_socket_path(state_dir, LEDGER_SOCKET_NAME)
}

/// The wire transport the ledger process boundary uses (IPCX.6).
///
/// The ledger boundary migrated to [`Wire::Connect`]; a parity run may set
/// [`agalma_ipc::WIRE_ENV`] to `framed` to compare the two wires against the
/// same scenario. The selected wire is also exported to the child process (see
/// [`LedgerProcess::spawn_with_spec_wire`]) so its server serves the matching
/// transport.
pub fn ledger_wire() -> Wire {
    Wire::from_env(Wire::Connect)
}

/// Launch spec for the ledger server bound to `socket`, owning `state_dir`.
pub fn ledger_server_spec(socket: &Path, state_dir: &Path) -> CommandSpec {
    server_command()
        .arg("--socket")
        .arg(socket.to_string_lossy().into_owned())
        .arg("--state-dir")
        .arg(state_dir.to_string_lossy().into_owned())
}

/// A supervised ledger component-server process.
pub struct LedgerProcess {
    socket: PathBuf,
    state_dir: PathBuf,
    child: Child,
    wire: Wire,
    stopped: bool,
}

impl LedgerProcess {
    /// Spawn a ledger server owning `state_dir`, listening on
    /// `<state_dir>/ledger.sock` over [`ledger_wire`]; wait for `READY`.
    pub fn spawn(state_dir: &Path, timeout: Duration) -> Result<LedgerProcess, String> {
        let socket = ledger_socket(state_dir);
        let spec = ledger_server_spec(&socket, state_dir);
        let program = resolve_server_binary(&spec.program)?;
        LedgerProcess::spawn_with_spec_wire(
            spec,
            program,
            socket,
            state_dir.to_path_buf(),
            ledger_wire(),
            timeout,
        )
    }

    /// Spawn a ledger server from an explicit spec/program over [`ledger_wire`]
    /// (used by tests that re-exec the test binary as the server when the real
    /// binary is absent).
    pub fn spawn_with_spec(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        state_dir: PathBuf,
        timeout: Duration,
    ) -> Result<LedgerProcess, String> {
        LedgerProcess::spawn_with_spec_wire(
            spec,
            program,
            socket,
            state_dir,
            ledger_wire(),
            timeout,
        )
    }

    /// Spawn a ledger server from an explicit spec/program over `wire`. The
    /// selected wire is exported to the child in [`WIRE_ENV`], so its server
    /// serves the matching transport.
    pub fn spawn_with_spec_wire(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        state_dir: PathBuf,
        wire: Wire,
        timeout: Duration,
    ) -> Result<LedgerProcess, String> {
        let spec = spec.env(WIRE_ENV, wire.as_str());
        let (guard, _info) = spawn_supervised(&spec, &program, &socket, timeout)?;
        Ok(LedgerProcess {
            socket,
            state_dir,
            child: guard.into_child(),
            wire,
            stopped: false,
        })
    }

    /// The socket the server is listening on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The state dir the server owns.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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
    pub fn client(&self) -> Result<LedgerClient, String> {
        LedgerClient::connect_wire(&self.socket, &self.state_dir, self.wire)
            .map_err(|e| e.to_string())
    }

    /// Stop the server with `SIGTERM`/`SIGKILL` evidence. Idempotent.
    pub fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "ledger server already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }
}

impl Drop for LedgerProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The operation backend: an in-process `SqliteLedger` or a client to a
/// supervised server.
enum Backend {
    InProcess(SqliteLedger),
    Process(LedgerClient),
}

/// A ledger bound either in-process or across the process boundary.
///
/// Cloneable: every clone shares the same backend (and, for the process binding,
/// keeps the supervised server alive until the last clone drops), so a work loop
/// reuses one durable-state owner across per-task executors. `executor`/kernel
/// construction is unchanged because the handle exposes the same surface as
/// `SqliteLedger`; only the constructed type differs at the construction sites
/// (D5).
#[derive(Clone)]
pub struct LedgerHandle {
    backend: Rc<RefCell<Backend>>,
    _process: Option<Rc<LedgerProcess>>,
}

impl LedgerHandle {
    /// Bind the in-process SQLite backend owning `state_dir`.
    pub fn in_process(state_dir: &Path) -> Result<LedgerHandle, ContractError> {
        let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite"))?;
        Ok(LedgerHandle {
            backend: Rc::new(RefCell::new(Backend::InProcess(ledger))),
            _process: None,
        })
    }

    /// Spawn a supervised ledger server owning `state_dir` and connect.
    pub fn process(state_dir: &Path) -> Result<LedgerHandle, ContractError> {
        let process = LedgerProcess::spawn(state_dir, READY_TIMEOUT)
            .map_err(|e| ContractError::KnownFailure(format!("start ledger server: {e}")))?;
        let client = process
            .client()
            .map_err(|e| ContractError::KnownFailure(format!("connect ledger server: {e}")))?;
        Ok(LedgerHandle {
            backend: Rc::new(RefCell::new(Backend::Process(client))),
            _process: Some(Rc::new(process)),
        })
    }

    /// Bind the process backend to an already-spawned supervised server (used by
    /// callers/tests that spawn the server through the re-exec fallback rather
    /// than [`LedgerHandle::process`]'s binary lookup).
    pub fn from_process(process: LedgerProcess) -> Result<LedgerHandle, ContractError> {
        let client = process
            .client()
            .map_err(|e| ContractError::KnownFailure(format!("connect ledger server: {e}")))?;
        Ok(LedgerHandle {
            backend: Rc::new(RefCell::new(Backend::Process(client))),
            _process: Some(Rc::new(process)),
        })
    }

    /// Build a handle for the selected `transport`.
    pub fn from_transport(
        state_dir: &Path,
        transport: LedgerTransport,
    ) -> Result<LedgerHandle, ContractError> {
        match transport {
            LedgerTransport::InProcess => LedgerHandle::in_process(state_dir),
            LedgerTransport::Process => LedgerHandle::process(state_dir),
        }
    }

    fn borrow(&self) -> std::cell::RefMut<'_, Backend> {
        self.backend.borrow_mut()
    }
}

impl LedgerApi for LedgerHandle {
    fn commit(&mut self, batch: CommitBatch) -> Result<CommitReceipt, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.commit(batch),
            Backend::Process(c) => c.commit(batch),
        }
    }

    fn execution(&self, id: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.execution(id),
            Backend::Process(c) => c.execution(id),
        }
    }

    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.executions(),
            Backend::Process(c) => c.executions(),
        }
    }

    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.pending_intents(),
            Backend::Process(c) => c.pending_intents(),
        }
    }

    fn events(&self, id: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.events(id),
            Backend::Process(c) => c.events(id),
        }
    }

    fn operation_receipt(
        &self,
        id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.operation_receipt(id),
            Backend::Process(c) => c.operation_receipt(id),
        }
    }

    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.lease_generation(task),
            Backend::Process(c) => c.lease_generation(task),
        }
    }

    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.begin_lease(task),
            Backend::Process(c) => c.begin_lease(task),
        }
    }

    fn kill_latch(&self) -> Result<bool, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.kill_latch(),
            Backend::Process(c) => c.kill_latch(),
        }
    }

    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.set_kill_latch(latched),
            Backend::Process(c) => c.set_kill_latch(latched),
        }
    }

    fn upsert_binding(&mut self, binding: &BindingRecord) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.upsert_binding(binding),
            Backend::Process(c) => c.upsert_binding(binding),
        }
    }

    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.bindings(),
            Backend::Process(c) => c.bindings(),
        }
    }

    fn schema_version(&self) -> Result<u32, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.schema_version(),
            Backend::Process(c) => c.schema_version(),
        }
    }

    fn record_decision(&mut self, request: &DecisionRequest) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.record_decision(request),
            Backend::Process(c) => c.record_decision(request),
        }
    }

    fn record_decision_outcome(
        &mut self,
        operation_id: &OperationId,
        outcome: &DecisionOutcome,
    ) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.record_decision_outcome(operation_id, outcome),
            Backend::Process(c) => c.record_decision_outcome(operation_id, outcome),
        }
    }

    fn decision_outcome(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<DecisionOutcome>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.decision_outcome(operation_id),
            Backend::Process(c) => c.decision_outcome(operation_id),
        }
    }

    fn record_digest(&mut self, digest: &DigestRecord) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.record_digest(digest),
            Backend::Process(c) => c.record_digest(digest),
        }
    }

    fn digests_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.digests_for_execution(execution_id),
            Backend::Process(c) => c.digests_for_execution(execution_id),
        }
    }

    fn kernel_meta_get(&self, key: &str) -> Result<Option<String>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.kernel_meta_get(key),
            Backend::Process(c) => c.kernel_meta_get(key),
        }
    }

    fn kernel_meta_set(&mut self, key: &str, value: &str) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.kernel_meta_set(key, value),
            Backend::Process(c) => c.kernel_meta_set(key, value),
        }
    }

    fn insert_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.insert_timer(timer),
            Backend::Process(c) => c.insert_timer(timer),
        }
    }

    fn timer(&self, timer_id: &str) -> Result<Option<TimerRecord>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.timer(timer_id),
            Backend::Process(c) => c.timer(timer_id),
        }
    }

    fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.due_timers(now_unix_ms),
            Backend::Process(c) => c.due_timers(now_unix_ms),
        }
    }

    fn mark_timer_fired(&mut self, timer_id: &str) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.mark_timer_fired(timer_id),
            Backend::Process(c) => c.mark_timer_fired(timer_id),
        }
    }

    fn cancel_timers_for_execution(
        &mut self,
        execution_id: &ExecutionId,
    ) -> Result<u64, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.cancel_timers_for_execution(execution_id),
            Backend::Process(c) => c.cancel_timers_for_execution(execution_id),
        }
    }

    fn next_deadline(&self) -> Result<Option<u64>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.next_deadline(),
            Backend::Process(c) => c.next_deadline(),
        }
    }

    fn upsert_directive(&mut self, directive: &DirectiveRow) -> Result<bool, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.upsert_directive(directive),
            Backend::Process(c) => c.upsert_directive(directive),
        }
    }

    fn directive(&self, directive_id: &str) -> Result<Option<DirectiveRow>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.directive(directive_id),
            Backend::Process(c) => c.directive(directive_id),
        }
    }

    fn consume_dispatch_intent(&mut self, operation_id: &OperationId) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.consume_dispatch_intent(operation_id),
            Backend::Process(c) => c.consume_dispatch_intent(operation_id),
        }
    }

    fn record_control_receipt(&mut self, receipt: &ControlReceipt) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.record_control_receipt(receipt),
            Backend::Process(c) => c.record_control_receipt(receipt),
        }
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.control_receipts(),
            Backend::Process(c) => c.control_receipts(),
        }
    }

    fn export_state(&self) -> Result<StateSnapshot, ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.export_state(),
            Backend::Process(c) => c.export_state(),
        }
    }

    fn import_state(&mut self, snapshot: &StateSnapshot) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.import_state(snapshot),
            Backend::Process(c) => c.import_state(snapshot),
        }
    }
}

impl LedgerOps for LedgerHandle {
    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.export_snapshot_to(path),
            Backend::Process(c) => c.export_snapshot_to(path),
        }
    }

    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError> {
        match &mut *self.borrow() {
            Backend::InProcess(l) => l.import_snapshot_from(path),
            Backend::Process(c) => c.import_snapshot_from(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_testkit::ScratchDir;

    #[test]
    fn transport_from_env_defaults_to_process() {
        // Absent -> Process (post-B1-F3 default); explicit `in-process` overrides;
        // an unknown value keeps the default.
        std::env::remove_var(TRANSPORT_ENV);
        assert_eq!(LedgerTransport::from_env(), LedgerTransport::Process);
        std::env::set_var(TRANSPORT_ENV, "in-process");
        assert_eq!(LedgerTransport::from_env(), LedgerTransport::InProcess);
        std::env::set_var(TRANSPORT_ENV, "bogus");
        assert_eq!(LedgerTransport::from_env(), LedgerTransport::Process);
        std::env::remove_var(TRANSPORT_ENV);
    }

    #[test]
    fn spec_includes_socket_and_state_dir() {
        let spec = ledger_server_spec(Path::new("/run/l.sock"), Path::new("/run/state"));
        assert_eq!(spec.program, agalma_ledger::server::SERVER_BIN);
        assert_eq!(
            spec.args,
            vec!["--socket", "/run/l.sock", "--state-dir", "/run/state"]
        );
    }

    #[test]
    fn in_process_handle_is_constructed_without_a_server() {
        let dir = ScratchDir::new("ledger-handle");
        let handle = LedgerHandle::in_process(&dir).expect("open ledger");
        assert!(handle._process.is_none());
    }

    #[test]
    fn wire_defaults_to_connect_and_env_restores_framed() {
        // Absent -> Connect (IPCX.6 migration default); `framed` for parity.
        std::env::remove_var(WIRE_ENV);
        assert_eq!(ledger_wire(), Wire::Connect);
        std::env::set_var(WIRE_ENV, "framed");
        assert_eq!(ledger_wire(), Wire::Framed);
        std::env::set_var(WIRE_ENV, "connect");
        assert_eq!(ledger_wire(), Wire::Connect);
        std::env::remove_var(WIRE_ENV);
    }
}
