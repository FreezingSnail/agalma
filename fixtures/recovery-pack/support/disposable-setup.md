# Disposable repo / database setup

Every recovery case runs against throwaway state created fresh per run, under
`target/test-runs/recovery-<case-id>/` (never a system temp directory, never the
live ledger, never a real repository).

## Layout per run

```
target/test-runs/recovery-<case-id>-<pid>-<nanos>/
  ledger.sqlite            # isolated SQLite ledger (schema from agalma-ledger)
  effects/<case-id>.ndjson # append-only fake-effect journal (see effect-recorder.md)
  runs/<execution-id>/     # disposable checkout tree (git repos)
```

## Database

- Open with `agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite"))` via
  `harness::open_isolated_ledger(case_id)`.
- A crash child and its parent must point at the **same** run directory so the
  surviving `ledger.sqlite` is reopened for recovery.
- The ledger enforces the physical `schema_version`; a mismatch parks rather than
  auto-migrating (`tests/support` never forces a migration).

## Git repositories

- Cases that exercise merge/rollback create their git repo inside the run
  directory (`runs/<execution-id>/checkout`). Use `git init`, commit a
  candidate branch, and let the fake integrate activity create the merge ref.
- Assertions read refs with `git rev-parse` (e.g. `main == candidate/<execution>`)
  and commit counts with `git rev-list --count`; the pack expects "merge landed
  once, not repeated".
- Never run git commands in the workspace repository from a case.

## Processes

- Stale-lease cases may spawn a real grouped sleeper process (`/bin/sleep` in its
  own process group) to stand in for a surviving worker, then assert it is
  terminated with recorded evidence before redispatch. Reap the child in the
  test process.

## Cleanup

Run directories are disposable. Tests may `remove_dir_all` only the specific
`target/test-runs/recovery-*` directory they created; the base is confined to the
workspace `target/` tree.
