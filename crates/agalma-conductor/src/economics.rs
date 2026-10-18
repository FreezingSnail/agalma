//! Economics + timeline derivation for the TUI read model (TUI.4, `agalma-3kr.5`).
//!
//! A read-only, ledger-only layer over the TUI.1 snapshot (`docs/tui-design.md`,
//! "Increments" TUI.4):
//!
//! - **spend** per execution, derived from `usage` events through the single
//!   shared aggregation [`crate::digest::cost_from_events`] (the digest rollup
//!   uses the same function — no duplicated derivation);
//! - **allowance** = the intake-fence `budget_usd`, display-only. The budget is
//!   not yet durable in the ledger, so the snapshot path passes no allowances
//!   and every row renders `unknown`; callers that can source them supply a map
//!   keyed by execution id. There is no budget enforcement (design non-goal);
//!   the allowance never gates anything;
//! - **digest summaries** — the latest `digests` row parsed back into the
//!   [`crate::digest::Digest`] shape;
//! - **parked / degradation alerts** — a parked execution row, or an execution
//!   with recorded `provider_health` signals;
//! - a curated **timeline** of `execution_events` (lifecycle/decision kinds plus
//!   digest milestones) and durable control receipts (`pause`/`resume`/`cancel`,
//!   `docs/tui-design.md` "Control policy"), ordered oldest-first.

use std::collections::BTreeMap;

use agalma_contracts::{
    Completeness, ControlReceipt, DigestRecord, ExecutionEvent, ExecutionPhase, ExecutionRecord,
    ExecutionState, ExactAmount, LedgerApi, SourceState, UsageCost,
};
use serde::Serialize;
use serde_json::Value;

use crate::digest::{cost_from_events, Digest};

/// Event kind carrying a provider/watchdog health (degradation) signal.
pub const PROVIDER_HEALTH_KIND: &str = "provider_health";
/// Event kind recording a parked execution.
pub const PARKED_KIND: &str = "parked";
/// Timeline kind carrying a durable control receipt (`pause`/`resume`/`cancel`).
pub const CONTROL_KIND: &str = "control";

/// Derived economics + timeline section of the read model.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EconomicsSnapshot {
    /// One row per execution, in read-model (execution-id) order.
    pub rows: Vec<EconomicsRow>,
    /// Curated event + digest timeline, oldest first.
    pub timeline: Vec<TimelineEntry>,
    /// Parked/degradation alerts, one per flagged signal.
    pub alerts: Vec<Alert>,
}

/// One execution's cost/budget row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EconomicsRow {
    pub execution_id: String,
    pub task_id: String,
    /// `Debug` name of [`ExecutionPhase`] (e.g. `"Build"`).
    pub phase: String,
    /// `Debug` name of [`ExecutionState`] (e.g. `"Running"`).
    pub state: String,
    pub attempt: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Spend in USD summed from `usage` events.
    pub spend_usd: f64,
    /// Intake-fence allowance; `null` when unknown.
    pub allowance_usd: Option<f64>,
    /// `allowance - spend`; `null` when the allowance is unknown.
    pub remaining_usd: Option<f64>,
    /// Whether spend exceeds a known allowance.
    pub over_budget: bool,
    /// Recorded `provider_health` (degradation) signals for this execution.
    pub stalls: u32,
    /// Whether the execution recorded any degradation signal.
    pub degraded: bool,
    /// Whether the execution is parked (state or phase).
    pub parked: bool,
    /// Recorded park reason, when one is present.
    pub parked_reason: Option<String>,
    /// Latest digest summary rollup, when a digest was recorded.
    pub digest: Option<DigestSummary>,
}

impl EconomicsRow {
    /// `spend / allowance` text; allowance renders `unknown` when unset.
    pub fn spend_allowance_text(&self) -> String {
        match self.allowance_usd {
            Some(allowance) => {
                let remaining = self
                    .remaining_usd
                    .map(|r| format!(" rem ${r:.6}"))
                    .unwrap_or_default();
                let over = if self.over_budget { " OVER" } else { "" };
                format!("${:.6} / ${allowance:.2}{remaining}{over}", self.spend_usd)
            }
            None => format!("${:.6} / unknown", self.spend_usd),
        }
    }
}

/// Compact digest rollup for the economics panel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DigestSummary {
    /// `green` / `red`.
    pub verdict: String,
    pub attempts: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub usd: f64,
    pub wall_ms: u64,
    pub stalls: u32,
}

impl From<&Digest> for DigestSummary {
    fn from(digest: &Digest) -> Self {
        DigestSummary {
            verdict: digest.verdict.as_str().to_string(),
            attempts: digest.repairs.attempts,
            tokens_in: digest.cost.tokens_in,
            tokens_out: digest.cost.tokens_out,
            usd: digest.cost.usd,
            wall_ms: digest.wall_ms,
            stalls: digest.stalls,
        }
    }
}

impl DigestSummary {
    /// One-line digest rollup (`verdict attempts=N tokens=in/out usd=… …`).
    pub fn summary_text(&self) -> String {
        format!(
            "{} attempts={} tokens={}/{} usd={:.6} wall_ms={} stalls={}",
            self.verdict,
            self.attempts,
            self.tokens_in,
            self.tokens_out,
            self.usd,
            self.wall_ms,
            self.stalls
        )
    }
}

/// One curated timeline entry (an execution event or a digest milestone).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineEntry {
    pub execution_id: String,
    /// Per-execution event sequence; `0` for digest milestones.
    pub sequence: u64,
    /// Event kind, or `digest` for a digest milestone.
    pub kind: String,
    pub summary: String,
    pub recorded_at_unix_ms: u64,
}

impl TimelineEntry {
    /// `#seq kind summary`.
    pub fn summary_line(&self) -> String {
        format!("#{} {} {}", self.sequence, self.kind, self.summary)
    }
}

/// Severity of an economics alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AlertLevel {
    /// The execution is parked.
    Park,
    /// The execution recorded degradation signals.
    Degraded,
}

/// A parked/degradation alert for one execution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alert {
    pub execution_id: String,
    pub level: AlertLevel,
    pub reason: String,
}

/// One source-bound read: explicit availability so a failed read is never
/// mistaken for a successful empty result (or a numeric zero).
#[derive(Clone, Debug, PartialEq)]
pub enum SourceRead<T> {
    /// Read succeeded and holds data.
    Available(T),
    /// Read succeeded and holds no entries.
    Empty,
    /// The source does not exist.
    Missing(String),
    /// The source exists but could not be read.
    Unreadable(String),
}

impl<T> SourceRead<T> {
    /// The wire [`SourceState`] this read observation maps to.
    pub fn state(&self) -> SourceState {
        match self {
            SourceRead::Available(_) => SourceState::Available,
            SourceRead::Empty => SourceState::Empty,
            SourceRead::Missing(_) => SourceState::Missing,
            SourceRead::Unreadable(_) => SourceState::Unreadable,
        }
    }

    /// A human-readable detail for the observation (`""` when self-evident).
    pub fn detail(&self) -> &str {
        match self {
            SourceRead::Missing(detail) | SourceRead::Unreadable(detail) => detail,
            SourceRead::Available(_) | SourceRead::Empty => "",
        }
    }

    /// Whether the read completed (available or empty).
    pub fn is_readable(&self) -> bool {
        matches!(self, SourceRead::Available(_) | SourceRead::Empty)
    }

    /// The carried value, when the read succeeded with entries.
    pub fn value(&self) -> Option<&T> {
        match self {
            SourceRead::Available(value) => Some(value),
            _ => None,
        }
    }
}

/// Bound domain reads for the economics projection: every source is explicit
/// [`SourceRead`], so a missing or unreadable ledger stays visible.
#[derive(Clone, Debug, PartialEq)]
pub struct EconomicsInputs {
    pub executions: SourceRead<Vec<ExecutionRecord>>,
    pub events: BTreeMap<String, SourceRead<Vec<ExecutionEvent>>>,
    pub digests: BTreeMap<String, SourceRead<Vec<DigestRecord>>>,
    pub control_receipts: SourceRead<Vec<ControlReceipt>>,
}

impl EconomicsInputs {
    /// Every source missing (no ledger bound).
    pub fn missing(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        EconomicsInputs {
            executions: SourceRead::Missing(detail.clone()),
            events: BTreeMap::new(),
            digests: BTreeMap::new(),
            control_receipts: SourceRead::Missing(detail),
        }
    }

    /// Every source unreadable (a bound ledger read failed).
    pub fn unreadable(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        EconomicsInputs {
            executions: SourceRead::Unreadable(detail.clone()),
            events: BTreeMap::new(),
            digests: BTreeMap::new(),
            control_receipts: SourceRead::Unreadable(detail),
        }
    }
}

/// The derived economics snapshot plus each source's observed availability.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EconomicsProjection {
    pub snapshot: EconomicsSnapshot,
    /// Availability of the execution list.
    pub executions: SourceState,
    /// Availability of the event read across observed executions.
    pub events: SourceState,
    /// Availability of the digest read across observed executions.
    pub digests: SourceState,
    /// Availability of the durable control receipts.
    pub control_receipts: SourceState,
    /// Completeness of the observed `usage` events: `Complete` only when every
    /// usage event carried its usage fields, `Partial` when some fields were
    /// absent, `Unknown` when the execution/event source failed.
    pub usage: Completeness,
}

impl EconomicsProjection {
    /// Whether usage and cost totals can be stated (no execution or event
    /// source failed).
    pub fn usage_is_known(&self) -> bool {
        matches!(self.executions, SourceState::Available | SourceState::Empty)
            && matches!(self.events, SourceState::Available | SourceState::Empty)
    }

    /// Known usage/cost/allowance totals. Any missing or unreadable source makes
    /// the affected total `None` (unknown), never a numeric zero.
    pub fn usage_cost(
        &self,
        observed_at_unix_ms: u64,
    ) -> UsageCost {
        let Some(rows) = self.known_rows() else {
            return UsageCost {
                completeness: Completeness::Unknown,
                usage: None,
                usage_unit: "tokens".to_string(),
                cost: None,
                allowance: None,
                cost_unit: "usd".to_string(),
                observed_at_unix_ms,
            };
        };
        // Exact wider accumulation: per-row and aggregate token overflow is
        // explicit Unknown, never a wrapped/saturated complete total.
        let mut tokens_total: u128 = 0;
        for row in rows {
            tokens_total += row.tokens_in as u128 + row.tokens_out as u128;
        }
        let token_overflow = tokens_total > u64::MAX as u128;
        let usd: f64 = rows.iter().map(|row| row.spend_usd).sum();
        let allowance_known = !rows.is_empty() && rows.iter().all(|row| row.allowance_usd.is_some());
        let allowance_usd: f64 = rows.iter().filter_map(|row| row.allowance_usd).sum();
        // Unknown/malformed/overflowing usage never fabricates a numeric total.
        let completeness = if token_overflow {
            Completeness::Unknown
        } else {
            self.usage
        };
        let totals_known = completeness == Completeness::Complete;
        UsageCost {
            completeness,
            usage: totals_known
                .then(|| ExactAmount::parse(&tokens_total.to_string()).ok())
                .flatten(),
            usage_unit: "tokens".to_string(),
            cost: totals_known.then(|| exact_amount(usd)).flatten(),
            allowance: allowance_known.then(|| exact_amount(allowance_usd)).flatten(),
            cost_unit: "usd".to_string(),
            observed_at_unix_ms,
        }
    }

    /// The rows usable for totals, or `None` when an input source failed.
    fn known_rows(&self) -> Option<&[EconomicsRow]> {
        self.usage_is_known().then_some(self.snapshot.rows.as_slice())
    }
}

/// Format a finite, non-negative `f64` as an exact decimal amount, or `None`.
fn exact_amount(value: f64) -> Option<ExactAmount> {
    // Normalize the empty-sum `-0` to `0` so a known zero is a valid amount.
    let value = if value == 0.0 { 0.0 } else { value };
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    ExactAmount::parse(&format!("{value:.6}")).ok()
}

/// Derive the economics section from bound domain inputs.
///
/// Every source is explicit: a missing or unreadable execution/event/digest/
/// receipt read never degrades into an empty successful section or a numeric
/// zero total.
pub fn derive_bound(
    inputs: &EconomicsInputs,
    allowances: &BTreeMap<String, f64>,
) -> EconomicsProjection {
    let records: Vec<ExecutionRecord> = match &inputs.executions {
        SourceRead::Available(records) => records.clone(),
        _ => Vec::new(),
    };

    let mut rows = Vec::new();
    let mut timeline = Vec::new();
    let mut alerts = Vec::new();
    let mut events_failed = false;
    let mut events_available = false;
    let mut digests_failed = false;
    let mut digests_available = false;
    let mut usage = Completeness::Complete;

    for record in &records {
        let execution_id = record.execution_id.as_str().to_string();
        let events: &[ExecutionEvent] = match inputs.events.get(&execution_id) {
            Some(SourceRead::Available(events)) => events,
            Some(SourceRead::Empty) => &[],
            _ => {
                events_failed = true;
                &[]
            }
        };
        if !events.is_empty() {
            events_available = true;
        }
        if inputs.events.get(&execution_id).map_or(true, |read| !read.is_readable()) {
            usage = Completeness::Unknown;
        } else {
            usage = merge_usage_completeness(usage, usage_completeness(events));
        }
        let digest_rows: &[DigestRecord] = match inputs.digests.get(&execution_id) {
            Some(SourceRead::Available(rows)) => rows,
            Some(SourceRead::Empty) => &[],
            _ => {
                digests_failed = true;
                &[]
            }
        };
        if !digest_rows.is_empty() {
            digests_available = true;
        }
        // Preflight the per-execution usage counters before the shared helper:
        // `cost_from_events` adds inside `u64`, so any execution whose in/out
        // total exceeds `u64::MAX` must be Unknown rather than overflow/wrap.
        let counter_overflow = usage_counter_overflow(events);
        let spend = if counter_overflow {
            crate::digest::CostSummary::default()
        } else {
            cost_from_events(events)
        };
        if counter_overflow {
            usage = Completeness::Unknown;
        }

        let mut stalls = 0u32;
        let mut last_health_reason: Option<String> = None;
        for event in events {
            if event.kind != PROVIDER_HEALTH_KIND {
                continue;
            }
            stalls += 1;
            if let Some(reason) = event.payload.get("reason").and_then(|v| v.as_str()) {
                last_health_reason = Some(reason.to_string());
            }
        }

        let parked_reason = parked_reason_from_events(events);
        let parked = matches!(record.state, ExecutionState::Parked)
            || matches!(record.phase, ExecutionPhase::Parked);
        let degraded = stalls > 0;

        let allowance_usd = allowances.get(&execution_id).copied();
        let remaining_usd = allowance_usd.map(|allowance| allowance - spend.usd);
        let over_budget = remaining_usd.map(|r| r < 0.0).unwrap_or(false);

        let digest = digest_summary(digest_rows);

        for event in events {
            if let Some(summary) = summarize_event(event) {
                timeline.push(TimelineEntry {
                    execution_id: execution_id.clone(),
                    sequence: event.sequence,
                    kind: event.kind.clone(),
                    summary,
                    recorded_at_unix_ms: event.recorded_at_unix_ms,
                });
            }
        }
        if let Some(digest_summary) = &digest {
            let recorded_at = digest_rows
                .last()
                .map(|row| row.recorded_at_unix_ms)
                .unwrap_or(0);
            timeline.push(TimelineEntry {
                execution_id: execution_id.clone(),
                sequence: 0,
                kind: "digest".to_string(),
                summary: digest_summary.summary_text(),
                recorded_at_unix_ms: recorded_at,
            });
        }

        if parked {
            alerts.push(Alert {
                execution_id: execution_id.clone(),
                level: AlertLevel::Park,
                reason: parked_reason
                    .clone()
                    .unwrap_or_else(|| "parked".to_string()),
            });
        }
        if degraded {
            alerts.push(Alert {
                execution_id: execution_id.clone(),
                level: AlertLevel::Degraded,
                reason: format!(
                    "{} stall(s) · last: {}",
                    stalls,
                    last_health_reason.as_deref().unwrap_or("-")
                ),
            });
        }

        rows.push(EconomicsRow {
            execution_id,
            task_id: record.task_id.as_str().to_string(),
            phase: format!("{:?}", record.phase),
            state: format!("{:?}", record.state),
            attempt: record.attempt,
            tokens_in: spend.tokens_in,
            tokens_out: spend.tokens_out,
            spend_usd: spend.usd,
            allowance_usd,
            remaining_usd,
            over_budget,
            stalls,
            degraded,
            parked,
            parked_reason,
            digest,
        });
    }

    let receipts: &[ControlReceipt] = match &inputs.control_receipts {
        SourceRead::Available(receipts) => receipts,
        SourceRead::Empty => &[],
        _ => &[],
    };
    for receipt in receipts {
        timeline.push(control_entry(receipt));
    }

    // Deterministic oldest-first order: timestamp, then execution, then the
    // per-execution sequence (digest milestones carry sequence 0).
    timeline.sort_by(|a, b| {
        a.recorded_at_unix_ms
            .cmp(&b.recorded_at_unix_ms)
            .then_with(|| a.execution_id.cmp(&b.execution_id))
            .then_with(|| a.sequence.cmp(&b.sequence))
    });

    let executions_state = match &inputs.executions {
        SourceRead::Available(records) if records.is_empty() => SourceState::Empty,
        other => other.state(),
    };
    let executions_readable = matches!(
        &inputs.executions,
        SourceRead::Available(_) | SourceRead::Empty
    );
    // A missing/unreadable execution list means the dependent event and digest
    // reads never happened: report the execution source state, not `Empty`.
    let events_state = if !executions_readable {
        executions_state
    } else if events_failed {
        SourceState::Unreadable
    } else if records.is_empty() {
        SourceState::Empty
    } else if events_available {
        SourceState::Available
    } else {
        SourceState::Empty
    };
    let digests_state = if !executions_readable {
        executions_state
    } else if digests_failed {
        SourceState::Unreadable
    } else if records.is_empty() {
        SourceState::Empty
    } else if digests_available {
        SourceState::Available
    } else {
        SourceState::Empty
    };
    if !executions_readable {
        usage = Completeness::Unknown;
    }

    EconomicsProjection {
        snapshot: EconomicsSnapshot {
            rows,
            timeline,
            alerts,
        },
        executions: executions_state,
        events: events_state,
        digests: digests_state,
        control_receipts: inputs.control_receipts.state(),
        usage,
    }
}

/// Whether any per-execution usage counter total exceeds `u64::MAX`.
fn usage_counter_overflow(events: &[ExecutionEvent]) -> bool {
    let mut tokens_in: u128 = 0;
    let mut tokens_out: u128 = 0;
    for event in events {
        if event.kind != "usage" {
            continue;
        }
        tokens_in += event
            .payload
            .get("tokens_in")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u128;
        tokens_out += event
            .payload
            .get("tokens_out")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u128;
    }
    tokens_in > u64::MAX as u128 || tokens_out > u64::MAX as u128
}

/// Completeness of the `usage` events in one execution's event list.
///
/// A recorded usage event with an absent `tokens_in`, `tokens_out`, or
/// `cost_usd` field is `Partial` — a missing field is never silently read as a
/// complete zero. No usage events is a known empty (`Complete`).
fn usage_completeness(events: &[ExecutionEvent]) -> Completeness {
    let mut seen_usage = false;
    let mut observed = Completeness::Complete;
    for event in events {
        if event.kind != "usage" {
            continue;
        }
        seen_usage = true;
        // `null`/non-numeric fields are missing, not zeros: only well-typed
        // fields make the usage observation complete.
        let fields_complete = event
            .payload
            .get("tokens_in")
            .and_then(Value::as_u64)
            .is_some()
            && event
                .payload
                .get("tokens_out")
                .and_then(Value::as_u64)
                .is_some()
            && event
                .payload
                .get("cost_usd")
                .and_then(Value::as_f64)
                .is_some();
        // Merge the producer's own declared completeness conservatively:
        // Unknown stays Unknown, Partial stays Partial. A missing declaration
        // with valid fields is treated as complete.
        // Producers publish the canonical `Completeness::label()` values
        // (`Complete`/`Partial`/`Unknown`); accept the exact labels plus their
        // lowercase spelling, and fail closed to Unknown for anything else.
        let producer = match event.payload.get("completeness").and_then(Value::as_str) {
            None => Completeness::Complete,
            Some(raw) => match raw {
                "Complete" | "complete" => Completeness::Complete,
                "Partial" | "partial" => Completeness::Partial,
                "Unknown" | "unknown" => Completeness::Unknown,
                _ => Completeness::Unknown,
            },
        };
        let event_state = match (fields_complete, producer) {
            (_, Completeness::Unknown) => Completeness::Unknown,
            (false, _) | (_, Completeness::Partial) => Completeness::Partial,
            (true, Completeness::Complete) => Completeness::Complete,
        };
        observed = merge_usage_completeness(observed, event_state);
    }
    if !seen_usage {
        Completeness::Complete
    } else {
        observed
    }
}

/// Combine per-execution usage completeness (worst case wins).
fn merge_usage_completeness(current: Completeness, next: Completeness) -> Completeness {
    match (current, next) {
        (Completeness::Unknown, _) | (_, Completeness::Unknown) => Completeness::Unknown,
        (Completeness::Partial, _) | (_, Completeness::Partial) => Completeness::Partial,
        _ => Completeness::Complete,
    }
}

/// Derive the economics section from the read model.
///
/// `allowances` maps execution id → intake-fence `budget_usd`; absent entries are
/// unknown. The derivation is read-only: a ledger read error for one source is
/// recorded explicitly in [`SourceRead`] rather than fabricated as empty data.
pub fn derive<L: LedgerApi + ?Sized>(
    ledger: &L,
    records: &[ExecutionRecord],
    allowances: &BTreeMap<String, f64>,
) -> EconomicsSnapshot {
    let mut events = BTreeMap::new();
    let mut digests = BTreeMap::new();
    for record in records {
        let execution_id = record.execution_id.as_str().to_string();
        events.insert(
            execution_id.clone(),
            match ledger.events(&record.execution_id) {
                Ok(events) if events.is_empty() => SourceRead::Empty,
                Ok(events) => SourceRead::Available(events),
                Err(error) => SourceRead::Unreadable(error.to_string()),
            },
        );
        digests.insert(
            execution_id,
            match ledger.digests_for_execution(&record.execution_id) {
                Ok(rows) if rows.is_empty() => SourceRead::Empty,
                Ok(rows) => SourceRead::Available(rows),
                Err(error) => SourceRead::Unreadable(error.to_string()),
            },
        );
    }
    let control_receipts = match ledger.control_receipts() {
        Ok(receipts) if receipts.is_empty() => SourceRead::Empty,
        Ok(receipts) => SourceRead::Available(receipts),
        Err(error) => SourceRead::Unreadable(error.to_string()),
    };
    let executions = if records.is_empty() {
        SourceRead::Empty
    } else {
        SourceRead::Available(records.to_vec())
    };
    derive_bound(
        &EconomicsInputs {
            executions,
            events,
            digests,
            control_receipts,
        },
        allowances,
    )
    .snapshot
}

/// Map one durable control receipt to a timeline entry (`pause applied by human`).
///
/// Latch actions (pause/resume) carry an empty `target`; cancel receipts carry
/// the cancelled execution id.
fn control_entry(receipt: &ControlReceipt) -> TimelineEntry {
    let outcome = receipt
        .request
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("recorded");
    TimelineEntry {
        execution_id: receipt.target.clone(),
        sequence: 0,
        kind: CONTROL_KIND.to_string(),
        summary: format!("{} {} by {}", receipt.action, outcome, receipt.actor),
        recorded_at_unix_ms: receipt.recorded_at_unix_ms,
    }
}

/// Latest recorded digest row, parsed back into the digest shape.
fn digest_summary(rows: &[DigestRecord]) -> Option<DigestSummary> {
    let latest = rows.last()?;
    let digest = serde_json::from_value::<Digest>(latest.summary.clone()).ok()?;
    Some(DigestSummary::from(&digest))
}

/// The latest recorded `parked` reason from an already-loaded event list.
fn parked_reason_from_events(events: &[ExecutionEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.kind == PARKED_KIND)
        .and_then(|event| event.payload.get("reason"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Curated one-line summary for an event, or `None` when the kind is not part
/// of the v1 timeline (only lifecycle/decision/knowledge kinds show).
fn summarize_event(event: &ExecutionEvent) -> Option<String> {
    let payload = &event.payload;
    let operation = payload.get("operation_id").and_then(|v| v.as_str());
    let attempt = payload.get("attempt").and_then(|v| v.as_u64());
    Some(match event.kind.as_str() {
        "start" => "started".to_string(),
        "claim" => match payload.get("lease_generation").and_then(|v| v.as_u64()) {
            Some(lease) => format!("claimed lease {lease}"),
            None => "claimed".to_string(),
        },
        "decision" => op_attempt("decision recorded", operation, attempt),
        "deliver" | "operation" => op_attempt("operation delivered", operation, attempt),
        "retry" => op_attempt("retry", operation, attempt),
        PARKED_KIND => format!(
            "parked: {}",
            payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("reason unknown")
        ),
        PROVIDER_HEALTH_KIND => format!(
            "degraded: {}",
            payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("reason unknown")
        ),
        "cancel" | "cancel_requested" => "cancel requested".to_string(),
        "timer_fired" => "timer fired".to_string(),
        "directive_update" => "directive update".to_string(),
        "kill_latch" => match payload.get("latched").and_then(|v| v.as_bool()) {
            Some(latched) => format!("kill_latch: {latched}"),
            None => "kill_latch".to_string(),
        },
        kind if kind == crate::harness_binding::BINDING_EVENT_KIND => "binding pinned".to_string(),
        _ => return None,
    })
}

/// `label op@attempt`, degrading to `label op` or `label` when fields are absent.
fn op_attempt(label: &str, operation: Option<&str>, attempt: Option<u64>) -> String {
    match (operation, attempt) {
        (Some(operation), Some(attempt)) => format!("{label} {operation}@{attempt}"),
        (Some(operation), None) => format!("{label} {operation}"),
        _ => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::{
        CommitBatch, DigestRecord, ExecutionId, ExecutionRecord, ExecutionState, TaskId,
    };
    use agalma_ledger::SqliteLedger;
    use agalma_testkit::ScratchDir;
    use serde_json::json;

    /// Seed one execution row and return its id.
    fn seed_execution(dir: &ScratchDir, task: &str, state: ExecutionState) -> ExecutionId {
        let exec = ExecutionId::derive(&TaskId::new(task), 1);
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(ExecutionRecord {
                    execution_id: exec.clone(),
                    task_id: TaskId::new(task),
                    generation: 1,
                    phase: ExecutionPhase::Build,
                    state,
                    attempt: 2,
                    revision: 0,
                }),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .expect("seed execution");
        exec
    }

    /// Append an event with an explicit kind/payload/timestamp.
    fn append_event(
        dir: &ScratchDir,
        exec: &ExecutionId,
        kind: &str,
        payload: serde_json::Value,
        at: u64,
    ) {
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: None,
                events: vec![ExecutionEvent {
                    execution_id: exec.clone(),
                    sequence: 0,
                    kind: kind.to_string(),
                    payload,
                    recorded_at_unix_ms: at,
                }],
                receipts: vec![],
                intents: vec![],
            })
            .expect("append event");
    }

    /// Record a digest row for `exec`.
    fn record_digest(dir: &ScratchDir, exec: &ExecutionId, version: u32) {
        let digest = Digest {
            execution_id: exec.as_str().to_string(),
            task_id: "task-a".to_string(),
            model: "model".to_string(),
            route: "free:reference".to_string(),
            base_sha: None,
            candidate_sha: None,
            result_sha: None,
            files_touched: vec![],
            commands: vec![],
            repairs: crate::digest::RepairSummary {
                attempts: 2,
                diagnosis_artifacts: vec![],
            },
            cost: crate::digest::CostSummary {
                tokens_in: 120,
                tokens_out: 45,
                usd: 0.0025,
            },
            cash_usd: 0.0,
            quota_usd: 0.0,
            wall_ms: 987,
            verdict: crate::digest::Verdict::Green,
            stalls: 1,
        };
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .record_digest(&DigestRecord {
                execution_id: exec.clone(),
                version,
                summary: serde_json::to_value(&digest).expect("digest json"),
                artifact_ref: None,
                recorded_at_unix_ms: 9_000,
            })
            .expect("record digest");
    }

    #[test]
    fn spend_vs_allowance_known_and_unknown() {
        let dir = ScratchDir::new("economics-spend");
        let exec = seed_execution(&dir, "task-a", ExecutionState::Running);
        append_event(
            &dir,
            &exec,
            "usage",
            json!({"tokens_in": 100, "tokens_out": 40, "cost_usd": 0.25}),
            1,
        );
        append_event(
            &dir,
            &exec,
            "usage",
            json!({"tokens_in": 50, "tokens_out": 10, "cost_usd": 0.05}),
            2,
        );

        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let records = ledger.executions().expect("executions");

        // Known allowance: remaining and over-budget are computed.
        let mut allowances = BTreeMap::new();
        allowances.insert(exec.as_str().to_string(), 0.20_f64);
        let snapshot = derive(&ledger, &records, &allowances);
        let row = &snapshot.rows[0];
        assert_eq!(row.tokens_in, 150);
        assert_eq!(row.tokens_out, 50);
        assert!((row.spend_usd - 0.30).abs() < 1e-9);
        assert_eq!(row.allowance_usd, Some(0.20));
        assert!(row.over_budget, "spend 0.30 exceeds allowance 0.20");
        let remaining = row.remaining_usd.expect("remaining");
        assert!((remaining + 0.10).abs() < 1e-9, "remaining {remaining}");
        assert!(row.spend_allowance_text().contains("OVER"));
        assert!(row.spend_allowance_text().contains("$0.30"));

        // Unknown allowance: everything budget-related renders as unknown.
        let unknown = derive(&ledger, &records, &BTreeMap::new());
        let row = &unknown.rows[0];
        assert_eq!(row.allowance_usd, None);
        assert_eq!(row.remaining_usd, None);
        assert!(!row.over_budget);
        assert_eq!(row.spend_allowance_text(), "$0.300000 / unknown");
    }

    #[test]
    fn digest_summary_is_derived_from_the_latest_row() {
        let dir = ScratchDir::new("economics-digest");
        let exec = seed_execution(&dir, "task-a", ExecutionState::Completed);
        record_digest(&dir, &exec, 1);
        record_digest(&dir, &exec, 2);

        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let records = ledger.executions().expect("executions");
        let snapshot = derive(&ledger, &records, &BTreeMap::new());

        let digest = snapshot.rows[0].digest.as_ref().expect("digest summary");
        assert_eq!(digest.verdict, "green");
        assert_eq!(digest.attempts, 2);
        assert_eq!(digest.tokens_in, 120);
        assert_eq!(digest.tokens_out, 45);
        assert_eq!(digest.wall_ms, 987);
        assert!(digest.summary_text().contains("green attempts=2"));
        // The digest appears as a final timeline milestone.
        assert!(snapshot.timeline.iter().any(|entry| entry.kind == "digest"));
    }

    #[test]
    fn parked_and_degradation_produce_alerts() {
        let dir = ScratchDir::new("economics-alerts");
        let exec = seed_execution(&dir, "task-a", ExecutionState::Parked);
        append_event(
            &dir,
            &exec,
            PROVIDER_HEALTH_KIND,
            json!({"reason": "session_gone"}),
            1,
        );

        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let records = ledger.executions().expect("executions");
        let snapshot = derive(&ledger, &records, &BTreeMap::new());

        assert!(snapshot.rows[0].parked);
        assert!(snapshot.rows[0].degraded);
        assert_eq!(snapshot.rows[0].stalls, 1);
        let levels: Vec<AlertLevel> = snapshot.alerts.iter().map(|a| a.level).collect();
        assert!(levels.contains(&AlertLevel::Park));
        assert!(levels.contains(&AlertLevel::Degraded));
        let degraded = snapshot
            .alerts
            .iter()
            .find(|a| a.level == AlertLevel::Degraded)
            .expect("degraded alert");
        assert!(degraded.reason.contains("session_gone"));
    }

    #[test]
    fn timeline_orders_events_and_decisions() {
        let dir = ScratchDir::new("economics-timeline");
        let exec = seed_execution(&dir, "task-a", ExecutionState::Running);
        // Out-of-order insertion into the ledger; the timeline must sort by
        // timestamp. `decision` events are the kernel's decision records.
        append_event(&dir, &exec, "claim", json!({"lease_generation": 3}), 30);
        append_event(
            &dir,
            &exec,
            "decision",
            json!({"operation_id": "op:build@2", "attempt": 2}),
            10,
        );
        append_event(
            &dir,
            &exec,
            "operation",
            json!({"operation_id": "op:build@2", "attempt": 2}),
            20,
        );
        // An uncurated kind is dropped from the timeline.
        append_event(&dir, &exec, "noise", json!({}), 40);

        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let records = ledger.executions().expect("executions");
        let snapshot = derive(&ledger, &records, &BTreeMap::new());

        let kinds: Vec<&str> = snapshot
            .timeline
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["decision", "operation", "claim"]);
        assert!(snapshot.timeline[0].summary.contains("op:build@2@2"));
        assert_eq!(snapshot.timeline[2].summary, "claimed lease 3");
    }

    #[test]
    fn control_receipts_appear_in_the_timeline() {
        let dir = ScratchDir::new("economics-control-receipts");
        let exec = seed_execution(&dir, "task-a", ExecutionState::Running);
        append_event(&dir, &exec, "start", json!({}), 100);

        // Pause then resume (latch actions: empty target), each receipted with
        // an explicit timestamp; both must surface as `control` entries.
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .record_control_receipt(&ControlReceipt {
                action_id: "pause-1".to_string(),
                action: "pause".to_string(),
                target: String::new(),
                actor: "human".to_string(),
                request: json!({"outcome": "applied"}),
                recorded_at_unix_ms: 200,
            })
            .expect("record pause receipt");
        ledger
            .record_control_receipt(&ControlReceipt {
                action_id: "resume-1".to_string(),
                action: "resume".to_string(),
                target: String::new(),
                actor: "human".to_string(),
                request: json!({"outcome": "applied"}),
                recorded_at_unix_ms: 300,
            })
            .expect("record resume receipt");

        let records = ledger.executions().expect("executions");
        let snapshot = derive(&ledger, &records, &BTreeMap::new());

        let controls: Vec<&TimelineEntry> = snapshot
            .timeline
            .iter()
            .filter(|entry| entry.kind == CONTROL_KIND)
            .collect();
        assert_eq!(
            controls.len(),
            2,
            "receipts visible: {:?}",
            snapshot.timeline
        );
        assert_eq!(controls[0].summary, "pause applied by human");
        assert_eq!(controls[0].recorded_at_unix_ms, 200);
        assert_eq!(controls[1].summary, "resume applied by human");
        assert_eq!(controls[1].recorded_at_unix_ms, 300);
        // Oldest-first across sources: the `start` event, then pause, then resume.
        let summaries: Vec<&str> = snapshot
            .timeline
            .iter()
            .map(|entry| entry.summary.as_str())
            .collect();
        assert_eq!(
            summaries,
            vec![
                "started",
                "pause applied by human",
                "resume applied by human"
            ]
        );
    }

    #[test]
    fn no_executions_yields_an_empty_section() {
        let dir = ScratchDir::new("economics-empty");
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let snapshot = derive(&ledger, &[], &BTreeMap::new());
        assert!(snapshot.rows.is_empty());
        assert!(snapshot.timeline.is_empty());
        assert!(snapshot.alerts.is_empty());
    }
}
