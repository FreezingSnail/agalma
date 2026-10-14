//! Crash-injection hook for the kernel's persistence/effect boundaries.
//!
//! Reuses the established `AGALMA_*_CRASH_AT` convention: a child test process
//! sets [`CRASH_ENV`] to a named point, the engine calls [`crash_hook`] at every
//! persistence/effect boundary, and the matching call aborts the process
//! (`SIGABRT`). Point names mirror `crates/agalma-kernel/tests/support/harness.rs`
//! so recovery tests arm the frozen harness constants against this engine.

/// Environment variable a child process sets to a crash point name.
pub const CRASH_ENV: &str = "AGALMA_KERNEL_CRASH_AT";

/// After the transition commit returned, before dispatch (fixture case 1).
pub const AFTER_TRANSITION_COMMIT: &str = "after_transition_commit";
/// After a cancellation request's durable settlement intent committed, before
/// the `Cancel` transition/cleanup runs (SF.4 crash window).
pub const AFTER_CANCEL_REQUEST: &str = "after_cancel_request";
/// Immediately before handing work to an activity handler.
pub const BEFORE_DISPATCH: &str = "before_dispatch";
/// After a handler returned, before the completion record.
pub const AFTER_DISPATCH: &str = "after_dispatch";
/// After the external effect landed, before completion (fixture cases 2/4).
pub const AFTER_EFFECT_BEFORE_COMPLETION: &str = "after_effect_before_completion";
/// After the decision/fallback outcome commit, before dependent work (case 3).
pub const AFTER_DECISION_COMMIT: &str = "after_decision_commit";
/// After an integrate/merge effect landed, before its completion record (case 4).
pub const AFTER_MERGE_BEFORE_COMPLETION: &str = "after_merge_before_completion";
/// After a retry deadline row committed, before it fires (fixture case 6).
pub const AFTER_TIMER_COMMIT: &str = "after_timer_commit";
/// After a directive version row committed, before its transition (case 7).
pub const AFTER_DIRECTIVE_COMMIT: &str = "after_directive_commit";

/// Abort the process when `point` matches the armed [`CRASH_ENV`]. No-op when
/// the env var is unset or names a different point.
pub fn crash_hook(point: &str) {
    if std::env::var(CRASH_ENV).as_deref() == Ok(point) {
        std::process::abort();
    }
}
