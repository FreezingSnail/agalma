//! Pure translation from OpenCode payloads to the canonical harness contract.
//!
//! Everything here is vendor-shape aware but side-effect free: it takes the
//! decoded JSON of `GET /api/session/{id}` (the durable session projection) and
//! the durable session log, and produces `agalma_contracts` types. No network,
//! no process, so the mapping is unit-tested against recorded payload fixtures
//! (`tests/fixtures/`).
//!
//! Reconciliation choice (S0d): the session projection is authoritative for
//! terminal state and usage; the durable log contributes a best-effort tool
//! stream and the `log.synced` marker. SSE is not parsed.

use agalma_contracts::{ArtifactRef, Completeness, Event, OperationState, ToolResult, Usage};
use serde_json::Value;

/// The artifact reference returned for a completed turn. Stable for M0: the
/// turn result is the session transcript, addressed by a fixed canonical name.
pub(crate) const TURN_RESULT: &str = "turn-result";

/// Usage from the session projection. `fallback` is the terminal completeness
/// when the projection carries token data; absent token data is `Unknown`, never
/// a silent `Complete`.
pub(crate) fn usage_from_session(session: &Value, fallback: Completeness) -> Usage {
    let data = session.get("data");
    let tokens = data.and_then(|d| d.get("tokens"));
    let completeness = if tokens.is_some() {
        fallback
    } else {
        Completeness::Unknown
    };
    let tokens_in = tokens
        .and_then(|t| t.get("input"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens_out = tokens
        .and_then(|t| t.get("output"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cost_usd = data
        .and_then(|d| d.get("cost"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Usage {
        tokens_in,
        tokens_out,
        cost_usd,
        completeness,
    }
}

/// Operation state from `GET /api/session/{id}`.
///
/// The projection aggregates `outcome` at session scope: `succeeded` is a
/// completed turn; any other non-empty value (e.g. `interrupted`, `error`) is a
/// terminal failure; absent/empty means the turn is still running.
pub(crate) fn state_from_session(session: &Value) -> OperationState {
    let outcome = session
        .pointer("/data/outcome")
        .and_then(Value::as_str)
        .unwrap_or("");
    match outcome {
        "succeeded" => OperationState::Completed {
            result_ref: ArtifactRef::derive(TURN_RESULT),
            usage: usage_from_session(session, Completeness::Complete),
        },
        other if !other.is_empty() => OperationState::Failed {
            reason: other.to_string(),
        },
        _ => OperationState::Running,
    }
}

/// Normalize a session projection plus an optional durable-log body. The log
/// contributes `ToolOutcome` events (best-effort) before the terminal events.
pub(crate) fn events_from(session: &Value, log: Option<&str>) -> Vec<Event> {
    let mut events = vec![Event::TurnStarted];
    if let Some(body) = log {
        events.extend(tool_events_from_log(body));
    }
    match state_from_session(session) {
        OperationState::Completed { result_ref, usage } => {
            events.push(Event::UsageSnapshot { usage });
            events.push(Event::TurnCompleted { result_ref });
        }
        OperationState::Failed { reason } => {
            let usage = usage_from_session(session, Completeness::Partial);
            events.push(Event::UsageSnapshot { usage });
            events.push(Event::TurnFailed { reason });
        }
        _ => {}
    }
    events
}

/// Whether the durable log body carries the sync marker.
pub(crate) fn log_synced(body: &str) -> bool {
    body.contains("log.synced")
}

/// Best-effort `ToolOutcome` extraction from the durable session log.
///
/// The log is SSE-framed (`data: {json}` per line); only frames whose `type`
/// names a tool terminal event are translated. Unknown frames are ignored, so a
/// format change degrades to "no tool events", never a failure.
pub(crate) fn tool_events_from_log(body: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for line in body.lines() {
        let payload = line.trim();
        let payload = payload
            .strip_prefix("data:")
            .map(str::trim)
            .unwrap_or(payload);
        if !payload.starts_with('{') {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        let Some(kind) = json.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !kind.contains("tool") {
            continue;
        }
        let tool = json
            .pointer("/data/tool")
            .or_else(|| json.pointer("/data/name"))
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_string();
        let detail = json
            .pointer("/data/callID")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if kind.ends_with("failed") || kind.contains("error") {
            out.push(Event::ToolOutcome {
                tool,
                outcome: ToolResult::Error,
                detail,
            });
        } else if kind.ends_with("success") || kind.ends_with("completed") {
            out.push(Event::ToolOutcome {
                tool,
                outcome: ToolResult::Ok,
                detail,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded `GET /api/session/{id}` from the S0d probe run (`ses_ede405cf…`):
    /// turn admitted, no outcome yet, zeroed token counters.
    const RUNNING: &str = include_str!("../tests/fixtures/session_running.json");
    /// `outcome:"succeeded"` with usage from the S0d OpenCode transcript.
    const SUCCEEDED: &str = include_str!("../tests/fixtures/session_succeeded.json");
    /// `outcome:"interrupted"`: the cancelled-turn terminal state.
    const INTERRUPTED: &str = include_str!("../tests/fixtures/session_interrupted.json");
    /// A succeeded session whose projection carries no `tokens` object.
    const NO_USAGE: &str = include_str!("../tests/fixtures/session_no_usage.json");
    /// A durable log fragment with tool terminal frames and the sync marker.
    const LOG_TOOLS: &str = include_str!("../tests/fixtures/log_tools.txt");

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture is valid json")
    }

    #[test]
    fn running_session_is_not_terminal_and_has_only_turn_started() {
        let events = events_from(&parse(RUNNING), None);
        assert_eq!(events, vec![Event::TurnStarted]);
    }

    #[test]
    fn succeeded_session_is_terminal_complete() {
        let state = state_from_session(&parse(SUCCEEDED));
        match &state {
            OperationState::Completed { result_ref, usage } => {
                assert_eq!(result_ref, &ArtifactRef::derive("turn-result"));
                assert_eq!(usage.tokens_in, 2964);
                assert_eq!(usage.tokens_out, 6);
                assert_eq!(usage.completeness, Completeness::Complete);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let events = events_from(&parse(SUCCEEDED), None);
        assert_eq!(
            events.iter().map(Event::kind).collect::<Vec<_>>(),
            vec!["TurnStarted", "UsageSnapshot", "TurnCompleted"]
        );
    }

    #[test]
    fn interrupted_session_is_terminal_failed_partial() {
        let state = state_from_session(&parse(INTERRUPTED));
        match &state {
            OperationState::Failed { reason } => assert_eq!(reason, "interrupted"),
            other => panic!("expected Failed, got {other:?}"),
        }
        let events = events_from(&parse(INTERRUPTED), None);
        assert_eq!(
            events.iter().map(Event::kind).collect::<Vec<_>>(),
            vec!["TurnStarted", "UsageSnapshot", "TurnFailed"]
        );
        assert!(matches!(
            &events[1],
            Event::UsageSnapshot { usage } if usage.completeness == Completeness::Partial
        ));
    }

    #[test]
    fn missing_tokens_declare_unknown_not_silent_zero() {
        let usage = usage_from_session(&parse(NO_USAGE), Completeness::Complete);
        assert_eq!(usage.completeness, Completeness::Unknown);
        assert_eq!(usage.tokens_in, 0);
    }

    #[test]
    fn log_sync_marker_detected() {
        assert!(log_synced(LOG_TOOLS));
        assert!(!log_synced(""));
    }

    #[test]
    fn tool_outcomes_translated_from_log_frames() {
        let events = tool_events_from_log(LOG_TOOLS);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            Event::ToolOutcome {
                tool: "bash".to_string(),
                outcome: ToolResult::Ok,
                detail: "call_ok".to_string(),
            }
        );
        assert_eq!(
            events[1],
            Event::ToolOutcome {
                tool: "edit".to_string(),
                outcome: ToolResult::Error,
                detail: "call_err".to_string(),
            }
        );
    }

    #[test]
    fn combined_events_place_tools_before_terminal() {
        let events = events_from(&parse(SUCCEEDED), Some(LOG_TOOLS));
        assert_eq!(
            events.iter().map(Event::kind).collect::<Vec<_>>(),
            vec![
                "TurnStarted",
                "ToolOutcome",
                "ToolOutcome",
                "UsageSnapshot",
                "TurnCompleted"
            ]
        );
    }

    #[test]
    fn unknown_log_frames_are_ignored() {
        let body =
            "data: {\"type\":\"log.synced\"}\nnot json\ndata: {\"type\":\"reasoning.delta\"}\n";
        assert!(tool_events_from_log(body).is_empty());
    }
}
