//! Agalma bd task-queue adapter (M1.1, `agalma-52k.2`).
//!
//! Implements `agalma_contracts::TaskQueueApi` over the `bd` CLI:
//!
//! - [`BdTaskQueue`] — backlog intake, atomic claims, and status projection;
//! - [`parse_task_block`] — the strict hand-rolled fenced-block parser.
//!
//! The `bd` binary, its JSON payloads, and the store layout are private to
//! this crate. `TaskQueueApi` is the only seam callers use.

mod bd;
pub mod block;

pub use bd::{BdTaskQueue, IssueProjection, PARKED_LABEL};
pub use block::{parse_task_block, BlockError, TaskBlock};
