//! Hub-owned factory runtime observation and conservative pause/resume control
//! adapters (WEB.11, `agalma-rwn.11`).
//!
//! Registers two authenticated capabilities behind HP.4 routing:
//!
//! - `factory.read/v1` — the bounded `FactoryRuntime/v1` snapshot and the
//!   read-only `FactoryControl/v1` receipt lookup. Observation-only: never
//!   journaled, never mutating, and admitted while factory effect admission is
//!   stopped.
//! - `factory.control/v1` — human pause/resume. Effectful, but explicitly
//!   stop-exempt (the narrow authenticated exception): pause and resume must be
//!   callable while admission is stopped.
//!
//! The adapter validates the WEB.9 request contracts, invokes an injected
//! [`RuntimeObservationSource`] (WEB.10 owns the source; a missing source yields
//! a bounded `Unavailable` snapshot with nullable identity), and is the **sole**
//! composer of the trusted [`FactoryRuntimePermissions`] flags. A raw source
//! flag is never authority: permission requires a healthy source observation,
//! the authenticated caller's live approved `factory.control/v1` binding (an
//! injected [`ControlBindingResolver`], supplied by WEB.13), and the exclusive
//! [`ControlOwnership`] guard. A read grant alone never enables control.
//!
//! Control durability reuses the existing declared ledger control-receipt
//! storage with deterministic ids `factory-control.intent/<action_id>` and
//! `factory-control.result/<action_id>`. The algorithm is conservative, not
//! atomic exactly-once: it persists intent before the effect, replays a
//! completed result exactly (immutable timestamp), returns `EffectUnconfirmed`
//! for an orphan intent without reexecuting it, refuses conflicting input under
//! the same action id, and never promotes an uncertain effect/result write to a
//! terminal receipt.
//!
//! Storage stays behind the injected [`crate::control::FactoryControlLedger`]
//! seam (production binds the selected `LedgerApi`; tests inject boundary
//! faults); no direct SQL. The state directory is used only to observe/remove
//! the `STOP` sentinel through the existing legacy sentinel check.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use agalma_contracts::digest::FailureCode;
use agalma_contracts::{
    ContractError, ControlReceipt, ExactU64, FactoryActionRequest, FactoryControlActor,
    FactoryControlNotFoundResponse, FactoryControlOutcome, FactoryControlReceiptBody,
    FactoryControlReceiptResponse, FactoryControlRequest, FactoryControlResponse,
    FactoryControlResult, FactoryControlUnconfirmedResponse, FactoryEligiblePage,
    FactoryLookupRequest, FactoryRuntime, FactoryRuntimePermissions, FactoryRuntimeRequest,
    FactoryRuntimeSource, FactoryRuntimeStatus, FactorySourceName, FactoryStopObservation,
    FactoryUnconfirmedCode, RuntimeObservationSource, SourceState, FACTORY_CONTROL_CAPABILITY,
    FACTORY_CONTROL_FAMILY, FACTORY_CONTROL_FORMAT, FACTORY_READ_CAPABILITY, FACTORY_RUNTIME_FORMAT,
};
use agalma_ipc::plugin_server::METHOD_INVOKE_CAPABILITY;

use crate::control::{apply_checked_latch, Actor, ControlAction, ControlRequest, FactoryControlLedger};
use crate::plugin_router::{CapabilityAdapter, CapabilityRouter, GrantScope};
use crate::stop::{StopControl, StopDecision};

/// The authenticated caller's live approved `factory.control/v1` binding.
///
/// WEB.13 supplies this from current grants, the installed binding and the
/// exclusive owner state; it is intentionally not derived from a read grant, a
/// UI actor field, or a plugin report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedControlBinding {
    /// The installed plugin id the binding was approved for.
    pub plugin_id: String,
    /// The capability namespace (must be `factory.control/v1`).
    pub capability: String,
    /// The pinned binding generation.
    pub generation: u64,
    /// Grant expiry (Unix milliseconds); at/after this the approval is stale.
    pub expires_at_unix_ms: u64,
}

/// Injected authoritative resolver for a caller's live control binding.
///
/// The default is no resolver: control remains disabled (fail-closed). A
/// resolver returning `None` also disables control for that caller.
pub trait ControlBindingResolver: Send + Sync {
    /// The caller's currently approved control binding, or `None`.
    fn resolve(&self, plugin_id: &str) -> Option<ApprovedControlBinding>;
}

/// Injected proof that this process holds the exclusive factory-control owner
/// guard for the service lifetime.
///
/// A process mutex is not sufficient cross-process ownership; WEB.13 establishes
/// a dedicated state-scoped `factory-control.lock`. The default has no owner, so
/// controls are disabled.
pub trait ControlOwnership: Send + Sync {
    /// Whether this process currently holds the exclusive control owner.
    fn owned(&self) -> bool;
}

/// The stage of a persisted `factory_control_v1` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FactoryControlStage {
    Intent,
    Result,
}

/// The namespaced durable detail persisted inside a `ControlReceipt.request`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactoryControlRecord {
    stage: FactoryControlStage,
    /// The exact validated public action object (never the outer grant/session).
    request: FactoryActionRequest,
    request_digest: String,
    /// Terminal outcome (`result` stage only).
    #[serde(default)]
    outcome: Option<FactoryControlOutcome>,
    /// Terminal reason (`result` stage only).
    #[serde(default)]
    reason: Option<String>,
}

/// The deterministic durable intent id for a public action id.
fn intent_ledger_id(action_id: &str) -> String {
    format!("factory-control.intent/{action_id}")
}

/// The deterministic durable result id for a public action id.
fn result_ledger_id(action_id: &str) -> String {
    format!("factory-control.result/{action_id}")
}

/// A validated durable control pair for one public action id.
struct DurableControl {
    /// The parsed intent record and its stored row, when an intent exists.
    intent: Option<(FactoryControlRecord, ControlReceipt)>,
    /// The parsed result record and its stored row, when a result exists.
    result: Option<(FactoryControlRecord, ControlReceipt)>,
}

/// The hub-owned factory observation/control service.
#[derive(Clone)]
pub struct FactoryControlService {
    repository_id: String,
    state_id: String,
    runtime: Option<Arc<dyn RuntimeObservationSource>>,
    ledger: Arc<Mutex<Box<dyn FactoryControlLedger>>>,
    stop: Option<Arc<dyn StopControl + Send + Sync>>,
    authority: Option<Arc<dyn ControlBindingResolver>>,
    ownership: Option<Arc<dyn ControlOwnership>>,
    /// Serializes API control within this process (the cross-process owner is
    /// the injected [`ControlOwnership`] guard).
    control_lock: Arc<Mutex<()>>,
    state_dir: PathBuf,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl FactoryControlService {
    /// Build the service over the configured repository/state identities and the
    /// bound ledger. Runtime source, stop observation, control authority and
    /// ownership are attached with the builders below; every unattached seam is
    /// a visible disabled/unknown state, never an implied authority.
    pub fn new(
        repository_id: impl Into<String>,
        state_id: impl Into<String>,
        ledger: Box<dyn FactoryControlLedger>,
        state_dir: impl Into<PathBuf>,
    ) -> Self {
        FactoryControlService {
            repository_id: repository_id.into(),
            state_id: state_id.into(),
            runtime: None,
            ledger: Arc::new(Mutex::new(ledger)),
            stop: None,
            authority: None,
            ownership: None,
            control_lock: Arc::new(Mutex::new(())),
            state_dir: state_dir.into(),
            now: Arc::new(system_now_ms),
        }
    }

    /// Attach the injected runtime observation source (WEB.10).
    pub fn with_runtime(mut self, runtime: Arc<dyn RuntimeObservationSource>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Attach the live stop observation (read-only; never clears a stop).
    pub fn with_stop(mut self, stop: Arc<dyn StopControl + Send + Sync>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Attach the authoritative live control-binding resolver (WEB.13).
    pub fn with_authority(mut self, authority: Arc<dyn ControlBindingResolver>) -> Self {
        self.authority = Some(authority);
        self
    }

    /// Attach the exclusive control-owner proof.
    pub fn with_ownership(mut self, ownership: Arc<dyn ControlOwnership>) -> Self {
        self.ownership = Some(ownership);
        self
    }

    /// Override the injected clock (deterministic tests).
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    /// Register both authenticated adapters on `router`.
    pub fn register(self, router: &mut CapabilityRouter) {
        router.register_adapter(FactoryReadAdapter {
            service: self.clone(),
        });
        router.register_adapter(FactoryControlAdapter { service: self });
    }

    fn now_ms(&self) -> u64 {
        (self.now)()
    }

    // --- runtime observation -------------------------------------------------

    /// Decode and serve one `factory.read/v1` request (snapshot or lookup).
    fn read(&self, scope: &GrantScope, input: &Value, deadline_unix_ms: u64) -> Result<Value, ContractError> {
        let format = input
            .get("format")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FailureCode::InvalidRecord.failure(FACTORY_READ_CAPABILITY, "request must carry a string `format`")
            })?;
        let family = format.split('/').next().unwrap_or("");
        if family == FACTORY_CONTROL_FAMILY {
            let request: FactoryLookupRequest =
                serde_json::from_value(input.clone()).map_err(|error| {
                    FailureCode::InvalidRecord.failure(
                        FACTORY_READ_CAPABILITY,
                        format!("malformed FactoryControl lookup: {error}"),
                    )
                })?;
            request.validate()?;
            crate::factory_control::to_value(&self.lookup(&request.action_id)?)
        } else {
            let request: FactoryRuntimeRequest =
                serde_json::from_value(input.clone()).map_err(|error| {
                    FailureCode::InvalidRecord.failure(
                        FACTORY_RUNTIME_FORMAT,
                        format!("malformed FactoryRuntime request: {error}"),
                    )
                })?;
            request.validate()?;
            if let Some(error) = self.deadline_error(deadline_unix_ms, &scope.plugin_id) {
                return Err(error);
            }
            let snapshot = self.snapshot(scope, &request)?;
            if let Some(error) = self.deadline_error(deadline_unix_ms, &scope.plugin_id) {
                return Err(error);
            }
            crate::factory_control::to_value(&snapshot)
        }
    }

    /// Compose one bounded runtime snapshot, substituting a bounded
    /// `Unavailable` observation when no source is bound.
    fn snapshot(
        &self,
        scope: &GrantScope,
        request: &FactoryRuntimeRequest,
    ) -> Result<FactoryRuntime, ContractError> {
        let mut runtime = match &self.runtime {
            Some(source) => {
                let runtime = source.snapshot(request)?;
                runtime.version_gate()?;
                runtime.validate()?;
                if runtime.repository_id != self.repository_id || runtime.state_id != self.state_id {
                    return Err(FailureCode::InvalidRecord.failure(
                        &scope.plugin_id,
                        "runtime source returned a snapshot for a different repository/state scope",
                    ));
                }
                runtime
            }
            None => self.unavailable_runtime("runtime observation source is not bound"),
        };
        // WEB.11 is the sole trusted permission composer: a raw source flag is
        // overwritten from the trusted composition, never trusted as authority.
        runtime.permissions = self.compose_permissions(scope, &runtime);
        // When a live stop observation is bound it is authoritative over the
        // source's `stop` field, so a missing/unreadable stop authority can
        // never surface as a green Running indication.
        if self.stop.is_some() {
            let observation = self.stop_observation();
            let (state, detail) = match &observation {
                Some(_) => (SourceState::Available, None),
                None => (
                    SourceState::Unreadable,
                    Some("stop authority is unavailable".to_string()),
                ),
            };
            let observed_at = Some(exact_u64(self.now_ms()));
            for source in runtime.sources.iter_mut() {
                if source.source == FactorySourceName::Stop {
                    source.state = state;
                    source.detail = detail.clone();
                    source.observed_at_unix_ms = observed_at.clone();
                }
            }
            runtime.stop = observation;
        }
        runtime.validate()?;
        Ok(runtime)
    }

    /// The bounded `Unavailable` snapshot used when no runtime source is bound.
    fn unavailable_runtime(&self, reason: &str) -> FactoryRuntime {
        FactoryRuntime {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            repository_id: self.repository_id.clone(),
            state_id: self.state_id.clone(),
            runtime_instance_id: None,
            observed_at_unix_ms: exact_u64(self.now_ms()),
            status: FactoryRuntimeStatus::Unavailable,
            reason: Some(reason.to_string()),
            sources: FactorySourceName::ALL
                .iter()
                .map(|name| FactoryRuntimeSource {
                    source: *name,
                    state: SourceState::Missing,
                    detail: Some(reason.to_string()),
                    observed_at_unix_ms: None,
                })
                .collect(),
            stop: None,
            current_execution: None,
            eligible: FactoryEligiblePage {
                entries: Vec::new(),
                revision: None,
                total: None,
                next_cursor: None,
            },
            permissions: FactoryRuntimePermissions::default(),
        }
    }

    /// Compose the trusted permission flags for `scope`.
    ///
    /// Both flags default `false`. Control is enabled only from a healthy source
    /// observation, the caller's live approved control binding, and the
    /// exclusive owner guard; `pause` requires an observed running state and
    /// `resume` an observed stopped state. An unknown stop state disables both.
    fn compose_permissions(
        &self,
        scope: &GrantScope,
        runtime: &FactoryRuntime,
    ) -> FactoryRuntimePermissions {
        let disabled = FactoryRuntimePermissions::default();
        if !self.control_allowed() || !self.control_binding_allows(scope) {
            return disabled;
        }
        if matches!(
            runtime.status,
            FactoryRuntimeStatus::Unavailable | FactoryRuntimeStatus::Failed
        ) {
            return disabled;
        }
        let healthy = runtime
            .sources
            .iter()
            .filter(|source| {
                matches!(
                    source.source,
                    FactorySourceName::Runtime | FactorySourceName::Worker
                )
            })
            .all(|source| matches!(source.state, SourceState::Available | SourceState::Empty));
        if !healthy {
            return disabled;
        }
        match self.stop_state() {
            Some(false) => FactoryRuntimePermissions {
                pause: true,
                resume: false,
            },
            Some(true) => FactoryRuntimePermissions {
                pause: false,
                resume: true,
            },
            None => disabled,
        }
    }

    // --- control -------------------------------------------------------------

    /// Apply one authenticated `factory.control/v1` pause/resume action.
    fn control(
        &self,
        scope: &GrantScope,
        request: FactoryActionRequest,
        deadline_unix_ms: u64,
    ) -> Result<FactoryControlResponse, ContractError> {
        request.validate()?;
        let action_id = request.action_id.clone();
        let digest = request.request_digest()?;

        // Serialize at the exclusive owner. The process mutex is the local
        // serialization; the injected ownership guard proves cross-process
        // exclusivity. Authority/deadline/stop-availability are rechecked after
        // the wait.
        let _serialized = self
            .control_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        if !self.control_allowed() || !self.control_binding_allows(scope) {
            return Err(FailureCode::AuthorityDenied.failure(
                &action_id,
                "factory.control is not authorized for this caller (owner or live binding)",
            ));
        }

        // Read, shape-check and cross-check the durable records first so a
        // malformed/conflicting pair is refused before any replay decision.
        let durable = self.read_durable(&action_id)?;
        if let Some((record, persisted)) = &durable.result {
            if record.request_digest != digest {
                return Err(FailureCode::IdentityConflict.failure(
                    &action_id,
                    "action id replayed with different submitted input",
                ));
            }
            return self.receipt_response(record, persisted);
        }
        if let Some((record, _persisted)) = &durable.intent {
            if record.request_digest != digest {
                return Err(FailureCode::IdentityConflict.failure(
                    &action_id,
                    "action id replayed with different submitted input",
                ));
            }
            // An orphan durable intent is never reexecuted.
            return self.unconfirmed_response(
                &request,
                &digest,
                "durable intent recorded but no terminal result; effect outcome unconfirmed",
            );
        }

        // No prior durable record: this is a NEW effect. A deadline that has
        // already passed refuses it without persisting anything.
        if let Some(error) = self.deadline_error(deadline_unix_ms, &action_id) {
            return Err(error);
        }
        // Fail closed when stop authority is not readable: a new durable
        // intent/effect must not be admitted while the resulting stop state
        // cannot be observed. UI permission flags are never sufficient
        // authority (they are hints, not admission).
        self.require_known_stop(&action_id)?;

        // Persist intent before the effect; an uncertain intent write never
        // authorizes the effect.
        let intent_record = FactoryControlRecord {
            stage: FactoryControlStage::Intent,
            request: request.clone(),
            request_digest: digest.clone(),
            outcome: None,
            reason: None,
        };
        self.persist(
            &intent_ledger_id(&action_id),
            request.operation.as_str(),
            &intent_record,
        )?;

        // Perform the checked effect only after intent is durable. An effect
        // that cannot be confirmed leaves the intent unresolved and reports
        // uncertainty; it is never promoted to a terminal receipt.
        let control_request = self.control_request(&request);
        let outcome = {
            let mut ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
            match apply_checked_latch(&mut **ledger, &self.state_dir, &control_request) {
                Ok(outcome) => outcome,
                Err(_) => {
                    return self.unconfirmed_response(
                        &request,
                        &digest,
                        "effect outcome could not be confirmed; durable intent retained",
                    )
                }
            }
        };
        let (outcome_tag, reason) = match &outcome {
            crate::control::ControlOutcome::Applied => (FactoryControlOutcome::Applied, None),
            crate::control::ControlOutcome::Denied { reason } => {
                (FactoryControlOutcome::Denied, Some(reason.clone()))
            }
        };
        let result_record = FactoryControlRecord {
            stage: FactoryControlStage::Result,
            request: request.clone(),
            request_digest: digest.clone(),
            outcome: Some(outcome_tag),
            reason: reason.clone(),
        };
        let persisted = match self.persist(
            &result_ledger_id(&action_id),
            request.operation.as_str(),
            &result_record,
        ) {
            Ok(persisted) => persisted,
            // A result write failure leaves the intent unresolved and reports
            // uncertainty, never a terminal receipt.
            Err(_) => {
                return self.unconfirmed_response(
                    &request,
                    &digest,
                    "terminal result could not be persisted; effect outcome unconfirmed",
                )
            }
        };
        let stored = self.parse_record(&persisted, FactoryControlStage::Result)?;
        self.receipt_response(&stored, &persisted)
    }

    /// Read-only receipt lookup: exact completed result, orphan intent, or
    /// authoritative `not_found`. Never initializes or mutates state.
    fn lookup(&self, action_id: &str) -> Result<FactoryControlResponse, ContractError> {
        let durable = self.read_durable(action_id)?;
        if let Some((record, persisted)) = &durable.result {
            return self.receipt_response(record, persisted);
        }
        if let Some((record, _persisted)) = &durable.intent {
            return self.unconfirmed_response(
                &record.request,
                &record.request_digest,
                "durable intent recorded but no terminal result; effect outcome unconfirmed",
            );
        }
        let response = FactoryControlResponse::NotFound(FactoryControlNotFoundResponse {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            result: FactoryControlResult::NotFound,
            action_id: action_id.to_string(),
            observed_stop: self.stop_observation(),
        });
        response.validate()?;
        Ok(response)
    }

    /// The durable intent and result rows for `action_id`, if present.
    fn read_records(
        &self,
        action_id: &str,
    ) -> Result<(Option<ControlReceipt>, Option<ControlReceipt>), ContractError> {
        let ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        let receipts = ledger.control_receipts()?;
        let intent = receipts
            .iter()
            .find(|receipt| receipt.action_id == intent_ledger_id(action_id))
            .cloned();
        let result = receipts
            .iter()
            .find(|receipt| receipt.action_id == result_ledger_id(action_id))
            .cloned();
        Ok((intent, result))
    }

    /// Read, shape-check and cross-check the durable intent/result pair for
    /// `action_id` before any replay or effect decision.
    ///
    /// Each row must carry the deterministic id for its stage, match its action
    /// name/operation, embed a canonical digest of the embedded request, and
    /// have the intent/result shape for its stage. A present pair must also
    /// agree on request and digest. Malformed or conflicting durable state is
    /// [`FailureCode::StateUnavailable`]; a result is never trusted ahead of a
    /// conflicting intent.
    fn read_durable(&self, action_id: &str) -> Result<DurableControl, ContractError> {
        let (intent_row, result_row) = self.read_records(action_id)?;
        let intent = match &intent_row {
            Some(receipt) => Some((
                self.parse_record(receipt, FactoryControlStage::Intent)?,
                receipt.clone(),
            )),
            None => None,
        };
        let result = match &result_row {
            Some(receipt) => Some((
                self.parse_record(receipt, FactoryControlStage::Result)?,
                receipt.clone(),
            )),
            None => None,
        };
        if let (Some((intent, _)), Some((result, _))) = (&intent, &result) {
            if intent.request != result.request || intent.request_digest != result.request_digest {
                return Err(FailureCode::StateUnavailable.failure(
                    action_id,
                    "durable factory-control intent and result disagree; refusing to trust the result",
                ));
            }
        }
        Ok(DurableControl { intent, result })
    }

    /// Persist one durable record, returning the stored row (exact timestamp).
    fn persist(
        &self,
        ledger_id: &str,
        action: &str,
        record: &FactoryControlRecord,
    ) -> Result<ControlReceipt, ContractError> {
        let receipt = ControlReceipt {
            action_id: ledger_id.to_string(),
            action: action.to_string(),
            target: String::new(),
            actor: crate::control::ACTOR_HUMAN.to_string(),
            request: json!({ "factory_control_v1": record }),
            recorded_at_unix_ms: self.now_ms(),
        };
        let mut ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.put_control_receipt(&receipt)?;
        ledger
            .control_receipts()?
            .into_iter()
            .find(|stored| stored.action_id == ledger_id)
            .ok_or_else(|| {
                FailureCode::StateUnavailable.failure(
                    ledger_id,
                    "persisted factory-control record is not readable",
                )
            })
    }

    /// Parse and shape-check one stored `factory_control_v1` record against the
    /// deterministic row id for its expected stage.
    fn parse_record(
        &self,
        receipt: &ControlReceipt,
        expected_stage: FactoryControlStage,
    ) -> Result<FactoryControlRecord, ContractError> {
        // The row id must be the deterministic id for the expected stage; a
        // stage/prefix mismatch is malformed state, not a valid record.
        let suffix = receipt
            .action_id
            .rsplit_once('/')
            .map(|(_, id)| id)
            .unwrap_or_default();
        let expected_id = match expected_stage {
            FactoryControlStage::Intent => intent_ledger_id(suffix),
            FactoryControlStage::Result => result_ledger_id(suffix),
        };
        if receipt.action_id != expected_id {
            return Err(FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (row id is not the deterministic id for its stage)",
            ));
        }
        if !receipt.target.is_empty() || receipt.actor != crate::control::ACTOR_HUMAN {
            return Err(FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (unexpected target or actor)",
            ));
        }
        let detail = receipt
            .request
            .get("factory_control_v1")
            .cloned()
            .ok_or_else(|| {
                FailureCode::StateUnavailable.failure(
                    &receipt.action_id,
                    "malformed factory-control record (missing namespace detail)",
                )
            })?;
        let record: FactoryControlRecord =
            serde_json::from_value(detail).map_err(|error| {
                FailureCode::StateUnavailable.failure(
                    &receipt.action_id,
                    format!("malformed factory-control record: {error}"),
                )
            })?;
        if record.stage != expected_stage {
            return Err(FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (stored stage disagrees with its deterministic row id)",
            ));
        }
        if record.request.action_id != suffix {
            return Err(FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (action id mismatch)",
            ));
        }
        // The outer action name must be the request's operation.
        if receipt.action != record.request.operation {
            return Err(FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (action name disagrees with the embedded request)",
            ));
        }
        // The embedded digest must be the canonical digest of the embedded
        // request; otherwise a mutated record could impersonate another input.
        //
        // This runs over *durable* data, so an embedded request that fails the
        // public input contract (unknown operation, unsupported format, or an
        // over-bound confirmation) is malformed state, never a bad caller
        // input: normalize its `InvalidRecord`/`UnsupportedVersion` detail to
        // `StateUnavailable`.
        let canonical = record.request.request_digest().map_err(|_| {
            FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (embedded request is not a valid action)",
            )
        })?;
        if record.request_digest != canonical {
            return Err(FailureCode::StateUnavailable.failure(
                &receipt.action_id,
                "malformed factory-control record (request digest is not canonical for the embedded request)",
            ));
        }
        // Stage shape: an intent carries no terminal result; a result must.
        match expected_stage {
            FactoryControlStage::Intent => {
                if record.outcome.is_some() || record.reason.is_some() {
                    return Err(FailureCode::StateUnavailable.failure(
                        &receipt.action_id,
                        "malformed factory-control intent (carries terminal outcome/reason)",
                    ));
                }
            }
            FactoryControlStage::Result => {
                if record.outcome.is_none() {
                    return Err(FailureCode::StateUnavailable.failure(
                        &receipt.action_id,
                        "malformed factory-control result (missing outcome)",
                    ));
                }
            }
        }
        Ok(record)
    }

    /// Build the terminal receipt response from a persisted result record.
    fn receipt_response(
        &self,
        record: &FactoryControlRecord,
        persisted: &ControlReceipt,
    ) -> Result<FactoryControlResponse, ContractError> {
        let outcome = record.outcome.ok_or_else(|| {
            FailureCode::StateUnavailable.failure(
                &persisted.action_id,
                "malformed factory-control result (no outcome)",
            )
        })?;
        let response = FactoryControlResponse::Receipt(FactoryControlReceiptResponse {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            result: FactoryControlResult::Receipt,
            receipt: FactoryControlReceiptBody {
                action_id: record.request.action_id.clone(),
                request: record.request.clone(),
                request_digest: record.request_digest.clone(),
                outcome,
                reason: record.reason.clone(),
                ledger_action_id: persisted.action_id.clone(),
                actor: FactoryControlActor::Human,
                recorded_at_unix_ms: exact_u64(persisted.recorded_at_unix_ms),
            },
            observed_stop: self.stop_observation(),
        });
        // A persisted result can carry a terminal reason that no longer passes
        // the public bound. That is malformed durable state, not caller input:
        // normalize any validation failure of the assembled receipt (and its
        // embedded request/reason) to `StateUnavailable`.
        response.validate().map_err(|_| {
            FailureCode::StateUnavailable.failure(
                &persisted.action_id,
                "malformed factory-control result (embedded request or terminal reason is not valid)",
            )
        })?;
        Ok(response)
    }

    /// Build an unconfirmed response for an orphan/unresolved intent.
    fn unconfirmed_response(
        &self,
        request: &FactoryActionRequest,
        digest: &str,
        reason: impl Into<String>,
    ) -> Result<FactoryControlResponse, ContractError> {
        let response = FactoryControlResponse::Unconfirmed(FactoryControlUnconfirmedResponse {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            result: FactoryControlResult::Unconfirmed,
            action_id: request.action_id.clone(),
            request: request.clone(),
            request_digest: digest.to_string(),
            code: FactoryUnconfirmedCode::EffectUnconfirmed,
            reason: reason.into(),
            observed_stop: self.stop_observation(),
        });
        // Every outgoing response is validated before it is returned/encoded.
        response.validate()?;
        Ok(response)
    }

    /// The existing human control request for one public action.
    fn control_request(&self, request: &FactoryActionRequest) -> ControlRequest {
        let action = match request.operation.as_str() {
            "resume" => ControlAction::Resume,
            _ => ControlAction::Pause,
        };
        ControlRequest {
            action_id: request.action_id.clone(),
            action,
            actor: Actor::Human,
            confirmation: request.confirmation.clone(),
        }
    }

    // --- authority / observation --------------------------------------------

    /// Whether this process holds the exclusive control owner.
    fn control_allowed(&self) -> bool {
        self.ownership.as_ref().map(|owner| owner.owned()).unwrap_or(false)
    }

    /// Whether `scope` has a live approved `factory.control/v1` binding.
    fn control_binding_allows(&self, scope: &GrantScope) -> bool {
        let Some(resolver) = &self.authority else {
            return false;
        };
        let Some(binding) = resolver.resolve(&scope.plugin_id) else {
            return false;
        };
        // The resolver-returned binding must name the authenticated caller, the
        // control capability, the pinned generation, and be unexpired.
        binding.plugin_id == scope.plugin_id
            && binding.capability == FACTORY_CONTROL_CAPABILITY
            && binding.generation == scope.binding.generation
            && self.now_ms() < binding.expires_at_unix_ms
    }

    /// Fail closed unless the live stop state is readable.
    ///
    /// A new durable intent/effect must not be admitted while the resulting stop
    /// state is unknown: a missing or `Unavailable` stop control makes both
    /// pause and resume unreadable, and an operator UI flag is never authority.
    fn require_known_stop(&self, subject: &str) -> Result<(), ContractError> {
        match self.stop_state() {
            Some(_) => Ok(()),
            None => Err(FailureCode::StateUnavailable.failure(
                subject,
                "factory-control stop authority is unavailable; refusing a new effect",
            )),
        }
    }

    /// The live stop observation, or `None` when stop authority is unknown.
    fn stop_observation(&self) -> Option<FactoryStopObservation> {
        let stop = self.stop.as_ref()?;
        let observed_at = exact_u64(self.now_ms());
        match stop.decision() {
            StopDecision::Running => Some(FactoryStopObservation {
                stopped: false,
                reason: None,
                observed_at_unix_ms: observed_at,
            }),
            StopDecision::Stopped(reason) => Some(FactoryStopObservation {
                stopped: true,
                reason: Some(reason),
                observed_at_unix_ms: observed_at,
            }),
            StopDecision::Unavailable(_) => None,
        }
    }

    /// The observable stopped state, or `None` when it is unknown.
    fn stop_state(&self) -> Option<bool> {
        let stop = self.stop.as_ref()?;
        match stop.decision() {
            StopDecision::Running => Some(false),
            StopDecision::Stopped(_) => Some(true),
            StopDecision::Unavailable(_) => None,
        }
    }

    /// The deadline refusal for an expired bound (`None` when unbounded/open).
    fn deadline_error(&self, deadline_unix_ms: u64, subject: &str) -> Option<ContractError> {
        if deadline_unix_ms == u64::MAX || self.now_ms() < deadline_unix_ms {
            return None;
        }
        Some(FailureCode::DeadlineExceeded.failure(
            subject,
            format!(
                "factory-control deadline {deadline_unix_ms} passed at {}",
                self.now_ms()
            ),
        ))
    }

    /// Reject a caller whose grant is not this capability/method.
    fn check_scope(
        &self,
        scope: &GrantScope,
        capability: &str,
    ) -> Result<(), ContractError> {
        if scope.resource_namespace != capability {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{capability}`",
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
        Ok(())
    }
}

/// The `factory.read/v1` adapter (snapshot + receipt lookup).
struct FactoryReadAdapter {
    service: FactoryControlService,
}

impl CapabilityAdapter for FactoryReadAdapter {
    fn capability(&self) -> &str {
        FACTORY_READ_CAPABILITY
    }

    fn invoke(&mut self, scope: &GrantScope, input: &Value) -> Result<Value, ContractError> {
        self.service.check_scope(scope, FACTORY_READ_CAPABILITY)?;
        self.service.read(scope, input, u64::MAX)
    }

    fn invoke_with_deadline(
        &mut self,
        scope: &GrantScope,
        input: &Value,
        deadline_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        self.service.check_scope(scope, FACTORY_READ_CAPABILITY)?;
        self.service.read(scope, input, deadline_unix_ms)
    }
}

/// The `factory.control/v1` adapter (pause/resume only).
struct FactoryControlAdapter {
    service: FactoryControlService,
}

impl CapabilityAdapter for FactoryControlAdapter {
    fn capability(&self) -> &str {
        FACTORY_CONTROL_CAPABILITY
    }

    fn invoke(&mut self, scope: &GrantScope, input: &Value) -> Result<Value, ContractError> {
        self.invoke_with_deadline(scope, input, u64::MAX)
    }

    fn invoke_with_deadline(
        &mut self,
        scope: &GrantScope,
        input: &Value,
        deadline_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        self.service.check_scope(scope, FACTORY_CONTROL_CAPABILITY)?;
        let request = FactoryControlRequest::from_value(input.clone())?;
        let FactoryControlRequest::Action(action) = request else {
            return Err(ContractError::UnsupportedCapability(format!(
                "{FACTORY_CONTROL_CAPABILITY} supports pause/resume only"
            )));
        };
        let response = self.service.control(scope, action, deadline_unix_ms)?;
        to_value(&response)
    }
}

/// Project a factory contract value into the response JSON.
fn to_value<T: Serialize>(value: &T) -> Result<Value, ContractError> {
    serde_json::to_value(value).map_err(|error| {
        FailureCode::InvalidRecord.failure(FACTORY_CONTROL_FORMAT, format!("cannot encode response: {error}"))
    })
}

/// Build a typed `EffectUnconfirmed` control response for a router timeout that
/// may have occurred after effect admission (`agalma-rwn.11`).
///
/// Once a router hands an effectful factory control to the adapter it cannot
/// prove the effect did not begin, so a deadline expiry must not be reported as
/// a cancellation: the caller receives a typed unconfirmed variant bound to the
/// exact original action id/request/digest and must look the action up. The live
/// stop observation is not available at the transport boundary; `observed_stop`
/// stays `None` (unknown), never a fabricated running indication.
pub fn timeout_unconfirmed(input: &Value, reason: &str) -> Result<Value, ContractError> {
    let request: FactoryActionRequest = serde_json::from_value(input.clone()).map_err(|error| {
        FailureCode::InvalidRecord.failure(
            FACTORY_CONTROL_FORMAT,
            format!("cannot bind an unconfirmed timeout to a malformed action: {error}"),
        )
    })?;
    request.validate()?;
    let digest = request.request_digest()?;
    let response = FactoryControlResponse::Unconfirmed(FactoryControlUnconfirmedResponse {
        format: FACTORY_CONTROL_FORMAT.to_string(),
        result: FactoryControlResult::Unconfirmed,
        action_id: request.action_id.clone(),
        request,
        request_digest: digest,
        code: FactoryUnconfirmedCode::EffectUnconfirmed,
        reason: reason.to_string(),
        observed_stop: None,
    });
    response.validate()?;
    to_value(&response)
}

/// Wrap a `u64` as an [`ExactU64`] decimal string.
fn exact_u64(value: u64) -> ExactU64 {
    ExactU64::parse(&value.to_string()).expect("a u64 always renders as an exact decimal string")
}

/// The current wall clock in Unix milliseconds.
fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
