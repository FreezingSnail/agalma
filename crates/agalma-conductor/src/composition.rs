//! Composition root: wires implementations behind Agalma contracts.
//!
//! Dependency direction is `conductor -> implementations -> contracts`. Each
//! implementation crate is constructed here and injected into the executor (the
//! SQLite ledger), the workspace, the process-launched harness, and the loopback
//! provider proxy. No implementation is imported by another implementation; the
//! architecture lint in `tests/architecture.rs` enforces that.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::{
    BindingPin, ContractError, ExecutionApi, ExecutionPhase, FailureCode, LedgerApi,
    PluginInstallation,
};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_process::LocalLauncher;

use crate::cli::PluginHostArgs;
use crate::config::{Config, PluginHostConfig};
use crate::harness_binding::{harness_handle, HarnessChoice};
use crate::harness_process::READY_TIMEOUT;
use crate::ledger_process::{LedgerHandle, LedgerTransport};
use crate::phases::{activities, PhaseDeps, RunState};
use crate::plugin_actions::{
    catalog_key, ActionCatalog, ActionExecutor, ActionService, AdvertisedAction,
    PluginRpcActionExecutor,
};
use crate::plugin_manifest::{DiscoveryDiagnostic, DiscoveredPackage};
use crate::plugin_registry::{
    plugin_socket_path, DesiredState, DesiredStateStore, HubInstallStore, PluginRegistry,
};
use crate::plugin_router::{
    CapabilityAdapter, CapabilityRouter, LaunchGrant, OperationJournal, RouterService,
    RpcHealthProbe, RunningStop, StopAdmission,
};
use crate::plugin_supervisor::{
    BackoffPolicy, Clock, HealthProbe, LaunchRequest, PluginSupervisor, RestartPlan, Spawned,
    SystemClock,
};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::stop::{BoundStopControl, StopControl};
use crate::task::{self, TaskDescriptor};
use crate::watch::{service_action_driver_shared, ActionDriver};
use crate::workspace_process::{WorkspaceHandle, WorkspaceTransport};

/// Repository-mode builder prompt shipped with the conductor. It is the base
/// text for every repository (`agalma work`) build; the per-attempt render adds
/// the frozen task context (`agalma-5fy.1`). Pinned as the builder role prompt
/// bytes at admission (`work::role_inputs`).
pub const BUILDER_PROMPT: &str = include_str!("../prompts/builder.md");

/// Fixture-mode builder prompt (M0): the deliberate one-line broken-function
/// exercise driven by `agalma run --fixture`. Kept on its own prompt path so a
/// repository feature build never inherits the fixture's one-line instruction
/// (`agalma-5fy.1`).
pub const BUILDER_FIXTURE_PROMPT: &str = include_str!("../prompts/builder-fixture.md");

/// Per-launch hub capability grant lifetime (24h), refreshed on each restart.
pub const LAUNCH_GRANT_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Private launch-channel env: the hub capability RPC socket the spoke calls.
pub const HUB_SOCKET_ENV: &str = "AGALMA_HUB_SOCKET";
/// Private launch-channel env: the spoke's per-launch scoped grant token.
pub const PLUGIN_GRANT_ENV: &str = "AGALMA_PLUGIN_GRANT";
/// Private launch-channel env: the capability the grant authorizes.
pub const PLUGIN_GRANT_CAPABILITY_ENV: &str = "AGALMA_PLUGIN_GRANT_CAPABILITY";
/// Private launch-channel env: the spoke's per-launch `hub.PublishReport` grant
/// token (issued only when the plugin declares a report capability).
pub const PLUGIN_REPORT_GRANT_ENV: &str = "AGALMA_PLUGIN_REPORT_GRANT";

/// Default bound on one model turn (free model can be slow; M0 allows minutes).
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Default bound on the verify command.
pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(900);
/// Default idle bound for the progress watchdog (H1): a session with no
/// token/state delta for this long is cancelled and classified `stalled`.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Environment variable overriding [`DEFAULT_IDLE_TIMEOUT`] (whole seconds).
pub const IDLE_TIMEOUT_ENV: &str = "AGALMA_IDLE_TIMEOUT_SECS";
/// Environment variable overriding the designer session turn bound (whole
/// seconds).
///
/// The designer default stays [`DEFAULT_TURN_TIMEOUT`] (600 s): the pilot
/// `agalma-i5h.7` authored 10 children plus edges and crossed 600 s on a slow
/// paid model, so operators raise this per host for large decompositions
/// without touching the worker/concierge/reviewer bounds.
pub const DESIGNER_TIMEOUT_ENV: &str = "AGALMA_DESIGNER_TIMEOUT_SECS";

/// Resolve the idle watchdog bound from [`IDLE_TIMEOUT_ENV`] or the default.
/// A non-numeric or zero value falls back to the default.
pub fn resolve_idle_timeout() -> Duration {
    timeout_from_secs(
        std::env::var(IDLE_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_IDLE_TIMEOUT,
    )
}

/// Resolve the designer session turn bound from [`DESIGNER_TIMEOUT_ENV`] or the
/// default. A non-numeric or zero value falls back to [`DEFAULT_TURN_TIMEOUT`].
pub fn resolve_designer_timeout() -> Duration {
    timeout_from_secs(
        std::env::var(DESIGNER_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_TURN_TIMEOUT,
    )
}

/// Pure whole-seconds parser shared by the idle and designer resolvers: a
/// trimmed positive integer wins, anything else (absent, zero, negative,
/// garbage) falls back to `default`. Kept free of process-env access so it is
/// unit-testable without mutating the environment.
fn timeout_from_secs(raw: Option<&str>, default: Duration) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// The wired service graph for one conductor process.
pub struct Composition {
    /// Serial executor with all five phase activities registered.
    pub executor: Executor<LedgerHandle>,
    /// Parent-owned provider proxy; held for the process lifetime.
    pub proxy: ProviderProxy,
    /// Parsed hardcoded-task descriptor.
    pub task: TaskDescriptor,
    pub state_dir: PathBuf,
    pub fixture: PathBuf,
    pub ledger_path: PathBuf,
    /// Shared bound stop control used by the fixture/run drive loop and phase
    /// polls (`agalma-5fy.5`).
    pub stop: Rc<dyn StopControl>,
}

/// Bounded survivor cleanup used when the stop decision is `Stopped`
/// (`agalma-5fy.5`): terminate live workers recorded for the authoritative bound
/// pending intents, without constructing the normal composition (no
/// proxy/workspace/prompts/session/task admission). Reuses the existing
/// [`crate::workers::reconcile_survivor`] path.
pub(crate) fn reconcile_bound_survivors(state_dir: &Path, ledger: &LedgerHandle) {
    let pending = match ledger.pending_intents() {
        Ok(pending) => pending,
        Err(err) => {
            eprintln!("agalma: survivor cleanup: pending intents unreadable: {err}");
            return;
        }
    };
    let mut launcher = LocalLauncher::new();
    for intent in pending {
        if intent.consumed {
            continue;
        }
        let Some(kind) = intent.payload.get("kind").and_then(|v| v.as_str()) else {
            continue;
        };
        let attempt = intent
            .payload
            .get("attempt")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        if let Err(err) = crate::workers::reconcile_survivor(
            state_dir,
            &intent.execution_id,
            kind,
            attempt,
            &mut launcher,
        ) {
            eprintln!(
                "agalma: survivor cleanup for {} failed: {err}",
                intent.execution_id
            );
        }
    }
}

impl Composition {
    /// Build the full service graph from resolved configuration.
    pub fn build(config: &Config) -> Result<Composition, ContractError> {
        std::fs::create_dir_all(&config.state_dir).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot create state dir {}: {e}",
                config.state_dir.display()
            ))
        })?;
        // Absolute, symlink-resolved state dir: the harness rejects relative
        // `location.directory` values, and Seatbelt subpath matching needs
        // canonical roots.
        let state_dir = std::fs::canonicalize(&config.state_dir).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot resolve state dir {}: {e}",
                config.state_dir.display()
            ))
        })?;
        let fixture =
            std::fs::canonicalize(&config.fixture).unwrap_or_else(|_| config.fixture.clone());

        let ledger_path = state_dir.join("ledger.sqlite");
        // Entry-aware STOP guard before any ledger open (`agalma-5fy.5`): a
        // present stop must never initialize a database or start the proxy.
        if let Some(reason) = crate::stop::observe_sentinel(&state_dir).reason() {
            return Err(ContractError::KnownFailure(format!(
                "stop admission refused: {reason}"
            )));
        }
        // Explicit fresh initialization vs established open through the selected
        // binding (`agalma-5fy.5`): a lost/partial established ledger is refused
        // here, never recreated. The same bound control is shared with the phase
        // polls and the fixture/run drive loop.
        let mut ledger =
            crate::ledger_process::open_admission_ledger(&state_dir, LedgerTransport::from_env())
                .map_err(ContractError::KnownFailure)?;
        let stop: Rc<dyn StopControl> = Rc::new(BoundStopControl::new(&state_dir, ledger.clone()));
        // Typed bound decision BEFORE normal composition (proxy/workspace/prompts/
        // activities/task admission) (`agalma-5fy.5`):
        // - Running: normal composition;
        // - Unavailable: immediate refusal (no attempt to infer/read lost state);
        // - Stopped: bounded existing survivor cleanup over the authoritative
        //   bound pending intents, then refusal — no proxy/workspace/session/task
        //   admission.
        match stop.decision() {
            crate::stop::StopDecision::Running => {}
            crate::stop::StopDecision::Unavailable(reason) => {
                return Err(ContractError::KnownFailure(format!(
                    "stop state unavailable; refusing composition: {reason}"
                )));
            }
            crate::stop::StopDecision::Stopped(reason) => {
                reconcile_bound_survivors(&state_dir, &ledger);
                return Err(ContractError::KnownFailure(format!(
                    "stop policy active; refusing normal composition: {reason}"
                )));
            }
        }

        let task = task::load_task_descriptor(&fixture)
            .map_err(|e| ContractError::KnownFailure(e.to_string()))?;

        let mut executor = Executor::new(
            ledger.clone(),
            ExecutorConfig {
                max_attempts: task.max_attempts,
                execution_definition_version: EXECUTION_DEFINITION_VERSION,
            },
        );

        let workspace =
            WorkspaceHandle::from_transport(&state_dir, WorkspaceTransport::from_env())?;

        // Provider egress for the confined harness (S0b profile denies
        // arbitrary network). The proxy binds a loopback port; its URL is handed
        // to the harness via `OpenCodeHarnessConfig::proxy_url`.
        let proxy = ProviderProxy::start(ProxyConfig::from_env())
            .map_err(|e| ContractError::KnownFailure(format!("provider proxy: {e}")))?;
        // Record the token-free provider credential audit (best effort) through
        // the bound handle, so under `process` transport it rides the conductor's
        // single ledger connection instead of opening a secondary WAL connection.
        if let Err(err) = crate::provider_auth::record_audit_via(&mut ledger, &proxy.auth_state()) {
            eprintln!("agalma: provider_auth audit: {err}");
        }

        let prompts_dir = state_dir.join("prompts");
        std::fs::create_dir_all(&prompts_dir)
            .map_err(|e| ContractError::KnownFailure(format!("cannot create prompts dir: {e}")))?;
        let builder_prompt = prompts_dir.join("builder.md");
        // Fixture runs (`agalma run --fixture`) use the explicit fixture prompt;
        // repository runs materialize the frozen repository prompt through
        // `work::phase_deps` instead (`agalma-5fy.1`).
        std::fs::write(&builder_prompt, BUILDER_FIXTURE_PROMPT).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write builder prompt {}: {e}",
                builder_prompt.display()
            ))
        })?;

        let (_, toolchain_env) = toolchain()?;

        let deps = PhaseDeps {
            state_dir: state_dir.clone(),
            fixture: fixture.clone(),
            model: config.model.clone(),
            verifier_model: config.model.clone(),
            route: "builder:direct".to_string(),
            verifier_route: "verifier:direct".to_string(),
            role: task.role.clone(),
            builder_prompt,
            proxy_url: Some(proxy.url()),
            verifier_proxy_url: None,
            paid_proxy: false,
            toolchain_env,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            verify_timeout: DEFAULT_VERIFY_TIMEOUT,
            idle_timeout: resolve_idle_timeout(),
            repo: None,
            harness: harness_handle(HarnessChoice::OpenCode),
            stop: Rc::clone(&stop),
        };

        let shared_state = Rc::new(RefCell::new(RunState::default()));
        let shared_workspace = Rc::new(RefCell::new(workspace));
        let acts = activities(deps, shared_workspace, shared_state);
        executor.add_activity(ExecutionPhase::Intake, acts.intake);
        executor.add_activity(ExecutionPhase::Checkout, acts.checkout);
        executor.add_activity(ExecutionPhase::Build, acts.build);
        executor.add_activity(ExecutionPhase::Verify, acts.verify);
        executor.add_activity(ExecutionPhase::Integrate, acts.integrate);
        // Boot gates (schema already checked by the ledger open): replay events,
        // projection agreement, and survivor reconciliation. Must run after the
        // activities are registered so probes can terminate survivors.
        executor.recover()?;

        Ok(Composition {
            executor,
            proxy,
            task,
            state_dir,
            fixture,
            ledger_path,
            stop,
        })
    }

    /// The ledger path (signal handler opens a second connection for the latch).
    pub fn ledger_path(&self) -> &Path {
        &self.ledger_path
    }
}

/// Resolve the read-only toolchain roots and environment handed to the confined
/// worker/verify.
///
/// Empirically (`which cargo rustc`, `rustc --print sysroot`) this host runs
/// rustup shims at `$CARGO_HOME/bin/cargo` (default `~/.cargo/bin`) selecting a
/// toolchain under `$RUSTUP_HOME` (default `~/.rustup/toolchains/<host>`). Both
/// homes must be readable inside the sandwich; `RUSTUP_HOME` and
/// `RUSTUP_TOOLCHAIN` must be set because the isolated `HOME` is not the user's.
/// Writes stay denied: only these read-only roots are added.
pub(crate) fn toolchain() -> Result<(Vec<PathBuf>, BTreeMap<String, String>), ContractError> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        ContractError::KnownFailure("HOME is not set; cannot locate toolchain".into())
    })?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".cargo"));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".rustup"));

    let mut roots = Vec::new();
    for candidate in [&cargo_home, &rustup_home] {
        if candidate.is_dir() {
            let canonical = std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.clone());
            roots.push(canonical);
        }
    }

    let cargo_bin = cargo_home.join("bin");
    let cargo_bin = std::fs::canonicalize(&cargo_bin).unwrap_or(cargo_bin);
    let path = format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin",
        cargo_bin.display()
    );

    let mut env = BTreeMap::new();
    env.insert(
        "RUSTUP_HOME".to_string(),
        rustup_home.to_string_lossy().into_owned(),
    );
    env.insert(
        "RUSTUP_TOOLCHAIN".to_string(),
        std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "stable".to_string()),
    );
    env.insert("PATH".to_string(), path);
    Ok((roots, env))
}

// --- HP.3 plugin host assembly ---------------------------------------------
//
// The hub's production plugin host: discover configured roots, install each
// validated package into hub-owned immutable storage, restore durable desired
// state, and run the common supervisor. HP.3 supplies the assembled host and its
// injected seams; HP.4 attaches the real RPC probe and HP.5 attaches the durable
// selected-ledger installation/desired-state stores. Until those land, the
// `agalma plugins` entry runs with the in-memory seams (no plugins start) and a
// configured reporting plugin still needs no paid factory task.

/// The conductor's built-in components, represented in the registry without
/// claiming their implementations have migrated to the plugin protocol.
pub const BUILTIN_COMPONENTS: &[&str] = &[
    "component:ledger",
    "component:workspace",
    "component:harness",
    "component:taskqueue",
    "component:provider-proxy",
];

/// A `Clock` backed by a `Send + Sync` closure.
///
/// The production plugin host needs one injected clock that both the
/// single-threaded supervisor and the `Send`-safe report ingestor can read.
struct FnClock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl Clock for FnClock {
    fn now_ms(&self) -> u64 {
        (self.0)()
    }
}

/// Declared report `(namespace, version)` pairs across every installed
/// generation's manifest — the set the report ingestor accepts.
fn allowed_schemas_in_registry(registry: &PluginRegistry) -> BTreeSet<(String, u64)> {
    let mut out = BTreeSet::new();
    for plugin_id in registry.plugin_ids() {
        if let Some(entry) = registry.entry(plugin_id) {
            for generation in entry.generations.values() {
                for schema in &generation.manifest.report_schemas {
                    out.insert((schema.namespace.clone(), schema.version));
                }
            }
        }
    }
    out
}

/// The report capability an installed generation declares, if any.
///
/// The launch issues a `hub.PublishReport` grant only for a declared report
/// capability, keeping the grant scope narrow (no unrelated action authority).
fn report_capability(
    installed: &crate::plugin_registry::InstalledGeneration,
) -> Option<String> {
    installed
        .approved_capabilities
        .iter()
        .find(|capability| capability.as_str() == "report.publish" || capability.starts_with("report."))
        .cloned()
}

/// The fixed hub capability allowlist for automatic (configured-root) grants.
///
/// A configured, user-controlled plugin root is the operator-managed allowlist:
/// a discovered package auto-installs without a separate approval record, but its
/// automatic grant is the exact intersection of the manifest's requested
/// capabilities and this fixed safe set. `ledger.write` and any unknown or
/// plugin-invented capability are never auto-granted.
pub const SAFE_CAPABILITY_ALLOWLIST: &[&str] = &["report.publish", "ledger.read"];

/// The automatic hub grant for a package: the exact intersection of its
/// requested capabilities and [`SAFE_CAPABILITY_ALLOWLIST`].
///
/// Order follows the request; the result is duplicate-free and never contains an
/// empty, `ledger.write` or unknown capability. It depends only on the request
/// set, so it is a deterministic function the host re-derives identically on
/// every restart.
pub fn auto_approved_capabilities(requested: &[String]) -> Vec<String> {
    let mut approved: Vec<String> = Vec::new();
    for capability in requested {
        if !SAFE_CAPABILITY_ALLOWLIST.contains(&capability.as_str()) {
            continue;
        }
        if approved.iter().any(|seen| seen == capability) {
            continue;
        }
        approved.push(capability.clone());
    }
    approved
}

/// Synthesize the immutable default `PluginInstallation/v1` pin for a discovered
/// package in a configured root (configured-root auto-install).
///
/// The pin is a pure function of the sealed package bytes — its package digest,
/// the manifest's declared **default configuration** reference, its launch
/// profile and the automatic capability intersection
/// ([`auto_approved_capabilities`]) — so it reconstructs with a stable identity
/// on every restart. No separate operator approval record is required for a
/// configured root; an injected [`InstallationSource`] remains the durable
/// operator override, and this default is used only when none is supplied.
/// Validation stays authoritative in [`verify_installation`] and
/// [`PluginRegistry::install`].
pub fn default_installation(
    package: &DiscoveredPackage,
) -> Result<PluginInstallation, ContractError> {
    let mut installation = PluginInstallation {
        format: agalma_contracts::PLUGIN_INSTALLATION_FORMAT.to_string(),
        plugin_id: package.plugin_id.clone(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: auto_approved_capabilities(
            &package.manifest.requested_capabilities,
        ),
        installation_digest: String::new(),
    };
    installation.installation_digest = installation.recomputed_installation_digest()?;
    Ok(installation)
}

/// Injected source of pinned [`PluginInstallation`] records.
///
/// HP.5 attaches the durable selected-ledger implementation. A package with no
/// supplied installation is discovered but not started (an explicit diagnostic),
/// so a hub never launches an unapproved generation.
pub trait InstallationSource {
    /// The approved installation for `plugin_id`, if any.
    fn installation(&self, plugin_id: &str) -> Option<PluginInstallation>;
}

/// In-memory installation source (test/stand-in double).
#[derive(Clone, Debug, Default)]
pub struct InMemoryInstallations {
    map: BTreeMap<String, PluginInstallation>,
}

impl InMemoryInstallations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `installation` under its plugin id.
    pub fn insert(&mut self, installation: PluginInstallation) {
        self.map
            .insert(installation.plugin_id.clone(), installation);
    }
}

impl InstallationSource for InMemoryInstallations {
    fn installation(&self, plugin_id: &str) -> Option<PluginInstallation> {
        self.map.get(plugin_id).cloned()
    }
}

/// One assembled, installable plugin entry recorded by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub plugin_id: String,
    pub generation: u64,
    pub package_digest: String,
}

/// Injected hub capability-service seams (HP.4; HP.5 owns durable persistence).
///
/// The hub router needs an operation journal (durable effect dedup), a stop
/// admission policy (SF stop authority), and the authorized typed adapters.
/// HP.5 replaces the volatile default journal with the selected-ledger store;
/// HP.7 owns the action service. Nothing here claims durable production
/// action recovery yet.
pub struct HubServices {
    pub journal: Box<dyn OperationJournal>,
    pub stop: Arc<dyn StopAdmission>,
    pub adapters: Vec<Box<dyn CapabilityAdapter>>,
    /// Durable `hub.PublishReport` ingestor (HP.5). `None` refuses report routing.
    pub reports: Option<Arc<std::sync::Mutex<crate::plugin_reports::ReportIngestor>>>,
}

impl Default for HubServices {
    fn default() -> Self {
        HubServices {
            journal: Box::new(crate::plugin_router::InMemoryOperationJournal::new()),
            stop: Arc::new(RunningStop),
            adapters: Vec::new(),
            reports: None,
        }
    }
}

/// The assembled plugin host.
pub struct PluginHost {
    pub state_dir: PathBuf,
    pub roots: Vec<crate::plugin_manifest::PluginRoot>,
    pub install_store: HubInstallStore,
    pub registry: PluginRegistry,
    pub supervisor: PluginSupervisor,
    /// Non-fatally skipped packages (e.g. no approved installation).
    pub diagnostics: Vec<DiscoveryDiagnostic>,
    /// Generation installed for each discovered plugin id.
    pub installed: BTreeMap<String, u64>,
    /// The hub's capability RPC service (served by [`crate::plugin_router::serve_hub`]).
    pub router: Arc<RouterService>,
    /// The hub capability RPC socket spokes call back on.
    pub hub_socket: PathBuf,
    /// Per-launch grants issued for installed plugins (private launch channel).
    grants: BTreeMap<String, LaunchGrant>,
    /// Per-launch `hub.PublishReport` grants (private launch channel).
    report_grants: BTreeMap<String, LaunchGrant>,
    desired: Box<dyn DesiredStateStore>,
    /// The selected durable ledger attached by [`PluginHost::assemble_with_ledger`].
    ledger: Option<LedgerHandle>,
    /// The durable report ingestor attached by [`PluginHost::assemble_with_ledger`].
    pub reports: Option<Arc<std::sync::Mutex<crate::plugin_reports::ReportIngestor>>>,
    /// Live child Connect socket per `(plugin_id, pinned generation)`, refreshed
    /// from the supervisor on each launch tick (HP.7 action-specific attachment).
    /// The action executor resolves the pinned generation's socket here; a pinned
    /// generation with no live launch is refused and a replacement generation is
    /// never contacted.
    action_sockets: Arc<Mutex<BTreeMap<(String, u64), PathBuf>>>,
    /// The host's current per-launch action grants, shared with the live action
    /// driver so a restart's rotated grant replaces/revokes the stale one in the
    /// active service (HP.7 action-specific attachment).
    action_grants: Arc<Mutex<BTreeMap<String, LaunchGrant>>>,
}

/// What one supervise tick observed and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuperviseOutcome {
    /// Children whose unexpected exit was observed and scheduled for restart.
    pub crashed: Vec<RestartPlan>,
    /// Plugins restarted and health-confirmed this tick.
    pub restarted: Vec<Spawned>,
}

impl PluginHost {
    /// Discover roots, install validated packages and restore desired state.
    ///
    /// `desired` is the injected persistence seam: HP.5 attaches the durable
    /// selected-ledger store here without changing HP.3 wiring; tests inject a
    /// fake store. `installations` is the injected source of pinned
    /// `PluginInstallation/v1` records.
    pub fn assemble(
        config: &PluginHostConfig,
        installations: &dyn InstallationSource,
        desired: Box<dyn DesiredStateStore>,
        clock: Rc<dyn Clock>,
    ) -> Result<PluginHost, ContractError> {
        PluginHost::assemble_with_hub(config, installations, desired, clock, HubServices::default())
    }

    /// Assemble the plugin host with injected hub capability-service seams
    /// (`agalma-jmr.4`).
    ///
    /// Same as [`PluginHost::assemble`], but the hub capability router is built
    /// from the injected journal/stop/adapters and a per-launch scoped grant is
    /// issued for each installed plugin, carried to the child through the private
    /// launch channel ([`LaunchRequest::env`]). Persistence stays injected until
    /// HP.5. A package with no supplied installation is discovered but not
    /// started; the configured-root auto-install path is
    /// [`PluginHost::assemble_with_ledger`].
    pub fn assemble_with_hub(
        config: &PluginHostConfig,
        installations: &dyn InstallationSource,
        desired: Box<dyn DesiredStateStore>,
        clock: Rc<dyn Clock>,
        services: HubServices,
    ) -> Result<PluginHost, ContractError> {
        PluginHost::assemble_inner(config, installations, desired, clock, services, false)
    }

    /// Shared assembly behind [`PluginHost::assemble_with_hub`] (explicit
    /// installation source only) and [`PluginHost::assemble_with_ledger`]
    /// (configured-root auto-install). `auto_install` selects whether a
    /// discovered package with no supplied installation pin is auto-installed
    /// from its immutable defaults or reported as an unapproved diagnostic.
    fn assemble_inner(
        config: &PluginHostConfig,
        installations: &dyn InstallationSource,
        desired: Box<dyn DesiredStateStore>,
        clock: Rc<dyn Clock>,
        services: HubServices,
        auto_install: bool,
    ) -> Result<PluginHost, ContractError> {
        std::fs::create_dir_all(&config.state_dir).map_err(|err| {
            FailureCode::StateUnavailable.failure(
                config.state_dir.display().to_string(),
                format!("cannot create plugin-host state dir: {err}"),
            )
        })?;
        let state_dir = std::fs::canonicalize(&config.state_dir).unwrap_or_else(|_| {
            config.state_dir.clone()
        });

        let report = crate::plugin_manifest::discover_plugins(&config.plugin_roots)?;
        let store = HubInstallStore::new(&state_dir);
        let mut registry = PluginRegistry::new();
        let now = clock.now_ms();

        // Restore the durable installed generations **before** discovery can
        // install anything. A hub restart must not reallocate a generation for a
        // package already installed under the selected ledger's registry
        // checkpoint (that would redirect a prior binding pin or action receipt
        // to a different executable). Reconstruction is bounded to the accepted
        // configured-root auto-install policy: each historical generation is
        // re-verified against the hub-owned immutable copy and pinned with either
        // an operator installation whose package digest matches the stored bytes
        // or the package's sealed default configuration with the fixed automatic
        // grant intersection.
        if auto_install {
            if let Some(checkpoint) = desired.checkpoint().cloned() {
                restore_installed_generations(&mut registry, &store, &checkpoint)?;
            }
        }

        for component in BUILTIN_COMPONENTS {
            registry.register_builtin(component, now);
        }

        let mut diagnostics = report.diagnostics.clone();
        let mut installed = BTreeMap::new();
        for package in &report.packages {
            let installation = match installations.installation(&package.plugin_id) {
                Some(installation) => installation,
                // Configured-root auto-install: no separate approval record is
                // required, so synthesize the immutable default pin (pinned
                // package digest + default configuration + intersected grant).
                None if auto_install => default_installation(package)?,
                None => {
                    diagnostics.push(DiscoveryDiagnostic {
                        root: package.root.clone(),
                        message: format!(
                            "plugin `{}` has no approved installation; discovered but not started",
                            package.plugin_id
                        ),
                    });
                    continue;
                }
            };
            verify_installation(package, &installation)?;
            // Reuse the persisted generation when this exact sealed package and
            // pinned installation are already installed; otherwise allocate the
            // next monotonic generation. Either way the discovered generation is
            // staged: it becomes active only after it is Ready and activation is
            // receipted (frozen contract), so new work never switches onto a
            // candidate that has not passed startup/health gating.
            let (generation, _reused) = registry.install_or_reuse(&store, package, &installation)?;
            registry.stage(&package.plugin_id, generation)?;
            installed.insert(package.plugin_id.clone(), generation);
        }

        let mut desired = desired;
        registry.load_desired(desired.as_mut())?;

        let supervisor = PluginSupervisor::new(clock.clone(), BackoffPolicy::default(), READY_TIMEOUT);

        // Hub capability service (HP.4). Grants are NOT issued at assembly: a
        // fresh launch-scoped grant is issued for every launch epoch by
        // [`PluginHost::launch_request`], so a restart rotates the token while
        // the immutable binding scope stays the same.
        let mut router = CapabilityRouter::new(services.journal, services.stop);
        for adapter in services.adapters {
            router.register_adapter(adapter);
        }
        let reports = services.reports;
        if let Some(ingestor) = &reports {
            router.install_reports(Arc::clone(ingestor));
        }
        let router = Arc::new(RouterService::new(router));
        let hub_socket = plugin_socket_path(&state_dir, "hub", 0);

        Ok(PluginHost {
            state_dir,
            roots: config.plugin_roots.clone(),
            install_store: store,
            registry,
            supervisor,
            diagnostics,
            installed,
            router,
            hub_socket,
            grants: BTreeMap::new(),
            report_grants: BTreeMap::new(),
            desired,
            ledger: None,
            reports,
            action_sockets: Arc::new(Mutex::new(BTreeMap::new())),
            action_grants: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Assemble the production plugin host attached to the **selected ledger**
    /// (`agalma-jmr.5`).
    ///
    /// Desired state is persisted/reconstructed through the durable
    /// [`crate::plugin_registry::LedgerDesiredStateStore`] (a selected-ledger
    /// component commit), never an in-memory map or private file database. The
    /// host keeps the attached ledger so report ingestion uses the same
    /// `LedgerApi`, and it records the registry's active/max generations in the
    /// durable registry checkpoint.
    ///
    /// Configured, user-controlled plugin roots auto-install: a discovered
    /// package with no supplied [`InstallationSource`] pin is installed from its
    /// immutable default configuration with an automatic grant that is exactly
    /// its requested capabilities intersected with
    /// [`SAFE_CAPABILITY_ALLOWLIST`] ([`default_installation`]). The synthesized
    /// pin is a pure function of the sealed package bytes, so it reconstructs
    /// identically across restarts.
    pub fn assemble_with_ledger(
        config: &PluginHostConfig,
        installations: &dyn InstallationSource,
        ledger: LedgerHandle,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<PluginHost, ContractError> {
        let desired = Box::new(crate::plugin_registry::LedgerDesiredStateStore::open(
            ledger.clone(),
        )?);
        let mut host = PluginHost::assemble_inner(
            config,
            installations,
            desired,
            Rc::new(FnClock(Arc::clone(&clock))),
            HubServices::default(),
            true,
        )?;
        let mut ledger = ledger;
        crate::plugin_registry::persist_registry_generations(&mut ledger, &host.registry)?;

        // Durable report ingestion over the **selected** `LedgerHandle`
        // (`component-store/v1`), attached to the host router so the production
        // `hub.PublishReport` RPC routes to it. No second connection is opened:
        // the same handle the host uses for desired state also stores reports.
        let schemas = allowed_schemas_in_registry(&host.registry);
        let ingestor = crate::plugin_reports::ReportIngestor::open(
            Box::new(std::sync::Mutex::new(ledger.clone())),
            Arc::clone(&clock),
            host.active_generations(),
            schemas,
        )?;
        let reports = Arc::new(std::sync::Mutex::new(ingestor));
        host.router.router().install_reports(Arc::clone(&reports));
        host.reports = Some(reports);
        host.ledger = Some(ledger);
        Ok(host)
    }

    /// The selected durable ledger attached to this host, when one is.
    pub fn ledger(&self) -> Option<&LedgerHandle> {
        self.ledger.as_ref()
    }

    /// The plugin -> active/pending generation map the registry is running.
    ///
    /// Used to open the [`crate::plugin_reports::ReportIngestor`] against the
    /// same selected ledger.
    pub fn active_generations(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for plugin_id in self.registry.plugin_ids() {
            if let Some(entry) = self.registry.entry(plugin_id) {
                if let Some(generation) = entry.active_generation.or(entry.pending_generation) {
                    out.insert(plugin_id.to_string(), generation);
                }
            }
        }
        out
    }

    /// Desired-state store (HP.5 attaches the durable implementation).
    pub fn desired_mut(&mut self) -> &mut dyn DesiredStateStore {
        self.desired.as_mut()
    }

    /// Record `plugin_id`'s desired state through the injected store.
    pub fn set_desired(
        &mut self,
        plugin_id: &str,
        desired: DesiredState,
    ) -> Result<(), ContractError> {
        let now = self.supervisor.now_ms();
        self.registry
            .set_desired(plugin_id, desired, self.desired.as_mut(), now)
    }

    /// Start every plugin whose desired state is `Running`.
    ///
    /// A plugin's health gate must pass before it is recorded `Ready`; a failed
    /// start is recorded (`Failed`) and its backoff scheduled, and the next
    /// plugin is attempted. Returns the successfully started plugins.
    pub fn start_desired(
        &mut self,
        probe: &mut dyn HealthProbe,
    ) -> Result<Vec<Spawned>, ContractError> {
        let ids: Vec<String> = self
            .registry
            .plugin_ids()
            .into_iter()
            .filter(|id| {
                self.registry.desired(id) == DesiredState::Running
                    && self.registry.entry(id).is_some_and(|entry| {
                        entry.pending_generation.is_some() || entry.active_generation.is_some()
                    })
            })
            .map(str::to_string)
            .collect();

        let mut started = Vec::new();
        for id in ids {
            let request = match self.launch_request(&id) {
                Ok(request) => request,
                Err(err) => {
                    eprintln!("agalma: plugin `{id}` launch refused: {err:?}");
                    continue;
                }
            };
            match self.supervisor.spawn(&mut self.registry, &request) {
                Ok(spawned) => {
                    if self.supervisor.confirm_ready(&mut self.registry, &id, probe)? {
                        started.push(spawned);
                    }
                }
                Err(err) => {
                    eprintln!("agalma: plugin `{id}` did not start: {err:?}");
                }
            }
        }
        Ok(started)
    }

    /// Build the launch request for `plugin_id`'s candidate or active generation.
    ///
    /// A staged candidate (still pending startup) is launched first; once Ready
    /// it becomes active, so a later restart relaunches the active generation.
    /// Rehashes the hub-owned installed copy first (frozen contract: rehash on
    /// install, stage and launch); a tampered installed generation is refused.
    ///
    /// Every call is one launch: it issues a **fresh** launch-scoped grant for
    /// the launch epoch and revokes the prior token, so a restart never reuses a
    /// token while the immutable binding scope (plugin/generation/method/
    /// capability) is unchanged. The token travels only through the private
    /// launch channel ([`LaunchRequest::env`]).
    pub fn launch_request(&mut self, plugin_id: &str) -> Result<LaunchRequest, ContractError> {
        let (generation, installed) = {
            let entry = self.registry.entry(plugin_id).ok_or_else(|| {
                FailureCode::InvalidRecord.failure(plugin_id, "plugin is not registered")
            })?;
            let generation = entry
                .pending_generation
                .or(entry.active_generation)
                .ok_or_else(|| {
                    FailureCode::InvalidRecord.failure(
                        plugin_id,
                        "plugin has no staged candidate or active generation",
                    )
                })?;
            let installed = entry.generations.get(&generation).cloned().ok_or_else(|| {
                FailureCode::StaleGeneration.failure(
                    plugin_id,
                    format!("active generation {generation} is not installed"),
                )
            })?;
            (generation, installed)
        };
        installed.rehash()?;
        let epoch = self
            .registry
            .entry(plugin_id)
            .map(|entry| entry.launch_epoch + 1)
            .unwrap_or(1);
        let socket = plugin_socket_path(&self.state_dir, plugin_id, epoch);
        let mut request = LaunchRequest::from_installed(&installed, socket);

        // Rotate the per-launch grants: revoke the prior tokens, then issue
        // fresh tokens for the identical immutable binding scope.
        let binding = agalma_contracts::BindingPin {
            binding_id: plugin_id.to_string(),
            api: "plugin".to_string(),
            api_version: 1,
            implementation_version: installed.package_version.clone(),
            generation,
            config_digest: installed.config_digest.clone(),
        };
        let expiry =
            crate::plugin_router::wall_clock_now_ms().saturating_add(LAUNCH_GRANT_TTL_MS);
        if let Some(capability) = installed.approved_capabilities.first().cloned() {
            if let Some(previous) = self.grants.remove(plugin_id) {
                self.router.router().revoke_grant(&previous.fingerprint());
            }
            let grant = LaunchGrant::for_capability(
                plugin_id.to_string(),
                binding.clone(),
                agalma_ipc::plugin_server::METHOD_INVOKE_CAPABILITY,
                capability,
                expiry,
                None,
            )?;
            self.router.router().install_grant(&grant);
            request
                .env
                .insert(PLUGIN_GRANT_ENV.to_string(), grant.token_hex());
            request.env.insert(
                PLUGIN_GRANT_CAPABILITY_ENV.to_string(),
                grant.scope().resource_namespace.clone(),
            );
            self.grants.insert(plugin_id.to_string(), grant);
        }

        // A plugin that declares a report capability gets a separate,
        // launch-scoped `hub.PublishReport` grant. The scope is narrow: the
        // pinned binding/generation plus the report capability namespace only.
        if let Some(capability) = report_capability(&installed) {
            if let Some(previous) = self.report_grants.remove(plugin_id) {
                self.router.router().revoke_grant(&previous.fingerprint());
            }
            let grant = LaunchGrant::for_capability(
                plugin_id.to_string(),
                binding,
                agalma_ipc::plugin_server::METHOD_PUBLISH_REPORT,
                capability,
                expiry,
                None,
            )?;
            self.router.router().install_grant(&grant);
            request
                .env
                .insert(PLUGIN_REPORT_GRANT_ENV.to_string(), grant.token_hex());
            self.report_grants.insert(plugin_id.to_string(), grant);
        }

        // Private launch channel: the hub socket and the spoke's per-launch
        // scoped grant (never a peer socket or database handle).
        request.env.insert(
            HUB_SOCKET_ENV.to_string(),
            self.hub_socket.to_string_lossy().into_owned(),
        );
        Ok(request)
    }

    /// The per-launch grant issued for `plugin_id`, if any (private launch
    /// channel; the token is never logged).
    pub fn launch_grant(&self, plugin_id: &str) -> Option<&LaunchGrant> {
        self.grants.get(plugin_id)
    }

    /// The per-launch `hub.PublishReport` grant issued for `plugin_id`, if the
    /// plugin declares a report capability (private launch channel).
    pub fn report_grant(&self, plugin_id: &str) -> Option<&LaunchGrant> {
        self.report_grants.get(plugin_id)
    }

    /// Every issued per-launch grant, keyed by plugin id (diagnostics/tests).
    pub fn launch_grants(&self) -> &BTreeMap<String, LaunchGrant> {
        &self.grants
    }

    /// The host's current per-launch action grants, shared with the live action
    /// driver (so a restart's rotation reaches the active service). Refreshed by
    /// [`Self::refresh_action_sockets`].
    pub fn action_grants(&self) -> Arc<Mutex<BTreeMap<String, LaunchGrant>>> {
        Arc::clone(&self.action_grants)
    }

    /// Refresh the live `(plugin_id, generation)` child-socket map and the shared
    /// per-launch action grants from the supervisor/registry (HP.7 action
    /// attachment). The host calls this on each supervise/UI tick so a restart's
    /// new launch epoch and rotated grant are visible to the action executor and
    /// driver, and a replaced generation's socket is dropped (never reused).
    pub fn refresh_action_sockets(&self) {
        {
            let mut sockets = self
                .action_sockets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sockets.clear();
            for plugin_id in self.registry.plugin_ids() {
                let Some(entry) = self.registry.entry(plugin_id) else {
                    continue;
                };
                // Only the currently-launched generation is reachable; a replaced
                // generation has no entry, so its pinned operations stay
                // unreconciled rather than being sent to the replacement.
                let Some(generation) = entry
                    .active_generation
                    .or(entry.pending_generation)
                else {
                    continue;
                };
                let Some(epoch) = self.supervisor.epoch(plugin_id) else {
                    continue;
                };
                sockets.insert(
                    (plugin_id.to_string(), generation),
                    plugin_socket_path(&self.state_dir, plugin_id, epoch),
                );
            }
        }
        let mut grants = self
            .action_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        grants.clear();
        for (plugin_id, grant) in &self.grants {
            grants.insert(plugin_id.clone(), grant.clone());
        }
    }

    /// The registry-derived action catalog: each plugin's advertised
    /// `ActionDescriptor`s bound to its **active** pinned generation. A staged or
    /// pending generation advertises nothing until it is active.
    fn action_catalog(&self) -> BTreeMap<(String, u64, String), AdvertisedAction> {
        let mut catalog = BTreeMap::new();
        for plugin_id in self.registry.plugin_ids() {
            let Some(entry) = self.registry.entry(plugin_id) else {
                continue;
            };
            let Some(generation) = entry.active_generation else {
                continue;
            };
            let Some(installed) = entry.generations.get(&generation) else {
                continue;
            };
            let binding = BindingPin {
                binding_id: plugin_id.to_string(),
                api: "plugin".to_string(),
                api_version: 1,
                implementation_version: installed.package_version.clone(),
                generation,
                config_digest: installed.config_digest.clone(),
            };
            for descriptor in &installed.manifest.actions {
                catalog.insert(
                    catalog_key(plugin_id, generation, &descriptor.action_id),
                    AdvertisedAction {
                        descriptor: descriptor.clone(),
                        binding: binding.clone(),
                    },
                );
            }
        }
        catalog
    }

    /// Build the service-backed UI action driver from this running host: the
    /// registry-derived catalog, the selected-ledger durable action store, the
    /// pinned-generation child-RPC executor, the shared stop admission and each
    /// active plugin's per-launch grant (HP.7 action-specific production
    /// attachment).
    ///
    /// Returns `Ok(None)` — fail-closed — when the host has no selected ledger to
    /// journal into; the interactive UI then refuses every action with zero
    /// effects. When the host was assembled with a ledger, prefer
    /// [`Self::action_driver`]; otherwise pass the selected ledger explicitly
    /// with [`Self::action_driver_with_ledger`].
    pub fn action_driver(&self) -> Result<Option<Box<dyn ActionDriver + Send>>, ContractError> {
        match self.ledger() {
            Some(ledger) => Ok(Some(self.action_driver_with_ledger(ledger)?)),
            None => Ok(None),
        }
    }

    /// Build the service-backed UI action driver over the **selected ledger**,
    /// without changing the host's desired-state/lifecycle wiring. The ledger is
    /// the durable `component-store/v1` backing for the action journal.
    pub fn action_driver_with_ledger(
        &self,
        ledger: &LedgerHandle,
    ) -> Result<Box<dyn ActionDriver + Send>, ContractError> {
        let service = self.action_service_with_ledger(ledger)?;
        // Share the host's current per-launch grants so a restart's rotation
        // replaces/revokes the stale grant in the active service before further
        // actions.
        Ok(service_action_driver_shared(
            service,
            Arc::clone(&self.action_grants),
        ))
    }

    /// Build the common validated [`ActionService`] from this running host over
    /// the selected ledger: the registry-derived catalog (active pinned
    /// generation), the durable `component-store/v1` action store, the
    /// pinned-generation child-RPC executor, the shared SF stop admission and
    /// each active plugin's per-launch grant. The host caller/test seam behind
    /// [`Self::action_driver_with_ledger`].
    pub fn action_service_with_ledger(
        &self,
        ledger: &LedgerHandle,
    ) -> Result<ActionService, ContractError> {
        self.refresh_action_sockets();
        let catalog = self.action_catalog();
        let store: Box<dyn crate::plugin_actions::ActionStore> =
            Box::new(Mutex::new(ledger.clone()));
        let executor: Box<dyn ActionExecutor> =
            Box::new(PluginRpcActionExecutor::new(Arc::clone(&self.action_sockets))?);
        // The shared SF stop admission: an explicit stop or an unreadable stop
        // state denies new effects (fail-closed).
        let stop: Arc<dyn StopAdmission> =
            Arc::new(BoundStopControl::new(&self.state_dir, ledger.clone()));
        let mut service = ActionService::new(
            Box::new(catalog) as Box<dyn ActionCatalog>,
            store,
            executor,
            stop,
        );
        for grant in self.grants.values() {
            service.install_grant(grant);
        }
        Ok(service)
    }

    /// Advance the production supervise loop one tick.
    ///
    /// Observes owned child exits (scheduling their backoff restart) and then
    /// restarts any scheduled plugin whose backoff is due, health-gating the
    /// replacement under the injected clock. The logical generation is
    /// unchanged; the replacement reuses the pinned generation under a fresh
    /// launch epoch.
    pub fn supervise_tick(
        &mut self,
        probe: &mut dyn HealthProbe,
    ) -> Result<SuperviseOutcome, ContractError> {
        let crashed = self.supervisor.reap_exited(&mut self.registry)?;
        let due = self.supervisor.take_due_restarts(self.supervisor.now_ms());
        let mut restarted = Vec::new();
        for id in due {
            let request = match self.launch_request(&id) {
                Ok(request) => request,
                Err(err) => {
                    eprintln!("agalma: plugin `{id}` restart refused: {err:?}");
                    continue;
                }
            };
            match self.supervisor.spawn(&mut self.registry, &request) {
                Ok(spawned) => {
                    if self.supervisor.confirm_ready(&mut self.registry, &id, probe)? {
                        restarted.push(spawned);
                    }
                }
                Err(err) => {
                    eprintln!("agalma: plugin `{id}` restart failed: {err:?}");
                }
            }
        }
        Ok(SuperviseOutcome { crashed, restarted })
    }

    /// Stop the whole owned plugin group and return cleanup evidence.
    pub fn shutdown(&mut self) -> Vec<(String, StopEvidence)> {
        self.supervisor.stop_all()
    }
}

/// Verify an approved `PluginInstallation/v1` pins this package.
///
/// Validates the record's format/digest, plugin id and package digest, then its
/// launch profile, its frozen configuration ref against the package bytes, and
/// its approved grants against the manifest's requested capabilities (a grant
/// must be requested, unique and non-empty). HP.3 has no operator configuration
/// store yet, so the pinned configuration ref must resolve inside the package;
/// an external mutable configuration source attaches with HP.5.
fn verify_installation(
    package: &DiscoveredPackage,
    installation: &PluginInstallation,
) -> Result<(), ContractError> {
    let plugin_id = package.plugin_id.as_str();
    installation.version_gate()?;
    installation.verify_installation_digest()?;
    if installation.plugin_id != plugin_id {
        return Err(FailureCode::IdentityConflict.failure(
            plugin_id,
            format!(
                "installation is for `{}`, not `{plugin_id}`",
                installation.plugin_id
            ),
        ));
    }
    if installation.package_digest != package.package_digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            plugin_id,
            format!(
                "installation pins {} but discovered package is {}",
                installation.package_digest, package.package_digest
            ),
        ));
    }
    if installation.launch_profile != package.manifest.entrypoint.launch_profile {
        return Err(FailureCode::InvalidRecord.failure(
            plugin_id,
            format!(
                "installation launch profile `{}` != package `{}`",
                installation.launch_profile, package.manifest.entrypoint.launch_profile
            ),
        ));
    }

    let requested: BTreeSet<&str> = package
        .manifest
        .requested_capabilities
        .iter()
        .map(String::as_str)
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for capability in &installation.approved_capabilities {
        if capability.is_empty() {
            return Err(FailureCode::InvalidRecord.failure(
                plugin_id,
                "installation approves an empty capability",
            ));
        }
        if !seen.insert(capability.as_str()) {
            return Err(FailureCode::InvalidRecord.failure(
                plugin_id,
                format!("installation approves `{capability}` more than once"),
            ));
        }
        if !requested.contains(capability.as_str()) {
            return Err(FailureCode::ScopeDenied.failure(
                plugin_id,
                format!("installation grants `{capability}`, which the package did not request"),
            ));
        }
    }

    // The frozen configuration ref must verify against the validated package.
    crate::plugin_manifest::verify_artifact_ref(
        &package.package_dir,
        plugin_id,
        "installation.configuration",
        &installation.configuration,
    )?;
    Ok(())
}

/// Restore the durable installed generations for every plugin named by the
/// selected ledger's [`RegistryCheckpoint`](crate::plugin_registry::RegistryCheckpoint).
///
/// Each generation present in the hub-owned immutable store is re-verified and
/// reconstructed before discovery runs, so a restart keeps the prior binding
/// generation for an unchanged package and never reallocates a used generation
/// number. Reconstruction is bounded to the accepted configured-root
/// auto-install policy: [`reconstruct_installation`] uses an operator pin whose
/// package digest matches the stored bytes, or the package's sealed default
/// configuration with the fixed automatic grant intersection. It never invents a
/// broader configuration/grant policy.
fn restore_installed_generations(
    registry: &mut PluginRegistry,
    store: &HubInstallStore,
    checkpoint: &crate::plugin_registry::RegistryCheckpoint,
) -> Result<(), ContractError> {
    let mut plugin_ids: BTreeSet<&str> = BTreeSet::new();
    plugin_ids.extend(checkpoint.active_generations.keys().map(String::as_str));
    plugin_ids.extend(checkpoint.max_generations.keys().map(String::as_str));
    for plugin_id in plugin_ids {
        let checkpoint_max = checkpoint
            .max_generations
            .get(plugin_id)
            .copied()
            .unwrap_or(0);
        let active = checkpoint
            .active_generations
            .get(plugin_id)
            .copied()
            .filter(|generation| *generation != 0);
        let generations = store.installed_generations(plugin_id)?;
        let mut ceiling = checkpoint_max.max(active.unwrap_or(0));
        let mut reconstructed = Vec::new();
        let mut active_reconstructed = false;
        for generation in generations {
            ceiling = ceiling.max(generation);
            let Some(package) = store.load_installed_package(plugin_id, generation)? else {
                continue;
            };
            let installation = reconstruct_installation(store, plugin_id, generation, &package)?;
            reconstructed.push(crate::plugin_registry::seal_installed_generation(
                &package.package_dir,
                &package,
                generation,
                &installation,
            )?);
            if Some(generation) == active {
                active_reconstructed = true;
            }
        }
        // Fail closed: a checkpoint that names an active generation whose
        // immutable hub copy is absent or cannot be reconstructed must refuse
        // assembly, never silently drop the active pointer (which would let the
        // next discovery reallocate the identity that pinned work and action
        // receipts are bound to).
        if let Some(generation) = active {
            if !active_reconstructed {
                return Err(FailureCode::StateUnavailable.failure(
                    plugin_id,
                    format!(
                        "registry checkpoint names active generation {generation}, but its \
                         immutable hub copy is absent or could not be reconstructed"
                    ),
                ));
            }
        }
        for installed in reconstructed {
            registry.restore_generation(installed);
        }
        registry.restore_bounds(plugin_id, ceiling, active);
    }
    Ok(())
}

/// Reconstruct the pinned [`PluginInstallation`] for a stored immutable
/// generation from the pin persisted beside its hub-owned copy.
///
/// The **exact** original pin (configuration ref, grant set, launch profile and
/// digest) is restored, not a re-derived default: a replacement package or a
/// changed current `InstallationSource` must never silently change an older
/// generation's binding identity or authority. The pin is re-validated with
/// [`verify_installation`] against the stored bytes. A generation with no
/// persisted pin is refused (fail closed) rather than fabricated.
fn reconstruct_installation(
    store: &HubInstallStore,
    plugin_id: &str,
    generation: u64,
    package: &DiscoveredPackage,
) -> Result<PluginInstallation, ContractError> {
    let Some(pin) = store.load_installation(plugin_id, generation)? else {
        return Err(FailureCode::StateUnavailable.failure(
            plugin_id,
            format!(
                "installed generation {generation} has no persisted installation pin; \
                 refusing to fabricate a replacement identity"
            ),
        ));
    };
    verify_installation(package, &pin)?;
    Ok(pin)
}

/// Run the HP.3 production plugin host for one command invocation.
///
/// `--once` assembles, starts the desired plugins, records their state and
/// shuts the group down. Without `--once` it supervises until the stop sentinel
/// appears. It never claims a paid queue task and requires no fixture/model.
pub async fn run_plugin_host(args: &PluginHostArgs) -> std::process::ExitCode {
    let config = match PluginHostConfig::resolve(args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("agalma: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(err) = std::fs::create_dir_all(&config.state_dir) {
        eprintln!(
            "agalma: cannot create state dir {}: {err}",
            config.state_dir.display()
        );
        return std::process::ExitCode::FAILURE;
    }
    // HP.5: configured, user-controlled plugin roots auto-install from each
    // package's declared default configuration through the selected LedgerApi
    // (durable desired state, registry checkpoint and report ingestion); there is
    // no private-storage fallback. No separate installation approval record is
    // required — the configured root is the operator-managed allowlist.
    let installations = InMemoryInstallations::new();
    let ledger = match LedgerHandle::from_transport(&config.state_dir, LedgerTransport::from_env())
    {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!("agalma: plugin ledger unavailable: {err:?}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| SystemClock.now_ms());
    let mut host = match PluginHost::assemble_with_ledger(&config, &installations, ledger, clock) {
        Ok(host) => host,
        Err(err) => {
            eprintln!("agalma: plugin host assembly failed: {err:?}");
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!(
        "agalma: plugin host state_dir={} roots={} installed={} diagnostics={} grant_plugins={}",
        host.state_dir.display(),
        host.roots.len(),
        host.installed.len(),
        host.diagnostics.len(),
        host.launch_grants().len()
    );
    // Serve the hub capability RPC so spokes can call back with their per-launch
    // grant; HP.5 attaches durable persistence to the router's journal.
    {
        let hub_socket = host.hub_socket.clone();
        let router = host.router.clone();
        tokio::spawn(async move {
            if let Err(err) = crate::plugin_router::serve_hub(&hub_socket, router).await {
                eprintln!("agalma: hub capability socket stopped: {err}");
            }
        });
    }
    // HP.4 (`agalma-jmr.4`) attaches the real capability-scoped RPC health
    // probe: it performs the common plugin protocol handshake + `plugin.Health`
    // over the child's Connect UDS instead of assuming readiness.
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    if let Err(err) = host.start_desired(&mut probe) {
        eprintln!("agalma: plugin host start failed: {err:?}");
        let _ = host.shutdown();
        return std::process::ExitCode::FAILURE;
    }

    if config.once {
        let evidence = host.shutdown();
        for (plugin_id, stop) in &evidence {
            eprintln!(
                "agalma: plugin `{plugin_id}` stopped: gone={} ({})",
                stop.process_group_gone, stop.detail
            );
        }
        return std::process::ExitCode::SUCCESS;
    }

    // HP.7 action-specific production attachment: on an interactive terminal run
    // the generic watch UI with this host's validated action service attached.
    // The host keeps supervising its children on each UI tick. A non-interactive
    // (piped/daemon) invocation keeps the headless supervise loop below, and a
    // host with no selected ledger stays fail-closed.
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        // Reuse the ledger already attached at assembly; opening another handle
        // would spawn a second connection to the selected ledger.
        let actions = match host.action_driver() {
            Ok(driver) => driver,
            Err(err) => {
                eprintln!("agalma: action service unavailable (fail-closed): {err:?}");
                None
            }
        };
        let ui_state_dir = host.state_dir.clone();
        let ui_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        crate::watch::spawn_signal_watch(Arc::clone(&ui_stop));
        let code = crate::watch::run_interactive_with(
            &ui_state_dir,
            1000,
            ui_stop,
            actions,
            || {
                if let Err(err) = host.supervise_tick(&mut probe) {
                    eprintln!("agalma: plugin supervise tick failed: {err:?}");
                }
                host.refresh_action_sockets();
            },
        );
        host.shutdown();
        return code;
    }

    // Long-running supervise loop: observe child exits, restart under backoff,
    // and drain due restarts until the stop sentinel.
    loop {
        if let Some(reason) = crate::stop::observe_sentinel(&host.state_dir).reason() {
            eprintln!("agalma: plugin host stopping: {reason}");
            break;
        }
        match host.supervise_tick(&mut probe) {
            Ok(outcome) => {
                for plan in &outcome.crashed {
                    eprintln!(
                        "agalma: plugin exited; restart #{} in {}ms (gone={})",
                        plan.attempt, plan.delay_ms, plan.stop.process_group_gone
                    );
                }
            }
            Err(err) => eprintln!("agalma: plugin supervise tick failed: {err:?}"),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    host.shutdown();
    std::process::ExitCode::SUCCESS
}

/// Assemble and start the plugin host for the `agalma work` loop.
///
/// Best-effort: a host-assembly or start failure is reported but never blocks
/// factory work. No configured roots yields an empty host (a no-op). The
/// returned host must be held for the loop's lifetime; dropping it shuts the
/// owned plugin group down. HP.5 attaches the host to the work loop's **selected
/// ledger** so configured-root auto-install, durable desired state, the registry
/// checkpoint and report ingestion share the one `LedgerHandle` already opened
/// by the loop (no second connection, no private-storage fallback).
pub fn attach_work_plugin_host(
    state_dir: &Path,
    roots: &crate::cli::PluginRootArgs,
    ledger: &LedgerHandle,
) -> Option<PluginHost> {
    let config = PluginHostConfig {
        state_dir: state_dir.to_path_buf(),
        plugin_roots: crate::config::resolve_plugin_roots(roots),
        once: true,
    };
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| SystemClock.now_ms());
    let installations = InMemoryInstallations::new();
    let mut host =
        match PluginHost::assemble_with_ledger(&config, &installations, ledger.clone(), clock) {
            Ok(host) => host,
            Err(err) => {
                eprintln!("agalma: plugin host assembly failed: {err:?}");
                return None;
            }
        };
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    if let Err(err) = host.start_desired(&mut probe) {
        eprintln!("agalma: plugin host start failed: {err:?}");
    }
    Some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_from_secs_parses_positive_and_falls_back_otherwise() {
        let default = Duration::from_secs(600);
        // A positive whole-second value wins (and is trimmed).
        assert_eq!(
            timeout_from_secs(Some("1800"), default),
            Duration::from_secs(1800)
        );
        assert_eq!(
            timeout_from_secs(Some(" 42 "), default),
            Duration::from_secs(42)
        );
        // Absent, zero, negative, and garbage all fall back to the default.
        assert_eq!(timeout_from_secs(None, default), default);
        assert_eq!(timeout_from_secs(Some("0"), default), default);
        assert_eq!(timeout_from_secs(Some("-5"), default), default);
        assert_eq!(timeout_from_secs(Some("soon"), default), default);
        assert_eq!(timeout_from_secs(Some(""), default), default);
    }

    #[test]
    fn timeout_from_secs_defaults_to_the_turn_bound() {
        assert_eq!(
            timeout_from_secs(None, DEFAULT_TURN_TIMEOUT),
            DEFAULT_TURN_TIMEOUT
        );
        assert_eq!(
            timeout_from_secs(Some("garbage"), DEFAULT_TURN_TIMEOUT),
            DEFAULT_TURN_TIMEOUT
        );
    }
}
