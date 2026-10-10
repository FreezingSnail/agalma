# Fake effect recorder

Recovery cases never touch the live ledger or real repositories. Every external
effect is represented by an append-only **effect journal**, one NDJSON file per
case, written by the fake activity handle in
`crates/agalma-kernel/tests/support/harness.rs` (`EffectJournal`, `FakeActivity`,
`RecordingActivity`).

## Journal location

```
target/test-runs/recovery-<case-id>/effects/<case-id>.ndjson
```

The run directory is created by `harness::open_isolated_ledger` /
`harness::recovery_dir`; it is disposable and lives under `target/` only (never a
system temp directory).

## Record format

One JSON object per line (NDJSON), in append order:

```json
{"seq":0,"operation_id":"op:exec:fix-answer:1:build@1","kind":"execute","outcome":"applied","recorded_at_unix_ms":1700000000000}
```

| field | meaning |
|-------|---------|
| `seq` | monotonic 0-based sequence within the file |
| `operation_id` | canonical `op:<execution>:<step>[@attempt]` from `agalma-contracts` |
| `kind` | `execute` (effect performed) or `reconcile` (survivor completed without repeating) |
| `outcome` | `applied` (new effect) or `replayed` (idempotent repeat / reconcile) |
| `recorded_at_unix_ms` | wall-clock stamp (not used for ordering) |

## Semantics

- `execute` is **idempotent by `operation_id`**: if an `execute` line already
  exists the call is a no-op and records nothing (mirrors
  `agalma_execution::ScriptedActivity`).
- Every line is flushed and `sync_all`'d before returning, so a crash injected
  after the effect leaves the line durably on disk — matching the design's
  "successful external effect may outlive the handler's completion record".
- `reconcile` always appends; it never repeats the effect.

## Crash injection

The harness reuses the `AGALMA_*_CRASH_AT` convention. A child test process sets
`AGALMA_KERNEL_CRASH_AT=<point>` and calls `harness::crash_hook(point)` at every
persistence and effect boundary; the matching call aborts (`SIGABRT`). Point
names are the `pub const`s in `tests/support/harness.rs`
(`after_transition_commit`, `after_effect_before_completion`,
`after_decision_commit`, `after_merge_before_completion`, `after_timer_commit`,
`after_directive_commit`, ...).

`journal-template.ndjson` in this directory is an inert example line; the real
journal is created at run time.
