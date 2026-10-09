//! Agalma sandbox service.
//!
//! Owning wave: **M0.4** (`agalma-4ui.6`). Implements
//! [`agalma_contracts::SandboxApi`] on macOS Seatbelt:
//!
//! - fail-closed profile render/validation gate before any process is created;
//! - confined launch as a process-group leader via `tokio::process`;
//! - liveness and evidence-backed termination of the whole descendant tree.
//!
//! The product confine profile ships at `profiles/worker.sb` and is embedded as
//! [`PRODUCT_PROFILE`]. Attempt-scoped `TMPDIR`/`XDG_*` is the caller's job and
//! travels in `LaunchSpec::env`.

pub mod profile;
pub mod seatbelt;

pub use profile::{PRODUCT_PROFILE, SUPPLIED_PARAMS};
pub use seatbelt::{SandboxOutput, SeatbeltSandbox};
