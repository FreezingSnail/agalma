//! `WorkspaceClient` (decisions D2, IPCX.4): a typed [`WorkspaceApi`] over the
//! workspace boundary, on either wire transport, plus the M1 repo-mode
//! operations the conductor drives.
//!
//! Method names are the existing method snake-case
//! (`prepare`/`diff`/`integrate`/`tag`/`reconcile`/`revert` plus the repo-mode
//! `prepare_repo`/`integrate_origin`/`candidate_sha`/`commit_candidate`/
//! `prepare_verify_checkout`/`origin_receipt`/`revert_origin`/
//! `integrated_receipt`). Each call carries one JSON params object and decodes a
//! typed result. Transport failures map onto the frozen [`ContractError`]
//! surface exactly as for the harness/sandbox clients: a mid-call connection
//! loss is already classified by the transport as `UnknownOutcome`; a pre-send
//! loss becomes `KnownFailure`.
//!
//! Since IPCX.4 the client can speak either [`Wire::Framed`] (B1 length-prefixed
//! JSON) or [`Wire::Connect`] (ConnectRPC over HTTP/2 on a Unix socket). The
//! envelope semantics are identical on both; only the byte transport differs.
//!
//! [`WorkspaceOps`] is the full operation surface a workspace server hosts: the
//! frozen [`WorkspaceApi`] trait plus the repo-mode inherent operations. It lets
//! a single registration helper and a single conductor handle cover both the
//! in-process `Workspace` and the process-bound `WorkspaceClient` without
//! changing any phase logic.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use agalma_contracts::{Checkout, ContractError, ExecutionId, MergeReceipt, WorkspaceApi};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::connect::{ConnectClient, Wire};
use crate::error::IpcError;

/// Params for `prepare` (template mode).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareParams {
    pub template: String,
    pub execution: ExecutionId,
}

/// Params for `prepare_repo` (repo mode, M1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareRepoParams {
    pub origin_path: String,
    pub base_ref: String,
    pub execution: ExecutionId,
    pub base_dir: String,
}

/// Params for the checkout-scoped methods (`diff`, `integrate`, `reconcile`,
/// `candidate_sha`, `commit_candidate`, `integrated_receipt`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckoutCall {
    pub checkout: Checkout,
}

/// Params for `integrate` (template mode).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrateParams {
    pub checkout: Checkout,
    pub expected_main_sha: String,
}

/// Params for `integrate_origin` (repo mode).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrateOriginParams {
    pub checkout: Checkout,
    pub origin_path: String,
    pub expected_main_sha: String,
}

/// Params for `tag`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagParams {
    pub name: String,
    pub sha: String,
}

/// Params for `revert`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevertParams {
    pub sha: String,
}

/// Params for `prepare_verify_checkout`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareVerifyParams {
    pub checkout: Checkout,
    pub base_dir: String,
    pub execution: ExecutionId,
    pub sha: String,
}

/// Params for `origin_receipt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OriginReceiptParams {
    pub checkout: Checkout,
    pub origin_path: String,
}

/// Params for `revert_origin`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevertOriginParams {
    pub origin_path: String,
    pub to_sha: String,
    pub from_sha: String,
}

/// The full workspace operation surface: the frozen [`WorkspaceApi`] plus the
/// repo-mode operations introduced in M1.
///
/// The conductor drives both the trait methods and the repo-mode operations, so
/// extraction must carry all of them. `agalma-workspace` implements this for the
/// git-backed `Workspace`; [`WorkspaceClient`] and the conductor's
/// `WorkspaceHandle` implement it over the transport (or by delegation).
pub trait WorkspaceOps: WorkspaceApi {
    /// Prepare an isolated checkout by cloning an origin repository (repo mode).
    fn prepare_repo(
        &mut self,
        origin_path: &Path,
        base_ref: &str,
        execution: &ExecutionId,
        base_dir: &Path,
    ) -> Result<Checkout, ContractError>;

    /// Integrate a repo-mode candidate into the origin with expected-SHA CAS.
    fn integrate_origin(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError>;

    /// The candidate branch HEAD SHA in `checkout`'s repository.
    fn candidate_sha(&self, checkout: &Checkout) -> Result<String, ContractError>;

    /// Commit a repo-mode candidate's dirty working tree onto its candidate
    /// branch and return the branch HEAD SHA.
    fn commit_candidate(&self, checkout: &Checkout) -> Result<String, ContractError>;

    /// Clone `checkout` at `sha` into a fresh detached verification checkout.
    fn prepare_verify_checkout(
        &self,
        checkout: &Checkout,
        base_dir: &Path,
        execution: &ExecutionId,
        sha: &str,
    ) -> Result<PathBuf, ContractError>;

    /// Derive a receipt for an already-landed repo-mode integration.
    fn origin_receipt(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
    ) -> Result<Option<MergeReceipt>, ContractError>;

    /// Compare-and-swap the origin's `main` back to `to_sha`.
    fn revert_origin(
        &self,
        origin_path: &Path,
        to_sha: &str,
        from_sha: &str,
    ) -> Result<(), ContractError>;

    /// Derive the merge receipt for an already-landed integration.
    fn integrated_receipt(
        &mut self,
        checkout: &Checkout,
    ) -> Result<Option<MergeReceipt>, ContractError>;
}

/// The transport backing a [`WorkspaceClient`].
enum Backend {
    /// B1 framed-JSON client ([`Client`]).
    Framed(Client),
    /// IPCX Connect client plus the runtime it is driven on.
    Connect(ConnectBackend),
}

/// Connect transport state: the async client and the runtime that drives it.
struct ConnectBackend {
    /// The runtime driving `client`; `None` after [`Drop`] tears it down.
    runtime: Option<tokio::runtime::Runtime>,
    client: ConnectClient,
}

impl Drop for ConnectBackend {
    fn drop(&mut self) {
        // The conductor builds and drops these clients from within its own Tokio
        // runtime (the async `agalma` binary). A blocking runtime drop is
        // forbidden in an async context, so shut the runtime down without
        // blocking; its transport tasks are abandoned with it.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// A [`WorkspaceApi`]/[`WorkspaceOps`] implementation backed by a workspace
/// component server.
pub struct WorkspaceClient {
    backend: RefCell<Backend>,
}

impl WorkspaceClient {
    /// Connect to `socket` over the framed transport.
    pub fn connect(socket: &Path) -> Result<WorkspaceClient, IpcError> {
        WorkspaceClient::connect_wire(socket, Wire::Framed)
    }

    /// Connect to the workspace server listening on `socket` over `wire`. Both
    /// wires preserve the envelope semantics.
    pub fn connect_wire(socket: &Path, wire: Wire) -> Result<WorkspaceClient, IpcError> {
        match wire {
            Wire::Framed => Ok(WorkspaceClient {
                backend: RefCell::new(Backend::Framed(Client::connect(socket)?)),
            }),
            Wire::Connect => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| IpcError::unavailable(format!("tokio runtime: {e}")))?;
                let client = crate::block_on_nested(&runtime, ConnectClient::connect(socket))?;
                Ok(WorkspaceClient {
                    backend: RefCell::new(Backend::Connect(ConnectBackend {
                        runtime: Some(runtime),
                        client,
                    })),
                })
            }
        }
    }

    /// Build a client from an already-connected transport (used by the process
    /// supervisor after the `READY` handshake).
    pub fn from_client(client: Client) -> WorkspaceClient {
        WorkspaceClient {
            backend: RefCell::new(Backend::Framed(client)),
        }
    }

    fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<R, ContractError> {
        match &mut *self.backend.borrow_mut() {
            Backend::Framed(client) => client.call_typed(method, params).map_err(map_ipc_error),
            Backend::Connect(backend) => crate::block_on_nested(
                backend.runtime.as_ref().expect("runtime present until drop"),
                backend.client.call_typed(method, params),
            )
            .map_err(map_ipc_error),
        }
    }
}

/// Map a transport error onto the frozen [`ContractError`] surface.
///
/// `IpcError::Contract` passes through unchanged (including `UnknownOutcome`).
/// Transport-only faults have no contract variant: `Unavailable`/`Protocol`
/// become `KnownFailure` (retry-safe or caller-visible framing faults).
pub fn map_ipc_error(err: IpcError) -> ContractError {
    match err {
        IpcError::Contract(error) => error,
        IpcError::Unavailable(message) => {
            ContractError::KnownFailure(format!("workspace transport unavailable: {message}"))
        }
        IpcError::Protocol(message) => {
            ContractError::KnownFailure(format!("workspace protocol violation: {message}"))
        }
    }
}

impl WorkspaceApi for WorkspaceClient {
    fn prepare(
        &mut self,
        template: &str,
        execution: &ExecutionId,
    ) -> Result<Checkout, ContractError> {
        self.call(
            "prepare",
            &PrepareParams {
                template: template.to_string(),
                execution: execution.clone(),
            },
        )
    }

    fn diff(&self, checkout: &Checkout) -> Result<String, ContractError> {
        self.call(
            "diff",
            &CheckoutCall {
                checkout: checkout.clone(),
            },
        )
    }

    fn integrate(
        &mut self,
        checkout: &Checkout,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        self.call(
            "integrate",
            &IntegrateParams {
                checkout: checkout.clone(),
                expected_main_sha: expected_main_sha.to_string(),
            },
        )
    }

    fn tag(&mut self, name: &str, sha: &str) -> Result<(), ContractError> {
        self.call(
            "tag",
            &TagParams {
                name: name.to_string(),
                sha: sha.to_string(),
            },
        )
    }

    fn reconcile(&mut self, checkout: &Checkout) -> Result<bool, ContractError> {
        self.call(
            "reconcile",
            &CheckoutCall {
                checkout: checkout.clone(),
            },
        )
    }

    fn revert(&mut self, sha: &str) -> Result<(), ContractError> {
        self.call(
            "revert",
            &RevertParams {
                sha: sha.to_string(),
            },
        )
    }
}

impl WorkspaceOps for WorkspaceClient {
    fn prepare_repo(
        &mut self,
        origin_path: &Path,
        base_ref: &str,
        execution: &ExecutionId,
        base_dir: &Path,
    ) -> Result<Checkout, ContractError> {
        self.call(
            "prepare_repo",
            &PrepareRepoParams {
                origin_path: origin_path.to_string_lossy().into_owned(),
                base_ref: base_ref.to_string(),
                execution: execution.clone(),
                base_dir: base_dir.to_string_lossy().into_owned(),
            },
        )
    }

    fn integrate_origin(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        self.call(
            "integrate_origin",
            &IntegrateOriginParams {
                checkout: checkout.clone(),
                origin_path: origin_path.to_string_lossy().into_owned(),
                expected_main_sha: expected_main_sha.to_string(),
            },
        )
    }

    fn candidate_sha(&self, checkout: &Checkout) -> Result<String, ContractError> {
        self.call(
            "candidate_sha",
            &CheckoutCall {
                checkout: checkout.clone(),
            },
        )
    }

    fn commit_candidate(&self, checkout: &Checkout) -> Result<String, ContractError> {
        self.call(
            "commit_candidate",
            &CheckoutCall {
                checkout: checkout.clone(),
            },
        )
    }

    fn prepare_verify_checkout(
        &self,
        checkout: &Checkout,
        base_dir: &Path,
        execution: &ExecutionId,
        sha: &str,
    ) -> Result<PathBuf, ContractError> {
        self.call(
            "prepare_verify_checkout",
            &PrepareVerifyParams {
                checkout: checkout.clone(),
                base_dir: base_dir.to_string_lossy().into_owned(),
                execution: execution.clone(),
                sha: sha.to_string(),
            },
        )
    }

    fn origin_receipt(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        self.call(
            "origin_receipt",
            &OriginReceiptParams {
                checkout: checkout.clone(),
                origin_path: origin_path.to_string_lossy().into_owned(),
            },
        )
    }

    fn revert_origin(
        &self,
        origin_path: &Path,
        to_sha: &str,
        from_sha: &str,
    ) -> Result<(), ContractError> {
        self.call(
            "revert_origin",
            &RevertOriginParams {
                origin_path: origin_path.to_string_lossy().into_owned(),
                to_sha: to_sha.to_string(),
                from_sha: from_sha.to_string(),
            },
        )
    }

    fn integrated_receipt(
        &mut self,
        checkout: &Checkout,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        self.call(
            "integrated_receipt",
            &CheckoutCall {
                checkout: checkout.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_error_passes_through() {
        let err = map_ipc_error(IpcError::Contract(ContractError::Conflict("x".into())));
        assert!(matches!(err, ContractError::Conflict(_)));
    }

    #[test]
    fn unknown_outcome_passes_through() {
        let err = map_ipc_error(IpcError::unknown_outcome("lost"));
        assert!(matches!(err, ContractError::UnknownOutcome(_)));
    }

    #[test]
    fn unavailable_maps_to_known_failure() {
        let err = map_ipc_error(IpcError::unavailable("gone"));
        assert!(matches!(err, ContractError::KnownFailure(_)));
    }

    /// The client must satisfy the conductor seam `WorkspaceOps + Send`.
    #[test]
    fn workspace_client_is_a_send_workspace_ops() {
        fn assert_seam<T: WorkspaceOps + Send + 'static>() {}
        assert_seam::<WorkspaceClient>();
    }
}
