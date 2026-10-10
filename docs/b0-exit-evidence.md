# B0 exit evidence — durable execution kernel

Bead: `agalma-9n4.7` (B0.6). Epic: `agalma-9n4`. Base revision: `845fe4b`
(B0.5). This is the B0 exit artifact: the per-case gate report for the protected
recovery pack, the upgrade/activation/rollback procedure, the measured seed
resource baseline and proposed regression limits, and deviations/open items.

Design: `docs/b0-kernel.md`. Requirements:
`docs/durable-execution-bootstrap.md` (Bootstrap sequence, Acceptance and
evaluation). Fixtures: `fixtures/recovery-pack/` (untouched; `MANIFEST.sha256`
verifies, `pack_integrity.rs` green).

## Gate report — all 10 protected cases

The pack (`fixtures/recovery-pack/cases/*.json`) is immutable and judged by
`crates/agalma-kernel/tests/*`. Every case is covered by a named test; all pass
on this revision.

| # | Case | Test | File |
|---|------|------|------|
| 1 | `transition-commit-before-dispatch` | `crash_after_transition_commit_before_dispatch` | `tests/recovery.rs` |
| 2 | `duplicate-delivery-after-completion` | `duplicate_delivery_returns_recorded_receipt` | `tests/recovery.rs` |
| 3 | `restart-after-decision-commit` | `restart_after_decision_commit_reuses_recorded_outcome` | `tests/recovery.rs` |
| 4 | `merge-before-completion` | `merge_before_completion_reconciles_without_remerge` | `tests/recovery.rs` |
| 5 | `stale-lease-worker` | `stale_lease_worker_cannot_advance_after_conductor_failure` | `tests/ownership.rs` |
| 6 | `restart-during-retry-delay` | `restart_during_retry_delay_fires_timer_once` | `tests/waiting.rs` |
| 7 | `directive-update-restart` | `directive_update_survives_restart_and_applies_once` | `tests/waiting.rs` |
| 8 | `upgrade-with-pending-executions` | `upgrade_with_pending_executions_resumes_or_parks` | `tests/upgrade.rs` |
| 9 | `kernel-adapter-replacement` | `kernel_adapter_replacement_preserves_pending_work` | `tests/activation.rs` |
| 10 | `rollback-restore-reconcile` | `rollback_restore_reconciles_before_dispatch` | `tests/activation.rs` |

Supporting B0.6 tests:

| Concern | Test |
|---------|------|
| Candidate check is reducer-only, no external effects | `check_candidate_reconstructs_pending_state_without_effects` (`tests/upgrade.rs`) |
| Canonical snapshot round-trip preserves pending work + receipts | `export_import_round_trip_preserves_pending_work_and_receipts` (`tests/upgrade.rs`) |
| Import refuses incompatible format/schema | `import_refuses_incompatible_versions` (`tests/upgrade.rs`) |
| Staged activation settles a present in-flight effect | `staged_activation_happy_path_settles_present_effect` (`tests/activation.rs`) |
| S0d bind gate (unmet required capability) | `staged_activation_refuses_unmet_capabilities` (`tests/activation.rs`) |
| Seed resource baseline | `seed_executor_resource_baseline` (`tests/resources.rs`) |
| Pack integrity (manifest append-only) | `protected_pack_matches_manifest`, `every_case_file_matches_schema`, `tamper_detection_self_test` (`tests/pack_integrity.rs`) |

Cases 8/9/10 build a durable pending execution, so a compatible swap has work to
preserve and reconcile.

## Upgrade, activation, rollback (procedure)

Persisted contract versions live in `kernel_meta` (`exec_def_version`,
`kernel_version`) and the physical ledger schema version is checked
(`KernelEngine::EXPECTED_LEDGER_SCHEMA_VERSION`). An unknown `exec_def` version or
a mismatched schema parks every execution rather than guessing.

### Candidate check (reducer-only)

`agalma_kernel::check_candidate(&ledger, migrations)` replays every execution
from `execution_events` and compares the derived state with the `executions`
projection. It writes nothing and runs no external effect (verified by an empty
effect journal). It reports per-execution `Compatible`, `NeedsMigration`, or
`Parks`.

### Explicit migration

`MigrationRegistry` holds explicit `Migration`s. `KernelEngine::open_with_migrations`
applies a resolved `stored → exec_def/v1` migration **before** reconstructing
work, through `LedgerApi` only. Incompatible state with no migration parks.
`VersionLabelMigration` is the safe case (a byte-compatible relabel); it records
a durable `migration:<from>` marker. Real history-rewriting migrations register
additional `Migration` implementations.

### State export / import (compatible replacement)

`LedgerApi::export_state` / `import_state` produce/consume a canonical JSON
`StateSnapshot` (format header `agalma-state/v1`; `schema_version`;
`exec_def_version`; `kernel_version`; kill latch; every `kernel_meta` row incl.
worker records; lease generations; projection; history; receipts; dispatch
intents incl. their `consumed` flag; timers; directives; bindings). Import
refuses a format/schema mismatch atomically (nothing partially applied) and is
canonical: export → import → export is byte-identical. Canonical ids, receipts,
and pending work survive; consumer APIs are unchanged.

### Staged activation (`staged_activate`)

1. **Set latch** — physical conductor latch and the durable per-execution
   `KillLatch` command (blocks new and recovered dispatch).
2. **Settle/reconcile in-flight** — probe every pending operation through its
   **pinned** binding; a present effect is completed (never repeated), an absent
   effect is left pending for the post-activation resume, an ambiguous effect
   aborts activation with the latch still set.
3. **Stage candidate** — upsert the candidate as `Staged` after the bind gate
   (`required_capabilities ⊆ capabilities`).
4. **Swap binding** (S0d rebind rules) — previous active → `Draining` (retained
   for pinned operations), candidate → `Active`.
5. **Schema check** — refuse on a mismatched ledger schema.
6. **Clear latch** — resume. Events are recorded in
   `kernel_meta::activation_evidence:<component_api>`.

### Rollback (`rollback`)

Settle in-flight, mark the candidate `Retired` (kept for reconciliation),
reactivate the seed binding, keep every pending operation pinned to the binding
it started under, record `rollback_evidence:<component_api>`, resume. The seed's
recovery path stays available.

### Restore (`restore`)

Check schema compatibility first, then reconcile **ledger, bd, Git, processes**
through the `RestoreReconciler` seam before any dispatch, with the latch set. An
ambiguous domain parks the execution and leaves the latch set (nothing
dispatches on stale/ambiguous state); a clean reconcile clears the latch and
resumes. Evidence is recorded in `kernel_meta::restore_evidence:<execution>`.

## Resource gates — seed baseline

Reproducible measurement: `sh scripts/b0-seed-baseline.sh` (wraps
`cargo test -p agalma-kernel --offline --test resources -- --nocapture`). The
harness measures `agalma-execution` in-process and writes
`target/test-runs/b0-resources/seed-baseline.json`. Numbers below are a debug
(`unoptimized`) build on the development host (macOS, arm64); they are seed
reference values, not cross-host guarantees.

| Metric | Measured seed | Proposed numeric limit |
|--------|---------------|------------------------|
| Idle RSS | 4 400 KiB | ≤ 8 800 KiB (2.0×) |
| Idle CPU (200 ms sample) | 0.0 ms | ≈ 0 (no busy-wait) |
| Restart (reopen + recover) | 5.64 ms | ≤ 16.9 ms (3.0×) |
| Dispatch latency | 4 044 µs/op | ≤ 16 177 µs/op (4.0×) |
| DB growth | 27 405 B/op | ≤ 55 834 B/op (2.0× + 1 KiB) |

Benchmark shape: 25 sequential seed executions (150 logical steps), one
SQLite ledger with `synchronous=FULL` under `target/test-runs/b0-resources/`.
Correctness is a veto: a candidate must pass every case above regardless of
resource numbers; these limits bound *regressions* only.

## Verification (this revision)

```
cargo test -p agalma-kernel --offline        # all green (incl. cases 8/9/10, resources)
cargo test --workspace --offline             # all green
cargo build --workspace --offline            # ok
cargo clippy --workspace --all-targets --offline -- -D warnings   # ok
cargo fmt --check --all                       # ok
sh scripts/b0-seed-baseline.sh               # writes target/test-runs/b0-resources/seed-baseline.json
```

## Deviations and open items

- **Consumer API unchanged.** `export_state`/`import_state` were added to the
  `LedgerApi` contract (additive) and implemented for `SqliteLedger`; the
  `ExecutionApi` seed executor and conductor are untouched and all their tests
  pass. The kernel carries its own `EXPECTED_LEDGER_SCHEMA_VERSION = 3` so it
  still depends only on `agalma-contracts`.
- **Restore seam.** `RestoreReconciler` is a contract-level seam; B0.6 tests a
  deterministic fake. Wiring the live conductor's bd/Git/process reconciliation
  into it is integration (out of the B0 kernel crate), tracked as a follow-up.
- **Activation not yet switched on.** The staged-activation/rollback mechanics
  are implemented and tested, but the conductor still runs the seed binding;
  performing the controlled-restart rebind in the live conductor is the
  activation step owned by the accepted conductor.
- **Migration coverage.** Only `VersionLabelMigration` ships (a compatible
  relabel, which is what the current `exec_def/v1` needs). A future definition
  change adds a payload-rewriting `Migration`; the registry already supports it.
- **Kernel schema-mismatch park** is exercised via the candidate report and the
  snapshot schema refusal; a live ledger can only reach a non-3 schema through a
  future migration (the ledger refuses commits under a mismatch).
- **Resource limits** are derived from a debug build on one host and should be
  re-measured in release at promotion; the harness is re-runnable and
  deterministic in shape.
