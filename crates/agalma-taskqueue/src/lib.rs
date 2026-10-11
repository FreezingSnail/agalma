//! Agalma bd task-queue adapter (M1.1, `agalma-52k.2`).
//!
//! Implements `agalma_contracts::TaskQueueApi` over the `bd` CLI:
//!
//! - [`BdTaskQueue`] — backlog intake, atomic claims, and status projection;
//! - [`parse_task_block`] — the strict hand-rolled fenced-block parser.
//!
//! The `bd` binary, its JSON payloads, and the store layout are private to
//! this crate. `TaskQueueApi` is the only seam callers use.
//!
//! B1.5 also ships the task-queue component server ([`server`]) that hosts
//! `BdTaskQueue` behind the framed transport; [`BdTaskQueue`] implements
//! [`agalma_ipc::TaskQueueOps`] (the frozen trait plus the reconcile-projection
//! operations) so one registration helper covers both bindings.

mod bd;
pub mod block;
pub mod decomp;
pub mod server;

use agalma_contracts::{ContractError, TaskId};

pub use agalma_ipc::{IssueProjection, TaskQueueOps};
pub use bd::{
    BdTaskQueue, EpicRecord, DECOMP_APPROVED_LABEL, DECOMP_ATTEMPTS_PREFIX, DECOMP_ATTEMPT_PREFIX,
    DECOMP_FIX_LABEL, DECOMP_INFLIGHT_LABEL, DECOMP_REJECTED_LABEL, HUMAN_LABEL, PARKED_LABEL,
    STAGED_LABEL,
};
pub use block::{parse_task_block, BlockError, TaskBlock};

impl TaskQueueOps for BdTaskQueue {
    fn projection(&self, id: &TaskId) -> Result<IssueProjection, ContractError> {
        BdTaskQueue::projection(self, id)
    }

    fn set_status(&mut self, id: &TaskId, status: &str) -> Result<(), ContractError> {
        BdTaskQueue::set_status(self, id, status)
    }

    fn remove_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        BdTaskQueue::remove_label(self, id, label)
    }
}
