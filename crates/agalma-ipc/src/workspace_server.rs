//! Typed `WorkspaceApi` method registration for component servers (decision D2).
//!
//! A workspace server hosts one [`WorkspaceOps`] implementation (the git-backed
//! `Workspace` in `agalma-workspace`). The registration helper keeps the method
//! handlers and their params-decoding boilerplate in one place, mirroring
//! [`crate::harness_server::register_harness`] and
//! [`crate::sandbox_server::register_sandbox`].

use std::sync::{Arc, Mutex, PoisonError};

use agalma_contracts::ContractError;
use serde_json::Value;

use crate::connect::{serve_wire, Wire};
use crate::harness_client::{decode_params, encode_result};
use crate::server::Server;
use crate::workspace_client::{
    CheckoutCall, IntegrateOriginParams, IntegrateOriginVerifiedParams, IntegrateParams,
    OriginReceiptParams, PrepareParams, PrepareRepoParams, PrepareVerifyParams, RevertOriginParams,
    RevertParams, TagParams, WorkspaceOps,
};

/// Serve a workspace [`Server`] over the selected wire transport (IPCX.4).
///
/// `Framed` runs the B1 serial accept loop; `Connect` runs the IPCX Connect/UDS
/// accept loop. Both announce the same `READY {json}` line and preserve every
/// envelope guarantee (receipt replay, version refusal, in-band error codes), so
/// the supervisor and its bind gate are wire-agnostic.
pub fn serve_workspace(
    server: Server,
    socket: &std::path::Path,
    wire: Wire,
) -> std::io::Result<()> {
    serve_wire(server, socket, wire)
}

/// Register every workspace operation on `server`, hosting `workspace`.
///
/// The workspace is shared behind a mutex because several operations take
/// `&mut self`; the server dispatches serially, so the lock is uncontended.
pub fn register_workspace<S>(server: &mut Server, workspace: S)
where
    S: WorkspaceOps + Send + 'static,
{
    register_workspace_inner(server, workspace, true)
}

/// Register the **legacy** workspace surface: every operation except the SF.6
/// `integrate_origin_verified` method. Reproduces a pre-SF.6 server so a test
/// can prove an old peer fails closed on the verified call instead of silently
/// ignoring the expected candidate.
pub fn register_workspace_legacy<S>(server: &mut Server, workspace: S)
where
    S: WorkspaceOps + Send + 'static,
{
    register_workspace_inner(server, workspace, false)
}

fn register_workspace_inner<S>(server: &mut Server, workspace: S, verified: bool)
where
    S: WorkspaceOps + Send + 'static,
{
    let shared = Arc::new(Mutex::new(workspace));

    {
        let shared = Arc::clone(&shared);
        server.on("prepare", move |params: Value| {
            let call: PrepareParams = decode_params(params)?;
            let result = lock(&shared).prepare(&call.template, &call.execution)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("diff", move |params: Value| {
            let call: CheckoutCall = decode_params(params)?;
            let result = lock(&shared).diff(&call.checkout)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("changes", move |params: Value| {
            let call: CheckoutCall = decode_params(params)?;
            let result = lock(&shared).changes(&call.checkout)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("integrate", move |params: Value| {
            let call: IntegrateParams = decode_params(params)?;
            let result = lock(&shared).integrate(&call.checkout, &call.expected_main_sha)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("tag", move |params: Value| {
            let call: TagParams = decode_params(params)?;
            lock(&shared).tag(&call.name, &call.sha)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("reconcile", move |params: Value| {
            let call: CheckoutCall = decode_params(params)?;
            let result = lock(&shared).reconcile(&call.checkout)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("revert", move |params: Value| {
            let call: RevertParams = decode_params(params)?;
            lock(&shared).revert(&call.sha)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("prepare_repo", move |params: Value| {
            let call: PrepareRepoParams = decode_params(params)?;
            let result = lock(&shared).prepare_repo(
                std::path::Path::new(&call.origin_path),
                &call.base_ref,
                &call.execution,
                std::path::Path::new(&call.base_dir),
            )?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("integrate_origin", move |params: Value| {
            let call: IntegrateOriginParams = decode_params(params)?;
            let result = lock(&shared).integrate_origin(
                &call.checkout,
                std::path::Path::new(&call.origin_path),
                &call.expected_main_sha,
            )?;
            encode_result(result)
        });
    }
    if verified {
        let shared = Arc::clone(&shared);
        server.on("integrate_origin_verified", move |params: Value| {
            let call: IntegrateOriginVerifiedParams = decode_params(params)?;
            let result = lock(&shared).integrate_origin_verified(
                &call.checkout,
                std::path::Path::new(&call.origin_path),
                &call.expected_main_sha,
                &call.expected_candidate_sha,
            )?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("candidate_sha", move |params: Value| {
            let call: CheckoutCall = decode_params(params)?;
            let result = lock(&shared).candidate_sha(&call.checkout)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("commit_candidate", move |params: Value| {
            let call: CheckoutCall = decode_params(params)?;
            let result = lock(&shared).commit_candidate(&call.checkout)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("prepare_verify_checkout", move |params: Value| {
            let call: PrepareVerifyParams = decode_params(params)?;
            let result = lock(&shared).prepare_verify_checkout(
                &call.checkout,
                std::path::Path::new(&call.base_dir),
                &call.execution,
                &call.sha,
            )?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("origin_receipt", move |params: Value| {
            let call: OriginReceiptParams = decode_params(params)?;
            let result = lock(&shared)
                .origin_receipt(&call.checkout, std::path::Path::new(&call.origin_path))?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("revert_origin", move |params: Value| {
            let call: RevertOriginParams = decode_params(params)?;
            lock(&shared).revert_origin(
                std::path::Path::new(&call.origin_path),
                &call.to_sha,
                &call.from_sha,
            )?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("integrated_receipt", move |params: Value| {
            let call: CheckoutCall = decode_params(params)?;
            let result = lock(&shared).integrated_receipt(&call.checkout)?;
            encode_result(result)
        });
    }
}

/// Lock the shared workspace, recovering a poisoned mutex into its inner value.
fn lock<S>(shared: &Arc<Mutex<S>>) -> std::sync::MutexGuard<'_, S> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The workspace operation names, in the order the server shares them.
pub const WORKSPACE_METHODS: [&str; 16] = [
    "prepare",
    "diff",
    "changes",
    "integrate",
    "tag",
    "reconcile",
    "revert",
    "prepare_repo",
    "integrate_origin",
    "integrate_origin_verified",
    "candidate_sha",
    "commit_candidate",
    "prepare_verify_checkout",
    "origin_receipt",
    "revert_origin",
    "integrated_receipt",
];

/// Assert the server has a handler for every workspace operation.
pub fn ensure_all_handlers(server: &Server) -> Result<(), ContractError> {
    for method in WORKSPACE_METHODS {
        if !server.has_handler(method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "workspace server is missing handler `{method}`"
            )));
        }
    }
    Ok(())
}
