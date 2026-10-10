# Agalma

> An always-running software factory. It evaluates, builds, and ships its target — and never stops.

Named after *Agalma*, a genus of siphonophore: colonial deep-sea organisms built from specialized parts that are continuously replaced, so the colony itself persists indefinitely. Agalma the factory borrows the model, not the vocabulary.

## The loop

```
recon → design → build → verify → ship → iterate
```

The loop never terminates. Each pass evaluates the current state of the target, decides the next task, and executes it.

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

A Rust workspace implementing the factory core. The conductor drives a queue of tasks from `bd`, executes them in confined sandboxed checkouts via OpenCode, verifies acceptance, merges on green, and records every step in a durable SQLite ledger. Every Tier-1 component (harness, sandbox, workspace, task queue, ledger, execution) implements a versioned Agalma contract and can run behind a process boundary over framed JSON (`agalma-ipc`).

| Crate | Responsibility |
|---|---|
| `agalma-contracts` | Typed APIs, canonical schemas and IDs, no vendor types |
| `agalma-conductor` | Composition root and CLI (`agalma`) |
| `agalma-ledger` | SQLite ledger: atomic commits, receipts, export/import |
| `agalma-execution` | Execution service: serial executor, receipts, reconciliation |
| `agalma-kernel` | Durable execution kernel (B0): history, timers, cancellation, upgrade |
| `agalma-sandbox` | macOS Seatbelt confinement, process-group kill |
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
```

## Milestones

| Milestone | State | Evidence |
|---|---|---|
| M0 — skeleton | ✅ | `docs/m0-exit-evidence.md` (recovery matrix 6/6, kill switch, schema gate) |
| M1 — working loop | ✅ | `docs/m1-exit-evidence.md` (seeded batch on a scratch clone, metrics, digest) |
| B0 — durable execution kernel | ✅ | `docs/b0-exit-evidence.md` (protected recovery pack 10/10, upgrade/rollback, resource baseline) |
| B1 — component extraction | ✅ | `docs/b1-exit-evidence.md` (Tier-1 seams behind process boundaries; parity/rebind/rollback per boundary) |
| M2 — bench + promotion + nerve | next | unblocked by B1 |
| M3 — always-on baseline | planned | MVP exit |
| P1 — autonomy gate | planned | removes transitional human review |

Operational posture: extraction is complete — components are replaceable through their
declared APIs with dual-binding parity, staged rebind, and rollback evidence. Production
transports default to in-process (process transport is selectable via `AGALMA_*_TRANSPORT`
env vars). Execution is supervised, one task at a time, on a scratch clone — the factory
does not yet run unattended. Open follow-ups: the kernel candidate's `step`/`recover`
adapter gap (`agalma-5de`), auxiliary ledger connections (`agalma-1o3`), and the live
process-transport smoke + default flip (`agalma-rvj`).

## Verified behavior

- **Recovery:** crash mid-build terminates survivors before redispatch; crash after merge completes without re-merge; duplicate delivery returns the recorded receipt; kill latch survives restart; stale-lease workers cannot advance state.
- **Durability:** restart during retry delay fires the timer once; directive updates survive restart; compatible upgrades resume pending work and preserve IDs/receipts.
- **Confinement:** workers launch under Seatbelt; harness egress goes through a parent-owned loopback proxy; verification runs in a pristine sandboxed checkout.
- **Replacement:** S0d bind gate refuses unmet capabilities; dual-binding rebind works between attempts; rollback reconciles before dispatch.
- **Extraction (B1):** all Tier-1 seams process-hosted behind framed JSON contracts; per-boundary shadow parity, staged rebind, and rollback evidence (`docs/b1-exit-evidence.md`); architecture test forbids impl→impl dependencies.
- Fixtures in `fixtures/recovery-pack/` are immutable (`MANIFEST.sha256`, `pack_integrity.rs`).

## Development

```sh
cargo test --offline              # workspace tests
cargo run --offline -p agalma-conductor -- run \
  --fixture fixtures/target-template --once \
  --state-dir target/test-runs/e2e
```

All run artifacts live under `target/test-runs/` — never a system temp directory. Dependency versions are pinned exactly in `Cargo.toml`; no new crates without explicit approval (`docs/m0-dependencies.md`).

## Docs

- `docs/architecture.md` — full architecture, components, roadmap
- `docs/component-contracts.md` — nano service contracts and tiers
- `docs/m0-skeleton.md`, `docs/m1-workloop.md` — milestone designs
- `docs/durable-execution-bootstrap.md` — B0 requirements; `docs/b0-kernel.md` — kernel design
- `docs/b1-extraction.md` — B1 design (transport, shadow parity, serial order); `docs/b1-exit-evidence.md` — per-boundary extraction evidence
- `docs/reliability-hardening.md`, `docs/hardening-evidence.md` — watchdog, routing, xcrun/TMPDIR
- `docs/research/`, `docs/spikes/` — recon and spike reports

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).
