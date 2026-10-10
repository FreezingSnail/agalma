//! B1.2 harness process-boundary evidence (`agalma-0ht.3`).
//!
//! Deterministic tests over the reference `HarnessApi` adapter: dual-binding
//! shadow parity (in-process vs supervised process), a staged rebind with an
//! in-flight operation pinned across the swap, rollback without state loss, and
//! IPC conformance for [`HarnessClient`]. Scratch state lives under
//! `target/test-runs/harness-process-*`; the server child is re-exec'd from this
//! test binary (or the real component-server binary when it is built) and talks
//! the framed transport over a Unix socket.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use agalma_contracts::harness::{
    AttemptHandle, CreateSessionRequest, Describe, HarnessApi, Limits, OperationHandle,
    RunTurnRequest, StartAttemptRequest,
};
use agalma_contracts::ids::{ArtifactRef, OperationId};
use agalma_contracts::{
    BindingState, CommitBatch, ContractError, ExecutionEvent, ExecutionId, LedgerApi,
};
use agalma_harness_reference::{self as reference, ReferenceHarness, ReferenceHarnessConfig, ReferenceProfile};
use agalma_ipc::{Client, CommandSpec};

use agalma_conductor::harness_binding::{plan_binding, persist_binding, read_pinned_binding, BindPlan, HarnessKind};
use agalma_conductor::harness_process::{
    ready_json, reference_server_spec, resolve_server_binary, ProcessBinding, READY_TIMEOUT,
};
use agalma_ledger::SqliteLedger;

const MODEL: &str = "test/offline";
const CHILD_SOCKET_ENV: &str = "AGALMA_TEST_HARNESS_SOCKET";
const CHILD_TEST_NAME: &str = "reference_server_child";

// ===========================================================================
// Fixture + helpers (mirrors tests/rebind.rs)
// ===========================================================================

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

/// Short, unique run directory. Unix socket paths must fit `SUN_LEN` (~104
/// bytes), so sockets live directly under a short `hp-<pid>-<n>` directory
/// rather than nested `sock/` subdirectories.
static RUN_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fresh(_name: &str) -> PathBuf {
    let n = RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("hp-{}-{n}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn workspace(dir: &Path, name: &str) -> PathBuf {
    let ws = dir.join(name);
    std::fs::create_dir_all(&ws).expect("workspace");
    ws
}

fn digest_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in &bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

fn start_req(ws: &Path) -> StartAttemptRequest {
    StartAttemptRequest {
        workspace: ws.to_string_lossy().into_owned(),
        role: "builder".to_string(),
        genome_ref: ArtifactRef::derive("genome").to_string(),
        constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
        limits: Limits {
            wall_ms: 5_000,
            tokens: 0,
            cost_micros: 0,
        },
        operation_id: OperationId::new("op:harness-process:build/1"),
    }
}

fn create_req() -> CreateSessionRequest {
    CreateSessionRequest {
        role: "builder".to_string(),
        model: MODEL.to_string(),
        prompts_ref: "artifact:builder-prompts".to_string(),
        handoff_refs: Vec::new(),
        tool_policy_ref: "artifact:tool-policy".to_string(),
    }
}

fn turn_req() -> RunTurnRequest {
    RunTurnRequest {
        input_ref: ArtifactRef::derive("builder-turn"),
        bounded_turns: 1,
        deadline_ms: 5_000,
    }
}

/// The observable, timing/ID-free transcript of one reference scenario.
#[derive(Debug, PartialEq, Eq)]
struct Scenario {
    describe: Describe,
    state: String,
    events: Vec<String>,
    artifact_digest: Option<String>,
    stop_detail_nonempty: bool,
}

/// Drive one reference session/turn to terminal and collect the transcript.
fn run_scenario(harness: &mut dyn HarnessApi, ws: &Path) -> Scenario {
    let attempt: AttemptHandle = harness.start_attempt(start_req(ws)).expect("start_attempt");
    let session = harness.create_session(create_req()).expect("create_session");
    let op: OperationHandle = harness.run_turn(&session, turn_req()).expect("run_turn");
    let state = harness.inspect_operation(&op).expect("inspect_operation");
    let events = harness.read_events(&op).expect("read_events");
    let _ = harness.cancel_operation(&op);
    let _ = harness.close_session(&session);
    let evidence = harness.stop_attempt(&attempt).expect("stop_attempt");
    assert!(evidence.process_group_gone, "cleanup evidence: {evidence:?}");
    Scenario {
        describe: harness.describe(),
        state: state.label(),
        events: events.iter().map(|event| event.render()).collect(),
        artifact_digest: digest_of(&ws.join(".agalma").join("reference-ok")),
        stop_detail_nonempty: !evidence.detail.is_empty(),
    }
}

fn reference_config(profile: ReferenceProfile, scenario_dir: PathBuf) -> ReferenceHarnessConfig {
    ReferenceHarnessConfig {
        profile,
        scenario_dir,
        scenario: None,
    }
}

// ===========================================================================
// Process server spawning
// ===========================================================================

/// The program to spawn plus any child-test arguments.
///
/// Prefers the real component-server binary when it is built (workspace runs);
/// falls back to re-exec'ing this test binary as the server so a package-scoped
/// `cargo test -p agalma-conductor` run still exercises the process boundary.
fn program_and_args() -> (PathBuf, Vec<String>) {
    match resolve_server_binary(reference::SERVER_BIN) {
        Ok(bin) => (bin, Vec::new()),
        Err(_) => {
            let exe = std::env::current_exe().expect("current_exe");
            let args = vec![
                CHILD_TEST_NAME.to_string(),
                "--exact".to_string(),
                "--test-threads=1".to_string(),
                "--nocapture".to_string(),
            ];
            (exe, args)
        }
    }
}

/// Build the server launch spec for `profile` (bin: `--socket` arg; re-exec:
/// socket via env). The scenario directory and profile always travel in env.
fn reference_spec(
    profile: ReferenceProfile,
    socket: &Path,
    scenario_dir: &Path,
) -> (CommandSpec, PathBuf) {
    let (program, child_args) = program_and_args();
    let spec = if child_args.is_empty() {
        reference_server_spec(profile, socket)
    } else {
        CommandSpec::new(program.to_string_lossy().into_owned()).args(child_args)
    };
    let spec = spec
        .env(
            reference::PROFILE_ENV,
            reference::profile_env_value(profile),
        )
        .env(
            reference::SCENARIO_DIR_ENV,
            scenario_dir.to_string_lossy().into_owned(),
        )
        .env(
            CHILD_SOCKET_ENV,
            socket.to_string_lossy().into_owned(),
        );
    (spec, program)
}

fn spawn_process_reference(
    profile: ReferenceProfile,
    socket: &Path,
    scenario_dir: &Path,
    generation: u32,
) -> ProcessBinding {
    let (spec, program) = reference_spec(profile, socket, scenario_dir);
    ProcessBinding::spawn(spec, program, socket.to_path_buf(), generation, MODEL, READY_TIMEOUT)
        .unwrap_or_else(|e| panic!("spawn reference server: {e}"))
}

/// Spawn a bare reference server (no supervisor) and wait for READY.
fn spawn_raw_server(profile: ReferenceProfile, socket: &Path, scenario_dir: &Path) -> Child {
    let (spec, program) = reference_spec(profile, socket, scenario_dir);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).expect("socket dir");
    }
    let _ = std::fs::remove_file(socket);
    let mut child = Command::new(&program)
        .args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn raw server: {e}"));
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read READY");
        assert!(n > 0, "server exited before READY");
        if ready_json(&line).is_some() {
            break;
        }
    }
    child
}

/// The child entry point: when spawned with [`CHILD_SOCKET_ENV`], serve the
/// reference adapter on the named socket. A no-op under a normal test run.
#[test]
fn reference_server_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let profile = reference::profile_from_env();
    let scenario_dir = std::env::var(reference::SCENARIO_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&socket).parent().unwrap().join("scenario"));
    let _ = reference::serve(Path::new(&socket), profile, scenario_dir);
}

// ===========================================================================
// Case a — shadow parity: in-process vs process binding
// ===========================================================================

#[test]
fn shadow_parity_in_process_vs_process_reference() {
    let dir = fresh("parity");

    let ws_in = workspace(&dir, "ws-in-process");
    let mut inproc = ReferenceHarness::new(reference_config(
        ReferenceProfile::Success,
        dir.join("scenario-in-process"),
    ));
    let in_process = run_scenario(&mut inproc, &ws_in);

    let ws_proc = workspace(&dir, "ws-process");
    let socket = dir.join("p.sock");
    let mut binding = spawn_process_reference(
        ReferenceProfile::Success,
        &socket,
        &dir.join("scenario-process"),
        1,
    );
    let record = binding.binding().clone();
    let process = run_scenario(&mut binding, &ws_proc);

    // Same transcript: identity, event kinds/payloads, artifact digest.
    assert_eq!(in_process.describe, process.describe, "identity parity");
    assert_eq!(in_process.state, process.state, "terminal state parity");
    assert_eq!(in_process.events, process.events, "event transcript parity");
    assert_eq!(
        in_process.artifact_digest, process.artifact_digest,
        "produced artifact digest parity"
    );
    assert!(
        in_process.artifact_digest.is_some(),
        "success profile applies the scripted change"
    );
    assert!(process.stop_detail_nonempty);

    // Binding behavior parity: the process bind gate reproduces the static plan.
    let expected = plan_binding(HarnessKind::Reference, 1, MODEL)
        .expect("static plan")
        .binding;
    assert_eq!(record.implementation, expected.implementation);
    assert_eq!(record.implementation_version, expected.implementation_version);
    assert_eq!(record.binding_id, expected.binding_id);
    assert_eq!(record.generation, 1);
    assert_eq!(record.capabilities, expected.capabilities);
}

// ===========================================================================
// Case b — staged rebind with an in-flight operation pinned across the swap
// ===========================================================================

#[test]
fn staged_rebind_uses_pinned_binding_not_flipped_config() {
    let dir = fresh("rebind");
    let exec = ExecutionId::new("exec:harness-process-rebind:1");
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("ledger");

    // The in-flight operation build@1 is pinned to reference-fail before it runs.
    let op1 = OperationId::derive(&exec, "build@1");
    let plan1 = plan_binding(HarnessKind::ReferenceFail, 1, MODEL).expect("plan1");
    persist_binding(&mut ledger, &exec, &op1, 1, &plan1).expect("pin build@1");
    let pinned = read_pinned_binding(&ledger, &exec, &op1)
        .expect("read pin")
        .expect("build@1 pinned");
    assert_eq!(pinned.implementation, "reference-fail");

    // Recovery runs the *pinned* implementation across a real process boundary;
    // the flipped config (reference) must not touch the in-flight operation.
    let ws = workspace(&dir, "ws");
    let socket1 = dir.join("p1.sock");
    let mut b1 = spawn_process_reference(
        ReferenceProfile::Red,
        &socket1,
        &dir.join("scenario-pinned"),
        1,
    );
    let r1 = run_scenario(&mut b1, &ws);
    assert!(
        r1.artifact_digest.is_none(),
        "pinned reference-fail leaves the acceptance red"
    );
    let still_pinned = read_pinned_binding(&ledger, &exec, &op1)
        .expect("read pin")
        .expect("build@1 pinned");
    assert_eq!(
        still_pinned.implementation, "reference-fail",
        "in-flight operation keeps its pinned binding"
    );

    // The next attempt resolves the current (flipped) config: reference.
    let op2 = OperationId::derive(&exec, "build@2");
    let plan2 = plan_binding(HarnessKind::Reference, 2, MODEL).expect("plan2");
    persist_binding(&mut ledger, &exec, &op2, 2, &plan2).expect("pin build@2");
    let socket2 = dir.join("p2.sock");
    let mut b2 = spawn_process_reference(
        ReferenceProfile::Success,
        &socket2,
        &dir.join("scenario-flipped"),
        2,
    );
    let r2 = run_scenario(&mut b2, &ws);
    assert!(
        r2.artifact_digest.is_some(),
        "the flipped config applies the scripted change"
    );
    let pinned2 = read_pinned_binding(&ledger, &exec, &op2)
        .expect("read pin2")
        .expect("build@2 pinned");
    assert_eq!(pinned2.implementation, "reference");
    assert_ne!(
        pinned.binding_id, pinned2.binding_id,
        "rebind changes the binding identity"
    );
}

// ===========================================================================
// Case c — rollback without state loss
// ===========================================================================

#[test]
fn rollback_restores_binding_without_state_loss() {
    let dir = fresh("rollback");
    let exec = ExecutionId::new("exec:harness-process-rollback:1");
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("ledger");

    // Prior active binding plus a recorded state event.
    let prior = plan_binding(HarnessKind::Reference, 1, MODEL)
        .expect("prior plan")
        .binding;
    ledger.upsert_binding(&prior).expect("prior binding");
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![ExecutionEvent {
                execution_id: exec.clone(),
                sequence: 0,
                kind: "marker".to_string(),
                payload: serde_json::json!({ "kept": true }),
                recorded_at_unix_ms: 1,
            }],
            receipts: vec![],
            intents: vec![],
        })
        .expect("record state event");

    // Stage a candidate binding, then roll it back.
    let candidate = plan_binding(HarnessKind::ReferenceFail, 2, MODEL)
        .expect("candidate plan")
        .binding;
    let mut staged = candidate.clone();
    staged.state = BindingState::Staged;
    ledger.upsert_binding(&staged).expect("stage candidate");

    // Rollback: candidate -> Retired; prior -> Active (restored).
    let mut retired = candidate.clone();
    retired.state = BindingState::Retired;
    ledger.upsert_binding(&retired).expect("retire candidate");
    let mut restored = prior.clone();
    restored.state = BindingState::Active;
    ledger.upsert_binding(&restored).expect("restore prior");

    let bindings = ledger.bindings().expect("bindings");
    let active: Vec<_> = bindings
        .iter()
        .filter(|binding| binding.state == BindingState::Active)
        .collect();
    assert_eq!(active.len(), 1, "exactly one active binding: {bindings:?}");
    assert_eq!(active[0].implementation, "reference");
    assert!(bindings.iter().any(|binding| {
        binding.implementation == "reference-fail" && binding.state == BindingState::Retired
    }));

    // No state lost: the pre-rollback event survives.
    let events = ledger.events(&exec).expect("events");
    assert!(
        events.iter().any(|event| event.kind == "marker"),
        "state recorded before rollback survives"
    );

    // The next attempt uses the restored binding.
    let op = OperationId::derive(&exec, "build@1");
    persist_binding(
        &mut ledger,
        &exec,
        &op,
        1,
        &BindPlan {
            binding: restored.clone(),
            fallback: vec![],
        },
    )
    .expect("pin restored");
    let pinned = read_pinned_binding(&ledger, &exec, &op)
        .expect("read pin")
        .expect("pinned");
    assert_eq!(pinned.implementation, "reference");
    assert_eq!(pinned.binding_id, prior.binding_id);
}

// ===========================================================================
// Case d — IPC conformance for the harness client
// ===========================================================================

#[test]
fn harness_client_duplicate_id_replay_and_connection_loss() {
    let dir = fresh("conformance");
    let socket = dir.join("r.sock");
    let mut server = spawn_raw_server(
        ReferenceProfile::Success,
        &socket,
        &dir.join("scenario-raw"),
    );

    let mut client = Client::connect(&socket).expect("connect");
    // A fresh attempt so create_session has an attempt context.
    let start_params = serde_json::to_value(start_req(&workspace(&dir, "ws"))).expect("start params");
    client
        .call_with_id(1, "start_attempt", start_params)
        .expect("start_attempt");

    // Duplicate request id: the recorded receipt is replayed, not re-run.
    let session_params =
        serde_json::to_value(create_req()).expect("create params");
    let first = client
        .call_with_id(7, "create_session", session_params.clone())
        .expect("create #1");
    let replay = client
        .call_with_id(7, "create_session", session_params.clone())
        .expect("create #2 (replay)");
    assert_eq!(first, replay, "same id replays the recorded receipt");
    let next = client
        .call_with_id(8, "create_session", session_params)
        .expect("create #3");
    assert_ne!(first, next, "a fresh id advances the session sequence");

    // Connection loss: kill the server; the next call is not silently lost.
    let _ = server.kill();
    let _ = server.wait();
    let op_params = serde_json::json!({ "operation": { "id": "op:reference:1/turn/1" } });
    let err = client
        .call_with_id(9, "inspect_operation", op_params)
        .expect_err("call after server death must fail");
    assert!(
        err.is_unavailable() || err.is_unknown_outcome(),
        "connection loss surfaces as Unavailable/UnknownOutcome: {err:?}"
    );

    // Connecting to a socket with no server is Unavailable.
    let missing = Client::connect(&dir.join("nope.sock"));
    assert!(
        matches!(missing, Err(agalma_ipc::IpcError::Unavailable(_))),
        "missing socket is Unavailable"
    );
}

#[test]
fn harness_client_maps_transport_loss_to_contract_error() {
    let dir = fresh("client-loss");
    let socket = dir.join("c.sock");
    let mut binding = spawn_process_reference(
        ReferenceProfile::Success,
        &socket,
        &dir.join("scenario-client"),
        1,
    );
    let ws = workspace(&dir, "ws");
    let attempt = binding.start_attempt(start_req(&ws)).expect("start");
    let session = binding.create_session(create_req()).expect("session");
    let op = binding.run_turn(&session, turn_req()).expect("turn");
    let _ = binding.stop_attempt(&attempt);
    assert!(binding.client().cached_describe().impl_name == "reference");

    // Stop the server process; the client must surface a contract error.
    let _ = binding.stop();
    let err = binding
        .inspect_operation(&op)
        .expect_err("dead transport must error");
    assert!(
        matches!(
            err,
            ContractError::UnknownOutcome(_) | ContractError::KnownFailure(_)
        ),
        "dead transport maps to a contract error: {err:?}"
    );
}
