# OpenCode provider model catalog & cheapest-adequate routing (research)

Snapshot date: **2026-10-09**. Author: research subagent (docs only; no code changes).

Scope: models offered by the two OpenCode-run providers — **OpenCode Zen** (`opencode`,
pay-as-you-go) and **OpenCode Go** (`opencode-go`, $10/$40-month subscription with per-model
usage allowances). Catalog metadata is from the live OpenCode model catalog
(`tools.opencode.models`) plus the local models.dev cache
(`~/.cache/opencode/models.json`, read 2026-10-09) and the Zen/Go docs. Benchmark evidence is
web-sourced and cited with dates. Nothing here was benchmarked locally.

Method / honesty rules:

- Catalog tool and docs are authoritative for availability and price as of 2026-10-09.
- The local models.dev cache lags the catalog slightly (missing `claude-haiku-5-5`,
  `exo-free`, `ling-3.1-flash-free`, `longcat-2.5-preview-free`, `step-5-preview-free`); those
  rows use catalog/docs data and mark metadata that is not verifiable.
- Benchmark figures are as reported by the cited party. Scaffolds differ (Vals harness,
  mini-SWE-agent, vendor harnesses). Where no number was found we write **unknown** — no number
  has been invented.

---

## 0. TL;DR

1. **Free routing floor is strong and unstable.** `opencode/big-pickle` (free) is
   Sonnet-4.5/4.6-class by community measurement (SWE Atlas 50.8%, single trial) and is the
   M1 default; `space-bunny-free`, `step-5-preview-free`, `longcat-2.5-preview-free`, and
   `muse-spark-1.3-contributor-free` are secondary free routes. Two of them are unlimited inside
   the Go plan for a limited time. All can change or end without notice, and most train on your
   data during the free period.
2. **Cheapest paid workhorses:** `muse-spark-1.3-contributor` ($0.10/$0.20),
   `gpt-6-luna` ($0.10/$0.50), `claude-haiku-5-5` ($0.10/$0.50),
   `mimo-v2.6-flash` ($0.14/$0.28), `deepseek-v4.1-flash` ($0.15/$0.60 off-peak),
   `qwen3.8-flash` ($0.15/$0.47), `glm-5.3-flash` ($0.15/$0.50).
3. **Strongest independent cheap signals:** Haiku 5.5 — Vibe Code Bench v1.1 90.44% (#3/110,
   Vals, 2026-10-08); GLM-5.3-Flash — SWE-bench 92.0% on the Vals Index subset (but ~60 min
   latency/test, max effort); DeepSeek V4.1 Flash — Vals Index 57.86% at the cheapest paid tier.
4. **Do not treat Go "prices" as pay-per-token.** Go is a subscription with per-model monthly
   allowances; the listed token prices only define how usage drains the allowance. `$/merge`
   must track *cash* and *notional quota* cost separately (see §5.3).
5. **Jev 1.13** (Zen, `$0.042/M` input, output free) is a purpose-built structured-decision
   model (yes/no, choice, score) and is a natural shadow candidate for agalma's `DecisionApi` —
   but it needs a separate adapter and has no published benchmarks.

---

## 1. Model catalog

### 1.1 Free models (provider `opencode` = OpenCode Zen)

All free in/out/cache-read; all `status: active` in the catalog tool on 2026-10-09. Context is
the catalog's input limit; output is max output tokens. "Struct" = structured output advertised
in models.dev; "—" = not advertised.

| Model id | Vendor / family | Ctx / out | Tool | Struct | Released | Notes |
|---|---|---|---|---|---|---|
| `opencode/big-pickle` | stealth (community guess: GLM-4.6) | 200,000 / 32,000 | yes | yes | 2025-10-17 | Oldest surviving stealth model; data may improve the model; 32K out is tightest of free set |
| `opencode/space-bunny-free` | stealth (community guess: MiniMax M3, unconfirmed) | 1,048,576 / 524,288 | yes | — | 2026-09-23 | Zero-retention per Zen docs; also paid as `opencode-go/space-bunny` ($0.15/$0.60) |
| `opencode/longcat-2.5-preview-free` | Meituan LongCat | unknown (1M family) | unknown | unknown | 2026-09-25 | Free also in Go, **unlimited** (limited time); metadata not in cache |
| `opencode/step-5-preview-free` | StepFun | 1,000,000 / 1,000,000 (sibling metadata) | yes | yes | 2026-09-16 | Free also in Go, **unlimited** (limited time); mostly vendor benchmarks |
| `opencode/muse-spark-1.3-contributor-free` | Meta | 1,048,576 / 131,072 | yes | yes | 2026-09-02 | Data used to train Meta models; limited regions; paid contributor tier is $0.10/$0.20 |
| `opencode/mimo-v2.6-flash-free` | Xiaomi MiMo | 200,000 / 32,000 | yes | — | 2026-09-22 | Data used to improve model; limited time |
| `opencode/nemotron-3.5-lightning-free` | NVIDIA | 262,144 / 262,144 | yes | yes | 2026-08-11 | NVIDIA trial terms: trial only, no confidential data, usage logged |
| `opencode/nemotron-3-ultra-free` | NVIDIA | 1,000,000 / 128,000 | yes | — | 2026-06-04 | Same NVIDIA trial terms |
| `opencode/ling-3.0-flash-fin-free` | Ant Group (InclusionAI) | 262,144 / 32,768 | yes | no | 2026-08-27 | Data may improve model; sibling Ling 3.0 Flash has benchmarks |
| `opencode/ling-3.1-flash-free` | Ant Group (InclusionAI) | 262K (family) | unknown | unknown | 2026-09-29 | Newest free model; metadata not in cache; unranked externally |
| `opencode/exo-free` | stealth / unknown | 1,048,576 / 131,072 | yes | yes | 2026-10-06 | Text+image; only `high` effort variant; no benchmark found |

Also free per Zen docs but not returned by the current catalog tool: `opencode/mimo-v2.5-free`
and the structured-decision model `opencode/jev-1.13-free` / `jev-1.13` ($0.042/M input, output
free; System One endpoint, `noul`/`choice`/`score` questions; not a text coder).

### 1.2 Paid models (provider `opencode-go`)

Prices are the Go usage-accounting prices in USD per 1M tokens (catalog tool + Go docs,
2026-10-09). "Zen list" is the Zen pay-as-you-go price; where vendor list differs it is noted.
Cache = cached read / cached write. All active in the catalog tool. Context/output from the
models.dev cache; `claude-haiku-5-5` from Vals.

| Model id | Vendor / family | Ctx / out | Tool | Struct | Price in/out (cache r/w) | vs Zen/vendor list | Released |
|---|---|---|---|---|---|---|---|
| `claude-haiku-5-5` | Anthropic | 1,000,000 / 128,000 | yes | no | 0.10/0.50 (0.01/0.125); >100K: 0.50/2.50 | = vendor list; >100K tier matches Zen | 2026-10-07 |
| `gpt-6-luna` | OpenAI | 1,050,000 / 128,000 | yes | yes | 0.10/0.50 (0.01/0.125); >272K: 0.20/0.75 | = vendor list | 2026-09-22 |
| `muse-spark-1.3-contributor` | Meta | 1,048,576 / 131,072 | yes | yes | 0.10/0.20 (0.002/—) | Contributor tier ($0.10/$0.20) vs standard $1.25/$4.25; trains on your data | 2026-09-02 |
| `muse-spark-1.2-contributor` | Meta | 1,048,576 / 131,072 | yes | yes | 0.10/0.20 (0.002/—) | Same contributor terms | 2026-08-05 |
| `mimo-v2.6-flash` | Xiaomi MiMo | 1,048,576 / 131,072 | yes | — | 0.14/0.28 (0.0028/—) | = vendor list | 2026-09-22 |
| `mimo-v2.6-pro` | Xiaomi MiMo | 1,048,576 / 131,072 | yes | — | 0.435/0.87 (0.003625/—) | = vendor list | 2026-09-22 |
| `mimo-v2.5` | Xiaomi MiMo | 1,000,000 / 128,000 | yes | yes | 0.14/0.28 (0.0028/—) | = vendor list | 2026-04-22 |
| `mimo-v2.5-pro` | Xiaomi MiMo | 1,048,576 / 128,000 | yes | yes | 0.435/0.87 (0.003625/—) | = vendor list | 2026-04-22 |
| `gpt-5.6-luna` | OpenAI | 1,050,000 / 128,000 | yes | yes | 0.20/1.20 (0.02/0.25); >272K: 0.40/1.80 | = vendor list | 2026-07-09 |
| `qwen3.8-flash` | Alibaba Qwen | 1,000,000 / 131,072 | yes | yes | 0.15/0.47 (0.016/0.2) | = vendor list | 2026-08-26 |
| `glm-5.3-flash` | Z.ai GLM | 1,000,000 / 131,072 | yes | yes | 0.15/0.50 (0.03/—) | = vendor list | 2026-08-26 |
| `hy3` | Tencent Hy | 256,000 / 128,000 | yes | — | 0.14/0.58 (0.035/—) | = vendor list | 2026-07-06 |
| `space-bunny` | stealth | 1,048,576 / 524,288 | yes | — | 0.15/0.60 (0.03/0) | Free while preview; same model as free route | 2026-09-23 |
| `deepseek-v4.1-flash` | DeepSeek | 1,000,000 / 384,000 | yes | yes | off-peak 0.15/0.60 (0.003/—); peak 0.30/1.20 | Off-peak = vendor off-peak; peak = Zen list | 2026-09-10 |
| `deepseek-v4-flash` | DeepSeek | 1,000,000 / 384,000 | yes | yes | off-peak 0.15/0.60 (0.003/—); peak 0.30/1.20 | Same peak/off-peak scheme | 2026-07-31 |
| `deepseek-v4-flash-vision-exp` | DeepSeek | 1,000,000 / 384,000 | yes | yes | off-peak 0.15/0.60 (0.003/—); peak 0.30/1.20 | Image input billed as input tokens | 2026-08-21 |
| `longcat-2.0` | Meituan LongCat | 1,000,000 / 131,072 | yes | — | 0.30/1.20 (0.006/—) | = vendor list | 2026-06-30 |
| `minimax-m3` | MiniMax | 1,000,000 / 131,072 | yes | — | 0.30/1.20 (0.06/—) | = vendor list base; >512K doubles | 2026-06-01 |
| `minimax-m2.7` | MiniMax | 204,800 / 131,072 | yes | — | 0.30/1.20 (0.06/0.375) | Legacy generation, still listed | 2026-03-18 |
| `qwen3.7-plus` | Alibaba Qwen | 1,000,000 / 65,536 | yes | yes | 0.40/1.60 (0.04/0.5); >256K: 1.20/4.80 | = Zen list | 2026-06-02 |
| `deepseek-v4-pro` | DeepSeek | 1,000,000 / 384,000 | yes | yes | off-peak 0.66/1.98 (0.022/—); peak 1.32/3.96 | Go is below vendor peak; Zen list 1.74/3.48 | 2026-08-12 (0813) |
| `hy4-preview` | Tencent Hy | 1,024,000 / 64,000 | yes | — | 0.834/2.501 (0.042/—) | = vendor list | 2026-08-28 |
| `kimi-k2.7-code` | Moonshot Kimi | 262,144 / 262,144 | yes | yes | 0.95/4.00 (0.19/—) | = vendor list | 2026-06-12 |
| `glm-5.3` | Z.ai GLM | 1,000,000 / 131,072 | yes | yes | 1.40/4.40 (0.26/—) | = vendor list | 2026-08-14 |
| `glm-5.2` | Z.ai GLM | 1,000,000 / 131,072 | yes | yes | 1.40/4.40 (0.26/—) | Prior generation | 2026-06-13 |
| `grok-4.7` | xAI | 500,000 / 500,000 | yes | yes | 2.00/6.00 (0.50/—); >200K: 4/12 | = vendor list | 2026-09-21 |
| `grok-4.6` | xAI | 500,000 / 500,000 | yes | yes | 2.00/6.00 (0.50/—); >200K: 4/12 | = vendor list | 2026-08-12 |
| `kimi-k3` | Moonshot Kimi | 1,048,576 / 131,072 | yes | yes | 3.00/15.00 (0.30/—) | = vendor list | 2026-07-16 |
| `qwen3.8-max` | Alibaba Qwen | 1,000,000 / 131,072 | yes | yes | 2.00/6.00 (0.25/2.5) | = vendor list | 2026-08-03 |

Go docs also still list `kimi-k2.6` ($0.95/$4.00) and legacy/deprecated entries in the cache
(`kimi-k2.5`, `glm-5`, `glm-5.1`, `minimax-m2.5`, `qwen3.5/3.6-plus`, `mimo-v2-omni/pro`,
`grok-4.5`, `ox-alpha-free`, `omen-alpha`) — not returned by the current catalog tool, treat as
legacy.

**Effort/reasoning variants** (from catalog tool; relevant for routing-data pins):
`claude-haiku-5-5` none→max; `gpt-6-luna` none→max; `muse-spark` minimal→xhigh;
`deepseek-v4*` low/high/max; `qwen3.8-*` none/low/medium/xhigh; `glm-5.3*` low/high/max;
`grok-4.*` low→xhigh; `hy3` none/low/high; `hy4-preview` none/high; `kimi-k3` max only;
`minimax-m3` none/thinking; `big-pickle` none.

---

## 2. Benchmark evidence

Interpretation rules: **Vals** = independent third-party harness (see each URL; Vals Index
composite differs from single-benchmark runs). **benchlm** = aggregator over self- and
third-party reports (marked by them as excluded/estimated where applicable). Vendor numbers are
self-reported launch tables. All URLs read 2026-10-09.

### 2.1 SWE-bench Verified / Pro

| Model | Score | Source | Note |
|---|---|---|---|
| `muse-spark` (family, likely 1.2) | SWE-bench Verified 77.4% | benchlm.ai/benchmarks/swe-bench-verified (2026-10-09) | Family-level row |
| `muse-spark-1.1` | SWE-bench Pro 61.5% | benchlm.ai/benchmarks/swe-bench-pro (2026-09-30) | Prior gen; 1.3 Pro number not published |
| `minimax-m3` | SWE-bench Verified 80.5% | benchlm (above) + llm-stats.com/benchmarks/swe-bench-verified | Two trackers agree |
| `qwen3.7-plus` | SWE-bench Verified 77.7% | benchlm (above) | |
| `qwen3.8-max` | SWE-bench 85.6% (Vals harness); SWE-bench Pro 67.7% | vals.ai/models/alibaba_qwen3.8-max; benchlm SWE-bench Pro | Vals Index composite 48.27% |
| `qwen3.8-flash` | SWE-bench Pro 62.5%; SWE Multilingual 81.0% (Flash-Next base) | docs.b.ai/llmservice/models/qwen3-8-flash (QwenCloud summary); HF Qwen3.8-Flash-Next card | Production endpoint; benchmarks from the Flash-Next foundation |
| `glm-5.3-flash` | **SWE-bench 92.0%** (Vals cover); 90.44%? see note | vals.ai/models/zai_glm-5.3-flash (2026-09-08 data) | Vals benchmark page; ~60 min average latency at max effort. Verify subset vs full before treating as full SWE-bench |
| `glm-5.3` | 50% better than GLM-5.2 on Z.ai Code Bench; open-source SOTA claims on Terminal-Bench 3.0 / Agents' Last Exam | z.ai/blog/glm-5.3; gigazine.net/gsc_news/en/20260817-z-ai-glm-5-3 | No clean numeric table extracted; mark medium confidence |
| `kimi-k2.7-code` | SWE-bench 78.2%; Terminal-Bench 2.1 67.04% | vals.ai/models/kimi_kimi-k2.7-code | "new #1 open-weight on SWE-bench Verified" at launch |
| `kimi-k3` | SWE-bench 93.4% (Vals page); 95.1% (Vals Index subset); TB 2.1 80.90% | vals.ai/models/kimi_kimi-k3 | Highest measured open-weight SWE signal in this set |
| `deepseek-v4-pro` | SWE-bench 80.6% (benchlm) / 77.4% (Vals); SWE Pro 55.4%; TB 2.0 67.9% | benchlm; vals.ai/models/deepseek_deepseek-v4-pro | Scaffold differences |
| `deepseek-v4-flash` (0731) | SWE-bench 79.0% (benchlm); SWE Pro 52.6% | benchlm/models/deepseek-v4-flash; deepinfra.com vendor table | |
| `deepseek-v4.1-flash` | Vals Index 57.86% | vals.ai/models/xiaomi_mimo-v2.6-pro update (2026-09-22) | No full SWE-bench number found |
| `mimo-v2.5-pro` | SWE-bench 78.9% | llm-stats (2026-10-10) | llm-stats is self-reported aggregation |
| `mimo-v2.5` | SWE-bench 78.0% (llm-stats); TB 2.0 65.8% (benchlm) | llm-stats; benchlm TB2 | |
| `mimo-v2.6-flash` | Vals Index 59.58% (#16/65; cheapest in top 20). Vendor: DeepSWE 67.9, Toolathlon 73.6, TB2.1 87.6 (unverified) | vals.ai/models/xiaomi_mimo-v2.6-flash; opentools.ai/llms/mimo-v26-flash | Vendor TB2.1 conflicts with vendor TB4 28.8 — use with caution |
| `mimo-v2.6-pro` | Vals Index 55.20%; Vibe Code Bench v1.1 85.22%; TB 4.0 31.31% | vals.ai/models/xiaomi_mimo-v2.6-pro | |
| `claude-haiku-5-5` | SWE-bench unknown; Vals Index 54.31% (#16/45); Vibe Code Bench v1.1 90.44% (#3/110); CyberBench 75.48%; TB 4.0 35.35% | vals.ai/models/anthropic_claude-haiku-5-5 (2026-10-08) | No SWE-bench in Vals suite |
| `gpt-6-luna` | Vals Index 58.45% (#20/65); TB 2.1 73.03%; Vibe Code Bench 81.65%; IOI 55.56% | vals.ai/models/openai_gpt-6-luna (2026-09-22) | "Lowest cost of any model in the top 20" |
| `muse-spark-1.3` | Vals Index 53.20% (#19/45); Vibe Code Bench 82.86%; Harvey Legal 22.92% (#3/76) | vals.ai/models/meta_muse_spark_1_3 | Contributor-tier price is the point |
| `hy3` (preview) | SWE-bench 78.0% (llm-stats); TB 2.0 54.4% (benchlm, preview) | llm-stats; benchlm TB2 | Final Hy3 numbers sparse |
| `hy4-preview` | SWE-bench Pro 65.7% | benchlm SWE-bench Pro | |
| `nemotron-3-ultra` | SWE-bench 71.9% | benchlm | Free tier is the same class |
| `nemotron-3.5-lightning` | SWE-bench 52.8% | benchlm | 30B A3B; free tier |
| `longcat-2.0` | **unknown** (publisher records exist but no extractable rows in trackers) | long-cat.org/benchmarks; benchlm compare shows "Coming soon" | |
| `step-5-preview` | Vendor: DeepSWE 67.7%, TB 2.1 85.0%, TB4 33.3%, MCP Atlas 85.6%, GPQA-D 93.5%, HLE 46.5% (AA independent), ProgramBench 80.5% | aiwiki.ai/wiki/step_5_preview; themodelgap.com/models/step-5-preview | 5 of 6 tracked scores are vendor self-reports; tbench board does not list it |
| `grok-4.7` | Vals Index 54.95% / 60.22% (update); TB 2.1 73.41%; Vibe Code Bench 86.17% | vals.ai/models/grok_grok-4.7 (2026-09-21) | Cost per test high ($11.92) |
| `big-pickle` | SWE Atlas Codebase QnA 50.8% (63/124), mini-swe-agent scaffold, **single trial** | github.com/PhillipChaffee/big-pickle-swe-atlas; alextech.ai (2026-08-16) | Community run; beats GPT-5.6-Sol (46%) and GLM-5.2 (48.1%) same scaffold |
| `space-bunny(-free)` | No official benchmarks. Third-party anecdote: 3/3 hidden-code retrieval @200K context; code review found 6.5/27 seeded issues | huggingface.co/blog/karmen-beatapi/space-bunny-tested-real-cases-evaluation | Identity unknown; treat numbers as directional only |
| `exo-free`, `ling-3.1-flash-free`, `longcat-2.5-preview-free` | **unknown** | — | New/stealth; no reliable benchmarks found |
| `ling-3.0-flash(-fin)` | SWE-bench Pro 56.6%; SWE Multilingual 72.4%; TB 2.1 57.0%; BFCL v4 73.0%; LiveCodeBench 82.8% | benchlm.ai/models/ling-3-0-flash; HF inclusionAI model card (OpenHands harness) | Fin is the same family |
| `jev-1.13` | **unknown** (no public benchmarks found) | opencode.ai/docs/zen/ | Specialized decision API |

### 2.2 Lower-confidence / conflicting

- `deepseek-v4.1-flash` vendor card reports GPQA-D 90.9 and **Terminal-Bench 2.1 90.6 Pass@1**
  (build.nvidia.com/deepseek-ai/deepseek-v4.1-flash, 2026-09-18) — far above what independent
  trackers show for the family; harness not stated. Use only as an upper bound.
- `step-5-preview` TB 2.1 85.0% (vendor) vs Vals-verified class numbers in the 60s for peers;
  benchlm compare has it "Coming soon". Treat as unverified.
- MiMo V2.6 Flash TB 2.1 87.6% (opentools.ai) vs TB 4.0 28.8% (same page): inconsistent;
  prefer Vals Index/ranks.

---

## 3. Free-tier caveats (from OpenCode docs, 2026-10-09)

Source unless noted: <https://opencode.ai/docs/zen/> and <https://opencode.ai/docs/go/>.

**Availability**

- All free models are **limited-time offers**, not permanent free inference: Big Pickle, Space
  Bunny, LongCat 2.5 Preview, Step 5 Preview, Exo, MiMo-V2.6-Flash Free, Ling 3.1/3.0 Fin,
  Nemotron 3 Ultra, Nemotron 3.5 Lightning, Muse Spark 1.3 Contributor Free, Jev 1.13 Free.
- Stealth models (`big-pickle`, `space-bunny`, `exo`) can be **swapped or withdrawn without
  notice**; identity is undisclosed (community guesses: GLM-4.6, MiniMax M3, unknown).
- `longcat-2.5-preview-free` and `step-5-preview-free` are also available inside Go and are
  **unlimited** there while the promotion lasts.

**Data policy (Zen)**

- Default: US/EU hosting, zero retention, not used for training.
- Exceptions that **do** use data to improve the model: Big Pickle, Exo Free, MiMo-V2.6-Flash
  Free, MiMo-V2.5 Free, Ling 3.1 Flash Free, Ling 3.0 Flash Fin Free, plus NVIDIA free
  endpoints.
- NVIDIA free endpoints: trial use only, **do not submit personal or confidential data**;
  usage is logged for security and product improvement.
- Muse Spark 1.3 Contributor (free and paid): prompts/completions are used to **train future
  Meta models** in exchange for heavily discounted pricing; availability limited to regions
  permitted by Meta's Geographic Use Policy.
- Space Bunny Free, LongCat 2.5 Preview Free, Step 5 Preview Free: provider zero-retention,
  no training.
- OpenAI/Anthropic models via Zen: requests retained 30 days per their data policies.
- DeepSeek ZDR is renewed monthly (valid through 2026-10-31 per docs).

**Limits / billing**

- Zen is pay-as-you-go; auto-reload of $20 when balance drops below $5; workspace/monthly
  limits available. Card fees passed at cost (4.4% + $0.30).
- Go: $10/mo (Go) or $40/mo (Go Plus). Each model has a monthly dollar allowance (e.g.,
  GLM-5.3-Flash $60, MiMo flash $60, GPT-6 Luna $15, Haiku 5.5 $15, GLM-5.3 $15). Within a
  model's allowance: per-5-hour limit is 20%, weekly 50%, monthly 100%. Free Go models are
  unlimited (limited time). Overflow can fall back to Zen balance if enabled.
- **Request-rate limits (req/min, req/day) for free Zen models are not documented** in the
  official Zen/Go pages. An unofficial mirror claims "100 requests/day", but that page's
  model tiers do not match current docs — treat rate limits as **unknown** and measure.
- Go is for coding-agent traffic; clients should send a stable session id
  (`x-opencode-session`) for routing/caching.

---

## 4. Draft cheapest-adequate routing table (for agalma)

Constraints from the repo: M1 routes through the free model via the egress proxy and expects
$0.00 merges; paid routes are gated on the provider-auth proxy (docs/m1-workloop.md "Open cost
items"). Routing/triage uses the versioned decision contract with static baselines first
(docs/architecture.md §3.7); these are candidate routes for shadow evaluation, not active
policy yet. "Escalate" is the mechanical ladder step, not a decision-model choice.

| Task class | Cheapest adequate (primary) | Budget alternative | Escalate on failure/timeout | Rationale | Confidence |
|---|---|---|---|---|---|
| Doc / prompt edit (prose, tiny edits) | `opencode/big-pickle` (free) | `opencode-go/muse-spark-1.3-contributor` $0.10/$0.20 | `opencode-go/deepseek-v4.1-flash` | Zero-cost, 200K ctx, tool use; M1 default class; free data policy forbids sensitive content | High on cost, medium on quality (stealth drift) |
| Small bug fix (single file, tests exist) | `opencode-go/deepseek-v4.1-flash` off-peak $0.15/$0.60 | `opencode-go/mimo-v2.6-flash` $0.14/$0.28; free: `space-bunny-free` | `opencode-go/deepseek-v4-pro` $0.66/$1.98 | DeepSeek Flash line = strong agentic coding per dollar; MiMo Flash is the cheapest Vals top-20 model | Medium |
| Test fix (make failing test green) | `opencode-go/glm-5.3-flash` $0.15/$0.50 | `opencode-go/claude-haiku-5-5` $0.10/$0.50 | `opencode-go/kimi-k2.7-code` $0.95/$4 | GLM-5.3-Flash has the highest independent SWE signal in the cheap tier (92% Vals cover) but is slow at max effort; Haiku is fast with top-tier Vibe Code | Medium (scaffold/subset risk) |
| Refactor (multi-file, behavior-preserving) | `opencode-go/qwen3.8-flash` $0.15/$0.47 | `opencode-go/mimo-v2.6-pro` $0.435/$0.87 | `opencode-go/deepseek-v4-pro` | Qwen3.8-Flash-Next: SWE Pro 62.5 / DeepSWE 58.7, 1M ctx, cheap cached input | Medium |
| Code review / diagnosis (read, explain, propose) | `opencode-go/claude-haiku-5-5` $0.10/$0.50 | `opencode-go/gpt-6-luna` $0.10/$0.50; free: `big-pickle` | `opencode-go/glm-5.3` $1.40/$4.40 | Haiku 5.5 is #3 on Vibe Code Bench and strong on CyberBench; review is read-heavy and cache-friendly | Medium-high (independent Vals) |
| Planning / decomposition | `opencode-go/mimo-v2.6-pro` $0.435/$0.87 | `opencode-go/gpt-6-luna` $0.10/$0.50 | `opencode-go/deepseek-v4-pro` | No dedicated planning benchmark; agentic plus reasoning scores used as proxy; keep plans short to control output cost | Low-medium |
| Triage / decision JSON (`DecisionApi`) | `opencode/jev-1.13` $0.042/M in, output free (`noul`/`choice`/`score`) | `opencode-go/gpt-6-luna` or `muse-spark-1.3-contributor` (both structured-output) | static fallback (mandated) | Jev returns typed values/probabilities for bounded decisions; generative models are a fallback-free alternative with untested JSON reliability at tiny budgets | Medium for Jev fit, low for calibration without agalma-labeled evals |

Free-tier default note: for M1 the effective table is just the free column. Because free models
are unstable and (mostly) training-enabled, treat them as bench/shadow routes only for
non-sensitive content; anything touching protected fixtures or private code should wait for a
paid route.

---

## 5. Routing-data recommendations (model metadata → ledger)

Agalma already records a `models` ledger entity and pins `DecisionPin.model`
(crates/agalma-contracts/src/decision.rs), computes `$/merge` and leaderboard cells
(docs/architecture.md §9, §12). To route by cost and later evaluate routes, add the following
metadata.

### 5.1 Model registry row (one per provider/model/revision)

- **Identity:** `provider_id`, `model_id`, `catalog_revision` (hash of the catalog/docs snapshot
  + `fetched_at`), vendor/family, base model revision (`0813`, `1.3-contributor`), status
  (`active`/`preview`/`free-limited`/`deprecated`), `release_date`.
- **Capability:** input/output context limits, tool-call, structured-output, reasoning +
  effort variants, modalities, open-weights flag. These are eligibility filters that Rust must
  check before offering an option (architecture §3.7: "capability, context, cost, and
  availability checks").
- **Economics:** per-tier prices (input, output, cache read, cache write, context-size tiers,
  peak/off-peak windows for DeepSeek), currency, free flag, and **plan bucket** (Go per-model
  monthly allowance, Zen pay-as-you-go, free). Record *effective pricing mode*: pay-per-token vs
  subscription quota.
- **Policy:** data training/retention class, confidential-data allowed (no for
  Big Pickle/Exo/MiMo Free/Ling Free/Nemotron/Muse Spark contributor), region restrictions.
- **Operational:** endpoint/auth package, session-header expectation, observed p50/p95 latency
  and throughput (append-only observations, not hardcoded), timeout, availability caveats
  ("limited time", stealth may change).
- **Evidence:** benchmark snapshot (name, score, harness, source URL, access date, confidence
  class: independent/aggregator/vendor/community) so route quality claims cite evidence, not
  vibes.

### 5.2 Per decision / attempt

- `DecisionPin.model` already pins the adapter; extend the pin value to include the exact model
  revision and effort variant so a changed revision invalidates reuse (already required: "a
  changed pin is a changed input").
- Record per decision: kind, eligible options offered, chosen/fallback, score semantics and
  calibration version, deadline, latency, and inference cost (architecture §3.7 durability).
- Record per attempt: model id + revision + effort, prompt/genome versions, harness profile,
  task family, role/phase, attempt number, escalation tier, and prior-diagnosis artifact hash.
- Usage snapshot: input/output/cache-read/cache-write tokens, `cost_usd`, and
  "unknown vs zero" distinction (already required by §9). Add `plan_quota_consumed_usd` and
  `cash_cost_usd` separately for Go.

### 5.3 `$/merge` accounting under subscriptions and free quotas

- Numerator: sum **all** inference costs for the task — failed attempts, retries, shadow
  decisions, diagnosis sessions — not just the merged attempt (already implied by §9:
  "All inference attempts, including failed or shadow decisions, count toward run economics").
- Go: cash cost is $0 within the monthly plan; the scarce resource is the per-model allowance.
  Track both `cash_usd` and `notional_quota_usd` per task/model, plus remaining allowance at
  dispatch time; otherwise $/merge looks artificially $0 and route comparisons distort.
- Free/limited models: record `cost=0` but tag `quota_kind=free`, `data_policy=training|zero-retention`,
  and a `expires_or_review_by` date; a route is not promotable if its only evidence comes from
  a model whose free window closed.
- DeepSeek: tag peak/off-peak at request time; the same route has 2x price in peak windows.
- Cache: cache read/write prices are 5-100x cheaper/richer than input; keep cache-hit ratio per
  leaderboard cell and include cache write cost in decision inference cost.

### 5.4 Evaluation hooks tying routes to the decision contract

- Leaderboard cells `(harness profile, model, role, task family) → success, $/merge, latency,
  repair, escalation, cache-hit` already defined; add `(model revision, decision kind, task
  family)` for Jev/generative deciders with accuracy, calibration, fallback rate, routing
  regret versus the static baseline (all already in §3.7/§9 — the metadata above is what makes
  them computable).
- Run new models/routes in shadow against the static baseline; promote through genome gates
  only after downstream `$/merge` and success evidence (no quality claims from benchmark
  scores alone).
- Drift guard: stealth/free models must pass a monthly re-verification (availability, identity
  signals, sample task success); a changed model id/revision requires new evidence before
  route reuse.
- Confidentiality guard: the router must refuse routes whose data-policy class forbids the
  task's content class (free/training models vs protected fixtures/secrets), independent of
  cost.

---

## 6. Sources (all accessed 2026-10-09 unless noted)

Local/authoritative:

- OpenCode model catalog tool (`tools.opencode.models`) — availability, status, prices,
  variants (2026-10-09).
- `~/.cache/opencode/models.json` — models.dev metadata cache (context limits, tool/structured
  output flags, release dates; read 2026-10-09).
- <https://opencode.ai/docs/zen/> — Zen pricing, free-model list, privacy/data policy
  (page last updated 2026-10-08).
- <https://opencode.ai/docs/go/> — Go plans, usage limits, token-cost accounting, privacy
  table (last updated 2026-10-08).
- <https://opencode.ai/docs/models/> — variants, model config semantics.

Benchmarks and vendor material:

- SWE-bench Verified leaderboard: <https://benchlm.ai/benchmarks/swe-bench-verified> (updated
  2026-10-09); <https://llm-stats.com/benchmarks/swe-bench-verified> (updated 2026-10-10);
- Terminal-Bench 2.0: <https://benchlm.ai/benchmarks/terminal-bench-2> (2026-10-09);
  <https://www.vals.ai/benchmarks/terminal-bench-2> (archived 2026-06-04).
- SWE-bench Pro: <https://benchlm.ai/benchmarks/swe-bench-pro> (2026-09-30).
- Vals AI model pages: [Haiku 5.5](https://www.vals.ai/models/anthropic_claude-haiku-5-5),
  [GPT-6 Luna](https://www.vals.ai/models/openai_gpt-6-luna),
  [Muse Spark 1.3](https://www.vals.ai/models/meta_muse_spark_1_3),
  [Grok 4.7](https://www.vals.ai/models/grok_grok-4.7),
  [Kimi K3](https://www.vals.ai/models/kimi_kimi-k3),
  [GLM 5.3 Flash](https://www.vals.ai/models/zai_glm-5.3-flash),
  [MiMo V2.6 Pro](https://www.vals.ai/models/xiaomi_mimo-v2.6-pro) /
  [Flash](https://www.vals.ai/models/xiaomi_mimo-v2.6-flash),
  [DeepSeek V4 Pro](https://www.vals.ai/models/deepseek_deepseek-v4-pro),
  [Qwen3.8 Max](https://www.vals.ai/models/alibaba_qwen3.8-max),
  [GPT-5.5](https://www.vals.ai/models/openai_gpt-5.5) (reference point).
- Vendor/model cards: [Kimi K2.7 Code](https://www.kimi.com/en/resources/kimi-k2-7-code) and
  [HF card](https://huggingface.co/moonshotai/Kimi-K2.7-Code);
  [DeepSeek V4.1 Flash on NVIDIA](https://build.nvidia.com/deepseek-ai/deepseek-v4.1-flash) and
  [DeepSeek news](https://www.deepseek.com/en/news/deepseek-v4-1-flash);
  [Qwen3.8-Flash summary](https://docs.b.ai/llmservice/models/qwen3-8-flash) and
  [HF Qwen3.8-Flash-Next](https://huggingface.co/Qwen/Qwen3.8-Flash-Next);
  [Z.ai GLM-5.3 blog](https://z.ai/blog/glm-5.3);
  [Step 5 Preview wiki](https://aiwiki.ai/wiki/step_5_preview) and
  [themodelgap analysis](https://themodelgap.com/models/step-5-preview);
  [Big Pickle SWE Atlas](https://github.com/PhillipChaffee/big-pickle-swe-atlas) and
  [news write-up](https://www.alextech.ai/en/news/big-pickle-beats-gpt-56-sol-on-swe-atlas-benchmark);
  [Space Bunny empirical notes](https://huggingface.co/blog/karmen-beatapi/space-bunny-tested-real-cases-evaluation);
  [Ling 3.0 Flash benchmarks](https://benchlm.ai/models/ling-3-0-flash);
  [Ling 3.1 Flash](https://benchlm.ai/models/ling-3-1-flash);
  [MiMo V2.6 launch](https://www.unite.ai/xiaomis-new-flagship-model-leads-open-weight-rankings-with-a-score-of-46).
- Aider polyglot (checked; none of the catalog's cheap models appear):
  <https://aider.chat/docs/leaderboards/>.

Known gaps (do not fill by guessing): Jev 1.13 benchmarks; Exo Free/Ling 3.1/LongCat
2.5-Preview quality; Muse Spark 1.3 SWE-bench specifically; LongCat-2.0 benchmark rows;
request-rate limits for free Zen models; per-model availability end dates.
