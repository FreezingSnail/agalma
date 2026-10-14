//! Real-sandbox wire regression for the conductor's in-process harness
//! (`agalma-6w8`).
//!
//! The bug: `open_opencode_harness`'s in-process branch built its outbound
//! [`agalma_ipc::SandboxClient`] with the legacy `SandboxClient::connect` framed
//! default, while the sandbox server (`SandboxSource::ensure` ->
//! `agalma-sandbox-server`) serves `sandbox_wire()` = `Wire::from_env(Wire::Connect)`.
//! With `AGALMA_IPC_WIRE` unset, an in-process harness paired a framed client
//! with a Connect server, so every sandbox call failed with a transport/protocol
//! fault. The fix pairs the client with `crate::sandbox_process::sandbox_wire()`.
//!
//! This target boots a **real** Seatbelt sandbox server (`agalma_sandbox::serve`,
//! the same backend as the `agalma-sandbox-server` binary) and drives the
//! in-process harness through the production `open_opencode_harness` factory with
//! a scripted program that exits immediately. Reaching the readiness stage proves
//! the sandbox `launch` call crossed the boundary on the matched wire; the
//! pre-fix framed/Connect mismatch fails *inside* `launch` with a transport error
//! instead.
//!
//! No vendor, no network, no paid provider call. Scratch state lives under
//! `target/test-runs/hw6-*` (a short prefix so the sandbox socket path fits
//! `SUN_LEN`); it auto-removes on success.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agalma_conductor::harness_process::{
    open_opencode_harness, HarnessTransport, OpenCodeHarnessInit,
};
use agalma_conductor::sandbox_process::sandbox_wire;
use agalma_contracts::harness::{HarnessApi, Limits, StartAttemptRequest};
use agalma_contracts::ids::{ArtifactRef, OperationId};
use agalma_harness_opencode::OpenCodeHarnessConfig;
use agalma_ipc::{SandboxClient, Wire, WIRE_ENV};
use agalma_sandbox::PRODUCT_PROFILE;
use agalma_testkit::{retry_connect, ScratchDir};

/// A wrong-wire client blocks on a read with no reply; bound every round trip so
/// a regression fails instead of hanging.
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(30);
/// Short readiness window: the scripted program exits immediately, so this only
/// bounds the post-launch readiness poll if the pid liveness probe lags.
const READINESS_TIMEOUT: Duration = Duration::from_secs(3);
/// A scripted "opencode serve" that ignores argv and exits immediately.
const EXITING_PROGRAM: &str = "/usr/bin/true";
const MODEL: &str = "role-default";

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

/// Attempt-scoped config with a scripted program and a short readiness window.
fn config(run: &Path) -> OpenCodeHarnessConfig {
    let attempt = run.join("attempt");
    let protected = run.join("protected");
    let sock = run.join("sock");
    let repo = run.join("repo");
    for sub in [&attempt, &protected, &sock, &repo] {
        fs::create_dir_all(sub).expect("create run subdir");
    }
    let profile = run.join("worker.sb");
    fs::write(&profile, PRODUCT_PROFILE).expect("write profile");
    let mut config = OpenCodeHarnessConfig::new(profile, attempt, protected, sock);
    config.program = Some(PathBuf::from(EXITING_PROGRAM));
    config.readiness_timeout = READINESS_TIMEOUT;
    config
}

fn start_attempt_req(workspace: &Path) -> StartAttemptRequest {
    StartAttemptRequest {
        workspace: workspace.to_string_lossy().into_owned(),
        role: "designer".to_string(),
        genome_ref: ArtifactRef::derive("genome").to_string(),
        constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
        limits: Limits {
            wall_ms: 5_000,
            tokens: 0,
            cost_micros: 0,
        },
        operation_id: OperationId::new("op:harness-sandbox-wire/1"),
    }
}

/// Boot a real sandbox server on the env-selected wire, build the in-process
/// harness through `open_opencode_harness`, and prove the sandbox `launch` calls
/// cross the boundary (the scripted program exits, so the observable is the
/// readiness-stage failure rather than a transport fault).
fn phase(label: &str, wire_env: Option<&str>, expected: Wire) {
    match wire_env {
        Some(value) => std::env::set_var(WIRE_ENV, value),
        None => std::env::remove_var(WIRE_ENV),
    }
    assert_eq!(
        sandbox_wire(),
        expected,
        "{label}: sandbox boundary wire must follow AGALMA_IPC_WIRE with a Connect default"
    );

    let dir = ScratchDir::new("hw6");
    let run = dir.child("d");
    fs::create_dir_all(&run).expect("run dir");
    let sandbox_sock = run.join("sandbox.sock");
    let harness_sock = run.join("harness.sock");

    // Real Seatbelt sandbox server on `sandbox_wire()` (mirrors the server the
    // conductor spawns via `SandboxSource::ensure`).
    {
        let sock = sandbox_sock.clone();
        thread::spawn(move || {
            let _ = agalma_sandbox::serve(&sock);
        });
    }
    retry_connect(|| SandboxClient::connect_wire(&sandbox_sock, expected).map(|_| ()))
        .unwrap_or_else(|e| panic!("{label}: sandbox server never became reachable: {e}"));

    let cfg = config(&run);
    let workspace = fs::canonicalize(run.join("repo")).expect("workspace");
    let init = OpenCodeHarnessInit {
        transport: HarnessTransport::InProcess,
        config: cfg,
        root: run.clone(),
        model: MODEL.to_string(),
        generation: 1,
        sandbox_socket: sandbox_sock.clone(),
        harness_socket: harness_sock,
    };

    let outcome = within(&format!("{label}: in-process harness start_attempt"), move || {
        let mut harness =
            open_opencode_harness(init).map_err(|e| format!("build harness: {e}"))?;
        match harness.start_attempt(start_attempt_req(&workspace)) {
            // `/usr/bin/true` exits, so a successful start_attempt would mean the
            // readiness probe never observed the exit.
            Ok(_) => Err("scripted program must exit before readiness".to_string()),
            Err(error) => Ok(error.to_string()),
        }
    });

    let message = outcome.unwrap_or_else(|e| panic!("{label}: {e}"));
    assert!(
        !message.contains("sandbox transport") && !message.contains("sandbox protocol"),
        "{label}: in-process harness sandbox client must speak the server wire, got a transport fault: {message}"
    );
    assert!(
        message.contains("exited before readiness") || message.contains("not ready within"),
        "{label}: the sandbox launch must reach the readiness stage, got: {message}"
    );

    std::env::remove_var(WIRE_ENV);
}

/// Default env: the in-process harness and the real sandbox server agree on
/// `Connect`. `AGALMA_IPC_WIRE=framed` still lines both up end to end.
#[test]
fn in_process_harness_matches_real_sandbox_wire_on_default_and_framed() {
    phase("default", None, Wire::Connect);
    phase("framed", Some("framed"), Wire::Framed);
}
