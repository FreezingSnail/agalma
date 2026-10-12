//! Typed `HarnessApi` method registration for component servers (decision D2).
//!
//! A server binary hosts one `HarnessApi` implementation. Without a shared
//! registration helper every server would repeat the same nine handlers plus the
//! params-decoding boilerplate, so the harness boundary owns it here.

use std::sync::{Arc, Mutex, PoisonError};

use agalma_contracts::harness::{Describe, HarnessApi};
use agalma_contracts::ContractError;
use serde_json::Value;

use crate::connect::{serve_wire, Wire};
use crate::harness_client::{
    decode_params, encode_result, AttemptCall, OperationCall, RunTurnCall, SessionCall,
};
use crate::server::{Server, ServerInfo};

/// Build the READY-line identity announced for a harness `Describe`.
pub fn server_info_from(describe: &Describe) -> ServerInfo {
    ServerInfo {
        api: describe.api.clone(),
        api_version: describe.api_version,
        impl_name: describe.impl_name.clone(),
        impl_version: describe.impl_version.clone(),
        capabilities: describe.capabilities.clone(),
    }
}

/// Serve a harness [`Server`] over the selected wire transport (IPCX.2).
///
/// `Framed` runs the B1 serial accept loop; `Connect` runs the IPCX Connect/UDS
/// accept loop. Both announce the same `READY {json}` line and preserve every
/// envelope guarantee (receipt replay, version refusal, in-band error codes), so
/// the supervisor and its bind gate are wire-agnostic.
pub fn serve_harness(server: Server, socket: &std::path::Path, wire: Wire) -> std::io::Result<()> {
    serve_wire(server, socket, wire)
}

/// Register every [`HarnessApi`] method on `server`, hosting `harness`.
///
/// The harness is shared behind a mutex because the frozen trait takes
/// `&mut self`; the server dispatches serially, so the lock is uncontended.
/// Returns the identity `Describe` used to build the READY line.
pub fn register_harness<H>(server: &mut Server, harness: H) -> Describe
where
    H: HarnessApi + Send + 'static,
{
    let describe = harness.describe();
    let shared = Arc::new(Mutex::new(harness));

    {
        let shared = Arc::clone(&shared);
        server.on("describe", move |_params: Value| {
            let harness = lock(&shared);
            encode_result(harness.describe())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("start_attempt", move |params: Value| {
            let req = decode_params(params)?;
            let result = lock(&shared).start_attempt(req)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("create_session", move |params: Value| {
            let req = decode_params(params)?;
            let result = lock(&shared).create_session(req)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("run_turn", move |params: Value| {
            let call: RunTurnCall = decode_params(params)?;
            let result = lock(&shared).run_turn(&call.session, call.request)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("inspect_operation", move |params: Value| {
            let call: OperationCall = decode_params(params)?;
            let result = lock(&shared).inspect_operation(&call.operation)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("read_events", move |params: Value| {
            let call: OperationCall = decode_params(params)?;
            let result = lock(&shared).read_events(&call.operation)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("cancel_operation", move |params: Value| {
            let call: OperationCall = decode_params(params)?;
            let result = lock(&shared).cancel_operation(&call.operation)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("close_session", move |params: Value| {
            let call: SessionCall = decode_params(params)?;
            lock(&shared).close_session(&call.session)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("stop_attempt", move |params: Value| {
            let call: AttemptCall = decode_params(params)?;
            let result = lock(&shared).stop_attempt(&call.attempt)?;
            encode_result(result)
        });
    }

    describe
}

/// Lock the shared harness, recovering a poisoned mutex into its inner value
/// (a panicked handler must not brick every later call).
fn lock<H>(shared: &Arc<Mutex<H>>) -> std::sync::MutexGuard<'_, H> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The nine harness method names, in trait order (used by tests/conformance).
pub const HARNESS_METHODS: [&str; 9] = [
    "describe",
    "start_attempt",
    "create_session",
    "run_turn",
    "inspect_operation",
    "read_events",
    "cancel_operation",
    "close_session",
    "stop_attempt",
];

/// Assert the server has a handler for every harness method.
pub fn ensure_all_handlers(server: &Server) -> Result<(), ContractError> {
    for method in HARNESS_METHODS {
        if !server.has_handler(method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "harness server is missing handler `{method}`"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::harness::HARNESS_API;

    fn sample_describe() -> Describe {
        Describe {
            api: HARNESS_API.to_string(),
            api_version: 0,
            impl_name: "sample".to_string(),
            impl_version: "sample/v1".to_string(),
            capabilities: Default::default(),
        }
    }

    #[test]
    fn server_info_mirrors_describe() {
        let info = server_info_from(&sample_describe());
        assert_eq!(info.api, "HarnessApi");
        assert_eq!(info.impl_name, "sample");
        assert_eq!(info.impl_version, "sample/v1");
    }
}
