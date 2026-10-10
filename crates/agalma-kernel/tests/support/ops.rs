//! B0.3 test doubles for the activity protocol and decision integration.
//!
//! - [`JournalHandler`] adapts the frozen harness [`FakeActivity`] to the kernel
//!   [`ActivityHandler`] seam, so recovery tests observe effect vs. replay
//!   through the append-only effect journal.
//! - [`BindingRecordingHandler`] additionally records the binding pin each
//!   reconciliation ran under (pinned-binding recovery).
//! - [`ScriptedHandler`] returns a scripted probe (for the ambiguous-park path).
//! - [`CountingDecider`] is a durable [`DecisionApi`] double that counts
//!   `decide` invocations and actual inference resolutions, so a test can prove
//!   recovery reuses a recorded outcome without a second inference call.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use agalma_contracts::{
    ArtifactRef, ContractError, DecisionApi, DecisionAttempt, DecisionOutcome, DecisionRequest,
    FallbackReason, LedgerApi,
};
use agalma_execution::EffectProbe as ExecProbe;
use agalma_kernel::{
    ActivityContext, ActivityDef, ActivityError, ActivityHandler, ActivityOutcome, BindingPin,
    EffectProbe,
};
use agalma_ledger::SqliteLedger;

use super::harness::{EffectJournal, FakeActivity};

fn outcome_for(ctx: &ActivityContext) -> ActivityOutcome {
    ActivityOutcome::new(ArtifactRef::derive(&format!(
        "{}:outcome",
        ctx.operation_id.as_str()
    )))
}

/// Adapt a harness [`FakeActivity`] to the kernel [`ActivityHandler`] seam.
pub struct JournalHandler<A: FakeActivity> {
    def: ActivityDef,
    inner: A,
}

impl<A: FakeActivity> JournalHandler<A> {
    pub fn new(def: ActivityDef, inner: A) -> Self {
        JournalHandler { def, inner }
    }

    pub fn inner(&self) -> &A {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut A {
        &mut self.inner
    }
}

impl<A: FakeActivity> ActivityHandler for JournalHandler<A> {
    fn def(&self) -> ActivityDef {
        self.def.clone()
    }

    fn execute(&mut self, ctx: &ActivityContext) -> Result<ActivityOutcome, ActivityError> {
        self.inner
            .execute(&ctx.operation_id)
            .map_err(ActivityError::KnownFailure)?;
        Ok(outcome_for(ctx))
    }

    fn reconcile(&mut self, ctx: &ActivityContext) -> Result<EffectProbe, ActivityError> {
        match self.inner.effect_present(&ctx.operation_id) {
            ExecProbe::Present => {
                self.inner
                    .reconcile(&ctx.operation_id)
                    .map_err(ActivityError::KnownFailure)?;
                Ok(EffectProbe::Present(outcome_for(ctx)))
            }
            ExecProbe::Absent => Ok(EffectProbe::Absent),
            ExecProbe::Ambiguous => Ok(EffectProbe::Ambiguous),
        }
    }
}

/// A [`JournalHandler`] that also records the binding pin seen on reconcile.
pub struct BindingRecordingHandler<A: FakeActivity> {
    def: ActivityDef,
    inner: A,
    seen: Rc<std::cell::RefCell<Vec<Option<String>>>>,
}

impl<A: FakeActivity> BindingRecordingHandler<A> {
    pub fn new(def: ActivityDef, inner: A) -> (Self, Rc<std::cell::RefCell<Vec<Option<String>>>>) {
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        (
            BindingRecordingHandler {
                def,
                inner,
                seen: seen.clone(),
            },
            seen,
        )
    }
}

impl<A: FakeActivity> ActivityHandler for BindingRecordingHandler<A> {
    fn def(&self) -> ActivityDef {
        self.def.clone()
    }

    fn execute(&mut self, ctx: &ActivityContext) -> Result<ActivityOutcome, ActivityError> {
        self.inner
            .execute(&ctx.operation_id)
            .map_err(ActivityError::KnownFailure)?;
        Ok(outcome_for(ctx))
    }

    fn reconcile(&mut self, ctx: &ActivityContext) -> Result<EffectProbe, ActivityError> {
        self.seen
            .borrow_mut()
            .push(ctx.binding.as_ref().map(binding_label));
        match self.inner.effect_present(&ctx.operation_id) {
            ExecProbe::Present => {
                self.inner
                    .reconcile(&ctx.operation_id)
                    .map_err(ActivityError::KnownFailure)?;
                Ok(EffectProbe::Present(outcome_for(ctx)))
            }
            ExecProbe::Absent => Ok(EffectProbe::Absent),
            ExecProbe::Ambiguous => Ok(EffectProbe::Ambiguous),
        }
    }
}

/// `implementation:version` label of a binding pin.
pub fn binding_label(pin: &BindingPin) -> String {
    format!("{}:{}", pin.implementation, pin.implementation_version)
}

/// A handler whose `reconcile` always returns a scripted probe.
pub struct ScriptedHandler {
    def: ActivityDef,
    probe: EffectProbe,
    journal: EffectJournal,
}

impl ScriptedHandler {
    pub fn new(def: ActivityDef, probe: EffectProbe, journal: EffectJournal) -> Self {
        ScriptedHandler {
            def,
            probe,
            journal,
        }
    }
}

impl ActivityHandler for ScriptedHandler {
    fn def(&self) -> ActivityDef {
        self.def.clone()
    }

    fn execute(&mut self, ctx: &ActivityContext) -> Result<ActivityOutcome, ActivityError> {
        self.journal
            .record(ctx.operation_id.as_str(), "execute", "applied")
            .map_err(ActivityError::KnownFailure)?;
        Ok(outcome_for(ctx))
    }

    fn reconcile(&mut self, _ctx: &ActivityContext) -> Result<EffectProbe, ActivityError> {
        Ok(self.probe.clone())
    }
}

/// A durable [`DecisionApi`] double over a sqlite ledger.
///
/// `decide_calls` counts every `decide` invocation; `inference_calls` counts only
/// the resolutions that actually produced a fresh outcome (a recorded outcome is
/// reused without incrementing it). Both counters are shared with the test.
pub struct CountingDecider {
    ledger: SqliteLedger,
    decide_calls: Rc<Cell<u32>>,
    inference_calls: Rc<Cell<u32>>,
}

impl CountingDecider {
    /// Open a decider over `path`; returns shared counters.
    pub fn open(path: &Path) -> (Self, Rc<Cell<u32>>, Rc<Cell<u32>>) {
        let ledger = SqliteLedger::open(path).expect("open decider ledger");
        let decide_calls = Rc::new(Cell::new(0));
        let inference_calls = Rc::new(Cell::new(0));
        (
            CountingDecider {
                ledger,
                decide_calls: decide_calls.clone(),
                inference_calls: inference_calls.clone(),
            },
            decide_calls,
            inference_calls,
        )
    }
}

impl DecisionApi for CountingDecider {
    fn decide(
        &mut self,
        request: &DecisionRequest,
        attempt: DecisionAttempt,
    ) -> Result<DecisionOutcome, ContractError> {
        self.decide_calls.set(self.decide_calls.get() + 1);
        // Persist the request before any apply (changed inputs -> conflict).
        self.ledger.record_decision(request)?;
        // A recorded outcome is terminal: reuse without a second inference.
        if let Some(existing) = self.ledger.decision_outcome(&request.operation_id)? {
            return Ok(existing);
        }
        self.inference_calls.set(self.inference_calls.get() + 1);
        let outcome = resolve(request, attempt);
        self.ledger
            .record_decision_outcome(&request.operation_id, &outcome)?;
        Ok(outcome)
    }
}

/// Deterministic stand-in for the pinned static baseline.
fn resolve(request: &DecisionRequest, attempt: DecisionAttempt) -> DecisionOutcome {
    let first = request.options.first().map(|o| o.option_id.clone());
    match attempt {
        DecisionAttempt::Response(response) => match response.chosen {
            Some(chosen) if request.offers(&chosen) => DecisionOutcome::Applied {
                chosen: Some(chosen),
                scores: response.scores,
            },
            _ => baseline_or_park(request, Some(FallbackReason::Malformed)),
        },
        DecisionAttempt::Baseline => match first {
            Some(chosen) => DecisionOutcome::Applied {
                chosen: Some(chosen),
                scores: vec![],
            },
            None => DecisionOutcome::Parked {
                reason: "no eligible options".to_string(),
            },
        },
        _ => baseline_or_park(request, Some(FallbackReason::Unavailable)),
    }
}

fn baseline_or_park(request: &DecisionRequest, reason: Option<FallbackReason>) -> DecisionOutcome {
    match request.options.first() {
        Some(option) => DecisionOutcome::Fallback {
            baseline: "counting/v1".to_string(),
            reason: reason.unwrap_or(FallbackReason::Unavailable),
            chosen: Some(option.option_id.clone()),
            scores: vec![],
        },
        None => DecisionOutcome::Parked {
            reason: "baseline cannot act: no eligible options".to_string(),
        },
    }
}
