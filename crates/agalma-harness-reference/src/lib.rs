//! Deterministic reference `HarnessApi` adapters (M1.7).
//!
//! Two implementations share one scripted engine and advertise distinct binding
//! identities so a staged rebind changes the recorded `implementation`:
//!
//! - [`ReferenceProfile::Success`] — `reference/v1`; applies the scripted change
//!   that makes a task's acceptance pass.
//! - [`ReferenceProfile::Red`] — `reference-fail/v1`; runs the same turns but
//!   leaves the acceptance red.
//!
//! The adapters are fully scripted: turns come from a JSON scenario file under
//! the attempt directory (a deterministic default is materialized when none is
//! supplied), usage is fixed, there is no clock, randomness, vendor, or network.
//! `CancelOperation` returns acknowledgment and termination separately; a cancel
//! turn reaches terminal `Failed` only after `cancel_operation`, and
//! `StopAttempt` returns cleanup evidence.

mod adapter;
mod scenario;

pub use adapter::{ReferenceHarness, ReferenceHarnessConfig, ReferenceProfile};
pub use scenario::{Mutation, ScriptedEvent, Turn, TurnScript};

use std::collections::BTreeSet;

use agalma_contracts::harness::{
    Describe, API_VERSION, CAP_CONTEXT_HOOKS, CAP_SYNTHETIC_FEEDBACK, HARNESS_API,
};

/// Canonical implementation name for a profile.
pub fn implementation(profile: ReferenceProfile) -> &'static str {
    match profile {
        ReferenceProfile::Success => "reference",
        ReferenceProfile::Red => "reference-fail",
    }
}

/// Canonical implementation version for a profile.
pub fn implementation_version(profile: ReferenceProfile) -> &'static str {
    match profile {
        ReferenceProfile::Success => "reference/v1",
        ReferenceProfile::Red => "reference-fail/v1",
    }
}

/// Capabilities advertised by a profile. The red profile advertises none so a
/// binding requiring `/` requesting capabilities can exercise the bind gate.
pub fn capabilities(profile: ReferenceProfile) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    if profile == ReferenceProfile::Success {
        caps.insert(CAP_SYNTHETIC_FEEDBACK.to_string());
        caps.insert(CAP_CONTEXT_HOOKS.to_string());
    }
    caps
}

/// `Describe` result for a profile.
pub fn describe_for(profile: ReferenceProfile) -> Describe {
    Describe {
        api: HARNESS_API.to_string(),
        api_version: API_VERSION,
        impl_name: implementation(profile).to_string(),
        impl_version: implementation_version(profile).to_string(),
        capabilities: capabilities(profile),
    }
}

/// Map an implementation name back to a profile (recovery/binding lookup).
pub fn profile_from_implementation(name: &str) -> Option<ReferenceProfile> {
    match name {
        "reference" => Some(ReferenceProfile::Success),
        "reference-fail" => Some(ReferenceProfile::Red),
        _ => None,
    }
}
