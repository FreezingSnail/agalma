//! Typed `TaskQueueApi` method registration for component servers (decision D2).
//!
//! A task-queue server hosts one [`TaskQueueOps`] implementation (the `bd`-backed
//! `BdTaskQueue` in `agalma-taskqueue`). The registration helper keeps the method
//! handlers and their params-decoding boilerplate in one place, mirroring
//! [`crate::harness_server::register_harness`], and
//! [`crate::workspace_server::register_workspace`]. [`serve_taskqueue`] serves a
//! built server over the selected wire transport (IPCX.5).

use std::sync::{Arc, Mutex, PoisonError};

use agalma_contracts::ContractError;
use serde_json::Value;

use crate::connect::{serve_wire, Wire};
use crate::harness_client::{decode_params, encode_result};
use crate::server::Server;
use crate::taskqueue_client::{
    CloseParams, CommentParams, LabelParams, ReviewAdmissionParams, StatusParams, TaskCall,
    TaskQueueOps,
};

/// Serve a task-queue [`Server`] over the selected wire transport (IPCX.5).
///
/// `Framed` runs the B1 serial accept loop; `Connect` runs the IPCX Connect/UDS
/// accept loop. Both announce the same `READY {json}` line and preserve every
/// envelope guarantee (receipt replay, version refusal, in-band error codes), so
/// the supervisor and its bind gate are wire-agnostic.
pub fn serve_taskqueue(
    server: Server,
    socket: &std::path::Path,
    wire: Wire,
) -> std::io::Result<()> {
    serve_wire(server, socket, wire)
}

/// Register every task-queue operation on `server`, hosting `queue`.
///
/// The queue is shared behind a mutex because several operations take
/// `&mut self`; the server dispatches serially, so the lock is uncontended.
pub fn register_taskqueue<S>(server: &mut Server, queue: S)
where
    S: TaskQueueOps + Send + 'static,
{
    let shared = Arc::new(Mutex::new(queue));

    {
        let shared = Arc::clone(&shared);
        server.on("ready_tasks", move |_params: Value| {
            let result = lock(&shared).ready_tasks()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("get", move |params: Value| {
            let call: TaskCall = decode_params(params)?;
            let result = lock(&shared).get(&call.id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("claim", move |params: Value| {
            let call: TaskCall = decode_params(params)?;
            let result = lock(&shared).claim(&call.id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("comment", move |params: Value| {
            let call: CommentParams = decode_params(params)?;
            lock(&shared).comment(&call.id, &call.text)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("close_task", move |params: Value| {
            let call: CloseParams = decode_params(params)?;
            lock(&shared).close_task(&call.id, &call.reason)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("add_label", move |params: Value| {
            let call: LabelParams = decode_params(params)?;
            lock(&shared).add_label(&call.id, &call.label)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("projection", move |params: Value| {
            let call: TaskCall = decode_params(params)?;
            let result = lock(&shared).projection(&call.id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("set_status", move |params: Value| {
            let call: StatusParams = decode_params(params)?;
            lock(&shared).set_status(&call.id, &call.status)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("remove_label", move |params: Value| {
            let call: LabelParams = decode_params(params)?;
            lock(&shared).remove_label(&call.id, &call.label)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("review_admission", move |params: Value| {
            let call: ReviewAdmissionParams = decode_params(params)?;
            let result = lock(&shared).review_admission(&call.id, &call.context)?;
            encode_result(result)
        });
    }
}

/// Lock the shared queue, recovering a poisoned mutex into its inner value.
fn lock<S>(shared: &Arc<Mutex<S>>) -> std::sync::MutexGuard<'_, S> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The task-queue operation names, in the order the server shares them.
pub const TASKQUEUE_METHODS: [&str; 10] = [
    "ready_tasks",
    "get",
    "claim",
    "comment",
    "close_task",
    "add_label",
    "projection",
    "set_status",
    "remove_label",
    "review_admission",
];

/// Assert the server has a handler for every task-queue operation.
pub fn ensure_all_handlers(server: &Server) -> Result<(), ContractError> {
    for method in TASKQUEUE_METHODS {
        if !server.has_handler(method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "taskqueue server is missing handler `{method}`"
            )));
        }
    }
    Ok(())
}
