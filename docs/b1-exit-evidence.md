# B1 exit evidence — component extraction

Bead: `agalma-0ht.9` (B1.8). Epic: `agalma-0ht` (B1). Date: 2026-10-10.
Design: `docs/b1-extraction.md`. Base at B1 start: `d0ece01` (README), design
`c63f210`; HEAD at exit: `767c3b0`.

Epic exit criteria (`docs/durable-execution-bootstrap.md` §Follow-on bootstrap
project, `docs/architecture.md` §13 B1): the component map is realized where
replacement pressure exists; every extraction passed dual-binding evidence,
staged rebind, and rollback without state loss. An extraction is complete when
replacement through the declared API is proven, not when a process was split.

## Extractions

All Tier-1 seams now have a process boundary behind their frozen contract.
The transport is the framed JSON protocol from `agalma-ipc` (D1); clients and
server runtimes follow the same shape per boundary.

| Boundary | Bead | Commit | Evidence | Selection | Parity | Rebind | Rollback |
|---|---|---|---|---|---|---|---|
| `HarnessApi` | `agalma-0ht.3` | `dfa6142` | `docs/b1-harness-evidence.md` | `AGALMA_HARNESS_TRANSPORT` | in-process vs process, reference impl, transcript + artifact digest | between attempts, pinned in-flight op | without state loss |
| `SandboxApi` | `agalma-0ht.4` | `127f49f` | `docs/b1-sandbox-evidence.md` | supervised `SandboxProcess`/`SandboxSource` (spawned where a sandbox-backed server runs) | launch/alive/terminate incl. process-group kill | between operations | without state loss |
| `WorkspaceApi` | `agalma-0ht.5` | `78ab8e6` | `docs/b1-workspace-evidence.md` | `AGALMA_WORKSPACE_TRANSPORT` | prepare/diff/integrate/tag/revert, produced git state | between operations, pinned checkout | without state loss |
| `TaskQueueApi` | `agalma-0ht.6` | `53ebe0c` | `docs/b1-taskqueue-evidence.md` | `AGALMA_TASKQUEUE_TRANSPORT` | scratch bd store sequences, projection compare | between operations, pinned claim | without state loss |
| `LedgerApi` | `agalma-0ht.7` | `adf21d0` | `docs/b1-ledger-evidence.md` | `AGALMA_LEDGER_TRANSPORT` | recovery-pack scenarios, normalized snapshot | quiesced, snapshot export/import, pinned op settles on new server | without state loss |
| `ExecutionApi` (kernel candidate) | `agalma-0ht.8` | `767c3b0` | `docs/b1-execution-evidence.md` | `AGALMA_EXECUTION_TRANSPORT` (candidate only) | recovery-pack scenarios at engine-op level; `start`/`cancel`/`status` faithful over the contract | pinned `begin_lease` op across swap | prior binding survives with pending work |

Boundary resolution records, per extraction, are in each evidence doc's
deliverables/deviations sections, including the B1.7 finding that the seed
executor's activity closures are consumer code and cannot cross a process
boundary — the B0 kernel is the process-ready execution boundary.

## Cross-cutting gates

- **Architecture test.** B1.1 extended it with the Cargo.toml normal-dependency
  edge check (no impl→impl edges; `agalma-ipc` infrastructure). One temporary
  sanctioned server-host edge (harness→sandbox, B1.2) was removed in B1.3; both
  checks are strict at HEAD and green.
- **Contracts unchanged.** `git diff c63f210..HEAD --stat -- crates/agalma-contracts/`
  is empty: no schema, ID, trait, or error change across all extractions.
- **Consumer invariance.** Each evidence doc carries its `git diff --name-only`
  proof; conductor changes are construction-site/wiring only, phase logic and
  activity bodies unchanged. Kernel/execution source changes were seam wiring
  (generics, `Send` bounds, serde derives for the wire) — no behavior change.
- **Test suite.** At HEAD: `cargo test --offline` — 70 test-result groups, 0
  failures (workspace + all boundary process suites; real-binary and re-exec
  fallback paths both exercised).

## Limitations and open items

1. **`ExecutionApi` adapter gap (tracked: `agalma-5de`, B1-F1).** `step`/`recover`
   return `UnsupportedCapability` over the process boundary because effect
   dispatch needs a handler registry that can cross it. Parity for those flows is
   proven at the engine-op level (`ExecutionOps`). The kernel remains a candidate;
   the production default is still the seed executor (`docs/b1-execution-evidence.md`).
2. **Auxiliary ledger connections (tracked: `agalma-1o3`, B1-F2).** A few
   short-lived conductor helpers still open direct SQLite (secondary WAL)
   connections under `AGALMA_LEDGER_TRANSPORT=process`; the durable-state owner
   and export/import generation remain the supervised server.
3. **Process defaults and the execution deferral.** Harness defaults to process
   after `agalma-ud1`'s live Connect smoke. `agalma-fez` bounds peer sockets and
   changes ledger, workspace, and task queue to process defaults; sandbox is
   always process. Explicit `in-process` overrides remain available. Execution
   keeps its in-process default until CV.1 (`agalma-pob.1`) integrates the kernel
   into the production work loop. See `docs/b1-f3-evidence.md` §§5–6.
4. **Live OpenCode process path.** `agalma-ud1` proved the default process path
   with a paid Connect-wire run on an isolated scratch clone. FEZ peer smokes
   exercise real local server processes without inference; they do not establish
   unattended operation or the later plugin platform acceptance.

## Verdict

The component map is realized for the harness, sandbox, workspace, task-queue,
and ledger boundaries with full dual-binding parity, staged rebind, and rollback
evidence; the execution boundary is realized at the kernel's engine surface with
the `ExecutionApi` adapter gap explicitly tracked (`agalma-5de`). Residual items
are tracked as `agalma-1o3` and `agalma-rvj`. Epic `agalma-0ht` closes on this
record.
