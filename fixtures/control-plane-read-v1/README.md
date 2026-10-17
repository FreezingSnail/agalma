# ControlPlaneRead/v1 wire fixtures (WEB.1, `agalma-rwn.1`)

Independent wire fixtures for the typed hub read surface described in
[`docs/web-dashboard-design.md`](../../docs/web-dashboard-design.md)
(implementation contract for decomposition). The Rust contract lives in
`crates/agalma-contracts/src/control_plane.rs`; the acceptance test that consumes
these fixtures is
`crates/agalma-contracts/tests/control_plane_protocol.rs`.

These files are hand-written, independent examples: they are not generated from
the Rust types. Each file is a valid `ControlPlaneRead/v1` response except
`invalid-records.json`, which is a table of malformed cases with the documented
`FailureDetail/v1` code the contract must report.

| File | Shape |
|---|---|
| `overview.json` | `ReadOverview` |
| `components-page.json` | `ListComponents` (a `ReadPage<ComponentSummary>`) |
| `component-page.json` | `ReadComponent` |
| `artifact-chunk.json` | `ReadArtifactChunk` |
| `field-selectors.json` | Documented field-selector grammar and the six view-block value conventions |
| `invalid-records.json` | Malformed cases with their expected failure codes |

## Response envelope and exactness

`ControlPlaneRead/v1` is independent of the ledger schema version. Every
successful response carries collection time, per-source availability, the
registry revision, and the report watermark where applicable.

- Identity-bearing `u64` values (generations, sequences, revisions, watermarks,
  offsets) travel as decimal **strings** so large values survive JSON exactly.
  `overview.json` pins `generation` at `18446744073709551615` (`u64::MAX`) and
  `revision` at `9007199254740993` (beyond `f64` precision).
- Money and usage totals travel as decimal **strings**; an unknown value is
  `null`, never a numeric zero. `overview.json` records a known usage string, a
  `null` cost, and a known allowance.
- A page holds at most 100 entries and at most 256 KiB serialized JSON. The limit
  defaults to 50 and accepts `1..=100`.
- Artifact chunks are at most 64 KiB; one immutable artifact reference is bounded
  by 16 MiB.
- Cursors are opaque `cp1:` tokens that pin the collection, filter, and revision.
  A malformed cursor fails with `InvalidRecord`; a cursor whose revision is no
  longer current fails with `RevisionConflict`.

Source availability states are `available`, `empty`, `missing`, `unreadable`,
and `unknown`. A missing ledger, an unreadable ledger, a successful-but-empty
list, and a stopped factory are distinct; `overview.json` shows a stopped factory
together with an `unknown` budget source.

## Field selectors

Selectors appear in `View.fields` (frozen HP.1 schema; unchanged). A selector
beginning with `/` is an object/array path using JSON-pointer escapes `~0`
(literal `~`) and `~1` (literal `/`). Any other selector is a literal top-level
key. Selectors are never expressions or executable templates. Missing fields,
non-numeric array indices, and out-of-range indices resolve to **missing**, never
to healthy or empty state. `field-selectors.json` records the supported forms.

## The six view-block value conventions

`field-selectors.json` also documents one example per supported `ViewKind`.
These conventions interpret the existing `View.fields` and do not rewrite any
frozen HP schema:

| Block | Resolved field value and presentation |
|---|---|
| Status | Scalar or bounded object, labeled by field name; plugin health stays a separate hub observation |
| Metrics | Scalar or `{value, unit}`; decimal values are text and unknown values stay unknown |
| Table | Array of objects, columns from sorted keys; scalar rows use a Value column; each field selects its own table |
| Timeline | Ordered array of entries; producer order preserved; time/title/detail fields displayed when present |
| Alert | Text or `{severity, message}`; an unsupported severity is visibly unknown |
| ArtifactLink | A canonical artifact reference resolved through the authorized adapter |
