# SF.6 protected input admission amendment

Human decision, 2026-10-15: use an operator-supplied manifest, frozen into each
task package; missing required inputs stop repository work. This supplies the
admission source left unspecified by the revision-1 protected-baseline design.
It does not reseal historical design receipts or release successors.

## Source and authority

Amendment 2026-10-20 (superseding the state-directory location only): use the
operator XDG configuration file
`$XDG_CONFIG_HOME/agalma/protected-inputs.json`, or
`$HOME/.config/agalma/protected-inputs.json` when `XDG_CONFIG_HOME` is unset,
empty, or relative. A valid absolute `XDG_CONFIG_HOME` needs no `HOME`; a
non-Unicode or relative `HOME`, an empty `HOME`, or no usable config base fails
closed. The production resolver and loader are the distinctly named
`scope_guard::resolve_config_path` and `scope_guard::load_config`; the existing
explicit-directory `ProtectedInputs::load(dir)` remains only as a low-level
fixture/file loader.

There is no fallback to `<state_dir>/protected-inputs.json` and no policy
merging: a missing or malformed selected config refuses even when a legacy
state-directory manifest is present. This file stays outside candidate writable
roots. A repository file, task-authored text, absent manifest or empty scope
never authorizes an unprotected repository execution. M0 fixture mode remains a
separate path.

Strict UTF-8 JSON has `format: "ProtectedInputs/v1"` and an `entries` list.
Each entry contains `task_id`, `repository_root` (canonical origin path),
`base_sha`, `allowed_paths`, `protected_paths`, and `baseline`. Reject unknown
fields, duplicate task entries, mismatched task/repository/base identity,
invalid paths and missing/unreadable/malformed inputs. Resolve the task base
once and compare that identity. Empty allowlists permit no changes. Preserve
meaningful filename bytes; invalid protected entries refuse the complete
policy. Deny wins. Task release, capabilities, stop and budget authority remain
independent requirements; this manifest cannot release work or authorize spend.

## Frozen baseline and execution

`baseline` is `ProtectedBaseline/v1`, with nonempty `files` and `commands` lists.
Files record repository-relative path, regular file mode, SHA-256 digest, byte
length and exact UTF-8 `text`. Refuse unsupported modes, invalid/duplicate or
conflicting paths, integrity mismatches and unknown/empty baseline formats.
This bootstrap representation supports text test inputs/runners; unsupported
input refuses instead of being guessed. Commands are immutable strings run
through the existing scoped command runner and SandboxApi. Do not introduce a
content-assertion DSL or new test framework.

Freeze the complete selected entry as the package's existing `scope.oracle`
artifact, including policy identity and baseline commands/files. Populate its
existing allowed/protected fields from the same entry; validate agreement when
loading. This pins provenance without changing canonical TaskPackage/v1 fields.
Builders cannot read the artifact. Recovery uses recorded package bytes, never
current operator input or current bd text. Operator edits affect future admissions.

Materialize the protected files into a separate read-only root outside every
child writable root. Never put the sole judge into a candidate-writable tree.
The sandbox's ATTEMPT and EXTRA_RW grants allow writes; PROTECTED must not lie
inside either. Parent pins, receipts and package objects also stay outside those
grants. Use narrower runtime/candidate roots through existing LaunchSpec fields;
do not weaken the sandbox policy or introduce a sandbox framework.

Provide parent-controlled `AGALMA_PROTECTED_DIR` and `AGALMA_CANDIDATE_DIR` to
the protected runner. Execute frozen baseline commands against the pinned
candidate, plus frozen task acceptance; both must pass. Candidate-authored tests
cannot supply the sole judge. A protected command failure is red; missing or
unsupported protected inputs refuse. Prove the runner independently exercises
candidate behavior and cannot write protected files or parent pin state. Keep
held-out inputs inaccessible to the builder.

The verification pin binds execution/generation, package digest, successful
attempt, candidate SHA and oracle digest. Missing/corrupt/mismatched/stale pins
refuse integration. Enforce the expected candidate at Workspace's actual merge
boundary; it must not commit later dirty/untracked changes and ship a different
SHA after a conductor comparison. Duplicate landed operations reconcile the
recorded identity/receipt without a second effect.

## Ownership and acceptance

Keep the existing SF.6 worktree/session, ownership and investigation limits.
Minimal package validation and supporting admission/recovery fixture changes in
conductor task_package.rs, tests/sf_task_package.rs and tests/sf_recovery_gate.rs
are included. Preserve those invariants, frozen historical fixtures and receipts.
Canonical package fields and current process bindings remain compatible.

Retain the required sf_scope_guard command and the dispatch handoff's focused
Workspace/IPC/process and package/recovery selections. Add production admission,
protected runner, changed-candidate integration, restart/duplicate, invalid input
and independent generated policy cases. A permitted real source fix must pass;
an always-pass replacement test must lose to protected behavioral verification.
Save every test source and preserve run commands, source identity, logs, exits,
generation budgets/seeds and fixture data outside the task worktree.

Socket fixture support is a separate bounded infrastructure issue if existing
supported paths cannot run required IPC/process checks. This does not authorize
implementation in the primary checkout, skipped required cases, unbounded
diagnosis or acceptance based only on helper tests.

## Amendment (2026-10-16): sandbox removal

The sandbox layer was removed; see
[sandbox-removal-design.md](sandbox-removal-design.md). Consequences for this
amendment, from the accepted guarantee reduction (human decision 2026-10-15):

- The protected root is still materialized outside candidate-writable roots and
  still supplied via `AGALMA_PROTECTED_DIR`, but it is **no longer OS-enforced
  read-only**. "Cannot write protected files" is a convention property now, not
  a sandbox-proved one.
- Frozen commands run through `ProcessApi`/`LocalLauncher` instead of
  `SandboxApi`; the scoped command runner and frozen-command judging are
  unchanged.
- `tests/sf_scope_guard.rs` no longer claims OS-enforced isolation.

All other requirements of this document stand.
