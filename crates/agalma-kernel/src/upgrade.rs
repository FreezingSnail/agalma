//! Kernel persisted-contract versioning, candidate checks, and explicit
//! migrations.
//!
//! `docs/b0-kernel.md` §"Upgrade, activation, rollback" and
//! `docs/durable-execution-bootstrap.md` §"Incremental work" (5. Upgrade and
//! integration):
//!
//! - Persisted contract versions live in `kernel_meta`
//!   ([`crate::engine::META_EXEC_DEF_VERSION`], [`crate::engine::META_KERNEL_VERSION`]).
//! - A candidate build is checked against saved fixture databases and histories
//!   by **reducer-only reconstruction**: [`check_candidate`] replays every
//!   execution from history and compares it with the durable projection. It
//!   performs no external effect and writes nothing.
//! - Migrations are explicit and registered in a [`MigrationRegistry`]; state
//!   whose version is unknown and unmigrated parks rather than being
//!   reinterpreted.

use agalma_contracts::{ContractError, ExecutionId, LedgerApi};

use crate::engine::{EXPECTED_LEDGER_SCHEMA_VERSION, META_EXEC_DEF_VERSION};
use crate::exec_def::{ExecDef, EXEC_DEF_VERSION, KERNEL_VERSION};

/// An explicit migration between two persisted `exec_def` versions.
///
/// Migrations run only through [`LedgerApi`] (never an external effect) and are
/// applied once, at open, before any work is reconstructed.
pub trait Migration {
    /// Version this migration reads.
    fn source_version(&self) -> &str;
    /// Version this migration produces (must be [`EXEC_DEF_VERSION`]).
    fn target_version(&self) -> &str;
    /// Apply the migration to `ledger`.
    fn migrate(&self, ledger: &mut dyn LedgerApi) -> Result<(), ContractError>;
}

/// A migration that relabels an additive/compatible definition version.
///
/// Used when a prior definition is byte-compatible with the current one apart
/// from its version string (the safest kind of migration: no history rewrite).
/// It updates `exec_def_version` and records a durable `migration:<from>`
/// marker so the upgrade is auditable.
pub struct VersionLabelMigration {
    from: String,
    to: String,
}

impl VersionLabelMigration {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        VersionLabelMigration {
            from: from.into(),
            to: to.into(),
        }
    }
}

impl Migration for VersionLabelMigration {
    fn source_version(&self) -> &str {
        &self.from
    }

    fn target_version(&self) -> &str {
        &self.to
    }

    fn migrate(&self, ledger: &mut dyn LedgerApi) -> Result<(), ContractError> {
        ledger.kernel_meta_set(META_EXEC_DEF_VERSION, &self.to)?;
        ledger.kernel_meta_set(&format!("migration:{}", self.from), &self.to)?;
        Ok(())
    }
}

/// Registry of explicit migrations, searched by `(from, to)`.
#[derive(Default)]
pub struct MigrationRegistry {
    migrations: Vec<Box<dyn Migration>>,
}

impl MigrationRegistry {
    pub fn new() -> Self {
        MigrationRegistry {
            migrations: Vec::new(),
        }
    }

    /// Register a migration. A duplicate `(from, to)` pair is rejected.
    pub fn register(&mut self, migration: Box<dyn Migration>) -> Result<(), ContractError> {
        if self
            .resolve(migration.source_version(), migration.target_version())
            .is_some()
        {
            return Err(ContractError::Conflict(format!(
                "duplicate migration {} -> {}",
                migration.source_version(),
                migration.target_version()
            )));
        }
        self.migrations.push(migration);
        Ok(())
    }

    /// Resolve a migration from `from` to `to`, if registered.
    pub fn resolve(&self, from: &str, to: &str) -> Option<&dyn Migration> {
        self.migrations
            .iter()
            .find(|m| m.source_version() == from && m.target_version() == to)
            .map(|m| m.as_ref())
    }

    pub fn len(&self) -> usize {
        self.migrations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty()
    }
}

/// Per-execution outcome of a candidate compatibility check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionCheck {
    /// Reducer replay agrees with the durable projection.
    Compatible {
        execution_id: ExecutionId,
        revision: u64,
    },
    /// The execution is compatible once `from -> to` is applied.
    NeedsMigration {
        execution_id: ExecutionId,
        from: String,
        to: String,
    },
    /// The execution cannot be reconstructed: it parks rather than guessing.
    Parks {
        execution_id: ExecutionId,
        reason: String,
    },
}

/// Report of a reducer-only candidate check against a saved fixture state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateReport {
    /// Candidate kernel version.
    pub kernel_version: &'static str,
    /// Stored `exec_def_version`, when present.
    pub stored_exec_def_version: Option<String>,
    /// Physical ledger schema version observed.
    pub schema_version: u32,
    /// Whether the schema differs from the candidate's expected version.
    pub schema_mismatch: bool,
    /// Whether the stored `exec_def_version` is unknown to the candidate.
    pub unknown_exec_def: bool,
    /// Whether an explicit migration to the current version is available.
    pub migration_available: bool,
    /// Per-execution checks, ordered by execution id.
    pub executions: Vec<ExecutionCheck>,
}

impl CandidateReport {
    /// Whether every execution reconstructs cleanly with no migration needed.
    pub fn compatible(&self) -> bool {
        !self.schema_mismatch
            && !self.unknown_exec_def
            && self
                .executions
                .iter()
                .all(|c| matches!(c, ExecutionCheck::Compatible { .. }))
    }

    /// Executions that park under the candidate.
    pub fn parks(&self) -> usize {
        self.executions
            .iter()
            .filter(|c| matches!(c, ExecutionCheck::Parks { .. }))
            .count()
    }

    /// Executions that require an explicit migration.
    pub fn needs_migration(&self) -> usize {
        self.executions
            .iter()
            .filter(|c| matches!(c, ExecutionCheck::NeedsMigration { .. }))
            .count()
    }
}

/// Check a candidate build against a saved fixture database or history.
///
/// Reconstructs every execution by reducer-only replay from `execution_events`
/// and requires agreement with the `executions` projection. Performs **no
/// external effects** and writes nothing: this is the "check candidates against
/// saved histories" gate before a staged activation. An unknown `exec_def`
/// version is reported as [`ExecutionCheck::NeedsMigration`] when `migrations`
/// resolves one, otherwise as [`ExecutionCheck::Parks`].
pub fn check_candidate<L: LedgerApi>(
    ledger: &L,
    migrations: Option<&MigrationRegistry>,
) -> Result<CandidateReport, ContractError> {
    let schema_version = ledger.schema_version()?;
    let schema_mismatch = schema_version != EXPECTED_LEDGER_SCHEMA_VERSION;
    let stored = ledger.kernel_meta_get(META_EXEC_DEF_VERSION)?;
    let unknown_exec_def = stored
        .as_deref()
        .map(|version| ExecDef::for_version(version).is_none())
        .unwrap_or(false);
    let resolved_migration = stored
        .as_deref()
        .and_then(|version| migrations.and_then(|reg| reg.resolve(version, EXEC_DEF_VERSION)));
    let migration_available = unknown_exec_def && resolved_migration.is_some();

    let mut executions = Vec::new();
    for record in ledger.executions()? {
        let execution_id = record.execution_id.clone();
        if schema_mismatch {
            executions.push(ExecutionCheck::Parks {
                execution_id,
                reason: format!(
                    "incompatible ledger schema version {schema_version} \
                     (candidate expects {EXPECTED_LEDGER_SCHEMA_VERSION})"
                ),
            });
            continue;
        }
        if unknown_exec_def {
            let from = stored.clone().unwrap_or_default();
            if migration_available {
                executions.push(ExecutionCheck::NeedsMigration {
                    execution_id,
                    from,
                    to: EXEC_DEF_VERSION.to_string(),
                });
            } else {
                executions.push(ExecutionCheck::Parks {
                    execution_id,
                    reason: format!("unknown exec_def version {from:?}; no approved migration"),
                });
            }
            continue;
        }
        let events = ledger.events(&execution_id)?;
        let replayed = crate::engine::replay(&execution_id, &events)?;
        if replayed.matches_projection(&record) {
            executions.push(ExecutionCheck::Compatible {
                execution_id,
                revision: record.revision,
            });
        } else {
            executions.push(ExecutionCheck::Parks {
                execution_id,
                reason: "projection mismatch: reducer-derived state disagrees with the \
                         executions row"
                    .to_string(),
            });
        }
    }

    Ok(CandidateReport {
        kernel_version: KERNEL_VERSION,
        stored_exec_def_version: stored,
        schema_version,
        schema_mismatch,
        unknown_exec_def,
        migration_available,
        executions,
    })
}
