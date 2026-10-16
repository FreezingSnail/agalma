//! Task-queue boundary over the Connect transport (IPCX.5).
//!
//! The typed [`TaskQueueApi`]/[`TaskQueueOps`] client (`TaskQueueClient`) is
//! exercised against a `register_taskqueue` server served over both wires: the
//! B1 framed transport and the IPCX Connect/UDS transport. A deterministic stub
//! queue drives the full operation surface (`ready_tasks`/`get`/`claim`/
//! `comment`/`add_label`/`close_task` plus `projection`/`set_status`/
//! `remove_label`); the two wires must produce an identical typed transcript,
//! proving the task-queue boundary works over Connect with parity to framed.
//!
//! Servers bind under `target/test-runs/` — never `/tmp`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::{
    AdmissionChild, ClaimEvidence, ContractError, EpicReviewAdmission, PackageArtifactRef,
    ReadyTasks, ReviewAdmissionContext, ReviewAdmissionDecision, SkipReason, SkippedIssue, Task,
    TaskFamily, TaskId, TaskPriority, TaskQueueApi, EPIC_REVIEW_ADMISSION_FORMAT,
    REVIEW_ADMISSION_CAPABILITY,
};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::taskqueue_server::{register_taskqueue, serve_taskqueue};
use agalma_ipc::{IssueProjection, TaskQueueClient, TaskQueueOps, Wire};
use agalma_testkit::{retry_connect, ScratchConfig, ScratchDir};

/// A deterministic stub task queue: no bd, no clock; operations mutate in-memory
/// state so the projection reflects them.
struct StubQueue {
    id: TaskId,
    status: String,
    labels: Vec<String>,
    comments: Vec<String>,
}

impl StubQueue {
    fn new() -> StubQueue {
        StubQueue {
            id: TaskId::new("tq-1"),
            status: "open".to_string(),
            labels: Vec::new(),
            comments: Vec::new(),
        }
    }

    fn task(&self) -> Task {
        Task {
            task_id: self.id.clone(),
            title: "Stub task".to_string(),
            directive: "directive/v0".to_string(),
            target: "agalma".to_string(),
            base_ref: "main".to_string(),
            acceptance: vec!["cargo test".to_string()],
            budget_usd: Some(2.0),
            priority: TaskPriority::Normal,
            family: TaskFamily::Fix,
            max_attempts: 2,
            raw_block: "directive: directive/v0".to_string(),
        }
    }
}

impl TaskQueueApi for StubQueue {
    fn ready_tasks(&self) -> Result<ReadyTasks, ContractError> {
        Ok(ReadyTasks {
            tasks: vec![self.task()],
            skipped: vec![SkippedIssue {
                task_id: TaskId::new("tq-2"),
                reason: SkipReason::MissingBlock,
            }],
        })
    }

    fn get(&self, id: &TaskId) -> Result<Task, ContractError> {
        if *id == self.id {
            Ok(self.task())
        } else {
            Err(ContractError::KnownFailure(format!("no task {id}")))
        }
    }

    fn claim(&mut self, id: &TaskId) -> Result<ClaimEvidence, ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        self.status = "in_progress".to_string();
        Ok(ClaimEvidence {
            task_id: id.clone(),
            status: self.status.clone(),
            assignee: Some("stub".to_string()),
            started_at: None,
        })
    }

    fn comment(&mut self, id: &TaskId, text: &str) -> Result<(), ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        self.comments.push(text.to_string());
        Ok(())
    }

    fn close_task(&mut self, id: &TaskId, reason: &str) -> Result<(), ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        self.status = "closed".to_string();
        self.comments.push(format!("closed: {reason}"));
        Ok(())
    }

    fn add_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        self.labels.push(label.to_string());
        Ok(())
    }

    fn review_admission(
        &self,
        epic_id: &str,
        context: &ReviewAdmissionContext,
    ) -> Result<EpicReviewAdmission, ContractError> {
        let mut record = EpicReviewAdmission {
            format: EPIC_REVIEW_ADMISSION_FORMAT.to_string(),
            epic_id: epic_id.to_string(),
            definition_digest: format!("sha256:{}", "0".repeat(64)),
            design: context.design.clone(),
            base_sha: context.base_sha.clone(),
            candidate_sha: context.candidate_sha.clone(),
            children: vec![AdmissionChild {
                id: "tq-1".to_string(),
                definition_digest: format!("sha256:{}", "1".repeat(64)),
                status: "closed".to_string(),
                labels: vec!["staged".to_string()],
            }],
            edges: Vec::new(),
            decision: ReviewAdmissionDecision::Eligible,
            blocking_ids: Vec::new(),
            observed_at_unix_ms: 1,
            input_digest: String::new(),
        };
        record.input_digest = record.compute_input_digest()?;
        Ok(record)
    }
}

impl TaskQueueOps for StubQueue {
    fn projection(&self, id: &TaskId) -> Result<IssueProjection, ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        Ok(IssueProjection {
            status: self.status.clone(),
            assignee: Some("stub".to_string()),
            labels: self.labels.clone(),
            comments: self.comments.clone(),
        })
    }

    fn set_status(&mut self, id: &TaskId, status: &str) -> Result<(), ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        self.status = status.to_string();
        Ok(())
    }

    fn remove_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        if *id != self.id {
            return Err(ContractError::KnownFailure(format!("no task {id}")));
        }
        self.labels.retain(|l| l != label);
        Ok(())
    }
}

fn taskqueue_info() -> ServerInfo {
    ServerInfo {
        api: "TaskQueueApi".to_string(),
        api_version: 0,
        impl_name: "stub".to_string(),
        impl_version: "stub/v1".to_string(),
        capabilities: BTreeSet::new(),
    }
}

/// A capable server identity advertising `review-admission/v1`.
fn capable_taskqueue_info() -> ServerInfo {
    let mut info = taskqueue_info();
    info.capabilities
        .insert(REVIEW_ADMISSION_CAPABILITY.to_string());
    info
}

fn spawn_taskqueue_server(socket: PathBuf, wire: Wire) {
    spawn_taskqueue_server_info(socket, wire, taskqueue_info());
}

fn spawn_taskqueue_server_info(socket: PathBuf, wire: Wire, info: ServerInfo) {
    let mut server = Server::new(info);
    register_taskqueue(&mut server, StubQueue::new());
    std::thread::spawn(move || {
        let _ = serve_taskqueue(server, &socket, wire);
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

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// A short repository-local socket root whose physical bytes are retained under
/// the configured (persistent) run root.
///
/// macOS `SUN_LEN` (~104 bytes) overflows for the long worktree/bead paths, so —
/// mirroring the established testkit short-alias pattern — a short symlink under
/// the shared `target/test-runs` points at the long retained root, and sockets
/// bind through the short alias. Evidence still lands physically under the
/// persistent root.
fn short_socket_root(tag: &str) -> PathBuf {
    use std::os::unix::fs::symlink;
    let physical_root = ScratchConfig::from_env()
        .expect("scratch config")
        .root()
        .to_path_buf();
    let physical = physical_root.join(format!("{tag}-{}-{}", std::process::id(), now_nanos()));
    fs::create_dir_all(&physical).expect("create physical socket root");

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let worktree = manifest
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/");
    let is_worktree = worktree
        .parent()
        .and_then(Path::file_name)
        .map(|n| n == "worktrees")
        .unwrap_or(false);
    let alias_base = if is_worktree {
        worktree
            .parent()
            .and_then(Path::parent)
            .expect("shared cargo target")
            .join("test-runs")
    } else {
        physical_root
    };
    fs::create_dir_all(&alias_base).expect("create alias base");

    let pid = std::process::id();
    for attempt in 0..64u32 {
        let candidate = alias_base.join(format!("tq{pid}-{attempt}"));
        match symlink(&physical, &candidate) {
            Ok(()) => return candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("create socket alias {}: {e}", candidate.display()),
        }
    }
    panic!(
        "socket alias allocation exhausted under {}",
        alias_base.display()
    );
}

/// Drive the stub through every operation and render a timing/ID-free
/// transcript.
fn run_stub_scenario(mut client: TaskQueueClient) -> Vec<String> {
    let id = TaskId::new("tq-1");
    let ready = client.ready_tasks().expect("ready_tasks");
    let task = client.get(&id).expect("get");
    let claim = client.claim(&id).expect("claim");
    client
        .comment(&id, "phase=build execution=exec:tq:1 lease=1")
        .expect("comment");
    client.add_label(&id, "parked").expect("add_label");
    client.set_status(&id, "blocked").expect("set_status");
    client.remove_label(&id, "parked").expect("remove_label");
    client.close_task(&id, "done: green").expect("close_task");
    let projection = client.projection(&id).expect("projection");
    vec![
        format!("ready={ready:?}"),
        format!("task={task:?}"),
        format!("claim={claim:?}"),
        format!("projection={projection:?}"),
    ]
}

#[test]
fn taskqueue_api_over_connect_matches_framed() {
    let root = short_socket_root("ipcx-taskqueue");
    let framed_socket = root.join("framed.sock");
    let connect_socket = root.join("connect.sock");

    spawn_taskqueue_server(framed_socket.clone(), Wire::Framed);
    spawn_taskqueue_server(connect_socket.clone(), Wire::Connect);
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let framed = retry_connect(|| TaskQueueClient::connect_wire(&framed_socket, Wire::Framed))
        .expect("framed connect");
    let connect = retry_connect(|| TaskQueueClient::connect_wire(&connect_socket, Wire::Connect))
        .expect("connect connect");

    // Every typed call produces the same transcript on both wires.
    assert_eq!(run_stub_scenario(framed), run_stub_scenario(connect));
}

#[test]
fn taskqueue_connect_missing_socket_is_unavailable() {
    let dir = ScratchDir::new("ipcx-taskqueue-miss");
    let err = TaskQueueClient::connect_wire(&dir.join("nope.sock"), Wire::Connect)
        .err()
        .expect("connect to a missing socket must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}

fn review_admission_context() -> ReviewAdmissionContext {
    ReviewAdmissionContext {
        design: PackageArtifactRef::new("bd:tq-e:design", b"stub design", "text/plain"),
        base_sha: "a".repeat(40),
        candidate_sha: "b".repeat(40),
    }
}

#[test]
fn review_admission_roundtrips_over_both_wires_with_semantic_parity() {
    let root = short_socket_root("ipcx-taskqueue-admission");
    let framed_socket = root.join("framed.sock");
    let connect_socket = root.join("connect.sock");

    let caps = capable_taskqueue_info().capabilities;
    spawn_taskqueue_server_info(
        framed_socket.clone(),
        Wire::Framed,
        capable_taskqueue_info(),
    );
    spawn_taskqueue_server_info(
        connect_socket.clone(),
        Wire::Connect,
        capable_taskqueue_info(),
    );
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let framed = retry_connect(|| {
        TaskQueueClient::connect_wire_with_capabilities(&framed_socket, Wire::Framed, caps.clone())
    })
    .expect("framed connect");
    let connect = retry_connect(|| {
        TaskQueueClient::connect_wire_with_capabilities(
            &connect_socket,
            Wire::Connect,
            caps.clone(),
        )
    })
    .expect("connect connect");

    let context = review_admission_context();
    let framed_record = framed.review_admission("tq-e", &context).expect("framed");
    let connect_record = connect.review_admission("tq-e", &context).expect("connect");
    // In-process reference: the same stub called directly.
    let local_record = StubQueue::new()
        .review_admission("tq-e", &context)
        .expect("in-process");

    assert_eq!(framed_record, connect_record, "wire parity");
    assert_eq!(
        framed_record, local_record,
        "semantic parity with in-process"
    );
    framed_record.verify_integrity().expect("integrity");
}

#[test]
fn review_admission_denied_on_an_unsupported_peer() {
    let root = short_socket_root("ipcx-taskqueue-admission-old");
    let socket = root.join("old.sock");
    // An old/incapable server: no `review-admission/v1` advertised.
    spawn_taskqueue_server_info(socket.clone(), Wire::Framed, taskqueue_info());
    wait_for_socket(&socket);

    let client = retry_connect(|| {
        TaskQueueClient::connect_wire_with_capabilities(&socket, Wire::Framed, BTreeSet::new())
    })
    .expect("connect");

    let err = client
        .review_admission("tq-e", &review_admission_context())
        .expect_err("an unsupported peer must deny review admission");
    assert!(
        matches!(err, ContractError::UnsupportedCapability(_)),
        "expected UnsupportedCapability, got {err:?}"
    );
}
