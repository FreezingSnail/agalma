# Cloud model offerings for Agalma roles and the decision contract

Status: research snapshot. All prices/features verified or accessed **2026-10-09** unless
noted; pricing is volatile and must be re-verified before it feeds a routing table.
Method: local OpenCode model catalog (`opencode-go`, `opencode` provider entries,
[28]) plus provider pricing/API docs and public trackers (sources §7). No downloads
or builds were run. Unknowns are marked `?` or "not retrieved" — never guessed.
Relevant Agalma design: `docs/architecture.md` §3.7 (decider models, decision
contract), §9 (metrics), §11 (credentials), and `docs/spikes/s0b-confinement.md`
(provider auth proxy evidence).

## 0. Top findings

1. **A cheap frontier tier now exists at every major US lab:** GPT-6 Luna
   ($0.10/$0.50), Claude Haiku 5.5 ($0.10/$0.50 ≤100K), Gemini 3.1 Flash-Lite
   ($0.25/$1.50) and 2.5 Flash-Lite ($0.10/$0.40). For Agalma this is the
   cheapest-adequate pool for builder/verifier/doc tasks. [1][3][6]
2. **Cache is the dominant agent-loop lever, not output price.** DeepSeek Flash
   cache hits cost $0.003/M off-peak (98% off), OpenAI/Gemini cache reads ~90%
   off, Anthropic cache reads ~90% off but with explicit write costs. An agent
   that re-sends a stable prefix is 5–20× cheaper per turn than one that does
   not. [1][3][6][12]
3. **Depth of structured-output guarantees varies materially.** OpenAI
   (`strict: true` json_schema), Anthropic (JSON outputs + strict tool use), and
   xAI (`response_format.json_schema`) offer grammar-constrained schemas;
   DeepSeek's `json_object` guarantees valid JSON but not schema conformance
   (strict tool calls are beta). Triage/decider routing should prefer the
   constrained providers, with Rust validation regardless. [19][20][21][22][12]
4. **OpenCode Go is a subscription cap on at-cost token rates, not a discount
   rate card.** Haiku 5.5, GPT-6 Luna, and DeepSeek V4.1 Flash are listed at
   direct-provider prices; the $10/mo buys up to a per-model monthly dollar
   allowance ($15–$60) with 20% per 5h / 50% weekly / 100% monthly windows.
   Batch APIs are absent and some free models train on data. [16][17]
5. **`Jev` (OpenCode Zen `systemone` endpoint) is purpose-built for the
   decision contract**: typed yes/no, choice, and score questions returning
   values/probabilities at $0.042/M input, free output. It is a strong
   evaluated candidate for `DecisionApi` triage/ranking inference, but it is
   gateway-hosted, has no public benchmark evidence, and needs a specialized
   adapter. [16]
6. **The provider-auth proxy is compatible with every provider examined.**
   Nearly all use long-lived bearer/x-api-key credentials; Google Vertex is the
   OAuth/service-account exception that forces short-lived token minting inside
   the proxy. Secrets stay parent-side; audit should record credential *IDs*,
   org/project/workspace, request IDs, cache-hit/token usage, and pricing-table
   version per call. [26][27]

---

## 1. Cheapest/fast tiers per provider (direct APIs)

Prices are USD per 1M tokens. "Cache R/W" = cache read / cache write. Latency
class is as marketed or observed by third parties; Agalma has not measured p50/p95
for any of these. `SO` = native structured output / strict schema support.
Context values are from the cited pages; where a page only documents the tier
boundary (e.g. OpenAI's 272K), the full window is marked.

### 1.1 OpenAI — cheapest lines [1][2][19]

| Model id | In | Cache R | Cache W | Out | Context | Batch | SO strict | Latency |
|---|---|---|---|---|---|---|---|---|
| `gpt-6-luna` | 0.10 | 0.01 | 0.125 | 0.50 | 272K tier boundary; full window ? | 50% (standard Batch) | yes (`json_schema` strict; strict tools) | fast |
| `gpt-6-luna` >272K | 0.20 | 0.02 | 0.25 | 0.75 | " | " | " | " |
| `gpt-5.6-luna` | 0.20 | 0.02 | 0.25 | 1.20 | 272K tier boundary | 50% | yes | fast |
| `gpt-5.4-nano` | 0.20 | 0.02 | ? | 1.25 | ? | 50% | yes | fast |
| `gpt-5-nano` | 0.05 | 0.005 | ? | 0.40 | ? | 50% | yes | fast |
| `gpt-5.4-mini` | 0.75 | 0.075 | ? | 4.50 | 400K | 50% | yes | fast |

Notes: premium: quality frontier (gpt-6-astra $10/$50, gpt-6.1-sol $2/$10). Strict
structured outputs are incompatible with parallel tool calls (set
`parallel_tool_calls: false`). Prompt caching is automatic; cache writes bill only
on newest models. Batch = 50% off, async. [1][19]

### 1.2 Anthropic — Haiku line [3][4][5][20]

| Model id | In | Cache R | Cache W (5m/1h) | Out | Context | Batch | SO strict | Latency |
|---|---|---|---|---|---|---|---|---|
| `claude-haiku-5-5` ≤100K prompt | 0.10 | 0.01 | 0.125 / 0.20 | 0.50 | 1M | 50% → 0.05/0.25 | yes: `output_config.format` + `tools[].strict` | fastest Anthropic (~0.73 s observed via OpenRouter) |
| `claude-haiku-5-5` >100K prompt | 0.50 | 0.05 | 0.625 / 1.00 | 2.50 | " | 50% | " | " |
| `claude-haiku-4-5` (prev) | 1.00 | 0.10 | 1.25 / 2.00 | 5.00 | ? | 50% | yes | fast |

Notes: prompt length counts all input including cache reads/writes when deciding
the 100K threshold; each request priced on its own. Strict schemas must set
`additionalProperties: false` and full `required`. [3][20]

### 1.3 Google Gemini — Flash/Flash-Lite lines [6][7][21]

| Model id | In | Cache R | Out | Context | Batch | SO strict | Latency |
|---|---|---|---|---|---|---|---|
| `gemini-2.5-flash-lite` | 0.10 | 0.01 | 0.40 | 1M | 50% → 0.05/0.20 | `responseSchema` (OpenAPI-subset), property ordering, union support | fast |
| `gemini-3.1-flash-lite` | 0.25 | 0.025 | 1.50 | 1M | 50% → 0.125/0.75 | yes | fast |
| `gemini-3.5-flash-lite` | 0.30 | 0.03 | 2.50 | 1M | 50% → 0.15/1.25 | yes | fast |
| `gemini-3.8-flash` | 0.75 list / 0.375 promo | 0.075 / 0.0375 | 3.75 / 1.875 | 1M | 50% | yes | fast |

Notes: Google's page shows Gemini 3.8 Flash promotional $0.375/$1.875 through
2026-12-31 rising to $0.75-class list; third-party trackers list $0.75/$3.75 as
standard. OpenCode Zen resells `gemini-3.8-flash` at $1.50/$7.50 — a resale
discrepancy to re-verify. Thinking tokens bill as output. Free tier sends data to
product improvement. [6][7][16]

### 1.4 xAI — no cheap mini tier [10][22]

| Model id | In | Cache R | Out | Context | Batch | SO strict | Latency |
|---|---|---|---|---|---|---|---|
| `grok-build-0.1` | 1.00 | 0.20 | 2.00 | 256K | none documented | yes (`json_schema`) | coding model |
| `grok-4.3` | 1.25 | 0.20 | 2.50 | 1M | 20% off | yes | standard |
| `grok-4.7` | 2.00 | 0.50 | 6.00 | 500K | **not supported** | yes | flagship |

Notes: any prompt ≥200K bills the whole request at 2×. ZDR disables Batch/stateful
Responses/Files. US regional endpoint 1.1×. xAI is the worst fit for the
cheapest-adequate pool; it may still be useful for a strong escalation tier. [10]

### 1.5 Mistral [11]

| Model id | In | Cache R | Out | Context | Batch | Function calling | Latency |
|---|---|---|---|---|---|---|---|
| `ministral-3-3b` | 0.10 | — | 0.10 | 256K | 50% | ? | edge |
| `mistral-small-3.2` | 0.08 | — | 0.20 | 128K | 50% | yes | high-throughput |
| `mistral-small-4` | 0.15 | 0.015 | 0.60 | 256K (one source: 128K) | 50% | yes | high-throughput |
| `devstral-small-1.1` | 0.10 | — | 0.30 | ? | 50% | yes | SWE-agent tuned |
| `codestral` | 0.30 | — | 0.90 | ? | 50% | yes | code |

Notes: Batch API is documented at 50%. Structured outputs exist (JSON mode) but the
strength of strict schema enforcement was not verified. Good cheap fallback
diversity; unusual among EU-hosted options. [11]

### 1.6 DeepSeek [12]

| Model id | In (cache miss) | In (cache hit) | Out | Context | Batch | SO strict |
|---|---|---|---|---|---|---|
| `deepseek-flash` (V4.1 Flash) off-peak | 0.15 | 0.003 | 0.60 | 1M, 384K max output | **none** | `json_object` only; strict tool calls beta |
| `deepseek-flash` peak | 0.30 | 0.006 | 1.20 | " | " | " |
| `deepseek-v4-pro` off-peak | 0.66 | 0.022 | 1.98 | 1M | " | " |
| `deepseek-v4-pro` peak | 1.32 | 0.044 | 3.96 | " | " | " |

Notes: peak = 01:00–04:00 and 06:00–10:00 UTC, Mon–Fri; all other hours and
weekends are off-peak (as restated by OpenCode Go docs [17]). Automatic prefix
caching; concurrency caps, not RPM: 2,500 Flash / 500 Pro. No Batch API. Cache
hit vs miss is the single largest discount observed (>98%). [12][17]

### 1.7 Moonshot Kimi [13]

| Model id | In | Cache R | Out | Context | Batch | SO strict |
|---|---|---|---|---|---|---|
| `kimi-k2.5` | 0.60 | ? | 3.00 | 256K | ? | ? |
| `kimi-k2.6` | 0.95 | 0.16 | 4.00 | 256K | 60% of price (40% off) | ? |
| `kimi-k2.7-code` | 0.95 | 0.19 | 4.00 | 256K | 60% of price | ? |
| `kimi-k3` | 3.00 | 0.30 | 15.00 | 1M | not listed | ? |

Notes: no cheap/flash line; OpenCode Go carries the same models at the same token
prices. Not a cheapest-tier candidate, but a plausible planning/escalation model.

### 1.8 Z.ai GLM [14]

| Model id | In | Cache R | Out | Context | Batch | Function calling |
|---|---|---|---|---|---|---|
| `glm-4.7-flash` | Free | Free | Free | 200K | ? | yes |
| `glm-5.3-flash` | 0.15 (one tracker: 0.08) | ? | 0.50 (one tracker: 0.25) | 1M | ~50% via batch listings ($0.06/$0.20) | yes |
| `glm-4.7` | 0.60 | 0.11 | 2.20 | 200K | ? | yes |
| `glm-5.3` | 1.40 | 0.26 | 4.40 | 1M | ? | yes |

Notes: GLM-5.3-Flash is a leading cheap candidate; the $0.08/$0.25 vs $0.15/$0.50
discrepancy between trackers is unresolved. GLM-4.7-Flash is a true free tier for
budget-sensitive loops. [14]

### 1.9 Qwen / Alibaba Model Studio [15]

| Model id | In | Cache R | Out | Context | Batch | Function calling |
|---|---|---|---|---|---|---|
| `qwen3.7-flash` (<32K) | 0.03 | ? | 0.13 | 1M | ? | yes (OpenAI-compatible) |
| `qwen3.8-flash` (intl list) | 0.15 | context-cache discount | 0.47 | 1M | ? | yes |
| `qwen3.8-flash` (Global rate) | 0.113 | ? | 0.382 | 1M | ? | yes |
| `qwen3.8-max` | 2.00 | 0.25 / write 2.50 | 6.00 | 1M | ? | yes |

Notes: Qwen Cloud/Model Studio is OpenAI-compatible (`DASHSCOPE_API_KEY`,
Singapore/Beijing/US endpoints). Batch support and explicit context-cache rates
were not retrieved. [15]

### 1.10 Resale cheap lines: OpenCode Zen and Go [16][17][28]

Pay-as-you-go (Zen), at-cost (token prices identical to direct sources where
comparable):

| Model | In | Out | Cache R/W | Notes |
|---|---|---|---|---|
| `gpt-5-nano` | 0.05 | 0.40 | 0.005 | |
| `jev-1.13` | 0.042 | Free | — | systemone decision endpoint; typed questions |
| `claude-haiku-5-5` | 0.10/0.50 | 0.50/2.50 | 0.01/0.125 | tiered at 100K like direct |
| `gpt-6-luna` | 0.10/0.20 | 0.50/0.75 | 0.01/0.125 | tiered at 272K |
| `glm-5.3-flash` | 0.15 | 0.50 | 0.03 | |
| `qwen3.8-flash` | 0.15 | 0.47 | 0.016/0.20 | |
| `deepseek-v4.1-flash` | 0.30 | 1.20 | 0.006 | Zen lists peak; Go lists both |
| free: `big-pickle`, `mimo-v2.6-flash-free`, `longcat-2.5-preview-free`, `step-5-preview-free`, `exo-free`, `space-bunny-free`, `ling-*-free`, `nemotron-*-free`, `jev-1.13-free`, `muse-spark-1.3-contributor-free` | 0 | 0 | 0 | limited-time; several train on data |

Go ($10/mo; Go Plus $40/mo): same token rates but per-model monthly dollar caps
($15/$30/$60), enforced as 20% per 5h, 50% per week, 100% per month. Cheap-line
caps: Haiku 5.5 $15, GPT-6 Luna $15, GLM-5.3-Flash $60, Qwen3.8 Flash $30,
DeepSeek V4.1 Flash $60 (off-peak and peak both counted), MiMo-V2.6-Flash $60,
Muse Spark Contributor $60 (trains on prompts — do not use for Agalma source).
Fallback to Zen balance is opt-in. Go requires an agent-like user agent and
`x-opencode-session` for prompt-cache routing. [16][17]

---

## 2. Structured outputs, caching, batch, and limits

### 2.1 Structured-output / tool-calling fidelity

| Provider | Schema-constrained output | Strict tool args | Notes / failure modes |
|---|---|---|---|
| OpenAI | Yes — `response_format.json_schema` + `strict:true` | Yes — `tools[].strict` | Best-in-class guarantee; no parallel calls with strict; unsupported schema keywords error at request time. [19] |
| Anthropic | Yes — `output_config.format` JSON schema | Yes — `tools[].strict` | Grammar-constrained; restricted schema subset; must set `additionalProperties:false` + full `required`. Hard-fails on large/unsupported schemas rather than degrading. [20] |
| Google | Yes — `responseSchema` / `response_format.schema` | Yes | OpenAPI 3.0-ish subset; expanded JSON Schema support with property ordering; thinking tokens counted as output. [21] |
| xAI | Yes — `response_format.json_schema` with `strict` in Responses API | Yes | Practical JSON-Schema subset; structured outputs combine with built-in tools. [22] |
| DeepSeek | JSON mode (`response_format.json_object`) = valid JSON only | Beta `strict` tool-call mode | No schema conformance guarantee; Rust must validate every decision payload. [12] |
| Mistral | JSON mode / structured outputs; function calling | Not verified | Detail pages exist; strictness unspecified in retrieved sources. [11] |
| Qwen | OpenAI-compatible JSON/tool calls; strictness not retrieved | — | [15] |
| Kimi / GLM | Tool calling present; strict-schema guarantee not retrieved | — | [13][14] |

For the decision contract (§3.7), prefer OpenAI strict JSON, Anthropic strict tool
use, or Google `responseSchema`; treat DeepSeek JSON mode as prompt-plus-validate.
Jev's `noul`/`choice`/`score` responses are structurally typed by design and are a
decision-native alternative where a specialized adapter is acceptable. [16]

### 2.2 Caching (the agent-loop economics)

| Provider | Mechanism | Discount | Notes |
|---|---|---|---|
| OpenAI | automatic prefix cache | ~90% on cached input ($0.01 vs $0.10) | newest models also bill cache writes ($0.125). [1] |
| Anthropic | explicit `cache_control`, 5m/1h TTL | read 0.10×, write 1.25×/2.0× | threshold pricing counts cache tokens as input. [3] |
| Google | implicit + explicit context cache | ~90% on implicit; explicit adds storage cost/hour | storage $0.50–$1.00 per 1M tokens/hour (flash lines). [6] |
| DeepSeek | automatic disk cache | cache hit ≈2% of miss ($0.003 vs $0.15 off-peak) | largest observed discount; persistence documented. [12] |
| xAI | prefix cache | cached $0.20–$0.50 vs $1–$2 input | ZDR disables stateful features. [10] |
| OpenCode Go | cached-read prices + session header | same as underlying models | `x-opencode-session` used for routing/cache optimization. [17] |

Consequence for Agalma: phase isolation (fresh session per phase) reduces cache
value; a stable system-prompt + handoff prefix per role is where the savings live.
The metrics ledger already has `cached_in`/`cache_write` fields to record this.

### 2.3 Batch APIs

| Provider | Discount | Latency | Fit for Agalma |
|---|---|---|---|
| OpenAI | 50% | async (24h target) | bench/eval, mining, bulk doc checks |
| Anthropic | 50% | async | same |
| Google | 50% (Batch + Flex) | async | same |
| Mistral | 50% | async | same |
| Moonshot | 40% (60% of price) | async | same |
| xAI | 20% only on 4.3/4.20; not on 4.7 | async | marginal |
| DeepSeek | none | — | off-peak is the equivalent lever |
| OpenCode Zen/Go | none exposed | — | gateway does not pass batch through |

Batch is unusable for interactive agent turns; it is relevant to L3 bench, history
replay, and bulk evaluation where latency is unconstrained. [1][3][6][11][13][10]

### 2.4 Rate limits and quotas on low tiers

- **OpenAI**: limits are per project, per model, in RPM/TPM (plus batch queues).
  A Tier-1 paid account was reported around 500K TPM / ~1K RPM for frontier
  models in early 2026; model-specific gpt-6-luna numbers were not retrieved.
  Budgets/limits are configurable per project; the Usage/Cost APIs expose
  per-project, per-model, per-key attribution. [23][27]
- **Anthropic**: tiers consolidated to Start/Build/Scale; Build reported at
  5,000 RPM / 5M ITPM / 1M OTPM; older Tier-1 Haiku-class figures were ~50 RPM /
  50K ITPM / 10K OTPM. 529 overload is distinct from 429 rate limiting. Exact
  current Haiku 5.5 numbers were not retrieved. [24]
- **Google Gemini**: free tier Flash ~10 RPM / 1,500 RPD, Flash-Lite ~15 RPM /
  1,000 RPD (Sep 2026), and a forum report of 20 RPD for 3.8 Flash; paid Flash
  ~2,000 RPM. Free-tier prompts are used to improve products. [8][9]
- **DeepSeek**: no RPM cap; concurrency 2,500 (Flash) / 500 (Pro); practical
  limits are capacity and off-peak scheduling. [12]
- **OpenCode Go**: hard per-model rolling windows ($ × 20%/50%/100%); requests
  beyond limits are blocked unless Zen-balance fallback is enabled; usage is
  monitored for "abuse that degrades the experience for other users"; only one
  member per workspace can subscribe. [17]
- **xAI**: prepaid credits or monthly invoice; invoiced accounts start at a $0
  spend limit (raised manually); priority tier 2×. [10]
- **Mistral / Moonshot / Z.ai / Qwen**: tier or concurrency limits not retrieved;
  treat as unknown and bound with proxy-side budgets.

---

## 3. Auth/secrets implications for Agalma (`ProviderAuthApi`)

Design constraints already fixed: credentials never enter the sandbox; the worker
reaches a parent-owned loopback auth proxy; arbitrary network is denied; the proxy
and narrow bridge were proved in S0b (`docs/spikes/s0b-confinement.md`). The
mapping below assumes the proxy terminates provider auth and may inject headers,
while OpenCode inside the sandbox talks only to the proxy (or to the OpenCode
server's provider layer configured with proxy-scoped tokens).

### 3.1 Per-provider auth shape

| Provider | Credential | Headers/flow | OAuth? | Credential-scoping knobs |
|---|---|---|---|---|
| OpenAI | API key (`sk-…`), optional admin key | `Authorization: Bearer`; `OpenAI-Organization`, `OpenAI-Project` when needed | No public API OAuth (ChatGPT OAuth is for Codex product, not Agalma) | project-scoped keys; per-project budgets/limits; usage+cost API by key/project/model [27] |
| Anthropic | API key (`sk-ant-…`), Workspace ID | `x-api-key` + `anthropic-version` | No (Claude Code OAuth is not an API credential) | workspace segmentation; Admin API key (`sk-ant-admin`) for usage/cost reports [26] |
| Google Gemini | API key (`x-goog-api-key`) | header key, project-bound | Gemini Developer API: key. Vertex: OAuth2 access token / service account | project + API restrictions; service account held by proxy; tokens expire ~1h [6] |
| xAI | API key | `Authorization: Bearer` | No | per-team console; per-response `cost_in_usd_ticks` [10] |
| Mistral | API key | `Authorization: Bearer` | No | workspace keys [11] |
| DeepSeek | API key | `Authorization: Bearer` | No | account keys; balance endpoint [12] |
| Moonshot Kimi | API key | `Authorization: Bearer` | No | [13] |
| Z.ai GLM | API key | `Authorization: Bearer` | No | [14] |
| Qwen/Model Studio | `DASHSCOPE_API_KEY` | Bearer via OpenAI-compatible base URLs (regional) | Alibaba RAM/service credentials exist for enterprise paths | regional endpoints; per-model free quota [15] |
| OpenCode Zen | OpenCode API key | `Authorization: Bearer` to `https://opencode.ai/zen/v1/...` | OpenAuth (GitHub/Google) for console only | workspace roles, per-member budgets, model enable/disable, BYO OpenAI/Anthropic key [16] |
| OpenCode Go | OpenCode API key | Bearer to `https://opencode.ai/zen/go/v1/...`; send `x-opencode-session` | console OAuth | one subscriber per workspace; per-model caps; usage console [17] |

### 3.2 Proxy mapping

1. **Sandbox identity**: OpenCode gets a per-attempt, proxy-issued token (not the
   provider key). The proxy binds token → attempt/execution ID → allowed provider
   routes. S0b proved worker reads of the credential file are denied; the proxy
   is the only holder.
2. **Header injection**: the proxy rewrites `Authorization` / `x-api-key` /
   `x-goog-api-key` per upstream, adds `OpenAI-Organization`/`-Project` or
   Anthropic workspace headers when configured, and strips any inbound
   credential-like headers from worker traffic.
3. **OAuth exception**: Vertex service-account JWT → short-lived access token is
   minted and refreshed *inside* the proxy. No service-account JSON ever renders
   into the worker's XDG/config tree. Everything else in this list is a
   long-lived API key that must be treated as a high-value secret (rotation,
   per-provider scoping, no logging).
4. **Egress allowlist (upstream, parent-side)**: `api.openai.com`,
   `api.anthropic.com`, `generativelanguage.googleapis.com` and the Vertex
   regional host(s), `api.x.ai`, `api.mistral.ai`, `api.deepseek.com`, the
   Moonshot/`platform.kimi.ai` host, `api.z.ai`, Alibaba Model Studio regional
   hosts, `opencode.ai` (`/zen/v1`, `/zen/go/v1`). The sandbox itself keeps only
   loopback to the proxy (plus the nerve socket); S0b's deny-by-default profile
   stays intact. Any provider-required auxiliary host must be added explicitly
   and revisited when the model catalog changes.
5. **Harness reality check**: S0a/S0d found `/api/provider` is not authoritative
   for auth and `OPENCODE_API_KEY` did not populate the isolated catalog; the
   OpenCode adapter must not infer credential validity from provider listing.
   Reconcile through the proxy and the usage/cost APIs instead. [28][29]

### 3.3 Audit record (minimum) per inference or decision call

- Agalma operation/decision ID, execution ID, attempt, phase, role, model ID +
  variant (e.g. reasoning effort), binding generation.
- Provider + gateway (`direct` | `opencode-zen` | `opencode-go`), endpoint host,
  and whether the call traversed the proxy.
- Credential **identifier** (provider key ID / admin key ID / service account
  name / OpenCode workspace key alias) — never the secret value.
- Org/project/workspace/team identifiers when the provider exposes them.
- Provider request/correlation ID; provider-reported model revision or
  fingerprint if any; service tier/region.
- Usage: input, cached input, cache write, output (and reasoning) tokens with a
  completeness flag (`Complete|Partial|Unknown`); DeepSeek cache-hit vs miss;
  xAI `cost_in_usd_ticks`.
- Computed cost + the pricing-table version used; timestamp in UTC with offset
  (DeepSeek peak/off-peak billing depends on it).
- Data-retention class of the model (ZDR / 30-day / trains-on-data) — required
  before any task with target source code is allowed on that route. Go/Zen
  publish per-model training/retention tables; several free models train. [16][17]
- Egress decision: allowlist entry matched; denials recorded with reason.

These fields extend the existing `runs` / `decisions` ledger rows (§9) rather
than inventing a parallel store.

---

## 4. Reseller vs direct: OpenCode Go/Zen vs provider APIs

### 4.1 Price deltas

- **Token rates: ~0% markup.** For GPT-6 Luna, Claude Haiku 5.5, GLM-5.3-Flash,
  Qwen3.8 Flash, and DeepSeek V4.1 Flash the Zen/Go table matches direct list
  prices (DeepSeek via Go uses peak/off-peak correctly). Zen states it sells at
  cost; card fees pass through at 4.4% + $0.30 per top-up. [16][17]
- **Go subscription changes effective price via bulk/reserved capacity:** $10/mo
  yields up to $15–$60 of per-model usage. Direct APIs have no equivalent
  subscription; heavy cheap-model use will be cheaper on Go when usage is steady
  and within caps.
- **Occasional/quiet use favors Zen/direct:** no fixed fee, no monthly floor.
- **Batch: absent in resale.** Any 50% batch saving needs direct APIs.
- **Discrepancies to re-verify:** Zen lists Gemini 3.8 Flash at $1.50/$7.50 while
  Google lists $0.75/$3.75 (promo $0.375/$1.875); GLM-5.3-Flash appears at both
  $0.15/$0.50 and $0.08/$0.25 across listings. [6][14][16]

### 4.2 Convenience

One key and one billing relationship for ~30 models; curated provider serving;
OpenAI-compatible/Anthropic-compatible endpoints per model; prompt-cache routing
via session header; free models for M1-class loops; usage console; workspace
roles and per-member budgets; BYO OpenAI/Anthropic key for the expensive
frontier while still using Zen for the rest. [16][17]

### 4.3 Limits and risks

- **Quota cliffs:** Go caps are rolling (5h/week/month); a burst can 429 mid-task
  even with budget left on other models. Falling back to Zen balance is manual.
- **Subscription churn:** Go's model list and caps can change; free models are
  explicitly limited-time. Routing tables must tolerate model disappearance.
- **No batch, no per-call limits API, no provider audit APIs:** reconciliation is
  via OpenCode console, not OpenAI/Anthropic usage endpoints. Independent
  accounting is weaker than direct.
- **Data policy varies per model:** most Go/Zen models are ZDR or 30-day
  no-training, but free and "Contributor" models train on prompts; NVIDIA free
  endpoints are trial-only. Agalma must gate routes on retention class.
- **Traffic monitoring and client identification:** Go requires coding-agent
  traffic, a non-generic user agent, and session headers; abusive patterns may be
  blocked. An automated factory with parallel attempts must respect this.
- **Resale opacity:** the same model ID may be served by a different upstream;
  quality/latency revisions are not pinned by ID. Agalma's model onboarding
  ("a new ID or variant is a new model") needs a provider-serving fingerprint or
  re-screen trigger when the gateway reroutes.
- **Single subscriber per workspace;** org-level procurement/per-key policies are
  thinner than enterprise direct agreements.

### 4.4 Recommendation framing

Use **OpenCode Zen/Go for M1–M2 cheap and free traffic** (aligns with the M1
"free model via egress proxy; paid models open" stance), keeping the proxy in
front. Add **direct accounts** for (a) batch-heavy bench/eval, (b) per-key
budgets/limits that the factory can enforce, (c) usage/cost APIs for independent
accounting, and (d) any provider whose ZDR/contract terms the factory needs.
Never route target source through train-on-data models. Re-evaluate before M3
budgets; re-verify prices and caps then.

---

## 5. Draft routing notes by task class (cheapest-adequate)

Candidates are ordered by expected cost-per-merged-change, not raw token price.
All confidences are pre-bench and must be treated as hypotheses. Static baseline
(`static/v1` in S0d) still applies until shadow decisions are evaluated.

| Task class | Role/phase | Candidates (cheapest first) | Rationale | Confidence |
|---|---|---|---|---|
| **Triage JSON** (`triage.pick-next`, `retry.escalate`, memory/task ranking) | decider (`DecisionApi`) | 1. `jev-1.13` ($0.042/M in, free out; typed noul/choice/score) 2. `gpt-6-luna` strict `json_schema` 3. `claude-haiku-5-5` strict tool use 4. `gemini-3.1-flash-lite` `responseSchema` 5. `deepseek-flash` `json_object` + Rust validation | Decision contract wants bounded IDs/scores, small prompts; strict schemas eliminate parse failures. Jev is decision-native but unproven and needs a specialized adapter. | Schema fidelity: high (1–4), medium (5). Quality/latency: low until evaluated. |
| **Docs edit** (M1 seed: markdown, grep acceptance) | builder | 1. free models (`mimo-v2.6-flash-free`, `longcat-2.5-preview-free`, `glm-4.7-flash`) 2. `gpt-6-luna` 3. `mimo-v2.5` ($0.14/$0.28) 4. `glm-5.3-flash` / `qwen3.8-flash` | Lowest-risk work; free/ZDR routes acceptable; acceptance is mechanical (grep/content). | Medium — M1 is designed to validate exactly this. |
| **Small fix** (single-file code + existing tests) | builder | 1. `gpt-6-luna` 2. `claude-haiku-5-5` 3. `deepseek-flash` (off-peak + cache) 4. `glm-5.3-flash` / `qwen3.8-flash` 5. `mimo-v2.6-flash` | Needs reliable tool-call/edit discipline more than raw reasoning; cache-heavy loop favors DeepSeek; Haiku/Luna give strict tool schemas. | Low–medium; no Agalma bench yet. |
| **Diagnosis** (red acceptance → `diagnosis.md`) | verifier (agent-side) | 1. `deepseek-flash` (1M ctx, cheap output, off-peak) 2. `gemini-3.8-flash` (1M ctx; thinking billed as output) 3. `grok-build-0.1` 4. `glm-5.3-flash` | Long failure logs + diff; output is a short analysis, so cheap output lines matter; thinking models can hide cost in output. | Low. |
| **Planning** (decompose task → plan artifact) | planner | 1. `deepseek-v4-pro` ($0.66/$1.98) 2. `glm-5.3` ($1.40/$4.40) 3. `kimi-k2.7-code` 4. `gpt-6.1-sol` / `claude-sonnet-5` | Quality-sensitive; do not use flash tiers for plans. Cost is bounded by one phase, so quality per dollar dominates. | Low. |
| **Escalation (attempt ≥2)** | any | per `static/v1`: escalate cheap → standard (`gpt-6.1-sol`, `sonnet-5`, `glm-5.3`, `gemini-3.1-pro`) | Mechanical ladder already defined; decider may pick among eligible options only. | High (policy), model choice low. |

Notes: `muse-spark-1.3-contributor` ($0.10/$0.20) trains on prompts — exclude for
any Agalma source. `nvidia` free endpoints are trial-only. Route gates must
consult the retention class, not just price.

### 5.1 Benchmark evidence missing for a real router

1. **No Agalma data yet.** M1's seed batch is docs-only; there is no builder/
   verifier success rate or $/merge per model, per role, per task family.
2. **Tool-loop fidelity:** tool-call schema-violation rate, repair rate, repeat
   loops, edit-format errors, and compaction behavior inside OpenCode for each
   candidate — the failure classes architecture §10 wants measured before
   porting mechanisms.
3. **Structured-output conformance for deciders:** per-model JSON-schema
   adherence, abstention/malformed rate, and calibration on Agalma-labeled
   decisions; confidence thresholds cannot be set before that.
4. **Cache behavior in the real loop:** measured cache-hit ratio and cache-write
   overhead per phase given fresh-session phase isolation.
5. **Latency:** p50/p95 turn latency and throughput under N parallel attempts
   (including gateway queueing on Go/Zen).
6. **Rate-limit behavior:** 429 incidence at low tiers (OpenAI Tier 1, Anthropic
   Build, Gemini free, Go rolling caps) and the true per-model limits for
   gpt-6-luna/Haiku 5.5, which are not published in retrieved pages.
7. **Economics completeness:** whether harness usage reports separate cache
   read/write and reasoning tokens for every route (needed for honest $/merge).
8. **Resale stability:** model-ID → upstream mapping churn, silent reroutes,
   ZDR renewal (e.g. DeepSeek via Go renewed through 2026-10-31), and cap
   changes. A route must be able to auto-revert on hard failures (§9).
9. **Jev evaluation:** accuracy/latency on Agalma's decision kinds vs the static
   baseline and vs strict-schema generative models, plus adapter cost.
10. **Model revision pinning:** provider-announced version bumps as re-screen
    triggers; gateway version IDs may not expose revisions.

Until (1)–(3) exist, the routing table should stay static or shadow-only, per
architecture §9 and §3.7.

---

## 6. Open questions / honest unknowns

- `gpt-6-luna` full context window (272K is only the pricing tier boundary).
- Gemini 3.8 Flash list vs promotional pricing and Zen's $1.50/$7.50 listing.
- GLM-5.3-Flash official first-party price ($0.08/$0.25 vs $0.15/$0.50).
- Batch support/pricing for Qwen/Model Studio, Z.ai, and Kimi details beyond K2.x.
- Current exact rate limits for gpt-6-luna, Haiku 5.5, Mistral, Kimi, GLM.
- OpenCode Go contractual SLA, upstream-provider pinning, and long-term caps.
- Whether Zen's Jev endpoint supports the full decision vocabulary Agalma needs
  (multi-question batching is documented; score semantics/pricing stability are
  not).
- Vertex-on-Gemini vs Gemini Developer API choice for Agalma (OAuth complexity
  vs key simplicity); not yet decided.

---

## 7. Sources (accessed 2026-10-09)

1. OpenAI API pricing — https://developers.openai.com/api/docs/pricing
2. OpenAI pricing summary (Sep 2026) — https://ai-magazine.com/api-pricing/openai/
3. Anthropic pricing — https://platform.claude.com/docs/en/about-claude/pricing
4. Anthropic Haiku page — https://www.anthropic.com/claude/haiku
5. Haiku 5.5 price tracker — https://llm-cost.io/model/anthropic--claude-haiku-5.5/
6. Gemini Developer API pricing — https://ai.google.dev/gemini-api/docs/pricing
7. Gemini pricing tracker (Oct 2026) — https://benchlm.ai/google/api-pricing
8. Gemini rate limits — https://ai.google.dev/gemini-api/docs/rate-limits
9. Gemini free-tier limits (Sep 2026) — https://pecollective.com/tools/gemini-free-tier-guide/
10. xAI pricing (updated 2026-09-29) — https://docs.x.ai/developers/pricing;
    Grok pricing analysis — https://blog.laozhang.ai/en/posts/xai-grok-api-pricing
11. Mistral API pricing — https://docs.mistral.ai/inference/pricing;
    https://mistral.ai/pricing/; batch API — https://mistral.ai/news/batch-api/
12. DeepSeek pricing — https://api-docs.deepseek.com/quick_start/pricing;
    context caching — https://api-docs.deepseek.com/guides/kv_cache/;
    tool calls — https://api-docs.deepseek.com/guides/tool_calls/;
    V4.1 Flash peak/off-peak — https://www.morphllm.com/deepseek-api and
    https://apidog.com/blog/deepseek-v4-1-flash-pricing/
13. Kimi API pricing — https://platform.kimi.ai/docs/pricing/chat;
    tracker — https://benchlm.ai/moonshot/api-pricing
14. Z.ai pricing — https://docs.z.ai/guides/overview/pricing;
    https://developer.puter.com/tutorials/zai-glm-api-pricing/;
    https://costgoat.com/pricing/glm-api
15. Alibaba Model Studio pricing —
    https://www.alibabacloud.com/help/en/model-studio/model-pricing;
    Qwen3.8-Flash-Next blog — https://qwen.ai/blog?id=qwen3.8-flash-next
16. OpenCode Zen docs (updated 2026-10-08) — https://opencode.ai/docs/zen/
17. OpenCode Go docs (updated 2026-10-08) — https://opencode.ai/docs/go/
18. OpenCode providers docs — https://opencode.ai/docs/providers/
19. OpenAI structured outputs —
    https://developers.openai.com/api/docs/guides/structured-outputs
20. Anthropic structured outputs —
    https://platform.claude.com/docs/en/build-with-claude/structured-outputs
21. Gemini structured outputs —
    https://ai.google.dev/gemini-api/docs/structured-output;
    function calling — https://ai.google.dev/gemini-api/docs/function-calling
22. xAI structured outputs —
    https://docs.x.ai/developers/model-capabilities/text/structured-outputs;
    function calling — https://docs.x.ai/developers/tools/function-calling
23. OpenAI rate limits —
    https://developers.openai.com/api/docs/guides/rate-limits;
    Vellum rate-limit survey — https://www.vellum.ai/blog/how-to-manage-openai-rate-limits-as-you-scale-your-app
24. Anthropic rate limits — https://platform.claude.com/docs/en/api/rate-limits;
    https://aicatchup.com/news/claude-platform-api-rate-limits-tier-simplification
25. OpenAI batch pricing example —
    https://costperprompt.com/models/openai-gpt-4-1-mini-batch
26. Anthropic Admin API usage/cost —
    https://www.toriihq.com/articles/how-to-monitor-spending-claude
27. OpenAI API auth and usage/cost APIs —
    https://developers.openai.com/api/reference/overview and
    https://developers.openai.com/api/cookbook/examples/completions-usage-api
28. Local OpenCode model catalog output (`opencode-go`, `opencode`), 2026-10-09;
    Agalma spikes: `docs/spikes/s0a-harness.md`, `docs/spikes/s0d-seam-freeze.md`
29. Agalma design references: `docs/architecture.md` §3.7/§9/§11;
    `docs/component-contracts.md` (`ProviderAuthApi`); `docs/spikes/s0b-confinement.md`
</content>
</invoke>
