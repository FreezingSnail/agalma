//! B1.5 task-queue process-boundary evidence (`agalma-0ht.6`).
//!
//! Dual-binding evidence for the extracted `TaskQueueApi` (+ the reconcile
//! projection operations the conductor drives): shadow-parity runs of the same
//! bd scenario in-process vs through the supervised task-queue component server
//! (comparing projected state, never timestamps); a staged rebind that keeps an
//! in-flight claim pinned to its claiming server across a server swap; a
//! rollback without state loss; and IPC conformance for
//! [`agalma_ipc::TaskQueueClient`] (duplicate-id replay, a fresh claim after the
//! issue was closed is refused, dead-server `Unavailable`/`UnknownOutcome`).
//!
//! Scratch bd stores live under `target/test-runs/tq-*` (never `/tmp`). The
//! server child is re-exec'd from this test binary (or the real
//! `agalma-taskqueue-server` binary when built) so a package-scoped
//! `cargo test -p agalma-conductor` run still exercises the real process
//! boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agalma_contracts::{ContractError, TaskId, TaskQueueApi};
use agalma_ipc::{Client, CloseParams, CommandSpec, IpcError, TaskCall, TaskQueueClient, TaskQueueOps};
use agalma_taskqueue::server::SERVER_BIN;

use agalma_conductor::harness_process::resolve_server_binary;
use agalma_conductor::taskqueue_process::{taskqueue_server_spec, TaskQueueHandle, TaskQueueProcess};

const CHILD_SOCKET_ENV: &str = "AGALMA_TEST_TASKQUEUE_SOCKET";
const CHILD_REPO_ENV: &str = "AGALMA_TEST_TASKQUEUE_REPO";
const CHILD_TEST_NAME: &str = "taskqueue_server_child";
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// A fully valid fenced task block (mirrors `agalma-taskqueue`'s fixture).
const VALID: &str = "Intro.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\n  - cargo test\nbudget_usd: 2.00\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n";

// ===========================================================================
// Fixtures + helpers (mirrors tests/workspace_process.rs)
// ===========================================================================

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

/// Short, unique run directory. Unix socket paths must fit `SUN_LEN` (~104
/// bytes), so sockets live directly under a short `tq-<pid>-<n>` directory.
static RUN_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fresh() -> PathBuf {
    let n = RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("tq-{}-{n}", std::process::id()));
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

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// An isolated bd workspace under `target/test-runs/`.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Initialize `dir` as a git repo with a local `.beads` store.
    fn new(dir: PathBuf) -> Self {
        require_bd();
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        run_ok("git", &dir, &["init", "-q"]);
        run_ok("git", &dir, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &dir, &["config", "user.name", "Agalma"]);

        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                "tq",
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        Scratch { dir }
    }

    fn repo(&self) -> &Path {
        &self.dir
    }

    fn beads_dir(&self) -> PathBuf {
        self.dir.join(".beads")
    }

    /// Create a task issue with `description`; return its id.
    fn create(&self, title: &str, description: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "create",
                title,
                "-t",
                "task",
                "-p",
                "2",
                "--description",
                description,
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd create");
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("created id").to_string()
    }

    /// `bd dep add <blocked> <blocker>`: `blocker` must close before `blocked`
    /// becomes ready.
    fn dep_add(&self, blocked: &str, blocker: &str) {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args(["dep", "add", blocked, blocker])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd dep add");
        assert!(out.status.success(), "bd dep add: {}", stderr_of(&out));
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
fn server_spec(socket: &Path, repo: &Path) -> (CommandSpec, PathBuf) {
    match resolve_server_binary(SERVER_BIN) {
        Ok(bin) => (taskqueue_server_spec(socket, repo), bin),
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
                .env(CHILD_REPO_ENV, repo.to_string_lossy().into_owned());
            (spec, exe)
        }
    }
}

fn spawn_server(socket: &Path, repo: &Path) -> TaskQueueProcess {
    let (spec, program) = server_spec(socket, repo);
    TaskQueueProcess::spawn_with_spec(spec, program, socket.to_path_buf(), READY_TIMEOUT)
        .unwrap_or_else(|e| panic!("spawn taskqueue server: {e}"))
}

/// The child entry point: when spawned with [`CHILD_SOCKET_ENV`], serve the bd
/// backlog at [`CHILD_REPO_ENV`] on the named socket. A no-op under a normal
/// test run.
#[test]
fn taskqueue_server_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let repo = std::env::var(CHILD_REPO_ENV).expect("child repo");
    let _ = agalma_taskqueue::server::serve(Path::new(&socket), Path::new(&repo));
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Sorted task ids admitted by a binding's `ready_tasks`.
fn sorted_ready_ids(queue: &dyn TaskQueueApi) -> Vec<String> {
    let mut ids: Vec<String> = queue
        .ready_tasks()
        .expect("ready_tasks")
        .tasks
        .iter()
        .map(|t| t.task_id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

// ===========================================================================
// Case a — shadow parity: in-process vs process binding
// ===========================================================================

/// Normalized, timing/id-free transcript of the bd scenario. Everything is
/// keyed by issue title (ids carry a per-store suffix; timestamps are never
/// read).
#[derive(Debug, PartialEq, Eq)]
struct Transcript {
    ready_before: Vec<String>,
    skipped_titles: Vec<String>,
    ready_after: Vec<String>,
    /// (title, status, sorted labels, comments) for every projected issue.
    projections: Vec<(String, String, Vec<String>, Vec<String>)>,
}

/// Drive `ready_tasks`/`get`/`claim`/`comment`/`close_task`/`add_label` plus the
/// projection operation against a scratch backlog.
fn run_scenario(queue: &mut dyn TaskQueueOps, scratch: &Scratch) -> Transcript {
    let mut titles: HashMap<String, String> = HashMap::new();
    let ready = scratch.create("Ready", VALID);
    let claimed = scratch.create("Claimed", VALID);
    let closed = scratch.create("Closed", VALID);
    let parked = scratch.create("Parked", VALID);
    let nontask = scratch.create("Not a task", "no fenced block here");
    for (title, id) in [
        ("Ready", &ready),
        ("Claimed", &claimed),
        ("Closed", &closed),
        ("Parked", &parked),
        ("Not a task", &nontask),
    ] {
        titles.insert(id.clone(), title.to_string());
    }

    let before = queue.ready_tasks().expect("ready before");
    // Sort: bd's list order is not part of the contract and can depend on the
    // per-store id suffix; parity compares the set, not the listing order.
    let mut ready_before: Vec<String> = before.tasks.iter().map(|t| t.title.clone()).collect();
    ready_before.sort();
    let mut skipped_titles: Vec<String> = before
        .skipped
        .iter()
        .map(|s| titles.get(s.task_id.as_str()).cloned().unwrap_or_default())
        .collect();
    skipped_titles.sort();

    // `get` reads one parsed task by id.
    let task = queue.get(&TaskId::new(&ready)).expect("get ready");
    assert_eq!(task.title, "Ready");

    queue.claim(&TaskId::new(&claimed)).expect("claim");
    queue
        .comment(&TaskId::new(&ready), "phase=build execution=exec:tq:1 lease=1")
        .expect("comment");
    queue
        .add_label(&TaskId::new(&parked), "parked")
        .expect("add_label");
    queue
        .close_task(&TaskId::new(&closed), "done: green")
        .expect("close_task");

    let after = queue.ready_tasks().expect("ready after");
    let mut ready_after: Vec<String> = after.tasks.iter().map(|t| t.title.clone()).collect();
    ready_after.sort();

    let mut projections = Vec::new();
    for (title, id) in [
        ("Ready", &ready),
        ("Claimed", &claimed),
        ("Closed", &closed),
        ("Parked", &parked),
        ("Not a task", &nontask),
    ] {
        let projection = queue
            .projection(&TaskId::new(id))
            .expect("projection");
        let mut labels = projection.labels.clone();
        labels.sort();
        let mut comments = projection.comments.clone();
        comments.sort();
        projections.push((title.to_string(), projection.status.clone(), labels, comments));
    }

    Transcript {
        ready_before,
        skipped_titles,
        ready_after,
        projections,
    }
}

#[test]
fn shadow_parity_in_process_vs_process() {
    let dir_in = fresh();
    let scratch_in = Scratch::new(dir_in.clone());
    let mut inproc = TaskQueueHandle::in_process(scratch_in.repo());
    let in_process = run_scenario(&mut inproc, &scratch_in);

    let dir_proc = fresh();
    let scratch_proc = Scratch::new(dir_proc.clone());
    let socket = dir_proc.join("a.sock");
    let mut process = spawn_server(&socket, scratch_proc.repo());
    let mut client = process.client().expect("connect taskqueue client");
    let via_process = run_scenario(&mut client, &scratch_proc);

    assert_eq!(in_process, via_process, "taskqueue transcript parity");

    // Spot-check the scenario itself.
    assert_eq!(
        in_process.ready_after,
        vec!["Ready".to_string()],
        "only the untouched open task is still ready"
    );
    assert_eq!(in_process.skipped_titles, vec!["Not a task".to_string()]);
    let by_title: HashMap<&str, &(String, String, Vec<String>, Vec<String>)> = in_process
        .projections
        .iter()
        .map(|p| (p.0.as_str(), p))
        .collect();
    assert_eq!(by_title["Claimed"].1, "in_progress");
    assert_eq!(by_title["Closed"].1, "closed");
    assert!(by_title["Parked"].2.iter().any(|l| l == "parked"));
    assert!(by_title["Ready"]
        .3
        .iter()
        .any(|c| c == "phase=build execution=exec:tq:1 lease=1"));

    drop(client);
    let evidence = process.stop();
    assert!(evidence.process_group_gone, "server stop evidence: {evidence:?}");
}

/// Readiness parity across the process boundary with a blocking dependency: a
/// blocked child is withheld until its prerequisite closes, and the in-process
/// and process bindings agree.
#[test]
fn process_binding_withholds_blocked_children() {
    let dir = fresh();
    let scratch = Scratch::new(dir.clone());
    let prereq = scratch.create("Prerequisite", VALID);
    let child = scratch.create("Blocked child", VALID);
    let independent = scratch.create("Independent", VALID);
    scratch.dep_add(&child, &prereq);

    let socket = dir.join("np4.sock");
    let mut server = spawn_server(&socket, scratch.repo());
    let mut client = server.client().expect("connect taskqueue client");

    let process_ids = sorted_ready_ids(&client);
    assert!(process_ids.contains(&prereq), "prerequisite ready: {process_ids:?}");
    assert!(
        process_ids.contains(&independent),
        "independent work ready: {process_ids:?}"
    );
    assert!(
        !process_ids.contains(&child),
        "process binding admitted a blocked child: {process_ids:?}"
    );

    // In-process binding agrees (parity across the boundary).
    let inproc = TaskQueueHandle::in_process(scratch.repo());
    assert_eq!(
        process_ids,
        sorted_ready_ids(&inproc),
        "in-process vs process readiness parity"
    );

    // Closing the prerequisite releases the child through the boundary too.
    client
        .close_task(&TaskId::new(prereq.as_str()), "done in test")
        .expect("close prerequisite");
    let process_ids = sorted_ready_ids(&client);
    assert!(
        process_ids.contains(&child),
        "child ready after prerequisite closes: {process_ids:?}"
    );
    assert!(!process_ids.contains(&prereq), "closed prerequisite gone: {process_ids:?}");

    drop(client);
    let _ = server.stop();
}

// ===========================================================================
// Case b — staged rebind with an in-flight claim pinned
// ===========================================================================

#[test]
fn staged_rebind_keeps_pinned_claim() {
    let dir = fresh();
    let scratch = Scratch::new(dir.clone());
    let claimed = scratch.create("Pinned", VALID);
    let task = TaskId::new(&claimed);

    // "Generation 1": claim on server A. The claim is the durable in-flight
    // artifact pinned to A.
    let socket_a = dir.join("b-a.sock");
    let mut server_a = spawn_server(&socket_a, scratch.repo());
    let mut a = server_a.client().expect("client a");
    let evidence = a.claim(&task).expect("claim in-flight");
    let a_pid = server_a.pid();

    // Stage the candidate ("generation 2") and continue the next operation on it.
    let socket_b = dir.join("b-b.sock");
    let mut server_b = spawn_server(&socket_b, scratch.repo());
    let mut b = server_b.client().expect("client b");

    // The next operation runs on B; the pinned claim is honored, not re-run.
    b.comment(&task, "phase=build execution=exec:tq:pin lease=1")
        .expect("comment via candidate server");
    let projection_b = b.projection(&task).expect("projection via B");
    assert_eq!(projection_b.status, "in_progress");
    assert_eq!(projection_b.assignee, evidence.assignee);
    assert!(projection_b.has_comment("phase=build"));

    // A is still alive and reports the same pinned claim.
    assert!(pid_alive(a_pid), "server A stays alive across the rebind");
    let projection_a = a.projection(&task).expect("projection via A");
    assert_eq!(projection_a.status, "in_progress");
    assert_eq!(projection_a.assignee, evidence.assignee);

    drop(a);
    drop(b);
    let _ = server_a.stop();
    let _ = server_b.stop();
}

// ===========================================================================
// Case c — rollback without state loss
// ===========================================================================

#[test]
fn rollback_keeps_prior_claim_and_server() {
    let dir = fresh();
    let scratch = Scratch::new(dir.clone());
    let id = scratch.create("Prior", VALID);
    let task = TaskId::new(&id);

    // Prior active binding: server A owns a persisted claim and comment.
    let socket_a = dir.join("c-a.sock");
    let mut server_a = spawn_server(&socket_a, scratch.repo());
    let mut a = server_a.client().expect("client a");
    let evidence = a.claim(&task).expect("claim prior");
    a.comment(&task, "phase=build execution=exec:tq:rb lease=1")
        .expect("comment prior");

    // Candidate binding: server B is staged then rolled back (stopped).
    let socket_b = dir.join("c-b.sock");
    let mut server_b = spawn_server(&socket_b, scratch.repo());
    let b_pid = server_b.pid();
    let stopped = server_b.stop();
    assert!(stopped.process_group_gone, "candidate stopped: {stopped:?}");
    assert!(!pid_alive(b_pid), "candidate server is gone after rollback");

    // No state lost: the prior server and its claim survive.
    assert!(pid_alive(server_a.pid()), "prior server survives the rollback");
    let projection = a.projection(&task).expect("projection prior");
    assert_eq!(projection.status, "in_progress");
    assert_eq!(projection.assignee, evidence.assignee);
    assert!(projection.has_comment("phase=build"));
    let ready = a.ready_tasks().expect("ready");
    assert!(
        !ready.tasks.iter().any(|t| t.task_id.as_str() == id),
        "the claimed task is not re-offered"
    );

    drop(a);
    let _ = server_a.stop();
}

// ===========================================================================
// Case d — IPC conformance for the taskqueue client
// ===========================================================================

#[test]
fn taskqueue_client_duplicate_id_replay_and_connection_loss() {
    let dir = fresh();
    let scratch = Scratch::new(dir.clone());
    let id = scratch.create("Claimable", VALID);
    let task = TaskId::new(&id);

    let socket = dir.join("d.sock");
    let mut server = spawn_server(&socket, scratch.repo());
    let mut client = Client::connect(&socket).expect("connect");

    // Duplicate request id: the recorded `claim` receipt is replayed, not re-run.
    let claim_params = serde_json::to_value(TaskCall { id: task.clone() }).expect("claim params");
    let first = client
        .call_with_id(1, "claim", claim_params.clone())
        .expect("claim #1");
    let replay = client
        .call_with_id(1, "claim", claim_params.clone())
        .expect("claim #2 (replay)");
    assert_eq!(first, replay, "same id replays the recorded claim evidence");

    // Close the issue, then replay the SAME id: the recorded receipt is still
    // returned even though a real re-run would now be refused ("not claimable:
    // status closed") — proving the effect ran exactly once.
    let close_params =
        serde_json::to_value(CloseParams { id: task.clone(), reason: "done".to_string() })
            .expect("close params");
    client
        .call_with_id(2, "close_task", close_params)
        .expect("close");
    let replay_after_close = client
        .call_with_id(1, "claim", claim_params.clone())
        .expect("claim replay after close");
    assert_eq!(
        replay_after_close, first,
        "the duplicate id returns the recorded receipt, never a re-run"
    );

    // A fresh id re-runs and is refused because the issue is now closed: the
    // claim cannot be double-claimed.
    let err = client
        .call_with_id(3, "claim", claim_params.clone())
        .expect_err("a fresh claim on a closed issue must fail");
    assert!(
        matches!(err, IpcError::Contract(ContractError::KnownFailure(_))),
        "fresh claim is refused, not silently re-applied: {err:?}"
    );

    // Connection loss: stop the server; the next call is not silently lost.
    let _ = server.stop();
    let projection_params =
        serde_json::to_value(TaskCall { id: task }).expect("projection params");
    let err = client
        .call_with_id(4, "projection", projection_params)
        .expect_err("call after server death must fail");
    assert!(
        err.is_unavailable() || err.is_unknown_outcome(),
        "connection loss surfaces as Unavailable/UnknownOutcome: {err:?}"
    );

    // Connecting to a socket with no server is Unavailable.
    let missing = TaskQueueClient::connect(&dir.join("nope.sock"));
    assert!(
        matches!(missing, Err(IpcError::Unavailable(_))),
        "missing socket is Unavailable"
    );
}
