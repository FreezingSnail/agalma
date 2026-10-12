//! Workspace boundary over the Connect transport (IPCX.4).
//!
//! The typed [`WorkspaceApi`]/[`WorkspaceOps`] client (`WorkspaceClient`) is
//! exercised against a `register_workspace` server served over both wires: the
//! B1 framed transport and the IPCX Connect/UDS transport. A deterministic stub
//! workspace drives the full operation surface (the frozen six-method trait plus
//! the M1 repo-mode operations); the two wires must produce an identical typed
//! transcript, proving the workspace boundary works over Connect with parity to
//! framed.
//!
//! Servers bind under `target/test-runs/` — never `/tmp`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::{Checkout, ContractError, ExecutionId, MergeReceipt, WorkspaceApi};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::workspace_client::WorkspaceOps;
use agalma_ipc::workspace_server::{register_workspace, serve_workspace};
use agalma_ipc::{Wire, WorkspaceClient};
use agalma_testkit::{retry_connect, ScratchDir};

/// A deterministic stub workspace: fixed SHAs and paths, no clock or real repo.
struct StubWorkspace {
    prepared: u32,
}

impl WorkspaceApi for StubWorkspace {
    fn prepare(
        &mut self,
        template: &str,
        execution: &ExecutionId,
    ) -> Result<Checkout, ContractError> {
        self.prepared += 1;
        Ok(Checkout {
            path: format!("{template}/checkout"),
            base_sha: "base0000".to_string(),
            candidate_branch: format!("candidate/{}", execution.as_str().replace(':', "-")),
        })
    }

    fn diff(&self, _checkout: &Checkout) -> Result<String, ContractError> {
        Ok("+stub change\n".to_string())
    }

    fn integrate(
        &mut self,
        _checkout: &Checkout,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        Ok(MergeReceipt {
            expected_main_sha: expected_main_sha.to_string(),
            candidate_sha: "cand0000".to_string(),
            result_sha: "cand0000".to_string(),
        })
    }

    fn tag(&mut self, _name: &str, _sha: &str) -> Result<(), ContractError> {
        Ok(())
    }

    fn reconcile(&mut self, _checkout: &Checkout) -> Result<bool, ContractError> {
        Ok(true)
    }

    fn revert(&mut self, _sha: &str) -> Result<(), ContractError> {
        Ok(())
    }
}

impl WorkspaceOps for StubWorkspace {
    fn prepare_repo(
        &mut self,
        origin_path: &Path,
        _base_ref: &str,
        execution: &ExecutionId,
        base_dir: &Path,
    ) -> Result<Checkout, ContractError> {
        self.prepared += 1;
        Ok(Checkout {
            path: format!(
                "{}::{}/runs/{}/checkout",
                origin_path.display(),
                base_dir.display(),
                execution.as_str().replace(':', "-")
            ),
            base_sha: "base0000".to_string(),
            candidate_branch: format!("candidate/{}", execution.as_str().replace(':', "-")),
        })
    }

    fn integrate_origin(
        &self,
        _checkout: &Checkout,
        _origin_path: &Path,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        Ok(MergeReceipt {
            expected_main_sha: expected_main_sha.to_string(),
            candidate_sha: "cand0000".to_string(),
            result_sha: "cand0000".to_string(),
        })
    }

    fn candidate_sha(&self, _checkout: &Checkout) -> Result<String, ContractError> {
        Ok("cand0000".to_string())
    }

    fn commit_candidate(&self, _checkout: &Checkout) -> Result<String, ContractError> {
        Ok("cand0000".to_string())
    }

    fn prepare_verify_checkout(
        &self,
        _checkout: &Checkout,
        base_dir: &Path,
        _execution: &ExecutionId,
        sha: &str,
    ) -> Result<PathBuf, ContractError> {
        Ok(base_dir.join("verify").join(sha))
    }

    fn origin_receipt(
        &self,
        _checkout: &Checkout,
        _origin_path: &Path,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        Ok(None)
    }

    fn revert_origin(
        &self,
        _origin_path: &Path,
        _to_sha: &str,
        _from_sha: &str,
    ) -> Result<(), ContractError> {
        Ok(())
    }

    fn integrated_receipt(
        &mut self,
        _checkout: &Checkout,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        Ok(Some(MergeReceipt {
            expected_main_sha: "base0000".to_string(),
            candidate_sha: "cand0000".to_string(),
            result_sha: "cand0000".to_string(),
        }))
    }
}

fn workspace_info() -> ServerInfo {
    ServerInfo {
        api: "WorkspaceApi".to_string(),
        api_version: 0,
        impl_name: "stub".to_string(),
        impl_version: "stub/v1".to_string(),
        capabilities: BTreeSet::new(),
    }
}

fn spawn_workspace_server(socket: PathBuf, wire: Wire) {
    let mut server = Server::new(workspace_info());
    register_workspace(&mut server, StubWorkspace { prepared: 0 });
    std::thread::spawn(move || {
        let _ = serve_workspace(server, &socket, wire);
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

/// Drive the stub through every workspace operation and render a
/// timing/ID-free transcript. Paths are fixed synthetic roots (the stub touches
/// no disk), so both wires must render byte-identical output.
fn run_stub_scenario(mut client: WorkspaceClient) -> Vec<String> {
    let exec = ExecutionId::new("exec:wp-connect:1");
    let template = "/ws/template";
    let origin = Path::new("/ws/origin");
    let state = Path::new("/ws/state");

    let checkout = client.prepare(template, &exec).expect("prepare");
    let diff = client.diff(&checkout).expect("diff");
    let receipt = client
        .integrate(&checkout, &checkout.base_sha)
        .expect("integrate");
    client.tag("wp/connect", &receipt.result_sha).expect("tag");
    let reconciled = client.reconcile(&checkout).expect("reconcile");
    client.revert(&checkout.base_sha).expect("revert");

    let repo_checkout = client
        .prepare_repo(origin, "main", &exec, state)
        .expect("prepare_repo");
    let repo_diff = client.diff(&repo_checkout).expect("repo diff");
    let before = client
        .origin_receipt(&repo_checkout, origin)
        .expect("origin_receipt before");
    let committed = client
        .commit_candidate(&repo_checkout)
        .expect("commit_candidate");
    let candidate = client.candidate_sha(&repo_checkout).expect("candidate_sha");
    let verify = client
        .prepare_verify_checkout(&repo_checkout, state, &exec, &committed)
        .expect("prepare_verify_checkout");
    let origin_receipt = client
        .integrate_origin(&repo_checkout, origin, &repo_checkout.base_sha)
        .expect("integrate_origin");
    let after = client
        .origin_receipt(&repo_checkout, origin)
        .expect("origin_receipt after");
    client
        .revert_origin(origin, &repo_checkout.base_sha, &origin_receipt.result_sha)
        .expect("revert_origin");
    let integrated = client
        .integrated_receipt(&repo_checkout)
        .expect("integrated_receipt");

    vec![
        format!("checkout={checkout:?}"),
        format!("diff={diff:?}"),
        format!("integrate={receipt:?}"),
        format!("reconcile={reconciled}"),
        format!("repo_checkout={repo_checkout:?}"),
        format!("repo_diff={repo_diff:?}"),
        format!("before={before:?}"),
        format!("committed={committed:?}"),
        format!("candidate={candidate:?}"),
        format!("verify={verify:?}"),
        format!("origin_receipt={origin_receipt:?}"),
        format!("after={after:?}"),
        format!("integrated={integrated:?}"),
    ]
}

#[test]
fn workspace_api_over_connect_matches_framed() {
    let dir = ScratchDir::new("ipcx-workspace");
    let framed_socket = dir.join("framed.sock");
    let connect_socket = dir.join("connect.sock");

    spawn_workspace_server(framed_socket.clone(), Wire::Framed);
    spawn_workspace_server(connect_socket.clone(), Wire::Connect);
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let framed = retry_connect(|| WorkspaceClient::connect_wire(&framed_socket, Wire::Framed))
        .expect("framed connect");
    let connect = retry_connect(|| WorkspaceClient::connect_wire(&connect_socket, Wire::Connect))
        .expect("connect connect");

    // Every typed call produces the same transcript on both wires.
    assert_eq!(run_stub_scenario(framed), run_stub_scenario(connect),);
}

#[test]
fn workspace_connect_missing_socket_is_unavailable() {
    let dir = ScratchDir::new("ipcx-workspace-miss");
    let err = WorkspaceClient::connect_wire(&dir.join("nope.sock"), Wire::Connect)
        .err()
        .expect("connect to a missing socket must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}
