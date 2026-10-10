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

/// Abort the process when `point` matches the armed [`CRASH_ENV`]. No-op when
/// the env var is unset or names a different point.
pub fn crash_hook(point: &str) {
    if std::env::var(CRASH_ENV).as_deref() == Ok(point) {
        std::process::abort();
    }
}
