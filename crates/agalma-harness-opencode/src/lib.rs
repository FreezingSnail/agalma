//! Agalma OpenCode harness adapter.
//!
//! Owning wave: **M0.6** (`agalma-4ui.8`). Implements
//! [`agalma_contracts::harness::HarnessApi`] (ported from the S0d spike,
//! `spikes/s0d/src/opencode.rs`). All vendor details — the `opencode serve`
//! invocation, HTTP/auth, XDG/`TMPDIR` rendering, native `ses_*` identifiers,
//! prompt text, and process management — are confined to this crate and never
//! leak into contracts or the conductor.
//!
//! Launch flows through the injected [`agalma_contracts::sandbox::SandboxApi`]
//! (the composition root supplies `agalma-sandbox`), so this crate depends only
//! on `agalma-contracts` in `src/`. See [`OpenCodeHarness`] for the runtime
//! expectation (sandbox-backed methods run inside a Tokio runtime).

mod adapter;
mod http;
mod translate;

pub use adapter::{OpenCodeHarness, OpenCodeHarnessConfig, DEFAULT_MODEL};
