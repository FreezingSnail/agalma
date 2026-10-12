//! B1.2 harness process-boundary evidence (`agalma-0ht.3`).
//!
//! Deterministic tests over the reference `HarnessApi` adapter: dual-binding
//! shadow parity (in-process vs supervised process), a staged rebind with an
//! in-flight operation pinned across the swap, rollback without state loss, and
//! IPC conformance for [`HarnessClient`]. Scratch state lives under
//! `target/test-runs/harness-process-*`; the server child is re-exec'd from this
//! test binary (or the real component-server binary when it is built) and talks
//! the framed transport over a Unix socket. Scratch dirs auto-remove on success
//! (kept on failure for forensics).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agalma_contracts::harness::{
    AttemptHandle, CreateSessionRequest, Describe, HarnessApi, Limits, OperationHandle,
    RunTurnRequest, StartAttemptRequest,
};
use agalma_contracts::ids::{ArtifactRef, OperationId};
use agalma_contracts::{
    BindingState, CommitBatch, ContractError, ExecutionEvent, ExecutionId, LedgerApi,
};
use agalma_harness_reference::{
    self as reference, ReferenceHarness, ReferenceHarnessConfig, ReferenceProfile,
};
use agalma_ipc::{Client, CommandSpec, Wire};

use agalma_conductor::harness_binding::{
    persist_binding, plan_binding, read_pinned_binding, BindPlan, HarnessKind,
};
use agalma_conductor::harness_process::{
    ready_json, reference_server_spec, resolve_server_binary, ProcessBinding, READY_TIMEOUT,
};
use agalma_conductor::sandbox_process::SandboxProcess;
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

const MODEL: &str = "test/offline";
const CHILD_SOCKET_ENV: &str = "AGALMA_TEST_HARNESS_SOCKET";
const CHILD_TEST_NAME: &str = "reference_server_child";

// ===========================================================================
// Fixture + helpers (mirrors tests/rebind.rs)
// ===========================================================================

/// Short, unique run directory. Unix socket paths must fit `SUN_LEN` (~104
/// bytes), so sockets live directly under a short `hp-<pid>-<n>` directory
/// rather than nested `sock/` subdirectories.
fn fresh(_name: &str) -> ScratchDir {
    ScratchDir::new("hp")
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
    let session = harness
        .create_session(create_req())
        .expect("create_session");
    let op: OperationHandle = harness.run_turn(&session, turn_req()).expect("run_turn");
    let state = harness.inspect_operation(&op).expect("inspect_operation");
    let events = harness.read_events(&op).expect("read_events");
    let _ = harness.cancel_operation(&op);
    let _ = harness.close_session(&session);
    let evidence = harness.stop_attempt(&attempt).expect("stop_attempt");
    assert!(
        evidence.process_group_gone,
        "cleanup evidence: {evidence:?}"
    );
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
        .env(CHILD_SOCKET_ENV, socket.to_string_lossy().into_owned());
    (spec, program)
}

fn spawn_process_reference(
    profile: ReferenceProfile,
    socket: &Path,
    scenario_dir: &Path,
    generation: u32,
    wire: Wire,
) -> ProcessBinding {
    let (spec, program) = reference_spec(profile, socket, scenario_dir);
    ProcessBinding::spawn(
        spec,
        program,
        socket.to_path_buf(),
        generation,
        MODEL,
        wire,
        READY_TIMEOUT,
    )
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
    let wire = Wire::from_env(Wire::Framed);
    let _ = reference::serve_wire(Path::new(&socket), profile, scenario_dir, wire);
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
        Wire::Connect,
    );
    assert_eq!(
        binding.wire(),
        Wire::Connect,
        "harness process wire is Connect"
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
    assert_eq!(
        record.implementation_version,
        expected.implementation_version
    );
    assert_eq!(record.binding_id, expected.binding_id);
    assert_eq!(record.generation, 1);
    assert_eq!(record.capabilities, expected.capabilities);
}

// ===========================================================================
// Case a' — wire parity: in-process vs framed vs Connect (IPCX.2)
// ===========================================================================

/// The harness boundary over Connect (IPCX.2): the same reference scenario
/// produces an identical normalized transcript under the in-process binding, the
/// framed process wire, and the Connect process wire. Both process wires also
/// reproduce the wire-independent static bind plan. This is the dual-binding
/// parity proof for the migrated boundary.
#[test]
fn wire_parity_framed_vs_connect_matches_in_process() {
    let dir = fresh("wire-parity");

    let ws_in = workspace(&dir, "ws-in-process");
    let mut inproc = ReferenceHarness::new(reference_config(
        ReferenceProfile::Success,
        dir.join("scenario-in-process"),
    ));
    let in_process = run_scenario(&mut inproc, &ws_in);

    let ws_framed = workspace(&dir, "ws-framed");
    let socket_framed = dir.join("framed.sock");
    let mut framed = spawn_process_reference(
        ReferenceProfile::Success,
        &socket_framed,
        &dir.join("scenario-framed"),
        1,
        Wire::Framed,
    );
    assert_eq!(framed.wire(), Wire::Framed);
    let framed_record = framed.binding().clone();
    let framed_transcript = run_scenario(&mut framed, &ws_framed);

    let ws_connect = workspace(&dir, "ws-connect");
    let socket_connect = dir.join("connect.sock");
    let mut connect = spawn_process_reference(
        ReferenceProfile::Success,
        &socket_connect,
        &dir.join("scenario-connect"),
        1,
        Wire::Connect,
    );
    assert_eq!(connect.wire(), Wire::Connect);
    let connect_record = connect.binding().clone();
    let connect_transcript = run_scenario(&mut connect, &ws_connect);

    // In-process vs framed vs Connect: identical normalized transcript.
    assert_eq!(
        in_process.describe, framed_transcript.describe,
        "framed identity parity"
    );
    assert_eq!(
        in_process.describe, connect_transcript.describe,
        "connect identity parity"
    );
    assert_eq!(
        in_process.state, framed_transcript.state,
        "framed state parity"
    );
    assert_eq!(
        in_process.state, connect_transcript.state,
        "connect state parity"
    );
    assert_eq!(
        in_process.events, framed_transcript.events,
        "framed event transcript parity"
    );
    assert_eq!(
        in_process.events, connect_transcript.events,
        "connect event transcript parity"
    );
    assert_eq!(
        in_process.artifact_digest, framed_transcript.artifact_digest,
        "framed produced artifact digest parity"
    );
    assert_eq!(
        in_process.artifact_digest, connect_transcript.artifact_digest,
        "connect produced artifact digest parity"
    );
    assert!(in_process.artifact_digest.is_some());

    // The bind gate is wire-independent: both process wires reproduce the plan.
    let expected = plan_binding(HarnessKind::Reference, 1, MODEL)
        .expect("static plan")
        .binding;
    assert_eq!(framed_record, expected, "framed bind plan parity");
    assert_eq!(connect_record, expected, "connect bind plan parity");
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
        Wire::Connect,
    );
    assert_eq!(b1.wire(), Wire::Connect);
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
        Wire::Connect,
    );
    assert_eq!(b2.wire(), Wire::Connect);
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
    let start_params =
        serde_json::to_value(start_req(&workspace(&dir, "ws"))).expect("start params");
    client
        .call_with_id(1, "start_attempt", start_params)
        .expect("start_attempt");

    // Duplicate request id: the recorded receipt is replayed, not re-run.
    let session_params = serde_json::to_value(create_req()).expect("create params");
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
        Wire::Connect,
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

// ===========================================================================
// Case e — startup-failure cleanup ownership (agalma-5l5)
//
// `std::process::Child` does not kill on drop, so a startup failure between
// spawn and full construction used to leak the server and its descendants. The
// supervisor now installs a cleanup owner (`SupervisedChild`) at spawn and
// retains it through READY, the bind gate, and connect. Each case below drives a
// failing mock (via `/bin/sh`, offline, no live provider), asserts the leader
// and its descendant are gone, the leader is reaped, the diagnostic survives,
// and a subsequent start on the same socket still succeeds.
// ===========================================================================

/// `Some(stat)` while `pid` exists (`stat` is `ps` process state, `Z` = zombie),
/// `None` once it is fully gone. A reaped child has no `ps` entry.
fn proc_stat(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stat.is_empty() {
        None
    } else {
        Some(stat)
    }
}

/// Poll until `pid` has no `ps` entry (gone and reaped), up to `timeout`.
fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if proc_stat(pid).is_none() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A `/bin/sh` mock component server. It records its own pid and the pid of a
/// background descendant in the same process group, optionally prints one
/// `ready` stdout line, then stays alive well past any startup timeout.
fn mock_spec(leader: &Path, descendant: &Path, ready: Option<&str>) -> CommandSpec {
    let ready_cmd = match ready {
        Some(line) => format!("printf '%s\\n' '{line}'; "),
        None => String::new(),
    };
    let script = format!(
        "echo $$ > '{leader}'; /bin/sleep 30 & echo $! > '{descendant}'; {ready_cmd}exec /bin/sleep 30",
        leader = leader.display(),
        descendant = descendant.display(),
    );
    CommandSpec::new("/bin/sh").arg("-c").arg(script)
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read pid {}: {e}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse pid {}: {e}", path.display()))
}

/// Spawn a failing mock binding and assert the whole process tree is gone and
/// the leader is reaped while `expect` survives in the diagnostic.
fn assert_failed_startup_cleanup(name: &str, ready: Option<&str>, expect: Option<&str>) -> PathBuf {
    let dir = fresh(name);
    let socket = dir.join("s.sock");
    let leader = dir.join("leader.pid");
    let descendant = dir.join("descendant.pid");

    let err = match ProcessBinding::spawn(
        mock_spec(&leader, &descendant, ready),
        PathBuf::from("/bin/sh"),
        socket.clone(),
        1,
        MODEL,
        Wire::Connect,
        Duration::from_millis(300),
    ) {
        Ok(_) => panic!("mock startup must fail"),
        Err(e) => e,
    };
    if let Some(fragment) = expect {
        assert!(err.contains(fragment), "diagnostic preserved: {err:?}");
    }

    let leader_pid = read_pid(&leader);
    let descendant_pid = read_pid(&descendant);

    assert!(
        wait_gone(leader_pid, Duration::from_secs(5)),
        "{name}: leader {leader_pid} survived failed startup (stat {:?})",
        proc_stat(leader_pid)
    );
    // A killed-but-unreaped leader would still show as a zombie `Z`.
    assert!(
        proc_stat(leader_pid).is_none(),
        "{name}: leader {leader_pid} not reaped (stat {:?})",
        proc_stat(leader_pid)
    );
    assert!(
        wait_gone(descendant_pid, Duration::from_secs(5)),
        "{name}: descendant {descendant_pid} survived failed startup (stat {:?})",
        proc_stat(descendant_pid)
    );

    socket
}

#[test]
fn startup_ready_timeout_kills_and_reaps_process_tree() {
    assert_failed_startup_cleanup("fail-timeout", None, Some("did not announce READY"));
}

#[test]
fn startup_malformed_ready_kills_and_reaps_process_tree() {
    assert_failed_startup_cleanup(
        "fail-malformed",
        Some("READY {this is not json"),
        Some("malformed READY"),
    );
}

#[test]
fn startup_bind_rejection_kills_and_reaps_process_tree() {
    let bad_api = r#"READY {"api":"NotHarnessApi","api_version":0,"impl":"mock","impl_version":"v1","capabilities":[]}"#;
    assert_failed_startup_cleanup("fail-bind", Some(bad_api), Some("bind gate rejected"));
}

#[test]
fn startup_unconnectable_socket_kills_and_reaps_process_tree() {
    // Correct API identity, but the mock never binds the socket, so connect fails.
    let good_api = r#"READY {"api":"HarnessApi","api_version":0,"impl":"mock","impl_version":"v1","capabilities":[]}"#;
    let socket = assert_failed_startup_cleanup("fail-socket", Some(good_api), None);
    // The failed start left no listener on the socket.
    assert!(
        Client::connect(&socket).is_err(),
        "no server should be listening after a failed start"
    );
}

#[test]
fn retry_on_the_same_socket_after_failed_startup_succeeds() {
    let dir = fresh("retry");
    let socket = dir.join("r.sock");
    let leader = dir.join("leader.pid");
    let descendant = dir.join("descendant.pid");

    let err = match ProcessBinding::spawn(
        mock_spec(&leader, &descendant, None),
        PathBuf::from("/bin/sh"),
        socket.clone(),
        1,
        MODEL,
        Wire::Connect,
        Duration::from_millis(300),
    ) {
        Ok(_) => panic!("first start fails"),
        Err(e) => e,
    };
    assert!(err.contains("did not announce READY"), "{err:?}");
    assert!(wait_gone(read_pid(&leader), Duration::from_secs(5)));

    // Same socket path: the real reference server starts and runs cleanly.
    let scenario = dir.join("scenario");
    std::fs::create_dir_all(&scenario).expect("scenario dir");
    let binding = spawn_process_reference(
        ReferenceProfile::Success,
        &socket,
        &scenario,
        1,
        Wire::Connect,
    );
    let pid = binding.pid();
    assert!(
        proc_stat(pid).is_some(),
        "retry binding must be alive on the same socket"
    );

    // Normal drop stops it (no leak on the happy path).
    drop(binding);
    assert!(
        wait_gone(pid, Duration::from_secs(5)),
        "retry binding leaked after drop (stat {:?})",
        proc_stat(pid)
    );
}

#[test]
fn supervisor_startup_restart_drop_lifecycle() {
    let dir = fresh("lifecycle");
    let socket = dir.join("l.sock");
    let scenario = dir.join("scenario");
    std::fs::create_dir_all(&scenario).expect("scenario dir");

    // Normal startup.
    let mut binding = spawn_process_reference(
        ReferenceProfile::Success,
        &socket,
        &scenario,
        1,
        Wire::Connect,
    );
    let started = binding.pid();
    assert!(proc_stat(started).is_some(), "server alive after startup");

    // Restart keeps the binding id/generation and yields a live server.
    binding.restart().expect("restart");
    let restarted = binding.pid();
    assert!(proc_stat(restarted).is_some(), "server alive after restart");

    // Drop stops the supervised process tree.
    let final_pid = binding.pid();
    drop(binding);
    assert!(
        wait_gone(final_pid, Duration::from_secs(5)),
        "server leaked after drop (stat {:?})",
        proc_stat(final_pid)
    );
}

#[test]
fn shared_boundary_sandbox_ready_timeout_leaves_no_process() {
    // The cleanup owner lives in the shared `spawn_supervised` helper, so a
    // READY failure on a non-harness boundary is covered as well.
    let dir = fresh("shared-sandbox");
    let socket = dir.join("s.sock");
    let leader = dir.join("leader.pid");
    let descendant = dir.join("descendant.pid");

    let err = match SandboxProcess::spawn_with_spec(
        mock_spec(&leader, &descendant, None),
        PathBuf::from("/bin/sh"),
        socket,
        Duration::from_millis(300),
    ) {
        Ok(_) => panic!("sandbox mock startup must fail"),
        Err(e) => e,
    };
    assert!(err.contains("did not announce READY"), "{err:?}");

    let leader_pid = read_pid(&leader);
    assert!(
        wait_gone(leader_pid, Duration::from_secs(5)),
        "sandbox leader {leader_pid} survived failed startup"
    );
    assert!(proc_stat(leader_pid).is_none(), "sandbox leader not reaped");
    assert!(
        wait_gone(read_pid(&descendant), Duration::from_secs(5)),
        "sandbox descendant survived failed startup"
    );
}
