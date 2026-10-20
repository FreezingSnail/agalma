//! WEB.6 production first-launch acceptance (`agalma-rwn.6`).
//!
//! This is the M1 delivery proof, not the M2 automatic-worker acceptance. It
//! drives the **real production path** end to end:
//!
//! 1. a real isolated ledger is established with the production selected
//!    `LedgerHandle` (`LedgerHandle::in_process`);
//! 2. the real `agalma-web-dashboard` executable is sealed as an immutable
//!    installed package (`package_inventory_digest` over the committed package
//!    plus the built binary);
//! 3. the production `PluginHost::assemble_with_ledger` assembles the real hub,
//!    installs the pinned package, issues a launch-scoped read grant, serves the
//!    real `control-plane.read/v1` router, and supervises the child;
//! 4. the child's reported loopback URL is opened in the **installed** Chrome
//!    via the shared harness;
//! 5. the real page renders production overview/component data, the factory
//!    endpoint is explicitly unavailable in M1 and controls are disabled;
//! 6. losing the hub read source surfaces as an explicit unavailable read;
//! 7. viewing is observation-only: the child holds exactly the read capability,
//!    no control route exists, and no control intent or task claim is created.
//!
//! `CHROME_BIN` must name the installed browser; a missing browser, real binary
//! or zero cases fails the test.

mod browser_support;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use agalma_conductor::composition::{InMemoryInstallations, PluginHost};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot,
};
use agalma_conductor::plugin_registry::DesiredState;
use agalma_conductor::plugin_router::{serve_hub, RpcHealthProbe};
use agalma_conductor::plugin_supervisor::{Clock, SystemClock};
use agalma_contracts::{
    canonical_digest, HealthState, PluginHealth, PluginInstallation, PluginManifest,
    FACTORY_CONTROL_CAPABILITY,
};
use agalma_ipc::PluginClient;
use agalma_testkit::ScratchDir;
use agalma_web_dashboard::reports::{REPORT_SCHEMA_NAMESPACE, REPORT_SCHEMA_VERSION};
use agalma_web_dashboard::{
    PACKAGE_VERSION, PLUGIN_ID, READ_CAPABILITY, REPORT_PUBLISH_CAPABILITY,
};

use browser_support::{capture_screenshot, chrome_version, resolve_chrome, run_browser};

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_BUDGET_MS: u64 = 6000;

// ---------------------------------------------------------------------------
// Package sealing and installation (production package primitives)
// ---------------------------------------------------------------------------

fn seal_dashboard_package(scratch: &Path) -> DiscoveredPackage {
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
    let mut manifest: PluginManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.package_digest = format!("sha256:{}", "0".repeat(64));
    manifest.package_digest = package_inventory_digest(&package_dir, &manifest).unwrap();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    discover_plugins(&[PluginRoot::required(&root)])
        .expect("sealed package discovers")
        .package(PLUGIN_ID)
        .expect("dashboard package discovered")
        .clone()
}

fn installation_for(package: &DiscoveredPackage) -> PluginInstallation {
    installation_with(package, &[READ_CAPABILITY, REPORT_PUBLISH_CAPABILITY])
}

fn installation_with(package: &DiscoveredPackage, capabilities: &[&str]) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        // The operator-managed explicit installation pin approves exactly the
        // observation-only read capability plus the separate publication
        // authority. No control capability is approved.
        approved_capabilities: capabilities.iter().map(|value| value.to_string()).collect(),
        installation_digest: String::new(),
    };
    installation.installation_digest = installation.recomputed_installation_digest().unwrap();
    installation
}

// ---------------------------------------------------------------------------
// Real production launch
// ---------------------------------------------------------------------------

/// A running production dashboard with a real ledger and hub read service.
struct ProductionLaunch {
    host: PluginHost,
    plugin_socket: PathBuf,
    hub_task: tokio::task::JoinHandle<()>,
}

impl Drop for ProductionLaunch {
    fn drop(&mut self) {
        self.hub_task.abort();
        let _ = self.host.shutdown();
    }
}

fn launch_production_dashboard(
    runtime: &tokio::runtime::Runtime,
    scratch: &ScratchDir,
    package: &DiscoveredPackage,
) -> ProductionLaunch {
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    // Real isolated, established ledger through the production selected handle.
    let ledger = LedgerHandle::in_process(&state_dir).expect("establish isolated ledger");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(package.root.clone())],
        once: false,
    };
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(package));
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| SystemClock.now_ms());

    let mut host = PluginHost::assemble_with_ledger(&config, &installations, ledger, clock)
        .expect("assemble production hub with the real ledger");

    // Serve the real hub capability router (the dashboard's read binding).
    let hub_socket = host.hub_socket.clone();
    let router = host.router.clone();
    let hub_task = runtime.spawn(async move {
        let _ = serve_hub(&hub_socket, router).await;
    });
    wait_for_path(&host.hub_socket, "hub socket");

    host.set_desired(PLUGIN_ID, DesiredState::Running)
        .expect("record desired running state");
    let request = host
        .launch_request(PLUGIN_ID)
        .expect("production host builds the installed launch request");
    let plugin_socket = request.socket.clone();
    host.supervisor
        .spawn(&mut host.registry, &request)
        .expect("real dashboard executable passes the READY identity gate");
    assert!(
        host.confirm_ready_health(PLUGIN_ID, &mut RpcHealthProbe::new(host.state_dir.clone()))
            .expect("production pinned health gate"),
        "the production dashboard must reach Ready"
    );

    ProductionLaunch {
        host,
        plugin_socket,
        hub_task,
    }
}

/// Launch the installed dashboard with only the separate publication grant.
/// The listener is healthy, the read binding is intentionally unavailable, and
/// the owner still publishes its true degraded readiness.
fn launch_report_only_dashboard(
    runtime: &tokio::runtime::Runtime,
    scratch: &ScratchDir,
    package: &DiscoveredPackage,
) -> ProductionLaunch {
    let state_dir = scratch.child("report-only-state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).expect("establish isolated ledger");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(package.root.clone())],
        once: false,
    };
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_with(package, &[REPORT_PUBLISH_CAPABILITY]));
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| SystemClock.now_ms());
    let mut host = PluginHost::assemble_with_ledger(&config, &installations, ledger, clock)
        .expect("assemble production hub for report-only launch");
    let hub_socket = host.hub_socket.clone();
    let router = host.router.clone();
    let hub_task = runtime.spawn(async move {
        let _ = agalma_conductor::plugin_router::serve_hub(&hub_socket, router).await;
    });
    wait_for_path(&host.hub_socket, "report-only hub socket");
    host.set_desired(PLUGIN_ID, DesiredState::Running)
        .expect("record desired running state");
    let request = host
        .launch_request(PLUGIN_ID)
        .expect("production host builds the report-only launch request");
    let plugin_socket = request.socket.clone();
    host.supervisor
        .spawn(&mut host.registry, &request)
        .expect("report-only child passes the READY identity gate");
    ProductionLaunch {
        host,
        plugin_socket,
        hub_task,
    }
}

fn wait_for_degraded_health(
    runtime: &tokio::runtime::Runtime,
    plugin_socket: &Path,
) -> PluginHealth {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let (health, capabilities) = dashboard_health(runtime, plugin_socket);
        if health.state == HealthState::Degraded {
            assert_eq!(capabilities, vec![REPORT_PUBLISH_CAPABILITY.to_string()]);
            return health;
        }
        assert!(
            Instant::now() < deadline,
            "report-only dashboard never reported degraded: {health:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_path(path: &Path, label: &str) {
    let deadline = Instant::now() + READY_TIMEOUT;
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn dashboard_health(
    runtime: &tokio::runtime::Runtime,
    plugin_socket: &Path,
) -> (PluginHealth, Vec<String>) {
    let mut client = runtime
        .block_on(PluginClient::connect(plugin_socket))
        .expect("connect the real dashboard plugin RPC");
    let identity = runtime
        .block_on(client.handshake(PLUGIN_ID))
        .expect("Describe identity agreement");
    let health_value = runtime.block_on(client.health()).expect("plugin.Health");
    let health: PluginHealth = serde_json::from_value(health_value).expect("PluginHealth/v1");
    health.version_gate().expect("health version gate");
    (health, identity.capabilities.into_iter().collect())
}

fn detail_field<'a>(health: &'a PluginHealth, name: &str) -> Option<&'a str> {
    health
        .detail
        .split(';')
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
}

/// Wait for the real producer publication to reach the real hub ingestor.
fn wait_for_status_report(host: &PluginHost) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reports = host
            .reports
            .as_ref()
            .expect("real ledger-backed report ingestor");
        let reports = reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(view) = reports.view(PLUGIN_ID) {
            if let Some(entry) = view.entries.get(REPORT_SCHEMA_NAMESPACE) {
                assert_eq!(entry.schema_version, REPORT_SCHEMA_VERSION);
                return entry.payload.clone();
            }
        }
        drop(reports);
        assert!(
            Instant::now() < deadline,
            "the dashboard's real status publication never reached the hub"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Persistent evidence root outside any removable worktree.
fn evidence_dir() -> PathBuf {
    std::env::var_os("AGALMA_EVIDENCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/test-runs/agalma-51dg.4"))
}

fn capture_owner_screenshot(chrome: &Path, url: &str, name: &str) -> PathBuf {
    let path = evidence_dir().join("screenshots").join(name);
    let outcome = capture_screenshot(
        chrome,
        url,
        &path,
        BROWSER_BUDGET_MS,
        name,
        1280,
        900,
        "light",
    );
    assert!(
        outcome.status.success(),
        "screenshot {name} failed: {}",
        outcome.stderr
    );
    let bytes = fs::read(&path).expect("captured screenshot bytes");
    assert!(
        bytes.len() > 1000 && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "screenshot {name} is not a PNG"
    );
    eprintln!("first_launch_report: screenshot={}", path.display());
    path
}

// ---------------------------------------------------------------------------
// Direct HTTP (the dashboard listener is plain HTTP/1)
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

fn http_request(port: u16, method: &str, path: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect dashboard listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
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

fn unused_loopback_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

/// Percent-encode a component id the same way the browser route does.
fn encode_component_id(plugin_id: &str) -> String {
    let mut out = String::new();
    for byte in plugin_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn component_detail_body(port: u16, plugin_id: &str) -> Value {
    let path = format!(
        "/api/v1/components/{}?limit=50",
        encode_component_id(plugin_id)
    );
    let response = http_request(port, "GET", &path);
    assert_eq!(response.status, 200, "{}: {}", plugin_id, response.body);
    let body: Value = serde_json::from_str(&response.body).expect("detail json");
    assert_eq!(body["metadata"]["format"], "ControlPlaneRead/v1");
    body
}
// ---------------------------------------------------------------------------
// The acceptance
// ---------------------------------------------------------------------------

#[test]
fn first_launch_production_page_renders_real_data_and_zero_effects() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    eprintln!(
        "first_launch: chrome={} version={}",
        chrome.display(),
        chrome_version(&chrome)
    );

    let scratch = ScratchDir::new("web6-first-launch");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let package = seal_dashboard_package(scratch.path());
    let launched = launch_production_dashboard(&runtime, &scratch, &package);

    // The production child reported an actual loopback URL.
    let (health, capabilities) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(
        health.state,
        HealthState::Ready,
        "detail: {}",
        health.detail
    );
    assert_eq!(health.plugin_id, PLUGIN_ID);
    assert_eq!(health.binding.implementation_version, PACKAGE_VERSION);
    let url = detail_field(&health, "url")
        .expect("actual URL reported")
        .to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "url={url}");
    let port: u16 = url.rsplit(':').next().unwrap().parse().expect("port");
    eprintln!("first_launch: installed dashboard URL={url}");

    // Observation-only: read plus the separate publication authority, no
    // factory control and no unrelated capability.
    assert_eq!(
        capabilities,
        vec![
            READ_CAPABILITY.to_string(),
            REPORT_PUBLISH_CAPABILITY.to_string()
        ],
        "capabilities={capabilities:?}"
    );
    assert!(!capabilities.iter().any(|c| c == FACTORY_CONTROL_CAPABILITY));

    // Real production read routes answer with the real ledger projection.
    let overview = http_request(port, "GET", "/api/v1/overview");
    assert_eq!(overview.status, 200, "{}", overview.body);
    let overview_body: Value = serde_json::from_str(&overview.body).unwrap();
    assert_eq!(overview_body["metadata"]["format"], "ControlPlaneRead/v1");
    assert!(overview_body["executions"].is_array());
    assert!(overview_body["economics"].is_object());

    let components = http_request(port, "GET", "/api/v1/components?limit=50");
    assert_eq!(components.status, 200, "{}", components.body);
    let components_body: Value = serde_json::from_str(&components.body).unwrap();
    assert_eq!(components_body["metadata"]["format"], "ControlPlaneRead/v1");
    let entries = components_body["entries"].as_array().unwrap();
    assert!(
        entries.iter().any(|entry| entry["plugin_id"] == PLUGIN_ID),
        "the production page must show the real installed dashboard component: {components_body}"
    );

    // The supervised child's own readiness observation reached the real hub
    // ingestor through `hub.PublishReport`, and the generic component detail
    // projection serves that canonical producer envelope.
    let reported = wait_for_status_report(&launched.host);
    assert_eq!(reported["listener"], "ready", "report={reported}");
    assert_eq!(reported["binding"], "ready", "report={reported}");
    assert_eq!(reported["url"], url.as_str(), "report={reported}");
    let detail = component_detail_body(port, PLUGIN_ID);
    let report_entries = detail["entries"].as_array().expect("detail entries");
    let status_entry = report_entries
        .iter()
        .find(|entry| entry["schema_namespace"] == REPORT_SCHEMA_NAMESPACE)
        .unwrap_or_else(|| panic!("no dashboard status report entry: {detail}"));
    assert_eq!(status_entry["payload"], reported);
    assert_eq!(
        status_entry["payload_digest"],
        canonical_digest(&status_entry["payload"]).unwrap()
    );
    assert_eq!(status_entry["plugin_id"], PLUGIN_ID);
    assert_eq!(
        status_entry["binding"]["generation"],
        launched
            .host
            .pinned_binding(PLUGIN_ID)
            .unwrap()
            .generation
            .to_string()
    );
    assert_eq!(status_entry["durability"], "durable");
    assert!(report_entries
        .iter()
        .all(|entry| entry["plugin_id"] == PLUGIN_ID));

    capture_owner_screenshot(
        &chrome,
        &format!("{url}#/components"),
        "cw3-healthy-components.png",
    );
    let identity = serde_json::json!({
        "plugin_id": PLUGIN_ID,
        "package_version": PACKAGE_VERSION,
        "package_digest": package.package_digest,
        "manifest_report_schema": {
            "namespace": REPORT_SCHEMA_NAMESPACE,
            "version": REPORT_SCHEMA_VERSION,
            "digest": agalma_web_dashboard::report_schema_ref().schema.digest,
        },
        "binding_generation": launched.host.pinned_binding(PLUGIN_ID).unwrap().generation,
        "approved_capabilities": capabilities,
        "acknowledged_report": reported,
        "listener_url": url,
        "healthy_screenshot": "screenshots/cw3-healthy-components.png",
    });
    fs::create_dir_all(evidence_dir()).unwrap();
    fs::write(
        evidence_dir().join("cw3-identity.json"),
        serde_json::to_vec_pretty(&identity).unwrap(),
    )
    .unwrap();

    // No control route exists: viewing can never create a control intent.
    let pause = http_request(port, "POST", "/api/v1/factory/pause");
    assert!(
        pause.status == 404 || pause.status == 405,
        "there must be no control route, got {}",
        pause.status
    );

    // Open the real printed URL with the components route: the production page
    // must prove the installed dashboard component is rendered by the registry
    // page, not merely listed in an API response.
    let components_url = format!("{url}#/components");
    let outcome = run_browser(&chrome, &components_url, BROWSER_BUDGET_MS, "first-launch");
    assert!(
        outcome.status.success(),
        "browser failed: {}",
        outcome.stderr
    );
    let dom = &outcome.dom;
    assert!(dom.contains("Agalma cockpit"), "page shell missing: {dom}");
    assert!(
        dom.contains(PLUGIN_ID),
        "real component must render in the components page: {dom}"
    );
    // The factory runtime endpoint is not part of M1, so the page says so
    // honestly and never claims Idle/Running.
    assert!(
        dom.contains("Unavailable (endpoint absent)") || dom.contains("Unknown"),
        "runtime must be explicit and unavailable: {dom}"
    );
    assert!(
        !dom.contains("Idle") && !dom.contains("Running"),
        "runtime must not be fabricated: {dom}"
    );
    // Controls are disabled without a control binding.
    assert!(
        dom.contains("controls disabled"),
        "controls must be disabled: {dom}"
    );

    // The page performed only same-origin reads; a navigation/reload never POSTs.
    let page_again = http_request(port, "GET", "/");
    assert_eq!(page_again.status, 200);
    assert!(page_again.body.contains("Agalma cockpit"));

    // Zero new task claims / control intents: the read source is observational,
    // the child holds no control or queue capability, and no factory-control
    // intent route exists. Checked after the browser runs.
    let (_, capabilities_after) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(
        capabilities_after,
        vec![
            READ_CAPABILITY.to_string(),
            REPORT_PUBLISH_CAPABILITY.to_string()
        ]
    );
    assert!(
        launched.host.ledger().is_some(),
        "the real ledger stays attached and unmutated by viewing"
    );
    let action_lookup = http_request(port, "GET", "/api/v1/factory/actions/ui-probe");
    assert!(
        action_lookup.status == 404 || action_lookup.status == 405,
        "no factory action route may exist: {}",
        action_lookup.status
    );

    // Lose the hub read source: shutting the hub service down drops the socket
    // and every accepted connection, so the dashboard's next read fails. A read
    // source is never a healthy empty page.
    runtime.shutdown_background();
    let mut unavailable = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let read = http_request(port, "GET", "/api/v1/overview");
        if read.status == 503 {
            unavailable = Some(read);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let read =
        unavailable.expect("the lost read source must surface as 503, never a healthy empty page");
    assert!(read.body.contains("read_unavailable"), "body={}", read.body);

    // A fresh page against the degraded source renders explicit unknown or
    // unavailable data rather than healthy/empty data.
    let degraded = run_browser(&chrome, &url, BROWSER_BUDGET_MS, "first-launch-degraded");
    assert!(
        degraded.status.success(),
        "browser failed: {}",
        degraded.stderr
    );
    assert!(
        degraded.dom.contains("Agalma cockpit"),
        "dom={}",
        degraded.dom
    );
    assert!(
        degraded.dom.contains("Unavailable") || degraded.dom.contains("unknown"),
        "degraded page must be explicit: {}",
        degraded.dom
    );
    capture_owner_screenshot(&chrome, &url, "cw3-degraded-read-source.png");

    eprintln!(
        "first_launch: ok url={url} capabilities={capabilities:?} ledger_state={}",
        scratch.child("state").display()
    );
}

#[test]
fn first_launch_production_component_detail_uses_real_hub_and_zero_effects() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    eprintln!(
        "first_launch_detail: chrome={} version={}",
        chrome.display(),
        chrome_version(&chrome)
    );

    let scratch = ScratchDir::new("web6-first-launch-detail");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let package = seal_dashboard_package(scratch.path());
    let launched = launch_production_dashboard(&runtime, &scratch, &package);

    let (health, capabilities) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(
        health.state,
        HealthState::Ready,
        "detail: {}",
        health.detail
    );
    let url = detail_field(&health, "url")
        .expect("actual URL reported")
        .to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "url={url}");
    let port: u16 = url.rsplit(':').next().unwrap().parse().expect("port");
    assert_eq!(
        capabilities,
        vec![
            READ_CAPABILITY.to_string(),
            REPORT_PUBLISH_CAPABILITY.to_string()
        ]
    );
    let reported = wait_for_status_report(&launched.host);

    // The installed dashboard and the conductor's built-in harness must both be
    // present in the real live registry projection before detail routing.
    let components = http_request(port, "GET", "/api/v1/components?limit=50");
    assert_eq!(components.status, 200, "{}", components.body);
    let components_body: Value = serde_json::from_str(&components.body).unwrap();
    let listed: Vec<&str> = components_body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["plugin_id"].as_str())
        .collect();
    assert!(listed.contains(&PLUGIN_ID), "listed={listed:?}");
    assert!(listed.contains(&"component:harness"), "listed={listed:?}");
    let pinned_generation = launched
        .host
        .pinned_binding(PLUGIN_ID)
        .unwrap()
        .generation;
    let listed_entry = |id: &str| {
        components_body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["plugin_id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from the real list"))
    };
    // The real adapter must carry the evidence-bearing status projection for
    // both a discovered generation and a generation-less builtin.
    let dashboard_list_status = &listed_entry(PLUGIN_ID)["status"];
    assert_eq!(dashboard_list_status["origin"], "discovered");
    assert_eq!(
        dashboard_list_status["generation"],
        pinned_generation.to_string()
    );
    assert_eq!(dashboard_list_status["lifecycle_source"], "live-registry");
    assert!(
        ["installed", "starting", "ready", "draining", "failed", "retired"].contains(
            &dashboard_list_status["lifecycle"].as_str().unwrap_or("")
        ),
        "live supervisor lifecycle: {dashboard_list_status}"
    );
    assert!(
        ["fresh", "stale", "unavailable", "unknown"].contains(
            &dashboard_list_status["reporting"].as_str().unwrap_or("")
        ),
        "reporting state: {dashboard_list_status}"
    );
    if dashboard_list_status["health"].is_null() {
        assert!(dashboard_list_status["health_observed_at_unix_ms"].is_null());
    } else {
        assert!(dashboard_list_status["health_observed_at_unix_ms"].is_string());
    }
    let harness_list_status = &listed_entry("component:harness")["status"];
    assert_eq!(harness_list_status["origin"], "built-in");
    assert!(harness_list_status["generation"].is_null(), "no synthetic zero");
    assert!(harness_list_status["lifecycle"].is_null());
    assert_eq!(harness_list_status["lifecycle_source"], "none");
    assert!(harness_list_status["lifecycle_observed_at_unix_ms"].is_null());
    assert!(harness_list_status["health"].is_null());
    assert_eq!(harness_list_status["reporting"], "unavailable");

    // Known installed plugin detail through the real hub router and host.
    let dashboard = component_detail_body(port, PLUGIN_ID);
    assert_eq!(dashboard["binding"]["binding_id"], PLUGIN_ID);
    assert!(
        dashboard["descriptors"]
            .as_array()
            .is_some_and(|views| !views.is_empty()),
        "installed plugin declared views must project: {dashboard}"
    );
    assert!(
        dashboard["metadata"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["state"] != "unreadable"),
        "real host sources must not be unreadable: {dashboard}"
    );
    let entries = dashboard["entries"].as_array().expect("detail entries");
    let status_entry = entries
        .iter()
        .find(|entry| entry["schema_namespace"] == REPORT_SCHEMA_NAMESPACE)
        .unwrap_or_else(|| panic!("dashboard detail has no owner report: {dashboard}"));
    assert_eq!(status_entry["payload"], reported);
    assert_eq!(
        status_entry["binding"]["generation"],
        pinned_generation.to_string()
    );
    let detail_status = &dashboard["status"];
    assert_eq!(detail_status, dashboard_list_status, "list/detail status agree");
    assert_eq!(detail_status["generation"], pinned_generation.to_string());

    // The conductor built-in `component:harness` is a first-class colon identity
    // from the actual host live-component projection, not a per-id exemption.
    let harness_detail = component_detail_body(port, "component:harness");
    assert_eq!(harness_detail["binding"]["binding_id"], "component:harness");
    assert_eq!(harness_detail["binding"]["generation"], "0");
    assert!(
        harness_detail["metadata"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["source"] == "registry" && source["state"] == "available"),
        "harness must be an available registry identity: {harness_detail}"
    );
    assert_eq!(harness_detail["status"], *harness_list_status);

    // A valid but unknown id is honest 200 missing metadata, never 503/404.
    let unknown = component_detail_body(port, "missing:valid.component");
    assert!(unknown["entries"].as_array().unwrap().is_empty());
    assert!(
        unknown["metadata"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["source"] == "registry" && source["state"] == "missing"),
        "unknown id must report missing registry data: {unknown}"
    );

    // Real browser list: compiled assets render the status columns from the
    // actual adapter, including the builtin's honest no-report state.
    let list_browser = run_browser(
        &chrome,
        &format!("{url}#/components"),
        BROWSER_BUDGET_MS,
        "first-launch-status-list",
    );
    assert!(
        list_browser.status.success(),
        "browser failed: {}",
        list_browser.stderr
    );
    assert!(
        list_browser.dom.contains("Reported health")
            && list_browser.dom.contains("Reporting")
            && list_browser.dom.contains("Not reporting")
            && list_browser.dom.contains("component:harness"),
        "real list status rendering missing: {}",
        list_browser.dom
    );

    // Real browser: the encoded dashboard detail route renders production data.
    let dashboard_url = format!("{url}#/components/{}", encode_component_id(PLUGIN_ID));
    let dashboard_browser = run_browser(
        &chrome,
        &dashboard_url,
        BROWSER_BUDGET_MS,
        "first-launch-detail",
    );
    assert!(
        dashboard_browser.status.success(),
        "browser failed: {}",
        dashboard_browser.stderr
    );
    let dom = &dashboard_browser.dom;
    assert!(
        dom.contains("Component detail"),
        "detail page missing: {dom}"
    );
    assert!(dom.contains(PLUGIN_ID), "detail id missing: {dom}");
    assert!(dom.contains("Binding"), "binding section missing: {dom}");
    assert!(
        dom.contains("Declared views"),
        "declared views section missing: {dom}"
    );
    assert!(
        !dom.contains("not registered in the hub registry"),
        "a registered dashboard must not render the not-registered state: {dom}"
    );
    assert!(
        !dom.contains("detail read unavailable") && !dom.contains("read_unavailable"),
        "detail read must not be unavailable: {dom}"
    );
    assert!(
        dom.contains(REPORT_SCHEMA_NAMESPACE) && dom.contains("listener"),
        "the generic component detail must render the owner report: {dom}"
    );

    // The colon identity route must not collapse into the generic unavailable
    // error or an unknown-route notice.
    let harness_url = format!(
        "{url}#/components/{}",
        encode_component_id("component:harness")
    );
    let harness_browser = run_browser(
        &chrome,
        &harness_url,
        BROWSER_BUDGET_MS,
        "first-launch-detail-harness",
    );
    assert!(
        harness_browser.status.success(),
        "browser failed: {}",
        harness_browser.stderr
    );
    assert!(
        harness_browser.dom.contains("Component detail")
            && harness_browser.dom.contains("component:harness"),
        "colon identity detail route did not render: {}",
        harness_browser.dom
    );
    assert!(
        harness_browser.dom.contains("Binding")
            && harness_browser.dom.contains("Declared views")
            && harness_browser.dom.contains("No declared views.")
            && harness_browser.dom.contains("No report envelopes."),
        "builtin binding/descriptors/no-report state missing: {}",
        harness_browser.dom
    );
    assert!(
        !harness_browser
            .dom
            .contains("not registered in the hub registry"),
        "a registered builtin with no reports must not render absent: {}",
        harness_browser.dom
    );
    assert!(
        !harness_browser.dom.contains("detail read unavailable")
            && !harness_browser.dom.contains("read_unavailable"),
        "colon identity detail read must not be unavailable: {}",
        harness_browser.dom
    );

    // Unknown valid id renders the explicit not-registered state, not an error.
    let unknown_url = format!(
        "{url}#/components/{}",
        encode_component_id("missing:valid.component")
    );
    let unknown_browser = run_browser(
        &chrome,
        &unknown_url,
        BROWSER_BUDGET_MS,
        "first-launch-detail-unknown",
    );
    assert!(
        unknown_browser.status.success(),
        "browser failed: {}",
        unknown_browser.stderr
    );
    assert!(
        unknown_browser
            .dom
            .contains("not registered in the hub registry"),
        "unknown id must render explicit missing state: {}",
        unknown_browser.dom
    );
    assert!(
        !unknown_browser.dom.contains("Binding")
            && !unknown_browser.dom.contains("No report envelopes."),
        "an absent registry identity must not render a healthy detail: {}",
        unknown_browser.dom
    );
    assert!(!unknown_browser.dom.contains("read_unavailable"));

    // Observation-only: browsing detail creates no control authority, intent,
    // task claim or second privilege.
    let (_, capabilities_after) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(
        capabilities_after,
        vec![
            READ_CAPABILITY.to_string(),
            REPORT_PUBLISH_CAPABILITY.to_string()
        ]
    );
    assert!(
        launched.host.ledger().is_some(),
        "real ledger stays attached"
    );
    let pause = http_request(port, "POST", "/api/v1/factory/pause");
    assert!(
        pause.status == 404 || pause.status == 405,
        "no control route may exist, got {}",
        pause.status
    );

    // Lose the real hub source: shutting down the runtime drops the hub socket
    // and every accepted connection. The component-detail route itself must
    // surface 503, never a stale identity or a healthy ControlPlaneRead/v1 body.
    runtime.shutdown_background();
    let mut unavailable = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    let detail_path = format!(
        "/api/v1/components/{}?limit=50",
        encode_component_id(PLUGIN_ID)
    );
    while Instant::now() < deadline {
        let read = http_request(port, "GET", &detail_path);
        if read.status == 503 {
            unavailable = Some(read);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let read = unavailable
        .expect("the lost hub source must make component detail 503, never a healthy empty page");
    assert!(read.body.contains("read_unavailable"), "body={}", read.body);
    assert!(
        !read.body.contains("ControlPlaneRead/v1"),
        "a failed detail read must not return a success projection: {}",
        read.body
    );

    eprintln!("first_launch_detail: ok url={url} listed={listed:?} capabilities={capabilities:?}");
}

#[test]
fn first_launch_production_degraded_status_publishes_without_faking_readiness() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let scratch = ScratchDir::new("web-cw3-report-only");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let package = seal_dashboard_package(scratch.path());
    let launched = launch_report_only_dashboard(&runtime, &scratch, &package);

    let health = wait_for_degraded_health(&runtime, &launched.plugin_socket);
    assert_eq!(detail_field(&health, "listener"), Some("ready"));
    assert_eq!(detail_field(&health, "binding"), Some("unavailable"));
    // The owner-derived report carries the factual listener URL even while the
    // read binding is unavailable; `plugin.Health` deliberately withholds it
    // until both readiness facts hold (unchanged compatibility).
    assert_eq!(detail_field(&health, "url"), None);
    let reported = wait_for_status_report(&launched.host);
    assert_eq!(reported["listener"], "ready", "report={reported}");
    assert_eq!(reported["binding"], "unavailable", "report={reported}");
    let url = reported["url"]
        .as_str()
        .unwrap_or_else(|| panic!("degraded report must carry the actual listener URL: {reported}"))
        .to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "url={url}");

    // Publication never changes the owner's read readiness.
    let (after, capabilities) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(after.state, HealthState::Degraded);
    assert_eq!(detail_field(&after, "binding"), Some("unavailable"));
    assert_eq!(
        capabilities,
        vec![REPORT_PUBLISH_CAPABILITY.to_string()],
        "report authority must not imply read/control authority"
    );

    capture_owner_screenshot(&chrome, &url, "cw3-degraded-owner-status.png");
    eprintln!(
        "first_launch_report_only: ok url={url} report={reported} capabilities={capabilities:?}"
    );
}
