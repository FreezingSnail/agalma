//! IPCX wire conformance for the OpenCode harness server's serve boundary.
//!
//! The production entry point [`run_from_args`] must serve on the
//! env-selected wire ([`WIRE_ENV`] via [`wire_from_env`], `Connect` by
//! default, `framed` for parity), matching the peer component servers. A
//! client that hardcoded one wire while the server served the other read the
//! HTTP/2 preamble as a frame length (`"HTTP"` → protocol violation), which
//! broke every live session (`agalma-3ej`).
//!
//! Sandbox removal (P2) deleted the harness server's outbound sandbox client, so
//! the harness↔sandbox wire agreement this file used to pin no longer exists;
//! the serve boundary is the only remaining wire. These tests pin its
//! convention through the real argv/env entry point.
//!
//! Scratch state lives under `target/test-runs/w-*` (never `/tmp`); the
//! scratch dir auto-removes on success.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agalma_contracts::harness::HarnessApi;
use agalma_harness_opencode::{run_from_args, wire_from_env};
use agalma_ipc::{HarnessClient, Wire, WIRE_ENV};
use agalma_testkit::{retry_connect, ScratchDir};

/// A framed client that talks to the wrong wire blocks on a read with no reply;
/// bound every boundary round trip so a regression fails instead of hanging.
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(15);

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

/// A socket path that fits `SUN_LEN`, addressed through a short symlink alias of
/// `run` under the system temp root (mirrors the conductor's
/// `bounded_socket_path`). The socket file still lands in `run`; only the
/// addressed path is shortened. A deep implementation worktree otherwise pushes
/// `<run>/<name>` past the Unix socket limit.
fn bounded_socket(run: &Path, name: &str) -> PathBuf {
    const SUN_LEN_LIMIT: usize = 104;
    let direct = run.join(name);
    if direct.as_os_str().len() < SUN_LEN_LIMIT {
        return direct;
    }
    let root = std::env::temp_dir().join("agalma-wire-conf");
    std::fs::create_dir_all(&root).expect("alias root");
    let alias = root.join(format!("w{}", std::process::id()));
    let _ = std::fs::remove_file(&alias);
    std::os::unix::fs::symlink(run, &alias).expect("alias symlink");
    alias.join(name)
}

/// Boot the harness server through the production [`run_from_args`] entry point
/// and prove its serve boundary speaks `expected_wire`.
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
    let harness_sock = bounded_socket(&run, "h.sock");

    // Harness server through the real argv/env entry point.
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
    let describe = within(&format!("{label}: harness describe"), move || {
        client.describe()
    });
    assert_eq!(describe.impl_name, "opencode", "{label}: harness identity");

    std::env::remove_var(WIRE_ENV);
}

#[test]
fn harness_server_serves_the_env_selected_wire() {
    // Default env: the serve boundary picks Connect.
    phase("default", Wire::Connect, None);
    // `AGALMA_IPC_WIRE=framed` lines the boundary up on framed.
    phase("framed", Wire::Framed, Some("framed"));
}
