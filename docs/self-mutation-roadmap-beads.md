# Remaining self-mutation roadmap bead index

Updated 2026-10-12. [Roadmap and contracts](self-mutation-roadmap.md) describe
the 33 staged implementation children below. Runtime design gate `agalma-16k`
blocks on `agalma-jmr.8` and `agalma-cg9.8`: inspect actual platform and mutation
receipts, freeze the later interfaces, then revalidate before release.
These children all remain decomposition-approved and deferred to 2099.

MX and CV follow HP. M3 blocks on original M2, MX and CV. P1 follows M3; it does
not replace original autonomy requirements. Actual task edges below provide
fine-grained implementation order. Parent-child/tracking edges are not authority.

## MX: MX remaining M2 nerve, onboarding, memory and evaluated deciders (agalma-xlw)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-xlw.1` | MX.1 canonical nerve events and pinned adapter profiles | `agalma-16k`, `agalma-cg9.8`, `agalma-myx.11` |
| `agalma-xlw.2` | MX.2 immutable context injection and compaction continuity | `agalma-16k`, `agalma-xlw.1` |
| `agalma-xlw.3` | MX.3 bounded repair and permission bridge without privilege escalation | `agalma-16k`, `agalma-xlw.1` |
| `agalma-xlw.4` | MX.4 scoped postmortem memory and evaluated skill curation | `agalma-16k`, `agalma-cg9.9`, `agalma-xlw.2`, `agalma-xlw.3` |
| `agalma-xlw.5` | MX.5 model tuple and harness-profile onboarding registry | `agalma-16k`, `agalma-cg9.8`, `agalma-myx.1`, `agalma-myx.11`, `agalma-myx.4` |
| `agalma-xlw.6` | MX.6 screen full evaluation and gated routing mutation | `agalma-16k`, `agalma-cg9.7`, `agalma-xlw.5` |
| `agalma-xlw.7` | MX.7 decision-kind shadow evaluation and eligible fallback | `agalma-16k`, `agalma-xlw.6` |
| `agalma-xlw.8` | MX.8 integrated remaining M2 evidence and milestone audit | `agalma-16k`, `agalma-xlw.4`, `agalma-xlw.7` |

## CV: CV production service convergence on common hub plugin platform (agalma-pob)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-pob.1` | CV.1 selected production ExecutionApi and caller-side handler dispatch | `agalma-16k`, `agalma-5fy.4`, `agalma-5fy.9`, `agalma-jmr.8` |
| `agalma-pob.2` | CV.2 auxiliary ledger and standalone CLI API convergence | `agalma-16k`, `agalma-jmr.8` |
| `agalma-pob.3` | CV.3 ledger execution packages and bounded bootstrap | `agalma-16k`, `agalma-pob.1`, `agalma-pob.2` |
| `agalma-pob.4` | CV.4 workspace taskqueue packaging and hub domain routing | `agalma-16k`, `agalma-pob.3` |
| `agalma-pob.5` | CV.5 sandbox harness packaging with credential and termination continuity | `agalma-16k`, `agalma-fez`, `agalma-pob.3` |
| `agalma-pob.6` | CV.6 Router integration using shared plugin services | `agalma-16k`, `agalma-myx.11`, `agalma-myx.12`, `agalma-pob.3` |
| `agalma-pob.7` | CV.7 cross-service drain rebind and compatible rollback scenarios | `agalma-16k`, `agalma-pob.4`, `agalma-pob.5`, `agalma-pob.6` |
| `agalma-pob.8` | CV.8 real production convergence evidence and architecture audit | `agalma-16k`, `agalma-pob.7` |

## M3: M3 — always-on baseline (epic, MVP exit) (agalma-3q5)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-3q5.1` | M3.1 versioned operator cadence and directive-authorized release policy | `agalma-16k`, `agalma-cg9.8`, `agalma-cg9.9` |
| `agalma-3q5.2` | M3.2 durable serial scheduler owner fencing idle and backoff | `agalma-16k`, `agalma-3q5.1` |
| `agalma-3q5.3` | M3.3 hierarchical aggregate cash quota and local resource governor | `agalma-16k`, `agalma-3q5.1`, `agalma-5fy.7` |
| `agalma-3q5.4` | M3.4 receipted reconciliation mining digest and alert jobs | `agalma-16k`, `agalma-3q5.2`, `agalma-3q5.3` |
| `agalma-3q5.5` | M3.5 external supervision and clean single-owner lifecycle | `agalma-16k`, `agalma-3q5.2` |
| `agalma-3q5.6` | M3.6 complete backups pinned retention and disk-pressure admission | `agalma-16k`, `agalma-3q5.3`, `agalma-pob.8` |
| `agalma-3q5.7` | M3.7 isolated restore and quiescent schema upgrade reconciliation | `agalma-16k`, `agalma-3q5.6` |
| `agalma-3q5.8` | M3.8 crash kill resume and degradation drill matrix | `agalma-16k`, `agalma-3q5.4`, `agalma-3q5.5`, `agalma-3q5.7` |
| `agalma-3q5.9` | M3.9 genuine 72-hour unattended baseline and MVP evidence | `agalma-16k`, `agalma-3q5.8`, `agalma-pob.8`, `agalma-xlw.8` |

## P1: P1 — autonomy gate (post-MVP epic) (agalma-52v)

| Bead | Implementation task | Blocking prerequisites |
|---|---|---|
| `agalma-52v.1` | P1.1 immutable core update candidate and compatibility manifest | `agalma-16k`, `agalma-3q5.9` |
| `agalma-52v.2` | P1.2 independent reproducible build and protected S0 M3 evaluation | `agalma-16k`, `agalma-52v.1` |
| `agalma-52v.3` | P1.3 external bootstrap supervisor handover journal and fencing | `agalma-16k`, `agalma-52v.1` |
| `agalma-52v.4` | P1.4 drain checkpoint stage and receipted core activation | `agalma-16k`, `agalma-52v.2`, `agalma-52v.3` |
| `agalma-52v.5` | P1.5 failed-start and health-regression compatible binary rollback | `agalma-16k`, `agalma-52v.4` |
| `agalma-52v.6` | P1.6 isolated shadow core evaluation and bounded canary | `agalma-16k`, `agalma-52v.5` |
| `agalma-52v.7` | P1.7 versioned directive-bounded code autonomy enablement | `agalma-16k`, `agalma-52v.6` |
| `agalma-52v.8` | P1.8 independent complete autonomy and code self-correction proof | `agalma-16k`, `agalma-52v.7` |

All four later decompositions passed plan-check alongside SF/HP/SM. No cycles
were found. Current runtime evidence is deliberately required before closing the
later design gate: the roadmap does not pretend its planned APIs already exist.
