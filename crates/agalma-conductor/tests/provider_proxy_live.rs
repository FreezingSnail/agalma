//! Gated live smoke: an OpenCode turn reaches the free model **through the
//! parent-owned loopback proxy**.
//!
//! Background (M0.6): the worker confine profile denied arbitrary network, so
//! the confined harness could not reach the provider directly. Sandbox removal
//! (P2) means the child *can* reach the network directly now, so the proxy is no
//! longer a security boundary (`docs/sandbox-removal-design.md` §7.2); it is
//! still the credential/audit path. This test proves the proxy path end to end.
//!
//! Ignored by default. Run it explicitly with the live flag set:
//!
//! ```text
//! AGALMA_PROXY_LIVE=1 cargo test -p agalma-conductor \
//!     --test provider_proxy_live -- --ignored --nocapture
//! ```
//!
//! Evidence asserted:
//!
//! 1. the turn reaches a terminal `Completed` with usage or a result;
//! 2. the proxy log shows an allowlisted `CONNECT` (`*.opencode.ai`) was the
//!    path used;
//! 3. the user's OpenCode state is untouched.
//!
//! All scratch state lives under `target/test-runs/proxy-live-*` (never `/tmp`);
//! scratch dirs auto-remove on success (kept on failure for forensics).

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use agalma_conductor::provider_proxy::{ProviderProxy, ProxyConfig};
use agalma_contracts::harness::{
    CreateSessionRequest, HarnessApi, OperationHandle, OperationState, RunTurnRequest,
    StartAttemptRequest,
};
use agalma_contracts::ids::{ArtifactRef, OperationId};
use agalma_harness_opencode::{OpenCodeHarness, OpenCodeHarnessConfig};
use agalma_process::LocalLauncher;
use agalma_testkit::ScratchDir;

fn live_enabled() -> bool {
    std::env::var("AGALMA_PROXY_LIVE").ok().as_deref() == Some("1")
}

fn run_root() -> ScratchDir {
    ScratchDir::new("proxy-live")
}

fn pid_alive(pid: u32) -> bool {
    match Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
        Err(_) => false,
    }
}

fn group_members(pgid: u32) -> Vec<u32> {
    Command::new("/usr/bin/pgrep")
        .arg("-g")
        .arg(pgid.to_string())
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn poll_terminal(
    harness: &mut OpenCodeHarness,
    op: &OperationHandle,
    timeout: Duration,
) -> OperationState {
    let deadline = Instant::now() + timeout;
    loop {
        let state = harness.inspect_operation(op).expect("inspect");
        if state.is_terminal() {
            return state;
        }
        if Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn turn_reaches_provider_through_parent_proxy() {
    if !live_enabled() {
        eprintln!("proxy live smoke skipped: set AGALMA_PROXY_LIVE=1 to run");
        return;
    }

    let root = run_root();
    let attempt_dir = root.join("attempt");
    let workspace = attempt_dir.join("work");
    for dir in [&attempt_dir, &workspace] {
        fs::create_dir_all(dir).expect("create attempt dir");
    }

    // Mirror `WorkspaceApi::prepare`: opencode's project discovery resolves the
    // git root, so an init'd checkout is created instead of climbing outward.
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("git init");
    assert!(status.success(), "git init workspace");

    // User OpenCode state must stay untouched.
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let user_config = home.join(".config").join("opencode");
    let user_data = home.join(".local").join("share").join("opencode");
    let before_config = mtime(&user_config);
    let before_data = mtime(&user_data);

    // 1. Parent-owned proxy on an ephemeral loopback port. Default allowlist is
    //    the provider host family; `AGALMA_PROXY_ALLOW` overrides.
    let proxy = ProviderProxy::start(ProxyConfig::from_env()).expect("start provider proxy");
    eprintln!(
        "proxy live smoke root: {} (proxy {})",
        root.display(),
        proxy.url()
    );

    let mut config = OpenCodeHarnessConfig::new();
    config.attempt_dir = attempt_dir.clone();
    config.proxy_url = Some(proxy.url());
    let mut harness = OpenCodeHarness::new(Box::new(LocalLauncher::new()), config);

    let attempt = harness
        .start_attempt(StartAttemptRequest {
            workspace: workspace.to_string_lossy().into_owned(),
            role: "builder".to_string(),
            genome_ref: ArtifactRef::derive("genome").to_string(),
            constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
            limits: agalma_contracts::harness::Limits {
                wall_ms: 300_000,
                tokens: 20_000,
                cost_micros: 0,
            },
            operation_id: OperationId::new("op:proxy-live:start"),
        })
        .expect("start_attempt in sandbox");
    let root_pid = harness.attempt_pid().expect("server pid");
    eprintln!("server pid: {root_pid}");

    let session = harness
        .create_session(CreateSessionRequest {
            role: "builder".to_string(),
            model: "role-default".to_string(),
            prompts_ref: ArtifactRef::derive("builder-prompts").to_string(),
            handoff_refs: vec![],
            tool_policy_ref: ArtifactRef::derive("tool-policy").to_string(),
        })
        .expect("create_session");
    let op = harness
        .run_turn(
            &session,
            RunTurnRequest {
                input_ref: ArtifactRef::derive("turn-1-input"),
                bounded_turns: 1,
                deadline_ms: 180_000,
            },
        )
        .expect("run_turn");

    let state = poll_terminal(&mut harness, &op, Duration::from_secs(240));
    let events = harness.read_events(&op).expect("read_events");
    eprintln!("turn state: {}", state.label());
    for event in &events {
        eprintln!("turn event: {}", event.render());
    }

    // 2. Proxy evidence: an allowlisted provider CONNECT was tunnelled.
    let requests = proxy.requests();
    for request in &requests {
        eprintln!(
            "proxy request: allowed={} {} {}:{}",
            request.allowed, request.method, request.host, request.port
        );
    }
    let provider_tunnels = requests
        .iter()
        .filter(|r| r.allowed && r.method == "CONNECT" && r.host.ends_with("opencode.ai"))
        .count();

    let _ = harness.close_session(&session);
    let evidence = harness.stop_attempt(&attempt).expect("stop_attempt");
    eprintln!("stop evidence: {}", evidence.detail);
    assert!(
        evidence.process_group_gone,
        "process group must be gone: {}",
        evidence.detail
    );
    assert!(!pid_alive(root_pid), "server pid {root_pid} survived stop");
    assert!(
        group_members(root_pid).is_empty(),
        "no confined process may survive stop"
    );

    // The proxy is no longer a security boundary (sandbox removal §7.2), but a
    // direct-network denial is no longer asserted here.

    // 3. User OpenCode state untouched.
    assert_eq!(
        mtime(&user_config),
        before_config,
        "user ~/.config/opencode was modified"
    );
    assert_eq!(
        mtime(&user_data),
        before_data,
        "user ~/.local/share/opencode was modified"
    );

    proxy.stop().await;

    // --- verdicts ----------------------------------------------------------
    assert!(
        provider_tunnels > 0,
        "no allowlisted provider CONNECT was tunnelled; proxy log: {requests:?}"
    );
    match &state {
        OperationState::Completed { usage, result_ref } => {
            eprintln!("completed: {result_ref} usage={usage:?}");
            assert!(
                usage.tokens_in > 0 || usage.tokens_out > 0,
                "completed turn must carry non-zero usage: {usage:?}"
            );
        }
        other => panic!(
            "turn must complete through the proxy, got {}",
            other.label()
        ),
    }

    eprintln!("proxy live smoke PASS: root {}", root.display());
}
