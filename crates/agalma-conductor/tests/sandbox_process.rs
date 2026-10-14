//! B1.3 sandbox process-boundary evidence (`agalma-0ht.4`).
//!
//! Dual-binding evidence for the extracted `SandboxApi`: a shadow-parity run of
//! one `LaunchSpec` scenario in-process vs through the supervised sandbox
//! component server; a staged rebind that keeps an in-flight confined child
//! alive across a server swap; a rollback without state loss; and IPC
//! conformance for [`agalma_ipc::SandboxClient`] (duplicate-id replay,
//! dead-server `Unavailable`/`UnknownOutcome`).
//!
//! Scratch state lives under `target/test-runs/sandbox-process-*` (never `/tmp`).
//! The server child is re-exec'd from this test binary (or the real
//! `agalma-sandbox-server` binary when built) so a package-scoped
//! `cargo test -p agalma-conductor` run still exercises the real process
//! boundary. Scratch dirs auto-remove on success (kept on failure).

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agalma_contracts::ids::AttemptId;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi};
use agalma_ipc::{Client, CommandSpec, SandboxClient, Wire};
use agalma_sandbox::{SeatbeltSandbox, PRODUCT_PROFILE, SERVER_BIN};

use agalma_conductor::harness_process::{ready_json, resolve_server_binary};
use agalma_conductor::sandbox_process::{
    sandbox_server_spec, sandbox_wire, SandboxProcess, SandboxSource,
};
use agalma_testkit::ScratchDir;

const CHILD_SOCKET_ENV: &str = "AGALMA_TEST_SANDBOX_SOCKET";
const CHILD_TEST_NAME: &str = "sandbox_server_child";
const READY_TIMEOUT: Duration = Duration::from_secs(20);

// ===========================================================================
// Fixture + helpers (mirrors tests/harness_process.rs)
// ===========================================================================

/// Short, unique run directory. Unix socket paths must fit `SUN_LEN` (~104
/// bytes), so sockets live directly under a short `sp-<pid>-<n>` directory.
fn fresh() -> ScratchDir {
    ScratchDir::new("sp")
}

/// One confined-launch scratch tree: attempt/protected/sock roots plus the
/// rendered profile.
struct Run {
    attempt: PathBuf,
    protected: PathBuf,
    sock: PathBuf,
    extra: PathBuf,
    profile: PathBuf,
    marker: PathBuf,
}

impl Run {
    fn new(dir: &Path, name: &str) -> Run {
        let base = dir.join(name);
        for sub in ["attempt", "protected", "sock", "extra"] {
            fs::create_dir_all(base.join(sub)).expect("create run subdir");
        }
        let profile = base.join("worker.sb");
        fs::write(&profile, PRODUCT_PROFILE).expect("write profile");
        Run {
            attempt: fs::canonicalize(base.join("attempt")).expect("attempt"),
            protected: fs::canonicalize(base.join("protected")).expect("protected"),
            sock: fs::canonicalize(base.join("sock")).expect("sock"),
            extra: fs::canonicalize(base.join("extra")).expect("extra"),
            profile,
            marker: base.join("attempt").join("marker"),
        }
    }

    fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        );
        for (key, sub) in [
            ("TMPDIR", "tmp"),
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
        ] {
            let dir = self.attempt.join(sub);
            fs::create_dir_all(&dir).expect("create attempt-scoped dir");
            env.insert(key.to_string(), dir.to_string_lossy().into_owned());
        }
        env
    }

    /// The `LaunchSpec` scenario: `/bin/sh` writes a file under the attempt dir,
    /// then sleeps so it is alive when probed.
    fn spec(&self) -> LaunchSpec {
        let script = format!(
            "echo ready > {marker}; sleep 300",
            marker = self.marker.display()
        );
        LaunchSpec {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script],
            cwd: self.attempt.to_string_lossy().into_owned(),
            env: self.env(),
            profile: self.profile.to_string_lossy().into_owned(),
            attempt: AttemptId::derive(1),
            attempt_dir: self.attempt.to_string_lossy().into_owned(),
            protected_dir: self.protected.to_string_lossy().into_owned(),
            sock_dir: self.sock.to_string_lossy().into_owned(),
            extra_ro_roots: vec![self.extra.to_string_lossy().into_owned()],
            extra_rw_roots: Vec::new(),
        }
    }
}

/// The observable, timing/ID-free transcript of one sandbox scenario.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    alive_before: bool,
    file_written: bool,
    group_gone: bool,
    detail_nonempty: bool,
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Launch, probe, and terminate one confined child through `sandbox`.
fn drive(sandbox: &mut dyn SandboxApi, run: &Run) -> Outcome {
    let child = sandbox.launch(run.spec()).expect("confined launch");
    let alive_before = sandbox.alive(&child).expect("alive");
    let file_written = wait_for(&run.marker, Duration::from_secs(10));
    let evidence = sandbox.terminate(&child).expect("terminate");
    Outcome {
        alive_before,
        file_written,
        group_gone: evidence.process_group_gone,
        detail_nonempty: !evidence.detail.is_empty(),
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
fn server_spec(socket: &Path) -> (CommandSpec, PathBuf) {
    match resolve_server_binary(SERVER_BIN) {
        Ok(bin) => (sandbox_server_spec(socket), bin),
        Err(_) => {
            let exe = std::env::current_exe().expect("current_exe");
            let spec = CommandSpec::new(exe.to_string_lossy().into_owned())
                .args([
                    CHILD_TEST_NAME.to_string(),
                    "--exact".to_string(),
                    "--test-threads=1".to_string(),
                    "--nocapture".to_string(),
                ])
                .env(CHILD_SOCKET_ENV, socket.to_string_lossy().into_owned());
            (spec, exe)
        }
    }
}

fn spawn_server(socket: &Path) -> SandboxProcess {
    spawn_server_wire(socket, sandbox_wire())
}

fn spawn_server_wire(socket: &Path, wire: Wire) -> SandboxProcess {
    let (spec, program) = server_spec(socket);
    SandboxProcess::spawn_with_spec_wire(spec, program, socket.to_path_buf(), wire, READY_TIMEOUT)
        .unwrap_or_else(|e| panic!("spawn sandbox server: {e}"))
}

/// The child entry point: when spawned with [`CHILD_SOCKET_ENV`], serve the
/// Seatbelt sandbox on the named socket. A no-op under a normal test run.
#[test]
fn sandbox_server_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let _ = agalma_sandbox::serve(Path::new(&socket));
}

/// Live process smoke under a **long** state dir: the production
/// [`SandboxSource::ensure`] binds through the bounded alias so
/// `<state_dir>/sandbox.sock` fits `SUN_LEN`, while the socket file still lands
/// under the state dir. One real confined launch round-trips. (Sandbox has no
/// transport selector — it is always process.)
#[test]
fn live_process_smoke_sandbox_long_state_dir() {
    let binary = resolve_server_binary(SERVER_BIN).expect(
        "required sandbox smoke binary missing; run cargo build --offline --workspace --bins",
    );
    let dir = fresh();
    let state = dir.join("n".repeat(60));
    std::fs::create_dir_all(&state).expect("long state dir");
    assert!(
        state.join("sandbox.sock").as_os_str().len() >= 104,
        "direct socket path must exceed SUN_LEN"
    );

    let source = SandboxSource::new(state.clone());
    source.ensure().expect("production sandbox ensure");
    eprintln!(
        "sandbox smoke: binary={} state={} socket={} wire={} pid={}",
        binary.display(),
        state.display(),
        source.socket().display(),
        sandbox_wire().as_str(),
        source.pid().expect("server started")
    );
    assert!(
        source.socket().as_os_str().len() < 104,
        "bounded socket path"
    );
    let server_pid = source.pid().expect("server started");
    let mut client = source.client().expect("connect sandbox client");

    let run = Run::new(&dir, "smoke");
    let child = client.launch(run.spec()).expect("confined launch");
    assert!(client.alive(&child).expect("alive"), "child is alive");
    let evidence = client.terminate(&child).expect("terminate");
    eprintln!("sandbox smoke child cleanup: {evidence:?}");
    assert!(
        evidence.process_group_gone,
        "termination evidence: {}",
        evidence.detail
    );

    assert!(
        state.join("sandbox.sock").exists(),
        "socket file lands under the state dir through the alias"
    );
    drop(client);
    drop(source);
    assert!(
        !pid_alive(server_pid),
        "sandbox server is gone after source drop"
    );
    eprintln!("sandbox smoke server cleanup: pid {server_pid} gone");
}

// ===========================================================================
// Case a — shadow parity: in-process vs process binding
// ===========================================================================

#[test]
fn shadow_parity_in_process_vs_process_sandbox() {
    let dir = fresh();

    // In-process: a direct SeatbeltSandbox under a Tokio reactor (the backend
    // launches through `tokio::process`).
    let run_in = Run::new(&dir, "in-process");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut inproc = SeatbeltSandbox::new();
    let in_process = runtime.block_on(async { drive(&mut inproc, &run_in) });

    // Process: a supervised sandbox component server (default Connect wire)
    // plus a client connected over that same wire.
    let run_proc = Run::new(&dir, "process");
    let socket = dir.join("a.sock");
    let process = spawn_server(&socket);
    let mut client = process.client().expect("connect sandbox client");
    let via_process = drive(&mut client, &run_proc);

    assert_eq!(in_process, via_process, "sandbox transcript parity");
    assert!(in_process.alive_before, "child is alive before terminate");
    assert!(
        in_process.file_written,
        "confined command writes the marker"
    );
    assert!(in_process.group_gone, "terminate reports the group gone");
    assert!(
        in_process.detail_nonempty,
        "termination evidence is recorded"
    );

    drop(client);
    let evidence = {
        let mut process = process;
        process.stop()
    };
    assert!(
        evidence.process_group_gone,
        "server stop evidence: {evidence:?}"
    );
}

// ===========================================================================
// Case a' — wire parity: in-process vs framed vs Connect (IPCX.3)
// ===========================================================================

/// The sandbox boundary over Connect (IPCX.3): the same `LaunchSpec` scenario
/// produces an identical normalized transcript in-process, over the framed
/// process wire, and over the Connect process wire. This is the dual-binding
/// parity proof for the migrated boundary.
#[test]
fn wire_parity_framed_vs_connect_matches_in_process() {
    let dir = fresh();

    // In-process baseline.
    let run_in = Run::new(&dir, "wire-in-process");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut inproc = SeatbeltSandbox::new();
    let in_process = runtime.block_on(async { drive(&mut inproc, &run_in) });

    // Framed process wire.
    let run_framed = Run::new(&dir, "wire-framed");
    let socket_framed = dir.join("wf.sock");
    let process_framed = spawn_server_wire(&socket_framed, Wire::Framed);
    assert_eq!(process_framed.wire(), Wire::Framed);
    let mut client_framed = process_framed.client().expect("framed client");
    let framed = drive(&mut client_framed, &run_framed);
    drop(client_framed);
    let _ = {
        let mut process = process_framed;
        process.stop()
    };

    // Connect process wire.
    let run_connect = Run::new(&dir, "wire-connect");
    let socket_connect = dir.join("wc.sock");
    let process_connect = spawn_server_wire(&socket_connect, Wire::Connect);
    assert_eq!(process_connect.wire(), Wire::Connect);
    let mut client_connect = process_connect.client().expect("connect client");
    let connect = drive(&mut client_connect, &run_connect);
    drop(client_connect);
    let _ = {
        let mut process = process_connect;
        process.stop()
    };

    assert_eq!(in_process, framed, "framed sandbox transcript parity");
    assert_eq!(in_process, connect, "connect sandbox transcript parity");
}

// ===========================================================================
// Case b — staged rebind with an in-flight child alive
// ===========================================================================

#[test]
fn staged_rebind_keeps_inflight_child_alive() {
    let dir = fresh();

    // "Generation 1": the server that owns the in-flight child.
    let socket_a = dir.join("b-a.sock");
    let mut server_a = spawn_server(&socket_a);
    let mut client_a = server_a.client().expect("client a");
    let run_a = Run::new(&dir, "rebind-a");
    let child_a = client_a
        .launch(run_a.spec())
        .expect("launch in-flight child");

    // Stage the candidate ("generation 2") and swap the next operation onto it.
    let socket_b = dir.join("b-b.sock");
    let mut server_b = spawn_server(&socket_b);
    let mut client_b = server_b.client().expect("client b");
    let run_b = Run::new(&dir, "rebind-b");
    let child_b = client_b.launch(run_b.spec()).expect("launch next child");

    // The in-flight child is pinned to server A and unaffected by the swap.
    assert!(
        client_a.alive(&child_a).expect("alive a"),
        "in-flight child stays alive across the rebind"
    );
    assert!(
        client_b.alive(&child_b).expect("alive b"),
        "rebound operation uses the candidate server"
    );
    assert_ne!(child_a.pid, child_b.pid, "the two children are distinct");

    let evidence_a = client_a.terminate(&child_a).expect("terminate a");
    let evidence_b = client_b.terminate(&child_b).expect("terminate b");
    assert!(
        evidence_a.process_group_gone,
        "detail: {}",
        evidence_a.detail
    );
    assert!(
        evidence_b.process_group_gone,
        "detail: {}",
        evidence_b.detail
    );

    drop(client_a);
    drop(client_b);
    let _ = server_a.stop();
    let _ = server_b.stop();
}

// ===========================================================================
// Case c — rollback without state loss
// ===========================================================================

#[test]
fn rollback_keeps_prior_child_and_server() {
    let dir = fresh();

    // Prior active binding: server A owns a launched child.
    let socket_a = dir.join("c-a.sock");
    let mut server_a = spawn_server(&socket_a);
    let mut client_a = server_a.client().expect("client a");
    let run_a = Run::new(&dir, "rollback-a");
    let child_a = client_a.launch(run_a.spec()).expect("launch prior child");

    // Candidate binding: server B is staged then rolled back (stopped).
    let socket_b = dir.join("c-b.sock");
    let mut server_b = spawn_server(&socket_b);
    let b_pid = server_b.pid();
    let evidence = server_b.stop();
    assert!(
        evidence.process_group_gone,
        "candidate stopped: {evidence:?}"
    );
    assert!(!pid_alive(b_pid), "candidate server is gone after rollback");

    // No state lost: the prior child survives and is still terminable through A.
    assert!(
        client_a.alive(&child_a).expect("alive prior"),
        "prior child survives the rollback"
    );
    let evidence = client_a.terminate(&child_a).expect("terminate prior");
    assert!(evidence.process_group_gone, "detail: {}", evidence.detail);

    drop(client_a);
    let _ = server_a.stop();
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

// ===========================================================================
// Case d — IPC conformance for the sandbox client
// ===========================================================================

#[test]
fn sandbox_client_duplicate_id_replay_and_connection_loss() {
    let dir = fresh();
    let run = Run::new(&dir, "conformance");
    let socket = dir.join("d.sock");
    // Raw framed `Client` conformance: the server must serve the framed wire.
    let mut server = spawn_server_wire(&socket, Wire::Framed);

    let mut client = Client::connect(&socket).expect("connect");
    let spec_params = serde_json::to_value(run.spec()).expect("spec params");

    // Duplicate request id: the recorded receipt (same child pid) is replayed,
    // not re-run.
    let first = client
        .call_with_id(1, "launch", spec_params.clone())
        .expect("launch #1");
    let replay = client
        .call_with_id(1, "launch", spec_params)
        .expect("launch #2 (replay)");
    assert_eq!(first, replay, "same id replays the recorded launch receipt");

    // A fresh id advances (a distinct child) and can be terminated.
    let second = client
        .call_with_id(2, "launch", serde_json::to_value(run.spec()).expect("spec"))
        .expect("launch #3");
    assert_ne!(first, second, "a fresh id launches a new child");
    client
        .call_with_id(3, "terminate", first.clone())
        .expect("terminate first");
    client
        .call_with_id(4, "terminate", second)
        .expect("terminate second");

    // Connection loss: stop the server; the next call is not silently lost.
    let _ = server.stop();
    let err = client
        .call_with_id(5, "alive", first)
        .expect_err("call after server death must fail");
    assert!(
        err.is_unavailable() || err.is_unknown_outcome(),
        "connection loss surfaces as Unavailable/UnknownOutcome: {err:?}"
    );

    // Connecting to a socket with no server is Unavailable.
    let missing = SandboxClient::connect(&dir.join("nope.sock"));
    assert!(
        matches!(missing, Err(agalma_ipc::IpcError::Unavailable(_))),
        "missing socket is Unavailable"
    );
}

/// `ready_json` is re-exported here so the re-exec'd child and the supervisor
/// agree on the handshake marker; exercised directly for the record.
#[test]
fn ready_marker_is_parsed() {
    let line = "test sandbox_server_child ... READY {\"api\":\"SandboxApi\"}";
    assert_eq!(ready_json(line), Some("{\"api\":\"SandboxApi\"}"));
}
