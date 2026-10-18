//! HP.4 authenticated capability routing over the common plugin RPC
//! (`agalma-jmr.4`).
//!
//! The router is the hub-owned half of the frozen common plugin protocol
//! (`docs/self-mutation-contracts.md` §"HP: common RPC and callback behavior").
//! A spoke never receives peer sockets or a database handle: it presents a
//! **per-launch scoped grant** and the router authenticates it, validates the
//! method, capability, binding generation, deadline and durable operation
//! identity, applies SF stop admission, journals the operation, and only then
//! routes to a hub-owned typed [`CapabilityAdapter`].
//!
//! Fail-closed guarantees (all proven by `tests/hp_routes.rs`):
//!
//! * a missing, expired, unknown or wrong-method/wrong-generation grant, an
//!   unknown hub method, an ungranted capability, a missing adapter, an expired
//!   deadline and an unknown stop state each refuse **before** any journal write
//!   or adapter call — zero effects;
//! * durable operation identity is `(plugin, generation, operation_id)`, never
//!   the Connect envelope's numeric transport request id, so two independent
//!   clients with overlapping transport ids cannot collide;
//! * the same durable operation id with a changed input is [`FailureCode::IdentityConflict`];
//! * the adapter runs **without** the router lock held (the router lock is only
//!   taken to validate/journal and again to record the terminal receipt), so an
//!   adapter that performs an external RPC can be called back into without
//!   deadlocking; combined with the concurrent Connect dispatcher
//!   ([`agalma_ipc::IpcDispatcher`]) no external call executes under the
//!   dispatcher or registry lock.
//!
//! Durability is injected: [`OperationJournal`] is the HP.5 seam. The in-memory
//! [`InMemoryOperationJournal`] is a test double, not durable production
//! storage, and this module never claims completed durable production action
//! recovery.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use agalma_contracts::digest::{canonical_digest, FailureCode};
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{
    ComponentOperationState, ComponentReceipt, ContractError, PluginOperationStatus,
    ReportEnvelope, ReportPublication, PLUGIN_OPERATION_STATUS_FORMAT,
};
use agalma_ipc::plugin_server::{
    self, HubHandler, METHOD_INVOKE_CAPABILITY, METHOD_PUBLISH_REPORT, METHOD_RECONCILE_CAPABILITY,
};

use crate::plugin_supervisor::HealthProbe;
use crate::stop::{StopControl, StopDecision};

/// Bytes in a per-launch grant token.
pub const GRANT_TOKEN_BYTES: usize = 32;
/// Default maximum nested-capability depth (a spoke cannot recursively
/// authorize the same operation chain without bound).
pub const DEFAULT_MAX_DEPTH: u32 = 2;

/// The scope a per-launch grant authorizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantScope {
    /// Authenticated caller identity (grant wins; a supplied id cannot override).
    pub plugin_id: String,
    /// The pinned binding the grant was issued for.
    pub binding: BindingPin,
    /// The single hub method this grant authorizes.
    pub method: String,
    /// The single capability namespace this grant authorizes.
    pub resource_namespace: String,
    /// Grant expiry (Unix milliseconds); at/after this the grant is refused.
    pub expiry_unix_ms: u64,
    /// Active directive id, when the grant derives from one.
    pub directive: Option<String>,
}

/// A hub-issued per-launch grant.
///
/// The raw token is a private launch-channel secret and is never written to a
/// report, receipt or log; only its [`fingerprint`](Self::fingerprint) is stored
/// hub-side. The grant scope (plugin, generation, method, namespace, expiry) is
/// immutable for the launch; a restart reissues the same scopes under a fresh
/// token (launch epoch), so a leaked old token cannot resume a new launch.
#[derive(Clone, Debug)]
pub struct LaunchGrant {
    token: [u8; GRANT_TOKEN_BYTES],
    scope: GrantScope,
}

impl LaunchGrant {
    /// Issue a grant with a fresh OS-random token.
    ///
    /// The token comes from the operating system CSPRNG (`/dev/urandom`), never
    /// from the PID/clock: a predictable token would let one spoke present
    /// another's grant. A failure to obtain OS randomness is fail-closed — no
    /// grant, no effects.
    pub fn issue(scope: GrantScope) -> Result<Self, ContractError> {
        Ok(LaunchGrant {
            token: fresh_token()?,
            scope,
        })
    }

    /// Issue a grant for one plugin generation and capability.
    pub fn for_capability(
        plugin_id: impl Into<String>,
        binding: BindingPin,
        method: impl Into<String>,
        resource_namespace: impl Into<String>,
        expiry_unix_ms: u64,
        directive: Option<String>,
    ) -> Result<Self, ContractError> {
        LaunchGrant::issue(GrantScope {
            plugin_id: plugin_id.into(),
            binding,
            method: method.into(),
            resource_namespace: resource_namespace.into(),
            expiry_unix_ms,
            directive,
        })
    }

    /// The immutable scope.
    pub fn scope(&self) -> &GrantScope {
        &self.scope
    }

    /// The raw token bytes (private launch channel).
    pub fn token(&self) -> &[u8; GRANT_TOKEN_BYTES] {
        &self.token
    }

    /// Lowercase-hex token, the form a spoke presents over the RPC.
    pub fn token_hex(&self) -> String {
        hex_encode(&self.token)
    }

    /// The hub-side stored fingerprint (`sha256:<hex>` of the token).
    pub fn fingerprint(&self) -> String {
        agalma_contracts::sha256_digest(&self.token)
    }
}

/// The hub's grant table: fingerprint → immutable scope.
#[derive(Clone, Debug, Default)]
pub struct GrantTable {
    by_fingerprint: BTreeMap<String, GrantScope>,
}

impl GrantTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `grant`'s fingerprint and scope; returns the fingerprint.
    pub fn insert(&mut self, grant: &LaunchGrant) -> String {
        let fingerprint = grant.fingerprint();
        self.by_fingerprint
            .insert(fingerprint.clone(), grant.scope.clone());
        fingerprint
    }

    /// Revoke a stored grant by fingerprint.
    pub fn revoke(&mut self, fingerprint: &str) -> bool {
        self.by_fingerprint.remove(fingerprint).is_some()
    }

    /// Number of live grants.
    pub fn len(&self) -> usize {
        self.by_fingerprint.len()
    }

    /// Whether no grant is stored.
    pub fn is_empty(&self) -> bool {
        self.by_fingerprint.is_empty()
    }

    /// Authenticate a presented token.
    ///
    /// A wrong-length token, an unknown fingerprint, or an expired grant is
    /// [`FailureCode::AuthorityDenied`]. The returned scope is the caller's
    /// identity; a supplied plugin id never overrides it.
    pub fn authenticate(&self, token: &[u8], now_ms: u64) -> Result<&GrantScope, ContractError> {
        if token.len() != GRANT_TOKEN_BYTES {
            return Err(FailureCode::AuthorityDenied.failure(
                "grant",
                format!("grant token must be {GRANT_TOKEN_BYTES} bytes"),
            ));
        }
        let fingerprint = agalma_contracts::sha256_digest(token);
        let scope = self.by_fingerprint.get(&fingerprint).ok_or_else(|| {
            FailureCode::AuthorityDenied.failure("grant", "unknown or revoked grant token")
        })?;
        if now_ms >= scope.expiry_unix_ms {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant expired at {} (now {now_ms})",
                    scope.expiry_unix_ms
                ),
            ));
        }
        Ok(scope)
    }
}

/// Injected durable-operation persistence (HP.5 seam).
///
/// `begin` implements the effect-dedup invariant: a repeated durable
/// `operation_key` with identical input returns the existing entry (a replay, so
/// the effect is not repeated), while a changed input under that key is
/// [`FailureCode::IdentityConflict`].
pub trait OperationJournal: Send {
    /// Atomically claim a durable operation.
    ///
    /// Returns [`JournalClaim::fresh`] `true` only for the single caller that
    /// claimed the operation for this `operation_key`; every later caller (a
    /// concurrent retry, an accepted-but-unfinished op, a completed op) gets the
    /// existing entry with `fresh == false`. That is the exactly-once guarantee:
    /// only the fresh claimer runs the adapter. A changed input under the same
    /// key is [`FailureCode::IdentityConflict`].
    fn begin(
        &mut self,
        operation_key: &str,
        operation_id: &str,
        input_digest: &str,
        binding: &BindingPin,
        deadline_unix_ms: u64,
        now_ms: u64,
    ) -> Result<JournalClaim, ContractError>;

    /// The current entry for `operation_key`, if any.
    fn entry(&self, operation_key: &str) -> Option<JournalEntry>;

    /// Record a terminal success + durable receipt.
    fn complete(
        &mut self,
        operation_key: &str,
        outcome: Value,
        now_ms: u64,
    ) -> Result<ComponentReceipt, ContractError>;

    /// Record a known failure (the adapter refused before an effect).
    fn fail(&mut self, operation_key: &str, message: &str, now_ms: u64)
        -> Result<(), ContractError>;

    /// Number of journaled operations.
    fn len(&self) -> usize;
}

/// The outcome of atomically claiming a durable operation.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalClaim {
    pub entry: JournalEntry,
    /// `true` only for the caller that created the operation (it must run the
    /// effect); `false` for a concurrent retry or any later observer.
    pub fresh: bool,
}

/// One durable operation entry.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    pub operation_id: String,
    pub input_digest: String,
    pub binding: BindingPin,
    pub state: ComponentOperationState,
    pub deadline_unix_ms: u64,
    pub receipt: Option<ComponentReceipt>,
}

/// In-memory journal (test double; HP.5 supplies the durable one).
#[derive(Clone, Debug, Default)]
pub struct InMemoryOperationJournal {
    entries: BTreeMap<String, JournalEntry>,
}

impl InMemoryOperationJournal {
    /// An empty journal.
    pub fn new() -> Self {
        Self::default()
    }
}

impl OperationJournal for InMemoryOperationJournal {
    fn begin(
        &mut self,
        operation_key: &str,
        operation_id: &str,
        input_digest: &str,
        binding: &BindingPin,
        deadline_unix_ms: u64,
        _now_ms: u64,
    ) -> Result<JournalClaim, ContractError> {
        if let Some(existing) = self.entries.get(operation_key) {
            if existing.input_digest != input_digest {
                return Err(FailureCode::IdentityConflict.failure(
                    operation_id,
                    format!(
                        "operation {operation_key} reused with changed input ({} != {input_digest})",
                        existing.input_digest
                    ),
                ));
            }
            return Ok(JournalClaim {
                entry: existing.clone(),
                fresh: false,
            });
        }
        let entry = JournalEntry {
            operation_id: operation_id.to_string(),
            input_digest: input_digest.to_string(),
            binding: binding.clone(),
            state: ComponentOperationState::Accepted,
            deadline_unix_ms,
            receipt: None,
        };
        self.entries.insert(operation_key.to_string(), entry.clone());
        Ok(JournalClaim {
            entry,
            fresh: true,
        })
    }

    fn entry(&self, operation_key: &str) -> Option<JournalEntry> {
        self.entries.get(operation_key).cloned()
    }

    fn complete(
        &mut self,
        operation_key: &str,
        outcome: Value,
        now_ms: u64,
    ) -> Result<ComponentReceipt, ContractError> {
        let entry = self.entries.get_mut(operation_key).ok_or_else(|| {
            FailureCode::InvalidRecord.failure(operation_key, "operation is not journaled")
        })?;
        let receipt = ComponentReceipt {
            operation_id: entry.operation_id.clone(),
            input_digest: entry.input_digest.clone(),
            binding: entry.binding.clone(),
            outcome,
            completed_at_unix_ms: now_ms,
        };
        entry.state = ComponentOperationState::Completed;
        entry.receipt = Some(receipt.clone());
        Ok(receipt)
    }

    fn fail(
        &mut self,
        operation_key: &str,
        message: &str,
        _now_ms: u64,
    ) -> Result<(), ContractError> {
        if let Some(entry) = self.entries.get_mut(operation_key) {
            entry.state = ComponentOperationState::Failed;
            entry.receipt = None;
            let _ = message;
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A hub-owned typed capability adapter.
///
/// An adapter validates canonical typed inputs (never an arbitrary shell
/// command or an arbitrary ledger method name) and performs the effect. It runs
/// outside the router lock.
pub trait CapabilityAdapter: Send {
    /// The capability namespace this adapter serves (e.g. `ledger.read`).
    fn capability(&self) -> &str;
    /// Perform the capability for an authenticated caller `scope`.
    fn invoke(&mut self, scope: &GrantScope, input: &Value) -> Result<Value, ContractError>;
}

impl CapabilityAdapter for Box<dyn CapabilityAdapter> {
    fn capability(&self) -> &str {
        (**self).capability()
    }

    fn invoke(&mut self, scope: &GrantScope, input: &Value) -> Result<Value, ContractError> {
        (**self).invoke(scope, input)
    }
}

/// Stop admission seam for effect requests.
///
/// The router requires [`StopDecision::Running`]; an explicit stop is
/// [`FailureCode::Stopped`] and an unreadable/unknown stop state is
/// [`FailureCode::StateUnavailable`] (fail-closed). Implemented for any
/// `StopControl` so production reuses the shared SF stop admission.
pub trait StopAdmission: Send + Sync {
    /// The current three-way admission decision.
    fn decision(&self) -> StopDecision;
}

impl<T: StopControl + Send + Sync> StopAdmission for T {
    fn decision(&self) -> StopDecision {
        StopControl::decision(self)
    }
}

/// A stop-admission policy that always permits effect admission.
///
/// Used as the default when no stop source is injected; production wires the
/// shared SF stop control here. Never mutates stop state.
pub struct RunningStop;

impl StopAdmission for RunningStop {
    fn decision(&self) -> StopDecision {
        StopDecision::Running
    }
}

/// Serve the hub capability RPC (`hub.InvokeCapability`/`ReconcileCapability`/
/// `PublishReport`) on `socket` until the process stops.
///
/// This is the production host attachment: the hub listens on a bounded UDS and
/// spoke callbacks authenticate with their per-launch grant. The router lock is
/// released before any adapter runs (see [`RouterService`]).
pub async fn serve_hub(
    socket: &std::path::Path,
    router: Arc<RouterService>,
) -> std::io::Result<()> {
    let info = agalma_ipc::plugin_server::plugin_server_info(
        "hub",
        env!("CARGO_PKG_VERSION"),
        Vec::<String>::new(),
    );
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut server = agalma_ipc::Server::new(info);
    agalma_ipc::register_hub(&mut server, router);
    agalma_ipc::connect::serve(server, socket).await
}

/// A capability request over the hub RPC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCallRequest {
    /// The presented per-launch grant token (lowercase hex).
    pub grant: String,
    /// The caller's declared plugin id (checked against the grant).
    pub plugin_id: String,
    /// The caller's declared binding generation (checked against the grant).
    pub generation: u64,
    /// The hub method being called (checked against the grant).
    pub method: String,
    /// The requested capability namespace (checked against the grant).
    pub capability: String,
    /// Caller-supplied durable operation id (never the transport id).
    pub operation_id: String,
    /// Canonical typed input for the capability.
    pub input: Value,
    /// Deadline (Unix milliseconds); at/after this a new effect is refused.
    pub deadline_unix_ms: u64,
    /// Nested-capability depth (0 for a top-level call).
    #[serde(default)]
    pub depth: u32,
}

/// A report publication over the hub RPC (`hub.PublishReport`).
///
/// The authenticated per-launch grant determines the caller; `plugin_id` and
/// `generation` are the caller's declared identity and must match the grant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishReportCall {
    /// The presented per-launch grant token (lowercase hex).
    pub grant: String,
    /// The caller's declared plugin id (checked against the grant).
    pub plugin_id: String,
    /// The caller's declared binding generation (checked against the grant).
    pub generation: u64,
    /// The producer's report publication (identity/content validated by the
    /// ingestor; the hub adds the observation timestamp).
    pub publication: ReportPublication,
}

/// A validated, journaled capability invocation ready to execute.
pub struct PreparedInvocation {    pub operation_key: String,
    pub operation_id: String,
    pub input: Value,
    pub scope: GrantScope,
    pub adapter: Arc<Mutex<dyn CapabilityAdapter>>,
    /// `Some` when the operation already existed (a replay: do not re-run).
    pub replayed: Option<PluginOperationStatus>,
}

/// The hub capability router.
pub struct CapabilityRouter {
    grants: GrantTable,
    journal: Box<dyn OperationJournal>,
    adapters: BTreeMap<String, Arc<Mutex<dyn CapabilityAdapter>>>,
    stop: Arc<dyn StopAdmission>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    max_depth: u32,
    reports: Option<Arc<Mutex<crate::plugin_reports::ReportIngestor>>>,
}

impl CapabilityRouter {
    /// Build a router with injected persistence, stop admission and clock.
    pub fn new(journal: Box<dyn OperationJournal>, stop: Arc<dyn StopAdmission>) -> Self {
        CapabilityRouter {
            grants: GrantTable::new(),
            journal,
            adapters: BTreeMap::new(),
            stop,
            now: Arc::new(system_now_ms),
            max_depth: DEFAULT_MAX_DEPTH,
            reports: None,
        }
    }

    /// Attach the durable report ingestor (`hub.PublishReport`).
    pub fn install_reports(
        &mut self,
        reports: Arc<Mutex<crate::plugin_reports::ReportIngestor>>,
    ) {
        self.reports = Some(reports);
    }

    /// Authenticate and route one `hub.PublishReport` call to the attached
    /// ingestor. A missing ingestor refuses before any effect.
    pub fn publish_report(
        &mut self,
        call: &PublishReportCall,
    ) -> Result<ReportEnvelope, ContractError> {
        let token = hex_decode_32(&call.grant)?;
        let now = (self.now)();
        let scope = self.grants.authenticate(&token, now)?.clone();
        if scope.method != METHOD_PUBLISH_REPORT {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{METHOD_PUBLISH_REPORT}`",
                    scope.method
                ),
            ));
        }
        if scope.plugin_id != call.plugin_id {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant identity `{}` does not match supplied `{}`",
                    scope.plugin_id, call.plugin_id
                ),
            ));
        }
        if scope.binding.generation != call.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &scope.plugin_id,
                format!(
                    "grant pins generation {}, request declared {}",
                    scope.binding.generation, call.generation
                ),
            ));
        }
        let Some(reports) = self.reports.as_ref() else {
            return Err(ContractError::UnsupportedCapability(format!(
                "report ingestion is not attached to this hub ({METHOD_PUBLISH_REPORT})"
            )));
        };
        let mut ingestor = reports.lock().unwrap_or_else(PoisonError::into_inner);
        ingestor.publish(&scope.plugin_id, &scope.binding, &call.publication)
    }

    /// Override the clock (deterministic tests).
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    /// Override the nested-capability depth limit.
    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Register a hub-owned adapter, keyed by its capability namespace.
    pub fn register_adapter(&mut self, adapter: impl CapabilityAdapter + 'static) {
        let capability = adapter.capability().to_string();
        self.adapters
            .insert(capability, Arc::new(Mutex::new(adapter)));
    }

    /// Install a per-launch grant; returns its stored fingerprint.
    pub fn install_grant(&mut self, grant: &LaunchGrant) -> String {
        self.grants.insert(grant)
    }

    /// Revoke a grant by fingerprint (returns whether one was removed).
    pub fn revoke_grant(&mut self, fingerprint: &str) -> bool {
        self.grants.revoke(fingerprint)
    }

    /// Number of live grants (diagnostics/tests).
    pub fn live_grants(&self) -> usize {
        self.grants.len()
    }

    /// Number of journaled operations (diagnostics/tests).
    pub fn journaled_operations(&self) -> usize {
        self.journal.len()
    }

    /// The current entry for a bare operation id under `plugin_id`/`generation`.
    pub fn operation_entry(
        &self,
        plugin_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Option<JournalEntry> {
        self.journal.entry(&operation_key(plugin_id, generation, operation_id))
    }

    /// Validate and journal one capability request. **No effect** happens here:
    /// on any refusal nothing is journaled and no adapter is consulted.
    pub fn prepare(&mut self, call: &CapabilityCallRequest) -> Result<PreparedInvocation, ContractError> {
        if call.depth > self.max_depth {
            return Err(FailureCode::MissingCapability.failure(
                &call.plugin_id,
                format!(
                    "nested capability depth {} exceeds the {} limit",
                    call.depth, self.max_depth
                ),
            ));
        }
        let token = hex_decode_32(&call.grant)?;
        let now = (self.now)();
        // Authenticate: missing token, unknown token, and expired grant all
        // refuse with zero effects.
        let scope = self.grants.authenticate(&token, now)?.clone();

        // Method validation: an unknown hub method is a typed refusal.
        if !plugin_server::is_hub_method(&call.method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "unknown hub method `{}`",
                call.method
            )));
        }
        // The grant must be for exactly this method.
        if scope.method != call.method {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{}`",
                    scope.method, call.method
                ),
            ));
        }
        // Authenticated identity wins; a supplied plugin id cannot override it.
        if scope.plugin_id != call.plugin_id {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant identity `{}` does not match supplied `{}`",
                    scope.plugin_id, call.plugin_id
                ),
            ));
        }
        // Binding generation validation.
        if scope.binding.generation != call.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &scope.plugin_id,
                format!(
                    "grant pins generation {}, request declared {}",
                    scope.binding.generation, call.generation
                ),
            ));
        }
        // Capability validation: the grant authorizes one namespace.
        if scope.resource_namespace != call.capability {
            return Err(FailureCode::MissingCapability.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{}`",
                    scope.resource_namespace, call.capability
                ),
            ));
        }
        let adapter = self.adapters.get(&call.capability).cloned().ok_or_else(|| {
            FailureCode::MissingCapability.failure(
                &call.capability,
                "no hub adapter is registered for this capability",
            )
        })?;
        // Deadline validation: an expired deadline refuses a new effect.
        if call.deadline_unix_ms <= now {
            return Err(FailureCode::DeadlineExceeded.failure(
                &scope.plugin_id,
                format!(
                    "deadline {} already passed (now {now})",
                    call.deadline_unix_ms
                ),
            ));
        }
        // Stop admission: only Running permits an effect.
        match self.stop.decision() {
            StopDecision::Running => {}
            StopDecision::Stopped(reason) => {
                return Err(FailureCode::Stopped.failure(&scope.plugin_id, reason))
            }
            StopDecision::Unavailable(reason) => {
                return Err(FailureCode::StateUnavailable.failure(&scope.plugin_id, reason))
            }
        }

        let input_digest = canonical_digest(&call.input)?;
        let key = operation_key(&scope.plugin_id, scope.binding.generation, &call.operation_id);
        let claim = self.journal.begin(
            &key,
            &call.operation_id,
            &input_digest,
            &scope.binding,
            call.deadline_unix_ms,
            now,
        )?;
        // Only the fresh claimer may run the effect. Any existing entry — even an
        // accepted-but-unfinished one — returns its prior status without a second
        // adapter call, so concurrent retries dedup to exactly one effect.
        let replayed = if claim.fresh {
            None
        } else {
            Some(entry_status(&scope, &call.operation_id, &claim.entry))
        };
        Ok(PreparedInvocation {
            operation_key: key,
            operation_id: call.operation_id.clone(),
            input: call.input.clone(),
            scope,
            adapter,
            replayed,
        })
    }

    /// Record the terminal outcome of a prepared invocation.
    ///
    /// Called after the adapter ran, again briefly, so the adapter never ran
    /// under the router lock.
    pub fn finish(
        &mut self,
        prepared: &PreparedInvocation,
        outcome: Result<Value, ContractError>,
    ) -> Result<PluginOperationStatus, ContractError> {
        let now = (self.now)();
        match outcome {
            Ok(value) => {
                let receipt = self.journal.complete(&prepared.operation_key, value, now)?;
                Ok(PluginOperationStatus {
                    format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
                    plugin_id: prepared.scope.plugin_id.clone(),
                    binding: prepared.scope.binding.clone(),
                    operation_id: prepared.operation_id.clone(),
                    input_digest: receipt.input_digest.clone(),
                    state: ComponentOperationState::Completed,
                    receipt: Some(receipt),
                })
            }
            Err(error) => {
                self.journal
                    .fail(&prepared.operation_key, &error.to_string(), now)?;
                Err(error)
            }
        }
    }

    /// Reconcile a durable operation without re-running it.
    pub fn reconcile(
        &mut self,
        call: &CapabilityCallRequest,
    ) -> Result<PluginOperationStatus, ContractError> {
        let token = hex_decode_32(&call.grant)?;
        let now = (self.now)();
        let scope = self.grants.authenticate(&token, now)?.clone();
        // Reconcile is its own frozen method scope: the grant must be issued for
        // `hub.ReconcileCapability`, not borrowed from an invoke grant.
        if !plugin_server::is_hub_method(&call.method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "unknown hub method `{}`",
                call.method
            )));
        }
        if scope.method != METHOD_RECONCILE_CAPABILITY || call.method != METHOD_RECONCILE_CAPABILITY {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{METHOD_RECONCILE_CAPABILITY}`",
                    scope.method
                ),
            ));
        }
        if scope.plugin_id != call.plugin_id {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                "grant identity does not match the reconcile request",
            ));
        }
        if scope.binding.generation != call.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &scope.plugin_id,
                format!(
                    "grant pins generation {}, request declared {}",
                    scope.binding.generation, call.generation
                ),
            ));
        }
        if scope.resource_namespace != call.capability {
            return Err(FailureCode::MissingCapability.failure(
                &scope.plugin_id,
                "grant does not authorize this capability",
            ));
        }
        let key = operation_key(&scope.plugin_id, scope.binding.generation, &call.operation_id);
        let entry = self.journal.entry(&key).ok_or_else(|| {
            FailureCode::StateUnavailable.failure(
                &call.operation_id,
                "operation is not journaled for this binding",
            )
        })?;
        Ok(entry_status(&scope, &call.operation_id, &entry))
    }

    /// Prepare, execute the adapter **outside the router lock**, and record the
    /// terminal status. Returns the terminal [`PluginOperationStatus`].
    pub fn invoke_and_execute(
        &mut self,
        call: &CapabilityCallRequest,
    ) -> Result<PluginOperationStatus, ContractError> {
        let prepared = self.prepare(call)?;
        if let Some(status) = prepared.replayed.clone() {
            return Ok(status);
        }
        let scope = prepared.scope.clone();
        let input = prepared.input.clone();
        let outcome = {
            // The adapter lock is the adapter's own; the router lock is not held.
            let mut adapter = prepared
                .adapter
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            adapter.invoke(&scope, &input)
        };
        self.finish(&prepared, outcome)
    }
}

/// The router exposed as the hub RPC handler (`HubHandler`).
///
/// The router lock is held only to validate/journal and to record the terminal
/// status; the adapter call runs with the lock released so a nested hub callback
/// cannot deadlock.
pub struct RouterService {
    router: Mutex<CapabilityRouter>,
}

impl RouterService {
    /// Wrap a router in an RPC-facing service.
    pub fn new(router: CapabilityRouter) -> RouterService {
        RouterService {
            router: Mutex::new(router),
        }
    }

    /// Lock the underlying router (registration/testing).
    pub fn router(&self) -> std::sync::MutexGuard<'_, CapabilityRouter> {
        self.router.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl HubHandler for RouterService {
    fn invoke_capability(&self, request: Value) -> Result<Value, ContractError> {
        let call: CapabilityCallRequest = serde_json::from_value(request).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                METHOD_INVOKE_CAPABILITY,
                format!("malformed capability call: {e}"),
            )
        })?;
        // Validate + journal under the lock, then release it.
        let (status, prepared) = {
            let mut router = self.router.lock().unwrap_or_else(PoisonError::into_inner);
            let prepared = router.prepare(&call)?;
            if let Some(status) = prepared.replayed.clone() {
                (Some(status), None)
            } else {
                (None, Some(prepared))
            }
        };
        let Some(prepared) = prepared else {
            return to_value(status.expect("replay carries a status"));
        };
        // Adapter runs with the router lock released.
        let scope = prepared.scope.clone();
        let input = prepared.input.clone();
        let outcome = {
            let mut adapter = prepared
                .adapter
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            adapter.invoke(&scope, &input)
        };
        let status = {
            let mut router = self.router.lock().unwrap_or_else(PoisonError::into_inner);
            router.finish(&prepared, outcome)?
        };
        to_value(status)
    }

    fn reconcile_capability(&self, request: Value) -> Result<Value, ContractError> {
        let call: CapabilityCallRequest = serde_json::from_value(request).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                METHOD_RECONCILE_CAPABILITY,
                format!("malformed reconcile call: {e}"),
            )
        })?;
        let mut router = self.router.lock().unwrap_or_else(PoisonError::into_inner);
        let status = router.reconcile(&call)?;
        to_value(status)
    }

    fn publish_report(&self, request: Value) -> Result<Value, ContractError> {
        let call: PublishReportCall = serde_json::from_value(request).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                METHOD_PUBLISH_REPORT,
                format!("malformed publish-report call: {e}"),
            )
        })?;
        let mut router = self.router.lock().unwrap_or_else(PoisonError::into_inner);
        let envelope = router.publish_report(&call)?;
        serde_json::to_value(envelope).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                METHOD_PUBLISH_REPORT,
                format!("encode report envelope: {e}"),
            )
        })
    }
}

/// Serialize a status record into the RPC result.
fn to_value(status: PluginOperationStatus) -> Result<Value, ContractError> {
    serde_json::to_value(status).map_err(|e| {
        FailureCode::InvalidRecord.failure("PluginOperationStatus", format!("encode status: {e}"))
    })
}

/// Build a `PluginOperationStatus/v1` from a journal entry.
fn entry_status(scope: &GrantScope, operation_id: &str, entry: &JournalEntry) -> PluginOperationStatus {
    PluginOperationStatus {
        format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
        plugin_id: scope.plugin_id.clone(),
        binding: entry.binding.clone(),
        operation_id: operation_id.to_string(),
        input_digest: entry.input_digest.clone(),
        state: entry.state,
        receipt: entry.receipt.clone(),
    }
}

/// The durable operation key: plugin + pinned generation + caller operation id.
///
/// The Connect envelope's numeric request id is deliberately absent, so two
/// independent clients that allocate overlapping transport ids do not collide.
fn operation_key(plugin_id: &str, generation: u64, operation_id: &str) -> String {
    format!("{plugin_id}#{generation}#{operation_id}")
}

/// The current wall clock in Unix milliseconds (public for grant expiry).
pub fn wall_clock_now_ms() -> u64 {
    system_now_ms()
}

/// The current wall clock in Unix milliseconds.
fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Read a fresh 32-byte token from the OS CSPRNG (`/dev/urandom`).
///
/// No new dependency: the platform OS source is the approved randomness API.
/// The private launch channel carries the token to the spoke.
fn fresh_token() -> Result<[u8; GRANT_TOKEN_BYTES], ContractError> {
    use std::io::Read;
    let mut token = [0u8; GRANT_TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut token))
        .map_err(|err| {
            FailureCode::StateUnavailable.failure(
                "grant",
                format!("OS randomness (/dev/urandom) unavailable: {err}"),
            )
        })?;
    Ok(token)
}

/// Lowercase hex of `bytes`.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Decode exactly [`GRANT_TOKEN_BYTES`] from a lowercase/uppercase hex string.
///
/// Shared with the HP.7 action service ([`crate::plugin_actions`]) so grant
/// presentation has one decoder; behavior is unchanged.
pub(crate) fn hex_decode_32(value: &str) -> Result<[u8; GRANT_TOKEN_BYTES], ContractError> {
    if value.len() != GRANT_TOKEN_BYTES * 2 {
        return Err(FailureCode::AuthorityDenied.failure(
            "grant",
            format!(
                "grant token must be {} hex characters",
                GRANT_TOKEN_BYTES * 2
            ),
        ));
    }
    let mut token = [0u8; GRANT_TOKEN_BYTES];
    for (i, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| {
            FailureCode::AuthorityDenied.failure("grant", "grant token is not valid hex")
        })?;
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// Production host RPC attachment: the real health probe
// ---------------------------------------------------------------------------

/// The real capability-scoped RPC health probe (HP.4 host attachment).
///
/// Replaces the HP.3 placeholder: after a child is spawned under launch `epoch`,
/// it connects to the child's deterministic socket
/// ([`crate::plugin_registry::plugin_socket_path`]), performs the common plugin
/// protocol handshake (identity + protocol-major agreement) and reads
/// `plugin.Health`, reporting Ready only for a `ready` observation. Any transport
/// or handshake failure is "not Ready" (the entry stays `Starting`), never a
/// silent Ready.
pub struct RpcHealthProbe {
    state_dir: PathBuf,
    runtime: tokio::runtime::Runtime,
}

impl RpcHealthProbe {
    /// Build the probe for a plugin-host state dir.
    pub fn new(state_dir: PathBuf) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("plugin health probe runtime");
        RpcHealthProbe { state_dir, runtime }
    }

    /// The socket a launched child serves for `plugin_id`/`epoch`.
    pub fn socket(&self, plugin_id: &str, epoch: u64) -> PathBuf {
        crate::plugin_registry::plugin_socket_path(&self.state_dir, plugin_id, epoch)
    }
}

impl HealthProbe for RpcHealthProbe {
    fn probe(&mut self, plugin_id: &str, epoch: u64) -> Result<bool, ContractError> {
        let socket = self.socket(plugin_id, epoch);
        let probe = async {
            let mut client = agalma_ipc::PluginClient::connect(&socket)
                .await
                .map_err(|e| {
                    FailureCode::StateUnavailable.failure(
                        plugin_id,
                        format!("plugin RPC connect failed: {e}"),
                    )
                })?;
            client.handshake(plugin_id).await.map_err(|e| {
                FailureCode::StateUnavailable.failure(
                    plugin_id,
                    format!("plugin handshake refused: {e}"),
                )
            })?;
            client.health().await.map_err(|e| {
                FailureCode::StateUnavailable.failure(
                    plugin_id,
                    format!("plugin health refused: {e}"),
                )
            })
        };
        let health = block_on_runtime(&self.runtime, probe);
        match health {
            Ok(record) => Ok(
                record.get("state").and_then(Value::as_str) == Some("ready"),
            ),
            // A failed probe means "not Ready yet" (the entry stays Starting);
            // it never fabricates readiness.
            Err(_) => Ok(false),
        }
    }
}

/// Drive `future` to completion on `runtime`, stepping aside if already inside
/// a Tokio runtime (the production supervise loop is async).
fn block_on_runtime<F: std::future::Future>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| runtime.block_on(future))
    } else {
        runtime.block_on(future)
    }
}
