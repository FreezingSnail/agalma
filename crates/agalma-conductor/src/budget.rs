//! Conductor-side request admission and durable reservation service
//! (`agalma-5fy.7`).
//!
//! [`BudgetGate`] is the parent-owned admission controller the paid proxy
//! consults **before** it contacts upstream. It pins one [`AdmissionScope`]
//! (package, execution, role) so a worker cannot change its own attribution,
//! inspects the request against a reviewed [`ProviderRequestProfile`], and
//! reserves the reviewed maximum.
//!
//! ## Durable before upstream (drift repair)
//! A reservation is only returned as [`GateAdmission::Billable`] **after** it
//! has been committed through the bound [`LedgerApi`] port
//! ([`ReservationLedger`]). A commit failure leaves the in-memory state
//! unchanged and returns `Err`, so the proxy refuses the request and never
//! contacts upstream. There is no drain/checkpoint: every mutation commits
//! first, then applies, so a crash between commit and apply replays to the
//! durable state (never loses an accepted reservation).
//!
//! ## One shared atomic admission operation
//! Every role/attempt of an execution shares a **single** [`ExecutionBudget`]
//! (`Arc<ExecutionBudget>`). Admission/settlement mutate that cell under one
//! mutex, so concurrent roles cannot each admit against a stale snapshot:
//! `spent + outstanding + proposed <= allowance` is evaluated atomically and
//! the durable commitment happens in the same critical section.
//!
//! ## Bound ledger only
//! This module never opens a database. Persistence is the bound
//! [`LedgerApi`] via [`ReservationLedger`]; the durable record is an additive
//! `budget.reservation` `execution_events` row, rebuilt on restart by
//! [`ExecutionBudget::open`].
//!
//! ## Semantics
//! * Reserve a reviewed upper bound before each paid request; enforce the
//!   reviewed request limits (path kind, declared output, billable context).
//! * Unknown allowance/price, an expired profile, an unknown quota cap, or a
//!   missing/over-limit declared output **deny**.
//! * Partial/missing usage retains the full liability and suppresses further
//!   dispatch in the execution until reconciled; duplicate observations never
//!   double count; cancellation cannot refund incurred charges.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::budget::{
    BudgetError, Nusd, PathClass, ProviderRequestProfile, Reservation, ReservationState,
    SpendState, UsageObservation,
};
use agalma_contracts::{CommitBatch, ContractError, ExecutionEvent, ExecutionId, LedgerApi, Role};
use serde_json::{json, Value};

/// The `execution_events.kind` of one durable reservation state change.
pub const BUDGET_EVENT_KIND: &str = "budget.reservation";

/// Milliseconds since the Unix epoch (0 on a pre-epoch clock).
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stable short role key for request ids and attribution.
pub fn role_key(role: Role) -> &'static str {
    match role {
        Role::Builder => "builder",
        Role::Verifier => "verifier",
        Role::Planner => "planner",
        Role::Reviewer => "reviewer",
        Role::Evaluator => "evaluator",
    }
}

/// Map a session/phase role label to its budget [`Role`]. Unknown labels are
/// denied by the caller (never silently attributed).
pub fn role_from_label(label: &str) -> Option<Role> {
    match label.trim().to_ascii_lowercase().as_str() {
        "builder" => Some(Role::Builder),
        "verifier" | "verify" => Some(Role::Verifier),
        "designer" | "concierge" | "planner" | "plan" => Some(Role::Planner),
        "reviewer" | "review" => Some(Role::Reviewer),
        "evaluator" | "evaluate" => Some(Role::Evaluator),
        _ => None,
    }
}

/// Pinned attribution for one paid proxy instance. The parent proxy pins this;
/// a worker cannot change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionScope {
    pub execution_id: String,
    /// The frozen package digest the spend is attributed to.
    pub package_digest: String,
    pub role: Role,
}

impl AdmissionScope {
    pub fn new(
        execution_id: impl Into<String>,
        package_digest: impl Into<String>,
        role: Role,
    ) -> Self {
        AdmissionScope {
            execution_id: execution_id.into(),
            package_digest: package_digest.into(),
            role,
        }
    }
}

/// One admitted paid request: the reservation id and its reviewed liability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateTicket {
    pub request_id: String,
    pub liability_nusd: Nusd,
}

/// The gate's verdict for one inspected request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateAdmission {
    /// A billable inference request; a durable reservation is now held.
    Billable(GateTicket),
    /// Explicitly separated non-billable metadata traffic.
    NonBillable,
}

/// The parent proxy's admission hook. Implementations are `Send + Sync` because
/// the proxy runs the request on a worker thread.
pub trait PaidRequestGate: Send + Sync {
    /// Inspect `(method, path, body)` and, if admissible, durably reserve before
    /// returning `Billable`. `Err(reason)` denies the request; the proxy must
    /// not contact upstream.
    fn admit(&self, method: &str, path: &str, body: &[u8]) -> Result<GateAdmission, String>;
    /// Record the single upstream send durably (before/at the send).
    fn mark_sent(&self, ticket: &GateTicket);
    /// Settle from authoritative usage; `usage.is_complete() == false` retains
    /// the full liability.
    fn settle_usage(&self, ticket: &GateTicket, usage: &UsageObservation);
    /// Release a not-yet-sent reservation (e.g. upstream connect failed).
    fn release(&self, ticket: &GateTicket);
}

/// A durable, `Send` reservation port over the bound ledger API. Implemented
/// for any `Send` [`LedgerApi`] (a bound in-process ledger or a bound IPC
/// client); it never opens a database itself.
pub trait ReservationLedger: Send {
    /// All durable reservation records for `execution`, in event order.
    fn load_records(&mut self, execution: &ExecutionId) -> Result<Vec<Reservation>, ContractError>;
    /// Commit one reservation record durably. Repo rule: additive events; a
    /// retried identical record is harmless because replay is monotone.
    fn commit_record(
        &mut self,
        execution: &ExecutionId,
        record: &Reservation,
    ) -> Result<(), ContractError>;
}

impl<T: LedgerApi + Send> ReservationLedger for T {
    fn load_records(&mut self, execution: &ExecutionId) -> Result<Vec<Reservation>, ContractError> {
        let mut records = Vec::new();
        for event in self.events(execution)? {
            if event.kind != BUDGET_EVENT_KIND {
                continue;
            }
            let Some(value) = event.payload.get("reservation").cloned() else {
                continue;
            };
            records.push(serde_json::from_value(value).map_err(|e| {
                ContractError::KnownFailure(format!("budget reservation decode: {e}"))
            })?);
        }
        Ok(records)
    }

    fn commit_record(
        &mut self,
        execution: &ExecutionId,
        record: &Reservation,
    ) -> Result<(), ContractError> {
        self.commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![reservation_event(execution, record)],
            receipts: vec![],
            intents: vec![],
        })
        .map(|_| ())
    }
}

/// Build a `Send` reservation port from the **bound ledger** transport
/// (`agalma-5fy.7`). The process transport yields a bound IPC client to the
/// single supervised ledger server (the same durable authority, not a second
/// database). The in-process transport has no `Send` bound handle, so it is
/// refused: paid dispatch then fails closed rather than persisting through a
/// private connection.
pub fn bound_reservation_ledger(
    state_dir: &Path,
) -> Result<Box<dyn ReservationLedger>, ContractError> {
    match crate::ledger_process::LedgerTransport::from_env() {
        crate::ledger_process::LedgerTransport::Process => {
            let client = agalma_ipc::LedgerClient::connect_wire(
                &crate::ledger_process::ledger_socket(state_dir),
                state_dir,
                crate::ledger_process::ledger_wire(),
            )
            .map_err(|e| ContractError::KnownFailure(format!("budget bound-ledger client: {e}")))?;
            Ok(Box::new(client))
        }
        crate::ledger_process::LedgerTransport::InProcess => Err(ContractError::KnownFailure(
            "paid budget admission requires the bound ledger over the process transport; \
             the in-process transport cannot provide a Send bound ledger API"
                .to_string(),
        )),
    }
}

/// The one shared admission cell for an execution, shared by every role's gate.
///
/// `Send + Sync`: it holds the durable reservation port and the in-memory view
/// behind one mutex, so an admission is a single atomic operation across
/// roles/attempts **and** the durable commit happens before the caller is told
/// the request is admitted.
pub struct ExecutionBudget {
    inner: Mutex<Inner>,
}

struct Inner {
    spend: SpendState,
    ledger: Box<dyn ReservationLedger>,
    execution: ExecutionId,
    /// Durable, restart-safe request-id sequence: seeded from the number of
    /// distinct reservations rebuilt from the ledger, so a new admission after a
    /// restart can never reuse an id already committed.
    next_seq: u64,
}

impl ExecutionBudget {
    /// Open a budget for `execution`, rebuilding the reservation view from the
    /// bound ledger and holding `ledger` as the durable write port.
    pub fn open(
        mut ledger: Box<dyn ReservationLedger>,
        execution: ExecutionId,
        allowance_nusd: Option<Nusd>,
        quota_cap_units: Option<u64>,
    ) -> Result<Arc<Self>, ContractError> {
        let mut spend = SpendState {
            allowance_nusd,
            quota_cap_units,
            ..Default::default()
        };
        for record in ledger.load_records(&execution)? {
            spend.apply(record);
        }
        // Restart-safe request identity (`agalma-c31`): seed the sequence from
        // the **highest** durable request number, never from the reservation
        // count. A count can shrink (a cancelled reservation is removed from the
        // in-memory view) and can leave gaps, so a restart could otherwise
        // reissue a request id that already exists durably. Reusing an id makes
        // [`SpendState::reserve`] return the existing reservation as an idempotent
        // success, which the gate would report `Billable` — i.e. a blind resend.
        // Deriving from the durable maximum proves a fresh request can never
        // collide with an earlier reservation.
        let next_seq = next_request_seq(&spend);
        Ok(Arc::new(ExecutionBudget {
            inner: Mutex::new(Inner {
                spend,
                ledger,
                execution,
                next_seq,
            }),
        }))
    }

    /// The current reservation view (diagnostics/tests).
    pub fn snapshot(&self) -> SpendState {
        self.lock().spend.clone()
    }

    /// Whether unresolved (unknown) usage suppresses further dispatch.
    pub fn has_unknown_liability(&self) -> bool {
        self.lock().spend.has_unknown_liability()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("execution budget poisoned")
    }

    /// Reserve `reservation` atomically and durably. The durable request id is
    /// assigned here from the restart-safe sequence (never from a process-local
    /// counter). On success the durable record exists before this returns; on a
    /// commit failure nothing changes and the error propagates.
    fn reserve(&self, mut reservation: Reservation) -> Result<Reservation, BudgetError> {
        let mut inner = self.lock();
        let seq = inner.next_seq;
        reservation.request_id = format!(
            "budget:{}:{}:{}",
            inner.execution.as_str(),
            role_key(reservation.role),
            seq
        );
        // A fresh admission must never reuse a durable id: reusing one would
        // make `reserve` return the earlier reservation as an idempotent
        // success and the proxy would resend a billable request. The seed above
        // makes a collision impossible; this guard fails closed if durable state
        // ever surprises that invariant.
        if inner.spend.reservations.contains_key(&reservation.request_id) {
            return Err(BudgetError::Conflict(reservation.request_id));
        }
        let mut candidate = inner.spend.clone();
        let kept = candidate.reserve(reservation)?;
        commit(&mut inner, &kept)?;
        inner.spend = candidate;
        inner.next_seq = seq + 1;
        Ok(kept)
    }

    fn mark_sent(&self, request_id: &str) -> Result<(), BudgetError> {
        let mut inner = self.lock();
        let mut candidate = inner.spend.clone();
        candidate.mark_sent(request_id)?;
        let record = candidate
            .reservations
            .get(request_id)
            .cloned()
            .ok_or_else(|| BudgetError::UnknownRequest(request_id.to_string()))?;
        commit(&mut inner, &record)?;
        inner.spend = candidate;
        Ok(())
    }

    fn settle(&self, request_id: &str, charged_nusd: Option<Nusd>) -> Result<(), BudgetError> {
        let mut inner = self.lock();
        let mut candidate = inner.spend.clone();
        candidate.settle(request_id, charged_nusd)?;
        let record = candidate
            .reservations
            .get(request_id)
            .cloned()
            .ok_or_else(|| BudgetError::UnknownRequest(request_id.to_string()))?;
        commit(&mut inner, &record)?;
        inner.spend = candidate;
        Ok(())
    }

    fn cancel(&self, request_id: &str) -> Result<(), BudgetError> {
        let mut inner = self.lock();
        let mut candidate = inner.spend.clone();
        candidate.cancel(request_id)?;
        // A released (never-sent) reservation needs a durable tombstone so replay
        // cannot resurrect the earlier `Reserved` record.
        let record = candidate
            .reservations
            .get(request_id)
            .cloned()
            .unwrap_or_else(|| tombstone(inner.execution.as_str(), request_id));
        commit(&mut inner, &record)?;
        inner.spend = candidate;
        Ok(())
    }
}

/// Commit `record` through the bound port while holding the execution lock.
fn commit(inner: &mut Inner, record: &Reservation) -> Result<(), BudgetError> {
    inner
        .ledger
        .commit_record(&inner.execution.clone(), record)
        .map_err(|e| BudgetError::Persist(e.to_string()))
}

/// The next durable request number for `spend`: one past the highest
/// `:<seq>` suffix across every reservation id, or `0` when none carry a
/// numeric suffix. Reading the durable maximum (not the reservation count) is
/// what makes a fresh request id restart-safe.
fn next_request_seq(spend: &SpendState) -> u64 {
    let highest = spend
        .reservations
        .keys()
        .filter_map(|id| id.rsplit(':').next())
        .filter_map(|tail| tail.parse::<u64>().ok())
        .max();
    match highest {
        Some(max) => max.saturating_add(1),
        // No parseable ids yet (e.g. a fresh execution, or hand-built test ids):
        // fall back to the reservation count so the first admission starts at a
        // value no smaller than the number already present.
        None => spend.reservations.len() as u64,
    }
}

/// A terminal record that supersedes a released reservation on replay.
fn tombstone(execution_id: &str, request_id: &str) -> Reservation {
    Reservation {
        request_id: request_id.to_string(),
        execution_id: execution_id.to_string(),
        package_digest: String::new(),
        role: Role::Builder,
        profile_digest: String::new(),
        max_liability_nusd: 0,
        quota_units: 0,
        state: ReservationState::Settled,
        charged_nusd: Some(0),
        created_at_unix_ms: now_unix_ms(),
    }
}

/// The parent-owned admission gate: pinned scope + reviewed profile + the one
/// shared [`ExecutionBudget`]. `Send + Sync`; share it with the proxy as
/// `Arc<BudgetGate>`.
pub struct BudgetGate {
    scope: AdmissionScope,
    profile: ProviderRequestProfile,
    budget: Arc<ExecutionBudget>,
}

impl BudgetGate {
    /// Build a gate over the shared execution budget. Every role/attempt of one
    /// execution must pass the **same** `Arc<ExecutionBudget>`.
    pub fn new(
        scope: AdmissionScope,
        profile: ProviderRequestProfile,
        budget: Arc<ExecutionBudget>,
    ) -> Self {
        BudgetGate {
            scope,
            profile,
            budget,
        }
    }

    pub fn scope(&self) -> &AdmissionScope {
        &self.scope
    }

    pub fn profile(&self) -> &ProviderRequestProfile {
        &self.profile
    }

    /// The shared execution budget (for diagnostics/checkpoints).
    pub fn budget(&self) -> &Arc<ExecutionBudget> {
        &self.budget
    }

    /// The current shared reservation view (test/diagnostics).
    pub fn snapshot(&self) -> SpendState {
        self.budget.snapshot()
    }

    /// Inspect `(method, path, body)` against the profile and **durably
    /// reserve** before returning `Billable`.
    pub fn inspect(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        now_unix_ms: u64,
    ) -> Result<GateAdmission, BudgetError> {
        if !self.profile.is_valid_at(now_unix_ms) {
            return Err(BudgetError::ProfileExpired(
                self.profile.profile_digest.clone(),
            ));
        }
        match self.profile.classify(method, path) {
            PathClass::Metadata => Ok(GateAdmission::NonBillable),
            PathClass::Unsupported => Err(BudgetError::UnsupportedRequest {
                reason: format!("{method} {path} is not in profile {}", self.profile.model),
            }),
            PathClass::Billable(_kind) => {
                if body.len() as u64 > self.profile.billable_context_bytes {
                    return Err(BudgetError::ContextBoundExceeded {
                        bytes: body.len() as u64,
                        bound: self.profile.billable_context_bytes,
                    });
                }
                let declared = declared_output_limit(body, &self.profile.output_limit_field)?;
                if declared > self.profile.output_limit_tokens {
                    return Err(BudgetError::OutputBoundExceeded {
                        declared,
                        bound: self.profile.output_limit_tokens,
                    });
                }
                let liability_nusd = self.profile.reservation_nusd()?;
                let quota_units = self.profile.quota_units_per_request.unwrap_or(0);
                let reservation = Reservation {
                    request_id: String::new(),
                    execution_id: self.scope.execution_id.clone(),
                    package_digest: self.scope.package_digest.clone(),
                    role: self.scope.role,
                    profile_digest: self.profile.profile_digest.clone(),
                    max_liability_nusd: liability_nusd,
                    quota_units,
                    state: ReservationState::Reserved,
                    charged_nusd: None,
                    created_at_unix_ms: now_unix_ms,
                };
                // Durable commit before the caller is told `Billable`.
                let kept = self.budget.reserve(reservation)?;
                Ok(GateAdmission::Billable(GateTicket {
                    request_id: kept.request_id,
                    liability_nusd: kept.max_liability_nusd,
                }))
            }
        }
    }
}

impl PaidRequestGate for BudgetGate {
    fn admit(&self, method: &str, path: &str, body: &[u8]) -> Result<GateAdmission, String> {
        self.inspect(method, path, body, now_unix_ms())
            .map_err(|e| e.to_string())
    }

    fn mark_sent(&self, ticket: &GateTicket) {
        if let Err(err) = self.budget.mark_sent(&ticket.request_id) {
            eprintln!("agalma: budget mark_sent {}: {err}", ticket.request_id);
        }
    }

    fn settle_usage(&self, ticket: &GateTicket, usage: &UsageObservation) {
        let charged = if usage.is_complete() {
            let prompt = usage.prompt_tokens.unwrap_or(0);
            let completion = usage.completion_tokens.unwrap_or(0);
            Some(
                usage
                    .reported_cash_nusd
                    .unwrap_or_else(|| self.profile.usage_cash_nusd(prompt, completion)),
            )
        } else {
            None
        };
        if let Err(err) = self.budget.settle(&ticket.request_id, charged) {
            eprintln!("agalma: budget settle {}: {err}", ticket.request_id);
        }
    }

    fn release(&self, ticket: &GateTicket) {
        if let Err(err) = self.budget.cancel(&ticket.request_id) {
            eprintln!("agalma: budget release {}: {err}", ticket.request_id);
        }
    }
}

/// Rebuild the shared execution budget for `execution` from the bound ledger
/// port (loads the durable records, then holds the port for future commits).
pub fn load_execution_budget(
    ledger: Box<dyn ReservationLedger>,
    execution: &ExecutionId,
    allowance_nusd: Option<Nusd>,
    quota_cap_units: Option<u64>,
) -> Result<Arc<ExecutionBudget>, ContractError> {
    ExecutionBudget::open(ledger, execution.clone(), allowance_nusd, quota_cap_units)
}

/// Per-role admission gates for **one** execution, all sharing a single
/// [`ExecutionBudget`] (`agalma-c31`).
///
/// The parent proxy instance pins the attribution scope, and each paid session
/// runs through the gate for **its own** role. Sharing one budget keeps
/// `spent + outstanding + proposed <= allowance` atomic across roles, so a
/// verifier request is charged to the verifier and still cannot push the
/// execution past its builder+verifier allowance. This is the production
/// constructor: [`crate::work`] and [`crate::session`] build their paid proxies
/// from it rather than pinning one role for every role's traffic.
pub struct ExecutionGates {
    budget: Arc<ExecutionBudget>,
    gates: Vec<(Role, Arc<BudgetGate>)>,
}

impl ExecutionGates {
    /// Open a restart-rebuilt [`ExecutionBudget`] over `ledger` and build one
    /// pinned [`BudgetGate`] per role in `roles`.
    pub fn open(
        ledger: Box<dyn ReservationLedger>,
        execution: ExecutionId,
        package_digest: &str,
        allowance_nusd: Option<Nusd>,
        quota_cap_units: Option<u64>,
        profile: ProviderRequestProfile,
        roles: &[Role],
    ) -> Result<Arc<Self>, ContractError> {
        let budget = ExecutionBudget::open(ledger, execution.clone(), allowance_nusd, quota_cap_units)?;
        let gates = roles
            .iter()
            .map(|role| {
                let gate = Arc::new(BudgetGate::new(
                    AdmissionScope::new(execution.as_str(), package_digest, *role),
                    profile.clone(),
                    Arc::clone(&budget),
                ));
                (*role, gate)
            })
            .collect();
        Ok(Arc::new(ExecutionGates { budget, gates }))
    }

    /// The shared execution budget (diagnostics/tests).
    pub fn budget(&self) -> &Arc<ExecutionBudget> {
        &self.budget
    }

    /// The pinned gate for `role`, when `role` was included at construction.
    pub fn gate(&self, role: Role) -> Option<&Arc<BudgetGate>> {
        self.gates
            .iter()
            .find(|(candidate, _)| *candidate == role)
            .map(|(_, gate)| gate)
    }
}

/// The durable event for one reservation state change.
pub fn reservation_event(execution: &ExecutionId, reservation: &Reservation) -> ExecutionEvent {
    ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: BUDGET_EVENT_KIND.to_string(),
        payload: json!({ "reservation": reservation }),
        recorded_at_unix_ms: now_unix_ms(),
    }
}

/// Parse an OpenAI-shaped response body for authoritative usage. A non-JSON,
/// chunked, or missing `usage` object yields [`UsageObservation::missing`]
/// (which retains the full liability).
pub fn usage_from_response_body(body: &[u8]) -> UsageObservation {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return UsageObservation::missing();
    };
    let Some(usage) = value.get("usage") else {
        return UsageObservation::missing();
    };
    UsageObservation {
        prompt_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
        completion_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
        reported_cash_nusd: usage.get("cost_nusd").and_then(Value::as_u64),
    }
}

/// Read the declared output limit from the inspected JSON body. A missing or
/// non-integer field denies (unknown bound).
fn declared_output_limit(body: &[u8], field: &str) -> Result<u64, BudgetError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| BudgetError::UnsupportedRequest {
            reason: format!("request body is not JSON: {e}"),
        })?;
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| BudgetError::MissingOutputBound {
            field: field.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::budget::{ProviderRequestProfile, FAKE_PROFILE_SEED};
    use agalma_ledger::SqliteLedger;
    use agalma_testkit::ScratchDir;

    /// A test-only port that records commits in memory and can inject a
    /// persistence failure (the reviewer's commit-failure regression).
    struct FakeLedger {
        records: Vec<Reservation>,
        fail: bool,
        commits: u64,
    }

    impl ReservationLedger for FakeLedger {
        fn load_records(
            &mut self,
            _execution: &ExecutionId,
        ) -> Result<Vec<Reservation>, ContractError> {
            Ok(self.records.clone())
        }
        fn commit_record(
            &mut self,
            _execution: &ExecutionId,
            record: &Reservation,
        ) -> Result<(), ContractError> {
            if self.fail {
                return Err(ContractError::KnownFailure(
                    "injected commit failure".to_string(),
                ));
            }
            self.commits += 1;
            self.records.push(record.clone());
            Ok(())
        }
    }

    fn fake_budget(allowance: Nusd, fail: bool) -> Arc<ExecutionBudget> {
        ExecutionBudget::open(
            Box::new(FakeLedger {
                records: Vec::new(),
                fail,
                commits: 0,
            }),
            ExecutionId::new("exec:t:1"),
            Some(allowance),
            None,
        )
        .unwrap()
    }

    fn gate(allowance: Nusd) -> BudgetGate {
        BudgetGate::new(
            AdmissionScope::new("exec:t:1", "sha256:pkg", Role::Builder),
            ProviderRequestProfile::fake_byte_token(),
            fake_budget(allowance, false),
        )
    }

    fn body(max_tokens: u64) -> Vec<u8> {
        json!({ "model": "fake-byte-token", "max_tokens": max_tokens })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn admits_a_bounded_request_and_reserves_the_reviewed_maximum() {
        let gate = gate(20_000_000);
        let ticket = match gate
            .inspect("POST", "/chat/completions", &body(1024), 1)
            .unwrap()
        {
            GateAdmission::Billable(t) => t,
            other => panic!("expected billable, got {other:?}"),
        };
        assert_eq!(ticket.liability_nusd, 14_336_000);
        assert_eq!(gate.snapshot().outstanding_nusd(), 14_336_000);
    }

    #[test]
    fn over_budget_is_refused_before_any_send() {
        let gate = gate(10_000_000);
        let err = gate
            .inspect("POST", "/chat/completions", &body(1024), 1)
            .expect_err("over budget");
        assert!(matches!(err, BudgetError::OverBudget { .. }));
        assert_eq!(gate.snapshot().outstanding_nusd(), 0, "nothing reserved");
    }

    /// A persistence failure refuses admission: no `Billable`, no applied state.
    #[test]
    fn commit_failure_refuses_admission_and_applies_nothing() {
        let budget = fake_budget(20_000_000, true);
        let gate = BudgetGate::new(
            AdmissionScope::new("exec:t:1", "sha256:pkg", Role::Builder),
            ProviderRequestProfile::fake_byte_token(),
            Arc::clone(&budget),
        );
        let err = gate
            .inspect("POST", "/chat/completions", &body(1024), 1)
            .expect_err("commit failure must refuse");
        assert!(matches!(err, BudgetError::Persist(_)), "{err:?}");
        assert_eq!(
            budget.snapshot().outstanding_nusd(),
            0,
            "a failed commit leaves the in-memory state unchanged"
        );
    }

    #[test]
    fn missing_or_over_limit_output_is_denied() {
        let gate = gate(20_000_000);
        let no_field = json!({ "model": "x" }).to_string().into_bytes();
        assert!(matches!(
            gate.inspect("POST", "/chat/completions", &no_field, 1),
            Err(BudgetError::MissingOutputBound { .. })
        ));
        assert!(matches!(
            gate.inspect("POST", "/chat/completions", &body(2048), 1),
            Err(BudgetError::OutputBoundExceeded { .. })
        ));
    }

    #[test]
    fn metadata_passes_without_reserving_and_unknown_paths_deny() {
        let gate = gate(1);
        assert_eq!(
            gate.inspect("GET", "/models", &[], 1).unwrap(),
            GateAdmission::NonBillable
        );
        assert!(matches!(
            gate.inspect("POST", "/v1/embeddings", &[], 1),
            Err(BudgetError::UnsupportedRequest { .. })
        ));
    }

    #[test]
    fn expired_profile_denies_even_when_funded() {
        let mut profile = ProviderRequestProfile::fake_byte_token();
        profile.valid_until_unix_ms = 5;
        let gate = BudgetGate::new(
            AdmissionScope::new("exec:t:1", "sha256:pkg", Role::Builder),
            profile,
            fake_budget(20_000_000, false),
        );
        assert!(matches!(
            gate.inspect("POST", "/chat/completions", &body(1), 10),
            Err(BudgetError::ProfileExpired(_))
        ));
    }

    /// Two roles sharing **one** [`ExecutionBudget`] cannot jointly exceed the
    /// execution allowance: the second admission sees the first's reservation.
    #[test]
    fn shared_budget_caps_cross_role_admission() {
        let shared = fake_budget(20_000_000, false);
        let builder = BudgetGate::new(
            AdmissionScope::new("exec:multi:1", "sha256:pkg", Role::Builder),
            ProviderRequestProfile::fake_byte_token(),
            Arc::clone(&shared),
        );
        let verifier = BudgetGate::new(
            AdmissionScope::new("exec:multi:1", "sha256:pkg", Role::Verifier),
            ProviderRequestProfile::fake_byte_token(),
            Arc::clone(&shared),
        );
        assert!(builder
            .inspect("POST", "/chat/completions", &body(1024), 1)
            .is_ok());
        assert!(matches!(
            verifier.inspect("POST", "/chat/completions", &body(1024), 1),
            Err(BudgetError::OverBudget { .. })
        ));
        assert_eq!(shared.snapshot().outstanding_nusd(), 14_336_000);
    }

    /// Durable rebuild through the bound [`LedgerApi`]: a committed reservation
    /// is reconstructed by a fresh open (restart).
    #[test]
    fn rebuild_through_bound_ledger_retains_reservation() {
        let dir = ScratchDir::new("budget-bound");
        let execution = ExecutionId::new("exec:t:1");
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open bound ledger");
        let shared =
            ExecutionBudget::open(Box::new(ledger), execution.clone(), Some(20_000_000), None)
                .expect("open budget");
        let gate = BudgetGate::new(
            AdmissionScope::new("exec:t:1", "sha256:pkg", Role::Builder),
            ProviderRequestProfile::fake_byte_token(),
            Arc::clone(&shared),
        );
        let ticket = match gate
            .inspect("POST", "/chat/completions", &body(1024), 1)
            .unwrap()
        {
            GateAdmission::Billable(t) => t,
            other => panic!("expected billable, got {other:?}"),
        };
        gate.mark_sent(&ticket);

        // A fresh execution budget over the same bound ledger (a restarted
        // process) retains the outstanding liability.
        let ledger2 = SqliteLedger::open(dir.join("ledger.sqlite")).expect("reopen bound ledger");
        let reborn = load_execution_budget(Box::new(ledger2), &execution, Some(20_000_000), None)
            .expect("rebuild");
        assert_eq!(reborn.snapshot().outstanding_nusd(), 14_336_000);
    }

    #[test]
    fn role_label_mapping_is_explicit() {
        assert_eq!(role_from_label("builder"), Some(Role::Builder));
        assert_eq!(role_from_label("Reviewer"), Some(Role::Reviewer));
        assert_eq!(role_from_label("designer"), Some(Role::Planner));
        assert_eq!(role_from_label("nonsense"), None);
    }

    #[test]
    fn profile_seed_is_stable() {
        assert_eq!(
            ProviderRequestProfile::fake_byte_token().profile_digest,
            agalma_contracts::digest::sha256_digest(FAKE_PROFILE_SEED.as_bytes())
        );
    }
}
