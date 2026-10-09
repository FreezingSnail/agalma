//! Gated live smoke: a Seatbelt-confined OpenCode turn reaches the free model
//! **through the parent-owned loopback proxy**.
//!
//! Background (M0.6): the worker confine profile denies arbitrary network, so
//! the confined harness cannot reach the provider directly. The
//! [`agalma_conductor::provider_proxy`] parent proxy is the fix; this test
//! proves it end to end.
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
//! 1. the confined turn reaches a terminal `Completed` with usage or a result;
//! 2. the proxy log shows an allowlisted `CONNECT` (`*.opencode.ai`) was the
//!    path used;
//! 3. a confined direct-network probe (`curl http://1.1.1.1/`) still fails —
//!    the profile boundary is intact, the proxy is the only egress;
//! 4. the user's OpenCode state is untouched.
//!
//! All scratch state lives under `target/test-runs/proxy-live-*` (never `/tmp`).

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_conductor::provider_proxy::{ProviderProxy, ProxyConfig};
use agalma_contracts::harness::{
    CreateSessionRequest, HarnessApi, OperationHandle, OperationState, RunTurnRequest,
    StartAttemptRequest,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId, OperationId};
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi};
use agalma_harness_opencode::{OpenCodeHarness, OpenCodeHarnessConfig};
use agalma_sandbox::{SeatbeltSandbox, PRODUCT_PROFILE};

fn live_enabled() -> bool {
    std::env::var("AGALMA_PROXY_LIVE").ok().as_deref() == Some("1")
}

fn run_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-runs");
    let root = base.join(format!("proxy-live-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("create run root");
    root
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

/// Launch a confined `curl` probe against a raw external IP and return its
/// recorded exit code. A non-zero code means the direct network stayed denied.
fn confined_direct_network_exit_code(root: &Path) -> i32 {
    let attempt = root.join("probe-attempt");
    let tmp = attempt.join("tmp");
    let work = attempt.join("work");
    for dir in [&attempt, &tmp, &work] {
        fs::create_dir_all(dir).expect("create probe dir");
    }
    // The profile's `subpath` filter matches resolved paths; the Seatbelt
    // params must be canonical (the adapter's `canonical_dir` does the same).
    let attempt = fs::canonicalize(&attempt).expect("resolve probe attempt");
    let tmp = fs::canonicalize(&tmp).expect("resolve probe tmp");
    let work = fs::canonicalize(&work).expect("resolve probe work");
    let protected = fs::canonicalize(root.join("protected")).expect("resolve protected");
    let sock = fs::canonicalize(root.join("sock")).expect("resolve sock");
    let profile = fs::canonicalize(root.join("worker.sb")).expect("resolve profile");
    let code_file = attempt.join("net-code.txt");
    let _ = fs::remove_file(&code_file);

    let mut sandbox = SeatbeltSandbox::new();
    let command = format!(
        "curl -sS --max-time 5 -o /dev/null http://1.1.1.1/ ; echo $? > {}",
        code_file.display()
    );
    let mut env = std::collections::BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
    );
    env.insert("HOME".to_string(), work.to_string_lossy().into_owned());
    env.insert("TMPDIR".to_string(), tmp.to_string_lossy().into_owned());
    let spec = LaunchSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), command],
        cwd: attempt.to_string_lossy().into_owned(),
        env,
        profile: profile.to_string_lossy().into_owned(),
        attempt: AttemptId::derive(2),
        attempt_dir: attempt.to_string_lossy().into_owned(),
        protected_dir: protected.to_string_lossy().into_owned(),
        sock_dir: sock.to_string_lossy().into_owned(),
        extra_ro: protected.to_string_lossy().into_owned(),
    };
    let child = sandbox.launch(spec).expect("launch confined network probe");
    let deadline = Instant::now() + Duration::from_secs(20);
    while sandbox.alive(&child).unwrap_or(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = sandbox.terminate(&child);
    fs::read_to_string(&code_file)
        .expect("confined probe must record an exit code (profile/exec worked)")
        .trim()
        .parse::<i32>()
        .expect("exit code is an integer")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn confined_turn_reaches_provider_through_parent_proxy() {
    if !live_enabled() {
        eprintln!("proxy live smoke skipped: set AGALMA_PROXY_LIVE=1 to run");
        return;
    }

    let root = run_root();
    let attempt_dir = root.join("attempt");
    let protected = root.join("protected");
    let sock = root.join("sock");
    let workspace = attempt_dir.join("work");
    for dir in [&attempt_dir, &protected, &sock, &workspace] {
        fs::create_dir_all(dir).expect("create attempt dir");
    }
    let profile = root.join("worker.sb");
    fs::write(&profile, PRODUCT_PROFILE).expect("write profile");

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

    let mut config = OpenCodeHarnessConfig::new(&profile, &attempt_dir, &protected, &sock);
    config.proxy_url = Some(proxy.url());
    if let Some(extra) = std::env::var_os("AGALMA_HARNESS_EXTRA_RO") {
        config.extra_ro = Some(PathBuf::from(extra));
    }
    let mut harness = OpenCodeHarness::new(Box::new(SeatbeltSandbox::new()), config);

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

    // 3. Direct external network is still denied inside the same profile.
    let exit_code = confined_direct_network_exit_code(&root);
    eprintln!("confined direct-network curl exit code: {exit_code}");
    assert_ne!(
        exit_code, 0,
        "confined direct network must stay denied (curl succeeded unproxied)"
    );

    // 4. User OpenCode state untouched.
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
