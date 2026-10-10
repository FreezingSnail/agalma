# Local decision models — candidates for the decision contract

Research date: 2026-10-09. **Paper study only.** No weights were downloaded, no
runtime was installed, no benchmark was run, and no build was invoked. All
latency/memory figures below are third-party measurements or estimates and are
labelled as such; §6 defines the measurements required before adoption. Target
host: macOS Apple Silicon, always-on factory.

Scope: small local models and runtimes for Agalma's decision contract
(`docs/architecture.md` §3.7): bounded choice/scoring over ≤~20 options for
`triage.pick-next`, `retry.escalate`, model routing/failure classification,
memory/context ranking, and task ranking. The contract requires: versioned
request/response, static fallback, a local adapter supervised through
`SandboxApi` with read-only weights and **no ledger, Git, or tool authority**,
cold/warm latency and memory measured before adoption, and no additional
always-on service for the MVP.

Related in-repo material: `crates/agalma-contracts/src/decision.rs`
(`DecisionRequest`, `DecisionResponse`, `FallbackReason`),
`crates/agalma-decision/src/baseline.rs` (pinned static baselines),
`docs/m1-workloop.md` (M1 kinds), `docs/spikes/s0b-confinement.md` (Seatbelt
confinement that the adapter can reuse).

---

## 1. What the adapter must do (constraints distilled)

The adapter is a *bounded classifier/ranker*, not an agent:

- Inputs are bounded: a decision kind, opaque bounded context, ≤~20 eligible
  option IDs with bounded attributes, a deadline, and pinned
  policy/genome/model versions. Rust filters options before inference; one
  eligible option needs no call; zero options means wait/park, never a model
  call.
- Outputs are bounded: a chosen ID and/or finite scores over the *supplied*
  IDs, optional confidence with declared meaning. Rust re-validates IDs, score
  ranges, schema, and current eligibility before applying anything.
- The decider cannot bypass mechanical escalations, grant authority, admit an
  unevaluated route, or remove mandatory context. Timeout, unavailability,
  malformed output, or abstention uses the pinned static baseline
  (`FallbackReason::{Timeout,Unavailable,Malformed,Abstention}`); a late
  response cannot supersede an applied fallback.
- There is no ledger/Git/tool authority: the adapter must not read artifact
  paths, the state directory, credentials, or the repo. If context needs
  artifact content, Rust inlines a bounded excerpt; the adapter treats all of
  it as untrusted data.
- Weights are read-only, the process is supervised through `SandboxApi`, and
  weights load on demand with an idle unload policy.

Two adapter styles are available and should both be kept behind `DecisionApi`:

1. **Constrained generative** — a small instruct model emits JSON constrained
   by a grammar/schema (primary candidate below).
2. **Specialized choice/scoring** — score candidate IDs directly (for example
   token log-probabilities over option IDs, or a small classifier), avoiding
   free-form generation. This is attractive for `pick-next` and ranking and is
   partially available in llama.cpp (token probabilities); it is listed as a
   pilot probe, not a commitment.

---

## 2. Candidate small models (2026 landscape)

Selection criteria: ≤~4B parameters, permissive-enough license, structured
output evidence, ≤~20-way choice/classification ability, Apple-Silicon
footprint. Benchmark scores are quoted from the cited source; BFCL v3 and
BFCL-V4 numbers are **not comparable across versions** and are marked.

### 2.1 Comparison table

| Model | Params | Context | License | Q4 disk / RAM class | Structured / decision evidence | Notes |
|---|---|---|---|---|---|---|
| **Qwen3.5-2B** | 2B dense (text+image+video) | 262,144 native | Apache-2.0 | ~1.3 GB GGUF; ~1.5 GB 4-bit weights | BFCL-V4 43.6, TAU2 48.8, IFEval 78.6 (thinking) / 61.2 (non-thinking), MMLU-Pro 55.3/66.5; 3rd-party routing suite: parse 100%, Skill 95.8%, Action 95.8% | Non-thinking by default; tool parser `qwen3_coder`; 201 languages. **Primary candidate.** |
| Qwen3.5-0.8B | 0.8B dense (VL) | 262,144 | Apache-2.0 | ~0.56 GB | BFCL-V4 25.3, TAU2 11.6, IFEval 52.1 (non-thinking) | Too weak for action selection; viable for coarse gating only. |
| Qwen3.5-4B | 4B dense (VL) | 262,144 | Apache-2.0 | ~2.6–3.4 GB | Same routing suite as 2B; family-level strength up | Accuracy step-up if 2B misses the gate; same runtime/schema. |
| Qwen3-1.7B (2025) | 1.7B | 32K | Apache-2.0 | ~1.2 GB | BFCL ~46.3 (v3, per LFM card), IFEval 68.2; BFCL 55.49 overall / 16.88 multi-turn in ACM TinyLLM study | Prior generation; useful compatibility fallback. |
| **Gemma 4 E2B** | 2.3B effective (5.1B total) | 128K | Apache-2.0 | ~3.2 GB | TAU2 24.5, MMLU-Pro 60.0; native function calling + structured JSON | Disk heavier than name suggests (per-layer embeddings). Weak agentic scores. |
| Gemma 4 E4B | 4.5B effective (8B total) | 128K | Apache-2.0 | 5.0–5.4 GB GGUF | TAU2 42.2, MMLU-Pro 69.4, GPQA 58.6; native function calling + structured JSON | Strongest cross-family alternative; heavy disk/RAM; best MLX decode (see §4). |
| **LFM2.5-1.2B** | 1.17B (10 conv + 6 GQA layers) | 32K | LFM Open License 1.0 (review) | ~0.7 GB (Q4_0 719 MB) | BFCLv3 49.1, IFEval 86.2, IFBench 47.3; 3rd-party routing suite: Action 38.5% | Fastest/lightest; Pythonic tool calls with JSON override; not for programming/knowledge tasks. |
| SmolLM3-3B | 3.1B | 64K (128K YaRN) | Apache-2.0 | ~1.8–2.0 GB | 3rd-party routing suite: parse 76%, Action 45.8% | Fully open recipe; weak action-selection evidence. |
| Phi-4-mini | 3.8B | 128K | MIT | ~2.3–2.4 GB | 3rd-party routing suite: parse 67.7%, Action 42.7%; high memory/thermal on phone run | Historically strong reasoning per size, weaker on this routing profile. |
| Granite 4.2 3B | 3.7B | 128K (ext. 512K) | Apache-2.0 | ~1.9–2.8 GB class | IBM documents structured JSON + tool use with `qwen3_coder` parser | Dark horse; no independent routing numbers found yet. |
| Llama 3.2 1B / 3B | 1.2B / 3.2B | 128K | Llama 3.2 Community License | ~0.8 / ~2.0 GB | 3rd-party routing suite: parse 53.1%/94.8%, Action 42.7%/42.7% | License friction; action selection poor; use only as controls. |

Screened down early: sub-1B models (multi-turn/tool accuracy collapse in the
ACM TinyLLM evaluation: TinyLlama-class ~0% multi-turn; Qwen3-1.7B 55.49%
overall but 16.88% multi-turn), and Mixture-of-Experts models (bigger resident
footprint for the same decision throughput; no MVP need).

### 2.2 Notes and sources for the table

- Qwen3.5 sizes, benchmarks, context, license, default non-thinking mode:
  [Qwen3.5-2B model card](https://huggingface.co/Qwen/Qwen3.5-2B), accessed
  2026-10-09; release notice 2026-03-02 in
  [QwenLM/Qwen3.5](https://github.com/QwenLM/Qwen3.5). Qwen3.5-0.8B/2B are
  Apache-2.0; thinking mode is opt-in for the 2B.
- Qwen3.5 tool serving (`qwen3_coder` parser, vLLM/SGLang commands): Qwen3.5-2B
  model card. llama.cpp supports the architecture and chat template (GGUF
  quants exist; reasoning-control flags in §4).
- Independent routing suite (iPhone 15 Pro Max, llama.cpp `b8668`, Q4_K_M, full
  Metal offload; 96 bilingual structured routing cases with a desktop Metal
  evaluator for capability): [MonoWare 10-model comparison,
  2026-08-22](https://monoware.app/blog/phone-local-small-language-models-4k-8k-benchmark).
  Qwen3.5-2B: parse 100%, Skill 95.8%, Action 95.8%, 4K E2E p50 1.098 s.
  LFM2.5-1.2B: parse 56.2%, Action 38.5%. SmolLM3-3B: parse 76%, Action 45.8%.
  Llama 3.2 3B: parse 94.8%, Action 42.7%. Load times 0.13–0.62 s for ≤4B
  models (mmap, phone); 8K fill ≈30.6 s p50 was a phone-storage-bound number,
  not Apple-Silicon.
- Gemma 4 (2026-04-02): Apache-2.0 family, E2B/E4B effective parameters, 128K
  small-model context, native function calling and structured JSON:
  [Google Gemma 4 announcement](https://blog.google/innovation-and-ai/technology/developers-tools/gemma-4/),
  [Gemma 4 model card](https://ai.google.dev/gemma/docs/core/model_card_4),
  [gemma-4-E4B-it model card](https://huggingface.co/google/gemma-4-E4B-it)
  (Tau2 42.2 E4B / 24.5 E2B, MMLU-Pro 69.4 / 60.0), accessed 2026-10-09.
- LFM2.5-1.2B: parameters, context (32,768), 8 languages, tool-use formats,
  BFCLv3/IFEval/IFBench scores, Q4_0 719 MB, day-one llama.cpp/MLX/vLLM:
  [LFM2.5-1.2B-Instruct model card](https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct),
  accessed 2026-10-09 (license `lfm1.0`, LFM Open License 1.0; review required).
- SmolLM3-3B: Apache-2.0, 3.1B, 64K native / 128K YaRN:
  [SmolLM3-3B model card](https://huggingface.co/HuggingFaceTB/SmolLM3-3B).
- Phi-4-mini: MIT, 3.8B, 128K:
  [Phi-4-mini-instruct model card](https://huggingface.co/microsoft/Phi-4-mini-instruct).
- Granite 4.2 3B: Apache-2.0, 128K extendable, structured JSON + tool use:
  [IBM Granite 4.2 docs](https://www.ibm.com/granite/docs/models/granite4-2),
  [granite-4.2-3b model card](https://huggingface.co/ibm-granite/granite-4.2-3b).
- Llama 3.2 sizes/license:
  [Llama 3.2 model card](https://huggingface.co/meta-llama/Llama-3.2-3B-Instruct).
- Small-model agentic limits (BFCL-based study, ≤4B, 4-bit):
  [ACM Computing Frontiers 2026 poster,
  "TinyLLM"](https://dl.acm.org/doi/full/10.1145/3801487.3805608) — 1–3B is the
  sweet spot; sub-1B collapses on multi-turn.
- Disk sizes: Qwen3.5-2B Q4_K_M ≈1.3 GB
  ([lmstudio-community GGUF](https://huggingface.co/lmstudio-community/Qwen3.5-2B-GGUF),
  [zanish-labs Q4_K_M](https://huggingface.co/zanish-labs/qwen3.5-2b-q4_k_m-gguf));
  Gemma 4 E4B Q4_K_M 5.0–5.4 GB
  ([bartowski 5.41 GB](https://huggingface.co/bartowski/google_gemma-4-E4B-it-GGUF),
  [lmstudio-community 5.34 GB](https://huggingface.co/lmstudio-community/gemma-4-E4B-it-GGUF));
  Gemma 4 E2B ≈3.2 GB Q4_K_M and Qwen3.5-2B ≈1.3 GB Q4_K_M cross-checked in
  [Pocket AI's on-device catalogue, 2026-09-05](https://mypocketai.app/blog/which-ai-models-run-on-your-iphone).

---

## 3. Structured output: what the evidence says

### 3.1 Constrained decoding works and is fast

- JSONSchemaBench (9,558 real-world schemas, six engines) finds constrained
  decoding can speed up generation up to **50%** versus unconstrained, and
  improves downstream task accuracy by up to ~3–4 points. Coverage depends on
  schema complexity: on function-call-style schemas, llama.cpp/GBNF scored
  ~95% empirical coverage and ~97% compliance; on hard nested schemas it drops
  to ~39%/~63%. Grammar compile time for llama.cpp was ~0.05–0.06 s.
  [Paper (arXiv:2501.10868)](https://arxiv.org/html/2501.10868v3),
  [repo](https://github.com/guidance-ai/jsonschemabench), accessed 2026-10-09.
- Practical reading for Agalma: keep decision schemas **flat and enum-like**
  (chosen ID from a fixed enum; optional scores over the same enum; optional
  abstain). Flat schemas are exactly where constrained decoding is strongest
  and cheapest. Avoid `$ref`/`anyOf` nesting in the decision schema.
- Ollama exposes the same idea as `format: <json schema>` on `/api/chat`
  ([Ollama structured outputs](https://docs.ollama.com/capabilities/structured-outputs),
  [announcement 2024-12-06](https://ollama.com/blog/structured-outputs)).
  llama.cpp exposes server-level `--json-schema`/`--json-schema-file` and
  per-request schema-constrained JSON, plus raw GBNF
  ([llama.cpp server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)).

### 3.2 Native (unconstrained) small models are not reliable enough

- On a 96-case routing suite, most ≤3B models failed to produce a complete,
  parseable, semantically-correct action: Qwen3.5-2B was the only sub-2.5B
  model with 100% parse and ≥95% action accuracy; LFM2.5-1.2B (38.5% action),
  Gemma 3 1B (52.1%), Llama 3.2 1B (42.7%) and SmolLM3-3B (45.8%) were below a
  safe execution threshold. [MonoWare](#21-comparison-table).
- Consequence: **constrain the surface form structurally** (grammar/schema)
  and validate semantics in Rust; do not rely on prompt instructions alone for
  JSON.

### 3.3 Format restriction has a cost

- "Let Me Speak Freely?" (EMNLP 2024 industry track) finds that format
  restrictions can measurably degrade reasoning, and stricter constraints
  degrade more. [Paper](https://aclanthology.org/2024.emnlp-industry.91.pdf),
  [arXiv:2408.02442](https://arxiv.org/abs/2408.02442). Accessed 2026-10-09.
- Mitigations for the decision contract:
  - Keep schemas minimal; ask only for the choice/scores.
  - Do not force step-by-step reasoning *inside* the JSON. If a rationale is
    useful for audit, request it as a short trailing plain-text field **after**
    the JSON, or not at all; never feed decider text to tools or agents.
  - For pure ranking, prefer log-probability scoring of option-ID tokens over
    generated explanations (probe in pilot; llama.cpp exposes token
    probabilities in completion responses).
  - Keep thinking mode off for latency and predictability (`--reasoning off`
    / `--reasoning-budget 0` in llama.cpp; non-thinking is Qwen3.5-2B's
    default).

### 3.4 Per-model structured-output evidence (summary)

| Model | Native structured evidence | Under constrained decoding |
|---|---|---|
| Qwen3.5-2B | tool parser `qwen3_coder`; BFCL-V4 43.6; Tau2 48.8 | expected 100% schema-valid on flat enums (pilot) |
| Gemma 4 E4B | native function calling + structured JSON; Tau2 42.2 | expected 100% schema-valid on flat enums (pilot) |
| LFM2.5-1.2B | BFCLv3 49.1; JSON tool calls on request | expected 100% schema-valid (pilot) |
| Qwen3-1.7B | BFCL ~46.3 (v3); ACM 55.49 overall / 16.88 multi-turn | fine for single-shot enums; weak multi-turn |

Tau2/BFCL scores are *end-task* quality, not JSON validity; they justify
candidate ranking, while §6 sets the validity bar that must actually be
measured.

---

## 4. Runtimes for macOS Apple Silicon

### 4.1 Comparison

| Runtime | License | Constrained/JSON output | Process supervision / lifecycle | Cold & warm behavior | Memory / footprint |
|---|---|---|---|---|---|
| **llama.cpp `llama-server`** | MIT | `--json-schema(-file)`, GBNF `--grammar(-file)`; schema-constrained JSON response format; ~0.05 s grammar compile | Single native process; child of supervisor; `--sleep-idle-seconds`; router mode `--models-dir/--models-preset/--models-max`; `--api-key(-file)`; can bind a **Unix socket** (`--host /path.sock`); Prometheus `--metrics`; `/health` | mmap default (`--load-mode mmap`, `mlock`, `mmap+mlock`, `dio`); `--warmup/--no-warmup`; `--offline` blocks network fetches; `/health` 503 while loading | Metal/CPU offload `-ngl`; KV cache type/quant `-ctk/-ctv`; no daemon; RSS ≈ weights + KV + runtime; unload = exit or idle-sleep |
| MLX / `mlx-lm` | MIT (Apple) | **Not first-class**: no `response_format` JSON-schema path in `mlx_lm.server` as of Sep 2026 (open issue); Outlines integration exists for MLX-LM, but a reported batching path bypassed the schema constraint — keep concurrency 1 if used | Python process; server + library; no daemon; supervisor manages child | Fast load on unified memory; Python import overhead; no built-in idle unload | Decode notably faster than llama.cpp on M-series (e.g. Gemma 4 E4B M4 Max: 113.5 tok/s MLX-swift vs 80.5 llama.cpp); lower peak (4,376 MB vs 5,150 MB) |
| Ollama | MIT | `format` JSON schema on chat/generate | App/daemon (`ollama serve`) with model runner children; `keep_alive` (`0` unload immediately, `-1` pin, default 5 min); `OLLAMA_KEEP_ALIVE`, `OLLAMA_MAX_LOADED_MODELS` (default 3), `OLLAMA_NUM_PARALLEL` (default 1, memory scales with parallel × context) | Preload via empty generate/chat request; `ollama ps` shows CPU/GPU split; default context **4096** (`OLLAMA_CONTEXT_LENGTH` to raise) | Managed store `~/.ollama/models`; KV cache quant `OLLAMA_KV_CACHE_TYPE`; macOS app auto-updates (pin digest / disable app update before adoption) |
| LM Studio (llmster headless) | Proprietary daemon; free for work; `lms` CLI MIT | OpenAI-compatible JSON-schema structured output; MLX engine uses Outlines for schema-constrained generation | Headless daemon + `lms server start/stop`, model load/unload CLI | On-demand model load; GUI-free since 0.3.5; server state persisted | Closed-source daemon, update/licensing behavior not auditable; treat as evaluation tool, not factory component |
| vLLM-Metal (community plugin) | Community vLLM plugin (Apache-2.0 ecosystem) | vLLM structured outputs supported (release notes include "structured outputs" in v0.28.0) | `vllm serve` HTTP server; Python; memory guard `--gpu-memory-utilization`; warmup at startup | Startup warmup accounts for weights/buffers before KV allocation; serving-oriented | For concurrent serving; heavier than needed for serial decisions |
| mistral.rs | MIT (Rust) | `generate_structured` with JSON schema; Rust-native engine; day-0 Gemma 4 support | Rust server/binary; embeddable or standalone | Rust process, no Python; Apple Silicon via Metal | Promising wildcard; small-model maturity unverified here |
| Apple Foundation Models (`fm`) | Proprietary, OS-provided | `@Generable`/guided generation; `fm schema`; `fm serve` reported to expose a local Chat Completions API | System-managed model; `fm` CLI pre-installed with macOS 27; Python SDK (Python 3.10+, Xcode, Apple Silicon) | No weights on disk; always-available on-device model; no usage limit for the on-device model (Private Cloud Compute has limits) | Context window documented as **4096 tokens** per session; 8K reported on macOS 27 in a secondary source — verify in pilot |

### 4.2 Runtime notes and caveats

- **llama.cpp** is the best fit for an Agalma `DecisionApi` adapter pilot:
  single native process (supervised like any other SandboxApi child), MIT
  license, real grammar/schema enforcement, Unix-socket binding for a
  no-TCP-loopback setup, API keys, Prometheus metrics, offline mode, explicit
  load modes, and an idle-sleep option that implements "load on demand, idle
  unload" without an extra daemon. Binary and weights can be pre-staged
  (no runtime downloads; `--offline` guarantees this). Version pinning is a git
  tag/commit plus the GGUF hash.
- **MLX/mlx-lm** is the performance alternative: Apple's own stack, faster
  decode and lower peak memory on M4 Max in a reproducible benchmark, and the
  same runtime LM Studio uses. The gaps are first-class structured output
  (Outlines wrapper; a reported schema-bypass under batching) and Python
  process supervision. Use it as the second pilot lane for a
  choices-only schema at concurrency 1, or after a structured-output fix lands.
- **Ollama** is the fastest way to run a *development/evaluation* loop:
  schema-constrained output, preload, `keep_alive`, one-line model pulls.
  For the factory, the daemon plus default 4K context, app auto-updates, and
  extra process layer make it a weaker permanent component. If used, set
  `OLLAMA_NO_CLOUD=1`, bind loopback, pin digests, and disable auto-update.
- **LM Studio/llmster** is free for work and has a genuine headless daemon and
  structured output, but it is closed source with its own update path; keep it
  for offline comparisons, not as always-on infrastructure.
- **vLLM-Metal** is a real 2026 option for concurrent serving on Apple Silicon
  (packed varlen Metal kernels, paged KV, MTP for Gemma 4). Agalma's decision
  load is serial and low-QPS; a serving engine is unnecessary complexity for
  the pay pilot, but it is the escape hatch if many concurrent decider sessions
  appear later.
- **mistral.rs** deserves a probe because it is Rust-native with JSON-schema
  structured generation and Gemma 4 support, which could remove an entire
  language/runtime dependency from the adapter. Its small-model and
  grammar-reliability evidence is thinner; treat as second-wave evaluation.
- **Apple Foundation Models** is the zero-disk, zero-marginal-cost option:
  OS-managed on-device model, guided generation, `fm` CLI/Python SDK on
  macOS 27, and (reported) a local Chat Completions endpoint via `fm serve`.
  Risks: 4K documented context (smaller than it sounds for long task
  descriptions, but fine for bounded decisions if kept small), no weight
  pinning (OS updates can change model behavior), availability gating (Apple
  Intelligence enabled, eligible hardware, asset download), and no audit of
  the model. Best used as a shadow lane on low-stakes kinds, with re-evaluation
  after every OS update.

### 4.3 Recommended runtime for the pilot

Primary: **llama.cpp `llama-server`, pinned release, one model, one slot,
Unix socket + API key, `--offline`, `--reasoning off`, `--ctx-size 8192`
(request context is far smaller), `--parallel 1`, `--no-mmproj`,
`--sleep-idle-seconds <T>`**, supervised as a `SandboxApi` child with read-only
weights and no other filesystem access. MLX/mlx-lm is the second lane; Ollama
is dev-only; LM Studio and vLLM-Metal are evaluation/escape-hatch options.

---

## 5. Decision schema pattern (proposed)

Keep the model's job to a flat, finite surface:

```json
{
  "chosen": "opt-3",
  "scores": [{"id": "opt-1", "score": 0.12}, {"id": "opt-3", "score": 0.71}],
  "abstain": false
}
```

- `chosen` and every `scores[].id` are enums of the exact offered IDs; the
  grammar makes out-of-set IDs impossible, and Rust re-validates anyway.
- Scores are finite floats; their meaning ("relative preference" /
  "probability-like") is declared and versioned. Ties and missing scores fall
  to the baseline.
- `abstain: true` is allowed and maps to `FallbackReason::Abstention`.
- No free-form fields required. If a rationale is wanted for audit, keep it out
  of the decision payload and discard/limit it; never route decider text to
  tools or agent prompts.
- Prompt assembly: system instructions fixed and versioned; task data
  (titles, failure text, attributes) passed as clearly delimited JSON; never
  concatenated as instructions. Option IDs are system-generated; only bounded
  attributes are untrusted text. Cap total input bytes and truncate with a
  recorded rule.

---

## 6. Measurement plan (required before adoption)

Definitions:

- **Cold spawn**: process start → `/health` 200 (model loaded).
- **Cold request**: process up with weights evicted → first schema-valid,
  semantically-valid decision.
- **Warm request**: weights resident → decision. Report p50/p95/p99 over ≥200
  samples per decision kind (M1: `triage.pick-next`, `retry.escalate`; then
  routing and ranking as they land).

### 6.1 Metrics and how to capture them

| Metric | Method |
|---|---|
| Latency (end-to-end, prefill/decode split) | `curl -w '%{time_total}'` plus server `timings` (llama.cpp returns prompt/predicted timings); hyperfine-style repetition in the eval harness |
| Peak RSS | `/usr/bin/time -l` for max RSS at spawn; periodic `ps -o rss=` during runs; `vm_stat` for pageouts/compression |
| Metal/GPU memory | llama.cpp load logs (buffer sizes) and `/props`; MLX/others via their own stats; `sudo powermetrics --samplers gpu_power` |
| Idle footprint | process alive, model unloaded (idle-sleep / `keep_alive: 0`): RSS after 10 min idle in a 24 h soak |
| CPU/GPU/energy | `sudo powermetrics -i 1000 --samplers cpu_power,gpu_power,thermal`; per-decision energy from package power × wall time |
| JSON validity | harness counts (a) HTTP success, (b) `serde_json` parse, (c) flat-schema validation, (d) ID membership, (e) score finiteness — per sample and per kind |
| Semantic accuracy | labeled examples per kind, compared to the pinned static baseline (`baseline.rs`) |
| Injection robustness | red-team suite of poisoned task titles/failure texts (see §7.1) |
| Stability | 24 h soak: RSS slope, swap growth, thermal throttling, latency drift |

### 6.2 Accuracy protocol

- Build a protected labeled set per decision kind (target ≥300 examples for
  ranking, ≥500 for triage before promotion), drawn from Agalma history plus
  hand-written adversarial cases.
- Shadow mode first: the decider's would-be choice is recorded but the
  baseline applies. Paired comparison (same request) and K≥3 repetitions for
  stability; report variance, never hide it.
- Metrics: top-1 agreement with the preferred choice, rank correlation /
  regret for rankings, per-class precision/recall for classification, and
  fallback rate. Calibration only after a labeled reliability curve exists;
  raw scores and self-confidence are not assumed calibrated.
- Promotion: point estimate ≥ baseline on the held-out set **and**
  non-inferiority lower 95% confidence bound ≥ baseline − 1 percentage point;
  any safety-invariant violation is an automatic fail.

### 6.3 Acceptance thresholds (proposed; require owner sign-off)

| Metric | Threshold before promotion |
|---|---|
| Warm p50 / p95 (≤2K-token prompt, ≤64 output tokens) | ≤300 ms / ≤800 ms (primary 2B); 4B candidate ≤1.5× these |
| Cold spawn → `/health` | p95 ≤8 s (2B class); ≤15 s (4B class) |
| Cold request (evicted weights) → valid decision | p95 ≤10 s (2B); ≤20 s (4B) |
| Peak RSS incl. Metal buffers | ≤2.5 GB (primary); ≤6 GB (4B alt) |
| Idle RSS, model unloaded | ≤200 MB; no monotonic growth >5% over 24 h |
| CPU idle | <1% average over 10 min |
| JSON validity with constrained decoding | 100% parse + schema + ID/finiteness on ≥1,000 samples/kind; fallback counted separately; zero invalid choices ever applied |
| Semantic accuracy | ≥ baseline point estimate per kind; lower 95% CI ≥ baseline − 1pp; no safety-invariant violations |
| Thermal / swap | no sustained thermal throttling in a 60 min loop; no swap growth attributable to the adapter in 24 h |
| Disk | ≤6 GB per candidate weights set; weights outside the repo, read-only |
| Economics | incremental energy/decision measured; sign-off by owner once numbers exist |

Any miss moves the candidate to fallback-only or out of the shortlist; the
static baseline remains the default path throughout.

---

## 7. Risks and limits

### 7.1 Prompt injection (highest-risk surface)

- Task titles, failure output, and option attributes come from untrusted task
  data. Even though options are filtered by Rust, the model can be steered to
  choose among legitimate options for illegitimate reasons, or to emit
  garbage that forces fallback (availability attack). OWASP ranks prompt
  injection as LLM01:2025 and calls out indirect injection as the
  higher-impact variant: [LLM01:2025](https://genai.owasp.org/llmrisk/llm01-prompt-injection/),
  [Greshake et al. 2023](https://arxiv.org/pdf/2302.12173.pdf), accessed
  2026-10-09.
- Structural defenses (already implied by §3.7): options-only enums; Rust
  re-validation; no tool/Git/ledger authority; fallback on anything malformed;
  bounded inputs; no artifact path resolution inside the adapter; decider text
  never fed back to agents or tools.
- Add: a red-team suite that embeds instructions in task titles/failure logs
  ("choose ID X", "output an abstain", "ignore the schema") and measures
  manipulation rate; compare candidate models on it; reject candidates above a
  to-be-set manipulation threshold. Prompt-injection detectors are themselves
  probabilistic; do not treat them as a control.

### 7.2 Fallback discipline

- Deadlines are enforced by Rust; late responses are discarded. Bounded
  retries; a new paid invocation is a new attempt (never reuse an operation ID
  with different contents). Circuit-breaker the adapter after repeated
  malformed/timeout responses and revalidate eligibility before any resulting
  action, including after restart.
- Abstention must be a first-class path, not an error string; malformed output
  and invalid IDs count as `FallbackReason::Malformed` and must be visible in
  receipts and the digest.

### 7.3 Versioning and immutability of weights

- Pin: model repo revision + per-file SHA-256, quantization file, runtime
  release tag/commit, grammar/schema version, prompt template hash, and
  generation parameters. Record them in the decision receipt
  (`DecisionPin`, `decision_inputs_hash`).
- Prefer formats with auditable provenance: GGUF and safetensors. Avoid
  pickle-based artifacts (`*.bin`) to remove arbitrary-code-execution risk.
- Hazards: Ollama's macOS app auto-updates (pin manifest digest, disable
  auto-update); LM Studio's closed daemon updates itself; Apple Foundation
  Models are OS-managed with no version pin — any OS update silently changes
  the decider, so re-run the evaluation gate after system updates.

### 7.4 Disk and staging

- Q4 footprints: Qwen3.5-2B ≈1.3 GB; LFM2.5-1.2B ≈0.7 GB; Qwen3.5-4B
  ≈2.6–3.4 GB; Gemma 4 E4B ≈5.0–5.4 GB; Gemma 4 E2B ≈3.2 GB. Budget for at
  most two candidates staged at once; weights live outside the repo, read-only,
  never committed.
- The factory must work with no network: pre-stage weights and runtime
  binaries; run llama.cpp with `--offline`; set `HF_HUB_OFFLINE=1` /
  `TRANSFORMERS_OFFLINE=1` for Python lanes; `OLLAMA_NO_CLOUD=1` for Ollama;
  verify that no runtime performs update checks at startup.

### 7.5 Memory, CPU/GPU, and thermal contention

- Unified memory is shared with builds and tests. Clamp context (e.g. 4–8K)
  even though models advertise 128K–262K; quantize KV cache if needed; prefer
  mmap over `mlock` while builds run (mlock can force memory pressure); unload
  on idle. Measure spike overlap with build/verify phases, not just idle.
- Small models can be confidently wrong. Confidence is not calibrated; use
  scores as ordinal and apply confidence thresholds only after an Agalma-
  specific reliability curve exists (§3.7).

### 7.6 License notes

- Apache-2.0: Qwen3.5 family, Qwen3, Gemma 4 family, Granite 4.2, SmolLM3.
- MIT: Phi-4-mini, llama.cpp, MLX/mlx-lm, Ollama, mistral.rs.
- Custom: LFM2.5 (LFM Open License 1.0 — review before adoption), Llama 3.2
  (Meta Community License), Apple Foundation Models (OS terms), LM Studio
  (proprietary daemon; free-for-work terms).
- Benchmarks from third parties may be produced under their own harnesses and
  settings; treat all numbers as inputs to §6, not as adoption evidence.

---

## 8. Recommendation shortlist

Pilot ordering (not adoption; adoption requires §6 gates):

1. **Qwen3.5-2B, Q4_K_M (~1.3 GB), llama.cpp `llama-server` pinned release.**
   Rationale: best independent structured-decision evidence below 3B (100%
   parse, 95.8% action selection on a bilingual routing suite; BFCL-V4 43.6;
   Tau2 48.8), Apache-2.0, non-thinking by default, native tool parser, tiny
   disk/RAM, and llama.cpp gives grammar/schema enforcement, single-process
   supervision, Unix socket, offline mode, and idle unload without a daemon.
   Confidence: **medium-high** for pilot; medium for adoption until §6 numbers
   exist on Agalma's own labeled decisions.
2. **Qwen3.5-4B, Q4_K_M (~2.6–3.4 GB), same runtime/schema.** Accuracy
   step-up in the same family/tooling if the 2B misses the accuracy gate.
   Confidence: **medium** (family evidence strong; heavier footprint).
3. **LFM2.5-1.2B, Q4 (~0.7 GB), llama.cpp.** Fastest/lightest, strongest
   instruction-following among the smallest models (IFEval 86.2, BFCLv3 49.1),
   but weak action-selection evidence (38.5%). Restrict to coarse
   classification/gating with forced enums; license review required. Confidence:
   **medium-low** for decision duty, high for gating duty.
4. **Cross-family shadow alternative: Gemma 4 E2B/E4B (Apache-2.0).**
   Native function calling/structured JSON; E4B Tau2 42.2. Choose only if
   cross-family diversity is wanted; E4B costs 5+ GB disk and more RAM, and the
   MLX path is the fastest consumer of it. Confidence: **medium-low**
   (footprint and weaker agentic scores).
5. **Runtime wildcards (shadow instrumentation only): MLX/mlx-lm** for the
   decode-speed lane (structured-output gap at concurrency >1), **Apple
   Foundation Models `fm`** for a zero-disk low-stakes lane (4K context
   documented, unversioned model, macOS 27+, availability gating), and
   **mistral.rs** as a Rust-native substitute to evaluate next wave.

Proposed next steps (to be filed as bd tasks by the parent session; none were
created or executed here): pin and stage weights offline; stand up the
llama.cpp adapter behind `DecisionApi`; build the decision eval harness
(schema validity, latency, RSS, energy, injection suite); run shadow
evaluations for `triage.pick-next` and `retry.escalate`; 24 h soak with idle
unload; re-evaluate after macOS or runtime updates.

---

## 9. Sources (all accessed 2026-10-09 unless noted)

Models

- Qwen3.5-2B model card — https://huggingface.co/Qwen/Qwen3.5-2B (released 2026-03-02)
- Qwen3.5 repository/release notes — https://github.com/QwenLM/Qwen3.5
- Gemma 4 announcement (2026-04-02) — https://blog.google/innovation-and-ai/technology/developers-tools/gemma-4/
- Gemma 4 model card (updated 2026-07-30) — https://ai.google.dev/gemma/docs/core/model_card_4
- gemma-4-E4B-it model card — https://huggingface.co/google/gemma-4-E4B-it
- Gemma 4 technical report — https://arxiv.org/abs/2607.02770
- LFM2.5-1.2B-Instruct model card — https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct
- LFM2 technical report — https://arxiv.org/abs/2511.23404
- SmolLM3-3B model card — https://huggingface.co/HuggingFaceTB/SmolLM3-3B
- Phi-4-mini-instruct model card — https://huggingface.co/microsoft/Phi-4-mini-instruct
- Granite 4.2 docs — https://www.ibm.com/granite/docs/models/granite4-2
- granite-4.2-3b model card — https://huggingface.co/ibm-granite/granite-4.2-3b
- Llama 3.2 model card — https://huggingface.co/meta-llama/Llama-3.2-3B-Instruct

Benchmarks and studies

- JSONSchemaBench (arXiv:2501.10868, v3 2025-02-27) — https://arxiv.org/html/2501.10868v3
- JSONSchemaBench repo — https://github.com/guidance-ai/jsonschemabench
- "Let Me Speak Freely?" (EMNLP 2024 industry) — https://aclanthology.org/2024.emnlp-industry.91.pdf
- ACM TinyLLM SLM agentic evaluation (2026-06-27) — https://dl.acm.org/doi/full/10.1145/3801487.3805608
- MonoWare 10-model on-device routing comparison (2026-08-22) — https://monoware.app/blog/phone-local-small-language-models-4k-8k-benchmark
- AICraftGuide on-device SLM comparison (2026-06-07) — https://aicraftguide.com/article/on-device-slm-phi4-gemma3-qwen3-smollm3-2026
- vdf.ai best small models (2026-10-06) — https://vdf.ai/blog/best-small-language-models/
- Apple Silicon LLM benchmark (reproducible, M4 Max; Gemma 4 E4B MLX vs llama.cpp) — https://github.com/john-rocky/apple-silicon-llm-bench
- Apple Silicon optimization guide (MLX vs llama.cpp) — https://blog.starmorph.com/blog/apple-silicon-llm-inference-optimization-guide
- llmcheck Apple Silicon estimates — https://llmcheck.net/benchmarks

Runtimes

- llama.cpp server README (flags, schema/grammar, metrics, router, idle sleep, offline) — https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md
- llama.cpp repository (MIT) — https://github.com/ggml-org/llama.cpp
- MLX-LM structured-output issue (open 2026-09-21) — https://github.com/ml-explore/mlx-lm/issues/1007
- Outlines MLX-LM integration — https://dottxt-ai.github.io/outlines/latest/features/models/mlxlm/
- Ollama structured outputs — https://docs.ollama.com/capabilities/structured-outputs
- Ollama FAQ (context 4096 default, keep_alive, preload, memory/concurrency, cloud toggle) — https://docs.ollama.com/faq
- LM Studio developer docs / headless — https://lmstudio.ai/docs/developer
- LM Studio structured output — https://lmstudio.ai/docs/developer/openai-compat/structured-output
- LM Studio free for work (2025-07) — https://lmstudio.ai/blog/free-for-work
- LM Studio 0.4.0 (llmster headless daemon) — https://lmstudio.ai/blog/0.4.0
- vLLM-Metal announcement (2026-09-22) — https://vllm.ai/blog/2026-09-22-vllm-metal-v0-28-0
- vllm-metal repo — https://github.com/vllm-project/vllm-metal
- mistral.rs (Rust, structured output, Gemma 4) — https://github.com/EricLBuehler/mistral.rs
- Apple Foundation Models framework — https://developer.apple.com/documentation/foundationmodels
- Apple Foundation Models context window (4096) — https://developer.apple.com/documentation/foundationmodels/managing-the-context-window
- WWDC26: `fm` CLI and Python SDK (macOS 27) — https://developer.apple.com/videos/play/wwdc2026/334/
- `fm serve` local Chat Completions (Apple Developer Forums, secondary) — https://developer.apple.com/forums/forums/topics/machine-learning-and-ai/machine-learning-and-ai-foundation-models

Security

- OWASP LLM01:2025 Prompt Injection — https://genai.owasp.org/llmrisk/llm01-prompt-injection/
- OWASP prompt-injection prevention cheat sheet — https://cheatsheetseries.owasp.org/cheatsheets/LLM_Prompt_Injection_Prevention_Cheat_Sheet.html
- Greshake et al., indirect prompt injection (2023) — https://arxiv.org/pdf/2302.12173.pdf

In-repo references

- `docs/architecture.md` §3.7 (decision contract, fallback, adapter footprint, evaluation)
- `crates/agalma-contracts/src/decision.rs` (`DecisionRequest`, `DecisionResponse`, `FallbackReason`)
- `crates/agalma-decision/src/baseline.rs` (pinned static baselines)
- `docs/m1-workloop.md` (M1 decision kinds; durability)
- `docs/spikes/s0b-confinement.md` (Seatbelt confinement for supervised children)
