//! B1.7 execution/kernel process-boundary evidence (`agalma-0ht.8`).
//!
//! Boundary: the process-ready candidate replacement for the M1 seed executor is
//! the B0 durable kernel (`KernelEngine<L>`). This test exercises the kernel
//! behind the `ExecutionApi` process boundary: a supervised ledger server (the
//! already-extracted `LedgerApi`) plus a supervised `agalma-kernel-server`
//! hosting `KernelEngine<LedgerClient>` and an `ExecutionClient` driving it.
//!
//! Cases:
//! - `shadow_parity_in_process_vs_process`: the same engine-op scenario
//!   (lease → start → announce/dispatch a build op → complete it → timer →
//!   directive → cancel) driven in-process (`KernelEngine<SqliteLedger>`) vs
//!   through the boundary (`ExecutionClient` → `KernelEngine<LedgerClient>`);
//!   outcomes, receipts, pending work, and the normalized exported snapshot are
//!   compared. The `ExecutionApi` adapter cannot express effect dispatch →
//!   covered at the engine-op level; the adapter's `start`/`status`/`cancel` and
//!   its `UnsupportedCapability` gaps are covered separately.
//! - `rebind_preserves_in_flight_pinned_operation`: a pinned (unconsumed)
//!   dispatch intent survives a snapshot export/import onto a fresh process
//!   binding and settles there.
//! - `rollback_keeps_prior_binding_and_state`: a candidate process binding is
//!   stopped (rolled back); the prior binding's state survives and serves the
//!   next operation.
//! - `execution_client_duplicate_id_replay_and_connection_loss`,
//!   `execution_api_adapter_reports_unsupported_capability`: IPC conformance.
//!
//! Scratch state dirs live under `target/test-runs/ep-*` (never `/tmp`). Each
//! component-server child is re-exec'd from this test binary (or the real
//! `agalma-ledger-server`/`agalma-kernel-server` binary when built), so a
//! package-scoped `cargo test -p agalma-conductor` run still exercises the real
//! process boundary. Scratch dirs auto-remove on success (kept on failure).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agalma_contracts::{
    ArtifactRef, BindingId, ContractError, ExecutionApi, ExecutionId, ExecutionPhase,
    ExecutionState, OperationId, StateSnapshot, TaskId, TimerRecord, TimerState,
};
use agalma_ipc::{
    Client, CommandSpec, EffectContext, EffectDispatch, EffectProbe as IpcProbe, ExecutionClient,
    ExecutionOps, HandlerError, HandlerKey, IpcError,
};
use agalma_kernel::{
    operation_input_hash, run_activity, ActivityContext, ActivityDef, ActivityError,
    ActivityHandler, ActivityOutcome, ActivityRegistry, ApplyOutcome, BindingPin,
    Command as KernelCommand, DirectivePlan, DirectiveStep, EffectProbe as KernelProbe, ExecState,
    Inputs, KernelEngine, OperationRequest, Probe, TimerFire,
};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::json;

use agalma_conductor::execution_process::{
    execution_server_spec, execution_socket, ExecutionProcess, ExecutionTransport,
};
use agalma_conductor::harness_process::resolve_server_binary;
use agalma_conductor::ledger_process::{ledger_server_spec, LedgerProcess};

const READY_TIMEOUT: Duration = Duration::from_secs(20);

const CHILD_LEDGER_SOCKET_ENV: &str = "AGALMA_TEST_EP_LEDGER_SOCKET";
const CHILD_LEDGER_STATE_DIR_ENV: &str = "AGALMA_TEST_EP_LEDGER_STATE_DIR";
const CHILD_LEDGER_TEST_NAME: &str = "ledger_server_child";

const CHILD_KERNEL_SOCKET_ENV: &str = "AGALMA_TEST_EP_KERNEL_SOCKET";
const CHILD_KERNEL_STATE_DIR_ENV: &str = "AGALMA_TEST_EP_KERNEL_STATE_DIR";
const CHILD_KERNEL_LEDGER_SOCKET_ENV: &str = "AGALMA_TEST_EP_KERNEL_LEDGER_SOCKET";
const CHILD_KERNEL_TEST_NAME: &str = "kernel_server_child";

// ===========================================================================
// Fixtures + helpers
// ===========================================================================

/// Short, unique run directory (sockets must fit `SUN_LEN`).
fn fresh() -> ScratchDir {
    ScratchDir::new("ep")
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
    ExecutionId::derive(&task(), 1)
}

fn build_op() -> OperationId {
    OperationId::derive(&exec(), "build")
}

fn pin() -> BindingPin {
    BindingPin {
        binding_id: BindingId::new("binding:seed"),
        component_api: "harness".to_string(),
        api_version: 1,
        implementation: "seed".to_string(),
        implementation_version: "v1".to_string(),
    }
}

// ===========================================================================
// Process server spawning
// ===========================================================================

fn ledger_spec(socket: &Path, state_dir: &Path) -> (CommandSpec, PathBuf) {
    match resolve_server_binary(agalma_ledger::server::SERVER_BIN) {
        Ok(bin) => (ledger_server_spec(socket, state_dir), bin),
        Err(_) => {
            let exe = std::env::current_exe().expect("current_exe");
            let spec = CommandSpec::new(exe.to_string_lossy().into_owned())
                .args([
                    CHILD_LEDGER_TEST_NAME.to_string(),
                    "--exact".to_string(),
                    "--test-threads=1".to_string(),
                    "--nocapture".to_string(),
                ])
                .env(
                    CHILD_LEDGER_SOCKET_ENV,
                    socket.to_string_lossy().into_owned(),
                )
                .env(
                    CHILD_LEDGER_STATE_DIR_ENV,
                    state_dir.to_string_lossy().into_owned(),
                );
            (spec, exe)
        }
    }
}

fn spawn_ledger(state_dir: &Path, socket: &Path) -> LedgerProcess {
    let (spec, program) = ledger_spec(socket, state_dir);
    LedgerProcess::spawn_with_spec(
        spec,
        program,
        socket.to_path_buf(),
        state_dir.to_path_buf(),
        READY_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("spawn ledger server: {e}"))
}

fn kernel_spec(socket: &Path, state_dir: &Path, ledger_socket: &Path) -> (CommandSpec, PathBuf) {
    match resolve_server_binary(agalma_kernel::server::SERVER_BIN) {
        Ok(bin) => (
            execution_server_spec(socket, state_dir, ledger_socket, None),
            bin,
        ),
        Err(_) => {
            let exe = std::env::current_exe().expect("current_exe");
            let spec = CommandSpec::new(exe.to_string_lossy().into_owned())
                .args([
                    CHILD_KERNEL_TEST_NAME.to_string(),
                    "--exact".to_string(),
                    "--test-threads=1".to_string(),
                    "--nocapture".to_string(),
                ])
                .env(
                    CHILD_KERNEL_SOCKET_ENV,
                    socket.to_string_lossy().into_owned(),
                )
                .env(
                    CHILD_KERNEL_STATE_DIR_ENV,
                    state_dir.to_string_lossy().into_owned(),
                )
                .env(
                    CHILD_KERNEL_LEDGER_SOCKET_ENV,
                    ledger_socket.to_string_lossy().into_owned(),
                );
            (spec, exe)
        }
    }
}

fn spawn_kernel(state_dir: &Path, ledger_socket: &Path) -> ExecutionProcess {
    let socket = execution_socket(state_dir);
    let (spec, program) = kernel_spec(&socket, state_dir, ledger_socket);
    ExecutionProcess::spawn_with_spec(
        spec,
        program,
        socket,
        state_dir.to_path_buf(),
        READY_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("spawn kernel server: {e}"))
}

/// Child entry point: serve the ledger state dir named by the child env.
#[test]
fn ledger_server_child() {
    let Ok(socket) = std::env::var(CHILD_LEDGER_SOCKET_ENV) else {
        return;
    };
    let state_dir = std::env::var(CHILD_LEDGER_STATE_DIR_ENV).expect("child state dir");
    let _ = agalma_ledger::server::serve(Path::new(&socket), Path::new(&state_dir));
}

/// Child entry point: serve the kernel over the ledger socket named by the child
/// env.
#[test]
fn kernel_server_child() {
    let Ok(socket) = std::env::var(CHILD_KERNEL_SOCKET_ENV) else {
        return;
    };
    let state_dir = std::env::var(CHILD_KERNEL_STATE_DIR_ENV).expect("child state dir");
    let ledger_socket = std::env::var(CHILD_KERNEL_LEDGER_SOCKET_ENV).expect("child ledger socket");
    let _ = agalma_kernel::server::serve(
        Path::new(&socket),
        Path::new(&state_dir),
        Path::new(&ledger_socket),
        None,
    );
}

// ===========================================================================
// Scenario driving (shared by both bindings)
// ===========================================================================

fn apply(
    ops: &mut dyn ExecutionOps,
    execution: &ExecutionId,
    command: &KernelCommand,
    inputs: &Inputs,
) -> ApplyOutcome {
    let command = serde_json::to_value(command).expect("encode command");
    let inputs = serde_json::to_value(inputs).expect("encode inputs");
    let outcome = ops
        .apply(execution, command, inputs)
        .expect("apply command");
    serde_json::from_value(outcome).expect("decode apply outcome")
}

/// Zero every wall-clock field so the snapshot is comparable across bindings.
fn normalize(mut snapshot: StateSnapshot) -> StateSnapshot {
    for e in &mut snapshot.events {
        e.recorded_at_unix_ms = 0;
        // The recorded `Inputs::now_unix_ms` is part of the event payload; the
        // adapter's wall-clock cancel stamp is the only nondeterministic one.
        if let Some(inputs) = e.payload.get_mut("inputs") {
            if let Some(now) = inputs.get_mut("now_unix_ms") {
                *now = json!(0);
            }
        }
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

/// A normalized, timing-free transcript of one kernel engine-op scenario.
#[derive(Debug, PartialEq, Eq)]
struct Transcript {
    lease: u32,
    start_outcome: String,
    pending_after_announce: usize,
    pending_op_ids: Vec<String>,
    announce_dispatches: usize,
    complete_outcome: String,
    receipt_kind: String,
    pending_after_complete: usize,
    due_timers: usize,
    timer_fired: Vec<(String, bool)>,
    directive_pending: usize,
    pending_after_cancel: usize,
    status_after_cancel: String,
    snapshot_json: String,
}

/// Drive the same engine-op scenario against any [`ExecutionOps`] binding.
fn run_scenario(ops: &mut dyn ExecutionOps, snapshot_path: &Path) -> Transcript {
    let task = task();
    let exec = exec();
    let op = build_op();
    let hash = "hash:build-v1";

    let lease = ops.begin_lease(&task).expect("begin_lease");

    let start = apply(
        ops,
        &exec,
        &KernelCommand::Start {
            task_id: task.clone(),
            generation: 1,
        },
        &Inputs::default(),
    );

    let announce_inputs = Inputs {
        probe: Probe::Absent,
        now_unix_ms: 2,
        lease_generation: lease,
        binding: Some(pin()),
        activity: Some(ActivityDef::new("fake/build", "v1")),
        operation_inputs: Some(json!({ "task": "fix-parity" })),
        phase: Some(ExecutionPhase::Build),
        ..Inputs::default()
    };
    let announce = apply(
        ops,
        &exec,
        &KernelCommand::Deliver {
            operation_id: op.clone(),
            inputs_hash: hash.to_string(),
        },
        &announce_inputs,
    );
    let pending = ops.pending_dispatch().expect("pending after announce");
    let pending_op_ids: Vec<String> = pending
        .iter()
        .map(|i| i.operation_id.as_str().to_string())
        .collect();

    let complete_inputs = Inputs {
        probe: Probe::Present,
        outcome_ref: Some(ArtifactRef::new("artifact:build-output")),
        now_unix_ms: 3,
        lease_generation: lease,
        ..Inputs::default()
    };
    let complete = apply(
        ops,
        &exec,
        &KernelCommand::Deliver {
            operation_id: op.clone(),
            inputs_hash: hash.to_string(),
        },
        &complete_inputs,
    );
    let receipt = ops
        .receipt(&op)
        .expect("receipt")
        .expect("receipt recorded");
    let pending_after_complete = ops
        .pending_dispatch()
        .expect("pending after complete")
        .len();

    let timer = TimerRecord {
        timer_id: "timer:parity".to_string(),
        execution_id: exec.clone(),
        kind: "deadline".to_string(),
        fire_at_unix_ms: 5,
        state: TimerState::Pending,
        payload: json!({ "cause": "parity" }),
    };
    let inserted = ops.schedule_timer(&timer).expect("schedule_timer");
    assert!(inserted, "the parity timer is new");
    let due_timers = ops.due_timers(10).expect("due_timers").len();
    let fired_value = ops.fire_due_timers(10).expect("fire_due_timers");
    let fired: Vec<TimerFire> = serde_json::from_value(fired_value).expect("decode timers");
    let timer_fired: Vec<(String, bool)> =
        fired.into_iter().map(|f| (f.timer_id, f.applied)).collect();

    let plan = DirectivePlan {
        source: "parity".to_string(),
        steps: vec![DirectiveStep {
            step: "integrate".to_string(),
            activity: ActivityDef::new("fake/integrate", "v1"),
            phase: ExecutionPhase::Integrate,
            inputs: json!({}),
        }],
    };
    let directive_plan = serde_json::to_value(&plan).expect("encode plan");
    let _ = ops
        .submit_directive(&exec, "directive:parity", 1, directive_plan, 6)
        .expect("submit_directive");
    let directive_pending = ops
        .pending_dispatch()
        .expect("pending after directive")
        .len();

    ExecutionApi::cancel(ops, &exec).expect("cancel");
    let pending_after_cancel = ops.pending_dispatch().expect("pending after cancel").len();
    let status = ExecutionApi::status(ops, &exec).expect("status");
    let status_after_cancel = format!("{:?}", status.state);

    ops.export_snapshot_to(snapshot_path)
        .expect("export snapshot");
    let text = std::fs::read_to_string(snapshot_path).expect("read snapshot");
    let snapshot = StateSnapshot::from_json(&text).expect("parse snapshot");
    let snapshot_json = normalize(snapshot)
        .to_canonical_json()
        .expect("canonical snapshot");

    Transcript {
        lease,
        start_outcome: serde_json::to_string(&start).unwrap(),
        pending_after_announce: pending.len(),
        pending_op_ids,
        announce_dispatches: announce.dispatches.len(),
        complete_outcome: serde_json::to_string(&complete).unwrap(),
        receipt_kind: receipt.kind,
        pending_after_complete,
        due_timers,
        timer_fired,
        directive_pending,
        pending_after_cancel,
        status_after_cancel,
        snapshot_json,
    }
}

// ===========================================================================
// Case a — shadow parity: in-process vs process binding
// ===========================================================================

#[test]
fn shadow_parity_in_process_vs_process() {
    // In-process binding: KernelEngine<SqliteLedger>.
    let dir_in = fresh();
    let ledger = SqliteLedger::open(dir_in.join("ledger.sqlite")).expect("open in-process ledger");
    let mut inproc = KernelEngine::open(ledger).expect("open in-process kernel");
    let in_snapshot = dir_in.join("in-snapshot.json");
    let in_process = run_scenario(&mut inproc, &in_snapshot);

    // Process binding: ExecutionClient → KernelEngine<LedgerClient> → ledger server.
    let dir_proc = fresh();
    let ledger_socket = dir_proc.join("ledger.sock");
    let mut ledger_server = spawn_ledger(&dir_proc, &ledger_socket);
    let mut kernel_server = spawn_kernel(&dir_proc, &ledger_socket);
    let mut client = kernel_server.client().expect("connect execution client");
    let proc_snapshot = dir_proc.join("proc-snapshot.json");
    let via_process = run_scenario(&mut client, &proc_snapshot);

    assert_eq!(in_process, via_process, "kernel transcript parity");

    // Spot-check the scenario itself.
    assert_eq!(in_process.lease, 1, "first claim cycle");
    assert_eq!(in_process.pending_after_announce, 1);
    assert_eq!(
        in_process.pending_op_ids,
        vec![build_op().as_str().to_string()]
    );
    assert_eq!(in_process.announce_dispatches, 1, "one dispatch hand-off");
    assert_eq!(in_process.receipt_kind, "operation");
    assert_eq!(
        in_process.pending_after_complete, 0,
        "completion consumed the intent"
    );
    assert_eq!(in_process.due_timers, 1);
    assert_eq!(
        in_process.timer_fired,
        vec![("timer:parity".to_string(), true)]
    );
    assert_eq!(in_process.directive_pending, 1);
    assert_eq!(
        in_process.pending_after_cancel, 0,
        "cancel released the intent"
    );
    assert_eq!(
        in_process.status_after_cancel, "Failed",
        "Cancelled coarsens to Failed"
    );

    drop(client);
    let _ = kernel_server.stop();
    let _ = ledger_server.stop();
}

/// The transport selector used by a caller defaults to in-process and maps
/// `process` when selected.
#[test]
fn transport_defaults_to_in_process() {
    assert_eq!(
        ExecutionTransport::from_env(),
        ExecutionTransport::InProcess
    );
    assert_eq!(ExecutionTransport::InProcess.as_str(), "in-process");
    assert_eq!(ExecutionTransport::Process.as_str(), "process");
}

// ===========================================================================
// Case b — rebind with an in-flight pinned operation
// ===========================================================================

#[test]
fn rebind_preserves_in_flight_pinned_operation() {
    let run = fresh();
    let a_dir = run.join("a");
    let b_dir = run.join("b");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::create_dir_all(&b_dir).unwrap();
    let snapshot_path = run.join("snapshot.json");

    // Generation 1: binding A holds a pinned (unconsumed) dispatch intent.
    let a_ledger = a_dir.join("ledger.sock");
    let mut ledger_a = spawn_ledger(&a_dir, &a_ledger);
    let mut kernel_a = spawn_kernel(&a_dir, &a_ledger);
    let a_pid = kernel_a.pid();
    let mut a = kernel_a.client().expect("client a");
    let lease = a.begin_lease(&task()).expect("begin lease");
    apply(
        &mut a,
        &exec(),
        &KernelCommand::Start {
            task_id: task(),
            generation: 1,
        },
        &Inputs::default(),
    );
    let announce = apply(
        &mut a,
        &exec(),
        &KernelCommand::Deliver {
            operation_id: build_op(),
            inputs_hash: "hash:build-v1".to_string(),
        },
        &Inputs {
            probe: Probe::Absent,
            now_unix_ms: 2,
            lease_generation: lease,
            binding: Some(pin()),
            activity: Some(ActivityDef::new("fake/build", "v1")),
            operation_inputs: Some(json!({ "task": "fix-parity" })),
            phase: Some(ExecutionPhase::Build),
            ..Inputs::default()
        },
    );
    assert_eq!(announce.dispatches.len(), 1);
    assert_eq!(a.pending_dispatch().expect("pending a").len(), 1);

    // Drain + export the canonical snapshot to a file (D3).
    a.export_snapshot_to(&snapshot_path)
        .expect("export snapshot");
    assert!(snapshot_path.is_file(), "snapshot file written");
    drop(a);
    let stopped = kernel_a.stop();
    assert!(stopped.process_group_gone, "kernel A drained: {stopped:?}");
    assert!(!pid_alive(a_pid), "kernel A is gone");
    let _ = ledger_a.stop();

    // Generation 2: a fresh process binding imports the snapshot.
    let b_ledger = b_dir.join("ledger.sock");
    let mut ledger_b = spawn_ledger(&b_dir, &b_ledger);
    let mut kernel_b = spawn_kernel(&b_dir, &b_ledger);
    let mut b = kernel_b.client().expect("client b");
    b.import_snapshot_from(&snapshot_path)
        .expect("import snapshot");

    // The pinned operation and its binding pin survive the rebind.
    assert_eq!(b.lease_generation(&task()).expect("lease on b"), lease);
    let pending = b.pending_dispatch().expect("pending b");
    assert_eq!(pending.len(), 1, "pinned intent survives the rebind");
    assert_eq!(pending[0].operation_id, build_op());
    let pinned = pending[0]
        .payload
        .get("binding")
        .cloned()
        .expect("intent carries the binding pin");
    let pinned: BindingPin = serde_json::from_value(pinned).expect("decode pin");
    assert_eq!(pinned, pin(), "the pin is unchanged across the rebind");

    // It settles on the new binding: the completion consumes the intent.
    let complete = apply(
        &mut b,
        &exec(),
        &KernelCommand::Deliver {
            operation_id: build_op(),
            inputs_hash: "hash:build-v1".to_string(),
        },
        &Inputs {
            probe: Probe::Present,
            outcome_ref: Some(ArtifactRef::new("artifact:build-output")),
            now_unix_ms: 3,
            lease_generation: lease,
            ..Inputs::default()
        },
    );
    assert!(complete.conflict.is_none());
    assert_eq!(b.pending_dispatch().expect("pending after").len(), 0);
    assert!(b.receipt(&build_op()).expect("receipt on b").is_some());

    drop(b);
    let _ = kernel_b.stop();
    let _ = ledger_b.stop();
}

// ===========================================================================
// Case c — rollback without state loss
// ===========================================================================

#[test]
fn rollback_keeps_prior_binding_and_state() {
    let run = fresh();
    let a_dir = run.join("a");
    let b_dir = run.join("b");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::create_dir_all(&b_dir).unwrap();

    // Prior active binding A with committed durable state.
    let a_ledger = a_dir.join("ledger.sock");
    let mut ledger_a = spawn_ledger(&a_dir, &a_ledger);
    let mut kernel_a = spawn_kernel(&a_dir, &a_ledger);
    let mut a = kernel_a.client().expect("client a");
    let lease = a.begin_lease(&task()).expect("begin lease");
    apply(
        &mut a,
        &exec(),
        &KernelCommand::Start {
            task_id: task(),
            generation: 1,
        },
        &Inputs::default(),
    );
    apply(
        &mut a,
        &exec(),
        &KernelCommand::Deliver {
            operation_id: build_op(),
            inputs_hash: "hash:build-v1".to_string(),
        },
        &Inputs {
            probe: Probe::Absent,
            now_unix_ms: 2,
            lease_generation: lease,
            activity: Some(ActivityDef::new("fake/build", "v1")),
            operation_inputs: Some(json!({ "task": "fix-parity" })),
            phase: Some(ExecutionPhase::Build),
            ..Inputs::default()
        },
    );

    // Candidate binding B is staged on its own state dir, then rolled back.
    let b_ledger = b_dir.join("ledger.sock");
    let mut ledger_b = spawn_ledger(&b_dir, &b_ledger);
    let mut kernel_b = spawn_kernel(&b_dir, &b_ledger);
    let b_pid = kernel_b.pid();
    let stopped = kernel_b.stop();
    assert!(stopped.process_group_gone, "candidate stopped: {stopped:?}");
    assert!(!pid_alive(b_pid), "candidate kernel is gone after rollback");
    let _ = ledger_b.stop();

    // No state lost: the prior binding serves the next operation.
    assert!(
        pid_alive(kernel_a.pid()),
        "prior kernel survives the rollback"
    );
    let state = a.load_state(&exec()).expect("load state");
    let state: agalma_kernel::KernelState = serde_json::from_value(state).expect("decode state");
    assert_eq!(state.state, ExecState::Running);
    assert_eq!(a.pending_dispatch().expect("pending").len(), 1);

    apply(
        &mut a,
        &exec(),
        &KernelCommand::Deliver {
            operation_id: build_op(),
            inputs_hash: "hash:build-v1".to_string(),
        },
        &Inputs {
            probe: Probe::Present,
            outcome_ref: Some(ArtifactRef::new("artifact:build-output")),
            now_unix_ms: 3,
            lease_generation: lease,
            ..Inputs::default()
        },
    );
    assert_eq!(a.pending_dispatch().expect("pending after").len(), 0);
    assert!(a.receipt(&build_op()).expect("receipt").is_some());

    drop(a);
    let _ = kernel_a.stop();
    let _ = ledger_a.stop();
}

// ===========================================================================
// Case d — IPC conformance for the execution client
// ===========================================================================

#[test]
fn execution_client_duplicate_id_replay_and_connection_loss() {
    let dir = fresh();
    let ledger_socket = dir.join("ledger.sock");
    let mut ledger_server = spawn_ledger(&dir, &ledger_socket);
    let mut kernel_server = spawn_kernel(&dir, &ledger_socket);
    let kernel_socket = execution_socket(&dir);

    // Duplicate request id: the recorded `begin_lease` result is replayed, not
    // re-run (lease generation stays 1). One live connection at a time, so all
    // live-server calls use the raw client.
    let mut raw = Client::connect(&kernel_socket).expect("connect raw");
    let first = raw
        .call_with_id(1, "begin_lease", json!({ "task": "fix-parity" }))
        .expect("begin_lease #1");
    let replay = raw
        .call_with_id(1, "begin_lease", json!({ "task": "fix-parity" }))
        .expect("begin_lease #2 (replay)");
    assert_eq!(first, replay, "same id replays the recorded result");
    let generation = raw
        .call_with_id(2, "lease_generation", json!({ "task": "fix-parity" }))
        .expect("lease_generation");
    assert_eq!(
        generation,
        json!(1),
        "duplicate begin_lease did not double the effect"
    );

    // Connection loss: stop the server; the next call surfaces
    // Unavailable/UnknownOutcome (never a silent success).
    let _ = kernel_server.stop();
    let err = raw
        .call_with_id(3, "pending_dispatch", json!({}))
        .expect_err("call after server death must fail");
    assert!(
        err.is_unavailable() || err.is_unknown_outcome(),
        "connection loss surfaces as Unavailable/UnknownOutcome: {err:?}"
    );

    drop(raw);
    let _ = ledger_server.stop();
}

#[test]
fn execution_api_adapter_reports_unsupported_capability() {
    let dir = fresh();
    let ledger_socket = dir.join("ledger.sock");
    let mut ledger_server = spawn_ledger(&dir, &ledger_socket);
    let mut kernel_server = spawn_kernel(&dir, &ledger_socket);
    let mut client = kernel_server.client().expect("connect execution client");

    // start/status map faithfully.
    let execution = client.start(&task()).expect("start");
    assert_eq!(execution, exec());
    let status = client.status(&execution).expect("status");
    assert_eq!(status.phase, ExecutionPhase::Intake);
    // Kernel `Start` sets `ExecState::Ready`, which coarsens to `Pending`.
    assert_eq!(status.state, ExecutionState::Pending);

    // step/recover cannot be expressed behind the boundary.
    let step_err = client.step(&execution).expect_err("step unsupported");
    assert!(
        matches!(step_err, ContractError::UnsupportedCapability(_)),
        "step gap: {step_err:?}"
    );
    let recover_err = client.recover().expect_err("recover unsupported");
    assert!(
        matches!(recover_err, ContractError::UnsupportedCapability(_)),
        "recover gap: {recover_err:?}"
    );

    // A missing socket is Unavailable.
    let missing = ExecutionClient::connect(&dir.join("nope.sock"));
    assert!(matches!(missing, Err(IpcError::Unavailable(_))));

    drop(client);
    let _ = kernel_server.stop();
    let _ = ledger_server.stop();
}

// ===========================================================================
// Case e — handler-registry protocol (B1-F1): faithful step/recover across the
// boundary via caller-side effect dispatch
// ===========================================================================

/// In-process handler that reports a fixed present effect.
struct PresentHandler {
    def: ActivityDef,
    outcome_ref: ArtifactRef,
}

impl ActivityHandler for PresentHandler {
    fn def(&self) -> ActivityDef {
        self.def.clone()
    }

    fn execute(&mut self, _ctx: &ActivityContext) -> Result<ActivityOutcome, ActivityError> {
        Ok(ActivityOutcome::new(self.outcome_ref.clone()))
    }

    fn reconcile(&mut self, _ctx: &ActivityContext) -> Result<KernelProbe, ActivityError> {
        Ok(KernelProbe::Present(ActivityOutcome::new(
            self.outcome_ref.clone(),
        )))
    }
}

/// Caller-side dispatch double: the half of the handler-registry protocol that
/// runs in the caller's process.
struct PresentDispatch {
    key: HandlerKey,
    outcome_ref: ArtifactRef,
}

impl EffectDispatch for PresentDispatch {
    fn contains(&self, key: &HandlerKey) -> bool {
        key == &self.key
    }

    fn probe(&mut self, _ctx: &EffectContext) -> Result<IpcProbe, HandlerError> {
        Ok(IpcProbe::Present(self.outcome_ref.clone()))
    }

    fn execute(&mut self, _ctx: &EffectContext) -> Result<ArtifactRef, HandlerError> {
        Ok(self.outcome_ref.clone())
    }
}

/// The announce inputs for a pending registry operation (`Probe::Absent`).
fn registry_announce_inputs(def: &ActivityDef, inputs: &serde_json::Value) -> Inputs {
    Inputs {
        probe: Probe::Absent,
        activity: Some(def.clone()),
        operation_inputs: Some(inputs.clone()),
        phase: Some(ExecutionPhase::Build),
        now_unix_ms: 2,
        ..Inputs::default()
    }
}

fn export_normalized(ops: &dyn ExecutionOps, path: &Path) -> String {
    ops.export_snapshot_to(path).expect("export snapshot");
    let text = std::fs::read_to_string(path).expect("read snapshot");
    normalize(StateSnapshot::from_json(&text).expect("parse snapshot"))
        .to_canonical_json()
        .expect("canonical snapshot")
}

/// The handler-registry protocol must reproduce the in-process `run_activity`
/// reconcile outcome and durable state through the process boundary.
#[test]
fn handler_registry_step_parity_in_process_vs_process() {
    let def = ActivityDef::new("fake/build", "v1");
    let inputs = json!({ "task": "fix-parity" });
    let op = OperationId::derive(&exec(), "build");
    let hash = operation_input_hash(&def, &inputs);
    let outcome_ref = ArtifactRef::derive("build-output");

    // In-process: KernelEngine + ActivityRegistry + run_activity.
    let dir_in = fresh();
    let ledger = SqliteLedger::open(dir_in.join("ledger.sqlite")).expect("open ledger");
    let mut engine = KernelEngine::open(ledger).expect("open kernel");
    ExecutionApi::start(&mut engine, &task()).expect("in-process start");
    engine
        .apply(
            &exec(),
            &KernelCommand::Deliver {
                operation_id: op.clone(),
                inputs_hash: hash.clone(),
            },
            &registry_announce_inputs(&def, &inputs),
        )
        .expect("announce in-process");
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(PresentHandler {
            def: def.clone(),
            outcome_ref: outcome_ref.clone(),
        }))
        .expect("register handler");
    let request = OperationRequest {
        activity: def.clone(),
        step: "build".to_string(),
        phase: ExecutionPhase::Build,
        component_api: "harness".to_string(),
        inputs: inputs.clone(),
        now_unix_ms: 3,
    };
    let result = run_activity(&mut engine, &mut registry, &exec(), &request).expect("run_activity");
    assert!(
        matches!(
            result,
            agalma_kernel::OperationResult::Completed {
                reconciled: true,
                duplicate: false,
                ..
            }
        ),
        "in-process reconcile: {result:?}"
    );
    let in_receipt = engine.receipt(&op).expect("receipt").expect("recorded");
    let in_pending = engine.pending_dispatch().expect("pending").len();
    let in_snapshot = export_normalized(&engine, &dir_in.join("in.json"));

    // Process: ExecutionClient + caller-side dispatch.
    let dir_proc = fresh();
    let ledger_socket = dir_proc.join("ledger.sock");
    let mut ledger_server = spawn_ledger(&dir_proc, &ledger_socket);
    let mut kernel_server = spawn_kernel(&dir_proc, &ledger_socket);
    let client = kernel_server.client().expect("connect");
    let mut client = client.with_effect_dispatch(Box::new(PresentDispatch {
        key: HandlerKey::new("fake/build", "v1"),
        outcome_ref: outcome_ref.clone(),
    }));
    ExecutionApi::start(&mut client, &task()).expect("process start");
    apply(
        &mut client,
        &exec(),
        &KernelCommand::Deliver {
            operation_id: op.clone(),
            inputs_hash: hash,
        },
        &registry_announce_inputs(&def, &inputs),
    );
    let step = ExecutionApi::step(&mut client, &exec()).expect("step");
    assert_eq!(
        step,
        agalma_contracts::StepOutcome::Dispatched {
            operation: op.clone(),
            reconciled: true,
            duplicate: false,
        },
        "process reconcile outcome"
    );
    let proc_receipt = client.receipt(&op).expect("receipt").expect("recorded");
    let proc_pending = client.pending_dispatch().expect("pending").len();
    let proc_snapshot = export_normalized(&client, &dir_proc.join("proc.json"));

    // Parity: same receipt, no pending work, identical normalized state.
    assert_eq!(in_receipt.inputs_hash, proc_receipt.inputs_hash);
    assert_eq!(in_receipt.kind, proc_receipt.kind);
    assert_eq!(in_receipt.result, proc_receipt.result);
    assert_eq!(in_pending, 0);
    assert_eq!(proc_pending, 0);
    assert_eq!(
        in_snapshot, proc_snapshot,
        "handler-protocol snapshot parity"
    );

    drop(client);
    let _ = kernel_server.stop();
    let _ = ledger_server.stop();
}

/// A pinned pending operation survives a rebind and is reconciled through the
/// boundary by the caller-side registry (`recover`).
#[test]
fn handler_registry_recover_after_rebind() {
    let run = fresh();
    let a_dir = run.join("a");
    let b_dir = run.join("b");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::create_dir_all(&b_dir).unwrap();
    let snapshot_path = run.join("snapshot.json");
    let def = ActivityDef::new("fake/build", "v1");
    let inputs = json!({ "task": "fix-parity" });
    let op = OperationId::derive(&exec(), "build");
    let hash = operation_input_hash(&def, &inputs);

    // Generation 1: binding A announces a pinned (unconsumed) intent.
    let a_ledger = a_dir.join("ledger.sock");
    let mut ledger_a = spawn_ledger(&a_dir, &a_ledger);
    let mut kernel_a = spawn_kernel(&a_dir, &a_ledger);
    let a_pid = kernel_a.pid();
    let mut a = kernel_a.client().expect("client a");
    ExecutionApi::start(&mut a, &task()).expect("start a");
    apply(
        &mut a,
        &exec(),
        &KernelCommand::Deliver {
            operation_id: op.clone(),
            inputs_hash: hash.clone(),
        },
        &registry_announce_inputs(&def, &inputs),
    );
    assert_eq!(a.pending_dispatch().expect("pending a").len(), 1);
    a.export_snapshot_to(&snapshot_path).expect("export");
    drop(a);
    assert!(kernel_a.stop().process_group_gone, "binding A drained");
    assert!(!pid_alive(a_pid), "binding A is gone");
    let _ = ledger_a.stop();

    // Generation 2: binding B imports the snapshot and recovers through the
    // caller-side registry — the pinned operation settles there.
    let b_ledger = b_dir.join("ledger.sock");
    let mut ledger_b = spawn_ledger(&b_dir, &b_ledger);
    let mut kernel_b = spawn_kernel(&b_dir, &b_ledger);
    let client = kernel_b.client().expect("client b");
    let mut b = client.with_effect_dispatch(Box::new(PresentDispatch {
        key: HandlerKey::new("fake/build", "v1"),
        outcome_ref: ArtifactRef::derive("build-output"),
    }));
    b.import_snapshot_from(&snapshot_path).expect("import");
    assert_eq!(b.pending_dispatch().expect("pending b").len(), 1);
    b.recover().expect("recover through boundary");
    assert_eq!(
        b.pending_dispatch().expect("pending after recover").len(),
        0,
        "recovered operation settled"
    );
    let receipt = b.receipt(&op).expect("receipt").expect("recorded");
    assert_eq!(receipt.kind, "operation");
    assert_eq!(receipt.inputs_hash, hash);

    drop(b);
    let _ = kernel_b.stop();
    let _ = ledger_b.stop();
}
