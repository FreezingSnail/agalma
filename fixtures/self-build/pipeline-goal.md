# Goal

Deliver the bounded scratch source feature that makes the library surface return
the protected expected answer, end to end: a fresh goal becomes a designed epic,
the epic is decomposed into validated approval-held children that stay open, a
human releases each child by resolving its native execution-approval gate, the
authorized builder ships the protected behavior, an independent review compares
the merged result with the epic design, and the epic closes.

# Scope

A single scratch Agalma origin whose `src/lib.rs` must provide
`pub fn answer() -> u32` returning `42`. The conductor's goal-to-closure pipeline
runs over it: concierge intake, designer decomposition, the native human
execution-approval gate, immutable task-package admission, protected behavioral
verification, integration, and prerequisite-aware review closure.

# Files

- `src/lib.rs` (allowed)

# Contract

- Provide `pub fn answer() -> u32` returning `42`.
- Keep the crate a valid Rust library (`--crate-type lib`).
- The recorded acceptance command (`sh -c 'grep -q 42 src/lib.rs'`) and the
  operator protected baseline both run; both must pass before a candidate
  integrates.

# Invariants

- The protected baseline independently compiles the candidate and asserts
  `answer() == 42`, so a comment containing the expected text cannot spoof it.
- Only `src/` is writable; `tests/` is protected.
- Children stay open and approval-held: each carries a native human
  execution-approval gate (a `gate` issue with `await_type=human`, the
  `human,execution-approval` labels, and `approval_for=<child>`, pinned by a
  direct `blocks` edge). Only an explicit human gate resolution releases a
  child. A cleared date, a decomposition approval label, or a malformed/absent
  gate grants no execution authority, and an unresolved gate alone never counts
  as approval.
- Review closes the epic only while its admitted design, child set, and blocking
  edges are unchanged; an edit between the reviewer dispatch and the approval
  withholds closure.

# Non-goals

- No dependencies, second database, or second scheduling path.
- No paid or live provider inference: this goal is deterministic scripted
  transport only.
- No change to the acceptance command, the protected oracle, or any dependency
  receipt.

```yaml
directive: directive/v0
target: agalma
ref: main
acceptance:
  - sh -c 'grep -q 42 src/lib.rs'
budget_usd: 0.00
priority: normal
family: feature
max_attempts: 2
```
