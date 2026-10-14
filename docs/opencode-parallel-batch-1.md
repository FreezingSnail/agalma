# OpenCode parallel implementation batch 1

Human-authorized dispatch on 2026-10-13: the user requested deployment of the
accepted SF.2, SF.5 and transport-repair batch through the OpenCode MCP using
DeepSeek. This is a bounded human-bootstrap assignment, not automatic factory
release authority. Other staged tasks remain deferred. No live-provider
acceptance run, promotion, commit, push or epic closure is authorized here.

Implementation route: `opencode-go/deepseek-v4.1-flash`, project directory
`/Users/connorfranc/code/agalma`. Sol owns planning and review. Implementers must
read AGENTS.md, docs/development-workflow.md, their complete bead (description,
design, acceptance and notes), relevant closed prerequisite handoffs, and the
revision-1 contracts in docs/self-mutation-contracts.md before editing.

### Confirmed MCP dispatch

The retry on 2026-10-13 created these sessions. MCP status reported each as
active (`idle: false`, no outcome); the session list confirmed the model and
project directory above. This records dispatch, not completed acceptance.

| Lane / bead | OpenCode session ID |
|---|---|
| A / agalma-5fy.2 | `ses_ec9b95aeeffe0Wec4hX31T6weo` |
| B / agalma-5fy.5 | `ses_ec9b93b5fffeOnt2kazYcEy11M` |
| C / agalma-6w8 | `ses_ec9b920dbffeUui0uLCNGCby4X` |

Lane A still owns work.rs. Sol must review its handoff and explicitly transfer
ownership before Lane B makes the held integration change.

### Sol review checklist

Review the implementer's completed handoff and owned diff against the bead and
revision-1 contracts. Record commands, exit status, required cases actually
executed, ignored cases and raw log paths. A successful isolated/store test does
not establish a production admission or recovery claim. Do not rerun checks
unless the supplied evidence leaves a concrete uncertainty. Return findings to
the same implementer; Sol does not repair code.

| Lane | Required behavior and evidence |
|---|---|
| A / SF.2 | Production dispatch follows a durable package-admission commit. Crash before admission cannot dispatch or reconstruct inputs from live bd; crash after admission reads the frozen package. Real scratch bd edits to title/design/notes/acceptance and source deletion preserve original behavior and readable context refs. Corrupt digests, unsupported versions and missing legacy packages park without effects. Authorization provenance, original binding/model pins and FEZ behavior remain intact. |
| B / SF.5 | Recording harnesses prove zero new launches or queue mutations on unavailable/incompatible/unreadable established ledger state with STOP absent. Explicit fresh initialization, persisted latch, STOP during a session, human resume and bounded cleanup are covered. Bound LedgerApi is the authority; failed reads cannot become Running. Fleet/named-epic designer, review, concierge, session and work callers use the same interpretation. |
| C / transport | In-process client matches the selected sandbox server wire. A real sandbox exercises unset-wire default Connect and explicit Framed, with required cases executed. designer_payload behavior remains valid. Preserve process-harness default, READY deadlines and existing FEZ repairs; use mock/scripted inference. |

SF.2 review separates durable package admission from SF.3's forthcoming
history/projection and survivor-reconciliation gate. Passing SF.2 is not evidence
that the production kernel or the complete recovery platform has migrated.

Review order follows available completed handoffs. Lane C can finish independently.
After SF.2's owned changes and focused evidence are reviewed, explicitly release
work.rs to the existing Lane B session for its held integration. SF.5 remains
partial until that integration and its required cases pass review. If SF.2 needs
repair, it retains ownership and Lane B remains on its other files.

### Sol review results — 2026-10-13

Reviewed local source and supplied evidence while MCP was unavailable. Source
findings below describe the inspected snapshot; no new acceptance command was
run by Sol. The SF.2 final implementation handoff has not been received, so this
is a review of its current changes rather than a claim that its session finished.

**Lane C / agalma-6w8: APPROVED for the agreed focused scope.** The in-process
factory uses SandboxClient::connect_wire with sandbox_wire(), preserving process
default and explicit Framed. designer_payload now exercises that factory. The
real sandbox test reaches readiness after launch for unset/default Connect and
Framed; this proves the sandbox RPC seam, not a live provider session. Supplied
final.log records 2 designer_payload cases plus 1 real-sandbox case passing,
none ignored. lib-harness_process.log records 6 passing cases. The negative
control fails with the old framed/Connect mismatch. Existing FEZ startup and
parent-death changes are outside this lane's fix and were preserved. The owned
files are released. No full-workspace or paid-provider claim is made.

**Lane A / agalma-5fy.2: NEEDS CHANGES in the inspected snapshot.** Supplied
sf_task_package.log records 9 passing cases and work-regression.log records 5.
Those cases do not cover these contract gaps:

1. **The durable admission follows an already-announced dispatch intent.**
   work.rs run_task calls Executor::start before admit_task. The existing start
   commits a Running/Intake row with an unconsumed DispatchIntent. The comment
   calling it preparation does not introduce a non-dispatchable durable state.
   Revision-1 admission requires no dispatch intent before package admission,
   or a genuine non-dispatchable preparation transition. Repair this ordering
   within the owned seam; if the existing executor API cannot express it, report
   the exact required interface as a bounded ownership blocker instead of editing
   SF.3's executor.rs unilaterally. Extend crash evidence to assert the durable
   pre-admission row/intent state and absence of effects.

2. **Stored input integrity is incomplete.** load_package verifies the document's
   own digest but never compares it with the digest requested by the admission
   event. It also never validates the referenced context/receipt/prompt/oracle
   objects before recovered dispatch. build_package only stores context and
   authorization objects; role_inputs creates objects/builder-prompt.txt and
   objects/verifier-prompt.txt references without storing their bytes, and the
   oracle ref is likewise not materialized. Persist all required immutable input
   objects and verify the admitted digest and every required reference before
   effects. Missing/corrupt input must park. Include referenced-object corruption,
   missing prompt objects and a self-consistent wrong document under the requested
   store path in focused failure evidence. Propagate directory-open/fsync failures
   in write_atomic; ignored errors cannot establish durable admission.

3. **Recovery still substitutes current harness/prompt configuration.**
   role_plan_from_package restores only model/route strings. recover_inflight
   passes the current process-selected harness to build_executor; phase_deps uses
   mutable state_dir/prompts/builder.md and the current compiled verifier prompt.
   role_inputs records a synthetic generation 1 and digest of {} rather than the
   actual selected binding/config. Recover the original validated pins and prompt
   bytes, or explicitly park an unavailable/incompatible pin. Preserve the
   existing per-operation binding rules. Test restart after changing the harness
   selector and prompt file: originals must remain pinned or dispatch must refuse.
   This does not include SF.1's richer prompt rendering or SF.8's new selector.

4. **Declared context failures are silently dropped at admission.**
   gather_admission_context ignores errors loading a declared parent or dependency
   and permits the directive block to satisfy the nonempty context check. A
   required design/handoff can disappear while the task still dispatches. Refuse
   admission when a declared required source cannot be captured; preserve explicit
   empty sections and legitimate absence of optional context. Freeze the repository
   instruction documents referenced by AGENTS.md that govern this execution.
   Add focused unreadable/missing required-parent/dependency evidence with zero
   effects. Do not infer historical context on recovery.

Repair handoff when the route returns: send the existing Lane A session this
review section, retain its work.rs ownership, preserve Lane B/C edits, and request
the same focused sf_task_package command plus evidence for the named failure
paths. No contract redesign, new dependency, paid acceptance or other task release
is authorized. Sol must review the repaired diff before any transfer.

MCP reconnect follow-up: Gangway responded to ping and recovered the original
sessions. Both SF.2 and SF.5 were idle with implementation handoffs; SF.5 still
reported partial acceptance and held work.rs integration. The repair prompt was
sent to existing SF.2 session `ses_ec9b95aeeffe0Wec4hX31T6weo` on the confirmed
DeepSeek Flash route in /Users/connorfranc/code/agalma. MCP acknowledged it as
queued. This is confirmed repair delivery, not completed repair acceptance.
Lane A retains work.rs; SF.5's transfer and the deferred next-wave tasks remain
gated. Lane C stays closed following its focused Sol approval.

**Lane B / agalma-5fy.5: PARTIAL; held integration remains.** Its output.md reports
11 focused cases plus existing regressions passing. This is implementer-reported
evidence. The subsequent non-work.rs Sol review below found repairs that can run
while SF.2 retains its files. work.rs integration and
threading the same bound control through live sessions remain held. No ownership
transfer has been issued; Lane B's handoff is retained for the post-SF.2 follow-up.

**Lane B non-work.rs review: NEEDS CHANGES.** Source and supplied focused logs
were reviewed; no additional acceptance command was run by Sol. Keep the current
owned files and preserve Lane A/C changes. Repair these concrete admission gaps:

1. **Production checks bypass the selected ledger binding.** bind_stop_control
   unconditionally constructs LedgerHandle::in_process when the file exists;
   FileStopControl::read_ledger directly opens SqliteLedger. The default production
   binding is Process. Opening another database implementation is not reading the
   selected service's stop state, and can succeed while that service is unavailable.
   Standalone designer/concierge and LiveSession use these paths today. Route
   production checks through the selected/bound LedgerApi, pass the same control
   through the session seam, and refuse failed binding/read observations. Do not
   fall back to a private database when the bound service fails. Keep legacy
   inspection fixtures separate from production admission. Add a recording/failing
   LedgerApi case proving no launch or queue mutation when service reads fail even
   though a readable database file is present.

2. **Missing state is treated as fresh authority without initialization.**
   FileStopControl::decision returns Running when Path::exists is false; exists
   also hides metadata errors. bind_stop_control has the same existence shortcut.
   An inaccessible/missing established ledger can therefore look fresh, while
   first-open state is never explicitly initialized before permitting inference.
   Preserve the accepted fresh-start behavior through explicit successful
   initialization and a readable bound stop observation. Do not classify a failed
   read/bind/metadata observation as Running or silently recreate established
   control state. Cover fresh initialization, unavailable established state and
   metadata/read failures with zero new effects on uncertainty. Existing tests
   that simply dispatch without any ledger need to assert the accepted initialized
   path rather than encode the shortcut as policy. Explicit resume remains the
   only way to clear a persisted stop.

3. **Successful review output can cause effects after stop becomes denied.**
   review_epics_with_stop checks after reviewer.run only in its Err branch. An
   Ok verdict immediately calls apply_verdict, which can close an epic or create
   drift tasks even if the session set STOP or the ledger became unavailable.
   Recheck the same bound decision after the session returns and before verdict/
   error projection; preserve interrupted evidence without creating new queue
   effects. Cover a recording reviewer that returns a valid approval or drift
   verdict while changing the stop observation to Stopped and Unavailable. Also
   recheck before paid dispatch and attempt-label mutations after candidate reads.

Send this section to existing Lane B session ses_ec9b93b5fffeOnt2kazYcEy11M.
The focused command remains cargo test --offline -p agalma-conductor --test sf_stop;
include nonzero behavioral evidence for the named cases and exact raw logs/exit
status. No paid provider check, business-logic redesign or new task release is
included. Do not edit work.rs, phases.rs or task_package.rs. Report integration
that requires those held files as partial and keep the existing stopping limit.

Sol delivered this non-work.rs repair to the existing Lane B DeepSeek Flash
session via MCP after confirming it was idle. The follow-up was acknowledged
queued; the subsequent status showed idle:false. Lane B can repair these owned
files concurrently with Lane A's SF.2 repair. work.rs remains held by Lane A.

### Sol review of completed repair handoffs — 2026-10-13

**Result: NEEDS CHANGES for both SF.2 and SF.5.** The bridge confirms both
completed repair turns used `opencode-go/deepseek-v4.1-flash`, not GPT-6 Luna,
in `/Users/connorfranc/code/agalma`. Existing session IDs above remain the
implementation destinations. This review covers the current owned working diff
against `1726298`, the assigned beads, revision-1 contracts, and the completed
repair handoffs. It does not reopen the approved transport lane or frozen FEZ
receipt. No acceptance command or paid inference was run by Sol in this review.

Reviewed raw focused logs record SF.2 **17 passed, zero failed/ignored** and SF.5
**15 passed, zero failed/ignored**. Commands and reported exit 0 are in
`target/test-runs/opencode-batch-1/agalma-5fy.2/commands.log` and
`target/test-runs/opencode-batch-1/agalma-5fy.5/exit-status.txt`; raw output is
`sf_task_package-repair.log` and `sf_stop.log` in the corresponding directories.
The handoffs also report passing library and scoped regression runs. These are
implementation evidence, not a new reviewer-run suite. The required failure
paths below are not established by those green logs.

**SF.2 repaired portions:** loading checks the requested admission digest,
referenced objects are materialized and verified on recovery, directory fsync
errors propagate, changed harness selection refuses recovery, frozen builder
prompt bytes replace the mutable prompt file, and declared missing parent or
dependency retrieval fails instead of being silently omitted. Parent/dependency
failure cases are function-level evidence; the store/object failure cases must
not be described as independently proving zero production dispatch.

Remaining SF.2 findings:

1. **Admission ordering remains an acknowledged ownership blocker.**
   `work.rs:434` starts the execution before admission; `admit_task` commits only
   a separate admission event. The repaired crash fixture positively asserts
   that a Running/Intake row and unconsumed intent already exist before the
   reference commit. This contradicts the normative order; it is not acceptance
   of a non-dispatchable preparation state. Keep SF.2 incomplete. Its implementer
   has identified the missing executor preparation/admission seam and correctly
   stopped at the ownership boundary. Resolve that bounded interface ownership
   before implementation; do not unilaterally edit SF.3's executor or relax the
   frozen contract. Comments describing the current start as non-dispatchable
   preparation must match the eventual behavior.

2. **Role binding pins still do not describe the actual binding.**
   `work.rs:1113` calls `plan_binding(kind, 1, model)` for every execution, while
   the production phase binding uses the execution's real generation.
   `work.rs:1118` stores `binding.implementation` in `implementation_version`,
   discarding `binding.implementation_version`. Recovery checks only that
   misnamed value, config hash, and binding ID; it does not validate API version
   or generation. Persist the actual binding fields and validate the complete
   required pin before effects. Add a non-first-generation case and independent
   incompatible-version/API/generation fixtures. Continue to preserve existing
   per-operation pins rather than redirecting a pending operation.

3. **Required repository context can still disappear silently.**
   `task_package.rs:535` ignores a missing instruction document explicitly
   referenced by AGENTS.md. `read_repo_file` uses `Path::is_file`, which also
   turns metadata errors into optional absence. In this repository a missing
   `agents/sol-architect.md` or `docs/development-workflow.md` would therefore
   admit a package without its declared governing instructions. Keep legitimate
   absence of optional top-level sources distinct from an explicit required
   reference; a missing/non-file/unreadable required source must refuse
   admission. Add a missing referenced-instruction case and a metadata/read
   failure case, with production evidence that no handler executes. Do not
   reconstruct those documents from current files during recovery.

**SF.5 repaired portions:** standalone production callers select LedgerHandle
transport rather than force a private in-process backend, and pass one shared
control through the live session seam. Successful reviewer output is checked
again before verdict projection. The two post-session approval cases execute.

Remaining SF.5 findings:

1. **Missing established state is recreated as clear authority.**
   `stop.rs:360-375` unconditionally binds an initializing ledger when STOP is
   absent. No established-state check distinguishes a first initialization from
   a lost ledger. Both selected backends ultimately create a fresh SQLite
   database when the path is missing; its initial latch is clear. Losing a
   previously initialized/latched database can therefore become Running.
   Preserve successful explicit fresh initialization, but require established
   state to open without creating replacement authority. If expressing that
   distinction needs an unowned ledger initialization/open contract, report a
   bounded ownership blocker. Add fresh and established-missing cases that
   prove zero planning/reviewer/session launches and queue mutations on loss.

2. **STOP metadata errors still permit admission.**
   `BoundStopControl::evaluate` at `stop.rs:163` and the binding shortcut at
   `stop.rs:362` use `Path::exists`. That hides IO/permission errors; with a
   readable clear bound ledger, an unreadable sentinel observation can become
   Running. Use an explicit metadata observation that distinguishes NotFound
   from uncertainty in the production control. A failing sentinel observation
   must be Unavailable, with no new effects. The legacy ledger metadata change
   alone does not establish this behavior.

SF.5's held `work.rs` integration remains incomplete. Its focused tests force
in-process transport; they do not prove default-process composition. Retain the
same bound work-loop ledger when that integration is authorized, as its handoff
specifies. No work.rs transfer, bead closure, new task release, or repair dispatch
was made during this review. Next step: settle SF.2's bounded executor seam,
return the remaining findings to the existing implementers, and review their
focused failure-path evidence before transferring ownership.

### Next repair wave authorization and ownership — 2026-10-13

The user said DeepSeek is ready for the next wave. Continue the two existing
repair sessions on `opencode-go/deepseek-v4.1-flash` in this repository. The
latest compact status confirmed both idle after completed repair handoffs.
This wave completes the already accepted SF.2/SF.5 behavior; it releases no
deferred SF, HP, CV, Router or WEB child and no live-provider acceptance run.

Dispatch observation: MCP acknowledged both follow-ups as queued, then status
reported `idle:false` for Lane A `ses_ec9b95aeeffe0Wec4hX31T6weo` and Lane B
`ses_ec9b93b5fffeOnt2kazYcEy11M`, both on the confirmed Flash route and repository
directory. This is active repair dispatch, not completed acceptance. The status
response still carried the earlier turn's succeeded outcome; current activity
is established by `idle:false`, not that retained outcome field.

Both implementers read the preceding completed-repair review locally. This
section resolves its bounded ownership blockers without changing revision-1
package, authority, stop, recovery or acceptance contracts.

**Lane A / SF.2 ownership amendment:** add
`crates/agalma-execution/src/executor.rs` and its existing execution unit-test
surface, solely for admission-safe preparation/activation. SF.3 retains full
history validation and execution-scoped survivor recovery. No SF.3 dispatch
or work.rs transfer is included.

The bounded executor seam allocates execution/lease identity and commits a
genuinely Pending/Intake preparation with no dispatch intent. Pending cannot
dispatch or advance through step/recovery. After immutable objects are durable,
activate with one atomic batch containing the execution's Running transition,
matching package-admission event/reference, and first dispatch intent. Validate
the prepared identity/generation/lease before activation; duplicate activation
cannot announce another intent or accept changed admission inputs. Failed
admission/crash leaves non-dispatchable preparation that production recovery
parks, without reconstructing current bd inputs. Preserve legacy start semantics
for existing fixture callers and existing version/lease/effect-replay gates.
Do not duplicate the executor's private allocation/intent logic in work.rs.
Minimal preparation/activation history handling needed to keep this new state
non-dispatchable is allowed; complete SF.3 recovery remains outside this wave.

Use the real prepared generation and BindingRecord implementation version/API/
generation when freezing role pins. Validate all required pin fields before
recovered effects. Missing explicitly referenced governing instruction files,
non-files or metadata/read failures refuse admission; legitimately optional
absence and explicit empty sections remain valid. Preserve earlier digest,
prompt, source-deletion and FEZ repairs. First dispatch consumes the frozen
admitted values as well as recovery.

Lane A owns work.rs, task_package.rs, sf_task_package.rs and the bounded executor
seam; phases.rs changes remain limited to its existing SF.2 assignment if needed.
Do not change Lane B ledger/stop/session/planning files. Keep one focused command:
`cargo test --offline -p agalma-conductor --test sf_task_package`. Required
nonzero cases cover no pre-admission intent/effects, admission/intent atomicity,
crashes on both sides, duplicate/changed activation, non-first generation and
incompatible pins, missing required instruction and metadata/read failures,
referenced-object refusal on production recovery, and frozen source/prompt
behavior. Existing green failure assertions that expect the defect must change
to the independently required behavior, not weaken that behavior.

**Lane B / SF.5 ownership amendment:** add
`crates/agalma-conductor/src/ledger_process.rs`,
`crates/agalma-ledger/src/lib.rs`, `crates/agalma-ledger/src/server.rs`, and
`crates/agalma-ledger/src/tests.rs`, solely for explicit initialization versus
open-existing semantics through the selected binding. These files have no other
active owner in this batch. HP.5 and CV retain their later migrations/packaging;
do not rewrite historical FEZ receipts for the changed source.

Established admission state must be opened without CREATE or schema initialization.
Opening a missing/corrupt/incompatible established database is Unavailable.
Fresh bootstrap must explicitly initialize and observe the selected service's
clear latch before admitting effects. Retain a durable establishment record
outside the database so ordinary database loss cannot be mistaken for first use;
adopt existing initialized state before admitting it. A partial/ambiguous
initialization refuses effects. Never replace an established latch through a
create-if-missing path. Creation errors propagate, and sentinel metadata errors
are Unavailable rather than false. The selected process and in-process bindings
must enforce the same policy; failed binding never falls back to a private DB.

This is initialization/open behavior only: no live schema migration, restore,
policy redesign, ledger writer takeover or recovery-root change. If preserving
these invariants requires another public/storage contract, stop with its exact
bounded proposal instead of expanding scope. The common stop control must remain
shared through the session seam; preserve post-reviewer checks and interrupted
evidence.

Lane B may build the required ledger-server prerequisite once with
`cargo build --offline -p agalma-ledger --bin agalma-ledger-server`, then uses
`cargo test --offline -p agalma-conductor --test sf_stop`. Required nonzero cases
cover explicit virgin initialization, missing established state with a previously
latched ledger and STOP absent, incomplete initialization, sentinel metadata
errors, no launch/queue mutation on uncertainty, selected default-Connect process
binding and in-process policy, persisted stop, mid-session stop, bounded cleanup,
explicit resume and post-reviewer suppression. Missing process prerequisites or
skipped required cases are not passes. Do not limit evidence to forced in-process
fixtures or describe scripted session refusal as actual worker cleanup evidence.

**Shared discipline:** preserve others' edits and caches. Lane B owns the
ledger-server prerequisite build for this wave; both focused Cargo commands use
the established target cache and normal Cargo serialization. Put raw logs,
exact commands, exit status and executed/ignored counts in the existing task
artifact directories, with distinct next-wave filenames. Do not rerun full
suites, enable paid providers, commit, push or close beads. Stop after focused
acceptance or one reproducible blocker and at most 30 minutes of diagnosis.
Each handoff reports complete versus held scope. Sol reviews before releasing
work.rs to SF.5; then SF.5 integration must pass before the prepared new SF wave.

### Sol review of next repair wave — 2026-10-13

SF.2 and non-work.rs SF.5 remain **NEEDS CHANGES**. Sol reviewed the production
paths and focused raw logs; no additional reviewer test run was needed to identify
the following concrete failures. SF.2 reports 26 focused cases passing; SF.5
reports 19. The recent `make build` compiles all workspace targets successfully;
it does not establish these failure paths.

**SF.2 / executor.rs, Executor::activate:** the non-Pending branch returns an
operation even when no event of the supplied admission kind exists. Changing the
event kind after successful activation, or activating a parked preparation which
was never admitted, therefore reports success. That branch also precedes lease
validation. The Pending path compares only the caller's lease with the historical
preparation lease; it does not check the current task lease after another
preparation advances it. Require a genuine matching recorded activation/admission
for idempotent replay and validate prepared identity and lease before accepting
either branch. Reject changed event kind, missing admission/activation evidence,
altered preparation lease, and superseded task lease without committing a new
intent/event/transition. Preserve identical replay and legacy `start` behavior.
Validate both the preparation's recorded lease and the current task lease: a
caller must not substitute a newly current lease into an older preparation.
Replay evidence must identify the actual admitted event, so changing the kind
to another existing event (such as execution_prepared) cannot masquerade as an
identical replay. These clarifications were sent while the implementer repaired
the same finding.
Use existing `sf_task_package` acceptance with named negative cases. Lane A retains
work.rs; this does not transfer ownership or expand into SF.3 history recovery.

**SF.5 / ledger_process.rs, write_establishment_marker:** directory open errors
and `sync_all` failures are ignored after marker rename, although successful
admission requires a durable establishment record. Propagate both failures;
do not return a successfully established admission handle without the directory
durability barrier. Propagate the fresh state-directory creation error as well.
Keep subsequent binds denied after a failed publication until durability is
established; a renamed marker alone must not turn the same failed barrier into
Running on retry. Cover the repeated bind/sweep in the same failure case.
Provide a bounded failure injection at the owned durability seam demonstrating
Unavailable and zero launch/queue effects, then retain normal fresh/adopt and
real process cases. Use existing `sf_stop` acceptance. No new storage policy,
migration, or work.rs integration is authorized yet.

The user's continuation authorizes these bounded repairs through the existing
Flash sessions. Preserve shared cache sequencing and existing evidence. Stop on
focused acceptance or one reproducible blocker with at most 30 minutes of
diagnosis. Sol reviews before transfer or closure. Following the user's commit
question, completed tasks receive a bounded commit after review approval;
unreviewed repairs and unrelated edits must not be included in that commit.

### Review and commit pass — 2026-10-13

The user explicitly requested review, approval and commits of the remaining
work. Commits are scoped to reviewed task hunks, with no push or successor
release. Source and historical acceptance receipts are preserved.

The final SF.2 activation repair passes review: historical and current task
leases are checked before either activation branch; replay pins the actual
admission kind and payload, and changed/forged/superseded inputs refuse before
commit. Sol read the negative assertions and raw sf_task_package-activate.log
(28 passed, zero failed/ignored), plus the recorded command exit 0. The full
SF.2 package still needs one bounded correction before approval:
`build_package` in task_package.rs automatically appends the held-out oracle
digest to `scope.readable_artifacts`. Remove that automatic grant and refuse a
caller-supplied grant of the same oracle. Keep its immutable object durable and
parent/verifier readable through its explicit oracle reference. Add independent
assertions for both exclusion and refusal in existing sf_task_package; do not
implement SF.6 scope enforcement. Lane A retains work.rs until commit/review.

SF.5 non-work.rs changes pass review within their delivered scope. Sol inspected
the selected open-existing path, marker durability barrier on initial publication
and retry, metadata failures, shared session control and post-reviewer refusal.
The existing sf_stop-wave3.log has 22 passing cases and recorded exit 0. Held
work.rs/composition.rs integration is still incomplete; the bead remains open.

The remaining FEZ transport hunks pass review against their existing accepted
scope: bounded socket addresses, process defaults with explicit overrides,
nested-runtime-safe Connect clients, parent EOF and startup cleanup, and five
real required smokes. Reuse the frozen acceptance receipt (335 conductor
unit/integration passes and five smokes; two live cases ignored; no resumed
doctest claim). Its identity catalog is historical, not a hash of the combined
SF.2/SF.5 working state. The previously accepted designer_payload regression is
also approved for its own bounded commit under agalma-6w8.

Git index ownership is serialized: reviewed designer payload, FEZ transport,
then SF.2 after its final correction, followed by SF.5 delivery/integration.
No unreviewed shared-file hunk may be swept into another task's commit.

**Held SF.5 integration contract:** after SF.2 is approved/committed and releases
work.rs/phases.rs, transfer both to Lane B for stop wiring only. Also assign
composition.rs and lib.rs for the existing fixture/run caller's explicit
admission opening and stop checks. Inspection found the live phase polls still
construct FileStopControl; inject the same bound control through PhaseDeps and
use it in those three polls. Adapt PhaseDeps construction in the existing
conductor, recovery, fencing, cargo_home, rebind and xcrun tests solely to provide
that control; preserve every behavioral assertion and frozen recovery artifact.
This is a bounded caller/trait-shape amendment, not a new workflow or supervisor.

Work and fixture composition must open through open_admission_ledger and check
Running before proxy/session construction or task admission. Preserve metadata
uncertainty as Unavailable, and use the same Rc bound ledger control for the
sweep, reviewer, work/phase boundary checks and live turn polls. No second stop
database/service may be opened by those paths. Preserve ordinary stop/latch,
explicit resume, crash/recovery and legacy start semantics. SF.4 retains durable
cancellation settlement; make only the existing bounded interrupt/cleanup calls.

The final inspection also found that metadata's NotFound may describe an
unresolved present symlink, not absence of the entry. A dangling establishment
marker plus lost database currently classifies Fresh, while a dangling STOP is
ignored. Distinguish entry absence with native symlink_metadata or equivalent:
a present STOP entry denies, and an unresolved marker/database/temp entry refuses
as Ambiguous/Unavailable instead of bootstrap. Cover both zero-effect cases in
sf_stop. This completes the existing genuine-absence/uncertainty contract and
does not introduce a broader plugin or path policy.

Lane B's required command remains cargo test --offline -p agalma-conductor
--test sf_stop. Extend it with real production work/run refusal for lost
established state, corrupt/incompatible/read/metadata uncertainty, fresh bootstrap
and explicit resume, plus evidence that phase polls reuse the injected control.
Use stub/reference/mock harnesses with no paid provider. Scoped regression
targets are permitted only for the changed constructors/callers. Build missing
required binaries through make build once, with no concurrent shared-cache test.
Do not rerun a full suite, change task acceptance or implement SF.3/SF.4/SF.6.
Stop for Sol review before commit/closure, or at one reproducible external
blocker with at most 30 minutes of diagnosis. No successor release or push.

### Approved commits and integration transfer — 2026-10-13

The subsequent user request to review, approve, and commit authorized bounded
commits after review. Earlier no-commit instructions describe the initial
dispatch; they do not block the explicit commit windows recorded here.

- SF.2 is approved, committed as `ca4d068`, and closed with implementation
  attribution to `opencode-go/deepseek-v4.1-flash`. Final focused evidence is
  `target/test-runs/opencode-batch-1/agalma-5fy.2/sf_task_package-oracle.log`
  and its `.exit` file: 30 passed, zero failed or ignored, exit 0. Supporting
  library and execution checks passed 159 and 11 cases respectively. Source
  review resolved the preparation/admission, frozen-reference, lease/replay,
  durability, and oracle-grant findings. This is not SF.3 history or SF.6
  enforcement acceptance.
- Sol explicitly transferred `work.rs` and `phases.rs` to Lane B session
  `ses_ec9b93b5fffeOnt2kazYcEy11M` after that commit. The amended SF.5 bead
  includes composition/run wiring and necessary PhaseDeps constructor files.
  The held integration above is now dispatched; SF.5 remains incomplete until
  the integrated production paths and required focused cases pass review.
- The requested Luna README update is approved and committed as `e869130`;
  `agalma-4ds` is closed with implementation attribution to
  `opencode-go/gpt-6-luna`. Documentation content, link targets, and whitespace
  were checked; no runtime acceptance is claimed by this documentation change.
- User-authorized build cleanup removed inactive `target/side` and six
  abandoned incremental `*-working` directories after a process check found no
  active Cargo or rustc build. About 2.3 GB was removed. Active debug outputs,
  `target/test-runs`, worker reports, source snapshots, and frozen receipts were
  retained. This operation did not rebuild or rerun acceptance.

No push, successor release, or broader platform acceptance is recorded here.

## Lane A: agalma-5fy.2 — finish immutable task admission/recovery

Preserve wave one at commit `1726298`: canonical digest helpers, complete task
retrieval, TaskPackage/v1 and immutable package store already exist. Finish the
production admission and recovery hooks and crash-boundary acceptance. Recovery
must use the frozen package rather than current bd contents. Dispatch must not
precede durable package admission. Missing legacy or corrupt/incompatible inputs
park explicitly without new effects. Do not implement SF.3 history replay,
SF.4 cancellation, SF.7 budgets or CV.1 kernel migration as part of this task.

Own the SF.2 bead's listed files; Lane A has exclusive ownership of work.rs
until Sol explicitly hands it to Lane B. Preserve existing FEZ changes in
phases.rs and all other concurrent edits. Focused acceptance:
`cargo test --offline -p agalma-conductor --test sf_task_package`.
Required evidence includes nonzero production-path admission/recovery cases,
changed/deleted source issues and crashes on both sides of admission, not merely
package-store tests. Stop after implementation and focused evidence for review,
or after one reproducible blocker that requires changing another task's contract.

## Lane B: agalma-5fy.5 — fail-closed stop admission

Its design gate and single-epic designer prerequisite are closed. Implement the
Running/Stopped/Unavailable contract through the bound LedgerApi. Unavailable
established state must suppress proxy/session creation and new queue effects.
Cover fleet and named-epic designer paths, review, concierge and sessions; keep
fresh absent state distinct from unreadable established state. Preserve explicit
resume, STOP semantics, timeout recovery and cancellation cleanup boundaries.

Own the SF.5 bead's files except work.rs during the first turn. Implement and
check all non-work.rs callers in parallel with Lane A. Do not edit work.rs,
phases.rs, task_package.rs, harness_process.rs or taskqueue contracts. If a
work.rs change is necessary, describe the exact remaining integration in the
handoff and stop for Sol's ownership-transfer message; do not claim SF.5 complete.
Sol will send a follow-up after Lane A hands off work.rs. Preserve the existing
uncommitted session.rs repairs. Focused acceptance:
`cargo test --offline -p agalma-conductor --test sf_stop`.
Record partial coverage explicitly if the held integration prevents full
acceptance. Stop on a bounded reproducible blocker rather than expanding scope.

## Lane C: agalma-6w8 — in-process harness sandbox transport

Own crates/agalma-conductor/src/harness_process.rs and
crates/agalma-conductor/tests/designer_payload.rs; a dedicated behavioral test
target crates/agalma-conductor/tests/harness_sandbox_wire.rs is permitted if
needed for the real sandbox regression. Read sandbox_process.rs and
crates/agalma-ipc/src/sandbox_client.rs but do not change their contract or defaults.
Match the in-process sandbox client wire to the sandbox server's selected wire.
Preserve the default process harness, existing explicit framed selection and
all uncommitted FEZ repairs. Do not change READY deadlines or transport policy.

Use a real local sandbox server and a scripted/mock provider where necessary;
no paid provider call is authorized. Scope is focused. Build only required
offline server prerequisites, then use one cargo test invocation selecting
designer_payload and the real-sandbox regression target if added. Default wire
must be unset for the default-Connect case. Existing assertions and nonzero
required regression execution must pass. If the environment prevents the real
case, report it as incomplete rather than skipped/passed. Stop for review after
the bounded fix and focused evidence or one reproducible external blocker.

## Shared discipline and review handoff

You are not alone in this repository. Preserve others' edits; do not reset,
stash, restore entire files, create a new checkout, or overwrite another lane's
files. Limit formatting to the files you own. No commits, pushes, dependency
closure, new task release or acceptance/oracle rewriting. Task state and
documentation in this batch are maintained by Sol.

Reuse the established target cache. Cargo serializes ordinary shared-cache
builds; Lane C owns sandbox-server prerequisite builds for this batch. If a
concurrent test would overwrite a required executable, stop and coordinate a
stable isolated cache rather than rebuilding repeatedly. Do not create temporary
test wrappers or helper programs. Save exact commands, raw output and exit
status under target/test-runs/opencode-batch-1/<bead-id>/. Focused evidence does
not claim full-workspace, plugin-platform or paid-provider acceptance.

Each session returns one concise handoff: files changed, behavior delivered,
commands and exit results, executed/ignored cases, raw log paths, remaining
limitations, ownership held/released and whether acceptance is complete.
Investigation limit: one reproducible example and at most 30 minutes of blocker
diagnosis; leave a usable handoff if the issue exceeds the task boundary.
