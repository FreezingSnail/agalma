//! Pure request-budget contract (`agalma-5fy.7`).
//!
//! Budgets were display-only (see `crates/agalma-conductor/src/economics.rs`);
//! this module is the frozen arithmetic and record model the conductor's
//! admission service enforces. It is pure: integer nanoUSD (`Nusd`) cash,
//! a **separate** subscription-quota bucket in the quota policy's native units,
//! and conservative reservations. No I/O, no vendor types, no pricing guesses.
//!
//! Revision-1 semantics (`docs/self-mutation-contracts.md` §"SF: enforceable
//! request budgets"):
//!
//! * For one admitted request, reserve the reviewed maximum billable input and
//!   output — never an estimate of the current prompt:
//!   `ceil((input_limit * max_input_price + output_limit * max_output_price) / 1e6)
//!   + maximum_other_charge`.
//! * `spent + outstanding + proposed <= allowance` is checked atomically (over
//!   every role/attempt of the execution).
//! * Unknown allowance, unknown price, an expired profile, an unknown quota cap
//!   for a quota-governed request, or a missing/over-limit declared output
//!   **deny** admission.
//! * Complete authoritative usage releases the unused reservation; unknown or
//!   partial usage retains the full liability.
//! * Cancellation cannot refund incurred charges: a sent/unconfirmed request
//!   keeps its full liability.
//! * Cash and quota are never added together; a zero cash charge does not mean
//!   unlimited quota or zero compute cost.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::task_package::Role;

/// Integer nanoUSD (1e-9 USD): the canonical cash unit.
pub type Nusd = u64;

/// NanoUSD per USD.
pub const NUSD_PER_USD: u64 = 1_000_000_000;

/// Token prices are quoted per one million tokens.
pub const TOKENS_PER_PRICE_UNIT: u64 = 1_000_000;

/// Seed for the frozen revision-1 fake profile digest (a test profile, **not** a
/// price claim about any live provider).
pub const FAKE_PROFILE_SEED: &str = "provider-profile/fake-byte-token/v1";

/// A billable request kind a reviewed profile may admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// POST chat-completions.
    ChatCompletions,
    /// POST legacy completions.
    Completions,
}

/// One billable `(method, path, kind)` triple in a reviewed profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillablePath {
    pub method: String,
    pub path: String,
    pub kind: RequestKind,
}

/// How the profile classifies one inspected request line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathClass {
    /// A billable inference request: reserve/enforce.
    Billable(RequestKind),
    /// Explicitly separated non-billable metadata traffic.
    Metadata,
    /// Not covered by this profile: deny.
    Unsupported,
}

/// Reviewed provider request profile: the frozen bounds a paid request is
/// reserved and enforced against. `Debug` is fine (no secret material).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequestProfile {
    /// Digest of the reviewed profile (source evidence, validity, limits).
    pub profile_digest: String,
    pub provider: String,
    pub model: String,
    pub billing_revision: String,
    /// Validity interval (`[from, until)`); an expired profile is unusable.
    pub valid_from_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    /// Maximum billable input tokens per request.
    pub input_limit_tokens: u64,
    /// Enforceable output bound (the declared output limit may not exceed it).
    pub output_limit_tokens: u64,
    pub max_input_price_per_million_nusd: u64,
    pub max_output_price_per_million_nusd: u64,
    /// Maximum of every additional billing feature (media/tool/…); reserved too.
    pub maximum_other_charge_nusd: Nusd,
    /// Maximum inspected request-body bytes (billable context bound).
    pub billable_context_bytes: u64,
    /// Exact JSON field carrying the declared output limit (`max_tokens`).
    pub output_limit_field: String,
    pub billable_paths: Vec<BillablePath>,
    /// Explicitly separated non-billable metadata paths.
    pub metadata_paths: Vec<String>,
    /// Native quota units reserved per request in the separate quota bucket.
    /// `None` means the profile reserves no quota.
    pub quota_units_per_request: Option<u64>,
    /// Only `n = 1` is supported; any other value denies admission.
    pub n: u32,
}

impl ProviderRequestProfile {
    /// The frozen revision-1 fake byte-token profile: 4,096 input tokens at
    /// 2,000,000,000 nusd/million, 1,024 output tokens at 6,000,000,000
    /// nusd/million, no other charges. Its exact reservation is 14,336,000 nusd.
    ///
    /// This is a **test** profile (`docs/self-mutation-contracts.md`), not a
    /// price claim about Flash or any live provider.
    pub fn fake_byte_token() -> Self {
        ProviderRequestProfile {
            profile_digest: crate::digest::sha256_digest(FAKE_PROFILE_SEED.as_bytes()),
            provider: "fake".to_string(),
            model: "fake-byte-token".to_string(),
            billing_revision: "fake/v1".to_string(),
            valid_from_unix_ms: 0,
            valid_until_unix_ms: u64::MAX,
            input_limit_tokens: 4_096,
            output_limit_tokens: 1_024,
            max_input_price_per_million_nusd: 2_000_000_000,
            max_output_price_per_million_nusd: 6_000_000_000,
            maximum_other_charge_nusd: 0,
            billable_context_bytes: 1_048_576,
            output_limit_field: "max_tokens".to_string(),
            billable_paths: vec![BillablePath {
                method: "POST".to_string(),
                path: "/chat/completions".to_string(),
                kind: RequestKind::ChatCompletions,
            }],
            metadata_paths: vec!["/models".to_string()],
            quota_units_per_request: None,
            n: 1,
        }
    }

    /// Whether the profile is usable at `now_unix_ms` (valid interval and the
    /// reviewed `n = 1`).
    pub fn is_valid_at(&self, now_unix_ms: u64) -> bool {
        self.valid_until_unix_ms > self.valid_from_unix_ms
            && now_unix_ms >= self.valid_from_unix_ms
            && now_unix_ms < self.valid_until_unix_ms
    }

    /// The exact reservation for one request against this profile:
    /// `ceil((input_limit * in_price + output_limit * out_price) / 1e6)
    ///  + maximum_other_charge`, all checked (unknown bound/price denies).
    pub fn reservation_nusd(&self) -> Result<Nusd, BudgetError> {
        if self.n != 1 {
            return Err(BudgetError::UnsupportedRequest {
                reason: format!(
                    "profile {} pins n = {} (only n = 1 is supported)",
                    self.model, self.n
                ),
            });
        }
        let input =
            (self.input_limit_tokens as u128) * (self.max_input_price_per_million_nusd as u128);
        let output =
            (self.output_limit_tokens as u128) * (self.max_output_price_per_million_nusd as u128);
        let sum = input.checked_add(output).ok_or(BudgetError::UnknownPrice)?;
        let per_unit = TOKENS_PER_PRICE_UNIT as u128;
        // Checked ceiling division: `(sum + per_unit - 1) / per_unit`.
        let ceil = sum
            .checked_add(per_unit - 1)
            .ok_or(BudgetError::UnknownPrice)?
            / per_unit;
        let nusd = u64::try_from(ceil).map_err(|_| BudgetError::UnknownPrice)?;
        nusd.checked_add(self.maximum_other_charge_nusd)
            .ok_or(BudgetError::UnknownPrice)
    }

    /// Billable cash for a complete authoritative observation, rounded up so the
    /// recorded charge never understates the provider's billable maximum.
    pub fn usage_cash_nusd(&self, prompt_tokens: u64, completion_tokens: u64) -> Nusd {
        let input = (prompt_tokens as u128) * (self.max_input_price_per_million_nusd as u128);
        let output = (completion_tokens as u128) * (self.max_output_price_per_million_nusd as u128);
        let sum = input
            .saturating_add(output)
            .saturating_add(TOKENS_PER_PRICE_UNIT as u128 - 1);
        let nusd = (sum / TOKENS_PER_PRICE_UNIT as u128).min(u128::from(u64::MAX));
        nusd as u64
    }

    /// Classify `(method, path)` against the profile.
    pub fn classify(&self, method: &str, path: &str) -> PathClass {
        let method_ok = |m: &str| m.eq_ignore_ascii_case(method.trim());
        if self.metadata_paths.iter().any(|p| p == path) {
            return PathClass::Metadata;
        }
        for entry in &self.billable_paths {
            if method_ok(&entry.method) && entry.path == path {
                return PathClass::Billable(entry.kind);
            }
        }
        PathClass::Unsupported
    }
}

/// One authoritative usage observation from the provider/harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageObservation {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reported_cash_nusd: Option<Nusd>,
}

impl UsageObservation {
    /// A wholly missing observation: retains full liability.
    pub const fn missing() -> Self {
        UsageObservation {
            prompt_tokens: None,
            completion_tokens: None,
            reported_cash_nusd: None,
        }
    }

    /// Complete only when **both** token counts are present and authoritative.
    pub fn is_complete(&self) -> bool {
        self.prompt_tokens.is_some() && self.completion_tokens.is_some()
    }
}

/// Lifecycle of one reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    /// Reserved, not yet sent upstream.
    Reserved,
    /// Sent once; awaiting usage.
    Sent,
    /// Settled with complete authoritative usage.
    Settled,
    /// Sent/unknown usage: full liability retained until reconciled.
    Unconfirmed,
}

impl ReservationState {
    /// Monotone rank, so replaying records is order-independent.
    pub fn rank(self) -> u8 {
        match self {
            ReservationState::Reserved => 0,
            ReservationState::Sent => 1,
            ReservationState::Unconfirmed => 2,
            ReservationState::Settled => 3,
        }
    }
}

/// One durable reservation: the reviewed maximum liability for one request,
/// attributed to package/execution/role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub request_id: String,
    pub execution_id: String,
    pub package_digest: String,
    pub role: Role,
    pub profile_digest: String,
    pub max_liability_nusd: Nusd,
    pub quota_units: u64,
    pub state: ReservationState,
    /// Settled cash charge, once authoritative.
    pub charged_nusd: Option<Nusd>,
    pub created_at_unix_ms: u64,
}

impl Reservation {
    /// Cash still outstanding for this reservation (`0` once settled).
    pub fn outstanding_nusd(&self) -> Nusd {
        match self.state {
            ReservationState::Settled => 0,
            _ => self.max_liability_nusd,
        }
    }

    /// Whether this reservation still holds quota.
    pub fn holds_quota(&self) -> bool {
        !matches!(self.state, ReservationState::Settled)
    }
}

/// The complete admission view for one execution: allowance, the separate quota
/// cap, and every reservation keyed by request id. Pure and cloneable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendState {
    pub allowance_nusd: Option<Nusd>,
    /// Separate subscription-quota cap in the quota policy's native units.
    pub quota_cap_units: Option<u64>,
    pub reservations: BTreeMap<String, Reservation>,
}

impl SpendState {
    /// Total settled cash spend.
    pub fn cash_spent_nusd(&self) -> Nusd {
        self.reservations
            .values()
            .filter(|r| r.state == ReservationState::Settled)
            .filter_map(|r| r.charged_nusd)
            .fold(0u64, |acc, v| acc.saturating_add(v))
    }

    /// Total outstanding conservative liability.
    pub fn outstanding_nusd(&self) -> Nusd {
        self.reservations
            .values()
            .fold(0u64, |acc, r| acc.saturating_add(r.outstanding_nusd()))
    }

    /// Total committed quota units (separate bucket; never added to nanoUSD).
    pub fn quota_committed_units(&self) -> u64 {
        self.reservations
            .values()
            .filter(|r| r.holds_quota())
            .fold(0u64, |acc, r| acc.saturating_add(r.quota_units))
    }

    /// Cash admission: `spent + outstanding + proposed <= allowance`, checked.
    /// An unknown allowance denies.
    pub fn admit_cash(&self, proposed_nusd: Nusd) -> Result<(), BudgetError> {
        let allowance = self.allowance_nusd.ok_or(BudgetError::UnknownAllowance)?;
        let used = self
            .cash_spent_nusd()
            .checked_add(self.outstanding_nusd())
            .ok_or(BudgetError::UnknownPrice)?;
        let total = used
            .checked_add(proposed_nusd)
            .ok_or(BudgetError::UnknownPrice)?;
        if total <= allowance {
            Ok(())
        } else {
            Err(BudgetError::OverBudget {
                allowance_nusd: allowance,
                used_nusd: used,
                proposed_nusd,
            })
        }
    }

    /// Quota admission in the native unit. A quota-governed request (one that
    /// reserves quota units) with an unknown cap denies; a request reserving no
    /// quota is not quota-governed.
    pub fn admit_quota(&self, proposed_units: u64) -> Result<(), BudgetError> {
        if proposed_units == 0 {
            return Ok(());
        }
        match self.quota_cap_units {
            None => Err(BudgetError::UnknownQuota),
            Some(cap) => {
                let used = self
                    .quota_committed_units()
                    .checked_add(proposed_units)
                    .ok_or(BudgetError::UnknownQuota)?;
                if used <= cap {
                    Ok(())
                } else {
                    Err(BudgetError::OverQuota {
                        cap_units: cap,
                        used_units: self.quota_committed_units(),
                        proposed_units,
                    })
                }
            }
        }
    }

    /// Whether any reservation holds unknown (unconfirmed) usage. Unknown usage
    /// suppresses further paid dispatch in this scope until reconciled.
    pub fn has_unknown_liability(&self) -> bool {
        self.reservations
            .values()
            .any(|r| r.state == ReservationState::Unconfirmed)
    }

    /// Reserve `proposed`, admitting cash and quota first. Idempotent by
    /// `request_id`: an identical repeat returns the existing reservation;
    /// changed liability/attribution is a conflict. Unknown outstanding usage
    /// suppresses any new reservation.
    pub fn reserve(&mut self, proposed: Reservation) -> Result<Reservation, BudgetError> {
        if let Some(existing) = self.reservations.get(&proposed.request_id) {
            if existing.max_liability_nusd == proposed.max_liability_nusd
                && existing.quota_units == proposed.quota_units
                && existing.package_digest == proposed.package_digest
                && existing.role == proposed.role
            {
                return Ok(existing.clone());
            }
            return Err(BudgetError::Conflict(proposed.request_id));
        }
        if self.has_unknown_liability() {
            return Err(BudgetError::UnknownUsage);
        }
        self.admit_cash(proposed.max_liability_nusd)?;
        self.admit_quota(proposed.quota_units)?;
        self.reservations
            .insert(proposed.request_id.clone(), proposed.clone());
        Ok(proposed)
    }

    /// Mark a reservation sent once. Idempotent.
    pub fn mark_sent(&mut self, request_id: &str) -> Result<(), BudgetError> {
        let reservation = self
            .reservations
            .get_mut(request_id)
            .ok_or_else(|| BudgetError::UnknownRequest(request_id.to_string()))?;
        if reservation.state == ReservationState::Reserved {
            reservation.state = ReservationState::Sent;
        }
        Ok(())
    }

    /// Settle `request_id`. `Some(charged)` is a complete authoritative
    /// observation (releases the unused reservation); `None` is partial/missing
    /// usage (full liability retained as `Unconfirmed`). Idempotent: once
    /// `Settled`, later observations never double count.
    pub fn settle(
        &mut self,
        request_id: &str,
        charged_nusd: Option<Nusd>,
    ) -> Result<(), BudgetError> {
        let reservation = self
            .reservations
            .get_mut(request_id)
            .ok_or_else(|| BudgetError::UnknownRequest(request_id.to_string()))?;
        match (reservation.state, charged_nusd) {
            // Already settled: first authoritative settlement wins.
            (ReservationState::Settled, _) => {}
            // A later complete observation upgrades an unconfirmed request.
            (_, Some(charged)) => {
                reservation.state = ReservationState::Settled;
                reservation.charged_nusd = Some(charged);
            }
            // Partial/missing usage never becomes zero: retain full liability.
            (_, None) => {
                reservation.state = ReservationState::Unconfirmed;
            }
        }
        Ok(())
    }

    /// Cancel a reservation. A reserved-but-never-sent request incurred no
    /// charge and is released; a sent/unconfirmed request keeps its full
    /// liability (cancellation cannot refund incurred charges).
    pub fn cancel(&mut self, request_id: &str) -> Result<(), BudgetError> {
        let state = self
            .reservations
            .get(request_id)
            .map(|r| r.state)
            .ok_or_else(|| BudgetError::UnknownRequest(request_id.to_string()))?;
        match state {
            ReservationState::Reserved => {
                self.reservations.remove(request_id);
            }
            ReservationState::Settled => {}
            ReservationState::Sent | ReservationState::Unconfirmed => {
                if let Some(reservation) = self.reservations.get_mut(request_id) {
                    reservation.state = ReservationState::Unconfirmed;
                }
            }
        }
        Ok(())
    }

    /// Apply one durable reservation record during replay. Monotone by state
    /// rank, so replaying records in any order converges to the same state.
    pub fn apply(&mut self, record: Reservation) {
        let keep = self
            .reservations
            .get(&record.request_id)
            .map(|existing| existing.state.rank() >= record.state.rank())
            .unwrap_or(false);
        if !keep {
            self.reservations.insert(record.request_id.clone(), record);
        }
    }
}

/// Budget admission errors. Every variant denies admission.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error("allowance is unknown")]
    UnknownAllowance,
    #[error("cost bound or price is unknown")]
    UnknownPrice,
    #[error("provider profile {0} is expired")]
    ProfileExpired(String),
    #[error("request is unsupported: {reason}")]
    UnsupportedRequest { reason: String },
    #[error("declared output bound missing for field {field}")]
    MissingOutputBound { field: String },
    #[error("declared output {declared} exceeds the reviewed bound {bound}")]
    OutputBoundExceeded { declared: u64, bound: u64 },
    #[error("request body {bytes} bytes exceeds the billable context bound {bound}")]
    ContextBoundExceeded { bytes: u64, bound: u64 },
    #[error(
        "over budget: allowance {allowance_nusd} nusd, used {used_nusd} nusd, proposed {proposed_nusd} nusd"
    )]
    OverBudget {
        allowance_nusd: Nusd,
        used_nusd: Nusd,
        proposed_nusd: Nusd,
    },
    #[error("quota is unknown for a quota-governed route")]
    UnknownQuota,
    #[error("unknown usage retains liability; paid dispatch is suppressed until reconciled")]
    UnknownUsage,
    #[error("over quota: cap {cap_units}, used {used_units}, proposed {proposed_units}")]
    OverQuota {
        cap_units: u64,
        used_units: u64,
        proposed_units: u64,
    },
    #[error("conflicting reservation {0}")]
    Conflict(String),
    #[error("no reservation for request {0}")]
    UnknownRequest(String),
    #[error("budget persistence failed: {0}")]
    Persist(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(id: &str, role: Role, liability: Nusd) -> Reservation {
        Reservation {
            request_id: id.to_string(),
            execution_id: "exec:test:1".to_string(),
            package_digest: "sha256:pkg".to_string(),
            role,
            profile_digest: "sha256:profile".to_string(),
            max_liability_nusd: liability,
            quota_units: 0,
            state: ReservationState::Reserved,
            charged_nusd: None,
            created_at_unix_ms: 1,
        }
    }

    #[test]
    fn fake_profile_reserves_exactly_the_reviewed_bound() {
        let profile = ProviderRequestProfile::fake_byte_token();
        assert_eq!(profile.reservation_nusd().unwrap(), 14_336_000);
        assert!(profile.is_valid_at(1_791_820_800_000));
    }

    #[test]
    fn expiry_and_unsupported_n_deny() {
        let mut profile = ProviderRequestProfile::fake_byte_token();
        profile.valid_until_unix_ms = 10;
        assert!(!profile.is_valid_at(11), "the interval end is exclusive");
        profile.n = 2;
        assert!(matches!(
            profile.reservation_nusd(),
            Err(BudgetError::UnsupportedRequest { .. })
        ));
        // An overflowing price is an unknown bound, never a silent wrap.
        let mut huge = ProviderRequestProfile::fake_byte_token();
        huge.input_limit_tokens = u64::MAX;
        huge.max_input_price_per_million_nusd = u64::MAX;
        assert!(matches!(
            huge.reservation_nusd(),
            Err(BudgetError::UnknownPrice)
        ));
    }

    #[test]
    fn cash_and_quota_are_never_added() {
        let mut state = SpendState {
            allowance_nusd: Some(20_000_000),
            quota_cap_units: Some(5),
            ..Default::default()
        };
        let mut r = reservation("r1", Role::Builder, 14_336_000);
        r.quota_units = 3;
        state.reserve(r).unwrap();
        assert_eq!(state.cash_spent_nusd(), 0);
        assert_eq!(state.outstanding_nusd(), 14_336_000);
        assert_eq!(state.quota_committed_units(), 3);
        // Cash alone fits; quota (3 + 3) fits too.
        let mut r2 = reservation("r2", Role::Verifier, 5_000_000);
        r2.quota_units = 2;
        state.reserve(r2).unwrap();
        // A third quota unit would exceed the cap.
        let mut r3 = reservation("r3", Role::Evaluator, 1);
        r3.quota_units = 1;
        assert!(matches!(
            state.reserve(r3),
            Err(BudgetError::OverQuota { .. })
        ));
    }

    #[test]
    fn unknown_quota_cap_denies_quota_governed_request() {
        let mut state = SpendState {
            allowance_nusd: Some(20_000_000),
            ..Default::default()
        };
        let mut r = reservation("r1", Role::Builder, 1);
        r.quota_units = 1;
        assert_eq!(state.reserve(r), Err(BudgetError::UnknownQuota));
    }

    #[test]
    fn boundary_equal_allowance_is_admitted() {
        let mut state = SpendState {
            allowance_nusd: Some(14_336_000),
            ..Default::default()
        };
        state
            .reserve(reservation("r", Role::Builder, 14_336_000))
            .expect("exactly the allowance is admitted");
        // One nanoUSD over is refused.
        let mut over = SpendState {
            allowance_nusd: Some(14_336_000),
            ..Default::default()
        };
        assert!(matches!(
            over.reserve(reservation("r", Role::Builder, 14_336_001)),
            Err(BudgetError::OverBudget { .. })
        ));
    }

    #[test]
    fn settle_is_idempotent_and_partial_usage_never_zero() {
        let mut state = SpendState {
            allowance_nusd: Some(20_000_000),
            ..Default::default()
        };
        state
            .reserve(reservation("r", Role::Builder, 14_336_000))
            .unwrap();
        state.mark_sent("r").unwrap();
        // Partial usage retains the full liability.
        state.settle("r", None).unwrap();
        assert_eq!(state.outstanding_nusd(), 14_336_000);
        // Repeated partial observations never change anything.
        state.settle("r", None).unwrap();
        assert_eq!(state.outstanding_nusd(), 14_336_000);
        // A later complete observation settles once.
        state.settle("r", Some(7_000_000)).unwrap();
        assert_eq!(state.outstanding_nusd(), 0);
        assert_eq!(state.cash_spent_nusd(), 7_000_000);
        // Duplicate settlement never double counts.
        state.settle("r", Some(7_000_000)).unwrap();
        state.settle("r", Some(9_999_999)).unwrap();
        assert_eq!(state.cash_spent_nusd(), 7_000_000);
    }

    #[test]
    fn cancellation_releases_only_unsent_work() {
        let mut state = SpendState {
            allowance_nusd: Some(40_000_000),
            ..Default::default()
        };
        state
            .reserve(reservation("unsent", Role::Builder, 1_000))
            .unwrap();
        state.cancel("unsent").unwrap();
        assert_eq!(state.outstanding_nusd(), 0, "never-sent work is released");

        state
            .reserve(reservation("sent", Role::Builder, 14_336_000))
            .unwrap();
        state.mark_sent("sent").unwrap();
        state.cancel("sent").unwrap();
        assert_eq!(
            state.outstanding_nusd(),
            14_336_000,
            "cancellation cannot refund an incurred charge"
        );
    }

    #[test]
    fn replay_is_order_independent_and_monotone() {
        let mut forward = SpendState::default();
        let mut a = reservation("r", Role::Builder, 100);
        a.state = ReservationState::Reserved;
        let mut b = a.clone();
        b.state = ReservationState::Sent;
        let mut c = a.clone();
        c.state = ReservationState::Settled;
        c.charged_nusd = Some(40);
        forward.apply(a.clone());
        forward.apply(b.clone());
        forward.apply(c.clone());

        let mut backward = SpendState::default();
        backward.apply(c);
        backward.apply(b);
        backward.apply(a);
        assert_eq!(forward, backward);
    }
}
