//! Sandbox boundary over the Connect transport (IPCX.3).
//!
//! The typed [`SandboxApi`] client (`SandboxClient`) is exercised against a
//! `register_sandbox` server served over both wires: the B1 framed transport and
//! the IPCX Connect/UDS transport. A deterministic stub sandbox drives a full
//! `launch`/`alive`/`terminate` scenario; the two wires must produce an
//! identical typed transcript, proving the sandbox boundary works over Connect
//! with parity to framed.
//!
//! Servers bind under `target/test-runs/` — never `/tmp`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::ids::AttemptId;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};
use agalma_contracts::ContractError;
use agalma_ipc::sandbox_server::{register_sandbox, serve_sandbox};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::{SandboxClient, Wire};
use agalma_testkit::{retry_connect, ScratchDir};

/// A deterministic stub sandbox: fixed pid sequence, no clock or real process.
struct StubSandbox {
    launched: u32,
}

impl SandboxApi for StubSandbox {
    fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
        self.launched += 1;
        let pid = 4_000 + self.launched;
        Ok(SandboxChild {
            pid,
            pgid: pid,
            attempt: spec.attempt,
            started_at_unix_ms: 1_700_000_000_000,
        })
    }

    fn alive(&self, child: &SandboxChild) -> Result<bool, ContractError> {
        Ok(child.pid != 0)
    }

    fn terminate(&mut self, child: &SandboxChild) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: true,
            detail: format!("stub terminated pgid {}", child.pgid),
        })
    }
}

fn sandbox_info() -> ServerInfo {
    ServerInfo {
        api: "SandboxApi".to_string(),
        api_version: 0,
        impl_name: "stub".to_string(),
        impl_version: "stub/v1".to_string(),
        capabilities: BTreeSet::new(),
    }
}

fn spawn_sandbox_server(socket: PathBuf, wire: Wire) {
    let mut server = Server::new(sandbox_info());
    register_sandbox(&mut server, StubSandbox { launched: 0 });
    std::thread::spawn(move || {
        let _ = serve_sandbox(server, &socket, wire);
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

fn spec() -> LaunchSpec {
    LaunchSpec {
        program: "/bin/echo".to_string(),
        args: vec!["ok".to_string()],
        cwd: "/ws".to_string(),
        env: BTreeMap::new(),
        profile: "/ws/worker.sb".to_string(),
        attempt: AttemptId::derive(1),
        attempt_dir: "/ws/attempt".to_string(),
        protected_dir: "/ws/protected".to_string(),
        sock_dir: "/ws/sock".to_string(),
        extra_ro_roots: Vec::new(),
        extra_rw_roots: Vec::new(),
    }
}

/// Drive the stub through every [`SandboxApi`] method and render a
/// timing/ID-free transcript.
fn run_stub_scenario(mut client: SandboxClient) -> Vec<String> {
    let child = client.launch(spec()).expect("launch");
    let alive = client.alive(&child).expect("alive");
    let evidence = client.terminate(&child).expect("terminate");
    vec![
        format!("child={child:?}"),
        format!("alive={alive}"),
        format!("terminate={evidence:?}"),
    ]
}

#[test]
fn sandbox_api_over_connect_matches_framed() {
    let dir = ScratchDir::new("ipcx-sandbox");
    let framed_socket = dir.join("framed.sock");
    let connect_socket = dir.join("connect.sock");

    spawn_sandbox_server(framed_socket.clone(), Wire::Framed);
    spawn_sandbox_server(connect_socket.clone(), Wire::Connect);
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let framed = retry_connect(|| SandboxClient::connect_wire(&framed_socket, Wire::Framed))
        .expect("framed connect");
    let connect = retry_connect(|| SandboxClient::connect_wire(&connect_socket, Wire::Connect))
        .expect("connect connect");

    // Every typed call produces the same transcript on both wires.
    assert_eq!(run_stub_scenario(framed), run_stub_scenario(connect));
}

#[test]
fn sandbox_connect_missing_socket_is_unavailable() {
    let dir = ScratchDir::new("ipcx-sandbox-miss");
    let err = SandboxClient::connect_wire(&dir.join("nope.sock"), Wire::Connect)
        .err()
        .expect("connect to a missing socket must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}
