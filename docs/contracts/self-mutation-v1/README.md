# Standup contract examples

Normative fields are in `schema.json`; operational invariants, limits, hashing,
authorization and transaction semantics are in
[self-mutation-contracts.md](../../self-mutation-contracts.md).

Nineteen positive JSON records cover immutable task input, plugin manifest, report,
action, component transaction, request-budget profile, cancellation settlement,
mutation candidate and the initial evaluation policy, plus publication, health,
snapshot, operation status, cancellation request, task release policy/receipt and
structured failure detail, installation configuration and prerequisite-aware
epic review admission. Authorization examples
are fixture identities, not grants.
`snapshot-sections.json` specifies complete-v2 export/import fields and canonical
row types, including the legacy sections omitted by the current v1 snapshot.
`invalid-records.json`
contains seven structural/version failures. `canonical-vectors.json` pins four
canonical encodings and SHA-256 results.

`objects.json` is a virtual fixture artifact store: keys are the suffix of an
`objects/` path and values are exact UTF-8 text. References pin their byte length
and raw SHA-256. The fixture oracle contains identity metadata only. Tests must
still enforce that it is outside builder-readable scope.

These fixtures do not implement a provider, plugin, action or qualification win.
The manifest's package digest and launch grant fingerprint are synthetic identity
examples; a real sealed package needs its complete executable/file inventory.
Fake-provider rates and limits are test constants, not live billing evidence.

The schema is Draft 2020-12 using a small explicit keyword set. The design audit
checks that set and reference/digest semantics offline; implementation acceptance
must deserialize these independent examples and exercise the negative operational
cases in each bead. Schema validity alone cannot establish capability authority,
termination, quiescence, replay safety, request-limit enforcement or measured gain.

Run `python3 docs/contracts/self-mutation-v1/validate.py` from the repository
root to repeat the offline audit. The script validates only the explicitly used
schema keywords; it is not a general JSON Schema implementation.
