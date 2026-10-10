//! Scripted turn scenarios for the reference adapters.
//!
//! A scenario is a JSON file (`turns.json`) describing the turns an attempt runs.
//! The reference adapter executes the scripted events deterministically; usage
//! is declared per turn (never a silent zero), and an optional `mutation`
//! applies a workspace change to stand in for a model editing the checkout.

use std::path::Path;

use serde::{Deserialize, Serialize};

use agalma_contracts::harness::{Completeness, Event, ToolResult, Usage};
use agalma_contracts::ids::{ArtifactRef, OperationId};

/// A whole attempt's turn script.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnScript {
    #[serde(default)]
    pub turns: Vec<Turn>,
}

/// One scripted turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    #[serde(default)]
    pub events: Vec<ScriptedEvent>,
    /// Declared usage. Absent => `Partial` with zero known values.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Workspace change applied while the turn runs.
    #[serde(default)]
    pub mutation: Option<Mutation>,
    /// A turn that stays `Running` until `cancel_operation` is called.
    #[serde(default)]
    pub cancel: bool,
    /// When set, the turn terminates `Failed { reason }`.
    #[serde(default)]
    pub failure: Option<String>,
}

/// A scripted file write inside the attempt workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mutation {
    pub path: String,
    pub content: String,
}

/// One normalized event described in a scenario file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptedEvent {
    /// One of `TurnStarted`, `ToolOutcome`, `TurnCompleted`, `TurnFailed`.
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub result_ref: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ScriptedEvent {
    /// A successful or failed tool outcome.
    pub fn tool(outcome: ToolResult, detail: &str) -> Self {
        ScriptedEvent {
            kind: "ToolOutcome".to_string(),
            tool: Some("reference-edit".to_string()),
            outcome: Some(outcome.label().to_string()),
            detail: Some(detail.to_string()),
            result_ref: None,
            reason: None,
        }
    }

    pub(crate) fn to_event(&self) -> Option<Event> {
        match self.kind.as_str() {
            "TurnStarted" => Some(Event::TurnStarted),
            "ToolOutcome" => Some(Event::ToolOutcome {
                tool: self.tool.clone().unwrap_or_default(),
                outcome: match self.outcome.as_deref() {
                    Some("Error") | Some("error") => ToolResult::Error,
                    _ => ToolResult::Ok,
                },
                detail: self.detail.clone().unwrap_or_default(),
            }),
            "TurnCompleted" => Some(Event::TurnCompleted {
                result_ref: ArtifactRef::derive(
                    self.result_ref.as_deref().unwrap_or("turn-result"),
                ),
            }),
            "TurnFailed" => Some(Event::TurnFailed {
                reason: self.reason.clone().unwrap_or_else(|| "failed".to_string()),
            }),
            _ => None,
        }
    }
}

impl TurnScript {
    /// The deterministic default script for a profile.
    pub fn default_for(red: bool) -> TurnScript {
        let (outcome, detail, mutation, usage) = if red {
            (
                ToolResult::Error,
                "reference-edit: scripted change rejected",
                Mutation {
                    path: ".agalma/reference-red".to_string(),
                    content: "reference-fail red build\n".to_string(),
                },
                Usage {
                    tokens_in: 96,
                    tokens_out: 16,
                    cost_usd: 0.0,
                    completeness: Completeness::Partial,
                },
            )
        } else {
            (
                ToolResult::Ok,
                "reference-edit: applied scripted change",
                Mutation {
                    path: ".agalma/reference-ok".to_string(),
                    content: "reference success\n".to_string(),
                },
                Usage {
                    tokens_in: 128,
                    tokens_out: 32,
                    cost_usd: 0.0,
                    completeness: Completeness::Complete,
                },
            )
        };
        TurnScript {
            turns: vec![
                Turn {
                    events: vec![ScriptedEvent::tool(outcome, detail)],
                    usage: Some(usage),
                    mutation: Some(mutation),
                    cancel: false,
                    failure: None,
                },
                // A second, cancellable turn: cancel reaches terminal only after
                // `cancel_operation`, mirroring the S0d cancellation sub-scenario.
                Turn {
                    events: Vec::new(),
                    usage: None,
                    mutation: None,
                    cancel: true,
                    failure: None,
                },
            ],
        }
    }

    /// Load a script from `path`, or return `None` when absent/unparseable.
    pub fn load(path: &Path) -> Option<TurnScript> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Serialize the script for evidence.
    pub fn to_pretty_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub(crate) fn turn(&self, index: usize) -> Option<&Turn> {
        if self.turns.is_empty() {
            return None;
        }
        self.turns.get(index).or_else(|| self.turns.last())
    }
}

/// Materialize a transcript line for one executed turn.
pub fn transcript(operation: &OperationId, events: &[Event]) -> String {
    let mut out = format!("operation {operation}\n");
    for event in events {
        out.push_str(&event.render());
        out.push('\n');
    }
    out
}
