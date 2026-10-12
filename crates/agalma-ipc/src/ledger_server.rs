//! Typed `LedgerApi`/`LedgerOps` method registration for component servers
//! (decision D2).
//!
//! A ledger server hosts one [`LedgerOps`] implementation (the SQLite-backed
//! `SqliteLedger` in `agalma-ledger`). The registration helper keeps the method
//! handlers and their params-decoding boilerplate in one place, mirroring
//! [`crate::harness_server::register_harness`],
//! [`crate::sandbox_server::register_sandbox`],
//! [`crate::workspace_server::register_workspace`], and
//! [`crate::taskqueue_server::register_taskqueue`].
//!
//! `export_state_to`/`import_state_from` are the D3 snapshot **file** transfer
//! primitives: the server writes/reads the snapshot under its state dir; only
//! the path crosses the frame.

use std::sync::{Arc, Mutex, PoisonError};

use agalma_contracts::ContractError;
use serde_json::Value;

use crate::harness_client::{decode_params, encode_result};
use crate::ledger_client::{
    BindingParams, CommitParams, ControlReceiptParams, DecisionParams, DigestParams,
    DirectiveIdCall, DirectiveParams, DueTimersParams, ExecutionCall, KeyCall, LatchParams,
    LeaseCall, LedgerOps, MetaParams, OperationIdCall, OutcomeParams, PathParams, TimerIdCall,
    TimerParams,
};
use crate::server::Server;

/// Register every ledger operation on `server`, hosting `ledger`.
///
/// The ledger is shared behind a mutex because several operations take
/// `&mut self`; the server dispatches serially, so the lock is uncontended.
pub fn register_ledger<S>(server: &mut Server, ledger: S)
where
    S: LedgerOps + Send + 'static,
{
    let shared = Arc::new(Mutex::new(ledger));

    {
        let shared = Arc::clone(&shared);
        server.on("commit", move |params: Value| {
            let call: CommitParams = decode_params(params)?;
            let result = lock(&shared).commit(call.batch)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("execution", move |params: Value| {
            let call: ExecutionCall = decode_params(params)?;
            let result = lock(&shared).execution(&call.execution_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("executions", move |_params: Value| {
            let result = lock(&shared).executions()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("pending_intents", move |_params: Value| {
            let result = lock(&shared).pending_intents()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("events", move |params: Value| {
            let call: ExecutionCall = decode_params(params)?;
            let result = lock(&shared).events(&call.execution_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("operation_receipt", move |params: Value| {
            let call: OperationIdCall = decode_params(params)?;
            let result = lock(&shared).operation_receipt(&call.operation_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("lease_generation", move |params: Value| {
            let call: LeaseCall = decode_params(params)?;
            let result = lock(&shared).lease_generation(&call.task)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("begin_lease", move |params: Value| {
            let call: LeaseCall = decode_params(params)?;
            let result = lock(&shared).begin_lease(&call.task)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("kill_latch", move |_params: Value| {
            let result = lock(&shared).kill_latch()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("set_kill_latch", move |params: Value| {
            let call: LatchParams = decode_params(params)?;
            lock(&shared).set_kill_latch(call.latched)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("upsert_binding", move |params: Value| {
            let call: BindingParams = decode_params(params)?;
            lock(&shared).upsert_binding(&call.binding)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("bindings", move |_params: Value| {
            let result = lock(&shared).bindings()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("schema_version", move |_params: Value| {
            let result = lock(&shared).schema_version()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("record_decision", move |params: Value| {
            let call: DecisionParams = decode_params(params)?;
            lock(&shared).record_decision(&call.request)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("record_decision_outcome", move |params: Value| {
            let call: OutcomeParams = decode_params(params)?;
            lock(&shared).record_decision_outcome(&call.operation_id, &call.outcome)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("decision_outcome", move |params: Value| {
            let call: OperationIdCall = decode_params(params)?;
            let result = lock(&shared).decision_outcome(&call.operation_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("record_digest", move |params: Value| {
            let call: DigestParams = decode_params(params)?;
            lock(&shared).record_digest(&call.digest)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("digests_for_execution", move |params: Value| {
            let call: ExecutionCall = decode_params(params)?;
            let result = lock(&shared).digests_for_execution(&call.execution_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("kernel_meta_get", move |params: Value| {
            let call: KeyCall = decode_params(params)?;
            let result = lock(&shared).kernel_meta_get(&call.key)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("kernel_meta_set", move |params: Value| {
            let call: MetaParams = decode_params(params)?;
            lock(&shared).kernel_meta_set(&call.key, &call.value)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("insert_timer", move |params: Value| {
            let call: TimerParams = decode_params(params)?;
            let result = lock(&shared).insert_timer(&call.timer)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("timer", move |params: Value| {
            let call: TimerIdCall = decode_params(params)?;
            let result = lock(&shared).timer(&call.timer_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("due_timers", move |params: Value| {
            let call: DueTimersParams = decode_params(params)?;
            let result = lock(&shared).due_timers(call.now_unix_ms)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("mark_timer_fired", move |params: Value| {
            let call: TimerIdCall = decode_params(params)?;
            lock(&shared).mark_timer_fired(&call.timer_id)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("cancel_timers_for_execution", move |params: Value| {
            let call: ExecutionCall = decode_params(params)?;
            let result = lock(&shared).cancel_timers_for_execution(&call.execution_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("next_deadline", move |_params: Value| {
            let result = lock(&shared).next_deadline()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("upsert_directive", move |params: Value| {
            let call: DirectiveParams = decode_params(params)?;
            let result = lock(&shared).upsert_directive(&call.directive)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("directive", move |params: Value| {
            let call: DirectiveIdCall = decode_params(params)?;
            let result = lock(&shared).directive(&call.directive_id)?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("consume_dispatch_intent", move |params: Value| {
            let call: OperationIdCall = decode_params(params)?;
            lock(&shared).consume_dispatch_intent(&call.operation_id)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("record_control_receipt", move |params: Value| {
            let call: ControlReceiptParams = decode_params(params)?;
            lock(&shared).record_control_receipt(&call.receipt)?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("control_receipts", move |_params: Value| {
            let result = lock(&shared).control_receipts()?;
            encode_result(result)
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("export_state_to", move |params: Value| {
            let call: PathParams = decode_params(params)?;
            lock(&shared).export_snapshot_to(std::path::Path::new(&call.path))?;
            encode_result(())
        });
    }
    {
        let shared = Arc::clone(&shared);
        server.on("import_state_from", move |params: Value| {
            let call: PathParams = decode_params(params)?;
            lock(&shared).import_snapshot_from(std::path::Path::new(&call.path))?;
            encode_result(())
        });
    }
}

/// Lock the shared ledger, recovering a poisoned mutex into its inner value.
fn lock<S>(shared: &Arc<Mutex<S>>) -> std::sync::MutexGuard<'_, S> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The ledger wire method names, in the order the server shares them.
///
/// `export_state`/`import_state` are client-side conveniences implemented over
/// the two snapshot file primitives (`export_state_to`/`import_state_from`), so
/// they are not separate wire methods.
pub const LEDGER_METHODS: [&str; 33] = [
    "commit",
    "execution",
    "executions",
    "pending_intents",
    "events",
    "operation_receipt",
    "lease_generation",
    "begin_lease",
    "kill_latch",
    "set_kill_latch",
    "upsert_binding",
    "bindings",
    "schema_version",
    "record_decision",
    "record_decision_outcome",
    "decision_outcome",
    "record_digest",
    "digests_for_execution",
    "kernel_meta_get",
    "kernel_meta_set",
    "insert_timer",
    "timer",
    "due_timers",
    "mark_timer_fired",
    "cancel_timers_for_execution",
    "next_deadline",
    "upsert_directive",
    "directive",
    "consume_dispatch_intent",
    "record_control_receipt",
    "control_receipts",
    "export_state_to",
    "import_state_from",
];

/// Assert the server has a handler for every ledger operation.
pub fn ensure_all_handlers(server: &Server) -> Result<(), ContractError> {
    for method in LEDGER_METHODS {
        if !server.has_handler(method) {
            return Err(ContractError::UnsupportedCapability(format!(
                "ledger server is missing handler `{method}`"
            )));
        }
    }
    Ok(())
}
