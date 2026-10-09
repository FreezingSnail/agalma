//! `WorkspaceApi` — isolated checkout and integration.
//!
//! Git command details stay inside the implementation. Integration requires the
//! expected old `main` SHA (compare-and-update); `MergeReceipt` records the
//! candidate and result SHAs so a crash after merge can be distinguished from a
//! crash before completion.

use serde::{Deserialize, Serialize};

use crate::error::ContractError;
use crate::ids::ExecutionId;

/// An isolated checkout prepared for one execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub path: String,
    pub base_sha: String,
    /// Candidate branch holding the execution's build output; the git ref name
    /// is derived from the execution id (git refs forbid `:`).
    pub candidate_branch: String,
}

/// Evidence of a compare-and-update integration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReceipt {
    pub expected_main_sha: String,
    pub candidate_sha: String,
    pub result_sha: String,
}

/// Workspace contract: prepare, inspect, integrate, tag, reconcile, revert.
pub trait WorkspaceApi {
    /// Prepare an isolated checkout from a template for an execution.
    fn prepare(
        &mut self,
        template: &str,
        execution: &ExecutionId,
    ) -> Result<Checkout, ContractError>;

    /// Render the working diff for a checkout.
    fn diff(&self, checkout: &Checkout) -> Result<String, ContractError>;

    /// Merge the candidate into `main`, requiring `expected_main_sha`.
    fn integrate(
        &mut self,
        checkout: &Checkout,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError>;

    /// Tag a commit.
    fn tag(&mut self, name: &str, sha: &str) -> Result<(), ContractError>;

    /// Reconcile checkout state against recorded receipts.
    ///
    /// Reports whether the candidate is already integrated (`main` is at the
    /// candidate HEAD, which is ahead of the base) without re-merging; the
    /// caller completes any missing ledger/tag records from the receipt.
    fn reconcile(&mut self, checkout: &Checkout) -> Result<bool, ContractError>;

    /// Revert to a commit.
    fn revert(&mut self, sha: &str) -> Result<(), ContractError>;
}
