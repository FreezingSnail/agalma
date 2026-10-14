# TUI pilot reconciliation

Audited 2026-10-12 against current source and closed `agalma-3kr` delivery.
The ten `agalma-63m` implementation children duplicate delivered behavior and
are retired with individual references. This does not certify that the factory
executed the staged pilot.

| Duplicate | Delivered bead | Implementation and checked behavior |
|---|---|---|
| `agalma-63m.1` | `agalma-3kr.2` | `status.rs`: absent ledger, schema/pending/binding snapshot, kill latch |
| `agalma-63m.2` | `agalma-3kr.7` | Workspace terminal dependency pins; `watch.rs` ModeGuard: drop, panic unwind, failed-enter restoration |
| `agalma-63m.3` | `agalma-3kr.6` | Contracts/ledger schema v4 control receipt methods; idempotent action identity and reopen persistence |
| `agalma-63m.4` | `agalma-3kr.3`, `.7` | `watch.rs`: refresh/quit, once frame, missing snapshot, terminal guard and draw loop |
| `agalma-63m.5` | `agalma-3kr.4` | `components.rs`: bindings, state labels and socket liveness; health remains the existing baseline |
| `agalma-63m.6` | `agalma-3kr.5` | `economics.rs`: known/unknown spend, allowance, digests and degradation/parked alerts |
| `agalma-63m.7` | `agalma-3kr.5` | `economics.rs`: ordered events/decisions and control receipts in timeline |
| `agalma-63m.8` | `agalma-3kr.6`, `.7` | `control.rs`, `watch.rs`: human policy, pause, typed resume, denials and receipts |
| `agalma-63m.9` | `agalma-3kr.6`, `.7` | `control.rs`, `watch.rs`: selected execution, typed cancel request, denial and request receipt |
| `agalma-63m.10` | `agalma-3kr.7` | `watch.rs`: pane composition, selection/focus, keybindings, exactly sized TestBackend frame |

The designer's proposed `tui/term.rs`, `timeline.rs` and `composition.rs` paths
are not reasons to duplicate working code: ModeGuard and UI composition live in
`watch.rs`; timeline lives in `economics.rs`. Conductor `composition.rs` owns
factory wiring and is not a TUI module.

Verification ran offline with nonzero test counts:

| Command | Passed |
|---|---|
| `cargo test --offline -p agalma-conductor --lib watch::tests` | 19 |
| `cargo test --offline -p agalma-conductor --lib status::tests` | 3 |
| `cargo test --offline -p agalma-conductor --lib components::tests` | 4 |
| `cargo test --offline -p agalma-conductor --lib economics::tests` | 6 |
| `cargo test --offline -p agalma-conductor --lib control::tests` | 4 |
| `cargo test --offline -p agalma-ledger --lib control_receipt` | 2 |

Total: 38 passed, zero failures. Five command logs and results are under
`target/test-runs/planning-tui-audit/`; the watch result was observed directly.
This audit changes planning records only.

Actual gaps have separate owners: SF.4 cancellation settlement (a request receipt
does not establish termination), HP.3/HP.6 supervised health and dynamic plugin
content, CV.2 public ledger convergence, SF.10 fresh full pipeline execution and
SF.11 prerequisite-aware review. `agalma-i5h.7` continues to establish staged
decomposition only. Duplicate closure does not add `review-approved` or create
live paid-run evidence.
