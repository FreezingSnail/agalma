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

/// How one candidate path changed against the recorded base commit (SF.6).
///
/// The workspace reports the raw change set; scope authorization (allowlist
/// versus protected denylist) is decided above the transport
/// (`scope_guard`), not here. Renames carry both sides so a candidate cannot
/// hide a protected destination behind an allowed source or vice versa.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A new path.
    Added,
    /// An existing path whose content changed.
    Modified,
    /// A removed path.
    Deleted,
    /// A path moved; `old_path` is the source side.
    Renamed,
    /// A path whose executable bit (or file mode) changed.
    ModeChanged,
}

/// One repository-relative candidate change against the base commit (SF.6).
///
/// Paths are normalized to `/`-separated repository-relative form. `old_path`
/// is the source side of a rename. `old_mode`/`new_mode` are the octal git file
/// modes (`100644`, `100755`, `120000`) when the change alters the mode.
/// `symlink_target` carries the recorded link target when the new path is a
/// symlink, so an escaping link is visible to the scope check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub kind: ChangeKind,
    /// The current (new) repository-relative path.
    pub path: String,
    /// The previous path for a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// The previous file mode (octal string, e.g. `100644`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    /// The new file mode (octal string, e.g. `100644`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    /// The recorded symlink target when the new path is a symlink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
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

    /// The candidate's change set against `checkout.base_sha` (SF.6).
    ///
    /// Returns one [`FileChange`] per added/modified/deleted/renamed path
    /// (both rename sides for a rename) and per mode change, in a
    /// deterministic order sorted by [`FileChange::path`]. This is a typed,
    /// transport-parity inspection: the same call works in-process and over
    /// the framed/Connect workspace boundary.
    fn changes(&self, checkout: &Checkout) -> Result<Vec<FileChange>, ContractError>;

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
