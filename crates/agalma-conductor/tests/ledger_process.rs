//! B1.6 ledger process-boundary evidence (`agalma-0ht.7`).
//!
//! Dual-binding evidence for the extracted `LedgerApi` (durable state — the
//! highest-ceremony boundary): shadow-parity runs of the same ledger scenario
//! in-process vs through the supervised ledger component server (comparing
//! receipts, pending work, a normalized exported snapshot, and the schema
//! version); a quiesced rebind that exports the canonical snapshot and starts a
//! fresh server from it with an in-flight pinned operation preserved; a rollback
//! without state loss; and IPC conformance for [`agalma_ipc::LedgerClient`]
//! (duplicate-id replay of `commit`, dead-server
//! `Unavailable`/`UnknownOutcome` with reconciliation through inspection APIs,
//! and a refused incompatible snapshot import).
//!
//! Scratch state dirs live under `target/test-runs/lp-*` (never `/tmp`). The
//! server child is re-exec'd from this test binary (or the real
//! `agalma-ledger-server` binary when built) so a package-scoped
//! `cargo test -p agalma-conductor` run still exercises the real process
//! boundary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agalma_contracts::{
    BindingId, BindingRecord, BindingState, CommitBatch, CommitReceipt, ContractError, DispatchIntent,
    ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord, ExecutionState, LedgerApi,
    OperationId, OperationReceipt, StateSnapshot, TaskId, TimerRecord, TimerState,
};
use agalma_ipc::{Client, CommandSpec, CommitParams, IpcError, LedgerClient, LedgerOps};
use agalma_ledger::server::SERVER_BIN;
use agalma_ledger::SqliteLedger;

use agalma_conductor::harness_process::resolve_server_binary;
use agalma_conductor::ledger_process::{LedgerProcess, LedgerTransport};

const CHILD_SOCKET_ENV: &str = "AGALMA_TEST_LEDGER_SOCKET";
const CHILD_STATE_DIR_ENV: &str = "AGALMA_TEST_LEDGER_STATE_DIR";
const CHILD_TEST_NAME: &str = "ledger_server_child";
const READY_TIMEOUT: Duration = Duration::from_secs(20);

// ===========================================================================
// Fixtures + helpers
// ===========================================================================

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

/// Short, unique run directory. Unix socket paths must fit `SUN_LEN` (~104
/// bytes), so sockets live directly under a short `lp-<pid>-<n>` directory.
static RUN_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fresh() -> PathBuf {
    let n = RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("lp-{}-{n}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn task() -> TaskId {
    TaskId::new("fix-parity")
}

fn exec() -> ExecutionId {
    ExecutionId::new("exec:fix-parity:1")
}

fn op(name: &str) -> OperationId {
    OperationId::new(format!("op:exec:fix-parity:1:{name}"))
}

fn exec_record() -> ExecutionRecord {
    ExecutionRecord {
        execution_id: exec(),
        task_id: task(),
        generation: 1,
        phase: ExecutionPhase::Build,
        state: ExecutionState::Running,
        attempt: 1,
        revision: 0,
    }
}

fn event(kind: &str, payload: serde_json::Value) -> ExecutionEvent {
    ExecutionEvent {
        execution_id: exec(),
        sequence: 0,
        kind: kind.to_string(),
        payload,
        recorded_at_unix_ms: 0,
    }
}

fn intent(operation: &OperationId, payload: serde_json::Value) -> DispatchIntent {
    DispatchIntent {
        operation_id: operation.clone(),
        execution_id: exec(),
        payload,
        enqueued_at_unix_ms: 0,
        consumed: false,
    }
}

fn receipt(operation: &OperationId, result: serde_json::Value) -> OperationReceipt {
    OperationReceipt {
        operation_id: operation.clone(),
        execution_id: exec(),
        kind: "build".to_string(),
        inputs_hash: format!("hash:{}", operation.as_str()),
        result,
        completed_at_unix_ms: 0,
    }
}

fn binding() -> BindingRecord {
    BindingRecord {
        binding_id: BindingId::new("harness:parity"),
        component_api: "HarnessApi".to_string(),
        api_version: 0,
        implementation: "reference".to_string(),
        implementation_version: "ref/v1".to_string(),
        capabilities: Default::default(),
        required_capabilities: Default::default(),
        optional_capabilities: Default::default(),
        config_hash: "cfg:parity".to_string(),
        generation: 1,
        state: BindingState::Active,
    }
}

fn timer() -> TimerRecord {
    TimerRecord {
        timer_id: "timer:parity".to_string(),
        execution_id: exec(),
        kind: "deadline".to_string(),
        fire_at_unix_ms: 1,
        state: TimerState::Pending,
        payload: serde_json::json!({ "cause": "parity" }),
    }
}

/// Zero every wall-clock field so the snapshot is comparable across bindings.
fn normalize(mut snapshot: StateSnapshot) -> StateSnapshot {
    for e in &mut snapshot.events {
        e.recorded_at_unix_ms = 0;
    }
    for r in &mut snapshot.receipts {
        r.completed_at_unix_ms = 0;
    }
    for i in &mut snapshot.intents {
        i.enqueued_at_unix_ms = 0;
    }
    for t in &mut snapshot.timers {
        t.fire_at_unix_ms = 0;
    }
    for d in &mut snapshot.directives {
        d.received_at_unix_ms = 0;
    }
    snapshot
}

/// A normalized, timing-free transcript of one ledger scenario.
#[derive(Debug, PartialEq, Eq)]
struct Transcript {
    lease: u32,
    commit1_revision: u64,
    pending_before: usize,
    commit2_revision: u64,
    receipt_kind: String,
    pending_after: usize,
    executions_len: usize,
    event_kinds: Vec<String>,
    bindings_len: usize,
    kernel_version: Option<String>,
    due_timers: usize,
    schema_version: u32,
    snapshot_json: String,
}

/// Drive a representative durable-state sequence: lease → atomic commit
/// (state+event+intent) → record a receipt → inspect → bind → meta → timer →
/// export.
fn run_scenario(ledger: &mut dyn LedgerApi) -> Transcript {
    let task = task();
    let exec = exec();
    let op1 = op("build");

    let lease = ledger.begin_lease(&task).expect("begin_lease");

    let commit1: CommitReceipt = ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: Some(exec_record()),
            events: vec![event(
                "execution_created",
                serde_json::json!({ "lease_generation": lease, "max_attempts": 2 }),
            )],
            receipts: vec![],
            intents: vec![intent(&op1, serde_json::json!({ "kind": "build" }))],
        })
        .expect("commit state+event+intent");

    let pending_before = ledger.pending_intents().expect("pending before").len();

    let commit2 = ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![event("build_started", serde_json::json!({}))],
            receipts: vec![receipt(&op1, serde_json::json!({ "ok": true }))],
            intents: vec![],
        })
        .expect("commit receipt");
    let recorded = ledger
        .operation_receipt(&op1)
        .expect("operation_receipt")
        .expect("receipt recorded");

    let pending_after = ledger.pending_intents().expect("pending after").len();
    let executions = ledger.executions().expect("executions");
    let event_kinds = ledger
        .events(&exec)
        .expect("events")
        .into_iter()
        .map(|e| e.kind)
        .collect();

    ledger.upsert_binding(&binding()).expect("upsert_binding");
    let bindings_len = ledger.bindings().expect("bindings").len();

    ledger
        .kernel_meta_set("kernel_version", "test/v1")
        .expect("kernel_meta_set");
    let kernel_version = ledger.kernel_meta_get("kernel_version").expect("meta get");

    let inserted = ledger.insert_timer(&timer()).expect("insert_timer");
    assert!(inserted, "the parity timer is new");
    let due_timers = ledger.due_timers(10).expect("due_timers").len();

    let schema_version = ledger.schema_version().expect("schema_version");
    let snapshot = normalize(ledger.export_state().expect("export_state"));
    let snapshot_json = snapshot.to_canonical_json().expect("canonical json");

    Transcript {
        lease,
        commit1_revision: commit1.revision,
        pending_before,
        commit2_revision: commit2.revision,
        receipt_kind: recorded.kind,
        pending_after,
        executions_len: executions.len(),
        event_kinds,
        bindings_len,
        kernel_version,
        due_timers,
        schema_version,
        snapshot_json,
    }
}

// ===========================================================================
// Process server spawning
// ===========================================================================

fn server_spec(socket: &Path, state_dir: &Path) -> (CommandSpec, PathBuf) {
    match resolve_server_binary(SERVER_BIN) {
        Ok(bin) => (
            agalma_conductor::ledger_process::ledger_server_spec(socket, state_dir),
            bin,
        ),
        Err(_) => {
            let exe = std::env::current_exe().expect("current_exe");
            let spec = CommandSpec::new(exe.to_string_lossy().into_owned())
                .args([
                    CHILD_TEST_NAME.to_string(),
                    "--exact".to_string(),
                    "--test-threads=1".to_string(),
                    "--nocapture".to_string(),
                ])
                .env(CHILD_SOCKET_ENV, socket.to_string_lossy().into_owned())
                .env(
                    CHILD_STATE_DIR_ENV,
                    state_dir.to_string_lossy().into_owned(),
                );
            (spec, exe)
        }
    }
}

fn spawn_server(socket: &Path, state_dir: &Path) -> LedgerProcess {
    let (spec, program) = server_spec(socket, state_dir);
    LedgerProcess::spawn_with_spec(
        spec,
        program,
        socket.to_path_buf(),
        state_dir.to_path_buf(),
        READY_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("spawn ledger server: {e}"))
}

/// The child entry point: when spawned with [`CHILD_SOCKET_ENV`], serve the
/// ledger state dir at [`CHILD_STATE_DIR_ENV`] on the named socket. A no-op under
/// a normal test run.
#[test]
fn ledger_server_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let state_dir = std::env::var(CHILD_STATE_DIR_ENV).expect("child state dir");
    let _ = agalma_ledger::server::serve(Path::new(&socket), Path::new(&state_dir));
}

// ===========================================================================
// Case a — shadow parity: in-process vs process binding
// ===========================================================================

#[test]
fn shadow_parity_in_process_vs_process() {
    let dir_in = fresh();
    let mut inproc =
        SqliteLedger::open(dir_in.join("ledger.sqlite")).expect("open in-process ledger");
    let in_process = run_scenario(&mut inproc);

    let dir_proc = fresh();
    let socket = dir_proc.join("a.sock");
    let mut server = spawn_server(&socket, &dir_proc);
    let mut client = server.client().expect("connect ledger client");
    let via_process = run_scenario(&mut client);

    assert_eq!(in_process, via_process, "ledger transcript parity");

    // Spot-check the scenario itself.
    assert_eq!(in_process.lease, 1, "first claim cycle");
    assert_eq!(in_process.pending_before, 1);
    assert_eq!(in_process.pending_after, 0, "receipt consumed the intent");
    assert_eq!(in_process.receipt_kind, "build");
    assert_eq!(in_process.event_kinds, vec!["execution_created", "build_started"]);
    assert_eq!(in_process.due_timers, 1);
    assert_eq!(in_process.schema_version, agalma_ledger::SCHEMA_VERSION);

    drop(client);
    let evidence = server.stop();
    assert!(evidence.process_group_gone, "server stop evidence: {evidence:?}");
}

// ===========================================================================
// Case b — quiesced rebind with an in-flight pinned operation
// ===========================================================================

#[test]
fn quiesced_rebind_preserves_pinned_operation() {
    let run = fresh();
    let a_dir = run.join("a");
    let b_dir = run.join("b");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::create_dir_all(&b_dir).unwrap();
    let snapshot_path = run.join("snapshot.json");

    // Generation 1: server A owns the state. A pinned (unconsumed) dispatch
    // intent is the in-flight durable artifact.
    let socket_a = run.join("a.sock");
    let mut server_a = spawn_server(&socket_a, &a_dir);
    let a_pid = server_a.pid();
    let mut a = server_a.client().expect("client a");
    let lease = a.begin_lease(&task()).expect("begin lease");
    a.commit(CommitBatch {
        expected_revision: None,
        state: Some(exec_record()),
        events: vec![event(
            "execution_created",
            serde_json::json!({ "lease_generation": lease }),
        )],
        receipts: vec![],
        intents: vec![intent(&op("build"), serde_json::json!({ "kind": "build" }))],
    })
    .expect("commit pinned intent");
    assert_eq!(a.pending_intents().expect("pending a").len(), 1);

    // Drain + export: the canonical snapshot is written to a file (D3).
    a.export_snapshot_to(&snapshot_path).expect("export snapshot");
    assert!(snapshot_path.is_file(), "snapshot file written");
    drop(a);
    let stopped = server_a.stop();
    assert!(stopped.process_group_gone, "server A drained: {stopped:?}");
    assert!(!pid_alive(a_pid), "server A is gone");

    // Generation 2: a fresh server B is started from the snapshot and binds the
    // same pinned operation.
    let socket_b = run.join("b.sock");
    let mut server_b = spawn_server(&socket_b, &b_dir);
    let mut b = server_b.client().expect("client b");
    b.import_snapshot_from(&snapshot_path).expect("import snapshot");

    // The pinned operation's ids/pending work survive on the new binding.
    assert_eq!(b.lease_generation(&task()).expect("lease on b"), lease);
    let pending = b.pending_intents().expect("pending b");
    assert_eq!(pending.len(), 1, "pinned intent survives the rebind");
    assert_eq!(pending[0].operation_id, op("build"));

    // It settles on the new binding: recording the receipt consumes the intent.
    b.commit(CommitBatch {
        expected_revision: None,
        state: None,
        events: vec![],
        receipts: vec![receipt(&op("build"), serde_json::json!({ "ok": true }))],
        intents: vec![],
    })
    .expect("settle pinned operation on B");
    assert_eq!(b.pending_intents().expect("pending after").len(), 0);
    assert!(b
        .operation_receipt(&op("build"))
        .expect("receipt on b")
        .is_some());

    drop(b);
    let _ = server_b.stop();
}

// ===========================================================================
// Case c — rollback without state loss
// ===========================================================================

#[test]
fn rollback_keeps_prior_state_and_server() {
    let run = fresh();
    let a_dir = run.join("a");
    let b_dir = run.join("b");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::create_dir_all(&b_dir).unwrap();

    // Prior active binding: server A owns committed durable state.
    let socket_a = run.join("c-a.sock");
    let mut server_a = spawn_server(&socket_a, &a_dir);
    let mut a = server_a.client().expect("client a");
    a.commit(CommitBatch {
        expected_revision: None,
        state: Some(exec_record()),
        events: vec![event("execution_created", serde_json::json!({ "lease_generation": 1 }))],
        receipts: vec![],
        intents: vec![intent(&op("build"), serde_json::json!({ "kind": "build" }))],
    })
    .expect("commit prior state");

    // Candidate binding: server B is staged on its own state dir, then rolled
    // back (stopped).
    let socket_b = run.join("c-b.sock");
    let mut server_b = spawn_server(&socket_b, &b_dir);
    let b_pid = server_b.pid();
    let stopped = server_b.stop();
    assert!(stopped.process_group_gone, "candidate stopped: {stopped:?}");
    assert!(!pid_alive(b_pid), "candidate server is gone after rollback");

    // No state lost: the prior server's DB is intact and it serves the next op.
    assert!(pid_alive(server_a.pid()), "prior server survives the rollback");
    assert_eq!(a.executions().expect("executions").len(), 1);
    assert_eq!(a.pending_intents().expect("pending").len(), 1);
    a.commit(CommitBatch {
        expected_revision: None,
        state: None,
        events: vec![],
        receipts: vec![receipt(&op("build"), serde_json::json!({ "ok": true }))],
        intents: vec![],
    })
    .expect("next op on the restored binding");
    assert_eq!(a.pending_intents().expect("pending after").len(), 0);

    drop(a);
    let _ = server_a.stop();
}

// ===========================================================================
// Case d — IPC conformance for the ledger client
// ===========================================================================

#[test]
fn ledger_client_duplicate_id_replay_and_connection_loss() {
    let dir = fresh();
    let socket = dir.join("d.sock");
    let mut server = spawn_server(&socket, &dir);

    // Duplicate request id: the recorded `commit` receipt is replayed, not
    // re-run (no double effect). One connection at a time (the server is
    // serial per connection), so all live-server calls use `raw`.
    let mut raw = Client::connect(&socket).expect("connect raw");
    let batch = CommitBatch {
        expected_revision: None,
        state: Some(exec_record()),
        events: vec![event("execution_created", serde_json::json!({}))],
        receipts: vec![],
        intents: vec![],
    };
    let params = serde_json::to_value(CommitParams { batch }).expect("commit params");
    let first = raw
        .call_with_id(1, "commit", params.clone())
        .expect("commit #1");
    let replay = raw
        .call_with_id(1, "commit", params.clone())
        .expect("commit #2 (replay)");
    assert_eq!(first, replay, "same id replays the recorded commit receipt");

    // Inspect through the same connection: exactly one event was recorded, so
    // the duplicate commit produced no second effect.
    let events = raw
        .call_with_id(
            3,
            "events",
            serde_json::json!({ "execution_id": "exec:fix-parity:1" }),
        )
        .expect("events");
    assert_eq!(
        events.as_array().map(Vec::len),
        Some(1),
        "duplicate commit must not double the effect"
    );

    // Connection loss: stop the server; the next call surfaces
    // Unavailable/UnknownOutcome (never a silent success).
    let _ = server.stop();
    let err = raw
        .call_with_id(4, "executions", serde_json::json!({}))
        .expect_err("call after server death must fail");
    assert!(
        err.is_unavailable() || err.is_unknown_outcome(),
        "connection loss surfaces as Unavailable/UnknownOutcome: {err:?}"
    );

    // Reconcile through the inspection APIs: restart the server on the same
    // state dir and confirm the recorded state is what was committed.
    let socket2 = dir.join("d2.sock");
    let mut server2 = spawn_server(&socket2, &dir);
    let mut reconciled = LedgerClient::connect(&socket2, &dir).expect("reconnect");
    assert_eq!(reconciled.executions().expect("reconciled executions").len(), 1);
    assert_eq!(reconciled.events(&exec()).expect("reconciled events").len(), 1);

    // An incompatible snapshot import is refused (format + schema gates).
    let good = reconciled.export_state().expect("export");
    let mut bad_format = good.clone();
    bad_format.format = "agalma-state/v999".to_string();
    let err = reconciled
        .import_state(&bad_format)
        .expect_err("bad format refused");
    assert!(matches!(err, ContractError::Conflict(_)), "{err:?}");

    let mut bad_schema = good;
    bad_schema.schema_version = bad_schema.schema_version.wrapping_add(1);
    let err = reconciled
        .import_state(&bad_schema)
        .expect_err("bad schema refused");
    assert!(matches!(err, ContractError::Conflict(_)), "{err:?}");

    // A missing socket is Unavailable.
    let missing = LedgerClient::connect(&dir.join("nope.sock"), &dir);
    assert!(
        matches!(missing, Err(IpcError::Unavailable(_))),
        "missing socket is Unavailable"
    );

    // The snapshot file-transfer primitive writes the snapshot to a path
    // (server-side) without a frame carrying the payload.
    let path = dir.join("primitive-snapshot.json");
    reconciled
        .export_snapshot_to(&path)
        .expect("path-based snapshot export");
    assert!(path.is_file(), "primitive wrote the snapshot file");

    drop(reconciled);
    let _ = server2.stop();
}

/// The transport selector used by the conductor defaults to in-process and maps
/// `process` when selected (mirrors the conductor factory).
#[test]
fn transport_defaults_to_in_process() {
    assert_eq!(LedgerTransport::from_env(), LedgerTransport::InProcess);
    assert_eq!(LedgerTransport::InProcess.as_str(), "in-process");
    assert_eq!(LedgerTransport::Process.as_str(), "process");
}
