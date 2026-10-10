# Protected recovery fixture pack

Immutable acceptance cases for the B0 durable execution kernel. Authored by
`agalma-9n4.2` (B0.1); pinned by [`MANIFEST.sha256`](MANIFEST.sha256) and enforced
by `crates/agalma-kernel/tests/pack_integrity.rs`.

> **Append-only.** Once a pack file is committed its digest is frozen in the
> manifest. Future beads **must not edit, rewrite, or delete** existing pack
> files; candidates must never edit the fixtures (or tests) that judge them.
> Adding a case is a reviewed orchestrator change: add the file, then regenerate
> the manifest and update the expected-case list in `pack_integrity.rs`.

Design: `docs/b0-kernel.md` §"Protected recovery fixture pack";
requirements: `docs/durable-execution-bootstrap.md` §"Acceptance and evaluation".

## Layout

```
fixtures/recovery-pack/
  README.md                       # this file
  MANIFEST.sha256                 # sha256 of every pack file (relative paths)
  cases/*.json                    # one acceptance case per file
  support/effect-recorder.md      # fake-effect journal format + crash hooks
  support/disposable-setup.md     # disposable git repo / sqlite ledger setup
  support/journal-template.ndjson # inert example journal line
```

Each `cases/*.json` has `id`, `description`, `crash_point`, `expected_observable`,
and `fixture_refs` (exec def, fake activities, effect journal, disposable state,
support docs). Case `id` equals the file stem.

## Cases → planned tests

| Case id | Planned test | Owning bead |
|---------|--------------|-------------|
| `transition-commit-before-dispatch` | `crash_after_transition_commit_before_dispatch` | B0.2 `agalma-9n4.3` |
| `duplicate-delivery-after-completion` | `duplicate_delivery_returns_recorded_receipt` | B0.3 `agalma-9n4.4` |
| `restart-after-decision-commit` | `restart_after_decision_commit_reuses_recorded_outcome` | B0.3 `agalma-9n4.4` |
| `merge-before-completion` | `merge_before_completion_reconciles_without_remerge` | B0.3 `agalma-9n4.4` |
| `stale-lease-worker` | `stale_lease_worker_cannot_advance_after_conductor_failure` | B0.5 `agalma-9n4.6` |
| `restart-during-retry-delay` | `restart_during_retry_delay_fires_timer_once` | B0.4 `agalma-9n4.5` |
| `directive-update-restart` | `directive_update_survives_restart_and_applies_once` | B0.4 `agalma-9n4.5` |
| `upgrade-with-pending-executions` | `upgrade_with_pending_executions_resumes_or_parks` | B0.6 `agalma-9n4.7` |
| `kernel-adapter-replacement` | `kernel_adapter_replacement_preserves_pending_work` | B0.6 `agalma-9n4.7` |
| `rollback-restore-reconcile` | `rollback_restore_reconciles_before_dispatch` | B0.6 `agalma-9n4.7` |

## Harness

`crates/agalma-kernel/tests/support/` is the frozen contract B0.2+ tests consume:

- `harness.rs` — crash-injection hooks (`AGALMA_KERNEL_CRASH_AT` + `crash_hook`
  abort), the fake-activity `EffectJournal` recorder, and
  `open_isolated_ledger` (state under `target/test-runs/recovery-*`).
- `pack.rs` — SHA-256 + manifest verification used by the integrity gate.

## Integrity

Enforced by `cargo test -p agalma-kernel`:

- `protected_pack_matches_manifest` — every digest matches; no missing, changed,
  or unlisted file.
- `every_case_file_matches_schema` — required fields present, case set exact.
- `tamper_detection_self_test` — a scratch copy under `target/test-runs/` proves
  change/addition/removal are detected.

The manifest covers **every** file under this directory except `MANIFEST.sha256`
itself. Regenerate after an approved addition (same line format,
`<sha256>  <path>`, paths relative to this directory, forward slashes):

```sh
cd fixtures/recovery-pack
find . -type f ! -name MANIFEST.sha256 | sed 's|^\./||' | sort \
  | while read -r f; do printf '%s  %s\n' "$(shasum -a 256 "$f" | cut -d' ' -f1)" "$f"; done \
  > MANIFEST.sha256
```

Regeneration is a reviewed change, not a candidate action.
