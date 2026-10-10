//! B1.4 workspace process-boundary evidence (`agalma-0ht.5`).
//!
//! Dual-binding evidence for the extracted `WorkspaceApi` (+ the repo-mode
//! `WorkspaceOps` surface the conductor drives): shadow-parity runs of the same
//! git scenarios in-process vs through the supervised workspace component
//! server (with comparable results and produced git-state digests); a staged
//! rebind that keeps an in-flight checkout pinned to its preparing server across
//! a server swap; a rollback without state loss; and IPC conformance for
//! [`agalma_ipc::WorkspaceClient`] (duplicate-id replay, dead-server
//! `Unavailable`/`UnknownOutcome`).
//!
//! Scratch state lives under `target/test-runs/wp-*` (never `/tmp`). The server
//! child is re-exec'd from this test binary (or the real
//! `agalma-workspace-server` binary when built) so a package-scoped
//! `cargo test -p agalma-conductor` run still exercises the real process
//! boundary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agalma_contracts::{Checkout, ContractError, ExecutionId};
use agalma_ipc::{
    Client, CheckoutCall, CommandSpec, IpcError, IntegrateOriginParams, PrepareRepoParams,
    WorkspaceClient, WorkspaceOps,
};
use agalma_workspace::server::SERVER_BIN;
use agalma_workspace::Workspace;

use agalma_conductor::harness_process::resolve_server_binary;
use agalma_conductor::workspace_process::{workspace_server_spec, WorkspaceProcess};

const CHILD_SOCKET_ENV: &str = "AGALMA_TEST_WORKSPACE_SOCKET";
const CHILD_BASE_ENV: &str = "AGALMA_TEST_WORKSPACE_BASE_DIR";
const CHILD_TEST_NAME: &str = "workspace_server_child";
const READY_TIMEOUT: Duration = Duration::from_secs(20);

// ===========================================================================
// Fixtures + helpers (mirrors tests/rebind.rs and tests/sandbox_process.rs)
// ===========================================================================

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

/// Short, unique run directory. Unix socket paths must fit `SUN_LEN` (~104
/// bytes), so sockets live directly under a short `wp-<pid>-<n>` directory.
static RUN_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fresh() -> PathBuf {
    let n = RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("wp-{}-{n}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(out.status.success(), "{program} {args:?}: {}", stderr_of(&out));
}

/// Run `git` in `dir`, capturing stdout (trimmed); panic on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A scratch origin repo (git) seeded with one source file.
fn build_origin(origin: &Path) -> PathBuf {
    std::fs::create_dir_all(origin.join("src")).expect("origin src");
    std::fs::write(
        origin.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .expect("seed source");
    run_ok("git", origin, &["init", "-q", "-b", "main"]);
    run_ok("git", origin, &["config", "user.email", "agalma@localhost"]);
    run_ok("git", origin, &["config", "user.name", "Agalma"]);
    run_ok("git", origin, &["add", "-A"]);
    run_ok("git", origin, &["commit", "-qm", "seed"]);
    origin.to_path_buf()
}

// ===========================================================================
// Process server spawning
// ===========================================================================

/// The program to spawn plus any child-test arguments.
///
/// Prefers the real component-server binary when it is built (workspace runs);
/// falls back to re-exec'ing this test binary as the server so a package-scoped
/// `cargo test -p agalma-conductor` run still exercises the process boundary.
fn server_spec(socket: &Path, base_dir: &Path) -> (CommandSpec, PathBuf) {
    match resolve_server_binary(SERVER_BIN) {
        Ok(bin) => (workspace_server_spec(socket, base_dir), bin),
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
                .env(CHILD_BASE_ENV, base_dir.to_string_lossy().into_owned());
            (spec, exe)
        }
    }
}

fn spawn_server(socket: &Path, base_dir: &Path) -> WorkspaceProcess {
    let (spec, program) = server_spec(socket, base_dir);
    WorkspaceProcess::spawn_with_spec(spec, program, socket.to_path_buf(), READY_TIMEOUT)
        .unwrap_or_else(|e| panic!("spawn workspace server: {e}"))
}

/// The child entry point: when spawned with [`CHILD_SOCKET_ENV`], serve the
/// git-backed workspace on the named socket rooted at [`CHILD_BASE_ENV`]. A
/// no-op under a normal test run.
#[test]
fn workspace_server_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let base = std::env::var(CHILD_BASE_ENV).expect("child base dir");
    let _ = agalma_workspace::server::serve(Path::new(&socket), Path::new(&base));
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

// ===========================================================================
// Case a — shadow parity: in-process vs process binding
// ===========================================================================

/// Normalized, sha/timing-free transcript of the template-mode scenario
/// (`prepare`/`diff`/`integrate`/`tag`/`revert`).
#[derive(Debug, PartialEq, Eq)]
struct TemplateOutcome {
    diff_contains_new: bool,
    candidate_is_result: bool,
    main_tree: String,
    tag_points_at_candidate: bool,
    reverted_equals_base: bool,
}

/// Drive the frozen trait surface against a scratch repo built from a template.
fn drive_template(ws: &mut dyn WorkspaceOps, dir: &Path) -> TemplateOutcome {
    let template = dir.join("template");
    std::fs::create_dir_all(&template).expect("template");
    std::fs::write(template.join("hello.txt"), "hello\n").expect("template file");

    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let exec = ExecutionId::new("exec:wp-template:1");

    let checkout = ws
        .prepare(template.to_str().expect("template utf8"), &exec)
        .expect("prepare");
    std::fs::write(
        Path::new(&checkout.path).join("hello.txt"),
        "hello world\n",
    )
    .expect("dirty edit");

    let diff = ws.diff(&checkout).expect("diff");
    let diff_contains_new = diff.contains("+hello world");

    let receipt = ws.integrate(&checkout, &checkout.base_sha).expect("integrate");
    let candidate_is_result = receipt.candidate_sha == receipt.result_sha;

    let tag = "wp/extra";
    ws.tag(tag, &receipt.result_sha).expect("tag");

    let repo = Path::new(&checkout.path);
    let main_tree = git(repo, &["rev-parse", "refs/heads/main^{tree}"]);
    let tag_sha = git(repo, &["rev-parse", &format!("refs/tags/{tag}")]);
    let tag_points_at_candidate = tag_sha == receipt.result_sha;

    ws.revert(&checkout.base_sha).expect("revert");
    let reverted_tree = git(repo, &["rev-parse", "refs/heads/main^{tree}"]);
    let base_tree = git(repo, &["rev-parse", &format!("{}^{{tree}}", checkout.base_sha)]);

    TemplateOutcome {
        diff_contains_new,
        candidate_is_result,
        main_tree,
        tag_points_at_candidate,
        reverted_equals_base: reverted_tree == base_tree,
    }
}

#[test]
fn shadow_parity_template_ops() {
    let dir_in = fresh();
    let mut inproc = Workspace::new(dir_in.join("state"));
    let in_process = drive_template(&mut inproc, &dir_in);

    let dir_proc = fresh();
    let socket = dir_proc.join("a.sock");
    let mut process = spawn_server(&socket, &dir_proc.join("state"));
    let mut client = process.client().expect("connect workspace client");
    let via_process = drive_template(&mut client, &dir_proc);

    assert_eq!(in_process, via_process, "workspace template transcript parity");
    assert!(in_process.diff_contains_new, "diff shows the candidate edit");
    assert!(in_process.candidate_is_result, "integration CAS is a fast-forward");
    assert!(in_process.tag_points_at_candidate, "tag points at the candidate");
    assert!(in_process.reverted_equals_base, "revert restores the base tree");

    drop(client);
    let evidence = process.stop();
    assert!(evidence.process_group_gone, "server stop evidence: {evidence:?}");
}

/// Normalized, sha/timing-free transcript of the repo-mode scenario
/// (`prepare_repo`/`diff`/`commit_candidate`/`integrate_origin`/`revert_origin`).
#[derive(Debug, PartialEq, Eq)]
struct RepoOutcome {
    diff_contains_new: bool,
    verify_checkout_exists: bool,
    before_is_none: bool,
    after_is_candidate: bool,
    result_is_candidate: bool,
    main_tree: String,
    reverted_equals_base: bool,
}

/// Drive the repo-mode surface the conductor's checkout/verify/integrate phases
/// use, against a scratch origin.
fn drive_repo(ws: &mut dyn WorkspaceOps, dir: &Path) -> RepoOutcome {
    let origin = build_origin(&dir.join("origin"));
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let exec = ExecutionId::new("exec:wp-repo:1");

    let checkout = ws
        .prepare_repo(&origin, "main", &exec, &state)
        .expect("prepare_repo");
    std::fs::write(
        Path::new(&checkout.path).join("src").join("lib.rs"),
        "//! Candidate.\npub fn answer() -> u32 { 42 }\n",
    )
    .expect("dirty edit");

    let diff = ws.diff(&checkout).expect("diff");
    let diff_contains_new = diff.contains("pub fn answer() -> u32 { 42 }");

    let before = ws.origin_receipt(&checkout, &origin).expect("origin_receipt before");
    let sha = ws.commit_candidate(&checkout).expect("commit_candidate");

    let verify = ws
        .prepare_verify_checkout(&checkout, &state, &exec, &sha)
        .expect("prepare_verify_checkout");
    let verify_checkout_exists = verify.join(".git").is_dir();

    let receipt = ws
        .integrate_origin(&checkout, &origin, &checkout.base_sha)
        .expect("integrate_origin");
    let after = ws.origin_receipt(&checkout, &origin).expect("origin_receipt after");

    let main_tree = git(&origin, &["rev-parse", "refs/heads/main^{tree}"]);
    ws.revert_origin(&origin, &checkout.base_sha, &receipt.result_sha)
        .expect("revert_origin");
    let reverted_tree = git(&origin, &["rev-parse", "refs/heads/main^{tree}"]);
    let base_tree = git(&origin, &["rev-parse", &format!("{}^{{tree}}", checkout.base_sha)]);

    RepoOutcome {
        diff_contains_new,
        verify_checkout_exists,
        before_is_none: before.is_none(),
        after_is_candidate: after
            .map(|r| r.result_sha == receipt.result_sha)
            .unwrap_or(false),
        result_is_candidate: receipt.result_sha == sha,
        main_tree,
        reverted_equals_base: reverted_tree == base_tree,
    }
}

#[test]
fn shadow_parity_repo_ops() {
    let dir_in = fresh();
    let mut inproc = Workspace::new(dir_in.join("state"));
    let in_process = drive_repo(&mut inproc, &dir_in);

    let dir_proc = fresh();
    let socket = dir_proc.join("a.sock");
    let mut process = spawn_server(&socket, &dir_proc.join("state"));
    let mut client = process.client().expect("connect workspace client");
    let via_process = drive_repo(&mut client, &dir_proc);

    assert_eq!(in_process, via_process, "workspace repo transcript parity");
    assert!(in_process.diff_contains_new, "diff shows the candidate edit");
    assert!(in_process.verify_checkout_exists, "verify checkout has git metadata");
    assert!(in_process.before_is_none, "no receipt before integration");
    assert!(in_process.result_is_candidate, "candidate sha equals result sha");
    assert!(in_process.after_is_candidate, "receipt after integration is the candidate");
    assert!(in_process.reverted_equals_base, "revert restores the base tree");

    drop(client);
    let evidence = process.stop();
    assert!(evidence.process_group_gone, "server stop evidence: {evidence:?}");
}

// ===========================================================================
// Case b — staged rebind with an in-flight checkout pinned
// ===========================================================================

#[test]
fn staged_rebind_keeps_pinned_checkout_usable() {
    let dir = fresh();
    let origin = build_origin(&dir.join("origin"));
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let exec = ExecutionId::new("exec:wp-rebind:1");

    // "Generation 1": prepare + commit a candidate on server A. The resulting
    // checkout (and its committed SHA) is the in-flight artifact pinned to A.
    let socket_a = dir.join("b-a.sock");
    let mut server_a = spawn_server(&socket_a, &state);
    let mut ws_a = server_a.client().expect("client a");
    let checkout = ws_a
        .prepare_repo(&origin, "main", &exec, &state)
        .expect("prepare in-flight");
    std::fs::write(Path::new(&checkout.path).join("added.txt"), "added\n").expect("dirty edit");
    let committed = ws_a.commit_candidate(&checkout).expect("commit in-flight");
    let a_pid = server_a.pid();

    // Stage the candidate ("generation 2") and continue the next operation on it.
    let socket_b = dir.join("b-b.sock");
    let mut server_b = spawn_server(&socket_b, &state);
    let ws_b = server_b.client().expect("client b");

    // The pinned checkout is honored by B: integration uses A's committed SHA.
    let receipt = ws_b
        .integrate_origin(&checkout, &origin, &checkout.base_sha)
        .expect("integrate via candidate server");
    assert_eq!(receipt.result_sha, committed, "candidate server lands A's pinned SHA");
    assert_eq!(
        git(&origin, &["rev-parse", "refs/heads/main"]),
        committed,
        "origin main is at the pinned candidate"
    );

    // A is still alive and usable: the swap never rebound the in-flight artifact.
    assert!(pid_alive(a_pid), "server A stays alive across the rebind");
    assert_eq!(
        ws_a.candidate_sha(&checkout).expect("candidate sha via A"),
        committed,
        "the pinned checkout is unchanged"
    );

    drop(ws_a);
    drop(ws_b);
    let _ = server_a.stop();
    let _ = server_b.stop();
}

// ===========================================================================
// Case c — rollback without state loss
// ===========================================================================

#[test]
fn rollback_keeps_prior_checkout_and_server() {
    let dir = fresh();
    let origin = build_origin(&dir.join("origin"));
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let exec = ExecutionId::new("exec:wp-rollback:1");

    // Prior active binding: server A owns a prepared + committed checkout.
    let socket_a = dir.join("c-a.sock");
    let mut server_a = spawn_server(&socket_a, &state);
    let mut ws_a = server_a.client().expect("client a");
    let checkout = ws_a
        .prepare_repo(&origin, "main", &exec, &state)
        .expect("prepare prior");
    std::fs::write(Path::new(&checkout.path).join("added.txt"), "added\n").expect("dirty edit");
    let committed = ws_a.commit_candidate(&checkout).expect("commit prior");

    // Candidate binding: server B is staged then rolled back (stopped).
    let socket_b = dir.join("c-b.sock");
    let mut server_b = spawn_server(&socket_b, &state);
    let b_pid = server_b.pid();
    let evidence = server_b.stop();
    assert!(evidence.process_group_gone, "candidate stopped: {evidence:?}");
    assert!(!pid_alive(b_pid), "candidate server is gone after rollback");

    // No state lost: the prior server and its committed checkout survive, and
    // integration still lands the pinned candidate.
    assert!(pid_alive(server_a.pid()), "prior server survives the rollback");
    let receipt = ws_a
        .integrate_origin(&checkout, &origin, &checkout.base_sha)
        .expect("integrate prior");
    assert_eq!(receipt.result_sha, committed, "prior candidate is not lost");
    assert_eq!(
        git(&origin, &["rev-parse", "refs/heads/main"]),
        committed,
        "origin main is at the prior candidate"
    );

    drop(ws_a);
    let _ = server_a.stop();
}

// ===========================================================================
// Case d — IPC conformance for the workspace client
// ===========================================================================

#[test]
fn workspace_client_duplicate_id_replay_and_connection_loss() {
    let dir = fresh();
    let origin = build_origin(&dir.join("origin"));
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let exec = ExecutionId::new("exec:wp-conformance:1");

    let socket = dir.join("d.sock");
    let mut server = spawn_server(&socket, &state);
    let mut client = Client::connect(&socket).expect("connect");

    // Duplicate request id: the recorded `prepare_repo` receipt is replayed, not
    // re-run.
    let prepare_params = serde_json::to_value(PrepareRepoParams {
        origin_path: origin.to_string_lossy().into_owned(),
        base_ref: "main".to_string(),
        execution: exec.clone(),
        base_dir: state.to_string_lossy().into_owned(),
    })
    .expect("prepare params");
    let first = client
        .call_with_id(1, "prepare_repo", prepare_params.clone())
        .expect("prepare #1");
    let replay = client
        .call_with_id(1, "prepare_repo", prepare_params)
        .expect("prepare #2 (replay)");
    assert_eq!(first, replay, "same id replays the recorded checkout receipt");
    let checkout: Checkout = serde_json::from_value(first).expect("checkout");

    // Commit a candidate, then integrate it. The duplicate id replays; a fresh
    // id re-runs and conflicts (main moved) — proving the effect ran once.
    std::fs::write(Path::new(&checkout.path).join("added.txt"), "added\n").expect("dirty edit");
    let commit_params = serde_json::to_value(CheckoutCall {
        checkout: checkout.clone(),
    })
    .expect("commit params");
    client
        .call_with_id(2, "commit_candidate", commit_params)
        .expect("commit");

    let integrate_params = serde_json::to_value(IntegrateOriginParams {
        checkout: checkout.clone(),
        origin_path: origin.to_string_lossy().into_owned(),
        expected_main_sha: checkout.base_sha.clone(),
    })
    .expect("integrate params");
    let integrated = client
        .call_with_id(3, "integrate_origin", integrate_params.clone())
        .expect("integrate");
    let integrated_replay = client
        .call_with_id(3, "integrate_origin", integrate_params.clone())
        .expect("integrate (replay)");
    assert_eq!(integrated, integrated_replay, "same id replays the integration");
    let err = client
        .call_with_id(4, "integrate_origin", integrate_params)
        .expect_err("a fresh id must re-run and conflict");
    assert!(
        matches!(err, IpcError::Contract(ContractError::Conflict(_))),
        "re-run integration conflicts on the moved main: {err:?}"
    );

    // Connection loss: stop the server; the next call is not silently lost.
    let _ = server.stop();
    let err = client
        .call_with_id(5, "candidate_sha", serde_json::to_value(CheckoutCall { checkout }).unwrap())
        .expect_err("call after server death must fail");
    assert!(
        err.is_unavailable() || err.is_unknown_outcome(),
        "connection loss surfaces as Unavailable/UnknownOutcome: {err:?}"
    );

    // Connecting to a socket with no server is Unavailable.
    let missing = WorkspaceClient::connect(&dir.join("nope.sock"));
    assert!(
        matches!(missing, Err(IpcError::Unavailable(_))),
        "missing socket is Unavailable"
    );
}
