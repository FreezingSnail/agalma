//! Agalma decision service.
//!
//! Owning wave: **M1.5** (`agalma-52k.6`). Implements
//! [`agalma_contracts::DecisionApi`] with deterministic static baselines and
//! durable request/outcome records over [`agalma_contracts::LedgerApi`].
//!
//! M1 runs no model inference: [`baseline`] provides the pinned static policies
//! (`triage.pick-next`, `retry.escalate`) and [`decider::StaticDecider`] applies
//! them durably, validating any candidate response and falling back per
//! architecture §3.7. The crate depends only on the frozen contracts; the
//! composition root injects the concrete ledger, so no implementation crate
//! imports another.

pub mod baseline;
pub mod decider;

pub use baseline::{
    as_outcome, baseline_name, baseline_outcome, retry_escalate, triage_pick_next, BaselineOutcome,
    RETRY_ESCALATE_BASELINE, TRIAGE_PICK_NEXT_BASELINE,
};
pub use decider::StaticDecider;
