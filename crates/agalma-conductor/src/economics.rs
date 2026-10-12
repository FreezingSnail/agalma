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
//!   digest milestones), ordered oldest-first.

use std::collections::BTreeMap;

use agalma_contracts::{
    ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord, ExecutionState, LedgerApi,
};
use serde::Serialize;

use crate::digest::{cost_from_events, Digest};

/// Event kind carrying a provider/watchdog health (degradation) signal.
pub const PROVIDER_HEALTH_KIND: &str = "provider_health";
/// Event kind recording a parked execution.
pub const PARKED_KIND: &str = "parked";

/// Derived economics + timeline section of the read model.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EconomicsSnapshot {
    /// One row per execution, in read-model (execution-id) order.
    pub rows: Vec<EconomicsRow>,
    /// Curated event + digest timeline, oldest first.
    pub timeline: Vec<TimelineEntry>,
    /// Parked/degradation alerts, one per flagged signal.
    pub alerts: Vec<Alert>,
}

/// One execution's cost/budget row.
#[derive(Debug, Clone, Serialize)]
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

/// Derive the economics section from the read model.
///
/// `allowances` maps execution id → intake-fence `budget_usd`; absent entries are
/// unknown. The derivation is read-only and never fails: a ledger read error for
/// one execution degrades to an empty event list for that row.
pub fn derive<L: LedgerApi + ?Sized>(
    ledger: &L,
    records: &[ExecutionRecord],
    allowances: &BTreeMap<String, f64>,
) -> EconomicsSnapshot {
    let mut rows = Vec::new();
    let mut timeline = Vec::new();
    let mut alerts = Vec::new();

    for record in records {
        let execution_id = record.execution_id.as_str().to_string();
        let events = ledger.events(&record.execution_id).unwrap_or_default();
        let spend = cost_from_events(&events);

        let mut stalls = 0u32;
        let mut last_health_reason: Option<String> = None;
        for event in &events {
            if event.kind != PROVIDER_HEALTH_KIND {
                continue;
            }
            stalls += 1;
            if let Some(reason) = event.payload.get("reason").and_then(|v| v.as_str()) {
                last_health_reason = Some(reason.to_string());
            }
        }

        let parked_reason = crate::status::parked_reason(ledger, record);
        let parked = matches!(record.state, ExecutionState::Parked)
            || matches!(record.phase, ExecutionPhase::Parked);
        let degraded = stalls > 0;

        let allowance_usd = allowances.get(&execution_id).copied();
        let remaining_usd = allowance_usd.map(|allowance| allowance - spend.usd);
        let over_budget = remaining_usd.map(|r| r < 0.0).unwrap_or(false);

        let digest = latest_digest(ledger, &record.execution_id);

        for event in &events {
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
            let recorded_at = ledger
                .digests_for_execution(&record.execution_id)
                .ok()
                .and_then(|rows| rows.last().map(|row| row.recorded_at_unix_ms))
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

    // Deterministic oldest-first order: timestamp, then execution, then the
    // per-execution sequence (digest milestones carry sequence 0).
    timeline.sort_by(|a, b| {
        a.recorded_at_unix_ms
            .cmp(&b.recorded_at_unix_ms)
            .then_with(|| a.execution_id.cmp(&b.execution_id))
            .then_with(|| a.sequence.cmp(&b.sequence))
    });

    EconomicsSnapshot {
        rows,
        timeline,
        alerts,
    }
}

/// Latest recorded digest for `execution`, parsed back into the digest shape.
fn latest_digest<L: LedgerApi + ?Sized>(
    ledger: &L,
    execution: &ExecutionId,
) -> Option<DigestSummary> {
    let rows = ledger.digests_for_execution(execution).ok()?;
    let latest = rows.last()?;
    let digest = serde_json::from_value::<Digest>(latest.summary.clone()).ok()?;
    Some(DigestSummary::from(&digest))
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
    use agalma_contracts::{CommitBatch, DigestRecord, ExecutionRecord, ExecutionState, TaskId};
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
    fn no_executions_yields_an_empty_section() {
        let dir = ScratchDir::new("economics-empty");
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let snapshot = derive(&ledger, &[], &BTreeMap::new());
        assert!(snapshot.rows.is_empty());
        assert!(snapshot.timeline.is_empty());
        assert!(snapshot.alerts.is_empty());
    }
}
