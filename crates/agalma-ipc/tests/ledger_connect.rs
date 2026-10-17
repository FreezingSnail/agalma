//! Ledger boundary over the Connect transport (IPCX.6).
//!
//! The typed [`LedgerApi`]/[`LedgerOps`] client (`LedgerClient`) is exercised
//! against a `register_ledger` server served over both wires: the B1 framed
//! transport and the IPCX Connect/UDS transport. A deterministic stub ledger
//! drives a representative durable-state scenario (lease, kernel meta, schema
//! version, control receipts from agalma-3kr.6, and the file-based snapshot
//! export); the two wires must produce an identical typed transcript, proving
//! the ledger boundary works over Connect with parity to framed.
//!
//! Snapshot transfer stays file-based on both wires (D3): the payload never
//! becomes a frame — only the path crosses the wire.
//!
//! Servers bind under `target/test-runs/` — never `/tmp`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::{
    BindingRecord, CommitBatch, CommitReceipt, ContractError, ControlReceipt, DecisionOutcome,
    DecisionRequest, DigestRecord, DirectiveRow, DispatchIntent, ExecutionEvent, ExecutionId,
    ExecutionRecord, LedgerApi, OperationId, OperationReceipt, StateSnapshot, TaskId, TimerRecord,
    SNAPSHOT_FORMAT,
};
use agalma_ipc::ledger_client::LedgerOps;
use agalma_ipc::ledger_server::{
    ensure_all_handlers, register_ledger, serve_ledger, LEDGER_METHODS,
};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::{LedgerClient, Wire};
use agalma_testkit::{retry_connect, ScratchDir};

/// A deterministic in-memory ledger: fixed schema version, no clock, no SQLite.
///
/// Only the methods the parity transcript drives are implemented; the rest are
/// unreachable in this test and panic if called (never reached).
#[derive(Default)]
struct StubLedger {
    schema_version: u32,
    kernel_meta: BTreeMap<String, String>,
    leases: BTreeMap<String, u32>,
    receipts: Vec<ControlReceipt>,
}

impl StubLedger {
    fn new() -> StubLedger {
        StubLedger {
            schema_version: 4,
            ..Default::default()
        }
    }

    fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            format: SNAPSHOT_FORMAT.to_string(),
            schema_version: self.schema_version,
            exec_def_version: String::new(),
            kernel_version: self
                .kernel_meta
                .get("kernel_version")
                .cloned()
                .unwrap_or_default(),
            kill_latch: false,
            kernel_meta: self.kernel_meta.clone(),
            lease_generations: self.leases.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            executions: Vec::new(),
            events: Vec::new(),
            receipts: Vec::new(),
            intents: Vec::new(),
            timers: Vec::new(),
            directives: Vec::new(),
            bindings: Vec::new(),
        }
    }
}

impl LedgerApi for StubLedger {
    fn schema_version(&self) -> Result<u32, ContractError> {
        Ok(self.schema_version)
    }

    fn kernel_meta_get(&self, key: &str) -> Result<Option<String>, ContractError> {
        Ok(self.kernel_meta.get(key).cloned())
    }

    fn kernel_meta_set(&mut self, key: &str, value: &str) -> Result<(), ContractError> {
        self.kernel_meta.insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        Ok(self.leases.get(task.as_str()).copied().unwrap_or(0))
    }

    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        let entry = self.leases.entry(task.as_str().to_string()).or_insert(0);
        *entry += 1;
        Ok(*entry)
    }

    fn record_control_receipt(&mut self, receipt: &ControlReceipt) -> Result<(), ContractError> {
        if !self
            .receipts
            .iter()
            .any(|r| r.action_id == receipt.action_id)
        {
            self.receipts.push(receipt.clone());
        }
        Ok(())
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        Ok(self.receipts.clone())
    }

    fn export_state(&self) -> Result<StateSnapshot, ContractError> {
        Ok(self.snapshot())
    }

    fn import_state(&mut self, snapshot: &StateSnapshot) -> Result<(), ContractError> {
        self.schema_version = snapshot.schema_version;
        self.kernel_meta = snapshot.kernel_meta.clone();
        self.leases = snapshot.lease_generations.clone().into_iter().collect();
        Ok(())
    }

    // ---- not exercised by the parity transcript -------------------------
    fn commit(&mut self, _batch: CommitBatch) -> Result<CommitReceipt, ContractError> {
        unimplemented!("stub ledger: commit unused")
    }
    fn execution(&self, _id: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError> {
        unimplemented!("stub ledger: execution unused")
    }
    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError> {
        unimplemented!("stub ledger: executions unused")
    }
    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        unimplemented!("stub ledger: pending_intents unused")
    }
    fn events(&self, _id: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError> {
        unimplemented!("stub ledger: events unused")
    }
    fn operation_receipt(
        &self,
        _id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        unimplemented!("stub ledger: operation_receipt unused")
    }
    fn kill_latch(&self) -> Result<bool, ContractError> {
        unimplemented!("stub ledger: kill_latch unused")
    }
    fn set_kill_latch(&mut self, _latched: bool) -> Result<(), ContractError> {
        unimplemented!("stub ledger: set_kill_latch unused")
    }
    fn upsert_binding(&mut self, _binding: &BindingRecord) -> Result<(), ContractError> {
        unimplemented!("stub ledger: upsert_binding unused")
    }
    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError> {
        unimplemented!("stub ledger: bindings unused")
    }
    fn record_decision(&mut self, _request: &DecisionRequest) -> Result<(), ContractError> {
        unimplemented!("stub ledger: record_decision unused")
    }
    fn record_decision_outcome(
        &mut self,
        _operation_id: &OperationId,
        _outcome: &DecisionOutcome,
    ) -> Result<(), ContractError> {
        unimplemented!("stub ledger: record_decision_outcome unused")
    }
    fn decision_outcome(
        &self,
        _operation_id: &OperationId,
    ) -> Result<Option<DecisionOutcome>, ContractError> {
        unimplemented!("stub ledger: decision_outcome unused")
    }
    fn record_digest(&mut self, _digest: &DigestRecord) -> Result<(), ContractError> {
        unimplemented!("stub ledger: record_digest unused")
    }
    fn digests_for_execution(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError> {
        unimplemented!("stub ledger: digests_for_execution unused")
    }
    fn insert_timer(&mut self, _timer: &TimerRecord) -> Result<bool, ContractError> {
        unimplemented!("stub ledger: insert_timer unused")
    }
    fn timer(&self, _timer_id: &str) -> Result<Option<TimerRecord>, ContractError> {
        unimplemented!("stub ledger: timer unused")
    }
    fn due_timers(&self, _now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        unimplemented!("stub ledger: due_timers unused")
    }
    fn mark_timer_fired(&mut self, _timer_id: &str) -> Result<(), ContractError> {
        unimplemented!("stub ledger: mark_timer_fired unused")
    }
    fn cancel_timers_for_execution(
        &mut self,
        _execution_id: &ExecutionId,
    ) -> Result<u64, ContractError> {
        unimplemented!("stub ledger: cancel_timers_for_execution unused")
    }
    fn next_deadline(&self) -> Result<Option<u64>, ContractError> {
        unimplemented!("stub ledger: next_deadline unused")
    }
    fn upsert_directive(&mut self, _directive: &DirectiveRow) -> Result<bool, ContractError> {
        unimplemented!("stub ledger: upsert_directive unused")
    }
    fn directive(&self, _directive_id: &str) -> Result<Option<DirectiveRow>, ContractError> {
        unimplemented!("stub ledger: directive unused")
    }
    fn consume_dispatch_intent(
        &mut self,
        _operation_id: &OperationId,
    ) -> Result<(), ContractError> {
        unimplemented!("stub ledger: consume_dispatch_intent unused")
    }
}

impl LedgerOps for StubLedger {
    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError> {
        let json = self.snapshot().to_canonical_json()?;
        std::fs::write(path, json)
            .map_err(|e| ContractError::KnownFailure(format!("write snapshot: {e}")))
    }

    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ContractError::KnownFailure(format!("read snapshot: {e}")))?;
        let snapshot = StateSnapshot::from_json(&text)?;
        self.import_state(&snapshot)
    }
}

fn ledger_info() -> ServerInfo {
    ServerInfo {
        api: "LedgerApi".to_string(),
        api_version: 0,
        impl_name: "stub".to_string(),
        impl_version: "stub/v1".to_string(),
        capabilities: BTreeSet::new(),
    }
}

fn spawn_ledger_server(socket: PathBuf, wire: Wire) {
    let mut server = Server::new(ledger_info());
    register_ledger(&mut server, StubLedger::new());
    ensure_all_handlers(&server).expect("every ledger method registered");
    std::thread::spawn(move || {
        let _ = serve_ledger(server, &socket, wire);
    });
}

/// Poll until the server has bound the socket (the listener is created before
/// the `READY` line, so file existence implies it is accepting).
fn wait_for_socket(socket: &Path) {
    for _ in 0..500 {
        if socket.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("server never bound {}", socket.display());
}

/// Drive the stub through the tested ledger surface and render a timing-free
/// transcript (lease, kernel meta, schema version, control receipts, snapshot
/// file export).
fn run_stub_scenario(client: &mut LedgerClient) -> Vec<String> {
    let task = TaskId::new("stub-task");
    let lease = client.begin_lease(&task).expect("begin_lease");
    let lease_read = client.lease_generation(&task).expect("lease_generation");

    client
        .kernel_meta_set("kernel_version", "stub/v1")
        .expect("kernel_meta_set");
    let meta = client
        .kernel_meta_get("kernel_version")
        .expect("kernel_meta_get");

    let applied = ControlReceipt {
        action_id: "cancel-1".to_string(),
        action: "cancel".to_string(),
        target: "exec:stub:1".to_string(),
        actor: "human".to_string(),
        request: serde_json::json!({ "outcome": "applied" }),
        recorded_at_unix_ms: 1,
    };
    let denied = ControlReceipt {
        action_id: "pause-2".to_string(),
        action: "pause".to_string(),
        target: String::new(),
        actor: "human".to_string(),
        request: serde_json::json!({ "outcome": "denied" }),
        recorded_at_unix_ms: 2,
    };
    client
        .record_control_receipt(&applied)
        .expect("record applied");
    client
        .record_control_receipt(&denied)
        .expect("record denied");
    let receipts = client.control_receipts().expect("control_receipts");

    let schema_version = client.schema_version().expect("schema_version");
    let snapshot = client.export_state().expect("export_state (file-based)");

    vec![
        format!("lease={lease}/{lease_read}"),
        format!("meta={meta:?}"),
        format!("receipts={receipts:?}"),
        format!("schema_version={schema_version}"),
        format!("snapshot={snapshot:?}"),
    ]
}

#[test]
fn ledger_api_over_connect_matches_framed() {
    let dir = ScratchDir::new("ipcx-ledger");
    let framed_socket = dir.join("framed.sock");
    let connect_socket = dir.join("connect.sock");

    spawn_ledger_server(framed_socket.clone(), Wire::Framed);
    spawn_ledger_server(connect_socket.clone(), Wire::Connect);
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let mut framed =
        retry_connect(|| LedgerClient::connect_wire(&framed_socket, &dir, Wire::Framed))
            .expect("framed connect");
    let mut connect =
        retry_connect(|| LedgerClient::connect_wire(&connect_socket, &dir, Wire::Connect))
            .expect("connect connect");

    // Every typed call produces the same transcript on both wires.
    assert_eq!(
        run_stub_scenario(&mut framed),
        run_stub_scenario(&mut connect)
    );
}

#[test]
fn ledger_connect_missing_socket_is_unavailable() {
    let dir = ScratchDir::new("ipcx-ledger-miss");
    let err = LedgerClient::connect_wire(&dir.join("nope.sock"), &dir, Wire::Connect)
        .err()
        .expect("connect to a missing socket must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}

/// The wire method surface stays consistent: it includes the agalma-3kr.6
/// control-receipt ops, the HP.5 snapshot file transfer, and the HP.5
/// component-store + complete-state ops.
#[test]
fn ledger_methods_include_control_receipt_ops() {
    assert_eq!(LEDGER_METHODS.len(), 39);
    assert!(LEDGER_METHODS.contains(&"record_control_receipt"));
    assert!(LEDGER_METHODS.contains(&"control_receipts"));
    assert!(LEDGER_METHODS.contains(&"export_state_to"));
    assert!(LEDGER_METHODS.contains(&"import_state_from"));
    for method in [
        "component_read",
        "component_events",
        "component_operation",
        "component_commit",
        "export_complete_state_to",
        "import_complete_state_from",
    ] {
        assert!(LEDGER_METHODS.contains(&method), "missing `{method}`");
    }
}
