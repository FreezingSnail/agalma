//! HP.5 durable plugin reporting and snapshot recovery (`agalma-jmr.5`).
//!
//! The hub ingests producer [`ReportPublication`]s and stores the hub-observed
//! [`ReportEnvelope`] (producer input plus the hub's injected observation time).
//! The producer never supplies the observation time: a fresh envelope is stamped
//! from the hub's injected [`Clock`], and an exact replay of an already-recorded
//! identity returns the *original* observed timestamp instead of appending or
//! re-stamping.
//!
//! Identity and content rules (frozen `docs/self-mutation-contracts.md`):
//!
//! * report identity is `(plugin_id, binding generation, sequence)`;
//! * the same identity with identical content is an acknowledged replay;
//! * the same identity with changed content is [`FailureCode::IdentityConflict`];
//! * the authenticated grant determines the caller: a publication whose
//!   `plugin_id`/`binding` differs from the authenticated identity is refused;
//! * a retired (stale) generation may complete its own pinned observations but
//!   cannot overwrite the active generation's view;
//! * a sequence gap marks the view incomplete and schedules a snapshot; a
//!   snapshot replaces the view at its watermark and then applies queued later
//!   reports, and out-of-order duplicates below the watermark never roll back;
//! * durable observations are committed to the selected ledger **before** the
//!   publication is acknowledged; transient heartbeats are ephemeral and
//!   drop-tolerant;
//! * an unknown report schema or an oversized payload is refused visibly.
//!
//! Durable observations and snapshots are written through the selected
//! [`LedgerHandle`] component store (`component-store/v1`), never through
//! private storage. Restart/open rehydrates the derived view and the durable
//! replay window from that store.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use agalma_contracts::digest::{canonical_digest, FailureCode};
use agalma_contracts::{
    BindingPin, ComponentCommit, ComponentCommitAnchor, ComponentEvent, ComponentEventRow,
    ComponentStateRow, ContractError, Durability, HealthState, LedgerApi, PluginHealth,
    PluginSnapshot, ReportEnvelope, ReportPublication, COMPONENT_COMMIT_FORMAT,
};
use agalma_ledger::SqliteLedger;

/// Send-safe durable backing for report ingestion (`component-store/v1`).
///
/// The hub RPC handler must be `Send + Sync`, so the ingestor reaches the
/// selected ledger through this seam rather than a non-`Send` handle. The
/// production implementation wraps the selected [`SqliteLedger`]; tests reuse
/// the same implementation, so the exercised path is the production one.
pub trait ReportStore: Send + Sync {
    /// Atomically commit a component-namespace transaction.
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError>;
    /// Read one component namespace's durable state row.
    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError>;
    /// Component events with sequence `> after`, up to `limit`.
    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError>;
}

impl ReportStore for std::sync::Mutex<SqliteLedger> {
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_commit(commit)
    }

    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_read(namespace)
    }

    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_events(namespace, after, limit)
    }
}

impl ReportStore for std::sync::Mutex<crate::ledger_process::LedgerHandle> {
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_commit(commit)
    }

    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_read(namespace)
    }

    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_events(namespace, after, limit)
    }
}

/// Component namespace holding the hub's durable registry/desired state.
pub const REGISTRY_NAMESPACE: &str = "hub.registry";
/// Prefix of the per-plugin durable report namespace.
pub const REPORT_NAMESPACE_PREFIX: &str = "plugin-report/";
/// Default stale threshold (contract: stale after 15 seconds).
pub const DEFAULT_STALE_AFTER_MS: u64 = 15_000;
/// Default failed/unavailable threshold (contract: failed after 30 seconds).
pub const DEFAULT_FAILED_AFTER_MS: u64 = 30_000;
/// Bounded number of gap-queued transient reports retained per plugin.
pub const DEFAULT_GAP_QUEUE_LIMIT: usize = 64;
/// Maximum component events read in one rehydration page.
const EVENT_PAGE: u32 = 500;

/// The component namespace for one plugin's durable reports.
pub fn report_namespace(plugin_id: &str) -> String {
    format!("{REPORT_NAMESPACE_PREFIX}{plugin_id}")
}

/// Freshness of a plugin's latest durable observation, computed from the hub's
/// injected observation time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// No observation yet.
    Unknown,
    /// Observed within the stale threshold.
    Fresh,
    /// Observed after the stale threshold but before the failed threshold.
    Stale,
    /// Observed after the failed threshold.
    Unavailable,
}

/// One applied report in the active read model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportEntry {
    pub schema_namespace: String,
    pub schema_version: u64,
    pub payload: Value,
    pub payload_digest: String,
    pub generation: u64,
    pub reported_at_unix_ms: u64,
    pub observed_at_unix_ms: u64,
}

/// The hub-observed health heartbeat (transient, never durable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthObservation {
    pub state: HealthState,
    pub detail: String,
    pub checked_at_unix_ms: u64,
    pub observed_at_unix_ms: u64,
}

/// The durable, derived view for one plugin generation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginReportView {
    pub plugin_id: String,
    /// The registry's active generation for this plugin.
    pub active_generation: u64,
    /// Last committed component revision.
    pub revision: u64,
    /// Highest contiguous producer sequence applied to the view.
    pub watermark: u64,
    /// Highest snapshot watermark applied.
    pub snapshot_watermark: u64,
    /// The view is missing a contiguous sequence range and needs a snapshot.
    pub incomplete: bool,
    /// A snapshot request should be issued.
    pub needs_snapshot: bool,
    /// Latest applied observation per schema namespace.
    pub entries: BTreeMap<String, ReportEntry>,
    /// Last observed health heartbeat.
    pub health: Option<HealthObservation>,
    /// Durable replay window keyed by `(generation, producer sequence)` (rebuilt
    /// from component events, never checkpointed).
    #[serde(skip)]
    pub envelopes: BTreeMap<(u64, u64), ReportEnvelope>,
    /// Gap-queued reports keyed by `(generation, producer sequence)` (bounded,
    /// transient across restart by design).
    #[serde(skip)]
    pub queued: BTreeMap<(u64, u64), ReportEnvelope>,
}

impl PluginReportView {
    /// Whether the view currently has a visible gap.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }
}

/// Durable hub report ingestion over the selected ledger component store.
pub struct ReportIngestor {
    store: Box<dyn ReportStore>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    active_generations: BTreeMap<String, u64>,
    allowed_schemas: BTreeSet<(String, u64)>,
    views: BTreeMap<String, PluginReportView>,
    stale_after_ms: u64,
    failed_after_ms: u64,
    gap_queue_limit: usize,
}

impl ReportIngestor {
    /// Open the ingestor, rehydrating every known plugin view and its durable
    /// replay window from the selected ledger.
    ///
    /// `active_generations` names the plugin -> generation the registry currently
    /// considers active. `allowed_schemas` is the set of declared
    /// `(schema namespace, schema version)` pairs; an undeclared pair is refused.
    pub fn open(
        store: Box<dyn ReportStore>,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
        active_generations: BTreeMap<String, u64>,
        allowed_schemas: BTreeSet<(String, u64)>,
    ) -> Result<ReportIngestor, ContractError> {
        let mut views = BTreeMap::new();
        for (plugin_id, generation) in &active_generations {
            let view = rehydrate(store.as_ref(), plugin_id, *generation)?;
            views.insert(plugin_id.clone(), view);
        }
        Ok(ReportIngestor {
            store,
            clock,
            active_generations,
            allowed_schemas,
            views,
            stale_after_ms: DEFAULT_STALE_AFTER_MS,
            failed_after_ms: DEFAULT_FAILED_AFTER_MS,
            gap_queue_limit: DEFAULT_GAP_QUEUE_LIMIT,
        })
    }

    /// Override the freshness thresholds (test/policy injected).
    pub fn with_thresholds(mut self, stale_after_ms: u64, failed_after_ms: u64) -> Self {
        self.stale_after_ms = stale_after_ms;
        self.failed_after_ms = failed_after_ms;
        self
    }

    /// The durable backing store.
    pub fn store(&self) -> &dyn ReportStore {
        self.store.as_ref()
    }

    /// The plugin -> active generation map this ingestor was opened with.
    pub fn active_generations(&self) -> &BTreeMap<String, u64> {
        &self.active_generations
    }

    /// The active generation view for `plugin_id`, if any.
    pub fn view(&self, plugin_id: &str) -> Option<&PluginReportView> {
        self.views.get(plugin_id)
    }

    /// Whether any plugin's active view is visibly incomplete.
    pub fn any_incomplete(&self) -> bool {
        self.views.values().any(|v| v.incomplete)
    }

    /// Freshness of `plugin_id` from the injected clock and the latest
    /// observation.
    pub fn freshness(&self, plugin_id: &str) -> Freshness {
        let Some(view) = self.views.get(plugin_id) else {
            return Freshness::Unknown;
        };
        let latest = view
            .entries
            .values()
            .map(|e| e.observed_at_unix_ms)
            .max()
            .or_else(|| view.health.as_ref().map(|h| h.observed_at_unix_ms));
        let Some(observed_at) = latest else {
            return Freshness::Unknown;
        };
        let now = (self.clock)();
        let age = now.saturating_sub(observed_at);
        if age <= self.stale_after_ms {
            Freshness::Fresh
        } else if age <= self.failed_after_ms {
            Freshness::Stale
        } else {
            Freshness::Unavailable
        }
    }

    /// Refuse a publication that a different authenticated identity tried to
    /// submit.
    fn authenticate(
        &self,
        plugin_id: &str,
        binding: &BindingPin,
        publication: &ReportPublication,
    ) -> Result<(), ContractError> {
        if publication.plugin_id != plugin_id || publication.binding != *binding {
            return Err(FailureCode::AuthorityDenied.failure(
                &publication.plugin_id,
                format!(
                    "publication identity ({}, generation {}) does not match the authenticated grant ({plugin_id}, generation {})",
                    publication.plugin_id,
                    publication.binding.generation,
                    binding.generation
                ),
            ));
        }
        Ok(())
    }

    /// Publish one report as the authenticated caller.
    ///
    /// Returns the stored hub-observed envelope. A replay returns the original
    /// envelope (including its original observation time).
    pub fn publish(
        &mut self,
        authenticated_plugin: &str,
        authenticated_binding: &BindingPin,
        publication: &ReportPublication,
    ) -> Result<ReportEnvelope, ContractError> {
        publication.validate()?;
        self.authenticate(authenticated_plugin, authenticated_binding, publication)?;
        let key = (
            publication.schema_namespace.clone(),
            publication.schema_version,
        );
        if !self.allowed_schemas.contains(&key) {
            return Err(FailureCode::UnsupportedSchema.failure(
                &publication.schema_namespace,
                format!(
                    "report schema {} version {} is not declared by the plugin manifest",
                    publication.schema_namespace, publication.schema_version
                ),
            ));
        }
        let plugin_id = publication.plugin_id.clone();
        let view = self.views.get_mut(&plugin_id).ok_or_else(|| {
            FailureCode::InvalidRecord.failure(&plugin_id, "plugin is not registered")
        })?;

        // Durable/identity replay window (rehydrated from component events).
        if let Some(existing) = view
            .envelopes
            .get(&(publication.binding.generation, publication.sequence))
        {
            return replay(existing, publication);
        }

        // An out-of-order report below the applied watermark (e.g. a report
        // already covered by a snapshot) never rolls the view back.
        if publication.sequence <= view.watermark {
            let envelope = ReportEnvelope::from_publication(publication, (self.clock)());
            if publication.durability == Durability::Durable {
                persist_envelope(self.store.as_ref(), view, &envelope, "report.stale")?;
            }
            return Ok(envelope);
        }

        let observed_at_unix_ms = (self.clock)();
        let envelope = ReportEnvelope::from_publication(publication, observed_at_unix_ms);
        let generation = publication.binding.generation;
        let is_active = generation == view.active_generation;

        // Stale/retired generation: record its pinned observation but never
        // overwrite the active view.
        if !is_active {
            if publication.durability == Durability::Durable {
                persist_envelope(
                    self.store.as_ref(),
                    view,
                    &envelope,
                    "report.retired",
                )?;
            }
            return Ok(envelope);
        }

        let expected = view.watermark.saturating_add(1);
        let contiguous = publication.sequence == expected;
        let mut gap = false;

        if contiguous {
            apply_entry(view, &envelope);
        } else {
            // Out-of-order or gap: queue (bounded) and mark incomplete.
            gap = true;
            view.incomplete = true;
            view.needs_snapshot = true;
            if view.queued.len() >= self.gap_queue_limit {
                // Overflow discards transient data only; the incomplete state
                // remains visible and a snapshot is still scheduled.
                if let Some((&first, _)) = view.queued.iter().next() {
                    if view.queued.get(&first).map(|e| e.durability) == Some(Durability::Transient) {
                        view.queued.remove(&first);
                    }
                }
            }
            view.queued.insert(
                (publication.binding.generation, publication.sequence),
                envelope.clone(),
            );
        }

        if publication.durability == Durability::Durable {
            let kind = if gap { "report.queued" } else { "report.observed" };
            persist_envelope(self.store.as_ref(), view, &envelope, kind)?;
        }
        Ok(envelope)
    }

    /// Record a health heartbeat (transient; never durable).
    pub fn record_health(
        &mut self,
        authenticated_plugin: &str,
        authenticated_binding: &BindingPin,
        health: &PluginHealth,
    ) -> Result<(), ContractError> {
        health.version_gate()?;
        if health.plugin_id != authenticated_plugin || health.binding != *authenticated_binding {
            return Err(FailureCode::AuthorityDenied.failure(
                &health.plugin_id,
                "health observation identity does not match the authenticated grant",
            ));
        }
        let Some(view) = self.views.get_mut(authenticated_plugin) else {
            return Err(FailureCode::InvalidRecord.failure(
                authenticated_plugin,
                "plugin is not registered",
            ));
        };
        view.health = Some(HealthObservation {
            state: health.state,
            detail: health.detail.clone(),
            checked_at_unix_ms: health.checked_at_unix_ms,
            observed_at_unix_ms: (self.clock)(),
        });
        Ok(())
    }

    /// Apply a snapshot: replace the active read model at `snapshot.watermark`,
    /// then apply queued later reports. A stale-generation, out-of-order (lower
    /// watermark) or integrity-invalid snapshot is refused/ignored without
    /// rolling the view back.
    pub fn apply_snapshot(
        &mut self,
        authenticated_plugin: &str,
        authenticated_binding: &BindingPin,
        snapshot: &PluginSnapshot,
    ) -> Result<(), ContractError> {
        snapshot.version_gate()?;
        snapshot.verify_snapshot_digest()?;
        snapshot.verify_payload_digests()?;
        snapshot.check_payload_bound()?;
        if snapshot.plugin_id != authenticated_plugin || snapshot.binding != *authenticated_binding
        {
            return Err(FailureCode::AuthorityDenied.failure(
                &snapshot.plugin_id,
                "snapshot identity does not match the authenticated grant",
            ));
        }
        let Some(view) = self.views.get_mut(authenticated_plugin) else {
            return Err(FailureCode::InvalidRecord.failure(
                authenticated_plugin,
                "plugin is not registered",
            ));
        };
        if snapshot.binding.generation != view.active_generation {
            return Err(FailureCode::StaleGeneration.failure(
                &snapshot.plugin_id,
                format!(
                    "snapshot generation {} is not the active generation {}",
                    snapshot.binding.generation, view.active_generation
                ),
            ));
        }
        if snapshot.watermark <= view.snapshot_watermark {
            // Already applied at this watermark (or older): idempotent no-op;
            // never roll the view back.
            return Ok(());
        }

        view.entries.clear();
        for entry in &snapshot.snapshots {
            view.entries.insert(
                entry.schema_namespace.clone(),
                ReportEntry {
                    schema_namespace: entry.schema_namespace.clone(),
                    schema_version: entry.schema_version,
                    payload: entry.payload.clone(),
                    payload_digest: entry.payload_digest.clone(),
                    generation: snapshot.binding.generation,
                    reported_at_unix_ms: snapshot.watermark,
                    observed_at_unix_ms: (self.clock)(),
                },
            );
        }
        view.snapshot_watermark = snapshot.watermark;
        view.watermark = view.watermark.max(snapshot.watermark);
        view.incomplete = false;
        view.needs_snapshot = false;

        // Apply queued later reports (contiguous above the snapshot watermark).
        let generation = snapshot.binding.generation;
        let mut later: Vec<ReportEnvelope> = view
            .queued
            .values()
            .filter(|e| e.binding.generation == generation && e.sequence > snapshot.watermark)
            .cloned()
            .collect();
        later.sort_by_key(|e| e.sequence);
        for envelope in later {
            if envelope.sequence == view.watermark.saturating_add(1) {
                apply_entry(view, &envelope);
            }
        }
        view.queued
            .retain(|(gen, seq), _| !(*gen == generation && *seq <= view.watermark));
        view.incomplete = !view.queued.is_empty();

        persist_snapshot(self.store.as_ref(), view, snapshot)?;
        Ok(())
    }
}

/// A replay of an existing identity: identical content returns the original
/// envelope; changed content conflicts.
fn replay(
    existing: &ReportEnvelope,
    publication: &ReportPublication,
) -> Result<ReportEnvelope, ContractError> {
    let same = existing.payload_digest == publication.payload_digest
        && existing.schema_namespace == publication.schema_namespace
        && existing.schema_version == publication.schema_version;
    if same {
        Ok(existing.clone())
    } else {
        Err(FailureCode::IdentityConflict.failure(
            &publication.plugin_id,
            format!(
                "report identity ({}, generation {}, sequence {}) reused with changed content",
                publication.plugin_id, publication.binding.generation, publication.sequence
            ),
        ))
    }
}

/// Apply a contiguous envelope to the derived view.
fn apply_entry(view: &mut PluginReportView, envelope: &ReportEnvelope) {
    view.entries.insert(
        envelope.schema_namespace.clone(),
        ReportEntry {
            schema_namespace: envelope.schema_namespace.clone(),
            schema_version: envelope.schema_version,
            payload: envelope.payload.clone(),
            payload_digest: envelope.payload_digest.clone(),
            generation: envelope.binding.generation,
            reported_at_unix_ms: envelope.reported_at_unix_ms,
            observed_at_unix_ms: envelope.observed_at_unix_ms,
        },
    );
    view.watermark = view.watermark.max(envelope.sequence);
    let generation = envelope.binding.generation;
    view.envelopes
        .insert((generation, envelope.sequence), envelope.clone());
    // Drain any contiguous queued reports.
    loop {
        let next = view.watermark.saturating_add(1);
        let Some(queued) = view.queued.remove(&(generation, next)) else {
            break;
        };
        view.entries.insert(
            queued.schema_namespace.clone(),
            ReportEntry {
                schema_namespace: queued.schema_namespace.clone(),
                schema_version: queued.schema_version,
                payload: queued.payload.clone(),
                payload_digest: queued.payload_digest.clone(),
                generation: queued.binding.generation,
                reported_at_unix_ms: queued.reported_at_unix_ms,
                observed_at_unix_ms: queued.observed_at_unix_ms,
            },
        );
        view.watermark = next;
        view.envelopes.insert((generation, next), queued);
    }
    if view.queued.is_empty() {
        view.incomplete = false;
    }
}

/// Commit one envelope and the updated view checkpoint atomically.
fn persist_envelope(
    store: &dyn ReportStore,
    view: &mut PluginReportView,
    envelope: &ReportEnvelope,
    event_kind: &str,
) -> Result<(), ContractError> {
    let namespace = report_namespace(&view.plugin_id);
    view.envelopes
        .insert((envelope.binding.generation, envelope.sequence), envelope.clone());
    let checkpoint = serde_json::to_value(&*view)
        .map_err(|e| ContractError::KnownFailure(format!("report view json: {e}")))?;
    let commit_id = format!(
        "report:{}:{}:{}",
        view.plugin_id, envelope.binding.generation, envelope.sequence
    );
    commit_component(
        store,
        &namespace,
        view,
        commit_id.clone(),
        checkpoint,
        ComponentEvent {
            event_id: commit_id,
            kind: event_kind.to_string(),
            payload: serde_json::to_value(envelope)
                .map_err(|e| ContractError::KnownFailure(format!("report envelope json: {e}")))?,
            recorded_at_unix_ms: envelope.observed_at_unix_ms,
        },
    )?;
    Ok(())
}

/// Commit one applied snapshot and the updated view checkpoint atomically.
fn persist_snapshot(
    store: &dyn ReportStore,
    view: &mut PluginReportView,
    snapshot: &PluginSnapshot,
) -> Result<(), ContractError> {
    let namespace = report_namespace(&view.plugin_id);
    let checkpoint = serde_json::to_value(&*view)
        .map_err(|e| ContractError::KnownFailure(format!("report view json: {e}")))?;
    let commit_id = format!(
        "snapshot:{}:{}:{}",
        view.plugin_id, snapshot.binding.generation, snapshot.watermark
    );
    commit_component(
        store,
        &namespace,
        view,
        commit_id.clone(),
        checkpoint,
        ComponentEvent {
            event_id: commit_id,
            kind: "snapshot.applied".to_string(),
            payload: serde_json::to_value(snapshot)
                .map_err(|e| ContractError::KnownFailure(format!("snapshot json: {e}")))?,
            recorded_at_unix_ms: 0,
        },
    )?;
    Ok(())
}

/// Build, seal and commit a `ComponentCommit/v1` at the view's current revision.
fn commit_component(
    store: &dyn ReportStore,
    namespace: &str,
    view: &mut PluginReportView,
    commit_id: String,
    checkpoint: Value,
    event: ComponentEvent,
) -> Result<(), ContractError> {
    let mut commit = ComponentCommit {
        format: COMPONENT_COMMIT_FORMAT.to_string(),
        namespace: namespace.to_string(),
        commit_id,
        input_digest: String::new(),
        expected_revision: view.revision,
        checkpoint: Some(checkpoint),
        events: vec![event],
        operations: Vec::new(),
        receipts: Vec::new(),
    };
    commit.input_digest = commit.computed_input_digest()?;
    let anchor = store.component_commit(&commit)?;
    view.revision = anchor.revision;
    Ok(())
}

/// Rehydrate one plugin's view and durable replay window from the ledger.
fn rehydrate(
    store: &dyn ReportStore,
    plugin_id: &str,
    active_generation: u64,
) -> Result<PluginReportView, ContractError> {
    let namespace = report_namespace(plugin_id);
    let state = store.component_read(&namespace)?;
    let mut view = match state.as_ref() {
        Some(state) => match state.checkpoint.as_ref() {
            Some(checkpoint) => serde_json::from_value(checkpoint.clone()).map_err(|e| {
                ContractError::KnownFailure(format!(
                    "report view checkpoint for {plugin_id} is invalid: {e}"
                ))
            })?,
            None => PluginReportView::default(),
        },
        None => PluginReportView::default(),
    };
    view.plugin_id = plugin_id.to_string();
    view.active_generation = active_generation;
    view.revision = state.as_ref().map(|s| s.revision).unwrap_or(0);
    view.watermark = state.as_ref().map(|s| s.watermark).unwrap_or(0);

    // Rebuild the durable replay window from component events. This is the
    // durable source of truth for identity dedup across restart/import.
    let mut after = 0u64;
    loop {
        let rows = store.component_events(&namespace, after, EVENT_PAGE)?;
        if rows.is_empty() {
            break;
        }
        let last = rows.last().map(|r| r.sequence).unwrap_or(after);
        for row in rows {
            if matches!(
                row.event.kind.as_str(),
                "report.observed" | "report.queued" | "report.retired" | "report.stale"
            ) {
                if let Ok(envelope) = serde_json::from_value::<ReportEnvelope>(row.event.payload) {
                    view.envelopes
                        .insert((envelope.binding.generation, envelope.sequence), envelope);
                }
            }
        }
        if last <= after {
            break;
        }
        after = last;
    }
    Ok(view)
}

/// Convenience: a snapshot payload for one namespace, with the canonical digest.
pub fn snapshot_entry(
    namespace: &str,
    version: u64,
    payload: Value,
) -> Result<agalma_contracts::PluginSnapshotEntry, ContractError> {
    let payload_digest = canonical_digest(&payload)?;
    Ok(agalma_contracts::PluginSnapshotEntry {
        schema_namespace: namespace.to_string(),
        schema_version: version,
        payload,
        payload_digest,
    })
}

/// Build and seal a `PluginSnapshot/v1` (empty entries plus a watermark) for
/// tests and callers that need a valid recovery payload.
pub fn seal_snapshot(
    plugin_id: &str,
    binding: BindingPin,
    watermark: u64,
    snapshots: Vec<agalma_contracts::PluginSnapshotEntry>,
) -> Result<PluginSnapshot, ContractError> {
    let mut snapshot = PluginSnapshot {
        format: agalma_contracts::PLUGIN_SNAPSHOT_FORMAT.to_string(),
        plugin_id: plugin_id.to_string(),
        binding,
        watermark,
        snapshots,
        snapshot_digest: String::new(),
    };
    snapshot.snapshot_digest = snapshot.recomputed_snapshot_digest()?;
    Ok(snapshot)
}

/// A `ReportPublication/v1` builder used by callers that need a sealed payload.
pub fn seal_publication(
    plugin_id: &str,
    binding: BindingPin,
    schema_namespace: &str,
    schema_version: u64,
    sequence: u64,
    reported_at_unix_ms: u64,
    durability: Durability,
    payload: Value,
) -> Result<ReportPublication, ContractError> {
    let payload_digest = canonical_digest(&payload)?;
    Ok(ReportPublication {
        format: agalma_contracts::REPORT_PUBLICATION_FORMAT.to_string(),
        plugin_id: plugin_id.to_string(),
        binding,
        schema_namespace: schema_namespace.to_string(),
        schema_version,
        sequence,
        reported_at_unix_ms,
        execution_id: None,
        durability,
        payload,
        payload_digest,
    })
}
