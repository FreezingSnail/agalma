//! Typed `ExecutionApi`/`ExecutionOps` method registration for the kernel
//! component server (B1.7).
//!
//! A kernel server hosts one [`ExecutionOps`] implementation (the durable
//! `KernelEngine<LedgerClient>` in `agalma-kernel`). The registration helper
//! keeps the method handlers and their params-decoding boilerplate in one place,
//! mirroring [`crate::ledger_server::register_ledger`] and the other boundary
//! servers.

use std::sync::{Arc, Mutex, PoisonError};

use agalma_contracts::ContractError;
use serde_json::Value;

use crate::connect::{serve_wire, Wire};
use crate::execution_client::{
    BeginStepCall, CompleteStepCall, DirectiveExecCall, ExecIdCall, ExecutionOps, KernelCall,
    LeaseExecCall, NowCall, PendingStepCall, ReceiptCall, ReclaimCall, SnapshotPathCall, StartCall,
    TimerExecParams,
};
use crate::harness_client::{decode_params, encode_result};
use crate::server::Server;

/// Serve a kernel [`Server`] over the selected wire transport (IPCX.7).
///
/// `Framed` runs the B1 serial accept loop; `Connect` runs the IPCX Connect/UDS
/// accept loop. Both announce the same `READY {json}` line and preserve every
/// envelope guarantee (receipt replay, version refusal, in-band error codes), so
/// the supervisor and its bind gate are wire-agnostic.
pub fn serve_execution(
    server: Server,
    socket: &std::path::Path,
    wire: Wire,
) -> std::io::Result<()> {
    serve_wire(server, socket, wire)
}

/// Register every execution/kernel operation on `server`, hosting `engine`.
///
/// The engine is shared behind a mutex because several operations take
/// `&mut self`; the server dispatches serially, so the lock is uncontended.
pub fn register_execution<S>(server: &mut Server, engine: S)
where
    S: ExecutionOps + Send + 'static,
{
    let shared = Arc::new(Mutex::new(engine));

    // ---- ExecutionApi adapter -------------------------------------------
    {
        let shared = Arc::clone(&shared);
        server.on("start", move |params: Value| {
            let call: StartCall = decode_params(params)?;
            let result = lock(&shared).start(&call.task)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("step", move |params: Value| {
            let call: ExecIdCall = decode_params(params)?;
            let result = lock(&shared).step(&call.execution_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("recover", move |_params: Value| {
            lock(&shared).recover()?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("cancel", move |params: Value| {
            let call: ExecIdCall = decode_params(params)?;
            lock(&shared).cancel(&call.execution_id)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("status", move |params: Value| {
            let call: ExecIdCall = decode_params(params)?;
            let result = lock(&shared).status(&call.execution_id)?;
            encode_result(result)
        });
    }

    // ---- kernel engine surface ------------------------------------------
    {
        let shared = Arc::clone(&shared);
        server.on("apply", move |params: Value| {
            let call: KernelCall = decode_params(params)?;
            lock(&shared).apply(&call.execution_id, call.command, call.inputs)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("load_state", move |params: Value| {
            let call: ExecIdCall = decode_params(params)?;
            lock(&shared).load_state(&call.execution_id)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("pending_dispatch", move |_params: Value| {
            let result = lock(&shared).pending_dispatch()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("receipt", move |params: Value| {
            let call: ReceiptCall = decode_params(params)?;
            let result = lock(&shared).receipt(&call.operation_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("fire_due_timers", move |params: Value| {
            let call: NowCall = decode_params(params)?;
            lock(&shared).fire_due_timers(call.now_unix_ms)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("schedule_timer", move |params: Value| {
            let call: TimerExecParams = decode_params(params)?;
            let result = lock(&shared).schedule_timer(&call.timer)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("due_timers", move |params: Value| {
            let call: NowCall = decode_params(params)?;
            let result = lock(&shared).due_timers(call.now_unix_ms)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("begin_lease", move |params: Value| {
            let call: LeaseExecCall = decode_params(params)?;
            let result = lock(&shared).begin_lease(&call.task)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("lease_generation", move |params: Value| {
            let call: LeaseExecCall = decode_params(params)?;
            let result = lock(&shared).lease_generation(&call.task)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("reclaim", move |params: Value| {
            let call: ReclaimCall = decode_params(params)?;
            lock(&shared).reclaim(&call.task, &call.execution_id, call.now_unix_ms)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("submit_directive", move |params: Value| {
            let call: DirectiveExecCall = decode_params(params)?;
            lock(&shared).submit_directive(
                &call.execution_id,
                &call.directive_id,
                call.version,
                call.plan,
                call.now_unix_ms,
            )
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("export_snapshot_to", move |params: Value| {
            let call: SnapshotPathCall = decode_params(params)?;
            lock(&shared).export_snapshot_to(std::path::Path::new(&call.path))?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("import_snapshot_from", move |params: Value| {
            let call: SnapshotPathCall = decode_params(params)?;
            lock(&shared).import_snapshot_from(std::path::Path::new(&call.path))?;
            encode_result(())
        });
    }

    // ---- handler-registry protocol (caller-side effect dispatch) ----------
    {
        let shared = Arc::clone(&shared);
        server.on("begin_step", move |params: Value| {
            let call: BeginStepCall = decode_params(params)?;
            let result = lock(&shared).begin_step(&call.execution_id, call.request)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("begin_pending_step", move |params: Value| {
            let call: PendingStepCall = decode_params(params)?;
            let result =
                lock(&shared).begin_pending_step(&call.execution_id, &call.operation_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("complete_step", move |params: Value| {
            let call: CompleteStepCall = decode_params(params)?;
            let result = lock(&shared).complete_step(
                &call.execution_id,
                &call.operation_id,
                call.report,
                call.now_unix_ms,
            )?;
            encode_result(result)
        });
    }
}

/// Lock the shared engine, recovering a poisoned mutex into its inner value.
fn lock<S>(shared: &Arc<Mutex<S>>) -> std::sync::MutexGuard<'_, S> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The kernel wire method names, in the order the server registers them.
pub const EXECUTION_METHODS: [&str; 21] = [
    "start",
    "step",
    "recover",
    "cancel",
    "status",
    "apply",
    "load_state",
    "pending_dispatch",
    "receipt",
    "fire_due_timers",
    "schedule_timer",
    "due_timers",
    "begin_lease",
    "lease_generation",
    "reclaim",
    "submit_directive",
    "export_snapshot_to",
    "import_snapshot_from",
    "begin_step",
    "begin_pending_step",
    "complete_step",
];

/// Assert the server has a handler for every execution/kernel operation.
pub fn ensure_all_handlers(server: &Server) -> Result<(), ContractError> {
    for method in EXECUTION_METHODS {
        if !server.has_handler(method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "kernel server is missing handler `{method}`"
            )));
        }
    }
    Ok(())
}
