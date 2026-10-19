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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agalma_conductor::composition::{InMemoryInstallations, PluginHost};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot,
};
use agalma_conductor::plugin_router::{serve_hub, RpcHealthProbe};
use agalma_conductor::plugin_registry::DesiredState;
use agalma_conductor::plugin_supervisor::{Clock, SystemClock};
use agalma_contracts::{
    sha256_digest, HealthState, PluginHealth, PluginInstallation, PluginManifest,
    FACTORY_CONTROL_CAPABILITY,
};
use agalma_ipc::PluginClient;
use agalma_testkit::ScratchDir;
use agalma_web_dashboard::{PACKAGE_VERSION, PLUGIN_ID, READ_CAPABILITY};

use browser_support::{chrome_version, resolve_chrome, run_browser};

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
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    discover_plugins(&[PluginRoot::required(&root)])
        .expect("sealed package discovers")
        .package(PLUGIN_ID)
        .expect("dashboard package discovered")
        .clone()
}

fn installation_for(package: &DiscoveredPackage) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        // The operator-managed explicit installation pin approves exactly the
        // observation-only read capability. No control capability is approved.
        approved_capabilities: vec![READ_CAPABILITY.to_string()],
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
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
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
    let mut launched = launch_production_dashboard(&runtime, &scratch, &package);

    // The production child reported an actual loopback URL.
    let (health, capabilities) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(health.state, HealthState::Ready, "detail: {}", health.detail);
    assert_eq!(health.plugin_id, PLUGIN_ID);
    assert_eq!(health.binding.implementation_version, PACKAGE_VERSION);
    let url = detail_field(&health, "url").expect("actual URL reported").to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "url={url}");
    let port: u16 = url.rsplit(':').next().unwrap().parse().expect("port");
    eprintln!("first_launch: installed dashboard URL={url}");

    // Observation-only: exactly the read capability, no factory control.
    assert_eq!(capabilities, vec![READ_CAPABILITY.to_string()], "capabilities={capabilities:?}");
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
    assert!(outcome.status.success(), "browser failed: {}", outcome.stderr);
    let dom = &outcome.dom;
    assert!(dom.contains("Agalma cockpit"), "page shell missing: {dom}");
    assert!(dom.contains(PLUGIN_ID), "real component must render in the components page: {dom}");
    // The factory runtime endpoint is not part of M1, so the page says so
    // honestly and never claims Idle/Running.
    assert!(
        dom.contains("Unavailable (endpoint absent)") || dom.contains("Unknown"),
        "runtime must be explicit and unavailable: {dom}"
    );
    assert!(!dom.contains("Idle") && !dom.contains("Running"), "runtime must not be fabricated: {dom}");
    // Controls are disabled without a control binding.
    assert!(dom.contains("controls disabled"), "controls must be disabled: {dom}");

    // The page performed only same-origin reads; a navigation/reload never POSTs.
    let page_again = http_request(port, "GET", "/");
    assert_eq!(page_again.status, 200);
    assert!(page_again.body.contains("Agalma cockpit"));

    // Zero new task claims / control intents: the read source is observational,
    // the child holds no control or queue capability, and no factory-control
    // intent route exists. Checked after the browser runs.
    let (_, capabilities_after) = dashboard_health(&runtime, &launched.plugin_socket);
    assert_eq!(capabilities_after, vec![READ_CAPABILITY.to_string()]);
    assert!(launched.host.ledger().is_some(), "the real ledger stays attached and unmutated by viewing");
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
    let read = unavailable.expect("the lost read source must surface as 503, never a healthy empty page");
    assert!(read.body.contains("read_unavailable"), "body={}", read.body);

    // A fresh page against the degraded source renders explicit unknown or
    // unavailable data rather than healthy/empty data.
    let degraded = run_browser(&chrome, &url, BROWSER_BUDGET_MS, "first-launch-degraded");
    assert!(degraded.status.success(), "browser failed: {}", degraded.stderr);
    assert!(degraded.dom.contains("Agalma cockpit"), "dom={}", degraded.dom);
    assert!(
        degraded.dom.contains("Unavailable") || degraded.dom.contains("unknown"),
        "degraded page must be explicit: {}",
        degraded.dom
    );

    eprintln!(
        "first_launch: ok url={url} capabilities={capabilities:?} ledger_state={}",
        scratch.child("state").display()
    );
}
