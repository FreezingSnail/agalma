//! Canonical Agalma identifiers.
//!
//! IDs are opaque strings carried in typed newtypes. The grammar below is
//! documentation plus a derivation helper; consumers never parse the payload.
//!
//! - `TaskId`: fixture task id (`fix-answer`)
//! - `ExecutionId = exec:<task_id>:<generation>` (a deliberate rerun is a new
//!   generation)
//! - `OperationId = op:<execution_id>:<step>` (stable across delivery retries)
//! - `AttemptId = attempt:<n>`
//! - `SessionId = session:<attempt_id>:<n>`
//! - `ArtifactRef = artifact:<name>`
//! - `BindingId`: component binding identity

use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// The identifier grammar prefix.
            pub const PREFIX: &'static str = $prefix;

            /// Wrap an existing opaque identifier string.
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            /// Borrow the opaque identifier string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

define_id! {
    /// Fixture task id (`fix-answer`).
    TaskId, ""
}

define_id! {
    /// Execution id: `exec:<task_id>:<generation>`.
    ExecutionId, "exec:"
}

define_id! {
    /// Operation id: `op:<execution_id>:<step>`.
    OperationId, "op:"
}

define_id! {
    /// Attempt id: `attempt:<n>`.
    AttemptId, "attempt:"
}

define_id! {
    /// Session id: `session:<attempt_id>:<n>`.
    SessionId, "session:"
}

define_id! {
    /// Artifact reference: `artifact:<name>`.
    ArtifactRef, "artifact:"
}

define_id! {
    /// Component binding id.
    BindingId, "binding:"
}

impl ExecutionId {
    /// Derive `exec:<task_id>:<generation>`.
    pub fn derive(task: &TaskId, generation: u32) -> Self {
        Self(format!("exec:{}:{generation}", task.as_str()))
    }
}

impl OperationId {
    /// Derive `op:<execution_id>:<step>`.
    pub fn derive(execution: &ExecutionId, step: &str) -> Self {
        Self(format!("op:{}:{step}", execution.as_str()))
    }
}

impl AttemptId {
    /// Derive `attempt:<n>`.
    pub fn derive(n: u32) -> Self {
        Self(format!("attempt:{n}"))
    }
}

impl SessionId {
    /// Derive `session:<attempt_id>:<n>`.
    pub fn derive(attempt: &AttemptId, n: u32) -> Self {
        Self(format!("session:{}:{n}", attempt.as_str()))
    }
}

impl ArtifactRef {
    /// Derive `artifact:<name>`.
    pub fn derive(name: &str) -> Self {
        Self(format!("artifact:{name}"))
    }
}
