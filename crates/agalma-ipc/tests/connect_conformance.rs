//! ConnectRPC envelope conformance tests (IPCX.1, design D1/D2).
//!
//! The same guarantees the framed-JSON suite asserts (`conformance.rs`) are
//! re-checked over the Connect/UDS transport: echo round trip, duplicate-id
//! receipt replay, reconnect replay, `UnknownOutcome`/contract error mapping,
//! unknown-method refusal, version refusal, missing-socket `Unavailable`, and
//! HTTP/2 connection multiplexing (which subsumes `agalma-www`).
//!
//! Servers run over a real `tokio::net::UnixListener` under
//! `target/test-runs/` — never `/tmp`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agalma_contracts::ContractError;
use agalma_ipc::connect::ConnectClient;
use agalma_ipc::error::IpcError;
use agalma_ipc::proto::agalma::ipc::v1::{IpcClient, IpcRequest};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

fn socket_path(tag: &str) -> (ScratchDir, PathBuf) {
    let dir = ScratchDir::new("ipcx");
    let socket = dir.join(format!("{tag}.sock"));
    (dir, socket)
}

fn info() -> ServerInfo {
    ServerInfo {
        api: "test-api".into(),
        api_version: 1,
        impl_name: "test-component".into(),
        impl_version: "0.0.1".into(),
        capabilities: BTreeSet::from(["echo".into(), "counter".into()]),
    }
}

/// A server with `echo`, an effectful `counter.increment`, and error methods.
fn test_server(counter: Arc<AtomicU64>) -> Server {
    let mut server = Server::new(info());
    server.on("echo", |params: Value| -> Result<Value, ContractError> {
        Ok(params)
    });
    let increment = counter.clone();
    server.on(
        "counter.increment",
        move |_params: Value| -> Result<Value, ContractError> {
            let n = increment.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(json!({ "count": n }))
        },
    );
    // A deliberately slow effectful method: a concurrent duplicate must arrive
    // while the first execution is still in flight.
    server.on(
        "counter.slow",
        move |_params: Value| -> Result<Value, ContractError> {
            std::thread::sleep(Duration::from_millis(150));
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(json!({ "count": n }))
        },
    );
    server.on(
        "fail.known",
        |_params: Value| -> Result<Value, ContractError> {
            Err(ContractError::KnownFailure("boom".into()))
        },
    );
    server.on(
        "fail.unknown_outcome",
        |_params: Value| -> Result<Value, ContractError> {
            Err(ContractError::UnknownOutcome("indeterminate".into()))
        },
    );
    server
}

async fn spawn_server(server: Server, socket: PathBuf) {
    tokio::spawn(async move {
        agalma_ipc::connect::serve(server, &socket).await.ok();
    });
}

/// Connect under a stable logical identity, retrying until the socket is bound.
async fn connect_as(socket: &Path, client_id: &str) -> ConnectClient {
    for _ in 0..500 {
        match ConnectClient::connect_as(socket, client_id).await {
            Ok(client) => return client,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    panic!("connect server never became ready at {}", socket.display());
}

/// Connect, retrying until the server's READY line has bound the socket.
async fn connect(socket: &Path) -> ConnectClient {
    for _ in 0..500 {
        match ConnectClient::connect(socket).await {
            Ok(client) => return client,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    panic!("connect server never became ready at {}", socket.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_echo_round_trip() {
    let (_dir, socket) = socket_path("echo");
    spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone()).await;

    let mut client = connect(&socket).await;
    let out = client
        .call("echo", json!({"hello": "world"}))
        .await
        .expect("call");
    assert_eq!(out, json!({"hello": "world"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_duplicate_id_returns_recorded_result() {
    let counter = Arc::new(AtomicU64::new(0));
    let (_dir, socket) = socket_path("dup");
    spawn_server(test_server(counter.clone()), socket.clone()).await;

    let client = connect(&socket).await;

    let first = client
        .call_with_id(7, "counter.increment", json!({}))
        .await
        .expect("first");
    assert_eq!(first, json!({ "count": 1 }));

    // Same id with different params: replayed receipt, no new effect.
    let replay = client
        .call_with_id(7, "counter.increment", json!({"ignored": true}))
        .await
        .expect("replay");
    assert_eq!(replay, json!({ "count": 1 }));
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // A fresh id runs the effect again.
    let next = client
        .call_with_id(8, "counter.increment", json!({}))
        .await
        .expect("next");
    assert_eq!(next, json!({ "count": 2 }));
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_replay_survives_reconnect() {
    // One logical client keeps the same stable identity across a reconnect, so
    // its explicit-id replay still returns the prior receipt (HP.4,
    // `agalma-jmr.4`): identity is carried in `x-agalma-client`, not the envelope.
    let counter = Arc::new(AtomicU64::new(0));
    let (_dir, socket) = socket_path("reconnect");
    spawn_server(test_server(counter.clone()), socket.clone()).await;

    {
        let client = connect_as(&socket, "logical-a").await;
        let first = client
            .call_with_id(1, "counter.increment", json!({}))
            .await
            .expect("first");
        assert_eq!(first, json!({ "count": 1 }));
    } // connection dropped

    let client = connect_as(&socket, "logical-a").await;
    let replay = client
        .call_with_id(1, "counter.increment", json!({}))
        .await
        .expect("replay after reconnect");
    assert_eq!(replay, json!({ "count": 1 }));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_receipts_are_scoped_per_client() {
    // Two independent clients (distinct identities) draw the same numeric
    // request id: their receipts must not collide.
    let counter = Arc::new(AtomicU64::new(0));
    let (_dir, socket) = socket_path("scope");
    spawn_server(test_server(counter.clone()), socket.clone()).await;

    let a = connect_as(&socket, "client-a").await;
    let b = connect_as(&socket, "client-b").await;
    let from_a = a.call_with_id(1, "counter.increment", json!({})).await.expect("a");
    let from_b = b.call_with_id(1, "counter.increment", json!({})).await.expect("b");
    assert_eq!(from_a, json!({ "count": 1 }));
    assert_eq!(from_b, json!({ "count": 2 }), "independent clients do not share receipts");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_unknown_outcome_maps_in_band() {
    let (_dir, socket) = socket_path("unknown");
    spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone()).await;

    let mut client = connect(&socket).await;
    let err = client
        .call("fail.unknown_outcome", json!({}))
        .await
        .expect_err("must fail");
    assert!(
        err.is_unknown_outcome(),
        "expected UnknownOutcome, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_contract_error_propagates() {
    let (_dir, socket) = socket_path("failknown");
    spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone()).await;

    let mut client = connect(&socket).await;
    let err = client
        .call("fail.known", json!({}))
        .await
        .expect_err("must fail");
    assert_eq!(
        err,
        IpcError::Contract(ContractError::KnownFailure("boom".into()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_unknown_method_is_unsupported_capability() {
    let (_dir, socket) = socket_path("nomethod");
    spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone()).await;

    let mut client = connect(&socket).await;
    let err = client
        .call("does.not.exist", json!({}))
        .await
        .expect_err("must fail");
    match err {
        IpcError::Contract(ContractError::UnsupportedCapability(message)) => {
            assert!(message.contains("does.not.exist"));
        }
        other => panic!("expected UnsupportedCapability, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_missing_socket_is_unavailable() {
    let (_dir, missing) = socket_path("missing");
    let err = ConnectClient::connect(&missing)
        .await
        .err()
        .expect("connect must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_version_mismatch_is_protocol() {
    let (_dir, socket) = socket_path("version");
    spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone()).await;

    // Wait until the server has bound the socket.
    let _ready = connect(&socket).await;

    // Reach under the typed client to send a version-2 envelope directly.
    let authority: http::Uri = "http://agalma.local".parse().unwrap();
    let connection = connectrpc::client::Http2Connection::connect_unix(&socket, authority.clone())
        .await
        .expect("connect");
    let config = connectrpc::client::ClientConfig::new(authority)
        .with_protocol(connectrpc::Protocol::Connect);
    let client = IpcClient::new(connection.shared(1), config);

    let request = IpcRequest {
        v: 2,
        id: 1,
        method: "echo".to_string(),
        params_json: b"{}".to_vec(),
        ..Default::default()
    };
    let response = client.call(request).await.expect("rpc").into_owned();
    let error = response.error.as_option().expect("error present");
    assert_eq!(error.code, "Protocol");
    assert!(response.result_json.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_multiplexes_concurrent_calls() {
    // One connection, two overlapping calls: HTTP/2 streams. This is the
    // capability that replaces `agalma-www`'s one-connection-one-call limit.
    let (_dir, socket) = socket_path("mux");
    spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone()).await;

    let client = connect(&socket).await;
    let (a, b) = tokio::join!(
        client.call_with_id(1, "echo", json!({"a": 1})),
        client.call_with_id(2, "echo", json!({"b": 2})),
    );
    assert_eq!(a.expect("first"), json!({"a": 1}));
    assert_eq!(b.expect("second"), json!({"b": 2}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_concurrent_duplicate_runs_handler_once() {
    // HP.4 (`agalma-jmr.4`): concurrent duplicates of one logical client's
    // request id must run the handler exactly once — the dispatcher holds an
    // atomic in-flight claim, and later duplicates await the outcome and replay
    // it instead of re-running the effect.
    let counter = Arc::new(AtomicU64::new(0));
    let (_dir, socket) = socket_path("dup-concurrent");
    spawn_server(test_server(counter.clone()), socket.clone()).await;

    let client = Arc::new(connect(&socket).await);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            client.call_with_id(42, "counter.slow", json!({})).await
        }));
    }
    for handle in handles {
        let response = handle.await.expect("task").expect("duplicate acknowledged");
        assert_eq!(
            response,
            json!({ "count": 1 }),
            "every concurrent duplicate replays the one recorded effect"
        );
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "the slow handler ran exactly once for the shared request id"
    );
}
