//! Scripted `HarnessApi` implementation sharing one engine across two profiles.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agalma_contracts::harness::{
    AttemptHandle, CancelAck, Completeness, CreateSessionRequest, Describe, Event, HarnessApi,
    OperationHandle, OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest,
    StopEvidence, Usage,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId, OperationId, SessionId};
use agalma_contracts::ContractError;

use crate::scenario::{transcript, Turn, TurnScript};
use crate::{describe_for, implementation};

/// Which deterministic reference profile an adapter runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceProfile {
    /// `reference/v1`: applies the scripted change (acceptance passes).
    Success,
    /// `reference-fail/v1`: runs the same turns but leaves acceptance red.
    Red,
}

/// Wiring for a reference adapter.
#[derive(Clone, Debug)]
pub struct ReferenceHarnessConfig {
    pub profile: ReferenceProfile,
    /// Attempt-scoped directory holding `turns.json` and the turn transcripts.
    pub scenario_dir: PathBuf,
    /// Explicit scenario file; otherwise `<scenario_dir>/turns.json`.
    pub scenario: Option<PathBuf>,
}

struct RefOp {
    state: OperationState,
    events: Vec<Event>,
    cancellable: bool,
    cancelled: bool,
}

/// Deterministic scripted harness (no vendor, no network, no clock).
pub struct ReferenceHarness {
    config: ReferenceHarnessConfig,
    script: TurnScript,
    workspace: PathBuf,
    attempt: Option<AttemptHandle>,
    sessions: Vec<SessionHandle>,
    session_seq: u32,
    turn_seq: u32,
    ops: BTreeMap<String, RefOp>,
}

impl ReferenceHarness {
    /// Build an adapter; the scenario is loaded (or a deterministic default is
    /// materialized) when the first attempt starts.
    pub fn new(config: ReferenceHarnessConfig) -> Self {
        ReferenceHarness {
            config,
            script: TurnScript { turns: Vec::new() },
            workspace: PathBuf::new(),
            attempt: None,
            sessions: Vec::new(),
            session_seq: 0,
            turn_seq: 0,
            ops: BTreeMap::new(),
        }
    }

    fn scenario_path(&self) -> PathBuf {
        self.config
            .scenario
            .clone()
            .unwrap_or_else(|| self.config.scenario_dir.join("turns.json"))
    }

    fn ensure_script(&mut self) -> Result<&TurnScript, ContractError> {
        if self.script.turns.is_empty() {
            let path = self.scenario_path();
            let script = TurnScript::load(&path).unwrap_or_else(|| {
                TurnScript::default_for(self.config.profile == ReferenceProfile::Red)
            });
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ContractError::KnownFailure(format!("create scenario dir: {e}"))
                })?;
            }
            std::fs::write(&path, script.to_pretty_json())
                .map_err(|e| ContractError::KnownFailure(format!("write scenario: {e}")))?;
            self.script = script;
        }
        Ok(&self.script)
    }

    fn apply_mutation(&self, turn: &Turn) -> Result<(), ContractError> {
        let Some(mutation) = &turn.mutation else {
            return Ok(());
        };
        let path = self.workspace.join(&mutation.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ContractError::KnownFailure(format!("create {}: {e}", parent.display()))
            })?;
        }
        std::fs::write(&path, &mutation.content)
            .map_err(|e| ContractError::KnownFailure(format!("write {}: {e}", path.display())))
    }

    fn run_scripted(turn: &Turn) -> (OperationState, Vec<Event>) {
        let usage = turn.usage.unwrap_or(Usage {
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            completeness: Completeness::Partial,
        });
        let mut events = vec![Event::TurnStarted];
        let mut terminal: Option<Event> = None;
        for scripted in &turn.events {
            match scripted.to_event() {
                Some(Event::TurnStarted) => {}
                Some(ev @ Event::ToolOutcome { .. }) => events.push(ev),
                Some(ev @ Event::TurnCompleted { .. }) => terminal = Some(ev),
                Some(ev @ Event::TurnFailed { .. }) => terminal = Some(ev),
                Some(Event::UsageSnapshot { .. }) => {}
                None => {}
            }
        }
        if turn.cancel {
            return (OperationState::Running, events);
        }
        events.push(Event::UsageSnapshot { usage });
        let state = match terminal {
            Some(Event::TurnCompleted { result_ref }) => {
                OperationState::Completed { result_ref, usage }
            }
            Some(Event::TurnFailed { reason }) => OperationState::Failed { reason },
            _ => {
                if let Some(reason) = &turn.failure {
                    events.push(Event::TurnFailed {
                        reason: reason.clone(),
                    });
                    OperationState::Failed {
                        reason: reason.clone(),
                    }
                } else {
                    let result_ref = ArtifactRef::derive("turn-result");
                    events.push(Event::TurnCompleted {
                        result_ref: result_ref.clone(),
                    });
                    OperationState::Completed { result_ref, usage }
                }
            }
        };
        (state, events)
    }

    fn op_mut(&mut self, op: &OperationHandle) -> Result<&mut RefOp, ContractError> {
        self.ops.get_mut(op.id.as_str()).ok_or_else(|| {
            ContractError::KnownFailure(format!("unknown reference operation {}", op.id))
        })
    }
}

impl HarnessApi for ReferenceHarness {
    fn describe(&self) -> Describe {
        describe_for(self.config.profile)
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        self.workspace = PathBuf::from(&req.workspace);
        self.ensure_script()?;
        self.turn_seq = 0;
        let handle = AttemptHandle {
            id: AttemptId::derive(1),
        };
        self.attempt = Some(handle.clone());
        Ok(handle)
    }

    fn create_session(
        &mut self,
        _req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        self.session_seq += 1;
        let handle = SessionHandle {
            id: SessionId::new(format!("session:reference:{}/{}", 1, self.session_seq)),
        };
        self.sessions.push(handle.clone());
        Ok(handle)
    }

    fn run_turn(
        &mut self,
        _session: &SessionHandle,
        _req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        self.turn_seq += 1;
        let index = (self.turn_seq - 1) as usize;
        let turn = self.ensure_script()?.turn(index).cloned().ok_or_else(|| {
            ContractError::KnownFailure("reference scenario has no turns".to_string())
        })?;
        self.apply_mutation(&turn)?;
        let (state, events) = Self::run_scripted(&turn);
        let op = OperationHandle {
            id: OperationId::new(format!("op:reference:{}/turn/{}", 1, self.turn_seq)),
        };
        let transcript = transcript(&op.id, &events);
        std::fs::create_dir_all(&self.config.scenario_dir)
            .map_err(|e| ContractError::KnownFailure(format!("create scenario dir: {e}")))?;
        std::fs::write(
            self.config
                .scenario_dir
                .join(format!("turn-{}.txt", self.turn_seq)),
            transcript,
        )
        .map_err(|e| ContractError::KnownFailure(format!("write transcript: {e}")))?;
        self.ops.insert(
            op.id.as_str().to_string(),
            RefOp {
                cancellable: turn.cancel,
                cancelled: false,
                state,
                events,
            },
        );
        Ok(op)
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, ContractError> {
        Ok(self.op_mut(op)?.state.clone())
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        Ok(self.op_mut(op)?.events.clone())
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError> {
        let entry = self.op_mut(op)?;
        if entry.cancellable && !entry.cancelled {
            entry.cancelled = true;
            entry.events.push(Event::TurnFailed {
                reason: "cancelled".to_string(),
            });
            entry.state = OperationState::Failed {
                reason: "cancelled".to_string(),
            };
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

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), ContractError> {
        self.sessions.retain(|s| s.id != session.id);
        Ok(())
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        let path = self.config.scenario_dir.join("cleanup.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
        existing.push_str(&format!("stop_attempt {}\n", attempt.id));
        let _ = std::fs::write(&path, existing);
        self.attempt = None;
        Ok(StopEvidence {
            process_group_gone: true,
            detail: format!(
                "{}: cleanup log at {}",
                implementation(self.config.profile),
                path.display()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::harness::Limits;

    fn config(dir: &std::path::Path, profile: ReferenceProfile) -> ReferenceHarnessConfig {
        ReferenceHarnessConfig {
            profile,
            scenario_dir: dir.to_path_buf(),
            scenario: None,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-runs")
            .join(format!("reference-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn start(harness: &mut ReferenceHarness, workspace: &std::path::Path) {
        harness
            .start_attempt(StartAttemptRequest {
                workspace: workspace.to_string_lossy().into_owned(),
                role: "builder".to_string(),
                genome_ref: ArtifactRef::derive("genome").to_string(),
                constraints_ref: ArtifactRef::derive("constraints").to_string(),
                limits: Limits {
                    wall_ms: 1000,
                    tokens: 0,
                    cost_micros: 0,
                },
                operation_id: OperationId::new("op:reference:test"),
            })
            .expect("start");
    }

    #[test]
    fn success_profile_applies_mutation_and_completes() {
        let dir = scratch("success");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut harness = ReferenceHarness::new(config(&dir, ReferenceProfile::Success));
        start(&mut harness, &ws);
        let session = harness
            .create_session(CreateSessionRequest {
                role: "builder".to_string(),
                model: "reference".to_string(),
                prompts_ref: "p".to_string(),
                handoff_refs: Vec::new(),
                tool_policy_ref: "t".to_string(),
            })
            .unwrap();
        let op = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("turn"),
                    bounded_turns: 1,
                    deadline_ms: 1000,
                },
            )
            .unwrap();
        assert!(matches!(
            harness.inspect_operation(&op).unwrap(),
            OperationState::Completed { .. }
        ));
        assert!(ws.join(".agalma/reference-ok").exists(), "mutation applied");
        // Determinism: re-running the same scenario yields the same transcript.
        let first = harness.read_events(&op).unwrap();
        let second = harness.read_events(&op).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|e| e.kind()).collect::<Vec<_>>(),
            vec![
                "TurnStarted",
                "ToolOutcome",
                "UsageSnapshot",
                "TurnCompleted"
            ]
        );
    }

    #[test]
    fn red_profile_leaves_acceptance_red() {
        let dir = scratch("red");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut harness = ReferenceHarness::new(config(&dir, ReferenceProfile::Red));
        start(&mut harness, &ws);
        let session = harness
            .create_session(CreateSessionRequest {
                role: "builder".to_string(),
                model: "reference".to_string(),
                prompts_ref: "p".to_string(),
                handoff_refs: Vec::new(),
                tool_policy_ref: "t".to_string(),
            })
            .unwrap();
        let op = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("turn"),
                    bounded_turns: 1,
                    deadline_ms: 1000,
                },
            )
            .unwrap();
        assert!(matches!(
            harness.inspect_operation(&op).unwrap(),
            OperationState::Completed { .. }
        ));
        assert!(
            !ws.join(".agalma/reference-ok").exists(),
            "red profile applies no success marker"
        );
        assert!(harness.describe().capabilities.is_empty());
    }

    #[test]
    fn cancel_turn_reaches_terminal_only_after_cancel_and_stop_evidence() {
        let dir = scratch("cancel");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut harness = ReferenceHarness::new(config(&dir, ReferenceProfile::Success));
        start(&mut harness, &ws);
        let session = harness
            .create_session(CreateSessionRequest {
                role: "builder".to_string(),
                model: "reference".to_string(),
                prompts_ref: "p".to_string(),
                handoff_refs: Vec::new(),
                tool_policy_ref: "t".to_string(),
            })
            .unwrap();
        // Turn 1 completes; turn 2 is the cancellable one.
        let _ = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("turn"),
                    bounded_turns: 1,
                    deadline_ms: 1000,
                },
            )
            .unwrap();
        let cancel_op = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("turn"),
                    bounded_turns: 1,
                    deadline_ms: 1000,
                },
            )
            .unwrap();
        assert!(matches!(
            harness.inspect_operation(&cancel_op).unwrap(),
            OperationState::Running
        ));
        let ack = harness.cancel_operation(&cancel_op).unwrap();
        assert!(ack.acknowledged && ack.terminated);
        assert!(matches!(
            harness.inspect_operation(&cancel_op).unwrap(),
            OperationState::Failed { .. }
        ));
        let attempt = harness.attempt.clone().unwrap();
        let evidence = harness.stop_attempt(&attempt).unwrap();
        assert!(evidence.process_group_gone);
        assert!(dir.join("cleanup.log").exists());
    }
}
