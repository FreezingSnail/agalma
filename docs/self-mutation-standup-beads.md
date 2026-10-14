# Self-mutation standup bead index

Updated 2026-10-12. Exact contracts: [revision 1](self-mutation-contracts.md).
Shared architecture/context: [standup](self-mutation-standup.md).
Design gate: `agalma-fjq`; [design receipt](self-mutation-design-receipt.json).
The 28 first-tranche implementation children are staged, decomposition-approved
and deferred to 2099. Design validation is separate from execution release.

Sequence: SF -> HP -> first SM tranche. M2 additionally blocks its MX extensions
epic; SF.11 prevents premature epic review while prerequisites remain open.

## SF: SF reliable self-build execution and rich Flash task delivery (agalma-5fy)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-5fy.1` | SF.1 general repository builder prompt with complete frozen context | `agalma-5fy.2`, `agalma-fjq` |
| `agalma-5fy.2` | SF.2 immutable task package: full issue design, rules, acceptance and recovery inputs | `agalma-fjq` |
| `agalma-5fy.3` | SF.3 execution-scoped history replay and survivor reconciliation before work dispatch | `agalma-5fy.2`, `agalma-fjq` |
| `agalma-5fy.4` | SF.4 durable per-execution cancellation that survives factory resume | `agalma-5fy.3`, `agalma-fjq` |
| `agalma-5fy.5` | SF.5 fail-closed stop admission for planning, sessions and plugin actions | `agalma-fjq` |
| `agalma-5fy.6` | SF.6 enforce candidate file scope and immutable protected verification inputs | `agalma-5fy.2`, `agalma-fjq` |
| `agalma-5fy.7` | SF.7 task cash admission, outstanding reservations and unknown-usage recovery | `agalma-5fy.2`, `agalma-5fy.5`, `agalma-fjq` |
| `agalma-5fy.8` | SF.8 builder-only Flash model selection with independent planner and reviewer routes | `agalma-5fy.2`, `agalma-fjq` |
| `agalma-5fy.9` | SF.9 scratch self-build evidence: Flash context, protected acceptance and recovery | `agalma-3ej`, `agalma-5fy.1`, `agalma-5fy.3`, `agalma-5fy.4`, `agalma-5fy.5`, `agalma-5fy.6`, `agalma-5fy.7`, `agalma-5fy.8`, `agalma-fjq`, `agalma-ud1` |
| `agalma-5fy.10` | SF.10 fresh goal to design staging execution review and epic closure proof | `agalma-5fy.11`, `agalma-5fy.9`, `agalma-9m2`, `agalma-cmq`, `agalma-fjq` |
| `agalma-5fy.11` | SF.11 prerequisite-aware epic review admission and closure recheck | `agalma-5fy.2`, `agalma-5fy.5`, `agalma-fjq` |

## HP: HP hub-and-spoke plugin platform with dynamic control-plane reporting (agalma-jmr)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-jmr.1` | HP.1 common plugin manifest, reporting, actions and component-ledger contracts | `agalma-5fy.10`, `agalma-5fy.9`, `agalma-fjq` |
| `agalma-jmr.2` | HP.2 discover independent plugin packages from configured directories | `agalma-fjq`, `agalma-jmr.1` |
| `agalma-jmr.3` | HP.3 dynamic plugin registry and supervised lifecycle with pinned generations | `agalma-fjq`, `agalma-jmr.2` |
| `agalma-jmr.4` | HP.4 common plugin IPC and capability-scoped hub routing | `agalma-fjq`, `agalma-jmr.1`, `agalma-jmr.3` |
| `agalma-jmr.5` | HP.5 durable plugin reports, component ledger storage and snapshot recovery | `agalma-fjq`, `agalma-jmr.4` |
| `agalma-jmr.6` | HP.6 registry-driven component navigation and declarative plugin data views | `agalma-fjq`, `agalma-jmr.5` |
| `agalma-jmr.7` | HP.7 authorized plugin actions with durable intents, receipts and recovery | `agalma-fjq`, `agalma-jmr.4`, `agalma-jmr.5` |
| `agalma-jmr.8` | HP.8 independent plugin install, UI reporting, action, restart and replacement proof | `agalma-fjq`, `agalma-jmr.3`, `agalma-jmr.6`, `agalma-jmr.7` |

## SM: M2 — bench + promotion + nerve (epic) (agalma-cg9)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-cg9.1` | SM.1 immutable genome and mutation candidate package with constrained lifecycle | `agalma-5fy.9`, `agalma-fjq`, `agalma-jmr.8` |
| `agalma-cg9.2` | SM.2 protected immutable benchmark and holdout pack for mutation evaluation | `agalma-cg9.1`, `agalma-fjq` |
| `agalma-cg9.3` | SM.3 paired immutable baseline/candidate benchmark runner with complete accounting | `agalma-cg9.1`, `agalma-cg9.2`, `agalma-fjq` |
| `agalma-cg9.4` | SM.4 independent evaluator and deterministic qualification with regression veto | `agalma-cg9.3`, `agalma-fjq` |
| `agalma-cg9.5` | SM.5 bounded mutation proposals produce complete staged factory tasks | `agalma-cg9.1`, `agalma-cg9.4`, `agalma-fjq` |
| `agalma-cg9.6` | SM.6 durable evidence-gated promotion of prompt and authorized plugin candidates | `agalma-cg9.4`, `agalma-cg9.5`, `agalma-fjq` |
| `agalma-cg9.7` | SM.7 canary regression detection and automatic receipted rollback | `agalma-cg9.6`, `agalma-fjq` |
| `agalma-cg9.8` | SM.8 full self-mutation proof: retained win, rejected candidate and canary rollback | `agalma-9m2`, `agalma-cg9.2`, `agalma-cg9.5`, `agalma-cg9.7`, `agalma-cg9.9`, `agalma-cmq`, `agalma-fjq` |
| `agalma-cg9.9` | SM.9 directive-authorized prompt skill task release with durable admission receipts | `agalma-5fy.2`, `agalma-5fy.7`, `agalma-cg9.4`, `agalma-cg9.5`, `agalma-fjq`, `agalma-jmr.5` |

## Existing backlog and later tranches

- Router `agalma-myx.11`/`.12` consume HP.8 and the common registry/routing platform.
  SR.5/SR.11/SR.12 prohibit unchecked fallback/override and bespoke extraction.
- `agalma-qww`, `agalma-acs`, `agalma-3ej`, `agalma-ud1` are closed prerequisite
  deliveries; SF evidence reuses their receipts rather than refiling them.
- `agalma-9m2`/`agalma-cmq` remain independent SM prerequisite work.
- The duplicate `agalma-63m` epic and ten children are retired; see
  [TUI reconciliation](tui-pilot-reconciliation.md). `agalma-i5h.7` proves staging,
  while SF.10 owns the fresh full execution/review/closure proof.
- [Later roadmap bead index](self-mutation-roadmap-beads.md) contains MX/CV/M3/P1
  and runtime design gate `agalma-16k`, which requires actual HP.8/SM.8 evidence.

## Validation

All seven real plan-checks passed (61 children total); the dependency graph has
no cycles. Each child has explicit ownership, behavioral acceptance, versioned
context, a valid factory intake fence, rich Flash handoff and a direct design-gate
blocker. All remain deferred. The offline contract audit checks 19 positive
records, seven rejected records, four canonical vectors, artifact digests,
complete snapshot field coverage and the exact fake request reservation.
No production implementation, task release or paid run is claimed by this plan.
