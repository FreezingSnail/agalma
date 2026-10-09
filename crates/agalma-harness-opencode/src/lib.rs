//! Agalma OpenCode harness adapter.
//!
//! Owning wave: **M0.6** (`agalma-4ui.8`). Implements
//! `agalma_contracts::HarnessApi` (ported from the S0d spike). All vendor
//! details — `opencode serve` invocation, HTTP/SSE, auth, configuration
//! rendering, native session IDs, and the nerve plugin — are confined to this
//! crate and never leak into contracts or the conductor.
//!
//! Stub: no implementation yet. Present so the workspace builds and the
//! architecture lint can enforce the dependency direction.
