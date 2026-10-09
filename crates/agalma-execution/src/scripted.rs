//! Scripted activity: a file-backed test double for [`Activity`].
//!
//! The "external effect" is an append-and-sync line in a log file, exactly like
//! the S0c fake: it stands in for Git refs, provider calls, and merges. Tests
//! script the outcome per `(phase, attempt)` and can force a probe result to
//! exercise the ambiguous-effect park path. This type is public so conductor
//! integration tests (M0.8) can reuse it.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use agalma_contracts::ExecutionPhase;
use serde_json::json;

use crate::activity::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, OperationContext,
};

/// File-backed scripted activity.
pub struct ScriptedActivity {
    effects_path: PathBuf,
    script: Vec<(ExecutionPhase, u32, ActivityOutcome)>,
    probes: Vec<(ExecutionPhase, u32, EffectProbe)>,
    default: ActivityOutcome,
}

impl ScriptedActivity {
    /// Create an activity whose effect log is `effects_path`.
    ///
    /// The default outcome advances the pipeline and records
    /// `{"status":"executed"}`.
    pub fn new(effects_path: impl Into<PathBuf>) -> Self {
        ScriptedActivity {
            effects_path: effects_path.into(),
            script: Vec::new(),
            probes: Vec::new(),
            default: ActivityOutcome {
                result: json!({ "status": "executed" }),
                next: ActivityNext::Advance,
            },
        }
    }

    /// Script the outcome for a `(phase, attempt)` pair.
    pub fn script(&mut self, phase: ExecutionPhase, attempt: u32, outcome: ActivityOutcome) {
        self.script.push((phase, attempt, outcome));
    }

    /// Force the probe result for a `(phase, attempt)` pair.
    pub fn probe(&mut self, phase: ExecutionPhase, attempt: u32, probe: EffectProbe) {
        self.probes.push((phase, attempt, probe));
    }

    /// Set the default outcome used when no script entry matches.
    pub fn default_outcome(&mut self, outcome: ActivityOutcome) {
        self.default = outcome;
    }

    /// Path of the effect log.
    pub fn effects_path(&self) -> &Path {
        &self.effects_path
    }

    /// Number of recorded effects.
    pub fn effect_count(&self) -> usize {
        std::fs::read_to_string(&self.effects_path)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// Whether `operation_id` has a recorded effect.
    pub fn effect_present_for(&self, operation_id: &str) -> bool {
        effect_present(&self.effects_path, operation_id)
    }

    fn outcome_for(&self, phase: ExecutionPhase, attempt: u32) -> ActivityOutcome {
        self.script
            .iter()
            .find(|(p, a, _)| *p == phase && *a == attempt)
            .map(|(_, _, o)| o.clone())
            .unwrap_or_else(|| self.default.clone())
    }

    fn append_effect(&self, operation_id: &str) -> Result<(), ActivityError> {
        if effect_present(&self.effects_path, operation_id) {
            return Ok(());
        }
        if let Some(parent) = self.effects_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ActivityError::KnownFailure(format!("create effects dir: {e}")))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.effects_path)
            .map_err(|e| ActivityError::KnownFailure(format!("open effects log: {e}")))?;
        writeln!(f, "{operation_id}")
            .map_err(|e| ActivityError::KnownFailure(format!("append effect: {e}")))?;
        f.sync_all()
            .map_err(|e| ActivityError::KnownFailure(format!("sync effect: {e}")))?;
        Ok(())
    }
}

fn effect_present(path: &Path, operation_id: &str) -> bool {
    match std::fs::read_to_string(path) {
        Ok(s) => s.lines().any(|l| l.trim() == operation_id),
        Err(_) => false,
    }
}

impl Activity for ScriptedActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.append_effect(ctx.operation_id.as_str())?;
        Ok(self.outcome_for(ctx.phase, ctx.attempt))
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        if let Some((_, _, probe)) = self
            .probes
            .iter()
            .find(|(p, a, _)| *p == ctx.phase && *a == ctx.attempt)
        {
            return *probe;
        }
        if effect_present(&self.effects_path, ctx.operation_id.as_str()) {
            EffectProbe::Present
        } else {
            EffectProbe::Absent
        }
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Ok(self.outcome_for(ctx.phase, ctx.attempt))
    }
}
