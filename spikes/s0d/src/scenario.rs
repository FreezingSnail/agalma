//! Scenario driver.
//!
//! This module imports the frozen contract and nothing else. It never mentions a
//! vendor, endpoint, transport, or native identifier — the privacy check
//! enforces that. The same driver runs against any concrete adapter and the
//! deterministic reference adapter; the only difference is the binding
//! configuration selected in `main`.
//!
//! Steps: start attempt → create session → run turn → read to terminal →
//! baseline event kinds + declared usage completeness → fresh session → run
//! turn → cancel mid-flight → terminal state + distinct acknowledgment →
//! close sessions → stop attempt + cleanup evidence → optional-capability
//! fallback.

use std::thread::sleep;
use std::time::Duration;

use crate::contract::*;

/// Canonical input artifact refs used by the scenario (opaque to adapters).
fn turn1_input() -> String {
    artifact_ref("turn-1-input")
}

fn turn2_input() -> String {
    artifact_ref("turn-2-input")
}

pub struct ScenarioReport {
    pub binding_id: String,
    pub impl_name: String,
    pub impl_version: String,
    pub capabilities: Vec<String>,
    pub events: Vec<String>,
    pub checks: Vec<(String, bool)>,
    pub fallback: Option<String>,
    pub cleanup: String,
    pub error: Option<String>,
}

impl ScenarioReport {
    pub fn pass(&self) -> bool {
        self.error.is_none() && self.checks.iter().all(|(_, ok)| *ok)
    }

    pub fn event_kinds(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| e.split_whitespace().next().unwrap_or("").to_string())
            .collect()
    }
}

fn poll_terminal(
    adapter: &mut dyn HarnessAdapter,
    op: &OperationHandle,
    max_iters: u32,
    sleep_ms: u64,
) -> Result<OperationState, HarnessError> {
    let mut last = OperationState::Running;
    for _ in 0..max_iters {
        last = adapter.inspect_operation(op)?;
        if last.is_terminal() {
            return Ok(last);
        }
        sleep(Duration::from_millis(sleep_ms));
    }
    Ok(last)
}

pub fn run(adapter: &mut dyn HarnessAdapter, binding_id: &str, workspace: &str) -> ScenarioReport {
    let describe = adapter.describe();
    let mut checks: Vec<(String, bool)> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    let mut fallback: Option<String> = None;
    let mut cleanup = String::new();
    let mut error: Option<String> = None;

    let finish = |adapter: &dyn HarnessAdapter,
                  checks: Vec<(String, bool)>,
                  events: Vec<Event>,
                  fallback: Option<String>,
                  cleanup: String,
                  error: Option<String>| {
        let d = adapter.describe();
        ScenarioReport {
            binding_id: binding_id.to_string(),
            impl_name: d.impl_name,
            impl_version: d.impl_version,
            capabilities: d.capabilities.iter().cloned().collect(),
            events: events.iter().map(Event::render).collect(),
            checks,
            fallback,
            cleanup,
            error,
        }
    };

    // --- StartAttempt -------------------------------------------------------
    let attempt = match adapter.start_attempt(StartAttemptRequest {
        workspace: workspace.to_string(),
        role: "builder".to_string(),
        genome_ref: artifact_ref("genome"),
        constraints_ref: artifact_ref("platform-constraints"),
        limits: Limits { wall_ms: 120_000, tokens: 20_000, cost_micros: 0 },
        operation_id: operation_id(1, 0),
    }) {
        Ok(a) => a,
        Err(e) => {
            checks.push(("start_attempt".to_string(), false));
            error = Some(format!("start_attempt: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };
    checks.push(("start_attempt".to_string(), true));

    // --- session 1 + turn 1 -------------------------------------------------
    let session1 = match adapter.create_session(CreateSessionRequest {
        role: "builder".to_string(),
        model: "role-default".to_string(),
        prompts_ref: artifact_ref("builder-prompts"),
        handoff_refs: vec![artifact_ref("handoff")],
        tool_policy_ref: artifact_ref("tool-policy"),
    }) {
        Ok(s) => s,
        Err(e) => {
            checks.push(("create_session".to_string(), false));
            error = Some(format!("create_session: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };
    checks.push(("create_session".to_string(), true));

    let op1 = match adapter.run_turn(
        &session1,
        RunTurnRequest { input_ref: turn1_input(), bounded_turns: 1, deadline_ms: 60_000 },
    ) {
        Ok(o) => o,
        Err(e) => {
            checks.push(("run_turn".to_string(), false));
            error = Some(format!("run_turn: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };

    let state1 = match poll_terminal(adapter, &op1, 150, 200) {
        Ok(s) => s,
        Err(e) => {
            checks.push(("turn1_terminal".to_string(), false));
            error = Some(format!("inspect turn1: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };
    checks.push(("turn1_terminal".to_string(), state1.is_terminal()));

    match adapter.read_events(&op1) {
        Ok(evs) => events.extend(evs),
        Err(e) => {
            checks.push(("read_events".to_string(), false));
            error = Some(format!("read_events turn1: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    }

    let kinds: Vec<&str> = events.iter().map(Event::kind).collect();
    let baseline_kinds = kinds.contains(&"TurnStarted")
        && kinds.contains(&"UsageSnapshot")
        && kinds.contains(&"TurnCompleted");
    checks.push(("baseline_event_kinds".to_string(), baseline_kinds));

    let usage_declared = events.iter().any(|e| {
        matches!(e, Event::UsageSnapshot { usage } if usage.completeness == Completeness::Complete)
    });
    checks.push(("usage_completeness_declared".to_string(), usage_declared));

    // --- session 2 + cancelled turn ----------------------------------------
    // Phase isolation: a fresh session keeps the turn's terminal state
    // unambiguous at session scope. Recorded as a deviation in Results.
    let session2 = match adapter.create_session(CreateSessionRequest {
        role: "builder".to_string(),
        model: "role-default".to_string(),
        prompts_ref: artifact_ref("builder-prompts"),
        handoff_refs: vec![],
        tool_policy_ref: artifact_ref("tool-policy"),
    }) {
        Ok(s) => s,
        Err(e) => {
            checks.push(("create_session_cancel".to_string(), false));
            error = Some(format!("create_session cancel: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };

    let op2 = match adapter.run_turn(
        &session2,
        RunTurnRequest { input_ref: turn2_input(), bounded_turns: 1, deadline_ms: 60_000 },
    ) {
        Ok(o) => o,
        Err(e) => {
            checks.push(("run_turn_cancel".to_string(), false));
            error = Some(format!("run_turn cancel: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };

    // Read once mid-flight, then cancel between events.
    let mid = adapter.read_events(&op2).unwrap_or_default();
    checks.push(("cancel_read_midflight".to_string(), !mid.is_empty()));

    let ack = match adapter.cancel_operation(&op2) {
        Ok(a) => a,
        Err(e) => {
            checks.push(("cancel_operation".to_string(), false));
            error = Some(format!("cancel_operation: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };
    checks.push(("cancel_acknowledged".to_string(), ack.acknowledged));

    let state2 = match poll_terminal(adapter, &op2, 100, 150) {
        Ok(s) => s,
        Err(e) => {
            checks.push(("cancel_terminal".to_string(), false));
            error = Some(format!("inspect cancel: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    };
    checks.push(("cancel_terminal".to_string(), state2.is_terminal()));
    // Acknowledgment is a distinct fact from confirmed termination: the ack is
    // reported before/independent of the terminal state and cleanup evidence.
    checks.push((
        "cancel_ack_distinct_from_termination".to_string(),
        ack.acknowledged && !ack.detail.is_empty() && state2.is_terminal(),
    ));

    match adapter.read_events(&op2) {
        Ok(evs) => events.extend(evs),
        Err(e) => {
            checks.push(("read_events_cancel".to_string(), false));
            error = Some(format!("read_events cancel: {e}"));
            return finish(adapter, checks, events, fallback, cleanup, error);
        }
    }

    // --- optional capability fallback --------------------------------------
    // Run while the attempt is still live: the fallback plan creates a real
    // session.
    let requested = CAP_SYNTHETIC_FEEDBACK;
    if describe.capabilities.contains(requested) {
        // Capability present: the conductor would use the direct plan.
        checks.push(("capability_direct".to_string(), true));
    } else {
        // Declared supported plan: fresh session + explicit handoff note.
        match adapter.create_session(CreateSessionRequest {
            role: "builder".to_string(),
            model: "role-default".to_string(),
            prompts_ref: artifact_ref("fallback-prompt"),
            handoff_refs: vec![artifact_ref("handoff-note")],
            tool_policy_ref: artifact_ref("tool-policy"),
        }) {
            Ok(s) => {
                let _ = adapter.close_session(&s);
                fallback = Some(format!("fallback:{requested} -> fresh_session+handoff"));
                checks.push(("capability_fallback".to_string(), true));
            }
            Err(e) => {
                checks.push(("capability_fallback".to_string(), false));
                error = Some(format!("capability fallback: {e}"));
            }
        }
    }
    checks.push((
        "required_capability_set_empty".to_string(),
        mvp_baseline_required().is_empty(),
    ));

    // --- close + stop -------------------------------------------------------
    let _ = adapter.close_session(&session1);
    let _ = adapter.close_session(&session2);

    match adapter.stop_attempt(&attempt) {
        Ok(evidence) => {
            checks.push(("cleanup_process_group_gone".to_string(), evidence.process_group_gone));
            cleanup = evidence.detail;
        }
        Err(e) => {
            checks.push(("cleanup_process_group_gone".to_string(), false));
            error = Some(format!("stop_attempt: {e}"));
        }
    }

    finish(adapter, checks, events, fallback, cleanup, error)
}
