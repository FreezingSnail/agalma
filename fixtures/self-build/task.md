# Scope

Implement the bounded repository feature in the scratch Agalma origin: make the
library surface return the protected expected answer. This fixture is the
complete, frozen task definition used by the SF.9 epic-exit self-build proof
(`crates/agalma-conductor/tests/sf_self_build.rs`); the conductor freezes this
text verbatim into a `TaskPackage/v1` before any builder dispatch.

# Files

- `src/lib.rs` (allowed)

# Contract

- Provide `pub fn answer() -> u32` returning `42`.
- Keep the crate a valid Rust library (`--crate-type lib`).
- The recorded acceptance command and the operator protected baseline both run;
  both must pass before the candidate integrates.

# Invariants

- The protected baseline independently compiles the candidate and asserts
  `answer() == 42`, so a comment containing the expected text cannot spoof it.
- Only `src/` is writable; `tests/` is protected. A candidate that touches an
  outside-scope or protected path must not ship.
- Restart and cancellation are safe: the frozen inputs are immutable and a
  cancelled or crashed execution can never integrate twice or after cancel.

# Non-goals

- Do not add dependencies, a second database, or a second scheduling path.
- Do not change the acceptance, the protected oracle, or any dependency receipt.
- Do not run paid or live provider inference: this fixture is deterministic
  scripted transport only.

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
