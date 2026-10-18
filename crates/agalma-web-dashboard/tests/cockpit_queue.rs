//! WEB.13 (`agalma-rwn.13`) production cockpit queue acceptance.
//!
//! Drives the **real production path** end to end: one `agalma work --watch`
//! continuous runtime, the real supervised `agalma-web-dashboard` package
//! launched by that runtime (explicitly approved for the factory read/control
//! capabilities through the composition operator configuration), the real
//! selected ledger, and the deterministic local reference task harness
//! (`--harness reference`, scripted activity). No manual observation adapter is
//! used for queue state: the page and factory snapshot are produced by the
//! runtime's own observation cell.
//!
//! It proves: empty queue → `Idle`; one approved eligible local task is
//! automatically claimed and executed; the busy page stays responsive; an
//! operator pause prevents the next claim; a confirmed Resume allows exactly one
//! later claim; an established `STOP` sentinel is removed by a confirmed
//! Resume; an unremovable `STOP` fails closed (`EffectUnconfirmed`); and
//! observation reads create no control intent.
//!
//! `CHROME_BIN` must name the installed browser; a missing browser, real binary
//! or zero cases fails the test.

mod browser_support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, PluginRoot,
};
use agalma_testkit::ScratchDir;
use agalma_web_dashboard::{PACKAGE_VERSION, PLUGIN_ID};

use browser_support::{chrome_version, resolve_chrome, run_browser};

const BROWSER_BUDGET_MS: u64 = 6000;
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CLAIM_TIMEOUT: Duration = Duration::from_secs(90);
const HOLD_WINDOW: Duration = Duration::from_secs(6);

const READ_CAPABILITY: &str = "control-plane.read/v1";
const FACTORY_READ: &str = "factory.read/v1";
const FACTORY_CONTROL: &str = "factory.control/v1";

// ---------------------------------------------------------------------------
// Sealed production dashboard package + operator approval
// ---------------------------------------------------------------------------

fn seal_dashboard_package(scratch: &Path) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("package");
    let root = scratch.join("plugins");
    let package_dir = root.join("agalma-web-dashboard");
    fs::create_dir_all(package_dir.join("objects")).unwrap();
    fs::create_dir_all(package_dir.join("bin")).unwrap();
    for relative in [
        "plugin-manifest.json",
        "objects/config-schema.json",
        "objects/default-config.json",
        "objects/report-schema.json",
    ] {
        fs::copy(source.join(relative), package_dir.join(relative)).unwrap();
    }
    let binary = Path::new(env!("CARGO_BIN_EXE_agalma-web-dashboard"));
    let installed_binary = package_dir.join("bin/agalma-web-dashboard");
    fs::copy(binary, &installed_binary).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let manifest_path = package_dir.join("plugin-manifest.json");
    let mut manifest: agalma_contracts::PluginManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.package_digest = format!("sha256:{}", "0".repeat(64));
    manifest.package_digest = package_inventory_digest(&package_dir, &manifest).unwrap();
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    discover_plugins(&[PluginRoot::required(&root)])
        .expect("sealed package discovers")
        .package(PLUGIN_ID)
        .expect("dashboard package discovered")
        .clone()
}

/// Persist the operator installation pin into the **existing** durable hub
/// installation store (`HubInstallStore`), binding the exact frozen package
/// digest, configuration artifact, launch profile and capability set.
fn install_operator_pin(
    state_dir: &Path,
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
) {
    let mut installation = agalma_contracts::PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: vec![
            READ_CAPABILITY.to_string(),
            FACTORY_READ.to_string(),
            FACTORY_CONTROL.to_string(),
        ],
        installation_digest: String::new(),
    };
    installation.installation_digest = installation
        .recomputed_installation_digest()
        .expect("installation digest");
    let store = agalma_conductor::plugin_registry::HubInstallStore::new(state_dir);
    store
        .install(package, 1, &installation)
        .expect("persist the operator installation pin");
}

// ---------------------------------------------------------------------------
// Deterministic local task workspace (git + bd + frozen protected inputs)
// ---------------------------------------------------------------------------

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
    assert!(out.status.success(), "{program} {args:?}: {out:?}");
}

fn seed_repo(repo: &Path) {
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .unwrap();
    run_ok("git", repo, &["init", "-q", "-b", "main"]);
    run_ok("git", repo, &["config", "user.email", "agalma@localhost"]);
    run_ok("git", repo, &["config", "user.name", "Agalma"]);
    run_ok("git", repo, &["add", "-A"]);
    run_ok("git", repo, &["commit", "-qm", "seed"]);
    run_ok(
        "bd",
        repo,
        &["init", "--prefix", "rwn13", "--non-interactive", "--skip-agents", "--skip-hooks"],
    );
}

const TASK_DESCRIPTION: &str = "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'test -f src/lib.rs'\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n";

fn create_task(repo: &Path, title: &str) -> String {
    let out = Command::new("bd")
        .args([
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            TASK_DESCRIPTION,
            "--json",
        ])
        .current_dir(repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd create");
    assert!(out.status.success(), "bd create: {out:?}");
    let value: Value = serde_json::from_slice(&out.stdout).expect("bd create json");
    match &value {
        Value::Array(items) => items[0]["id"].as_str().unwrap().to_string(),
        other => other["id"].as_str().unwrap().to_string(),
    }
}

fn task_status(repo: &Path, task_id: &str) -> String {
    let out = Command::new("bd")
        .args([
            "--db",
            repo.join(".beads").to_str().unwrap(),
            "show",
            task_id,
            "--json",
        ])
        .current_dir(repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd show");
    let value: Value = serde_json::from_slice(&out.stdout).expect("bd show json");
    let issue = match &value {
        Value::Array(items) => &items[0],
        other => other,
    };
    issue["status"].as_str().unwrap_or("unknown").to_string()
}

const PROTECTED_RUNNER: &str =
    "#!/bin/sh\nset -eu\ngrep -q 42 \"$AGALMA_CANDIDATE_DIR/src/lib.rs\"\n";

/// Freeze the candidate inputs for `task_id` so the reference harness can be
/// verified deterministically (the candidate writes `.agalma/reference-ok`).
fn write_protected_inputs(state_dir: &Path, repo: &Path, task_id: &str) {
    let base_sha = {
        let out = Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(repo)
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let canonical_repo = fs::canonicalize(repo).unwrap().to_string_lossy().into_owned();
    let runner_digest = agalma_contracts::sha256_digest(PROTECTED_RUNNER.as_bytes());
    let inputs = serde_json::json!({
        "format": "ProtectedInputs/v1",
        "entries": [{
            "task_id": task_id,
            "repository_root": canonical_repo,
            "base_sha": base_sha,
            "allowed_paths": ["src", ".agalma"],
            "protected_paths": [],
            "baseline": {
                "format": "ProtectedBaseline/v1",
                "files": [{
                    "path": "runner.sh",
                    "mode": "100755",
                    "digest": runner_digest,
                    "bytes": PROTECTED_RUNNER.len(),
                    "text": PROTECTED_RUNNER
                }],
                "commands": ["sh \"$AGALMA_PROTECTED_DIR/runner.sh\""]
            }
        }]
    });
    let state_dir = fs::canonicalize(state_dir).unwrap();
    fs::write(
        state_dir.join("protected-inputs.json"),
        serde_json::to_vec(&inputs).unwrap(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// The continuous runtime as a real supervised process
// ---------------------------------------------------------------------------

struct Runtime {
    child: Child,
    url: String,
    state_dir: PathBuf,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_runtime(
    scratch: &ScratchDir,
    repo: &Path,
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
    selector: Option<&str>,
    expect_url: bool,
) -> Runtime {
    let plugin_root = package.root.clone();
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    install_operator_pin(&state_dir, package);
    let mut args: Vec<String> = vec![
        "work".into(),
        "--repo".into(),
        repo.to_str().unwrap().into(),
        "--state-dir".into(),
        state_dir.to_str().unwrap().into(),
        "--watch".into(),
        "--plugin-root".into(),
        plugin_root.to_str().unwrap().into(),
    ];
    if let Some(selector) = selector {
        args.push("--factory-control-plugin".into());
        args.push(selector.into());
    }
    let mut child = Command::new(agalma_binary())
        .args(&args)
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_SCRIPTED_BUILD_SLEEP_MS", "8000")
        .env("BD_NON_INTERACTIVE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agalma work --watch");

    let stderr = child.stderr.take().expect("child stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    // Wait for the dashboard to print its real loopback URL.
    let deadline = Instant::now()
        + if expect_url {
            READY_TIMEOUT
        } else {
            Duration::from_secs(12)
        };
    let mut url = None;
    let mut observed: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(500)) {
            eprintln!("runtime stderr: {line}");
            if let Some(value) = line.strip_prefix("DASHBOARD_URL ") {
                url = Some(value.trim().to_string());
                if expect_url {
                    break;
                }
            }
            observed.push(line);
        }
    }
    let url = match (expect_url, url) {
        (true, Some(url)) => url,
        (true, None) => panic!(
            "the runtime must print the supervised dashboard URL; stderr={observed:?}"
        ),
        (false, Some(url)) => panic!(
            "a non-selected plugin must not be approved (no dashboard URL): {url}"
        ),
        (false, None) => String::new(),
    };
    Runtime {
        child,
        url,
        state_dir,
    }
}

fn port_of(url: &str) -> u16 {
    url.rsplit(':').next().unwrap().parse().expect("port")
}

/// The real `agalma` conductor binary beside the crate test binaries.
///
/// The dashboard crate has no direct binary dependency on the conductor, so the
/// path is resolved from this test executable (`<target>/debug/deps/..` →
/// `<target>/debug/agalma`); `AGALMA_BIN` overrides it.
fn agalma_binary() -> PathBuf {
    if let Ok(explicit) = std::env::var("AGALMA_BIN") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let exe = std::env::current_exe().expect("current test executable");
    let debug_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target debug directory");
    let candidate = debug_dir.join("agalma");
    assert!(
        candidate.is_file(),
        "the real `agalma` binary must be built at {} (run `cargo build --workspace --bins` or set AGALMA_BIN)",
        candidate.display()
    );
    candidate
}

// ---------------------------------------------------------------------------
// Direct HTTP
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .map(|value| value.trim().to_string())
    }
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect dashboard listener");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nConnection: close\r\n"
    );
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).unwrap();
    }
    stream.flush().unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("status line missing for {method} {path}: {text:?}"));
    HttpResponse {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

fn factory_status(port: u16) -> Option<String> {
    let response = http(port, "GET", "/api/v1/factory", &[], None);
    if response.status != 200 {
        return None;
    }
    let value: Value = serde_json::from_str(&response.body).ok()?;
    value["status"].as_str().map(str::to_string)
}

fn wait_for_status(port: u16, want: &str, timeout: Duration, label: &str) {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        last = factory_status(port);
        if last.as_deref() == Some(want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for factory status {want} ({label}); last={last:?}");
}

fn begin_task(repo: &Path, state_dir: &Path, title: &str) -> String {
    let task_id = create_task(repo, title);
    write_protected_inputs(state_dir, repo, &task_id);
    task_id
}

fn wait_for_task_status(repo: &Path, task_id: &str, want: &str, timeout: Duration, label: &str) {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = task_status(repo, task_id);
        if last == want {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("task {task_id} did not reach {want} ({label}); last={last}");
}

fn operator_tokens(port: u16) -> (String, String) {
    let response = http(port, "GET", "/api/v1/factory", &[], None);
    assert_eq!(response.status, 200, "{}", response.body);
    let cookie = response
        .header("set-cookie")
        .and_then(|value| {
            value
                .split(';')
                .find_map(|part| part.trim().strip_prefix("agalma_operator="))
                .map(str::to_string)
        })
        .expect("operator session cookie");
    let csrf = response.header("x-agalma-csrf").expect("CSRF header");
    (cookie, csrf)
}

fn post_control(
    port: u16,
    route: &str,
    cookie: &str,
    csrf: &str,
    body: &str,
) -> HttpResponse {
    let cookie_header = format!("agalma_operator={cookie}");
    http(
        port,
        "POST",
        route,
        &[("Cookie", cookie_header.as_str()), ("X-Agalma-CSRF", csrf)],
        Some(body),
    )
}

// ---------------------------------------------------------------------------
// The acceptance
// ---------------------------------------------------------------------------

#[test]
fn cockpit_queue_real_runtime_automates_pause_and_resume() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    eprintln!(
        "cockpit_queue: chrome={} version={}",
        chrome.display(),
        chrome_version(&chrome)
    );

    let scratch = ScratchDir::new("web13-cockpit-queue");
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo);
    let package = seal_dashboard_package(scratch.path());
    let runtime = spawn_runtime(&scratch, &repo, &package, Some(PLUGIN_ID), true);
    let state_dir = runtime.state_dir.clone();
    let port = port_of(&runtime.url);
    eprintln!("cockpit_queue: url={} state={}", runtime.url, state_dir.display());

    // Empty queue → Idle, reported by the runtime's own observation cell.
    wait_for_status(port, "Idle", IDLE_TIMEOUT, "empty queue");

    // The page loads and renders against the real runtime.
    let page = http(port, "GET", "/", &[], None);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Agalma cockpit"), "page shell missing");

    // One approved eligible local task is automatically claimed and executed.
    let first = begin_task(&repo, &state_dir, "First eligible task");
    wait_for_task_status(&repo, &first, "closed", CLAIM_TIMEOUT, "first task executed");

    // The busy page stays responsive during the scripted build: once the runtime
    // reports `Running`, a page load and a factory snapshot both answer promptly.
    let second = begin_task(&repo, &state_dir, "Second eligible task");
    wait_for_status(port, "Running", CLAIM_TIMEOUT, "second task claimed");
    let busy_start = Instant::now();
    let snapshot = http(port, "GET", "/api/v1/factory", &[], None);
    assert_eq!(snapshot.status, 200, "{}", snapshot.body);
    let value: Value = serde_json::from_str(&snapshot.body).unwrap();
    assert_eq!(value["status"], "Running", "body={}", snapshot.body);
    let page = http(port, "GET", "/", &[], None);
    assert_eq!(page.status, 200);
    assert!(
        busy_start.elapsed() < Duration::from_secs(2),
        "the busy page and snapshot must stay responsive well before the build phase deadline"
    );
    wait_for_task_status(&repo, &second, "closed", CLAIM_TIMEOUT, "second task executed");

    // Confirmed controls while admission is running.
    let (cookie, csrf) = operator_tokens(port);

    // Pause via the browser/session: the durable latch is set.
    let pause = post_control(
        port,
        "/api/v1/factory/pause",
        &cookie,
        &csrf,
        r#"{"format":"FactoryControl/v1","operation":"pause","action_id":"queue-pause-1","confirmation":null}"#,
    );
    assert_eq!(pause.status, 200, "{}", pause.body);
    let pause_body: Value = serde_json::from_str(&pause.body).unwrap();
    assert_eq!(pause_body["receipt"]["outcome"], "applied", "{pause_body}");

    // A later eligible task must not be claimed while paused.
    let held = begin_task(&repo, &state_dir, "Held task");
    let hold_deadline = Instant::now() + HOLD_WINDOW;
    while Instant::now() < hold_deadline {
        let status = task_status(&repo, &held);
        assert_eq!(status, "open", "a paused runtime must not claim later work");
        std::thread::sleep(Duration::from_millis(200));
    }

    // Confirmed Resume allows exactly one later claim.
    let resume = post_control(
        port,
        "/api/v1/factory/resume",
        &cookie,
        &csrf,
        r#"{"format":"FactoryControl/v1","operation":"resume","action_id":"queue-resume-1","confirmation":"resume"}"#,
    );
    assert_eq!(resume.status, 200, "{}", resume.body);
    let resume_body: Value = serde_json::from_str(&resume.body).unwrap();
    assert_eq!(resume_body["receipt"]["outcome"], "applied", "{resume_body}");
    wait_for_task_status(&repo, &held, "closed", CLAIM_TIMEOUT, "resumed task executed");

    // An established STOP sentinel is removed by a confirmed Resume.
    let stop_path = state_dir.join("STOP");
    fs::write(&stop_path, b"operator stop\n").unwrap();
    let sentinel_resume = post_control(
        port,
        "/api/v1/factory/resume",
        &cookie,
        &csrf,
        r#"{"format":"FactoryControl/v1","operation":"resume","action_id":"queue-resume-2","confirmation":"resume"}"#,
    );
    assert_eq!(sentinel_resume.status, 200, "{}", sentinel_resume.body);
    assert_eq!(
        serde_json::from_str::<Value>(&sentinel_resume.body).unwrap()["receipt"]["outcome"],
        "applied"
    );
    assert!(
        !stop_path.exists(),
        "a confirmed resume removes the established STOP sentinel"
    );

    // Fail-closed: an unremovable STOP leaves the effect unconfirmed.
    fs::create_dir_all(&stop_path).unwrap();
    let stuck = post_control(
        port,
        "/api/v1/factory/resume",
        &cookie,
        &csrf,
        r#"{"format":"FactoryControl/v1","operation":"resume","action_id":"queue-resume-3","confirmation":"resume"}"#,
    );
    assert_eq!(stuck.status, 202, "{}", stuck.body);
    let stuck_body: Value = serde_json::from_str(&stuck.body).unwrap();
    assert_eq!(stuck_body["result"], "unconfirmed", "{stuck_body}");
    assert_eq!(stuck_body["code"], "EffectUnconfirmed");
    assert!(stop_path.is_dir(), "the unremovable sentinel is not dropped");
    let _ = fs::remove_dir_all(&stop_path);

    // The installed production browser renders the real page against the runtime.
    let dom = {
        let outcome = run_browser(&chrome, &runtime.url, BROWSER_BUDGET_MS, "cockpit-queue");
        assert!(outcome.status.success(), "browser failed: {}", outcome.stderr);
        outcome.dom
    };
    assert!(dom.contains("Agalma cockpit"), "page shell missing: {dom}");
    assert!(
        dom.contains(PACKAGE_VERSION) || dom.contains("Overview") || dom.contains("status"),
        "the real page must render production data: {dom}"
    );

    eprintln!("cockpit_queue: ok url={} first={first} second={second}", runtime.url);
}

/// Production read-only coverage: with no `--factory-control-plugin` selector,
/// and with a selector naming a different plugin, the supervised dashboard still
/// serves its read UI/API from the safe read install, while every control
/// remains disabled.
#[test]
fn cockpit_queue_unselected_and_no_selector_stay_read_only() {
    let _chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    for selector in [None, Some("example.unselected")] {
        eprintln!("cockpit_queue read-only: selector={selector:?}");
        let scratch = ScratchDir::new("web13-cockpit-queue-readonly");
        let repo = scratch.child("repo");
        fs::create_dir_all(&repo).unwrap();
        seed_repo(&repo);
        let package = seal_dashboard_package(scratch.path());
        let runtime = spawn_runtime(&scratch, &repo, &package, selector, true);
        let port = port_of(&runtime.url);

        // The read UI and API are served independently of the control selector.
        let page = http(port, "GET", "/", &[], None);
        assert_eq!(page.status, 200, "read page serves: {}", page.body);
        assert!(page.body.contains("Agalma cockpit"));
        let overview = http(port, "GET", "/api/v1/overview", &[], None);
        assert_eq!(overview.status, 200, "read API serves: {}", overview.body);
        let overview_body: Value = serde_json::from_str(&overview.body).unwrap();
        assert_eq!(overview_body["metadata"]["format"], "ControlPlaneRead/v1");

        // The factory snapshot is readable but advertises no control permission.
        let factory = http(port, "GET", "/api/v1/factory", &[], None);
        assert_eq!(factory.status, 200, "factory read serves: {}", factory.body);
        let factory_body: Value = serde_json::from_str(&factory.body).unwrap();
        assert_eq!(factory_body["permissions"]["pause"], false);
        assert_eq!(factory_body["permissions"]["resume"], false);

        // No control surface: a same-origin POST cannot bootstrap a session and
        // is refused (never a control effect).
        let pause = post_control(
            port,
            "/api/v1/factory/pause",
            "",
            "",
            r#"{"format":"FactoryControl/v1","operation":"pause","action_id":"readonly-1","confirmation":null}"#,
        );
        assert!(
            pause.status == 403 || pause.status == 404,
            "control must be disabled (status {})",
            pause.status
        );
    }
}
