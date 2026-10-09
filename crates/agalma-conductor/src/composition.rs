//! Composition root: wires implementations behind Agalma contracts.
//!
//! Dependency direction is `conductor -> implementations -> contracts`. Each
//! implementation crate is constructed here and handed to the conductor as a
//! `dyn` contract handle; no implementation is imported by another
//! implementation. The underscore imports below keep those dependency edges
//! explicit while the crates are still building stubs; the construction sites
//! are TODO for the owning waves.

use agalma_contracts::ContractError;
use agalma_execution as _;
use agalma_harness_opencode as _;
use agalma_ledger as _;
use agalma_sandbox as _;
use agalma_workspace as _;

/// The wired service graph for one conductor process.
///
/// Each field is a contract handle, never a concrete implementation type, so
/// that components remain replaceable.
pub struct Composition {
    // TODO(M0.2): ledger: Box<dyn agalma_contracts::LedgerApi>,
    // TODO(M0.3): execution: Box<dyn agalma_contracts::ExecutionApi>,
    // TODO(M0.4): sandbox: Box<dyn agalma_contracts::SandboxApi>,
    // TODO(M0.5): workspace: Box<dyn agalma_contracts::WorkspaceApi>,
    // TODO(M0.6): harness: Box<dyn agalma_contracts::HarnessApi>,
}

impl Composition {
    /// Build the full service graph.
    ///
    /// TODO(M0.2–M0.6): construct each implementation from resolved config and
    /// inject it as a contract handle. Until the waves land, the graph is
    /// deliberately unavailable rather than partially wired.
    pub fn build() -> Result<Composition, ContractError> {
        Err(ContractError::UnsupportedCapability(
            "composition root is not wired until M0.2-M0.6".to_string(),
        ))
    }
}
