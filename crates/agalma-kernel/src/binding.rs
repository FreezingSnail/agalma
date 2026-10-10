//! Binding pins: per-operation component/API binding versions.
//!
//! When the kernel dispatches an operation it resolves the component's active
//! [`BindingRecord`] and records a lean [`BindingPin`] alongside the operation.
//! Recovery reconciles against that *pinned* binding, never silently switching
//! to whatever adapter is currently configured (`docs/b0-kernel.md`
//! §"Activity protocol").
//!
//! A pin carries only identity/version fields (no capability sets, no config
//! hash): enough to detect that the bound implementation or API changed between
//! dispatch and reconciliation.

use agalma_contracts::{BindingId, BindingRecord, BindingState, ContractError, LedgerApi};
use serde::{Deserialize, Serialize};

/// The component/API binding an operation was dispatched under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPin {
    pub binding_id: BindingId,
    pub component_api: String,
    pub api_version: u32,
    pub implementation: String,
    pub implementation_version: String,
}

impl BindingPin {
    /// Pin the identity/version fields of `record`.
    pub fn from_record(record: &BindingRecord) -> Self {
        BindingPin {
            binding_id: record.binding_id.clone(),
            component_api: record.component_api.clone(),
            api_version: record.api_version,
            implementation: record.implementation.clone(),
            implementation_version: record.implementation_version.clone(),
        }
    }

    /// Whether `record` still resolves to the pinned API + implementation.
    pub fn still_pins(&self, record: &BindingRecord) -> bool {
        record.binding_id == self.binding_id
            && record.component_api == self.component_api
            && record.api_version == self.api_version
            && record.implementation == self.implementation
            && record.implementation_version == self.implementation_version
    }
}

/// The active binding for `component_api`, pinned, or `None` when unbound.
///
/// Deterministic: `LedgerApi::bindings` is ordered by binding id, so ties on
/// multiple active bindings resolve to the first.
pub fn active_binding<L: LedgerApi>(
    ledger: &L,
    component_api: &str,
) -> Result<Option<BindingPin>, ContractError> {
    Ok(ledger
        .bindings()?
        .iter()
        .find(|record| {
            record.component_api == component_api && record.state == BindingState::Active
        })
        .map(BindingPin::from_record))
}

/// The recorded binding with `binding_id`, pinned, or `None` when retired.
pub fn pinned_binding<L: LedgerApi>(
    ledger: &L,
    binding_id: &BindingId,
) -> Result<Option<BindingPin>, ContractError> {
    Ok(ledger
        .bindings()?
        .iter()
        .find(|record| &record.binding_id == binding_id)
        .map(BindingPin::from_record))
}
