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
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::{
    ClaimEvidence, ContractError, ReadyTasks, SkipReason, SkippedIssue, Task, TaskFamily, TaskId,
    TaskPriority, TaskQueueApi,
};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::taskqueue_server::{register_taskqueue, serve_taskqueue};
use agalma_ipc::{IssueProjection, TaskQueueClient, TaskQueueOps, Wire};
use agalma_testkit::{retry_connect, ScratchDir};

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

fn spawn_taskqueue_server(socket: PathBuf, wire: Wire) {
    let mut server = Server::new(taskqueue_info());
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
    let dir = ScratchDir::new("ipcx-taskqueue");
    let framed_socket = dir.join("framed.sock");
    let connect_socket = dir.join("connect.sock");

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
