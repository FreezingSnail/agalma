//! Deterministic reference `HarnessApi` adapter.
//!
//! The reference adapter exists to prove API independence: the same scenario
//! driver runs against it and the OpenCode adapter by changing only the binding
//! configuration. It is fully scripted — fixed events, fixed usage values, no
//! clock, no randomness, no network — so two runs produce byte-identical
//! transcripts.
//!
//! Turn 1 completes immediately with the full scripted event sequence. Turn 2
//! (the cancellation sub-scenario) emits only `TurnStarted` and stays running
//! until `cancel_operation` is called; the scenario therefore cancels "between
//! scripted events".

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::contract::*;

const IMPL_NAME: &str = "reference";
const IMPL_VERSION: &str = "s0d-reference-v1";

/// Fixed usage for the scripted successful turn.
fn fixed_usage() -> Usage {
    Usage { tokens_in: 128, tokens_out: 32, cost_usd: 0.0, completeness: Completeness::Complete }
}

struct RefOp {
    state: OperationState,
    events: Vec<Event>,
    cancel_turn: bool,
    cancelled: bool,
}

pub struct ReferenceAdapter {
    workspace: PathBuf,
    attempt: Option<AttemptHandle>,
    sessions: Vec<SessionHandle>,
    session_seq: u32,
    turn_seq: u32,
    ops: BTreeMap<String, RefOp>,
    cleanup: Vec<String>,
}

impl ReferenceAdapter {
    pub fn new(workspace: &str) -> Self {
        ReferenceAdapter {
            workspace: PathBuf::from(workspace),
            attempt: None,
            sessions: Vec::new(),
            session_seq: 0,
            turn_seq: 0,
            ops: BTreeMap::new(),
            cleanup: Vec::new(),
        }
    }

    fn cleanup_log(&self) -> PathBuf {
        self.workspace.join(".s0d-reference").join("cleanup.log")
    }

    fn record_cleanup(&mut self, line: &str) {
        self.cleanup.push(line.to_string());
        let path = self.cleanup_log();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut existing = fs::read_to_string(&path).unwrap_or_default();
        existing.push_str(line);
        existing.push('\n');
        let _ = fs::write(&path, existing);
    }

    fn op_mut(&mut self, op: &OperationHandle) -> Result<&mut RefOp, HarnessError> {
        self.ops
            .get_mut(&op.id)
            .ok_or_else(|| HarnessError::KnownFailure(format!("unknown operation {}", op.id)))
    }
}

impl HarnessAdapter for ReferenceAdapter {
    fn describe(&self) -> Describe {
        Describe {
            api: HARNESS_API.to_string(),
            api_version: API_VERSION,
            impl_name: IMPL_NAME.to_string(),
            impl_version: IMPL_VERSION.to_string(),
            // The reference adapter advertises no optional capabilities.
            capabilities: std::collections::BTreeSet::new(),
        }
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, HarnessError> {
        let handle = AttemptHandle { id: attempt_id(1) };
        self.attempt = Some(handle.clone());
        self.record_cleanup("start_attempt");
        let _ = req;
        Ok(handle)
    }

    fn create_session(&mut self, req: CreateSessionRequest) -> Result<SessionHandle, HarnessError> {
        self.session_seq += 1;
        let handle = SessionHandle { id: session_id(1, self.session_seq) };
        self.sessions.push(handle.clone());
        self.record_cleanup(&format!("create_session {}", handle.id));
        let _ = req;
        Ok(handle)
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, HarnessError> {
        self.turn_seq += 1;
        let handle = OperationHandle { id: operation_id(1, self.turn_seq) };
        let cancel_turn = self.turn_seq >= 2;
        let (state, events) = if cancel_turn {
            (OperationState::Running, vec![Event::TurnStarted])
        } else {
            (
                OperationState::Completed {
                    result_ref: artifact_ref("turn-result"),
                    usage: fixed_usage(),
                },
                vec![
                    Event::TurnStarted,
                    Event::ToolOutcome {
                        tool: "shell".to_string(),
                        outcome: ToolResult::Ok,
                        detail: "scripted-tool-ok".to_string(),
                    },
                    Event::UsageSnapshot { usage: fixed_usage() },
                    Event::TurnCompleted { result_ref: artifact_ref("turn-result") },
                ],
            )
        };
        self.ops.insert(
            handle.id.clone(),
            RefOp { state, events, cancel_turn, cancelled: false },
        );
        let _ = (session, req);
        Ok(handle)
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, HarnessError> {
        Ok(self.op_mut(op)?.state.clone())
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, HarnessError> {
        Ok(self.op_mut(op)?.events.clone())
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, HarnessError> {
        let entry = self.op_mut(op)?;
        if entry.cancel_turn && !entry.cancelled {
            entry.cancelled = true;
            entry.events.push(Event::TurnFailed { reason: "cancelled".to_string() });
            entry.state = OperationState::Failed { reason: "cancelled".to_string() };
            Ok(CancelAck {
                acknowledged: true,
                terminated: true,
                detail: "reference: cancelled between scripted events".to_string(),
            })
        } else {
            Ok(CancelAck {
                acknowledged: false,
                terminated: true,
                detail: "reference: nothing running".to_string(),
            })
        }
    }

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), HarnessError> {
        self.sessions.retain(|s| s.id != session.id);
        self.record_cleanup(&format!("close_session {}", session.id));
        Ok(())
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, HarnessError> {
        self.record_cleanup(&format!("stop_attempt {}", attempt.id));
        self.attempt = None;
        Ok(StopEvidence {
            process_group_gone: true,
            detail: "reference-cleanup-log".to_string(),
        })
    }
}
