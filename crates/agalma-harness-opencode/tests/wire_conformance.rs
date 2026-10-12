//! IPCX wire conformance across the OpenCode harness ↔ sandbox boundary
//! (`agalma-3ej`).
//!
//! The pilot failure: the OpenCode harness **server** hardcoded `Wire::Framed`
//! for its outbound sandbox client while the sandbox **server** env-selects
//! `Connect` by default, so a default-env harness talked B1 framed JSON to a
//! Connect sandbox and every live session died with
//! `sandbox protocol violation … "HTTP"` (the HTTP/2 preamble was read as a
//! frame length). The conductor exports `AGALMA_IPC_WIRE` (`WIRE_ENV`) to its
//! harness child, but that never reached the hardcoded sandbox client.
//!
//! These tests pin the convention: every boundary the harness server owns (the
//! inbound serve loop and the outbound sandbox client) reads `AGALMA_IPC_WIRE`
//! with a `Connect` default, matching the sandbox server, and
//! `AGALMA_IPC_WIRE=framed` still lines both up end to end.
//!
//! Scratch state lives under `target/test-runs/w-*` (never `/tmp`); the
//! scratch dir auto-removes on success.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agalma_contracts::harness::HarnessApi;
use agalma_contracts::ids::AttemptId;
use agalma_contracts::sandbox::{SandboxApi, SandboxChild};
use agalma_harness_opencode::{connect_sandbox, run_from_args, wire_from_env};
use agalma_ipc::{HarnessClient, Wire, WIRE_ENV};
use agalma_testkit::{retry_connect, ScratchDir};

/// A framed client that talks to the wrong wire blocks on a read with no reply;
/// bound every boundary round trip so a regression fails instead of hanging.
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(15);

/// A child handle that is never alive: `alive` is a pure transport round trip.
fn bogus_child() -> SandboxChild {
    SandboxChild {
        pid: u32::MAX,
        pgid: u32::MAX,
        attempt: AttemptId::derive(0),
        started_at_unix_ms: 0,
    }
}

/// Run `f` on a worker thread; panic if it does not finish within
/// [`ROUND_TRIP_TIMEOUT`].
fn within<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(ROUND_TRIP_TIMEOUT)
        .unwrap_or_else(|_| panic!("{what}: no reply within {ROUND_TRIP_TIMEOUT:?}"))
}

/// Boot a default-env sandbox server beside a harness server that reaches it
/// through the production [`run_from_args`] entry point, then prove both owned
/// boundaries interoperate on `expected_wire`.
///
/// `wire_env` is the value to place in [`WIRE_ENV`] (`None` removes it, i.e. the
/// default env).
fn phase(label: &str, expected_wire: Wire, wire_env: Option<&str>) {
    match wire_env {
        Some(value) => std::env::set_var(WIRE_ENV, value),
        None => std::env::remove_var(WIRE_ENV),
    }
    assert_eq!(
        wire_from_env(),
        expected_wire,
        "{label}: harness wire must follow AGALMA_IPC_WIRE with a Connect default"
    );

    let dir = ScratchDir::new("w");
    let run = dir.child("d");
    let sandbox_sock = run.join("sandbox.sock");
    let harness_sock = run.join("h.sock");

    // Default-env sandbox server (mirrors `agalma-sandbox-server`): its wire is
    // `Wire::from_env(Wire::Connect)`.
    {
        let sock = sandbox_sock.clone();
        thread::spawn(move || {
            let _ = agalma_sandbox::serve(&sock);
        });
    }
    retry_connect(|| connect_sandbox(&sandbox_sock))
        .unwrap_or_else(|e| panic!("{label}: sandbox server never became reachable: {e}"));

    // Outbound sandbox boundary — the exact `agalma-3ej` failure: the harness
    // server's sandbox client must round-trip to the sandbox server. This runs
    // before the harness server starts because the B1 framed server serves one
    // connection at a time; once the harness holds its (persistent) connection,
    // a second framed client would block.
    let sock = sandbox_sock.clone();
    let alive = within(&format!("{label}: sandbox alive round trip"), move || {
        connect_sandbox(&sock)
            .map_err(|e| e.to_string())
            .and_then(|sandbox| sandbox.alive(&bogus_child()).map_err(|e| e.to_string()))
    });
    assert!(
        !alive.unwrap_or_else(|e| panic!("{label}: sandbox round trip: {e}")),
        "{label}: sandbox liveness round trip"
    );

    // Harness server through the real argv/env entry point. Its sandbox socket
    // defaults to `<socket parent>/sandbox.sock`, which is where we bound it.
    {
        let sock = harness_sock.clone();
        thread::spawn(move || {
            let args = ["--socket".to_string(), sock.to_string_lossy().into_owned()];
            let _ = run_from_args(args);
        });
    }

    // Serve boundary: a client on the same env-selected wire reaches the
    // harness server (`connect_wire` runs a live `describe`).
    let client = retry_connect(|| HarnessClient::connect_wire(&harness_sock, expected_wire))
        .unwrap_or_else(|e| panic!("{label}: connect harness server: {e}"));
    assert_eq!(
        client.describe().impl_name,
        "opencode",
        "{label}: harness identity"
    );

    std::env::remove_var(WIRE_ENV);
}

#[test]
fn harness_and_sandbox_wire_agree_on_default_and_framed() {
    // Default env: both harness boundaries and the sandbox server pick Connect.
    phase("default", Wire::Connect, None);
    // `AGALMA_IPC_WIRE=framed` still lines every boundary up on framed.
    phase("framed", Wire::Framed, Some("framed"));
}
