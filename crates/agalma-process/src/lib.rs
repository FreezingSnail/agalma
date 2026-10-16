//! `agalma-process` — local process launcher.
//!
//! Infrastructure crate (like `agalma-ipc`), the in-process replacement for the
//! Seatbelt sandbox backend (`docs/sandbox-removal-design.md` §4.2). It
//! implements [`agalma_contracts::ProcessApi`] with process-group launch,
//! liveness, and evidence-backed termination, and provides a one-shot
//! [`LocalLauncher::run`] whose exit code is the verify-phase verdict.
//!
//! There is no confinement profile, no `sandbox-exec`, and no `preflight`: the
//! only external tools are `/bin/kill` and `/usr/bin/pgrep`, which exist on
//! macOS and Linux.

pub mod local;

pub use local::{LocalLauncher, ProcessOutput};
