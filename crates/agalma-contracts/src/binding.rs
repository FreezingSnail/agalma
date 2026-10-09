//! Component binding records.
//!
//! A binding pins a component API to a selected implementation and generation.
//! Operations record their binding so that recovery never silently redirects a
//! pending effect to the currently configured implementation.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ids::BindingId;

/// Lifecycle state of a binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingState {
    /// Staged for activation but not yet serving new work.
    Staged,
    /// Active and eligible for new operations.
    Active,
    /// Draining: existing operations retain it; it receives no new work.
    Draining,
    /// Retired; retained only for reconciliation and recovery.
    Retired,
}

/// A versioned record selecting one implementation behind a component API.
///
/// `required_capabilities`/`optional_capabilities` express the requested
/// capability profile (e.g. `mvp-baseline`, whose required set is empty). The
/// bind gate refuses any binding whose `required_capabilities` are unmet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRecord {
    pub binding_id: BindingId,
    pub component_api: String,
    pub api_version: u32,
    pub implementation: String,
    pub implementation_version: String,
    pub capabilities: BTreeSet<String>,
    pub required_capabilities: BTreeSet<String>,
    pub optional_capabilities: BTreeSet<String>,
    pub config_hash: String,
    pub generation: u64,
    pub state: BindingState,
}
