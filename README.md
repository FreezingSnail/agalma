# Agalma

> A software factory designed for continuous operation. It evaluates, builds, and ships its target.

Named after *Agalma*, a genus of siphonophore: colonial deep-sea organisms built from specialized parts that are continuously replaced, so the colony itself persists indefinitely. Agalma the factory borrows the model, not the vocabulary.

## The loop

```
recon → design → build → verify → ship → iterate
```

The intended loop continually evaluates the target, chooses work, and executes it. Current runs are supervised and do not yet operate unattended.

## Roles

Canonical role definitions (`docs/architecture.md` §3.5):

- **orchestrator** — the conductor: runs the loop, schedules work, enforces policy
- **researcher** — external recon and scanning
- **planner** — breaks the target into tasks
- **builder** — implements
- **verifier** — diagnoses failures and reviews output against acceptance criteria
- **evaluator** — rubrics and paired bench assessments against immutable candidates
- **postmortem** — distills run outcomes into durable notes and memory

## What exists today

A Rust workspace implementing the factory core. The conductor drives tasks from `bd`, executes them in isolated scratch checkouts via OpenCode, verifies acceptance, merges on green, and records steps in a durable SQLite ledger. Tier-1 components (harness, workspace, task queue, ledger, execution) have versioned Agalma contracts and can run behind framed-JSON process boundaries (`agalma-ipc`). OS-level confinement was removed by design; see [sandbox-removal-design.md](docs/sandbox-removal-design.md).

| Crate | Responsibility |
|---|---|
| `agalma-contracts` | Typed APIs, canonical schemas and IDs, no vendor types |
| `agalma-conductor` | Composition root and CLI (`agalma`) |
| `agalma-ledger` | SQLite ledger: atomic commits, receipts, export/import |
| `agalma-execution` | Execution service: serial executor, receipts, reconciliation |
| `agalma-kernel` | Durable execution kernel (B0): history, timers, cancellation, upgrade |
| `agalma-process` | Local process launcher: process-group launch, liveness, evidence-backed tree-kill |
| `agalma-workspace` | Git checkout, merge, reconcile, revert |
| `agalma-taskqueue` | `bd` backlog adapter |
| `agalma-decision` | Decision contract, static policies, model routing |
| `agalma-harness-opencode` | OpenCode v2 adapter (`opencode serve`, SSE normalization) |
| `agalma-ipc` | Framed JSON transport: clients/server runtimes for extracted boundaries |
| `agalma-harness-reference` | Deterministic reference adapter for parity tests |

### CLI

```
agalma run        Run (or resume) the conductor against a fixture task
agalma work       Queue-driven work loop over a real repository
agalma reconcile  Reconcile the bd projection against the ledger (ledger wins)
agalma resume     Clear the kill latch and resume dispatch (human-only)
agalma status     Report ledger and execution status
agalma digest     Print the recorded digest for an execution
agalma models     Print the model routing table and active routes
agalma gc         Reap terminal executions' heavy scratch from a state dir
```

## Milestones

| Milestone | State | Evidence |
|---|---|---|
| M0 — skeleton | ✅ | `docs/m0-exit-evidence.md` (recovery matrix 6/6, kill switch, schema gate) |
| M1 — working loop | ✅ | `docs/m1-exit-evidence.md` (seeded batch on a scratch clone, metrics, digest) |
| B0 — durable execution kernel | ✅ | `docs/b0-exit-evidence.md` (protected recovery pack 10/10, upgrade/rollback, resource baseline) |
| B1 — component extraction | ✅ | `docs/b1-exit-evidence.md` (Tier-1 seams behind process boundaries; parity/rebind/rollback per boundary) |
| M2 — bench + promotion + nerve | remaining work | [self-hosting roadmap](docs/self-hosting-roadmap.md) |
| M3 — always-on baseline | planned | MVP exit |
| P1 — autonomy gate | planned | removes transitional human review |

Operational posture: the current production defaults use process transports for the
harness, ledger, workspace, and task queue; `AGALMA_*_TRANSPORT=in-process` retains
the in-process override. The process launcher is in-process by design (no server
boundary exists). Execution/kernel remains
in-process by default; its production integration is planned in CV.1
(`agalma-pob.1`). Work is supervised, one task at a time, on a scratch clone; the
factory does not yet run unattended. Peer socket bounds and smoke evidence are in
[`docs/b1-f3-evidence.md`](docs/b1-f3-evidence.md) §6 (`agalma-fez`); auxiliary
ledger connections remain migration work (`agalma-1o3`).

The hub platform is partially delivered: an independently installed dashboard
plugin serves the operator cockpit, and `agalma work --watch` supervises it with
live factory reads. The remaining hub work (dynamic registry UI, generic reports,
actions, artifacts) stays on the [self-hosting roadmap](docs/self-hosting-roadmap.md);
the cockpit scope is specified in the
[browser dashboard design](docs/web-dashboard-design.md).

## Verified behavior

- **Recovery:** crash mid-build terminates survivors before redispatch; crash after merge completes without re-merge; duplicate delivery returns the recorded receipt; kill latch survives restart; stale-lease workers cannot advance state.
- **Durability:** restart during retry delay fires the timer once; directive updates survive restart; compatible upgrades resume pending work and preserve IDs/receipts.
- **Process lifecycle:** workers launch as process-group leaders via `agalma-process`; termination proves the descendant tree is gone. OS confinement and egress denial were removed by design ([sandbox-removal-design.md](docs/sandbox-removal-design.md)); the provider proxy remains for credential handling.
- **Replacement:** S0d bind gate refuses unmet capabilities; dual-binding rebind works between attempts; rollback reconciles before dispatch.
- **Extraction (B1):** all Tier-1 seams process-hosted behind framed JSON contracts; per-boundary shadow parity, staged rebind, and rollback evidence (`docs/b1-exit-evidence.md`); architecture test forbids impl→impl dependencies.
- Fixtures in `fixtures/recovery-pack/` are immutable (`MANIFEST.sha256`, `pack_integrity.rs`).

## Development

Repository collaboration follows [AGENTS.md](AGENTS.md) and the
[development workflow](docs/development-workflow.md). Sol owns architecture,
planning, and review using the [Sol architect persona](agents/sol-architect.md).
All code implementation goes to GPT-6 Luna or DeepSeek Flash through OpenCode.

Build, verify, and maintain the workspace with the root Makefile — the single
entry point for routine builds, scoped checks, and build-artifact hygiene:

```sh
make             # same as make all / make build; debug profile
make release     # same targets in the release profile
make test-focused SELECTION='-p <crate> --test <target> [filter]'
make test-peers  # five local process smokes on the default Connect wire
make test-full   # workspace tests; excludes ignored live-provider checks
make start       # live cockpit: real work --watch runtime (dispatches eligible work)
make preview     # offline fixture cockpit preview at http://127.0.0.1:8791/
make report      # sizes of every target/ consumer
make sweep       # classify worktree caches; sweep-apply reclaims them
make help        # targets, scope, outputs, and overrides
```

The build commands compile all Cargo workspace targets (libraries, binaries,
examples, tests, and benches) without running tests. Standalone spikes and
fixture projects are excluded. Defaults are `--offline --locked`; builds reuse
Cargo's normal `target/debug/` or `target/release/` outputs. `CARGO` selects the
Cargo executable, `CARGO_FLAGS` replaces the default flags as a whole,
`CARGO_TARGET_DIR` selects an alternate cache, and `CARGO_INCREMENTAL` defaults
to `0` (set `1` for fast local iteration). Build inside an implementation
worktree with `CARGO_TARGET_DIR=target/worktree-caches/<bead>/target`; see the
[development workflow](docs/development-workflow.md) for details.

Choose and state a focused, peer, or full test scope before running checks; the
Makefile target for the scope records the exact command, raw log, and exit status
under `target/test-runs/<bead>/<run-id>/`. The workflow defines the unified
`scripts/test.sh` runner, which is pending implementation; the Makefile forwards
to it once it exists. A full workspace run is not the default check for every
change, and process checks may need separately built server binaries.

```sh
make test-full                    # full workspace tests; excludes ignored live checks
cargo run --offline -p agalma-conductor -- run \
  --fixture fixtures/target-template --once \
  --state-dir target/test-runs/e2e
```

Verification runs belong under `target/test-runs/`; worker reports and handoffs
are retained under `target/worker-output/`. Treat both as evidence, including
frozen records; hygiene targets never sweep them. `target/debug/` and
`target/release/` are active build outputs; do not remove all of `target/` while
sessions may be using its binaries. Reclaim an obsolete isolated cache with
`make sweep-apply`, which preserves registered, dirty, and unmerged worktrees and
all evidence. Dependency versions are pinned exactly in `Cargo.toml`; no new
crates without explicit approval
([`docs/m0-dependencies.md`](docs/m0-dependencies.md)).

OpenCode attempts disable native snapshots; Agalma uses workspace Git for rollback.
The harness clears stale runtime folders before launch and removes them after stop
or drop once the process group is gone. Server logs remain under the attempt root
as `opencode-log-<unique-id>/`; workspace files are preserved. Set
`AGALMA_HARNESS_KEEP_STATE=1` to retain runtime state after stop for debugging
(the next launch still clears stale state).

## Docs

- `docs/architecture.md` — full architecture, components, roadmap
- `docs/component-contracts.md` — nano service contracts and tiers
- `docs/m0-skeleton.md`, `docs/m1-workloop.md` — milestone designs
- `docs/durable-execution-bootstrap.md` — B0 requirements; `docs/b0-kernel.md` — kernel design
- `docs/b1-extraction.md` — B1 design (transport, shadow parity, serial order); `docs/b1-exit-evidence.md` — per-boundary extraction evidence
- `docs/reliability-hardening.md`, `docs/hardening-evidence.md` — watchdog, routing, xcrun/TMPDIR
- `docs/research/`, `docs/spikes/` — recon and spike reports
- [Self-hosting roadmap](docs/self-hosting-roadmap.md) — current gaps and planned platform sequence
- [Browser dashboard design](docs/web-dashboard-design.md) — cockpit design, delivered scope, and live/fixture start targets
- [Development workflow](docs/development-workflow.md) — build and verification guidance

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).
