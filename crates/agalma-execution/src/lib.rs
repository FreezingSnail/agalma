//! Agalma execution service.
//!
//! Owning wave: **M0.3** (`agalma-4ui.5`). Implements
//! `agalma_contracts::ExecutionApi`: serial executor, durable operation receipts,
//! and restart reconciliation.
//!
//! The executor is generic over `LedgerApi` so this crate depends only on the
//! frozen contracts (the composition root injects the concrete SQLite ledger).
//! Per the architecture rule, no implementation crate imports another; the
//! `SqliteLedger` is wired in the conductor and exercised from this crate's
//! integration tests via a dev-dependency.
//!
//! Durability semantics and the phase machine are ported from S0c
//! (`docs/spikes/s0c-execution.md`); see [`executor`] for the contract.

pub mod activity;
pub mod executor;
pub mod scripted;

pub use activity::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, OperationContext,
};
pub use executor::{
    DispatchOutcome, Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION, LEDGER_SCHEMA_VERSION,
};
pub use scripted::ScriptedActivity;
