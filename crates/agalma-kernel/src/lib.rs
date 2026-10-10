//! Agalma durable execution kernel.
//!
//! Owning wave: **B0** (`agalma-9n4`). Design: `docs/b0-kernel.md`.
//!
//! B0.2 (`agalma-9n4.3`) implements the kernel model, history, and pure
//! transition function against the frozen [`agalma_contracts::ExecutionApi`],
//! persisting only through [`agalma_contracts::LedgerApi`]. This crate is the
//! home for that implementation; today it intentionally contains **no kernel
//! logic** — it exists so the protected recovery fixture pack
//! (`fixtures/recovery-pack/`) and the crash harness
//! (`crates/agalma-kernel/tests/support/`) have a stable crate to attach to.
//!
//! B0.1 (`agalma-9n4.2`) delivers:
//!
//! - `fixtures/recovery-pack/` — one JSON case per acceptance case in
//!   `docs/b0-kernel.md` §"Protected recovery fixture pack" and
//!   `docs/durable-execution-bootstrap.md` §"Acceptance and evaluation",
//!   integrity-pinned by `MANIFEST.sha256`.
//! - `tests/pack_integrity.rs` — recomputes the manifest hashes and fails on any
//!   change, addition, or removal. Candidates cannot edit the tests that judge
//!   them.
//! - `tests/support/` — the crash-injection hook convention, the fake-activity
//!   effect-journal contract, and the isolated-ledger helper that B0.2+ tests
//!   consume unchanged.
//!
//! `agalma-execution` (the M1 seed executor) and this crate satisfy the same
//! `ExecutionApi`; the conductor selects the binding (S0d binding records +
//! staged rebind), so activation and rollback are config flips with pinned
//! recovery.
//!
//! ## B0.2 surface (`agalma-9n4.3`)
//!
//! - [`exec_def`] — versioned `ExecDef` (`exec_def/v1`): named states, per-op
//!   lifecycle, and the eight transition [`Command`]s.
//! - [`transition`] — pure `transition(state, command, inputs) -> Vec<Effect>`
//!   (`Effect::{Commit, Dispatch}`) plus [`step`]/[`detect_conflict`], with no
//!   I/O, clock, model, or tool calls.
//! - [`state`] — [`KernelState`] and its coarse projection; the reducer
//!   reconstructs it from history alone.
//! - [`engine`] — [`KernelEngine`] appends `execution_events` + updates the
//!   `executions` projection in one atomic `LedgerApi` commit, replays history,
//!   and parks unknown `exec_def` versions or projection mismatches.

pub mod crash;
pub mod engine;
pub mod exec_def;
pub mod state;
pub mod transition;

pub use engine::{replay, ApplyOutcome, KernelEngine};
pub use exec_def::{
    Command, ExecDef, ExecState, Inputs, KernelEvent, OperationRecord, OperationState, Probe,
    EXEC_DEF_VERSION, KERNEL_VERSION,
};
pub use state::KernelState;
pub use transition::{detect_conflict, step, transition, Conflict, Effect, StepResult};

// Re-export the frozen contract crate the kernel implements against, so the
// dependency is explicit.
pub use agalma_contracts as contracts;
