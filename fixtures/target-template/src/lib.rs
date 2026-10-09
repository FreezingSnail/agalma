//! Fixture library for the M0 hardcoded task.
//!
//! The seed is deliberately wrong (`answer()` returns 41); the builder agent
//! must make the failing acceptance test pass.

/// Returns the answer the acceptance test expects.
pub fn answer() -> u32 {
    41
}
