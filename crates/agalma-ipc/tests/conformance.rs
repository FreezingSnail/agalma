//! IPC conformance tests (B1.1).
//!
//! A test-only component implements `echo` + an effectful counter; the server
//! runtime is exercised over a real Unix socket under `target/test-runs/`. The
//! cases cover decision D1's guarantees: duplicate-id receipt replay, reconnect
//! replay, connection-loss classification (`Unavailable` vs `UnknownOutcome`),
//! frame-limit enforcement, version refusal, unknown methods, and id mismatch.

use std::collections::BTreeSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::ContractError;
use agalma_ipc::client::Client;
use agalma_ipc::error::IpcError;
use agalma_ipc::frame;
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::{Response, MAX_FRAME};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/agalma-ipc
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate must live under <root>/crates/")
        .to_path_buf()
}

/// A unique socket path under `target/test-runs/b1ipc` (never `/tmp`).
fn socket_path(tag: &str) -> PathBuf {
    let dir = workspace_root()
        .join("target")
        .join("test-runs")
        .join("b1ipc");
    std::fs::create_dir_all(&dir).expect("create test-runs dir");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    dir.join(format!("{tag}-{}-{nanos}.sock", std::process::id()))
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

/// A server with `echo` (returns params) and an effectful `counter.increment`.
fn test_server(counter: Arc<AtomicU64>) -> Server {
    let mut server = Server::new(info());
    server.on("echo", |params: Value| -> Result<Value, ContractError> {
        Ok(params)
    });
    server.on(
        "counter.increment",
        move |_params: Value| -> Result<Value, ContractError> {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(json!({ "count": n }))
        },
    );
    server.on("fail.known", |_params: Value| -> Result<Value, ContractError> {
        Err(ContractError::KnownFailure("boom".into()))
    });
    server
}

/// Bind and accept in a background thread; returns once the socket is bound.
fn spawn_server(server: Server, socket: PathBuf) -> thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let bind_socket = socket.clone();
    let handle = thread::spawn(move || {
        let mut server = server;
        let listener = match server.bind(&bind_socket) {
            Ok(listener) => {
                tx.send(Ok(())).ok();
                listener
            }
            Err(e) => {
                tx.send(Err(e)).ok();
                return;
            }
        };
        server.accept_loop(listener).ok();
    });
    match rx.recv().expect("server thread should signal bind") {
        Ok(()) => handle,
        Err(e) => panic!("server bind failed: {e}"),
    }
}

#[test]
fn echo_round_trip() {
    let socket = socket_path("echo");
    let _server = spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone());

    let mut client = Client::connect(&socket).expect("connect");
    let out = client.call("echo", json!({"hello": "world"})).expect("call");
    assert_eq!(out, json!({"hello": "world"}));
}

#[test]
fn typed_call_round_trip() {
    #[derive(Serialize)]
    struct Add {
        a: i64,
        b: i64,
    }
    #[derive(Deserialize, PartialEq, Debug)]
    struct Sum {
        sum: i64,
    }

    let mut server = test_server(Arc::new(AtomicU64::new(0)));
    server.on("add", |params: Value| -> Result<Value, ContractError> {
        let a = params.get("a").and_then(Value::as_i64).unwrap_or(0);
        let b = params.get("b").and_then(Value::as_i64).unwrap_or(0);
        Ok(json!({ "sum": a + b }))
    });

    let socket = socket_path("typed");
    let _server = spawn_server(server, socket.clone());

    let mut client = Client::connect(&socket).expect("connect");
    let sum: Sum = client
        .call_typed("add", &Add { a: 2, b: 3 })
        .expect("typed call");
    assert_eq!(sum, Sum { sum: 5 });
}

#[test]
fn duplicate_id_returns_recorded_result() {
    let counter = Arc::new(AtomicU64::new(0));
    let socket = socket_path("dup");
    let _server = spawn_server(test_server(counter.clone()), socket.clone());

    let mut client = Client::connect(&socket).expect("connect");

    let first = client
        .call_with_id(7, "counter.increment", json!({}))
        .expect("first");
    assert_eq!(first, json!({ "count": 1 }));

    // Same id with different params: replayed receipt, no new effect.
    let replay = client
        .call_with_id(7, "counter.increment", json!({"ignored": true}))
        .expect("replay");
    assert_eq!(replay, json!({ "count": 1 }));
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // A fresh id runs the effect again.
    let next = client
        .call_with_id(8, "counter.increment", json!({}))
        .expect("next");
    assert_eq!(next, json!({ "count": 2 }));
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn replay_survives_reconnect() {
    let counter = Arc::new(AtomicU64::new(0));
    let socket = socket_path("reconnect");
    let _server = spawn_server(test_server(counter.clone()), socket.clone());

    {
        let mut client = Client::connect(&socket).expect("connect");
        let first = client
            .call_with_id(1, "counter.increment", json!({}))
            .expect("first");
        assert_eq!(first, json!({ "count": 1 }));
    } // drop connection

    let mut client = Client::connect(&socket).expect("reconnect");
    let replay = client
        .call_with_id(1, "counter.increment", json!({}))
        .expect("replay after reconnect");
    assert_eq!(replay, json!({ "count": 1 }));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn connect_failure_is_unavailable() {
    let missing = socket_path("missing");
    // Path was never bound.
    let err = Client::connect(&missing)
        .err()
        .expect("connect must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}

#[test]
fn connection_loss_mid_call_is_unknown_outcome() {
    let socket = socket_path("loss");
    let listener = UnixListener::bind(&socket).expect("bind");

    // Server reads one request frame, then drops the connection unresponsive.
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let _ = frame::read_payload(&mut reader);
        drop(stream);
    });

    let mut client = Client::connect(&socket).expect("connect");
    let err = client
        .call("counter.increment", json!({}))
        .expect_err("call must fail");
    assert!(
        err.is_unknown_outcome(),
        "expected UnknownOutcome, got {err:?}"
    );
    handle.join().ok();
}

#[test]
fn method_not_found_is_unsupported_capability() {
    let socket = socket_path("nomethod");
    let _server = spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone());

    let mut client = Client::connect(&socket).expect("connect");
    let err = client.call("does.not.exist", json!({})).expect_err("must fail");
    match err {
        IpcError::Contract(ContractError::UnsupportedCapability(message)) => {
            assert!(message.contains("does.not.exist"));
        }
        other => panic!("expected UnsupportedCapability, got {other:?}"),
    }
}

#[test]
fn contract_error_propagates() {
    let socket = socket_path("failknown");
    let _server = spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone());

    let mut client = Client::connect(&socket).expect("connect");
    let err = client.call("fail.known", json!({})).expect_err("must fail");
    assert_eq!(err, IpcError::Contract(ContractError::KnownFailure("boom".into())));
}

#[test]
fn client_rejects_oversized_request() {
    let socket = socket_path("bigreq");
    let _server = spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone());

    let mut client = Client::connect(&socket).expect("connect");
    let big = "x".repeat(MAX_FRAME + 1);
    let err = client
        .call("echo", json!({ "blob": big }))
        .expect_err("oversized request must fail");
    assert!(matches!(err, IpcError::Protocol(_)), "got {err:?}");
}

#[test]
fn server_rejects_oversized_frame() {
    let socket = socket_path("bigframe");
    let _server = spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone());

    let mut raw = UnixStream::connect(&socket).expect("connect");
    let declared = u32::try_from(MAX_FRAME).expect("fits") + 1;
    raw.write_all(&declared.to_be_bytes()).expect("write prefix");
    raw.flush().expect("flush");

    // The server refuses the frame and closes the connection.
    let mut buf = [0u8; 1];
    match raw.read(&mut buf) {
        Ok(0) | Err(_) => {}
        Ok(n) => panic!("expected closed connection, read {n} bytes"),
    }
}

#[test]
fn version_mismatch_is_refused() {
    let socket = socket_path("version");
    let _server = spawn_server(test_server(Arc::new(AtomicU64::new(0))), socket.clone());

    let mut raw = UnixStream::connect(&socket).expect("connect");
    let request = json!({"v": 2, "id": 1, "method": "echo", "params": {}});
    frame::write_value(&mut raw, &request).expect("write request");
    raw.flush().expect("flush");

    let mut reader = BufReader::new(raw.try_clone().expect("clone"));
    let payload = frame::read_payload(&mut reader)
        .expect("read response")
        .expect("response present");
    let response: Response = serde_json::from_slice(&payload).expect("decode response");
    assert_eq!(response.id, 1);
    let error = response.error.expect("error present");
    assert_eq!(error.code, "Protocol");
    assert!(response.result.is_none());

    // And the server closes the connection after refusing.
    drop(reader);
    let mut buf = [0u8; 1];
    match raw.read(&mut buf) {
        Ok(0) | Err(_) => {}
        Ok(n) => panic!("expected closed connection, read {n} bytes"),
    }
}

#[test]
fn id_mismatch_is_rejected() {
    let socket = socket_path("idmismatch");
    let listener = UnixListener::bind(&socket).expect("bind");

    // Malicious/broken server replies with the wrong id.
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = BufWriter::new(stream);
        let _ = frame::read_payload(&mut reader);
        let response = Response::ok(999, json!({}));
        frame::write_value(&mut writer, &response).expect("write response");
        writer.flush().ok();
    });

    let mut client = Client::connect(&socket).expect("connect");
    let err = client.call("echo", json!({})).expect_err("must fail");
    assert!(matches!(err, IpcError::Protocol(_)), "got {err:?}");
    handle.join().ok();
}
