//! Per-component vision for the TUI read model (TUI.3, `agalma-3kr.4`).
//!
//! Derives one [`ComponentSnapshot`] per supervised component boundary from the
//! same read model the TUI already consumes (the TUI.1 snapshot): the component
//! binding registry (`LedgerApi::bindings`), per-operation binding pins
//! (`harness_binding` events), the boundary transports (the `AGALMA_*_TRANSPORT`
//! env selectors), the supervised server sockets under the state dir, and the
//! provider/watchdog health signals (`provider_health` events).
//!
//! Read-only and std-only: it opens no sockets, spawns nothing, and writes
//! nothing. The supervised server **pid** is not durable (it lives only in the
//! in-memory supervisor), so liveness is a proxy: the boundary socket file under
//! the state dir is present. This matches the v1 decision that there is no
//! `Health` type — liveness proxies only (`docs/tui-design.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use agalma_contracts::{
    BindingState, ContractError, ExecutionRecord, HealthState, LedgerApi, PluginManifest, View,
    ViewKind,
};
use serde::Serialize;
use serde_json::Value;

use crate::plugin_registry::{
    ComponentOrigin, DesiredState, LifecycleState, PluginRegistry, RegistryEntry,
};
use crate::plugin_reports::{Freshness, PluginReportView, ReportIngestor};

/// Event kind carrying a provider/watchdog health signal.
pub const PROVIDER_HEALTH_KIND: &str = "provider_health";

/// The boundary transport selectors, one per supervised component.
///
/// Derived from the same env selectors the conductor binds with
/// (`AGALMA_LEDGER_TRANSPORT`, `AGALMA_HARNESS_TRANSPORT`, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transports {
    pub ledger: &'static str,
    pub harness: &'static str,
    pub workspace: &'static str,
    pub taskqueue: &'static str,
    pub execution: &'static str,
}

impl Transports {
    /// Read every boundary selector from the process environment.
    pub fn from_env() -> Self {
        Transports {
            ledger: crate::ledger_process::LedgerTransport::from_env().as_str(),
            harness: crate::harness_process::HarnessTransport::from_env().as_str(),
            workspace: crate::workspace_process::WorkspaceTransport::from_env().as_str(),
            taskqueue: crate::taskqueue_process::TaskQueueTransport::from_env().as_str(),
            execution: crate::execution_process::ExecutionTransport::from_env().as_str(),
        }
    }

    /// Every boundary in-process (the default; used by tests).
    pub fn all_in_process() -> Self {
        Transports {
            ledger: "in-process",
            harness: "in-process",
            workspace: "in-process",
            taskqueue: "in-process",
            execution: "in-process",
        }
    }

    /// Every boundary in-process except the given selectors set to `process`.
    pub fn with_process(ledger: bool, harness: bool) -> Self {
        let mut transports = Transports::all_in_process();
        if ledger {
            transports.ledger = "process";
        }
        if harness {
            transports.harness = "process";
        }
        transports
    }
}

/// One component row in the read model.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentSnapshot {
    /// Stable component name (`ledger`, `harness`, …).
    pub component: String,
    /// Component API name (`LedgerApi`, `HarnessApi`, …).
    pub api: String,
    /// API major version from the newest binding; `null` when unbound.
    pub api_version: Option<u32>,
    /// Boundary transport mode (`in-process` | `process`).
    pub transport: String,
    /// Socket file the supervised server binds (a glob for per-session sockets).
    pub socket: Option<String>,
    /// Liveness proxy: the socket file is present under the state dir.
    pub socket_present: bool,
    /// Component binding records, newest first.
    pub bindings: Vec<BindingView>,
    /// `provider_health` signals recorded for this component (harness only).
    pub stalls: u32,
    /// Reason of the most recent health signal, if any.
    pub last_health_reason: Option<String>,
}

/// One component binding record in the read model.
#[derive(Debug, Clone, Serialize)]
pub struct BindingView {
    pub binding_id: String,
    pub implementation: String,
    pub implementation_version: String,
    pub api_version: u32,
    pub generation: u64,
    /// `Debug` name of [`agalma_contracts::BindingState`] (`Active`, …).
    pub state: String,
    pub capabilities: Vec<String>,
    /// Operation ids whose binding pin points at this record.
    pub pinned_operations: Vec<String>,
}

/// A canonical component boundary: name, API, and socket file.
struct ComponentSpec {
    name: &'static str,
    api: &'static str,
    /// Exact socket file name, or a file-name prefix when `glob` is set.
    socket: &'static str,
    /// Whether `socket` is a prefix glob (per-session harness sockets).
    glob: bool,
}

/// The supervised component boundaries, in a stable display order.
const COMPONENTS: &[ComponentSpec] = &[
    ComponentSpec {
        name: "ledger",
        api: "LedgerApi",
        socket: crate::ledger_process::LEDGER_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "harness",
        api: agalma_contracts::harness::HARNESS_API,
        socket: "harness",
        glob: true,
    },
    ComponentSpec {
        name: "workspace",
        api: "WorkspaceApi",
        socket: crate::workspace_process::WORKSPACE_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "taskqueue",
        api: "TaskQueueApi",
        socket: crate::taskqueue_process::TASKQUEUE_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "execution",
        api: "ExecutionApi",
        socket: crate::execution_process::EXECUTION_SOCKET_NAME,
        glob: false,
    },
];

/// Derive the per-component panel from the read model.
///
/// `ledger` is `None` when no ledger file is present (the panel still reports
/// transports and socket liveness; bindings and health are empty).
pub fn derive<L: LedgerApi + ?Sized>(
    state_dir: &Path,
    ledger: Option<&L>,
    records: &[ExecutionRecord],
    transports: &Transports,
) -> Vec<ComponentSnapshot> {
    let mut by_api: BTreeMap<String, Vec<BindingView>> = BTreeMap::new();
    let mut pins: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stalls = 0u32;
    let mut last_health_reason: Option<String> = None;

    if let Some(ledger) = ledger {
        if let Ok(bindings) = ledger.bindings() {
            for binding in bindings {
                by_api
                    .entry(binding.component_api.clone())
                    .or_default()
                    .push(BindingView {
                        binding_id: binding.binding_id.to_string(),
                        implementation: binding.implementation,
                        implementation_version: binding.implementation_version,
                        api_version: binding.api_version,
                        generation: binding.generation,
                        state: format!("{:?}", binding.state),
                        capabilities: binding.capabilities.into_iter().collect(),
                        pinned_operations: Vec::new(),
                    });
            }
        }

        for record in records {
            let Ok(events) = ledger.events(&record.execution_id) else {
                continue;
            };
            for event in events {
                match event.kind.as_str() {
                    kind if kind == crate::harness_binding::BINDING_EVENT_KIND => {
                        if let (Some(binding_id), Some(operation_id)) = (
                            event.payload.get("binding_id").and_then(|v| v.as_str()),
                            event.payload.get("operation_id").and_then(|v| v.as_str()),
                        ) {
                            let entry = pins.entry(binding_id.to_string()).or_default();
                            if !entry.iter().any(|op| op == operation_id) {
                                entry.push(operation_id.to_string());
                            }
                        }
                    }
                    PROVIDER_HEALTH_KIND => {
                        stalls += 1;
                        if let Some(reason) = event.payload.get("reason").and_then(|v| v.as_str()) {
                            last_health_reason = Some(reason.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Attach per-operation pins to their binding rows.
    for bindings in by_api.values_mut() {
        for binding in bindings.iter_mut() {
            if let Some(operations) = pins.remove(&binding.binding_id) {
                binding.pinned_operations = operations;
            }
        }
    }

    let mut components: Vec<ComponentSnapshot> = COMPONENTS
        .iter()
        .map(|spec| {
            let bindings = by_api.remove(spec.api).unwrap_or_default();
            let api_version = bindings.first().map(|binding| binding.api_version);
            let socket_present = socket_present(state_dir, spec);
            let is_harness = spec.api == agalma_contracts::harness::HARNESS_API;
            ComponentSnapshot {
                component: spec.name.to_string(),
                api: spec.api.to_string(),
                api_version,
                transport: transport_for(spec.name, transports).to_string(),
                socket: Some(socket_label(spec)),
                socket_present,
                bindings,
                stalls: if is_harness { stalls } else { 0 },
                last_health_reason: if is_harness {
                    last_health_reason.clone()
                } else {
                    None
                },
            }
        })
        .collect();

    // Any binding whose API is not a known boundary still shows (future-proof).
    for (api, bindings) in by_api {
        let api_version = bindings.first().map(|binding| binding.api_version);
        components.push(ComponentSnapshot {
            component: api.clone(),
            api,
            api_version,
            transport: "-".to_string(),
            socket: None,
            socket_present: false,
            bindings,
            stalls: 0,
            last_health_reason: None,
        });
    }

    components
}

/// Boundary transport selector for a component name.
fn transport_for(name: &str, transports: &Transports) -> &'static str {
    match name {
        "ledger" => transports.ledger,
        "harness" => transports.harness,
        "workspace" => transports.workspace,
        "taskqueue" => transports.taskqueue,
        "execution" => transports.execution,
        _ => "-",
    }
}

/// Human-facing socket label (`harness*.sock`, `ledger.sock`).
fn socket_label(spec: &ComponentSpec) -> String {
    if spec.glob {
        format!("{}*.sock", spec.socket)
    } else {
        spec.socket.to_string()
    }
}

/// Liveness proxy: a matching socket file exists under the state dir.
fn socket_present(state_dir: &Path, spec: &ComponentSpec) -> bool {
    if spec.glob {
        std::fs::read_dir(state_dir)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(spec.socket) && name.ends_with(".sock")
                })
            })
            .unwrap_or(false)
    } else {
        state_dir.join(spec.socket).exists()
    }
}

/// Debug label for a binding state (kept next to the view type so callers do not
/// re-derive it).
pub fn binding_state_label(state: BindingState) -> &'static str {
    match state {
        BindingState::Staged => "Staged",
        BindingState::Active => "Active",
        BindingState::Draining => "Draining",
        BindingState::Retired => "Retired",
    }
}

// ---------------------------------------------------------------------------
// Registry/report-driven plugin read model (HP.6, `agalma-jmr.6`)
// ---------------------------------------------------------------------------
//
// The control plane derives component navigation and detail views from the
// **production** registry (`PluginRegistry`) and report read model
// (`ReportIngestor`), never from env guesses, socket existence alone, or a
// plugin-specific hub derivation. Reports render only through the frozen generic
// building blocks (`ViewKind`); no executable plugin UI payload crosses the
// boundary. Everything rendered is read-only, control-character escaped, and
// bounded in rows and text.

/// Maximum rows rendered from one Table/Timeline/Metrics block.
pub const MAX_VIEW_BLOCK_ROWS: usize = 24;
/// Maximum rendered characters in one view string (title, cell, value, note).
pub const MAX_VIEW_TEXT_CHARS: usize = 96;

/// Escape control characters so a report payload can never inject terminal
/// control sequences into a rendered frame. Non-control characters (including
/// multi-byte text) are preserved.
pub fn escape_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() {
            out.extend(ch.escape_default());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Escape `text` and truncate to `max` characters with an ellipsis.
fn bounded(text: &str, max: usize) -> String {
    let escaped = escape_control(text);
    if escaped.chars().count() <= max {
        return escaped;
    }
    let mut out: String = escaped.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A rendered key/value metric row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetricRow {
    pub name: String,
    pub value: String,
}

/// A rendered timeline row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineRow {
    pub time: String,
    pub text: String,
}

/// One generic rendered block. The set is exactly the frozen declarative view
/// building blocks; there is no executable or plugin-specific variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewBlock {
    Status {
        value: String,
    },
    Metrics {
        rows: Vec<MetricRow>,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        /// Rows dropped to stay within [`MAX_VIEW_BLOCK_ROWS`].
        truncated: usize,
    },
    Timeline {
        rows: Vec<TimelineRow>,
    },
    Alert {
        level: String,
        message: String,
    },
    ArtifactLink {
        label: String,
        target: String,
    },
}

/// One declared view rendered for a plugin component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentView {
    pub view_id: String,
    /// Building block name (`status` | `metrics` | `table` | `timeline` |
    /// `alert` | `artifact-link`).
    pub kind: String,
    pub title: String,
    pub schema_namespace: String,
    pub schema_version: u64,
    /// `rendered` | `awaiting-data` | `unsupported`.
    pub state: String,
    pub note: Option<String>,
    pub blocks: Vec<ViewBlock>,
}

/// A plugin-declared action, shown read-only. Execution is not part of this
/// read model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionView {
    pub action_id: String,
    pub required_capability: String,
    pub effectful: bool,
    pub human_confirmation: Option<String>,
}

/// The hub-observed health heartbeat for one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthView {
    pub state: String,
    pub detail: String,
    pub observed_at_unix_ms: u64,
}

/// One plugin component derived from the registry and its report view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginComponent {
    pub plugin_id: String,
    /// `discovered` | `built-in`.
    pub origin: String,
    /// `running` | `stopped`.
    pub desired: String,
    /// `installed` | `starting` | `ready` | `draining` | `failed` | `retired`.
    pub lifecycle: String,
    pub generation: Option<u64>,
    /// `fresh` | `stale` | `unavailable` | `unknown`.
    pub freshness: String,
    pub stale: bool,
    pub disconnected: bool,
    pub incomplete: bool,
    pub last_error: Option<String>,
    pub health: Option<HealthView>,
    pub actions: Vec<ActionView>,
    pub views: Vec<ComponentView>,
}

/// One plugin's identity/lifecycle detail, sourced either from the live
/// [`PluginRegistry`] or from the durable registry state.
///
/// The neutral input to the read model: the live host and the stateless
/// `status`/`watch` CLI derive the same components from the same
/// registry/report state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginSource {
    pub plugin_id: String,
    pub origin: ComponentOrigin,
    pub desired: DesiredState,
    pub observed: LifecycleState,
    pub generation: Option<u64>,
    pub last_error: Option<String>,
    pub manifest: Option<PluginManifest>,
}

/// Detail accessor shared by a live registry entry and a durable source.
trait PluginDetail {
    fn plugin_id(&self) -> &str;
    fn origin(&self) -> ComponentOrigin;
    fn desired(&self) -> DesiredState;
    fn observed(&self) -> LifecycleState;
    fn generation(&self) -> Option<u64>;
    fn last_error(&self) -> Option<&str>;
    fn manifest(&self) -> Option<&PluginManifest>;
}

impl PluginDetail for RegistryEntry {
    fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn origin(&self) -> ComponentOrigin {
        self.origin
    }

    fn desired(&self) -> DesiredState {
        self.desired
    }

    fn observed(&self) -> LifecycleState {
        self.observed
    }

    fn generation(&self) -> Option<u64> {
        self.active_generation.or(self.pending_generation)
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn manifest(&self) -> Option<&PluginManifest> {
        self.generation()
            .and_then(|generation| self.generations.get(&generation))
            .map(|installed| &installed.manifest)
    }
}

impl PluginDetail for PluginSource {
    fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn origin(&self) -> ComponentOrigin {
        self.origin
    }

    fn desired(&self) -> DesiredState {
        self.desired
    }

    fn observed(&self) -> LifecycleState {
        self.observed
    }

    fn generation(&self) -> Option<u64> {
        self.generation
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn manifest(&self) -> Option<&PluginManifest> {
        self.manifest.as_ref()
    }
}

/// Derive the plugin component panel from the live production registry and
/// report read model (the in-host path).
///
/// `reports` is the hub's report ingestor (freshness, heartbeats, durable
/// observations), or `None` when no report read model is attached. The result is
/// ordered by plugin id and is read-only: it opens no socket and writes nothing.
pub fn derive_plugin_components(
    registry: &PluginRegistry,
    reports: Option<&ReportIngestor>,
) -> Vec<PluginComponent> {
    let sources: Vec<PluginSource> = registry
        .plugin_ids()
        .into_iter()
        .filter_map(|plugin_id| registry.entry(plugin_id))
        .map(|entry| PluginSource {
            plugin_id: entry.plugin_id.clone(),
            origin: entry.origin,
            desired: entry.desired,
            observed: entry.observed,
            generation: entry.active_generation.or(entry.pending_generation),
            last_error: entry.last_error.clone(),
            manifest: entry
                .active_generation
                .or(entry.pending_generation)
                .and_then(|generation| entry.generations.get(&generation))
                .map(|installed| installed.manifest.clone()),
        })
        .collect();
    derive_plugin_components_from_sources(&sources, reports)
}

/// Derive the plugin component panel from durable registry sources and the
/// report read model (the stateless `status`/`watch` path).
pub fn derive_plugin_components_from_sources(
    sources: &[PluginSource],
    reports: Option<&ReportIngestor>,
) -> Vec<PluginComponent> {
    let mut components: Vec<PluginComponent> = sources
        .iter()
        .map(|source| derive_plugin_component(source, reports))
        .collect();
    components.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    components
}

fn derive_plugin_component<D: PluginDetail>(
    source: &D,
    reports: Option<&ReportIngestor>,
) -> PluginComponent {
    let plugin_id = source.plugin_id();
    let generation = source.generation();
    let freshness = reports
        .map(|reports| reports.freshness(plugin_id))
        .unwrap_or(Freshness::Unknown);
    let disconnected =
        source.observed() == LifecycleState::Failed || freshness == Freshness::Unavailable;
    let stale = freshness == Freshness::Stale;
    let report_view = reports.and_then(|reports| reports.view(plugin_id));
    let health = report_view
        .and_then(|view| view.health.as_ref())
        .map(|health| HealthView {
            state: health_state_label(health.state).to_string(),
            detail: bounded(&health.detail, MAX_VIEW_TEXT_CHARS),
            observed_at_unix_ms: health.observed_at_unix_ms,
        });

    let mut covered: BTreeSet<String> = BTreeSet::new();
    let (mut views, actions) = match source.manifest() {
        Some(manifest) => (
            declared_views(manifest, report_view, &mut covered),
            actions_from(manifest),
        ),
        None => (Vec::new(), Vec::new()),
    };
    views.extend(generic_views(report_view, &covered));

    PluginComponent {
        plugin_id: bounded(plugin_id, MAX_VIEW_TEXT_CHARS),
        origin: origin_label(source.origin()).to_string(),
        desired: desired_label(source.desired()).to_string(),
        lifecycle: lifecycle_label(source.observed()).to_string(),
        generation,
        freshness: freshness_label(freshness).to_string(),
        stale,
        disconnected,
        incomplete: report_view.is_some_and(PluginReportView::is_incomplete),
        last_error: source
            .last_error()
            .map(|error| bounded(error, MAX_VIEW_TEXT_CHARS)),
        health,
        actions,
        views,
    }
}

/// The read-only action descriptors a manifest advertises (execution is HP.7's).
fn actions_from(manifest: &PluginManifest) -> Vec<ActionView> {
    manifest
        .actions
        .iter()
        .map(|action| ActionView {
            action_id: bounded(&action.action_id, MAX_VIEW_TEXT_CHARS),
            required_capability: bounded(&action.required_capability, MAX_VIEW_TEXT_CHARS),
            effectful: action.effectful,
            human_confirmation: action
                .human_confirmation
                .as_deref()
                .map(|text| bounded(text, MAX_VIEW_TEXT_CHARS)),
        })
        .collect()
}

/// Build the durable registry sources for the selected ledger: the hub-owned
/// immutable installed packages (`<state_dir>/plugin-installed/...`) plus the
/// durable `RegistryCheckpoint` read **through the selected ledger service**
/// (`component-store/v1`). No discovery configuration is required, and no
/// private database or SQL is touched.
///
/// Fails closed: a selected-ledger read failure (an unavailable or corrupt
/// checkpoint) is returned, never collapsed into an empty/healthy plugin list.
pub fn load_plugin_sources(
    state_dir: &Path,
    ledger: &crate::ledger_process::LedgerHandle,
) -> Result<Vec<PluginSource>, ContractError> {
    let (_, checkpoint) = crate::plugin_registry::read_registry_checkpoint(ledger)?;
    let store = crate::plugin_registry::HubInstallStore::new(state_dir);

    let mut sources: Vec<PluginSource> = crate::composition::BUILTIN_COMPONENTS
        .iter()
        .map(|component| PluginSource {
            plugin_id: (*component).to_string(),
            origin: ComponentOrigin::BuiltIn,
            desired: checkpoint
                .desired
                .get(*component)
                .copied()
                .unwrap_or(DesiredState::Stopped),
            observed: LifecycleState::Ready,
            generation: None,
            last_error: None,
            manifest: None,
        })
        .collect();

    let mut discovered: BTreeSet<&str> = BTreeSet::new();
    discovered.extend(checkpoint.desired.keys().map(String::as_str));
    discovered.extend(checkpoint.active_generations.keys().map(String::as_str));
    discovered.extend(checkpoint.max_generations.keys().map(String::as_str));
    for plugin_id in discovered {
        if crate::composition::BUILTIN_COMPONENTS.contains(&plugin_id) {
            continue;
        }
        let generation = checkpoint.active_generations.get(plugin_id).copied();
        let manifest = generation
            .and_then(|generation| load_installed_manifest(&store, plugin_id, generation));
        sources.push(PluginSource {
            plugin_id: plugin_id.to_string(),
            origin: ComponentOrigin::Discovered,
            desired: checkpoint
                .desired
                .get(plugin_id)
                .copied()
                .unwrap_or(DesiredState::Stopped),
            observed: LifecycleState::Installed,
            generation,
            last_error: None,
            manifest,
        });
    }
    Ok(sources)
}

/// Read one hub-owned immutable installed manifest (the generation is rehashed
/// by HP.3 on install/stage/launch; this read model only reads).
fn load_installed_manifest(
    store: &crate::plugin_registry::HubInstallStore,
    plugin_id: &str,
    generation: u64,
) -> Option<PluginManifest> {
    let path = store
        .generation_dir(plugin_id, generation)
        .join("package")
        .join(crate::plugin_manifest::PLUGIN_MANIFEST_FILE);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Render every view declared by a manifest.
fn declared_views(
    manifest: &PluginManifest,
    report_view: Option<&PluginReportView>,
    covered: &mut BTreeSet<String>,
) -> Vec<ComponentView> {
    manifest
        .views
        .iter()
        .map(|view| {
            covered.insert(view.schema_namespace.clone());
            declared_view(view, report_view)
        })
        .collect()
}

fn declared_view(view: &View, report_view: Option<&PluginReportView>) -> ComponentView {
    let entry = report_view.and_then(|reports| reports.entries.get(&view.schema_namespace));
    let (state, note, blocks) = match entry {
        None => (
            "awaiting-data",
            Some("no report has been received for this namespace".to_string()),
            Vec::new(),
        ),
        Some(entry) if entry.schema_version != view.schema_version => (
            "unsupported",
            Some(format!(
                "view expects schema v{} but the report carries v{}",
                view.schema_version, entry.schema_version
            )),
            Vec::new(),
        ),
        Some(entry) => (
            "rendered",
            None,
            render_blocks(view.kind, &view.fields, &entry.payload),
        ),
    };
    ComponentView {
        view_id: bounded(&view.view_id, MAX_VIEW_TEXT_CHARS),
        kind: view_kind_label(view.kind).to_string(),
        title: bounded(&view.title, MAX_VIEW_TEXT_CHARS),
        schema_namespace: bounded(&view.schema_namespace, MAX_VIEW_TEXT_CHARS),
        schema_version: view.schema_version,
        state: state.to_string(),
        note,
        blocks,
    }
}

/// Generic rendering for report namespaces no declared view covers: unknown
/// private namespaces still show their data, never a plugin-specific UI path.
fn generic_views(
    report_view: Option<&PluginReportView>,
    covered: &BTreeSet<String>,
) -> Vec<ComponentView> {
    let Some(report_view) = report_view else {
        return Vec::new();
    };
    report_view
        .entries
        .iter()
        .filter(|(namespace, _)| !covered.contains(*namespace))
        .map(|(namespace, entry)| ComponentView {
            view_id: bounded(namespace, MAX_VIEW_TEXT_CHARS),
            kind: "metrics".to_string(),
            title: bounded(namespace, MAX_VIEW_TEXT_CHARS),
            schema_namespace: bounded(namespace, MAX_VIEW_TEXT_CHARS),
            schema_version: entry.schema_version,
            state: "rendered".to_string(),
            note: Some("undeclared namespace; rendered generically".to_string()),
            blocks: render_blocks(ViewKind::Metrics, &[], &entry.payload),
        })
        .collect()
}

/// Render one declarative view payload through its generic building block.
///
/// The function is pure and total: any JSON payload yields bounded, escaped
/// blocks. It never fetches remote data, evaluates a script, or emits a
/// plugin-specific type.
pub fn render_blocks(kind: ViewKind, fields: &[String], payload: &Value) -> Vec<ViewBlock> {
    match kind {
        ViewKind::Status => vec![ViewBlock::Status {
            value: status_value(fields, payload),
        }],
        ViewKind::Metrics => vec![ViewBlock::Metrics {
            rows: metric_rows(fields, payload),
        }],
        ViewKind::Table => table_blocks(fields, payload),
        ViewKind::Timeline => timeline_blocks(fields, payload),
        ViewKind::Alert => alert_blocks(fields, payload),
        ViewKind::ArtifactLink => artifact_blocks(fields, payload),
    }
}

fn status_value(fields: &[String], payload: &Value) -> String {
    for field in fields {
        if let Some(value) = field_value(payload, field) {
            return value;
        }
    }
    value_text(payload)
}

fn metric_rows(fields: &[String], payload: &Value) -> Vec<MetricRow> {
    let mut rows: Vec<MetricRow> = fields
        .iter()
        .filter_map(|field| {
            field_value(payload, field).map(|value| MetricRow {
                name: bounded(field, MAX_VIEW_TEXT_CHARS),
                value,
            })
        })
        .collect();
    if rows.is_empty() {
        if let Some(object) = payload.as_object() {
            for (name, value) in object.iter().take(MAX_VIEW_BLOCK_ROWS) {
                rows.push(MetricRow {
                    name: bounded(name, MAX_VIEW_TEXT_CHARS),
                    value: value_text(value),
                });
            }
        }
    }
    rows.truncate(MAX_VIEW_BLOCK_ROWS);
    rows
}

fn table_blocks(fields: &[String], payload: &Value) -> Vec<ViewBlock> {
    let Some(items) = payload.as_array() else {
        return vec![ViewBlock::Metrics {
            rows: metric_rows(fields, payload),
        }];
    };
    let headers: Vec<String> = if fields.is_empty() {
        vec!["value".to_string()]
    } else {
        fields
            .iter()
            .map(|field| bounded(field, MAX_VIEW_TEXT_CHARS))
            .collect()
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    for item in items.iter().take(MAX_VIEW_BLOCK_ROWS) {
        let row = if fields.is_empty() {
            vec![value_text(item)]
        } else {
            fields
                .iter()
                .map(|field| field_value(item, field).unwrap_or_else(|| "-".to_string()))
                .collect()
        };
        rows.push(row);
    }
    vec![ViewBlock::Table {
        headers,
        rows,
        truncated: items.len().saturating_sub(MAX_VIEW_BLOCK_ROWS),
    }]
}

fn timeline_blocks(fields: &[String], payload: &Value) -> Vec<ViewBlock> {
    let items: &[Value] = if let Some(items) = payload.as_array() {
        items
    } else if let Some(items) = payload.get("entries").and_then(Value::as_array) {
        items
    } else {
        return vec![ViewBlock::Metrics {
            rows: metric_rows(fields, payload),
        }];
    };
    let mut rows: Vec<TimelineRow> = Vec::new();
    for item in items.iter().take(MAX_VIEW_BLOCK_ROWS) {
        let time = fields
            .first()
            .and_then(|field| field_value(item, field))
            .unwrap_or_default();
        let text = if fields.len() > 1 {
            fields[1..]
                .iter()
                .filter_map(|field| field_value(item, field))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            value_text(item)
        };
        rows.push(TimelineRow { time, text });
    }
    vec![ViewBlock::Timeline { rows }]
}

fn alert_blocks(fields: &[String], payload: &Value) -> Vec<ViewBlock> {
    let level = payload
        .get("level")
        .map(value_text)
        .unwrap_or_else(|| "alert".to_string());
    let parts: Vec<String> = fields
        .iter()
        .filter(|field| field.as_str() != "level")
        .filter_map(|field| field_value(payload, field))
        .collect();
    let message = if parts.is_empty() {
        value_text(payload)
    } else {
        parts.join(" ")
    };
    vec![ViewBlock::Alert { level, message }]
}

fn artifact_blocks(fields: &[String], payload: &Value) -> Vec<ViewBlock> {
    let label = fields
        .first()
        .map(|field| bounded(field, MAX_VIEW_TEXT_CHARS))
        .unwrap_or_else(|| "artifact".to_string());
    let target = fields
        .iter()
        .find_map(|field| field_value(payload, field))
        .or_else(|| payload.get("path").map(value_text))
        .unwrap_or_else(|| value_text(payload));
    vec![ViewBlock::ArtifactLink { label, target }]
}

/// Read a top-level or dotted field path and render it bounded.
fn field_value(payload: &Value, field: &str) -> Option<String> {
    let mut current = payload;
    for part in field.split('.') {
        current = current.get(part)?;
    }
    Some(value_text(current))
}

/// Render one JSON scalar (or a bounded compact rendering of a composite).
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "-".to_string(),
        Value::String(text) => bounded(text, MAX_VIEW_TEXT_CHARS),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => bounded(&number.to_string(), MAX_VIEW_TEXT_CHARS),
        other => bounded(&other.to_string(), MAX_VIEW_TEXT_CHARS),
    }
}

fn view_kind_label(kind: ViewKind) -> &'static str {
    match kind {
        ViewKind::Status => "status",
        ViewKind::Metrics => "metrics",
        ViewKind::Table => "table",
        ViewKind::Timeline => "timeline",
        ViewKind::Alert => "alert",
        ViewKind::ArtifactLink => "artifact-link",
    }
}

fn origin_label(origin: ComponentOrigin) -> &'static str {
    match origin {
        ComponentOrigin::Discovered => "discovered",
        ComponentOrigin::BuiltIn => "built-in",
    }
}

fn desired_label(desired: DesiredState) -> &'static str {
    match desired {
        DesiredState::Running => "running",
        DesiredState::Stopped => "stopped",
    }
}

fn lifecycle_label(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Installed => "installed",
        LifecycleState::Starting => "starting",
        LifecycleState::Ready => "ready",
        LifecycleState::Draining => "draining",
        LifecycleState::Failed => "failed",
        LifecycleState::Retired => "retired",
    }
}

fn freshness_label(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Unknown => "unknown",
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
        Freshness::Unavailable => "unavailable",
    }
}

fn health_state_label(state: HealthState) -> &'static str {
    match state {
        HealthState::Ready => "ready",
        HealthState::Degraded => "degraded",
        HealthState::Unavailable => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::{
        CommitBatch, DispatchIntent, ExecutionId, ExecutionPhase, ExecutionState, OperationId,
        TaskId,
    };
    use agalma_ledger::SqliteLedger;
    use agalma_testkit::ScratchDir;
    use serde_json::json;

    /// Seed a ledger with one execution, a pending operation, a binding pin, and
    /// two `provider_health` stall signals.
    fn seed(dir: &ScratchDir) -> (SqliteLedger, ExecutionId) {
        let exec = ExecutionId::derive(&TaskId::new("task-a"), 1);
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(agalma_contracts::ExecutionRecord {
                    execution_id: exec.clone(),
                    task_id: TaskId::new("task-a"),
                    generation: 1,
                    phase: ExecutionPhase::Build,
                    state: ExecutionState::Running,
                    attempt: 2,
                    revision: 0,
                }),
                events: vec![],
                receipts: vec![],
                intents: vec![DispatchIntent {
                    operation_id: OperationId::derive(&exec, "build@2"),
                    execution_id: exec.clone(),
                    payload: json!({}),
                    enqueued_at_unix_ms: 1,
                    consumed: false,
                }],
            })
            .expect("seed execution");
        let plan = crate::harness_binding::plan_binding(
            crate::harness_binding::HarnessKind::Reference,
            1,
            "model",
        )
        .expect("plan binding");
        crate::harness_binding::persist_binding(
            &mut ledger,
            &exec,
            &OperationId::derive(&exec, "build@2"),
            2,
            &plan,
        )
        .expect("persist binding");
        for reason in [
            crate::phases::PollStallReason::Idle,
            crate::phases::PollStallReason::SessionGone,
        ] {
            crate::phases::record_provider_health(
                dir.path(),
                &exec,
                "model",
                "reference@reference/v1",
                2,
                &crate::phases::PollStall {
                    reason,
                    idle_ms: 0,
                    tokens: 0,
                    wall_ms: 0,
                    last_state: String::new(),
                },
            )
            .expect("record health");
        }
        (ledger, exec)
    }

    #[test]
    fn absent_ledger_lists_components_without_bindings() {
        let dir = ScratchDir::new("components-absent");
        let components =
            derive::<SqliteLedger>(dir.path(), None, &[], &Transports::all_in_process());
        assert_eq!(components.len(), COMPONENTS.len());
        let harness = components
            .iter()
            .find(|c| c.component == "harness")
            .expect("harness row");
        assert_eq!(harness.transport, "in-process");
        assert!(harness.bindings.is_empty());
        assert_eq!(harness.stalls, 0);
        assert!(!harness.socket_present);
    }

    #[test]
    fn present_ledger_reports_bindings_and_health() {
        let dir = ScratchDir::new("components-present");
        let (ledger, exec) = seed(&dir);
        let components = derive(
            dir.path(),
            Some(&ledger),
            &ledger.executions().expect("executions"),
            &Transports::with_process(true, true),
        );

        let ledger_row = components
            .iter()
            .find(|c| c.component == "ledger")
            .expect("ledger row");
        assert_eq!(ledger_row.transport, "process");

        let harness = components
            .iter()
            .find(|c| c.component == "harness")
            .expect("harness row");
        assert_eq!(harness.transport, "process");
        assert_eq!(harness.api, "HarnessApi");
        assert_eq!(harness.api_version, Some(0));
        assert_eq!(harness.stalls, 2);
        assert_eq!(harness.last_health_reason.as_deref(), Some("session_gone"));
        assert_eq!(harness.bindings.len(), 1);
        let binding = &harness.bindings[0];
        assert_eq!(binding.implementation, "reference");
        assert_eq!(binding.implementation_version, "reference/v1");
        assert_eq!(binding.generation, 1);
        assert_eq!(binding.state, "Active");
        assert_eq!(
            binding.pinned_operations,
            vec![OperationId::derive(&exec, "build@2").to_string()]
        );
        assert!(binding.capabilities.contains(&"context_hooks".to_string()));
    }

    #[test]
    fn socket_liveness_tracks_files_under_state_dir() {
        let dir = ScratchDir::new("components-socket");
        std::fs::write(
            dir.path()
                .join(crate::workspace_process::WORKSPACE_SOCKET_NAME),
            b"",
        )
        .expect("touch workspace socket");
        std::fs::write(dir.path().join("harness-build.sock"), b"").expect("touch harness socket");
        let components =
            derive::<SqliteLedger>(dir.path(), None, &[], &Transports::all_in_process());

        let workspace = components
            .iter()
            .find(|c| c.component == "workspace")
            .expect("workspace row");
        assert!(workspace.socket_present);
        assert_eq!(workspace.socket.as_deref(), Some("workspace.sock"));

        let harness = components
            .iter()
            .find(|c| c.component == "harness")
            .expect("harness row");
        assert!(harness.socket_present, "harness*.sock glob should match");
        assert_eq!(harness.socket.as_deref(), Some("harness*.sock"));

        let ledger_row = components
            .iter()
            .find(|c| c.component == "ledger")
            .expect("ledger row");
        assert!(!ledger_row.socket_present);
    }

    #[test]
    fn binding_state_label_matches_states() {
        assert_eq!(binding_state_label(BindingState::Active), "Active");
        assert_eq!(binding_state_label(BindingState::Draining), "Draining");
        assert_eq!(binding_state_label(BindingState::Retired), "Retired");
        assert_eq!(binding_state_label(BindingState::Staged), "Staged");
    }
}
