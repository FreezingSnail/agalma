//! Typed `SandboxApi` method registration for component servers (decision D2).
//!
//! A sandbox server hosts one [`SandboxApi`] implementation (the Seatbelt
//! backend in `agalma-sandbox`). The registration helper keeps the three
//! method handlers and their params-decoding boilerplate in one place, mirroring
//! [`crate::harness_server::register_harness`].

use std::sync::{Arc, Mutex, PoisonError};

use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};
use agalma_contracts::ContractError;
use serde_json::Value;

use crate::harness_client::{decode_params, encode_result};
use crate::server::Server;

/// Register every [`SandboxApi`] method on `server`, hosting `sandbox`.
///
/// The sandbox is shared behind a mutex because `launch`/`terminate` take
/// `&mut self`; the server dispatches serially, so the lock is uncontended.
pub fn register_sandbox<S>(server: &mut Server, sandbox: S)
where
    S: SandboxApi + Send + 'static,
{
    let shared = Arc::new(Mutex::new(sandbox));

    {
        let shared = Arc::clone(&shared);
        server.on("launch", move |params: Value| {
            let spec: LaunchSpec = decode_params(params)?;
            let result = lock(&shared).launch(spec)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server
            .on("alive", move |params: Value| {
                let child: SandboxChild = decode_params(params)?;
                let result = lock(&shared).alive(&child)?;
                encode_result(result)
            });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("terminate", move |params: Value| {
            let child: SandboxChild = decode_params(params)?;
            let result = lock(&shared).terminate(&child)?;
            encode_result(result)
        });
    }
}

/// Lock the shared sandbox, recovering a poisoned mutex into its inner value.
fn lock<S>(shared: &Arc<Mutex<S>>) -> std::sync::MutexGuard<'_, S> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The three sandbox method names, in trait order (tests/conformance).
pub const SANDBOX_METHODS: [&str; 3] = ["launch", "alive", "terminate"];

/// Assert the server has a handler for every sandbox method.
pub fn ensure_all_handlers(server: &Server) -> Result<(), ContractError> {
    for method in SANDBOX_METHODS {
        if !server.has_handler(method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "sandbox server is missing handler `{method}`"
            )));
        }
    }
    Ok(())
}
