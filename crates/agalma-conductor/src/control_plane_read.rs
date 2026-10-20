//! Hub-owned typed `ControlPlaneRead/v1` service (WEB.2, `agalma-rwn.2`).
//!
//! Registers one observation-only capability (`control-plane.read/v1`) behind
//! HP.4's authenticated routing. The service projects the production registry
//! and HP.5 report read model plus bound ledger domain reads into the pure
//! browser wire contracts; it never mutates storage, claims a task, or acquires
//! action authority.
//!
//! Invariants (proven by `tests/control_plane_read.rs`):
//!
//! * the authenticated grant's namespace/method, expiry, revocation and binding
//!   generation are enforced by [`crate::plugin_router::CapabilityRouter`]; the
//!   service additionally re-checks the namespace/method before projecting;
//! * reads are admitted while factory effect admission is stopped and are never
//!   journaled, so viewing creates no claims or action intents;
//! * missing/unreadable sources are explicit [`SourceState`] observations and
//!   unknown totals, never numeric zero or a healthy empty panel;
//! * cursors pin the collection/filter/revision; a malformed or mismatched
//!   cursor fails and a stale revision returns `RevisionConflict`;
//! * pages are bounded to the contract's entry and byte limits;
//! * artifact reads resolve a digest only from a reference visible in that
//!   plugin's authoritative reports, confine the resolved path to the immutable
//!   package store, re-verify the digest, and are chunk-bounded.
//!
//! Storage stays behind injected seams: the registry is the live production
//! [`PluginRegistry`], reports are the HP.5 [`ReportIngestor`], and ledger reads
//! are a [`DomainReads`] binding. A missing seam is an explicit missing source.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use agalma_contracts::digest::{canonical_digest, sha256_digest, FailureCode};
use agalma_contracts::{
    check_cursor, BindingPin, ComponentLifecycle, ComponentStatus, ComponentStatusOrigin,
    ComponentSummary, Completeness, ContractError, ControlReceipt, DigestRecord, ExecutionEvent,
    ExecutionId, ExecutionRecord, ExactU64, FactoryStop, Freshness, HealthState, LifecycleSource,
    ListComponents, PackageArtifactRef, ReadArtifactChunk, ReadArtifactChunkRequest,
    ReadComponent, ReadCursor, ReadMetadata, ReadOverview, ReadPageRequest, ReportingState,
    ReportEnvelope, BrowserWire, SourceAvailability, SourceState, StatusDesired, TimelineEntry,
    View, CONTROL_PLANE_READ_CAPABILITY, CONTROL_PLANE_READ_FORMAT, READ_PAGE_MAX_BYTES,
    STATUS_TEXT_MAX_CHARS,
};
use agalma_ipc::plugin_server::METHOD_INVOKE_CAPABILITY;

use crate::economics::{derive_bound, EconomicsInputs, SourceRead};
use crate::ledger_process::LedgerHandle;
use crate::plugin_registry::{
    ComponentOrigin, DesiredState, LifecycleState, PluginRegistry, RegistryEntry,
};
use crate::plugin_reports::{
    Freshness as ReportFreshness, PluginReportView, ReportIngestor, REGISTRY_NAMESPACE,
};
use crate::plugin_router::{CapabilityAdapter, GrantScope};
use crate::stop::{BoundStopControl, StopControl, StopDecision};

/// Bound ledger domain reads used by the read projections.
///
/// Production binds the selected [`LedgerHandle`]; tests inject failures. Every
/// method returns a [`ContractError`] the projection records as an explicit
/// unreadable/source state.
pub trait DomainReads: Send + Sync {
    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError>;
    fn events(&self, execution: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError>;
    fn digests_for_execution(
        &self,
        execution: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError>;
    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError>;
}

impl DomainReads for LedgerHandle {
    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError> {
        agalma_contracts::LedgerApi::executions(self)
    }

    fn events(&self, execution: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError> {
        agalma_contracts::LedgerApi::events(self, execution)
    }

    fn digests_for_execution(
        &self,
        execution: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError> {
        agalma_contracts::LedgerApi::digests_for_execution(self, execution)
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        agalma_contracts::LedgerApi::control_receipts(self)
    }
}

/// One typed `control-plane.read/v1` request.
///
/// The capability grants no arbitrary method or SQL: only these bounded
/// observation operations exist.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlPlaneReadRequest {
    /// Factory stop observation, executions, economics, and curated timeline.
    ReadOverview,
    /// Registry identities, generations, health/freshness/completeness, views.
    ListComponents {
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// One component's declared views and bounded authoritative report pages.
    ReadComponent {
        plugin_id: String,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// At most `ARTIFACT_CHUNK_MAX_BYTES` from a visible report artifact ref.
    ReadArtifactChunk {
        plugin_id: String,
        digest: String,
        #[serde(default)]
        offset: Option<ExactU64>,
        #[serde(default)]
        max_bytes: Option<u64>,
    },
}

/// Maximum executions in one overview projection.
pub const OVERVIEW_MAX_EXECUTIONS: usize = 100;
/// Maximum curated timeline entries in one overview projection.
pub const OVERVIEW_MAX_TIMELINE: usize = 100;

/// One registry-derived component projection entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedComponent {
    pub plugin_id: String,
    pub origin: ComponentOrigin,
    /// Legacy heartbeat-refined lifecycle; retained for legacy list fields.
    pub observed: LifecycleState,
    /// Raw supervisor lifecycle observation. `None` when no live supervised
    /// generation exists, so a durable fallback or builtin placeholder never
    /// becomes observed.
    pub raw_lifecycle: Option<LifecycleState>,
    /// Raw supervisor observation time, only with actual live lifecycle.
    pub lifecycle_observed_at_ms: Option<u64>,
    pub desired: Option<DesiredState>,
    pub last_error: Option<String>,
    pub generation: Option<u64>,
    pub implementation_version: String,
    pub config_digest: String,
    pub views: Vec<View>,
}

/// The bound component projection used by `ListComponents`/`ReadComponent`.
///
/// Production binds the durable selected-ledger registry projection; the
/// in-host/tests path binds the live [`PluginRegistry`]. A read failure is
/// returned so the caller reports an unreadable registry rather than an empty
/// healthy list.
pub trait ComponentProjection: Send + Sync {
    fn components(&self) -> Result<Vec<ProjectedComponent>, ContractError>;
}

/// A live in-process registry projection (the in-host and test path).
pub struct LiveComponents {
    registry: Arc<Mutex<PluginRegistry>>,
}

impl LiveComponents {
    /// Wrap a live registry handle.
    pub fn new(registry: Arc<Mutex<PluginRegistry>>) -> Self {
        LiveComponents { registry }
    }
}

impl ComponentProjection for LiveComponents {
    fn components(&self) -> Result<Vec<ProjectedComponent>, ContractError> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(registry
            .plugin_ids()
            .into_iter()
            .filter_map(|plugin_id| registry.entry(plugin_id))
            .map(project_live_entry)
            .collect())
    }
}

/// The durable selected-ledger registry projection (the production path).
///
/// Reads the hub's durable `RegistryCheckpoint` and immutable installed
/// packages through the bound ledger; no private storage or discovery config is
/// required, and a corrupt/unreadable checkpoint fails visibly.
pub struct DurableComponents {
    state_dir: PathBuf,
    ledger: LedgerHandle,
    reports: Arc<Mutex<ReportIngestor>>,
}

impl DurableComponents {
    /// Bind the durable projection to the selected ledger, state dir and the
    /// live report ingestor (the actual health heartbeat source).
    pub fn new(
        state_dir: impl Into<PathBuf>,
        ledger: LedgerHandle,
        reports: Arc<Mutex<ReportIngestor>>,
    ) -> Self {
        DurableComponents {
            state_dir: state_dir.into(),
            ledger,
            reports,
        }
    }
}

impl ComponentProjection for DurableComponents {
    fn components(&self) -> Result<Vec<ProjectedComponent>, ContractError> {
        let sources = crate::components::load_plugin_sources(&self.state_dir, &self.ledger)?;
        let store = crate::plugin_registry::HubInstallStore::new(&self.state_dir);
        let reports = self.reports.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::with_capacity(sources.len());
        for source in sources {
            let views = source
                .manifest
                .as_ref()
                .map(|manifest| manifest.views.clone())
                .unwrap_or_default();
            let (implementation_version, config_digest) = match (source.generation, &source.manifest) {
                (Some(generation), Some(manifest)) => {
                    // Authoritative installed pin, never a hash of the manifest:
                    // the generation's frozen `PluginInstallation/v1` config ref.
                    let installation = store.load_installation(&source.plugin_id, generation)?.ok_or_else(|| {
                        FailureCode::StateUnavailable.failure(
                            &source.plugin_id,
                            format!("installed generation {generation} has no pinned installation"),
                        )
                    })?;
                    (manifest.package_version.clone(), installation.configuration.digest)
                }
                _ => (
                    if source.origin == ComponentOrigin::BuiltIn {
                        "builtin".to_string()
                    } else {
                        "unknown".to_string()
                    },
                    sha256_digest(b""),
                ),
            };
            // Live lifecycle/health: a report heartbeat is the authoritative
            // runtime signal available to the hub read path. A stale/unavailable
            // heartbeat shows as Failed (unavailable), a degraded one as
            // Starting, a ready one as Ready.
            let observed = match reports.view(&source.plugin_id).and_then(|view| view.health.as_ref()) {
                Some(health) => match health.state {
                    HealthState::Ready => LifecycleState::Ready,
                    HealthState::Degraded => LifecycleState::Starting,
                    HealthState::Unavailable => LifecycleState::Failed,
                },
                None => source.observed,
            };
            out.push(ProjectedComponent {
                plugin_id: source.plugin_id,
                origin: source.origin,
                observed,
                // A durable registry snapshot is not a live observation: no
                // lifecycle evidence or timestamp is invented here.
                raw_lifecycle: None,
                lifecycle_observed_at_ms: None,
                desired: Some(source.desired),
                last_error: source.last_error,
                generation: source.generation,
                implementation_version,
                config_digest,
                views,
            });
        }
        Ok(out)
    }
}

/// Project the live host registry + report health into read components.
///
/// This is the production supervisor/registry lifecycle source: a crash
/// recorded by the host (`mark_failed`) is visible immediately, without waiting
/// for the report freshness threshold.
pub fn live_components(
    registry: &PluginRegistry,
    reports: Option<&ReportIngestor>,
) -> Vec<ProjectedComponent> {
    registry
        .plugin_ids()
        .into_iter()
        .filter_map(|plugin_id| registry.entry(plugin_id))
        .map(|entry| {
            let mut projection = project_live_entry(entry);
            // The live registry lifecycle is the crash authority: a supervisor
            // `Failed`/`Retired` observation is never masked by a stale Ready
            // heartbeat. Otherwise the heartbeat refines the observed state.
            let live_authoritative = matches!(
                entry.observed,
                LifecycleState::Failed | LifecycleState::Retired
            );
            if !live_authoritative {
                if let Some(health) = reports
                    .and_then(|reports| reports.view(&entry.plugin_id))
                    .and_then(|view| view.health.as_ref())
                {
                    projection.observed = match health.state {
                        HealthState::Ready => LifecycleState::Ready,
                        HealthState::Degraded => LifecycleState::Starting,
                        HealthState::Unavailable => LifecycleState::Failed,
                    };
                }
            }
            projection
        })
        .collect()
}

/// The hub-owned read projection service.
#[derive(Clone)]
pub struct ControlPlaneReadService {
    components: Arc<dyn ComponentProjection>,
    /// The production live host projection, refreshed each supervise tick.
    live: Option<Arc<Mutex<Vec<ProjectedComponent>>>>,
    reports: Option<Arc<Mutex<ReportIngestor>>>,
    domains: Option<Arc<dyn DomainReads>>,
    stop: Option<Arc<dyn StopControl + Send + Sync>>,
    state_dir: PathBuf,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl ControlPlaneReadService {
    /// Build the service over a live registry projection and a state dir for
    /// artifact resolution. Reports, ledger domains and stop control are
    /// attached with the builders below; an unattached seam is a visible
    /// missing source.
    pub fn new(registry: Arc<Mutex<PluginRegistry>>, state_dir: impl Into<PathBuf>) -> Self {
        ControlPlaneReadService {
            components: Arc::new(LiveComponents::new(registry)),
            live: None,
            reports: None,
            domains: None,
            stop: None,
            state_dir: state_dir.into(),
            now: Arc::new(system_now_ms),
        }
    }

    /// Build the production service over the selected ledger and report
    /// ingestor: the durable registry projection, bound ledger reads, live
    /// report projection, stop observation and artifact root.
    ///
    /// This is the constructor the production plugin host registers; it opens
    /// no private storage and creates no second ledger connection.
    pub fn production(
        state_dir: impl Into<PathBuf>,
        ledger: LedgerHandle,
        reports: Arc<Mutex<ReportIngestor>>,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let state_dir = state_dir.into();
        let stop = Arc::new(BoundStopControl::new(&state_dir, ledger.clone()));
        ControlPlaneReadService {
            components: Arc::new(DurableComponents::new(
                state_dir.clone(),
                ledger.clone(),
                Arc::clone(&reports),
            )),
            live: None,
            reports: Some(reports),
            domains: Some(Arc::new(ledger)),
            stop: Some(stop),
            state_dir,
            now: clock,
        }
    }

    /// Build with an injected component projection (tests/alternate hosts).
    pub fn with_projection(mut self, projection: Arc<dyn ComponentProjection>) -> Self {
        self.components = projection;
        self
    }

    /// Attach the production live host projection cell (refreshed by the
    /// supervisor tick). When populated it is authoritative over the durable
    /// registry snapshot, so a crash is visible immediately.
    pub fn with_live_cell(
        mut self,
        live: Arc<Mutex<Vec<ProjectedComponent>>>,
    ) -> Self {
        self.live = Some(live);
        self
    }

    /// Current components: the live host projection when populated, otherwise
    /// the bound durable/live projection seam.
    fn component_snapshot(&self) -> Result<Vec<ProjectedComponent>, ContractError> {
        if let Some(live) = &self.live {
            let live = live.lock().unwrap_or_else(PoisonError::into_inner);
            if !live.is_empty() {
                return Ok(live.clone());
            }
        }
        self.components.components()
    }

    /// Attach the HP.5 durable report ingestor (registry revision + reports).
    pub fn with_reports(mut self, reports: Arc<Mutex<ReportIngestor>>) -> Self {
        self.reports = Some(reports);
        self
    }

    /// Attach the bound ledger domain reads.
    pub fn with_domains(mut self, domains: Arc<dyn DomainReads>) -> Self {
        self.domains = Some(domains);
        self
    }

    /// Attach the factory stop observation (read-only; never clears a stop).
    pub fn with_stop(mut self, stop: Arc<dyn StopControl + Send + Sync>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Override the injected clock (deterministic tests).
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    fn now_ms(&self) -> u64 {
        (self.now)()
    }

    /// The durable registry checkpoint revision plus its observed availability.
    fn registry_revision(&self) -> (u64, SourceState, String) {
        let Some(reports) = &self.reports else {
            return (
                0,
                SourceState::Missing,
                "durable registry checkpoint is not bound".to_string(),
            );
        };
        let reports = reports.lock().unwrap_or_else(PoisonError::into_inner);
        match reports.store().component_read(REGISTRY_NAMESPACE) {
            Ok(Some(row)) => (row.revision, SourceState::Available, String::new()),
            Ok(None) => (0, SourceState::Empty, String::new()),
            Err(error) => (
                0,
                SourceState::Unreadable,
                format!("registry checkpoint unreadable: {error}"),
            ),
        }
    }

    /// The highest report watermark across the active views, when reports bound.
    fn report_watermark(&self) -> (Option<u64>, SourceState, String) {
        let Some(reports) = &self.reports else {
            return (
                None,
                SourceState::Missing,
                "report projection is not bound".to_string(),
            );
        };
        let reports = reports.lock().unwrap_or_else(PoisonError::into_inner);
        let watermark = reports
            .active_generations()
            .keys()
            .filter_map(|plugin_id| reports.view(plugin_id))
            .map(|view| view.watermark)
            .max();
        match watermark {
            Some(watermark) => (Some(watermark), SourceState::Available, String::new()),
            None => (
                Some(0),
                SourceState::Empty,
                "no report observations recorded".to_string(),
            ),
        }
    }

    fn availability(&self, source: &str, state: SourceState, detail: impl Into<String>) -> SourceAvailability {
        SourceAvailability {
            source: source.to_string(),
            state,
            detail: detail.into(),
            observed_at_unix_ms: self.now_ms(),
        }
    }

    fn metadata(
        &self,
        registry_revision: Option<ExactU64>,
        report_watermark: Option<ExactU64>,
        sources: Vec<SourceAvailability>,
    ) -> ReadMetadata {
        ReadMetadata {
            format: CONTROL_PLANE_READ_FORMAT.to_string(),
            collected_at_unix_ms: self.now_ms(),
            registry_revision,
            report_watermark,
            sources,
        }
    }

    /// `ReadOverview`.
    pub fn read_overview(&self) -> Result<ReadOverview, ContractError> {
        let (registry_revision, registry_state, registry_detail) = self.registry_revision();
        let (watermark, reports_state, reports_detail) = self.report_watermark();

        let (factory, factory_source) = self.factory_stop();

        let (inputs, ledger_detail) = self.economics_inputs();

        let projection = derive_bound(&inputs, &BTreeMap::new());
        let executions: Vec<agalma_contracts::ExecutionSummary> = match &inputs.executions {
            SourceRead::Available(records) => records
                .iter()
                .map(|record| agalma_contracts::ExecutionSummary {
                    execution_id: record.execution_id.as_str().to_string(),
                    task_id: record.task_id.as_str().to_string(),
                    generation: ExactU64::from(record.generation as u64),
                    phase: record.phase,
                    state: record.state,
                    attempt: ExactU64::from(record.attempt as u64),
                    revision: ExactU64::from(record.revision),
                    updated_at_unix_ms: 0,
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut timeline: Vec<TimelineEntry> = projection
            .snapshot
            .timeline
            .iter()
            .map(|entry| TimelineEntry {
                at_unix_ms: entry.recorded_at_unix_ms,
                kind: entry.kind.clone(),
                title: entry.summary.clone(),
                detail: entry.execution_id.clone(),
            })
            .collect();
        // Explicit contract caps: an overview is a bounded summary, never an
        // unbounded dump of the execution or event history.
        let mut truncated = false;
        let mut executions = executions;
        if executions.len() > OVERVIEW_MAX_EXECUTIONS {
            executions.truncate(OVERVIEW_MAX_EXECUTIONS);
            truncated = true;
        }
        if timeline.len() > OVERVIEW_MAX_TIMELINE {
            timeline.truncate(OVERVIEW_MAX_TIMELINE);
            truncated = true;
        }
        let economics = projection.usage_cost(self.now_ms());

        let event_detail = if projection.events == SourceState::Unreadable {
            "event source unreadable or not loaded".to_string()
        } else {
            String::new()
        };
        let mut sources = vec![
            self.availability("ledger", projection.executions, ledger_detail.clone()),
            self.availability("ledger.events", projection.events, event_detail),
            self.availability("ledger.digests", projection.digests, String::new()),
            self.availability(
                "ledger.receipts",
                projection.control_receipts,
                String::new(),
            ),
            self.availability("budget", SourceState::Missing, "allowance source is not bound"),
            self.availability("registry", registry_state, registry_detail),
            self.availability("reports", reports_state, reports_detail),
            factory_source,
        ];
        if truncated {
            sources.push(self.availability(
                "overview",
                SourceState::Available,
                format!(
                    "bounded to {OVERVIEW_MAX_EXECUTIONS} executions and {OVERVIEW_MAX_TIMELINE} timeline entries"
                ),
            ));
        }
        sources.sort_by(|left, right| left.source.cmp(&right.source));

        let mut metadata = self.metadata(
            Some(ExactU64::from(registry_revision)),
            watermark.map(ExactU64::from),
            sources,
        );
        // Enforce the page byte cap on the actual encoded response: entry caps
        // alone do not bound a wide timeline/execution projection.
        let mut overview = ReadOverview {
            metadata: metadata.clone(),
            factory: factory.clone(),
            executions: executions.clone(),
            economics: economics.clone(),
            timeline: timeline.clone(),
        };
        loop {
            let bytes = overview.to_wire_json()?.len() as u64;
            if bytes <= READ_PAGE_MAX_BYTES {
                break;
            }
            if overview.timeline.pop().is_none() && overview.executions.pop().is_none() {
                return Err(FailureCode::InvalidRecord.failure(
                    "overview",
                    "overview cannot be encoded within the page byte bound",
                ));
            }
            if !metadata
                .sources
                .iter()
                .any(|source| source.source == "overview")
            {
                metadata.sources.push(self.availability(
                    "overview",
                    SourceState::Available,
                    format!("bounded to {READ_PAGE_MAX_BYTES} encoded bytes"),
                ));
            }
            overview.metadata = metadata.clone();
        }
        Ok(overview)
    }

    /// The observed factory stop latch and its source availability.
    fn factory_stop(&self) -> (FactoryStop, SourceAvailability) {
        let observed_at = self.now_ms();
        let Some(stop) = &self.stop else {
            return (
                FactoryStop {
                    stopped: true,
                    reason: Some("stop state unavailable: no stop source bound".to_string()),
                    observed_at_unix_ms: observed_at,
                },
                self.availability(
                    "factory.stop",
                    SourceState::Missing,
                    "stop source is not bound",
                ),
            );
        };
        match stop.decision() {
            StopDecision::Running => (
                FactoryStop {
                    stopped: false,
                    reason: None,
                    observed_at_unix_ms: observed_at,
                },
                self.availability("factory.stop", SourceState::Available, ""),
            ),
            StopDecision::Stopped(reason) => (
                FactoryStop {
                    stopped: true,
                    reason: Some(reason),
                    observed_at_unix_ms: observed_at,
                },
                self.availability("factory.stop", SourceState::Available, ""),
            ),
            StopDecision::Unavailable(reason) => (
                FactoryStop {
                    stopped: true,
                    reason: Some(reason.clone()),
                    observed_at_unix_ms: observed_at,
                },
                self.availability("factory.stop", SourceState::Unreadable, reason),
            ),
        }
    }

    /// Load the bound ledger reads into explicit [`EconomicsInputs`].
    fn economics_inputs(&self) -> (EconomicsInputs, String) {
        let Some(domains) = &self.domains else {
            return (
                EconomicsInputs::missing("ledger is not bound"),
                "ledger is not bound".to_string(),
            );
        };
        let executions = match domains.executions() {
            Ok(records) if records.is_empty() => SourceRead::Empty,
            Ok(records) => SourceRead::Available(records),
            Err(error) => {
                let detail = error.to_string();
                return (EconomicsInputs::unreadable(detail.clone()), detail);
            }
        };
        let mut events = BTreeMap::new();
        let mut digests = BTreeMap::new();
        if let SourceRead::Available(records) = &executions {
            for record in records {
                let execution_id = record.execution_id.as_str().to_string();
                events.insert(
                    execution_id.clone(),
                    match domains.events(&record.execution_id) {
                        Ok(events) if events.is_empty() => SourceRead::Empty,
                        Ok(events) => SourceRead::Available(events),
                        Err(error) => SourceRead::Unreadable(error.to_string()),
                    },
                );
                digests.insert(
                    execution_id,
                    match domains.digests_for_execution(&record.execution_id) {
                        Ok(rows) if rows.is_empty() => SourceRead::Empty,
                        Ok(rows) => SourceRead::Available(rows),
                        Err(error) => SourceRead::Unreadable(error.to_string()),
                    },
                );
            }
        }
        let control_receipts = match domains.control_receipts() {
            Ok(receipts) if receipts.is_empty() => SourceRead::Empty,
            Ok(receipts) => SourceRead::Available(receipts),
            Err(error) => SourceRead::Unreadable(error.to_string()),
        };
        (
            EconomicsInputs {
                executions,
                events,
                digests,
                control_receipts,
            },
            String::new(),
        )
    }

    /// `ListComponents`.
    pub fn list_components(
        &self,
        request: &ReadPageRequest,
    ) -> Result<ListComponents, ContractError> {
        let limit = request.effective_limit()?;
        let (_checkpoint_revision, _registry_state, _registry_detail) = self.registry_revision();
        let (watermark, reports_state, reports_detail) = self.report_watermark();
        let projections = match self.component_snapshot() {
            Ok(mut projections) => {
                projections.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
                Ok(projections)
            }
            Err(error) => Err(error),
        };

        let summaries: Vec<ComponentSummary> = {
            let reports_guard = self.reports.as_ref().map(|reports| {
                reports.lock().unwrap_or_else(PoisonError::into_inner)
            });
            match &projections {
                Ok(projections) => projections
                    .iter()
                    .map(|projection| component_summary(projection, reports_guard.as_deref()))
                    .collect(),
                Err(_) => Vec::new(),
            }
        };
        // The cursor revision fingerprints the actual summary collection,
        // including live health/freshness/availability, so any relevant
        // registry/generation/health mutation invalidates an old cursor. It is
        // an opaque revision, never treated as a monotonic counter.
        let current_revision = match &projections {
            Ok(_) => ExactU64::from(component_collection_revision(&summaries)?),
            Err(_) => ExactU64::from(0u64),
        };
        let filter_digest = canonical_digest(&Value::String("components".to_string()))?;
        let cursor = check_cursor(
            request.cursor.as_deref(),
            "components",
            &filter_digest,
            &current_revision,
        )?;
        let start = match &cursor {
            None => 0,
            Some(cursor) => {
                let position = summaries
                    .iter()
                    .position(|summary| summary.plugin_id == cursor.after)
                    .ok_or_else(|| {
                        FailureCode::InvalidRecord.failure(
                            "cursor",
                            "cursor position is not in the component collection",
                        )
                    })?;
                position + 1
            }
        };
        let candidates = &summaries[start.min(summaries.len())..];

        let (registry_availability, registry_detail) = match &projections {
            Err(error) => (SourceState::Unreadable, error.to_string()),
            Ok(_) if summaries.is_empty() => (SourceState::Empty, String::new()),
            Ok(_) => (SourceState::Available, String::new()),
        };
        let sources = vec![
            self.availability("registry", registry_availability, registry_detail),
            self.availability("reports", reports_state, reports_detail),
        ];

        let metadata = self.metadata(
            Some(current_revision.clone()),
            watermark.map(ExactU64::from),
            sources,
        );

        let mut selected: Vec<ComponentSummary> = Vec::new();
        for candidate in candidates {
            if selected.len() >= limit as usize {
                break;
            }
            selected.push(candidate.clone());
            let probe = ListComponents {
                metadata: metadata.clone(),
                entries: selected.clone(),
                next_cursor: None,
            };
            if probe.check_bounds().is_err() {
                selected.pop();
                break;
            }
        }
        if selected.is_empty() && !candidates.is_empty() {
            return Err(FailureCode::InvalidRecord.failure(
                "components",
                format!(
                    "one component summary exceeds the {READ_PAGE_MAX_BYTES}-byte page bound"
                ),
            ));
        }
        let has_more = selected.len() < candidates.len();
        let next_cursor = has_more
            .then(|| {
                selected.last().map(|last| {
                    ReadCursor {
                        collection: "components".to_string(),
                        filter_digest: filter_digest.clone(),
                        revision: current_revision.clone(),
                        after: last.plugin_id.clone(),
                    }
                    .encode()
                })
            })
            .flatten()
            .transpose()?;

        let page = ListComponents {
            metadata,
            entries: selected,
            next_cursor,
        };
        page.check_bounds()?;
        Ok(page)
    }

    /// `ReadComponent`.
    pub fn read_component(
        &self,
        plugin_id: &str,
        request: &ReadPageRequest,
    ) -> Result<ReadComponent, ContractError> {
        let limit = request.effective_limit()?;
        let (revision, _registry_state, _registry_detail) = self.registry_revision();
        let (watermark, reports_state, reports_detail) = self.report_watermark();
        let current_revision = ExactU64::from(revision);

        let projection = match self.component_snapshot() {
            Ok(projections) => projections
                .into_iter()
                .find(|projection| projection.plugin_id == plugin_id),
            Err(error) => {
                let sources = vec![
                    self.availability("registry", SourceState::Unreadable, error.to_string()),
                    self.availability("reports", SourceState::Unknown, String::new()),
                ];
                return Ok(ReadComponent {
                    metadata: self.metadata(
                        Some(current_revision),
                        watermark.map(ExactU64::from),
                        sources,
                    ),
                    binding: synthetic_binding(plugin_id, "unknown"),
                    descriptors: Vec::new(),
                    entries: Vec::new(),
                    next_cursor: None,
                    status: None,
                });
            }
        };

        let Some(projection) = projection else {
            let sources = vec![
                self.availability(
                    "registry",
                    SourceState::Missing,
                    format!("plugin `{plugin_id}` is not registered"),
                ),
                self.availability(
                    "reports",
                    SourceState::Missing,
                    format!("plugin `{plugin_id}` has no report projection"),
                ),
            ];
            return Ok(ReadComponent {
                metadata: self.metadata(
                    Some(current_revision),
                    watermark.map(ExactU64::from),
                    sources,
                ),
                binding: synthetic_binding(plugin_id, "unknown"),
                descriptors: Vec::new(),
                entries: Vec::new(),
                next_cursor: None,
                status: None,
            });
        };

        let binding = binding_from_projected(&projection);
        let descriptors = projection.views.clone();

        let Some(reports) = &self.reports else {
            let report_state = if reports_state == SourceState::Missing {
                SourceState::Missing
            } else {
                reports_state
            };
            let sources = vec![
                self.availability("registry", SourceState::Available, ""),
                self.availability("reports", report_state, reports_detail),
            ];
            return Ok(ReadComponent {
                metadata: self.metadata(
                    Some(current_revision),
                    watermark.map(ExactU64::from),
                    sources,
                ),
                binding,
                descriptors,
                entries: Vec::new(),
                next_cursor: None,
                status: Some(component_status(&projection, None)),
            });
        };
        let reports = reports.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(view) = reports.view(plugin_id) else {
            let sources = vec![
                self.availability("registry", SourceState::Available, ""),
                self.availability(
                    "reports",
                    SourceState::Missing,
                    format!("no durable report view for `{plugin_id}`"),
                ),
            ];
            return Ok(ReadComponent {
                metadata: self.metadata(
                    Some(current_revision),
                    watermark.map(ExactU64::from),
                    sources,
                ),
                binding,
                descriptors,
                entries: Vec::new(),
                next_cursor: None,
                status: Some(component_status(&projection, Some(&reports))),
            });
        };

        // Authoritative current view entries, never the durable replay window:
        // only entries with a canonical producer envelope are served. A
        // snapshot-recovered entry has no producer identity and is reported as
        // unavailable (owned by the HP.5 snapshot projection), never fabricated.
        let (envelopes, unsupported) = authoritative_envelopes(view);
        let report_detail = if unsupported == 0 {
            String::new()
        } else {
            format!(
                "{unsupported} snapshot-recovered entr(ies) have no canonical producer identity (owner: HP.5 snapshot projection)"
            )
        };
        // Any unmatched recovered row makes the aggregate source Unknown, even
        // when other rows are supported (no partial-availability claim).
        let report_state = if unsupported > 0 {
            SourceState::Unknown
        } else if !envelopes.is_empty() {
            SourceState::Available
        } else {
            SourceState::Empty
        };
        let report_revision = ExactU64::from(report_collection_revision(&envelopes)?);
        let report_watermark = ExactU64::from(view.watermark);
        let collection = format!("component/{plugin_id}");
        let filter_digest = canonical_digest(&Value::String(plugin_id.to_string()))?;
        let cursor = check_cursor(
            request.cursor.as_deref(),
            &collection,
            &filter_digest,
            &report_revision,
        )?;
        let start = match &cursor {
            None => 0,
            Some(cursor) => {
                let after = parse_component_cursor(&cursor.after)?;
                let position = envelopes
                    .iter()
                    .position(|envelope| envelope.schema_namespace == after)
                    .ok_or_else(|| {
                        FailureCode::InvalidRecord.failure(
                            "cursor",
                            "cursor position is not in the component report set",
                        )
                    })?;
                position + 1
            }
        };
        let candidates: Vec<ReportEnvelope> =
            envelopes[start.min(envelopes.len())..].to_vec();

        let sources = vec![
            self.availability("registry", SourceState::Available, ""),
            self.availability("reports", report_state, report_detail),
        ];
        let metadata = self.metadata(
            Some(current_revision),
            Some(report_watermark),
            sources,
        );
        let status = component_status(&projection, Some(&reports));

        let mut selected: Vec<ReportEnvelope> = Vec::new();
        for candidate in &candidates {
            if selected.len() >= limit as usize {
                break;
            }
            selected.push(candidate.clone());
            let probe = ReadComponent {
                metadata: metadata.clone(),
                binding: binding.clone(),
                descriptors: descriptors.clone(),
                entries: selected.clone(),
                next_cursor: None,
                status: Some(status.clone()),
            };
            if probe.check_bounds().is_err() {
                selected.pop();
                break;
            }
        }
        if selected.is_empty() && !candidates.is_empty() {
            return Err(FailureCode::InvalidRecord.failure(
                "component",
                format!("one report envelope exceeds the {READ_PAGE_MAX_BYTES}-byte page bound"),
            ));
        }
        let has_more = selected.len() < candidates.len();
        let next_cursor = has_more
            .then(|| {
                selected.last().map(|last| {
                    ReadCursor {
                        collection: collection.clone(),
                        filter_digest: filter_digest.clone(),
                        revision: report_revision.clone(),
                        after: component_cursor(&last.schema_namespace),
                    }
                    .encode()
                })
            })
            .flatten()
            .transpose()?;

        let page = ReadComponent {
            metadata,
            binding,
            descriptors,
            entries: selected,
            next_cursor,
            status: Some(status),
        };
        page.check_bounds()?;
        Ok(page)
    }

    /// `ReadArtifactChunk`: resolve an immutable reference visible in the
    /// plugin's authoritative reports and return one bounded chunk.
    pub fn read_artifact_chunk(
        &self,
        request: &ReadArtifactChunkRequest,
    ) -> Result<ReadArtifactChunk, ContractError> {
        request.check()?;
        let offset = request.effective_offset()?;
        let max_bytes = request.effective_max_bytes()?;
        let (revision, _state, _detail) = self.registry_revision();
        let Some(reports) = &self.reports else {
            return Err(FailureCode::StateUnavailable.failure(
                &request.plugin_id,
                "report projection is not bound",
            ));
        };
        let reports = reports.lock().unwrap_or_else(PoisonError::into_inner);
        let view = reports.view(&request.plugin_id).ok_or_else(|| {
            FailureCode::StateUnavailable.failure(
                &request.plugin_id,
                "plugin has no authoritative report projection",
            )
        })?;
        // Only references in the authoritative current entries are visible; a
        // reference removed by a later report or snapshot recovery is gone.
        let (authoritative, _) = authoritative_envelopes(view);
        let mut matches: Vec<PackageArtifactRef> = Vec::new();
        for entry in &authoritative {
            collect_artifact_refs(&entry.payload, &request.digest, &mut matches);
        }
        let Some(reference) = matches.first().cloned() else {
            return Err(FailureCode::ScopeDenied.failure(
                &request.plugin_id,
                "no report reference authorizes this artifact digest",
            ));
        };
        if matches.iter().any(|candidate| candidate != &reference) {
            return Err(FailureCode::IntegrityMismatch.failure(
                &request.plugin_id,
                "conflicting report references share the requested digest",
            ));
        }
        if reference.bytes > agalma_contracts::ARTIFACT_TOTAL_MAX_BYTES {
            return Err(FailureCode::InvalidRecord.failure(
                &reference.path,
                format!(
                    "artifact {} bytes exceeds limit {}",
                    reference.bytes,
                    agalma_contracts::ARTIFACT_TOTAL_MAX_BYTES
                ),
            ));
        }
        let bytes = self.read_authorized_artifact(&reference)?;
        let total = bytes.len() as u64;
        if offset > total {
            return Err(FailureCode::InvalidRecord.failure(
                &reference.path,
                format!("artifact offset {offset} is beyond the artifact's {total} bytes"),
            ));
        }
        let end = offset.saturating_add(max_bytes).min(total);
        let slice = &bytes[offset as usize..end as usize];
        let next_offset = (end < total).then(|| ExactU64::from(end));
        let chunk = ReadArtifactChunk {
            metadata: self.metadata(
                Some(ExactU64::from(revision)),
                Some(ExactU64::from(view.watermark)),
                vec![
                    self.availability("reports", SourceState::Available, ""),
                    self.availability("artifacts", SourceState::Available, ""),
                ],
            ),
            reference,
            offset: ExactU64::from(offset),
            data_hex: hex_lower(slice),
            next_offset,
        };
        chunk.check_bounds()?;
        Ok(chunk)
    }

    /// Resolve a report-referenced path inside the immutable package store and
    /// re-verify its digest before returning the bytes.
    fn read_authorized_artifact(
        &self,
        reference: &PackageArtifactRef,
    ) -> Result<Vec<u8>, ContractError> {
        let root = self
            .state_dir
            .canonicalize()
            .unwrap_or_else(|_| self.state_dir.clone());
        let candidate = crate::task_package::resolve_artifact_path(&root, &reference.path)?;
        let resolved = candidate.canonicalize().map_err(|error| {
            FailureCode::StateUnavailable.failure(
                &reference.path,
                format!("artifact object is unreadable: {error}"),
            )
        })?;
        if !resolved.starts_with(&root) {
            return Err(FailureCode::ScopeDenied.failure(
                &reference.path,
                "artifact resolves outside the hub state dir",
            ));
        }
        let store = crate::task_package::store_root(&root);
        if !resolved.starts_with(&store) {
            return Err(FailureCode::ScopeDenied.failure(
                &reference.path,
                "artifact is outside the immutable package store",
            ));
        }
        // Trust the filesystem size, not the declared reference size, before
        // allocating: an oversized or lying-size object is refused without
        // reading it into memory.
        let metadata = std::fs::metadata(&resolved).map_err(|error| {
            FailureCode::StateUnavailable.failure(
                &reference.path,
                format!("artifact object is unreadable: {error}"),
            )
        })?;
        if metadata.len() > agalma_contracts::ARTIFACT_TOTAL_MAX_BYTES {
            return Err(FailureCode::InvalidRecord.failure(
                &reference.path,
                format!(
                    "artifact object {} bytes exceeds limit {}",
                    metadata.len(),
                    agalma_contracts::ARTIFACT_TOTAL_MAX_BYTES
                ),
            ));
        }
        if metadata.len() != reference.bytes {
            return Err(FailureCode::IntegrityMismatch.failure(
                &reference.path,
                format!(
                    "artifact object is {} bytes but the reference declares {}",
                    metadata.len(),
                    reference.bytes
                ),
            ));
        }
        let mut file = std::fs::File::open(&resolved).map_err(|error| {
            FailureCode::StateUnavailable.failure(
                &reference.path,
                format!("artifact object is unreadable: {error}"),
            )
        })?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes).map_err(|error| {
            FailureCode::StateUnavailable.failure(
                &reference.path,
                format!("artifact object is unreadable: {error}"),
            )
        })?;
        if bytes.len() as u64 != metadata.len() {
            return Err(FailureCode::StateUnavailable.failure(
                &reference.path,
                "artifact object changed size while being read",
            ));
        }
        reference.verify(&bytes)?;
        Ok(bytes)
    }
}

impl CapabilityAdapter for ControlPlaneReadService {
    fn capability(&self) -> &str {
        CONTROL_PLANE_READ_CAPABILITY
    }

    fn invoke(&mut self, scope: &GrantScope, input: &Value) -> Result<Value, ContractError> {
        if scope.resource_namespace != CONTROL_PLANE_READ_CAPABILITY {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{CONTROL_PLANE_READ_CAPABILITY}`",
                    scope.resource_namespace
                ),
            ));
        }
        if scope.method != METHOD_INVOKE_CAPABILITY {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{METHOD_INVOKE_CAPABILITY}`",
                    scope.method
                ),
            ));
        }
        self.invoke_bounded(scope, input, u64::MAX)
    }

    fn invoke_with_deadline(
        &mut self,
        scope: &GrantScope,
        input: &Value,
        deadline_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        self.invoke_bounded(scope, input, deadline_unix_ms)
    }
}

impl ControlPlaneReadService {
    /// The deadline refusal for an expired bound (`None` when unbounded/open).
    fn deadline_error(&self, deadline_unix_ms: u64, subject: &str) -> Option<ContractError> {
        if deadline_unix_ms == u64::MAX || self.now_ms() < deadline_unix_ms {
            return None;
        }
        Some(FailureCode::DeadlineExceeded.failure(
            subject,
            format!(
                "read deadline {deadline_unix_ms} passed at {}",
                self.now_ms()
            ),
        ))
    }

    /// Validate, dispatch and bound one read request.
    fn invoke_bounded(
        &self,
        scope: &GrantScope,
        input: &Value,
        deadline_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        if scope.resource_namespace != CONTROL_PLANE_READ_CAPABILITY {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{CONTROL_PLANE_READ_CAPABILITY}`",
                    scope.resource_namespace
                ),
            ));
        }
        if scope.method != METHOD_INVOKE_CAPABILITY {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{METHOD_INVOKE_CAPABILITY}`",
                    scope.method
                ),
            ));
        }
        let request: ControlPlaneReadRequest =
            serde_json::from_value(input.clone()).map_err(|error| {
                FailureCode::InvalidRecord.failure(
                    CONTROL_PLANE_READ_CAPABILITY,
                    format!("malformed read request: {error}"),
                )
            })?;
        if let Some(error) = self.deadline_error(deadline_unix_ms, &scope.plugin_id) {
            return Err(error);
        }
        let value = match request {
            ControlPlaneReadRequest::ReadOverview => to_value(&self.read_overview()?)?,
            ControlPlaneReadRequest::ListComponents { limit, cursor } => {
                let request = ReadPageRequest { limit, cursor };
                to_value(&self.list_components(&request)?)?
            }
            ControlPlaneReadRequest::ReadComponent {
                plugin_id,
                limit,
                cursor,
            } => {
                let request = ReadPageRequest { limit, cursor };
                to_value(&self.read_component(&plugin_id, &request)?)?
            }
            // Artifact byte reads remain deferred: fail before any payload or
            // file read, and never dispatch a client-selected ledger method.
            ControlPlaneReadRequest::ReadArtifactChunk { .. } => {
                return Err(ContractError::UnsupportedCapability(format!(
                    "control-plane read artifact operation is deferred to the browser-platform epic ({CONTROL_PLANE_READ_CAPABILITY})"
                )));
            }
        };
        // The source work completed, but a result whose deadline passed while it
        // ran is not delivered (no post-deadline result).
        if let Some(error) = self.deadline_error(deadline_unix_ms, &scope.plugin_id) {
            return Err(error);
        }
        Ok(value)
    }
}

/// Project a typed read response through the canonical WEB.1
/// [`BrowserWire`] codec, so nested identity `u64`s are exact decimal strings
/// in the actual response bytes (never generic serde numbers).
fn to_value<T: BrowserWire>(value: &T) -> Result<Value, ContractError> {
    value.to_wire_value()
}

/// Project one live registry entry into the neutral component projection.
fn project_live_entry(entry: &RegistryEntry) -> ProjectedComponent {
    let generation = entry.active_generation.or(entry.pending_generation);
    let installed = generation.and_then(|generation| entry.generations.get(&generation));
    // Only an actual supervised generation supplies live lifecycle evidence. A
    // generation-less builtin placeholder is represented but not observed.
    let supervised = generation.is_some();
    ProjectedComponent {
        plugin_id: entry.plugin_id.clone(),
        origin: entry.origin,
        observed: entry.observed,
        raw_lifecycle: supervised.then_some(entry.observed),
        lifecycle_observed_at_ms: supervised
            .then_some(entry.observed_at_ms)
            .filter(|observed_at| *observed_at != 0),
        desired: Some(entry.desired),
        last_error: entry.last_error.clone(),
        generation,
        implementation_version: installed
            .map(|installed| installed.package_version.clone())
            .unwrap_or_else(|| {
                if entry.origin == ComponentOrigin::BuiltIn {
                    "builtin".to_string()
                } else {
                    "unknown".to_string()
                }
            }),
        config_digest: installed
            .map(|installed| installed.config_digest.clone())
            .unwrap_or_else(|| sha256_digest(b"")),
        views: installed
            .map(|installed| installed.manifest.views.clone())
            .unwrap_or_default(),
    }
}

/// A stable revision fingerprint of the registry/generation collection.
///
/// It changes whenever a component identity, generation, origin, pinned
/// implementation/config, or declared view set changes, so a page cursor pinned
/// to an older collection is refused instead of silently skipping or repeating
/// entries. Health/freshness are excluded (they are separate observations).
fn component_collection_revision(summaries: &[ComponentSummary]) -> Result<u64, ContractError> {
    let keys: Vec<Value> = summaries
        .iter()
        .map(|summary| {
            json!({
                "plugin_id": summary.plugin_id,
                "binding": summary.binding,
                "generation": summary.generation,
                "health": summary.health,
                "freshness": summary.freshness,
                "completeness": summary.completeness,
                "available": summary.available,
                "views": summary.views,
                "status": summary.status,
            })
        })
        .collect();
    let digest = canonical_digest(&Value::Array(keys))?;
    let hex = digest.trim_start_matches("sha256:");
    let prefix = hex.get(..16).unwrap_or(hex);
    u64::from_str_radix(prefix, 16).map_err(|error| {
        FailureCode::InvalidRecord.failure(
            "registry",
            format!("collection revision digest is not hex: {error}"),
        )
    })
}

/// Bound one actual status detail/error for the browser wire.
fn bounded_status_text(text: &str) -> String {
    let escaped = crate::components::escape_control(text);
    if escaped.chars().count() <= STATUS_TEXT_MAX_CHARS {
        return escaped;
    }
    let mut out: String = escaped
        .chars()
        .take(STATUS_TEXT_MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

/// The shared evidence-bearing status projection for list and detail.
///
/// It is deliberately a pure function of the neutral projection and the report
/// ingestor so both read paths agree. It never infers health from lifecycle,
/// never invents a timestamp/generation, and only reads a heartbeat from the
/// view matching the selected generation.
fn component_status(
    projection: &ProjectedComponent,
    reports: Option<&ReportIngestor>,
) -> ComponentStatus {
    let origin = match projection.origin {
        ComponentOrigin::BuiltIn => ComponentStatusOrigin::BuiltIn,
        ComponentOrigin::Discovered => ComponentStatusOrigin::Discovered,
    };
    let desired = projection.desired.map(|desired| match desired {
        DesiredState::Running => StatusDesired::Running,
        DesiredState::Stopped => StatusDesired::Stopped,
    });
    let lifecycle = projection.raw_lifecycle.map(|lifecycle| match lifecycle {
        LifecycleState::Installed => ComponentLifecycle::Installed,
        LifecycleState::Starting => ComponentLifecycle::Starting,
        LifecycleState::Ready => ComponentLifecycle::Ready,
        LifecycleState::Draining => ComponentLifecycle::Draining,
        LifecycleState::Failed => ComponentLifecycle::Failed,
        LifecycleState::Retired => ComponentLifecycle::Retired,
    });
    let lifecycle_source = if projection.raw_lifecycle.is_some() {
        LifecycleSource::LiveRegistry
    } else {
        LifecycleSource::None
    };
    let lifecycle_observed_at_unix_ms = projection
        .lifecycle_observed_at_ms
        .filter(|observed_at| *observed_at != 0)
        .map(ExactU64::from);

    let view = reports.and_then(|reports| reports.view(&projection.plugin_id));
    // Health belongs to one exact selected generation. A view for another
    // generation (or a generation-less placeholder with no supervised process)
    // cannot contribute current health.
    let matching_view = view.filter(|view| {
        projection
            .generation
            .is_some_and(|generation| generation == view.active_generation)
    });
    let health = matching_view.and_then(|view| view.health.as_ref());
    let health_observed_at_unix_ms = health
        .map(|health| ExactU64::from(health.observed_at_unix_ms));
    let health_detail = health
        .map(|health| bounded_status_text(&health.detail))
        .filter(|detail| !detail.is_empty());
    let incomplete = matching_view.is_some_and(PluginReportView::is_incomplete);

    let reporting = match reports {
        None => ReportingState::Unknown,
        Some(reports) => match matching_view {
            None => ReportingState::Unavailable,
            Some(view) if view.entries.is_empty() && view.health.is_none() => {
                ReportingState::Unavailable
            }
            Some(_) => match reports.freshness(&projection.plugin_id) {
                ReportFreshness::Fresh => ReportingState::Fresh,
                ReportFreshness::Stale => ReportingState::Stale,
                ReportFreshness::Unavailable => ReportingState::Unavailable,
                ReportFreshness::Unknown => ReportingState::Unknown,
            },
        },
    };

    ComponentStatus {
        origin,
        generation: projection.generation.map(ExactU64::from),
        desired,
        lifecycle,
        lifecycle_source,
        lifecycle_observed_at_unix_ms,
        health: health.map(|health| health.state),
        health_observed_at_unix_ms,
        health_detail,
        last_error: projection
            .last_error
            .as_deref()
            .map(bounded_status_text)
            .filter(|error| !error.is_empty()),
        reporting,
        incomplete,
    }
}

/// Build one [`ComponentSummary`] from a component projection and the report
/// view.
fn component_summary(
    projection: &ProjectedComponent,
    reports: Option<&ReportIngestor>,
) -> ComponentSummary {
    let report_freshness = reports
        .map(|reports| reports.freshness(&projection.plugin_id))
        .unwrap_or(ReportFreshness::Unknown);
    let view = reports.and_then(|reports| reports.view(&projection.plugin_id));
    // A live registry crash/retirement is authoritative over a stale heartbeat.
    let health = if matches!(
        projection.observed,
        LifecycleState::Failed | LifecycleState::Retired
    ) {
        HealthState::Unavailable
    } else {
        view.and_then(|view| view.health.as_ref())
            .map(|health| health.state)
            .unwrap_or_else(|| lifecycle_health(projection.observed))
    };
    let freshness = match report_freshness {
        ReportFreshness::Fresh => Freshness::Fresh,
        ReportFreshness::Stale | ReportFreshness::Unavailable => Freshness::Stale,
        ReportFreshness::Unknown => Freshness::Unknown,
    };
    let completeness = match view {
        Some(view) if view.is_incomplete() => Completeness::Partial,
        Some(_) => Completeness::Complete,
        None => Completeness::Unknown,
    };
    let disconnected = projection.observed == LifecycleState::Failed
        || report_freshness == ReportFreshness::Unavailable;
    let serving_lifecycle = matches!(
        projection.observed,
        LifecycleState::Ready | LifecycleState::Draining
    );
    // A durable registry projection cannot observe process lifecycle, so a
    // fresh report heartbeat is the authoritative serving signal there.
    let serving_reports = health == HealthState::Ready;
    ComponentSummary {
        plugin_id: projection.plugin_id.clone(),
        name: projection.plugin_id.clone(),
        binding: binding_from_projected(projection),
        generation: ExactU64::from(projection.generation.unwrap_or(0)),
        health,
        freshness,
        completeness,
        available: (serving_lifecycle || serving_reports) && !disconnected,
        views: projection.views.clone(),
        status: Some(component_status(projection, reports)),
    }
}

/// The hub-pinned binding identity for a component projection.
fn binding_from_projected(projection: &ProjectedComponent) -> BindingPin {
    BindingPin {
        binding_id: projection.plugin_id.clone(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: projection.implementation_version.clone(),
        generation: projection.generation.unwrap_or(0),
        config_digest: projection.config_digest.clone(),
    }
}

/// The authoritative current report envelopes for one plugin view.
///
/// The durable `envelopes` map is the identity-replay window; it is **not** the
/// current data. This resolves each current `view.entries` namespace to its
/// matching (highest-sequence) envelope, so an entry replaced by a newer report
/// or a snapshot recovery is served with current data, and an envelope removed
/// from the view is never served or used to authorize an artifact.
/// The authoritative current report envelopes for one plugin view, plus the
/// number of current entries that have no canonical producer envelope.
///
/// Only entries with an exact matching producer envelope (namespace, version,
/// payload digest, active generation) are returned: their identity is the
/// canonical producer identity. A snapshot-recovered entry has no producer
/// sequence/binding/durability, so it is **not** fabricated into an envelope;
/// it is reported as an explicit unavailable observation owned by the HP.5
/// snapshot projection instead.
fn authoritative_envelopes(view: &PluginReportView) -> (Vec<ReportEnvelope>, usize) {
    let mut out: Vec<ReportEnvelope> = Vec::new();
    let mut unsupported = 0usize;
    for entry in view.entries.values() {
        if entry.generation != view.active_generation {
            continue;
        }
        // Exact provenance link: HP.5 `apply_entry` copies the producing
        // envelope's observed/reported timestamps into the entry, so the
        // current entry for a namespace maps to exactly that applied envelope.
        // No digest comparison and no sequence ordering is used. A recovered
        // entry (snapshot path) carries the snapshot-application clock
        // observation, which no envelope carries, so it stays unsupported and
        // its aggregate source stays Unknown, while later live reports (which do
        // have envelopes) remain servable.
        let mut applied = view.envelopes.iter().filter(|((generation, sequence), envelope)| {
            *generation == view.active_generation
                // Only envelopes applied after the snapshot watermark can
                // correspond to a post-recovery entry; a pre-snapshot envelope
                // cannot be claimed for a recovered row.
                && *sequence > view.snapshot_watermark
                && envelope.schema_namespace == entry.schema_namespace
                && envelope.observed_at_unix_ms == entry.observed_at_unix_ms
                && envelope.reported_at_unix_ms == entry.reported_at_unix_ms
        })
        .map(|(_, envelope)| envelope);
        match (applied.next(), applied.next()) {
            (Some(envelope), None) => out.push(envelope.clone()),
            _ => unsupported += 1,
        }
    }
    out.sort_by(|left, right| {
        left.sequence
            .cmp(&right.sequence)
            .then_with(|| left.schema_namespace.cmp(&right.schema_namespace))
    });
    (out, unsupported)
}

/// A revision fingerprint of the authoritative report collection: the set of
/// current entries and their canonical envelope identities. It changes on any
/// applied report (durable or transient) and on generation/entry replacement,
/// so a page cursor from a superseded collection is refused.
fn report_collection_revision(envelopes: &[ReportEnvelope]) -> Result<u64, ContractError> {
    let keys: Vec<Value> = envelopes
        .iter()
        .map(|envelope| {
            json!({
                "plugin_id": envelope.plugin_id,
                "generation": envelope.binding.generation,
                "namespace": envelope.schema_namespace,
                "schema_version": envelope.schema_version,
                "sequence": envelope.sequence,
                "payload_digest": envelope.payload_digest,
                "observed_at_unix_ms": envelope.observed_at_unix_ms,
            })
        })
        .collect();
    let digest = canonical_digest(&Value::Array(keys))?;
    let hex = digest.trim_start_matches("sha256:");
    let prefix = hex.get(..16).unwrap_or(hex);
    u64::from_str_radix(prefix, 16).map_err(|error| {
        FailureCode::InvalidRecord.failure(
            "component",
            format!("report collection revision digest is not hex: {error}"),
        )
    })
}

/// The opaque cursor position for one component report entry: the namespace,
/// which is the unique authoritative key of a current entry.
fn component_cursor(namespace: &str) -> String {
    namespace.to_string()
}

/// Parse a component report cursor position, refusing a malformed value.
fn parse_component_cursor(raw: &str) -> Result<String, ContractError> {
    if raw.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            "cursor",
            "cursor position is not a component namespace",
        ));
    }
    Ok(raw.to_string())
}

fn synthetic_binding(plugin_id: &str, version: &str) -> BindingPin {
    BindingPin {
        binding_id: plugin_id.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: version.to_string(),
        generation: 0,
        config_digest: sha256_digest(b""),
    }
}

/// Health state implied by lifecycle when no health heartbeat was observed.
fn lifecycle_health(observed: LifecycleState) -> HealthState {
    match observed {
        LifecycleState::Ready | LifecycleState::Draining => HealthState::Ready,
        LifecycleState::Starting => HealthState::Degraded,
        LifecycleState::Installed | LifecycleState::Failed | LifecycleState::Retired => {
            HealthState::Unavailable
        }
    }
}

/// Collect every exact `ArtifactRef`-shaped object with `digest` from `value`.
fn collect_artifact_refs(value: &Value, digest: &str, out: &mut Vec<PackageArtifactRef>) {
    match value {
        Value::Object(map) => {
            if let Ok(reference) =
                serde_json::from_value::<PackageArtifactRef>(Value::Object(map.clone()))
            {
                if reference.digest == digest {
                    out.push(reference);
                }
                return;
            }
            for nested in map.values() {
                collect_artifact_refs(nested, digest, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_artifact_refs(item, digest, out);
            }
        }
        _ => {}
    }
}

/// Lowercase hex of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Current wall-clock milliseconds.
fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
