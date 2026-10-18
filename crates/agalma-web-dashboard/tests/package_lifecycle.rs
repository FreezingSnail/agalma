//! WEB.4 package lifecycle acceptance (`agalma-rwn.4`).
//!
//! This test drives the **real** `agalma-web-dashboard` executable through the
//! production package discovery/install/launch primitives and an independent
//! typed hub RPC fixture. It proves the package-level contract only:
//!
//! - `plugin.Describe`/`plugin.Health` identity and the actual port-0 URL;
//! - compiled asset serving and the two supported read routes;
//! - a missing or rejected `control-plane.read/v1` grant degrades health and
//!   answers `503` (never a healthy empty page);
//! - invalid configuration and an occupied explicit port fail startup;
//! - unknown/encoded-traversal asset paths answer `404`;
//! - listener shutdown releases the owned child and the port.
//!
//! It does **not** establish conductor summary registration: the hub fixture is
//! an independent typed RPC peer, not the production conductor host.
//!
//! Deterministic property budget: 64 config cases and 64 asset-path cases with
//! seed `0x5745_4204` (a xorshift64 stream). Cases are generated in the test
//! source and the runner retains the raw log under `target/test-runs`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agalma_conductor::composition::{
    InMemoryInstallations, PluginHost, HUB_SOCKET_ENV, PLUGIN_GRANT_CAPABILITY_ENV,
    PLUGIN_GRANT_ENV,
};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot,
};
use agalma_conductor::plugin_registry::{
    plugin_socket_path, DesiredState, HubInstallStore, InMemoryDesiredStateStore, PluginRegistry,
};
use agalma_conductor::plugin_router::RpcHealthProbe;
use agalma_conductor::plugin_supervisor::{
    BackoffPolicy, Clock, LaunchRequest, PluginSupervisor, SystemClock,
};
use agalma_contracts::{
    sha256_digest, BindingPin, ContractError, FailureCode, HealthState, PluginHealth,
    PluginInstallation, PluginManifest,
};
use agalma_ipc::{
    register_hub, HubHandler, PluginClient, Server, ServerInfo, METHOD_INVOKE_CAPABILITY,
};
use agalma_testkit::ScratchDir;
use agalma_web_dashboard::config::{parse_config, ConfigError, DEFAULT_CONFIG_JSON};
use agalma_web_dashboard::inventory;
use agalma_web_dashboard::server::{
    LaunchInputs, PLUGIN_BINDING_GENERATION_ENV, PLUGIN_CONFIG_DIGEST_ENV, PLUGIN_CONFIG_PATH_ENV,
};
use agalma_web_dashboard::{PACKAGE_VERSION, PLUGIN_ID, READ_CAPABILITY};

/// Recorded deterministic property seed.
const PROPERTY_SEED: u64 = 0x5745_4204;
/// Recorded case budget for each generated collection.
const PROPERTY_CASES: u64 = 64;

const OVERVIEW_JSON: &str = include_str!("../../../fixtures/control-plane-read-v1/overview.json");
const COMPONENTS_JSON: &str =
    include_str!("../../../fixtures/control-plane-read-v1/components-page.json");

// ---------------------------------------------------------------------------
// Deterministic generator
// ---------------------------------------------------------------------------

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

// ---------------------------------------------------------------------------
// Configuration properties
// ---------------------------------------------------------------------------

/// Independent oracle: the only valid configuration is loopback `127.0.0.1`,
/// an integer port in `0..=65535` and an interval in `1..=60`, with no extra
/// keys and no malformed document.
fn config_is_valid(address: &str, port: i64, interval: i64, extra: bool) -> bool {
    address == "127.0.0.1" && (0..=65535).contains(&port) && (1..=60).contains(&interval) && !extra
}

#[test]
fn property_config_bounds_and_unknown_fields() {
    let addresses = [
        "127.0.0.1",
        "0.0.0.0",
        "192.168.1.10",
        "::1",
        "localhost",
        "",
    ];
    let ports = [0i64, 1, 65535, 65536, -1, 70000, 8080];
    let intervals = [0i64, 1, 60, 61, -1, 30];
    let mut rng = XorShift64::new(PROPERTY_SEED);

    for case in 0..PROPERTY_CASES {
        let address = addresses[rng.below(addresses.len())];
        let port = ports[rng.below(ports.len())];
        let interval = intervals[rng.below(intervals.len())];
        let extra = rng.below(5) == 0;
        let malformed = rng.below(11) == 0;

        let text = if malformed {
            format!("{{\"address\":{address:?},\"port\":{port},")
        } else {
            let mut object = serde_json::Map::new();
            object.insert("address".into(), Value::String(address.into()));
            object.insert("port".into(), json!(port));
            object.insert("refresh_interval_secs".into(), json!(interval));
            if extra {
                object.insert("unexpected".into(), json!(true));
            }
            Value::Object(object).to_string()
        };

        let expected = !malformed && config_is_valid(address, port, interval, extra);
        let parsed = parse_config(&text);
        assert_eq!(
            parsed.is_ok(),
            expected,
            "case {case}: text={text} result={parsed:?}"
        );
        if let Ok(config) = &parsed {
            assert_eq!(config.address.as_str(), address, "case {case}");
            assert_eq!(i64::from(config.port), port, "case {case}");
            assert_eq!(
                i64::from(config.refresh_interval_secs),
                interval,
                "case {case}"
            );
        }
        if malformed {
            assert!(
                matches!(parsed, Err(ConfigError::Malformed(_))),
                "case {case}"
            );
        } else if extra {
            assert!(
                matches!(parsed, Err(ConfigError::UnknownField(_))),
                "case {case}"
            );
        } else if interval < 0 {
            assert!(
                matches!(parsed, Err(ConfigError::Malformed(_))),
                "case {case}"
            );
        } else if address != "127.0.0.1"
            && (0..=65535).contains(&port)
            && (1..=60).contains(&interval)
        {
            assert!(
                matches!(parsed, Err(ConfigError::ForeignAddress(_))),
                "case {case}"
            );
        } else if address == "127.0.0.1"
            && (0..=65535).contains(&port)
            && !(1..=60).contains(&interval)
        {
            assert!(
                matches!(parsed, Err(ConfigError::RefreshIntervalOutOfRange(_))),
                "case {case}"
            );
        }
    }

    // Keep every contract boundary present even if a future generator change
    // alters its distribution. These are independent oracle cases, not samples
    // whose occurrence depends on the PRNG stream.
    for (case, address, port, interval) in [
        ("loopback-default-boundaries", "127.0.0.1", 0, 1),
        ("loopback-upper-port-and-interval", "127.0.0.1", 65535, 60),
        ("loopback-interval-zero", "127.0.0.1", 0, 0),
        ("loopback-interval-61", "127.0.0.1", 0, 61),
        ("foreign-ipv4", "192.168.1.10", 0, 1),
        ("ipv6-loopback", "::1", 0, 60),
    ] {
        let text = json!({
            "address": address,
            "port": port,
            "refresh_interval_secs": interval,
        })
        .to_string();
        let expected = config_is_valid(address, port, interval, false);
        assert_eq!(
            parse_config(&text).is_ok(),
            expected,
            "boundary case {case}: {text}"
        );
    }
    assert!(matches!(
        parse_config(
            r#"{"address":"127.0.0.1","port":0,"refresh_interval_secs":1,"unexpected":true}"#
        ),
        Err(ConfigError::UnknownField(_))
    ));

    assert_eq!(
        parse_config(DEFAULT_CONFIG_JSON).unwrap(),
        agalma_web_dashboard::DashboardConfig::default()
    );
    assert_eq!(
        parse_config("{}").unwrap(),
        agalma_web_dashboard::DashboardConfig::default()
    );
    // The committed package default and the compiled default agree.
    let packaged = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("package/objects/default-config.json"),
    )
    .unwrap();
    assert_eq!(
        parse_config(&packaged).unwrap(),
        agalma_web_dashboard::DashboardConfig::default()
    );
}

// ---------------------------------------------------------------------------
// Asset-path properties
// ---------------------------------------------------------------------------

/// Independent oracle for the WEB.3 inventory path grammar: plain absolute
/// path, no query/fragment/backslash/control/space, no `.`/`..` segments.
fn oracle_safe_asset_path(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 1024 {
        return false;
    }
    if path.contains('?') || path.contains('#') || path.contains('\\') {
        return false;
    }
    if path
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return false;
    }
    path[1..]
        .split('/')
        .all(|segment| segment != "." && segment != "..")
}

#[test]
fn property_asset_paths_stay_confined() {
    let pieces = [
        "health.html",
        "app.js",
        "index.html",
        "nested",
        "assets",
        "..",
        ".",
        "%2e%2e",
        "%2F",
        "",
        "\\",
        "?x",
    ];
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x0000_0000_0000_0001);
    let compiled: BTreeSet<&str> = inventory::compiled_paths().collect();
    let required_paths = [
        "/health.html",
        "/%2e%2e/package/plugin-manifest.json",
        "/..%2Fpackage/plugin-manifest.json",
        "/%2f..%2fpackage/plugin-manifest.json",
    ];
    assert!(
        compiled.contains(inventory::HEALTH_PAGE),
        "the committed inventory contains the health page"
    );

    for case in 0..PROPERTY_CASES {
        let path = if let Some(path) = required_paths.get(case as usize) {
            (*path).to_string()
        } else {
            let depth = 1 + rng.below(3);
            let mut segments = Vec::new();
            for _ in 0..depth {
                segments.push(pieces[rng.below(pieces.len())]);
            }
            format!("/{}", segments.join("/"))
        };

        // The compiled inventory only contains committed safe keys.
        for asset in &compiled {
            assert!(
                oracle_safe_asset_path(asset),
                "committed asset {asset:?} is safe"
            );
        }
        if !oracle_safe_asset_path(&path) {
            assert!(
                !inventory::contains(&path),
                "case {case}: unsafe path {path:?} must not resolve"
            );
        }

        // The WEB.3 inventory applies the same confinement independently.
        let mut inventory = agalma_ipc::AssetInventory::new();
        let inserted = inventory.insert(&path, "text/plain; charset=utf-8", vec![]);
        assert_eq!(
            inserted.is_ok(),
            oracle_safe_asset_path(&path),
            "case {case}: {path:?} insertion={inserted:?}"
        );
        if inserted.is_ok() {
            assert!(
                inventory
                    .insert(&path, "text/plain; charset=utf-8", vec![])
                    .is_err(),
                "case {case}: duplicate request path {path:?} must be refused"
            );
        }
    }

    // Explicit duplicate-path regression: the seeded property cases above
    // always include this ordinary, valid asset path as their first case.
    assert!(inventory::contains("/health.html"));
}

#[cfg(unix)]
#[test]
fn asset_walk_is_deterministic_and_rejects_unsafe_links() {
    use std::os::unix::fs::symlink;

    let scratch = ScratchDir::new("web4-asset-walk");
    let root = scratch.child("assets");
    fs::create_dir_all(root.join("nested/assets")).unwrap();
    // Write out of lexical order on purpose: the walk sorts.
    fs::write(root.join("nested/assets/app.js"), b"console.log('ok');\n").unwrap();
    fs::write(root.join("health.html"), b"<h1>health</h1>\n").unwrap();
    symlink(root.join("nested/assets/app.js"), root.join("alias.js")).unwrap();

    let first = agalma_web_dashboard::asset_walk::collect_assets(&root).unwrap();
    let second = agalma_web_dashboard::asset_walk::collect_assets(&root).unwrap();
    assert_eq!(first, second, "the walk is deterministic");
    let paths: Vec<&str> = first
        .iter()
        .map(|asset| asset.request_path.as_str())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        paths, sorted,
        "inventory order follows request-path byte order"
    );
    assert_eq!(
        first
            .iter()
            .find(|asset| asset.request_path == "/health.html")
            .unwrap()
            .content_type,
        "text/html; charset=utf-8"
    );
    assert!(first.iter().any(|asset| asset.request_path == "/alias.js"));
    assert!(first
        .iter()
        .any(|asset| asset.request_path == "/nested/assets/app.js"));

    // A directory symlink is refused outright (it could cycle).
    symlink(root.join("nested"), root.join("dirlink")).unwrap();
    let error = agalma_web_dashboard::asset_walk::collect_assets(&root).unwrap_err();
    assert!(
        matches!(
            error,
            agalma_web_dashboard::asset_walk::AssetWalkError::UnsafePath(_)
        ),
        "{error:?}"
    );
    fs::remove_dir_all(root.join("dirlink")).unwrap();

    // A symlink that escapes the asset root is refused.
    let outside = scratch.path().join("outside.txt");
    fs::write(&outside, b"secret").unwrap();
    symlink(&outside, root.join("escape.txt")).unwrap();
    let error = agalma_web_dashboard::asset_walk::collect_assets(&root).unwrap_err();
    assert!(
        matches!(
            error,
            agalma_web_dashboard::asset_walk::AssetWalkError::UnsafePath(_)
        ),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------
// Independent typed hub RPC fixture
// ---------------------------------------------------------------------------

struct FixtureHub {
    valid_grant: String,
    overview: Value,
    components: Value,
    overview_reads: Arc<AtomicU64>,
    component_cursors: Arc<Mutex<Vec<Option<String>>>>,
}

impl HubHandler for FixtureHub {
    fn invoke_capability(&self, request: Value) -> Result<Value, ContractError> {
        let object = request.as_object().ok_or_else(|| {
            FailureCode::InvalidRecord.failure("capability-call", "not an object")
        })?;
        let field = |name: &str| object.get(name).and_then(Value::as_str);
        if field("method") != Some(METHOD_INVOKE_CAPABILITY) {
            return Err(FailureCode::MissingCapability
                .failure("method", "fixture only serves hub.InvokeCapability"));
        }
        if field("capability") != Some(READ_CAPABILITY) {
            return Err(FailureCode::MissingCapability
                .failure("capability", format!("expected {READ_CAPABILITY}")));
        }
        if field("plugin_id") != Some(PLUGIN_ID) {
            return Err(FailureCode::ScopeDenied
                .failure("plugin_id", "fixture is pinned to the dashboard plugin"));
        }
        if field("grant") != Some(self.valid_grant.as_str()) {
            return Err(
                FailureCode::AuthorityDenied.failure("grant", "expired or unknown read grant")
            );
        }
        let operation = object
            .get("input")
            .and_then(|input| input.get("operation"))
            .and_then(Value::as_str)
            .ok_or_else(|| FailureCode::InvalidRecord.failure("input", "missing operation"))?;
        match operation {
            "read_overview" => {
                self.overview_reads.fetch_add(1, Ordering::SeqCst);
                Ok(self.overview.clone())
            }
            "list_components" => {
                let cursor = object
                    .get("input")
                    .and_then(|input| input.get("cursor"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.component_cursors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(cursor.clone());
                let next_cursor = self.components["next_cursor"].as_str();
                let after = cursor
                    .as_deref()
                    .and_then(|raw| raw.strip_prefix("cp1:"))
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|value| value["after"].as_str().map(str::to_string));
                if after.as_deref() == Some("deadline") {
                    return Err(FailureCode::DeadlineExceeded
                        .failure("components", "fixture deadline reached"));
                }
                if cursor.is_some() && cursor.as_deref() != next_cursor {
                    return Err(FailureCode::RevisionConflict
                        .failure("components", "fixture cursor revision changed"));
                }
                Ok(self.components.clone())
            }
            other => Err(FailureCode::ScopeDenied
                .failure(other, "fixture supports overview/components only")),
        }
    }
}

fn start_fixture_hub(
    runtime: &tokio::runtime::Runtime,
    socket: &Path,
    grant: &str,
) -> Arc<FixtureHub> {
    let hub = Arc::new(FixtureHub {
        valid_grant: grant.to_string(),
        overview: serde_json::from_str(OVERVIEW_JSON).expect("overview fixture"),
        components: fixture_components(),
        overview_reads: Arc::new(AtomicU64::new(0)),
        component_cursors: Arc::new(Mutex::new(Vec::new())),
    });
    let mut server = Server::new(ServerInfo {
        api: "hub".to_string(),
        api_version: 1,
        impl_name: "web4-fixture-hub".to_string(),
        impl_version: "1".to_string(),
        capabilities: BTreeSet::new(),
    });
    register_hub(&mut server, hub.clone());
    let serve_socket = socket.to_path_buf();
    runtime.spawn(async move {
        let _ = agalma_ipc::connect::serve(server, &serve_socket).await;
    });
    wait_for_path(socket, "hub fixture socket");
    hub
}

fn fixture_components() -> Value {
    let mut components: Value = serde_json::from_str(COMPONENTS_JSON).expect("components fixture");
    // BindingPin generations are exact decimal strings on the typed RPC wire.
    for entry in components["entries"]
        .as_array_mut()
        .expect("fixture entries are an array")
    {
        let binding_generation = &mut entry["binding"]["generation"];
        let generation = binding_generation
            .as_u64()
            .expect("fixture binding generation is an unsigned integer");
        *binding_generation = json!(generation.to_string());
    }
    components
}

// ---------------------------------------------------------------------------
// Package sealing and production launch
// ---------------------------------------------------------------------------

fn seal_dashboard_package(scratch: &Path, config_override: Option<&str>) -> DiscoveredPackage {
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
    if let Some(config) = config_override {
        fs::write(package_dir.join("objects/default-config.json"), config).unwrap();
    }

    let binary = Path::new(env!("CARGO_BIN_EXE_agalma-web-dashboard"));
    let installed_binary = package_dir.join("bin/agalma-web-dashboard");
    fs::copy(binary, &installed_binary).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Seal the complete package: the manifest's package_digest covers the
    // executable bytes and every committed object except the manifest itself.
    let manifest_path = package_dir.join("plugin-manifest.json");
    let mut manifest: PluginManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    if let Some(config) = config_override {
        manifest.default_configuration.digest = sha256_digest(config.as_bytes());
        manifest.default_configuration.bytes = config.len() as u64;
    }
    manifest.package_digest = format!("sha256:{}", "0".repeat(64));
    manifest.package_digest = package_inventory_digest(&package_dir, &manifest).unwrap();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let report =
        discover_plugins(&[PluginRoot::required(&root)]).expect("sealed package discovers");
    report
        .package(PLUGIN_ID)
        .expect("dashboard package discovered")
        .clone()
}

struct Launched {
    supervisor: PluginSupervisor,
    plugin_socket: PathBuf,
}

impl Drop for Launched {
    fn drop(&mut self) {
        // Also reap the real package child if an assertion unwinds before the
        // test's explicit stop/check path.
        let _ = self.supervisor.stop(PLUGIN_ID);
    }
}

fn launch_dashboard(
    scratch: &ScratchDir,
    package: &DiscoveredPackage,
    hub: Option<(&Path, &str)>,
) -> Launched {
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let installation = installation_for(package);
    let store = HubInstallStore::new(&state_dir);
    let mut registry = PluginRegistry::new();
    let generation = registry
        .install(&store, package, &installation)
        .expect("immutable generation installs");
    let installed = registry
        .entry(PLUGIN_ID)
        .expect("registry entry")
        .generations
        .get(&generation)
        .expect("generation installed")
        .clone();
    let plugin_socket = plugin_socket_path(&state_dir, PLUGIN_ID, 1);
    let mut request = LaunchRequest::from_installed(&installed, plugin_socket.clone());
    if let Some((hub_socket, grant)) = hub {
        request.env.insert(
            HUB_SOCKET_ENV.to_string(),
            hub_socket.to_string_lossy().into_owned(),
        );
        request
            .env
            .insert(PLUGIN_GRANT_ENV.to_string(), grant.to_string());
        request.env.insert(
            PLUGIN_GRANT_CAPABILITY_ENV.to_string(),
            READ_CAPABILITY.to_string(),
        );
    }
    let clock: Rc<dyn Clock> = Rc::new(SystemClock);
    let mut supervisor =
        PluginSupervisor::new(clock, BackoffPolicy::default(), Duration::from_secs(20));
    supervisor
        .spawn(&mut registry, &request)
        .expect("real dashboard executable passes the READY identity gate");
    Launched {
        supervisor,
        plugin_socket,
    }
}

fn installation_for(package: &DiscoveredPackage) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: vec![READ_CAPABILITY.to_string()],
        installation_digest: String::new(),
    };
    installation.installation_digest = installation.recomputed_installation_digest().unwrap();
    installation
}

struct ProductionLaunch {
    host: PluginHost,
    plugin_socket: PathBuf,
    pinned: BindingPin,
    overview_reads: Arc<AtomicU64>,
    component_cursors: Arc<Mutex<Vec<Option<String>>>>,
    _hub: Arc<FixtureHub>,
}

impl Drop for ProductionLaunch {
    fn drop(&mut self) {
        let _ = self.host.shutdown();
    }
}

fn launch_dashboard_through_host(
    runtime: &tokio::runtime::Runtime,
    scratch: &ScratchDir,
    package: &DiscoveredPackage,
    generation_two_package: &DiscoveredPackage,
) -> ProductionLaunch {
    let state_dir = scratch.child("production-host-state");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(package.root.clone())],
        once: true,
    };
    let installation = installation_for(package);
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation.clone());

    let mut host = PluginHost::assemble(
        &config,
        &installations,
        Box::new(InMemoryDesiredStateStore::new()),
        Rc::new(SystemClock),
    )
    .expect("production plugin host discovers and installs dashboard");

    // Launch generation 2 so the runtime health pin cannot pass by hard-coding
    // the first generation.
    let generation_two_installation = installation_for(generation_two_package);
    let generation = host
        .registry
        .install(
            &host.install_store,
            generation_two_package,
            &generation_two_installation,
        )
        .expect("second pinned generation installs");
    assert_eq!(generation, 2);
    host.registry
        .stage(PLUGIN_ID, generation)
        .expect("second generation stages");
    let pinned = host.pinned_binding(PLUGIN_ID).expect("host pin");
    assert_eq!(pinned.generation, generation);
    assert_eq!(
        pinned.config_digest,
        generation_two_package.manifest.default_configuration.digest
    );

    host.set_desired(PLUGIN_ID, DesiredState::Running)
        .expect("desired running state records");
    let mut request = host
        .launch_request(PLUGIN_ID)
        .expect("production host builds the installed launch request");
    let grant = request
        .env
        .get(PLUGIN_GRANT_ENV)
        .expect("host-issued launch grant")
        .clone();
    let hub_socket = short_socket(scratch, "host-fixture-hub");
    let hub = start_fixture_hub(runtime, &hub_socket, &grant);
    request.env.insert(
        HUB_SOCKET_ENV.to_string(),
        hub_socket.to_string_lossy().into_owned(),
    );
    let plugin_socket = request.socket.clone();
    host.supervisor
        .spawn(&mut host.registry, &request)
        .expect("production-installed child passes the READY identity gate");
    assert!(host
        .confirm_ready_health(PLUGIN_ID, &mut RpcHealthProbe::new(state_dir.clone()))
        .expect("production full pinned health gate"));
    assert_eq!(
        host.registry.entry(PLUGIN_ID).unwrap().observed,
        agalma_conductor::plugin_registry::LifecycleState::Ready
    );
    assert_eq!(hub.overview_reads.load(Ordering::SeqCst), 1);

    ProductionLaunch {
        plugin_socket,
        host,
        pinned,
        overview_reads: hub.overview_reads.clone(),
        component_cursors: hub.component_cursors.clone(),
        _hub: hub,
    }
}

// ---------------------------------------------------------------------------
// HTTP helper (plain HTTP/1.1; the listener serves HTTP/1)
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

async fn health_status(client: &mut PluginClient) -> PluginHealth {
    let value = client.health().await.expect("plugin.Health");
    let health: PluginHealth = serde_json::from_value(value).expect("PluginHealth/v1");
    health.version_gate().expect("health version gate");
    health
}

fn wait_for_binding_unavailable(
    runtime: &tokio::runtime::Runtime,
    client: &mut PluginClient,
    deadline: Instant,
) -> PluginHealth {
    loop {
        let health = runtime.block_on(health_status(client));
        if health.detail.contains("hub read binding validation failed") {
            return health;
        }
        assert!(
            Instant::now() < deadline,
            "binding validation did not finish: {health:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn detail_field<'a>(health: &'a PluginHealth, name: &str) -> Option<&'a str> {
    health
        .detail
        .split(';')
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
}

fn percent_encode_query_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn cursor_token(after: &str) -> String {
    format!(
        "cp1:{}",
        json!({
            "after": after,
            "collection": "components",
            "filter_digest": format!("sha256:{}", "a".repeat(64)),
            "revision": "9007199254740993",
        })
    )
}

// ---------------------------------------------------------------------------
// Success path
// ---------------------------------------------------------------------------

#[test]
fn package_bootstraps_serves_reads_and_shuts_down() {
    let scratch = ScratchDir::new("web4-package-success");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let expected_port = unused_loopback_port();
    let custom_config =
        format!(r#"{{"address":"127.0.0.1","port":{expected_port},"refresh_interval_secs":17}}"#);
    let package = seal_dashboard_package(scratch.path(), None);
    let generation_two_package = seal_dashboard_package(
        &scratch.path().join("generation-two-package"),
        Some(&custom_config),
    );
    let mut launched =
        launch_dashboard_through_host(&runtime, &scratch, &package, &generation_two_package);

    let mut client = runtime
        .block_on(PluginClient::connect(&launched.plugin_socket))
        .expect("connect real dashboard plugin RPC");
    let identity = runtime
        .block_on(client.handshake(PLUGIN_ID))
        .expect("Describe identity/protocol agreement");
    assert_eq!(identity.api, "plugin");
    assert_eq!(identity.protocol_major, 1);
    assert_eq!(identity.plugin_id, PLUGIN_ID);
    assert_eq!(identity.implementation_version, PACKAGE_VERSION);
    assert!(identity.capabilities.iter().any(|c| c == READ_CAPABILITY));

    let health = runtime.block_on(health_status(&mut client));
    assert_eq!(health.plugin_id, PLUGIN_ID);
    assert_eq!(
        health.state,
        HealthState::Ready,
        "detail: {}",
        health.detail
    );
    assert_eq!(detail_field(&health, "listener"), Some("ready"));
    assert_eq!(detail_field(&health, "binding"), Some("ready"));
    assert_eq!(health.binding, launched.pinned);
    assert_eq!(health.binding.generation, 2, "host pin is not hard-coded");
    assert_eq!(
        health.binding.config_digest, generation_two_package.manifest.default_configuration.digest,
        "health uses the exact generation-2 installed config artifact digest"
    );
    assert_eq!(
        launched.overview_reads.load(Ordering::SeqCst),
        1,
        "authenticated typed read completed before initial Ready health"
    );

    let url = detail_field(&health, "url").expect("actual URL reported");
    assert!(url.starts_with("http://127.0.0.1:"), "url={url}");
    let port: u16 = url
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .expect("configured port");
    assert_eq!(
        port, expected_port,
        "the generation-2 private installed config path controls the listener"
    );

    let page = http_request(port, "GET", "/health.html");
    assert_eq!(page.status, 200);
    assert!(page.headers.contains("content-type: text/html"));
    assert!(page.body.contains("agalma-dashboard-health"));

    let root = http_request(port, "GET", "/");
    assert_eq!(
        root.status, 200,
        "root falls back to health.html before WEB.6"
    );

    let overview = http_request(port, "GET", "/api/v1/overview");
    assert_eq!(overview.status, 200, "{}", overview.body);
    let overview_body: Value = serde_json::from_str(&overview.body).unwrap();
    assert_eq!(overview_body["metadata"]["format"], "ControlPlaneRead/v1");
    assert_eq!(overview_body["factory"]["stopped"], true);
    assert_eq!(
        overview_body["executions"][0]["generation"],
        "18446744073709551615"
    );

    let invalid_limit = http_request(port, "GET", "/api/v1/components?limit=%31%30%31");
    assert_eq!(invalid_limit.status, 400, "{}", invalid_limit.body);
    let invalid_cursor = http_request(port, "GET", "/api/v1/components?cursor=not-a-cursor");
    assert_eq!(invalid_cursor.status, 400, "{}", invalid_cursor.body);
    let malformed_escape = http_request(port, "GET", "/api/v1/components?cursor=%zz");
    assert_eq!(malformed_escape.status, 400, "{}", malformed_escape.body);
    assert_eq!(
        runtime.block_on(health_status(&mut client)).state,
        HealthState::Ready,
        "invalid browser requests do not degrade the established binding"
    );

    let components = http_request(port, "GET", "/api/v1/components?limit=1");
    assert_eq!(components.status, 200, "{}", components.body);
    let components_body: Value = serde_json::from_str(&components.body).unwrap();
    assert_eq!(components_body["metadata"]["format"], "ControlPlaneRead/v1");
    assert_eq!(
        components_body["entries"][0]["plugin_id"],
        "example.unfamiliar-reporter"
    );
    let next_cursor = components_body["next_cursor"]
        .as_str()
        .expect("fixture cursor");
    let encoded_cursor = percent_encode_query_component(next_cursor);
    let next_page = http_request(
        port,
        "GET",
        &format!("/api/v1/components?limit=1&cursor={encoded_cursor}"),
    );
    assert_eq!(next_page.status, 200, "{}", next_page.body);
    assert_eq!(
        launched
            .component_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .and_then(Option::as_deref),
        Some(next_cursor),
        "browser's encoded opaque cursor arrives at the typed hub RPC decoded once"
    );
    let deadline_cursor = percent_encode_query_component(&cursor_token("deadline"));
    let deadline_response = http_request(
        port,
        "GET",
        &format!("/api/v1/components?cursor={deadline_cursor}"),
    );
    assert_eq!(deadline_response.status, 504, "{}", deadline_response.body);
    assert_eq!(
        runtime.block_on(health_status(&mut client)).state,
        HealthState::Ready,
        "request deadline is preserved without invalidating the binding"
    );

    // Deterministic property budget: 64 valid opaque cursor tokens containing
    // URL-reserved/UTF-8 bytes; the independent oracle is exact token identity.
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x0000_0000_0000_0002);
    let alphabet = ["a", "+", " ", "&", "=", "%", "/", "é"];
    let mut generated = Vec::new();
    for _ in 0..PROPERTY_CASES {
        let after = (0..8)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect::<String>();
        let token = cursor_token(&after);
        let encoded = percent_encode_query_component(&token);
        let response = http_request(port, "GET", &format!("/api/v1/components?cursor={encoded}"));
        assert_eq!(response.status, 409, "token={token}: {}", response.body);
        generated.push(token);
    }
    let cursors = launched
        .component_cursors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        &cursors[cursors.len() - PROPERTY_CASES as usize..],
        &generated.into_iter().map(Some).collect::<Vec<_>>(),
        "percent decoding preserves each generated opaque cursor exactly"
    );
    drop(cursors);
    assert_eq!(
        runtime.block_on(health_status(&mut client)).state,
        HealthState::Ready,
        "revision conflicts do not degrade binding readiness"
    );

    // Unknown and traversal-shaped asset paths never escape the inventory.
    for path in [
        "/missing.html",
        "/../package/plugin-manifest.json",
        "/%2e%2e/%2e%2e/package/plugin-manifest.json",
    ] {
        let response = http_request(port, "GET", path);
        assert_eq!(response.status, 404, "path {path}: {}", response.body);
        assert!(!response.body.contains("package_digest"));
    }
    let encoded_separator = http_request(port, "GET", "/..%2Fpackage%2Fplugin-manifest.json");
    assert_eq!(
        encoded_separator.status, 400,
        "encoded separator: {}",
        encoded_separator.body
    );
    assert!(!encoded_separator.body.contains("package_digest"));
    // The read transport grants no mutation method.
    let method = http_request(port, "POST", "/api/v1/overview");
    assert_eq!(method.status, 405);
    assert!(method.headers.contains("allow: get"));

    // Evidence-bearing shutdown: stop the owned child, then confirm the
    // listener no longer accepts connections.
    let evidence = launched.host.shutdown();
    assert_eq!(evidence.len(), 1);
    assert!(evidence[0].1.process_group_gone, "{}", evidence[0].1.detail);
    assert!(
        !launched.plugin_socket.exists(),
        "RPC socket is removed after join"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "listener still accepts after shutdown"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn package_bootstraps_ephemeral_port_serves_and_shuts_down() {
    let scratch = ScratchDir::new("web4-package-port-zero");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let grant = "port-zero-read-grant";
    let hub_socket = short_socket(&scratch, "port-zero-hub");
    let hub = start_fixture_hub(&runtime, &hub_socket, grant);
    let config = r#"{"address":"127.0.0.1","port":0,"refresh_interval_secs":1}"#;
    let package = seal_dashboard_package(scratch.path(), Some(config));
    let mut launched = launch_dashboard(&scratch, &package, Some((&hub_socket, grant)));
    let mut client = runtime
        .block_on(PluginClient::connect(&launched.plugin_socket))
        .expect("connect real dashboard plugin RPC");
    let identity = runtime
        .block_on(client.handshake(PLUGIN_ID))
        .expect("Describe identity/protocol agreement");
    assert_eq!(identity.plugin_id, PLUGIN_ID);

    let readiness_deadline = Instant::now() + Duration::from_secs(15);
    let health = loop {
        let health = runtime.block_on(health_status(&mut client));
        if health.state == HealthState::Ready {
            break health;
        }
        assert!(
            Instant::now() < readiness_deadline,
            "port-zero package did not become ready: {health:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(detail_field(&health, "listener"), Some("ready"));
    assert_eq!(detail_field(&health, "binding"), Some("ready"));
    assert_eq!(hub.overview_reads.load(Ordering::SeqCst), 1);

    let url = detail_field(&health, "url").expect("actual URL reported");
    let port: u16 = url
        .strip_prefix("http://127.0.0.1:")
        .expect("reported URL is loopback HTTP")
        .parse()
        .expect("reported URL contains only its allocated port");
    assert_ne!(port, 0, "the operating system allocated a nonzero port");
    assert_eq!(url, format!("http://127.0.0.1:{port}"));

    let response = http_request(port, "GET", "/api/v1/overview");
    assert_eq!(response.status, 200, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).expect("valid overview JSON");
    assert_eq!(body["metadata"]["format"], "ControlPlaneRead/v1");

    let evidence = launched
        .supervisor
        .stop(PLUGIN_ID)
        .expect("supervisor owns the dashboard process");
    assert!(evidence.process_group_gone, "{}", evidence.detail);
    let shutdown_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            break;
        }
        assert!(
            Instant::now() < shutdown_deadline,
            "ephemeral listener still accepts after process stop"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Missing / rejected read binding
// ---------------------------------------------------------------------------

#[test]
fn missing_and_rejected_read_binding_degrade_health_and_refuse_reads() {
    let scratch = ScratchDir::new("web4-package-binding");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let missing_port = unused_loopback_port();
    let missing_config =
        format!(r#"{{"address":"127.0.0.1","port":{missing_port},"refresh_interval_secs":1}}"#);
    let package = seal_dashboard_package(scratch.path(), Some(&missing_config));

    // Missing binding: no hub socket/grant was supplied.
    let mut missing = launch_dashboard(&scratch, &package, None);
    let mut client = runtime
        .block_on(PluginClient::connect(&missing.plugin_socket))
        .expect("connect dashboard");
    let health = wait_for_binding_unavailable(
        &runtime,
        &mut client,
        Instant::now() + Duration::from_secs(15),
    );
    assert_eq!(
        health.state,
        HealthState::Degraded,
        "detail: {}",
        health.detail
    );
    assert_eq!(detail_field(&health, "listener"), Some("ready"));
    assert_eq!(detail_field(&health, "binding"), Some("unavailable"));
    assert_eq!(
        detail_field(&health, "url"),
        None,
        "URL is withheld until read readiness"
    );
    let refused = http_request(missing_port, "GET", "/api/v1/overview");
    assert_eq!(refused.status, 503);
    assert!(refused.body.contains("read_unavailable"));
    assert!(!refused.body.contains("package_digest"));
    assert!(
        missing.supervisor.stop(PLUGIN_ID).is_some(),
        "missing-binding child stopped"
    );

    // A failed private socket connection must not reveal its path or grant.
    let scratch = ScratchDir::new("web4-private-binding");
    let private_port = unused_loopback_port();
    let private_config =
        format!(r#"{{"address":"127.0.0.1","port":{private_port},"refresh_interval_secs":1}}"#);
    let package = seal_dashboard_package(scratch.path(), Some(&private_config));
    let private_socket = short_socket(&scratch, "PRIVATE_HUB_SOCKET_SENTINEL");
    let private_token = "PRIVATE_GRANT_SENTINEL_".to_string() + &"dd44".repeat(16);
    let mut private = launch_dashboard(&scratch, &package, Some((&private_socket, &private_token)));
    let mut private_client = runtime
        .block_on(PluginClient::connect(&private.plugin_socket))
        .expect("connect dashboard with private hub path");
    let health = wait_for_binding_unavailable(
        &runtime,
        &mut private_client,
        Instant::now() + Duration::from_secs(15),
    );
    assert_eq!(health.state, HealthState::Degraded);
    assert!(!health.detail.contains("PRIVATE_HUB_SOCKET_SENTINEL"));
    assert!(!health.detail.contains("PRIVATE_GRANT_SENTINEL"));
    assert_eq!(detail_field(&health, "url"), None);
    let refused = http_request(private_port, "GET", "/api/v1/overview");
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(!refused.body.contains("PRIVATE_HUB_SOCKET_SENTINEL"));
    assert!(!refused.body.contains("PRIVATE_GRANT_SENTINEL"));
    assert!(private.supervisor.stop(PLUGIN_ID).is_some());

    // Rejected (expired) grant: the fixture knows a different valid token.
    let scratch = ScratchDir::new("web4-package-expired");
    let expired_port = unused_loopback_port();
    let expired_config =
        format!(r#"{{"address":"127.0.0.1","port":{expired_port},"refresh_interval_secs":1}}"#);
    let package = seal_dashboard_package(scratch.path(), Some(&expired_config));
    let hub_socket = short_socket(&scratch, "hub");
    let valid_grant = "bb22".repeat(16);
    let _hub = start_fixture_hub(&runtime, &hub_socket, &valid_grant);
    let expired_grant = "cc33".repeat(16);
    let mut expired = launch_dashboard(&scratch, &package, Some((&hub_socket, &expired_grant)));
    let mut client = runtime
        .block_on(PluginClient::connect(&expired.plugin_socket))
        .expect("connect dashboard");
    let health = wait_for_binding_unavailable(
        &runtime,
        &mut client,
        Instant::now() + Duration::from_secs(15),
    );
    assert_eq!(health.state, HealthState::Degraded);
    assert_eq!(detail_field(&health, "url"), None);
    let refused = http_request(expired_port, "GET", "/api/v1/overview");
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(refused.body.contains("read_unavailable"));
    // The failed read must not become a healthy empty page.
    assert!(!refused.body.contains("ControlPlaneRead/v1"));
    let health = runtime.block_on(health_status(&mut client));
    assert_ne!(
        health.state,
        HealthState::Ready,
        "detail: {}",
        health.detail
    );
    assert_eq!(detail_field(&health, "binding"), Some("unavailable"));
    assert!(expired.supervisor.stop(PLUGIN_ID).is_some());
}

// ---------------------------------------------------------------------------
// Startup refusals
// ---------------------------------------------------------------------------

#[test]
fn invalid_config_and_occupied_port_fail_startup_explicitly() {
    let scratch = ScratchDir::new("web4-package-refusals");
    let binary = Path::new(env!("CARGO_BIN_EXE_agalma-web-dashboard"));

    let missing_pin_socket = short_socket(&scratch, "missing-private-config-pin");
    let missing_pin = run_failing(binary, &missing_pin_socket, None);
    assert!(!missing_pin.status.success());
    assert!(missing_pin
        .stderr
        .contains("missing or invalid private installed configuration pin"));

    let invalid_configs = [
        r#"{"address":"0.0.0.0","port":0,"refresh_interval_secs":1}"#,
        r#"{"address":"::1","port":0,"refresh_interval_secs":1}"#,
        r#"{"address":"127.0.0.1","port":0,"refresh_interval_secs":0}"#,
        r#"{"address":"127.0.0.1","port":0,"refresh_interval_secs":61}"#,
        r#"{"address":"127.0.0.1","port":0,"refresh_interval_secs":1,"unexpected":true}"#,
        r#"{"address":"127.0.0.1","port":65536}"#,
        "{not json",
    ];
    for (index, config) in invalid_configs.iter().enumerate() {
        let socket = short_socket(&scratch, &format!("invalid-{index}"));
        let output = run_failing(binary, &socket, Some(config));
        assert!(
            !output.status.success(),
            "case {index}: invalid config must fail: {config}"
        );
        assert!(
            output.stderr.to_ascii_lowercase().contains("dashboard"),
            "case {index}: stderr={}",
            output.stderr
        );
    }

    // Occupy a concrete loopback port, then require an explicit bind failure.
    let occupied = TcpListener::bind(("127.0.0.1", 0)).expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();
    let socket = short_socket(&scratch, "occupied");
    let config = format!(r#"{{"address":"127.0.0.1","port":{port},"refresh_interval_secs":1}}"#);
    let output = run_failing(binary, &socket, Some(&config));
    assert!(!output.status.success(), "occupied port must fail startup");
    let stderr = output.stderr.to_ascii_lowercase();
    assert!(
        stderr.contains("bind failed") || stderr.contains("in use"),
        "stderr={}",
        output.stderr
    );
    drop(occupied);

    // A plugin RPC bind failure is surfaced, the HTTP listener is drained, and
    // no URL is announced for an unusable plugin process.
    let port = unused_loopback_port();
    let config = format!(r#"{{"address":"127.0.0.1","port":{port},"refresh_interval_secs":1}}"#);
    let socket = short_socket(&scratch, "rpc-failure");
    fs::create_dir(&socket).expect("directory forces Unix socket bind failure");
    let output = run_failing(binary, &socket, Some(&config));
    assert!(
        !output.status.success(),
        "RPC serving failure must fail launch"
    );
    assert!(
        output.stderr.contains("plugin RPC server failed"),
        "{}",
        output.stderr
    );
    assert!(
        !output.stdout.contains("DASHBOARD_URL"),
        "{}",
        output.stdout
    );
    assert!(
        !output.stderr.contains("DASHBOARD_URL"),
        "{}",
        output.stderr
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "HTTP listener is shut down after RPC failure"
    );
}

struct FailingOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_failing(binary: &Path, socket: &Path, config: Option<&str>) -> FailingOutput {
    let mut command = std::process::Command::new(binary);
    command.arg("--socket").arg(socket);
    let config_path = socket.with_extension("config.json");
    if let Some(config) = config {
        fs::write(&config_path, config).expect("write launch config fixture");
        command.env(PLUGIN_CONFIG_PATH_ENV, &config_path);
        command.env(PLUGIN_BINDING_GENERATION_ENV, "1");
        command.env(PLUGIN_CONFIG_DIGEST_ENV, sha256_digest(config.as_bytes()));
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn dashboard executable");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            let mut stdout = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                let _ = pipe.read_to_string(&mut stdout);
            }
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            return FailingOutput {
                status,
                stdout,
                stderr,
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dashboard did not exit within the startup bound");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn private_installed_config_pin_fails_closed_when_missing_or_malformed() {
    let scratch = ScratchDir::new("web4-private-config-pin");
    let path = scratch.path().join("installed-config.json");
    fs::write(&path, DEFAULT_CONFIG_JSON).unwrap();
    let digest = sha256_digest(DEFAULT_CONFIG_JSON.as_bytes());
    let mut values = std::collections::BTreeMap::from([
        (
            PLUGIN_CONFIG_PATH_ENV.to_string(),
            path.to_string_lossy().into_owned(),
        ),
        (PLUGIN_BINDING_GENERATION_ENV.to_string(), "2".to_string()),
        (PLUGIN_CONFIG_DIGEST_ENV.to_string(), digest.clone()),
    ]);

    let valid = LaunchInputs::from_lookup(|key| values.get(key).cloned());
    assert!(valid.validate_installed_config_pin().is_ok());
    let parsed = agalma_web_dashboard::config::resolve_installed_config(&path, &digest).unwrap();
    assert_eq!(parsed, parse_config(DEFAULT_CONFIG_JSON).unwrap());

    for (key, invalid_value) in [
        (PLUGIN_CONFIG_PATH_ENV, "relative/config.json".to_string()),
        (PLUGIN_BINDING_GENERATION_ENV, "02".to_string()),
        (PLUGIN_BINDING_GENERATION_ENV, "0".to_string()),
        (PLUGIN_CONFIG_DIGEST_ENV, "sha256:bad".to_string()),
    ] {
        values.insert(key.to_string(), invalid_value);
        let launch = LaunchInputs::from_lookup(|name| values.get(name).cloned());
        assert!(launch.validate_installed_config_pin().is_err(), "{key}");
        values.insert(
            key.to_string(),
            match key {
                PLUGIN_CONFIG_PATH_ENV => path.to_string_lossy().into_owned(),
                PLUGIN_BINDING_GENERATION_ENV => "2".to_string(),
                _ => digest.clone(),
            },
        );
    }
    assert!(LaunchInputs::from_lookup(|_| None)
        .validate_installed_config_pin()
        .is_err());
    assert_eq!(
        agalma_web_dashboard::config::resolve_installed_config(
            &path,
            &sha256_digest(b"different bytes")
        ),
        Err(ConfigError::InstalledConfigDigestMismatch)
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wait_for_path(path: &Path, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{label} did not appear");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn unused_loopback_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve an ephemeral loopback port");
    listener.local_addr().unwrap().port()
}

/// A short socket path: `plugin_socket_path` already falls back for plugin
/// sockets, but a scratch-root hub socket can exceed the platform `SUN_LEN`.
fn short_socket(scratch: &ScratchDir, name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let direct = scratch.child(&format!("{name}.sock"));
    if direct.as_os_str().len() <= 96 {
        return direct;
    }
    std::env::temp_dir().join(format!(
        "agalma-web4-{}-{}-{name}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
