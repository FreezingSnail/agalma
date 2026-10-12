//! Harness boundary over the Connect transport (IPCX.2).
//!
//! The typed [`HarnessApi`] client (`HarnessClient`) is exercised against a
//! `register_harness` server served over both wires: the B1 framed transport and
//! the IPCX Connect/UDS transport. A deterministic stub harness drives a full
//! scenario; the two wires must produce an identical typed transcript (identity,
//! handles, terminal state, event rendering, cancel/stop evidence), proving the
//! harness boundary works over Connect with parity to framed.
//!
//! Servers bind under `target/test-runs/` — never `/tmp`.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use agalma_contracts::harness::{
    AttemptHandle, CancelAck, Completeness, CreateSessionRequest, Describe, Event, HarnessApi,
    Limits, OperationHandle, OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest,
    StopEvidence, Usage, API_VERSION, HARNESS_API,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId, OperationId, SessionId};
use agalma_contracts::ContractError;
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::{register_harness, serve_harness, server_info_from, HarnessClient, Wire};
use agalma_testkit::{retry_connect, ScratchDir};

/// A deterministic stub harness: fixed handles/state, no clock or vendor.
struct StubHarness;

impl HarnessApi for StubHarness {
    fn describe(&self) -> Describe {
        Describe {
            api: HARNESS_API.to_string(),
            api_version: API_VERSION,
            impl_name: "stub".to_string(),
            impl_version: "stub/v1".to_string(),
            capabilities: BTreeSet::new(),
        }
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        Ok(AttemptHandle {
            id: AttemptId::new(format!("attempt:{}", req.role)),
        })
    }

    fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        Ok(SessionHandle {
            id: SessionId::new(format!("session:{}/{}", req.role, req.model)),
        })
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        Ok(OperationHandle {
            id: OperationId::new(format!("op:{}:{}", session.id.as_str(), req.bounded_turns)),
        })
    }

    fn inspect_operation(
        &mut self,
        _op: &OperationHandle,
    ) -> Result<OperationState, ContractError> {
        Ok(OperationState::Completed {
            result_ref: ArtifactRef::derive("stub-result"),
            usage: Usage {
                tokens_in: 3,
                tokens_out: 5,
                cost_usd: 0.0,
                completeness: Completeness::Complete,
            },
        })
    }

    fn read_events(&mut self, _op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        Ok(vec![
            Event::TurnStarted,
            Event::TurnCompleted {
                result_ref: ArtifactRef::derive("stub-result"),
            },
        ])
    }

    fn cancel_operation(&mut self, _op: &OperationHandle) -> Result<CancelAck, ContractError> {
        Ok(CancelAck {
            acknowledged: true,
            terminated: true,
            detail: "stub cancel".to_string(),
        })
    }

    fn close_session(&mut self, _session: &SessionHandle) -> Result<(), ContractError> {
        Ok(())
    }

    fn stop_attempt(&mut self, _attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: true,
            detail: "stub stop".to_string(),
        })
    }
}

fn spawn_harness_server(socket: std::path::PathBuf, wire: Wire) {
    let harness = StubHarness;
    let mut server = Server::new(server_info_from(&harness.describe()));
    register_harness(&mut server, harness);
    std::thread::spawn(move || {
        let _ = serve_harness(server, &socket, wire);
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

fn start_req() -> StartAttemptRequest {
    StartAttemptRequest {
        workspace: "/ws".to_string(),
        role: "builder".to_string(),
        genome_ref: ArtifactRef::derive("genome").to_string(),
        constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
        limits: Limits {
            wall_ms: 1_000,
            tokens: 0,
            cost_micros: 0,
        },
        operation_id: OperationId::new("op:stub:build/1"),
    }
}

fn create_req() -> CreateSessionRequest {
    CreateSessionRequest {
        role: "builder".to_string(),
        model: "stub/model".to_string(),
        prompts_ref: "artifact:builder-prompts".to_string(),
        handoff_refs: Vec::new(),
        tool_policy_ref: "artifact:tool-policy".to_string(),
    }
}

fn turn_req() -> RunTurnRequest {
    RunTurnRequest {
        input_ref: ArtifactRef::derive("builder-turn"),
        bounded_turns: 1,
        deadline_ms: 1_000,
    }
}

/// Drive the stub through every [`HarnessApi`] method and render a
/// timing/ID-free transcript.
fn run_stub_scenario(mut client: HarnessClient) -> Vec<String> {
    let attempt = client.start_attempt(start_req()).expect("start_attempt");
    let session = client.create_session(create_req()).expect("create_session");
    let op = client.run_turn(&session, turn_req()).expect("run_turn");
    let state = client.inspect_operation(&op).expect("inspect_operation");
    let events = client.read_events(&op).expect("read_events");
    let ack = client.cancel_operation(&op).expect("cancel_operation");
    client.close_session(&session).expect("close_session");
    let stop = client.stop_attempt(&attempt).expect("stop_attempt");

    vec![
        format!("describe={}", client.cached_describe().impl_version),
        format!("attempt={}", attempt.id),
        format!("session={}", session.id),
        format!("operation={}", op.id),
        format!("state={}", state.label()),
        format!(
            "events={:?}",
            events.iter().map(Event::render).collect::<Vec<_>>()
        ),
        format!("ack={ack:?}"),
        format!("stop={stop:?}"),
    ]
}

#[test]
fn harness_api_over_connect_matches_framed() {
    let dir = ScratchDir::new("ipcx-harness");
    let framed_socket = dir.join("framed.sock");
    let connect_socket = dir.join("connect.sock");

    spawn_harness_server(framed_socket.clone(), Wire::Framed);
    spawn_harness_server(connect_socket.clone(), Wire::Connect);
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let framed = retry_connect(|| HarnessClient::connect_wire(&framed_socket, Wire::Framed))
        .expect("framed connect");
    let connect = retry_connect(|| HarnessClient::connect_wire(&connect_socket, Wire::Connect))
        .expect("connect connect");

    // Cached identity is fetched over each wire at connect time.
    assert_eq!(framed.cached_describe(), connect.cached_describe());
    assert_eq!(framed.cached_describe().impl_name, "stub");

    // Every typed call produces the same transcript on both wires.
    assert_eq!(run_stub_scenario(framed), run_stub_scenario(connect));
}

#[test]
fn harness_connect_live_describe_round_trips() {
    let dir = ScratchDir::new("ipcx-harness-live");
    let socket = dir.join("live.sock");
    spawn_harness_server(socket.clone(), Wire::Connect);
    wait_for_socket(&socket);

    let client =
        retry_connect(|| HarnessClient::connect_wire(&socket, Wire::Connect)).expect("connect");
    let live = client.describe_live().expect("live describe over Connect");
    assert_eq!(live, *client.cached_describe());
}

#[test]
fn harness_connect_missing_socket_is_unavailable() {
    let dir = ScratchDir::new("ipcx-harness-miss");
    let err = HarnessClient::connect_wire(&dir.join("nope.sock"), Wire::Connect)
        .err()
        .expect("connect to a missing socket must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}

#[test]
fn wire_selector_parses_and_defaults() {
    assert_eq!(Wire::parse("framed"), Some(Wire::Framed));
    assert_eq!(Wire::parse("connect"), Some(Wire::Connect));
    assert_eq!(Wire::parse("nope"), None);
    assert_eq!(Wire::Framed.as_str(), "framed");
    assert_eq!(Wire::Connect.as_str(), "connect");
}
