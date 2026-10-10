//! H3 static model routing (`agalma-zxi.4`).
//!
//! A code-resident [`ModelSpec`] table (prices, context, strict-schema flag,
//! cash/quota class) plus cheapest-adequate route classes selected from the
//! task `family` and phase role. Routing is a **static policy**: no evaluation
//! and no network here. Each selection is recorded as a `route.model` decision
//! in the ledger `decisions` table through [`StaticDecider`]; the resolved model
//! is then pinned for the attempt/operation by the phase wiring.
//!
//! Defaults stay free-only: paid candidates are skipped unless
//! `AGALMA_ALLOW_PAID=1`. `--model` (or `AGALMA_MODEL`) overrides every class
//! and is recorded as an `override` origin.
//!
//! ## Table revision
//!
//! Prices were re-verified against `docs/research/opencode-models.md`
//! (snapshot **2026-10-09**) at implementation time **2026-10-09**. Prices are
//! USD per 1M tokens; `opencode-go` values are usage-accounting prices that
//! drain the per-model monthly allowance (cash is $0 inside the plan, tracked as
//! notional quota). DeepSeek rows use the off-peak tier. Free rows are
//! limited-time offers.

use std::path::Path;

use agalma_contracts::{
    ContractError, DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionPin,
    DecisionRequest, OperationId, TaskFamily, DECISION_API_VERSION,
};
use agalma_decision::{StaticDecider, ROUTE_MODEL_BASELINE};
use agalma_ledger::SqliteLedger;
use serde_json::json;

/// Environment flag unlocking paid route candidates (`1`/`true`/`yes`).
pub const ALLOW_PAID_ENV: &str = "AGALMA_ALLOW_PAID";
/// Environment override for the model identifier (same precedence as `--model`).
pub const MODEL_ENV: &str = "AGALMA_MODEL";
/// Revision marker for the [`MODEL_TABLE`] prices (research snapshot date).
pub const MODEL_TABLE_REVISION: &str = "2026-10-09";
/// Provider id served by the paid credential-injecting proxy (`opencode-go`).
pub const PAID_PROVIDER: &str = "opencode-go";

/// How a spec bills cash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CashClass {
    /// Free tier: no cash.
    Free,
    /// Subscription allowance: cash $0 inside the plan, drains notional quota.
    Subscription,
    /// Pay-as-you-go: real cash per token.
    PayPerToken,
}

impl CashClass {
    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            CashClass::Free => "free",
            CashClass::Subscription => "subscription",
            CashClass::PayPerToken => "pay-per-token",
        }
    }
}

/// Which scarce bucket a spec consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaClass {
    /// Free / limited-time offer (no paid quota).
    Free,
    /// `opencode-go` per-model monthly allowance.
    GoAllowance,
    /// Zen pay-as-you-go balance.
    ZenBalance,
}

impl QuotaClass {
    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            QuotaClass::Free => "free",
            QuotaClass::GoAllowance => "go-allowance",
            QuotaClass::ZenBalance => "zen-balance",
        }
    }
}

/// Metadata for one provider/model candidate (code, not config).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelSpec {
    /// Fully-qualified `provider/model` id handed to the harness.
    pub id: &'static str,
    /// Provider id (`opencode` = Zen, `opencode-go` = Go subscription).
    pub provider: &'static str,
    /// USD per 1M input tokens.
    pub price_in: f64,
    /// USD per 1M output tokens.
    pub price_out: f64,
    /// USD per 1M cached-read tokens.
    pub price_cache_read: f64,
    /// USD per 1M cached-write tokens (0.0 when not published).
    pub price_cache_write: f64,
    /// Input context limit (tokens).
    pub context: u64,
    /// Max output tokens.
    pub max_output: u64,
    /// Structured-output advertised in the catalog.
    pub strict_schema: bool,
    /// Cash billing class.
    pub cash: CashClass,
    /// Quota bucket class.
    pub quota: QuotaClass,
    /// Short provenance note (catalog row + caveat).
    pub notes: &'static str,
}

impl ModelSpec {
    /// Whether this spec is paid (needs [`ALLOW_PAID_ENV`]).
    pub fn is_paid(&self) -> bool {
        self.cash != CashClass::Free
    }

    /// Notional token cost in USD at the table prices (cache-agnostic).
    pub fn token_cost_usd(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        (tokens_in as f64 / 1_000_000.0) * self.price_in
            + (tokens_out as f64 / 1_000_000.0) * self.price_out
    }

    /// Split a token cost into `(cash_usd, quota_usd)` by billing class.
    pub fn cash_quota(&self, tokens_in: u64, tokens_out: u64) -> (f64, f64) {
        let notional = self.token_cost_usd(tokens_in, tokens_out);
        match self.cash {
            CashClass::Free => (0.0, 0.0),
            CashClass::Subscription => (0.0, notional),
            CashClass::PayPerToken => (notional, 0.0),
        }
    }
}

/// The researched candidate table (H3). Order is presentation order; route
/// preference order lives in [`candidates`].
pub const MODEL_TABLE: &[ModelSpec] = &[
    ModelSpec {
        id: "opencode/big-pickle",
        provider: "opencode",
        price_in: 0.0,
        price_out: 0.0,
        price_cache_read: 0.0,
        price_cache_write: 0.0,
        context: 200_000,
        max_output: 32_000,
        strict_schema: true,
        cash: CashClass::Free,
        quota: QuotaClass::Free,
        notes: "free stealth coder; training-enabled; limited time",
    },
    ModelSpec {
        id: "opencode/mimo-v2.6-flash-free",
        provider: "opencode",
        price_in: 0.0,
        price_out: 0.0,
        price_cache_read: 0.0,
        price_cache_write: 0.0,
        context: 200_000,
        max_output: 32_000,
        strict_schema: false,
        cash: CashClass::Free,
        quota: QuotaClass::Free,
        notes: "free MiMo flash; training-enabled; limited time",
    },
    ModelSpec {
        id: "opencode-go/deepseek-v4.1-flash",
        provider: "opencode-go",
        price_in: 0.15,
        price_out: 0.60,
        price_cache_read: 0.003,
        price_cache_write: 0.0,
        context: 1_000_000,
        max_output: 384_000,
        strict_schema: true,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go off-peak 0.15/0.60 (peak 0.30/1.20); Vals 57.86%",
    },
    ModelSpec {
        id: "opencode-go/glm-5.3-flash",
        provider: "opencode-go",
        price_in: 0.15,
        price_out: 0.50,
        price_cache_read: 0.03,
        price_cache_write: 0.0,
        context: 1_000_000,
        max_output: 131_072,
        strict_schema: true,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go; highest cheap SWE signal (~92% Vals cover), slow at max effort",
    },
    ModelSpec {
        id: "opencode-go/qwen3.8-flash",
        provider: "opencode-go",
        price_in: 0.15,
        price_out: 0.47,
        price_cache_read: 0.016,
        price_cache_write: 0.2,
        context: 1_000_000,
        max_output: 131_072,
        strict_schema: true,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go; SWE Pro 62.5, 1M ctx, cheap cached input",
    },
    ModelSpec {
        id: "opencode-go/mimo-v2.6-flash",
        provider: "opencode-go",
        price_in: 0.14,
        price_out: 0.28,
        price_cache_read: 0.0028,
        price_cache_write: 0.0,
        context: 1_048_576,
        max_output: 131_072,
        strict_schema: false,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go; cheapest Vals top-20 (index 59.58)",
    },
    ModelSpec {
        id: "opencode-go/claude-haiku-5-5",
        provider: "opencode-go",
        price_in: 0.10,
        price_out: 0.50,
        price_cache_read: 0.01,
        price_cache_write: 0.125,
        context: 1_000_000,
        max_output: 128_000,
        strict_schema: false,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go; Vibe Code 90.44% #3/110; no structured-output advertised",
    },
    ModelSpec {
        id: "opencode-go/gpt-6-luna",
        provider: "opencode-go",
        price_in: 0.10,
        price_out: 0.50,
        price_cache_read: 0.01,
        price_cache_write: 0.125,
        context: 1_050_000,
        max_output: 128_000,
        strict_schema: true,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go; structured output; Vals index 58.45; $15/mo allowance",
    },
    ModelSpec {
        id: "opencode-go/mimo-v2.6-pro",
        provider: "opencode-go",
        price_in: 0.435,
        price_out: 0.87,
        price_cache_read: 0.003625,
        price_cache_write: 0.0,
        context: 1_048_576,
        max_output: 131_072,
        strict_schema: false,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go; reasoning planning proxy; Vals index 55.20",
    },
    ModelSpec {
        id: "opencode-go/deepseek-v4-pro",
        provider: "opencode-go",
        price_in: 0.66,
        price_out: 1.98,
        price_cache_read: 0.022,
        price_cache_write: 0.0,
        context: 1_000_000,
        max_output: 384_000,
        strict_schema: true,
        cash: CashClass::Subscription,
        quota: QuotaClass::GoAllowance,
        notes: "Go off-peak 0.66/1.98 (peak 1.32/3.96); SWE 80.6",
    },
];

/// Look up a spec by `provider/model` id.
pub fn spec(id: &str) -> Option<&'static ModelSpec> {
    MODEL_TABLE.iter().find(|s| s.id == id)
}

/// Route class derived from `(role, family)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteClass {
    /// Builder (fix/feature/bench/mutation): free-first, paid fallbacks.
    Builder,
    /// Verifier / diagnosis: strict-schema tier.
    Verifier,
    /// Planner / decision JSON: static baseline, no model.
    Planner,
}

impl RouteClass {
    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            RouteClass::Builder => "builder",
            RouteClass::Verifier => "verifier",
            RouteClass::Planner => "planner",
        }
    }
}

/// Derive the route class from the phase role and task family.
///
/// Verifier/diagnosis roles own the strict-schema tier; planner/decision roles
/// use the static baseline (no model). Every builder family (bench, feature,
/// fix, mutation) shares the v1 free-first list; the family is recorded in the
/// route decision context for later refinement.
pub fn route_class(role: &str, family: TaskFamily) -> RouteClass {
    match role {
        "verifier" | "diagnosis" => RouteClass::Verifier,
        "planner" | "decision" => RouteClass::Planner,
        _ => match family {
            TaskFamily::Bench | TaskFamily::Feature | TaskFamily::Fix | TaskFamily::Mutation => {
                RouteClass::Builder
            }
        },
    }
}

/// Candidate ids for a class, in preference order (cheapest-adequate).
pub fn candidates(class: RouteClass) -> &'static [&'static str] {
    match class {
        RouteClass::Builder => &[
            "opencode/big-pickle",
            "opencode/mimo-v2.6-flash-free",
            "opencode-go/deepseek-v4.1-flash",
            "opencode-go/glm-5.3-flash",
            "opencode-go/mimo-v2.6-flash",
        ],
        RouteClass::Verifier => &[
            "opencode-go/claude-haiku-5-5",
            "opencode-go/gpt-6-luna",
            "opencode/big-pickle",
        ],
        RouteClass::Planner => &[],
    }
}

/// Eligible candidate ids for `class`, dropping paid specs unless `allow_paid`.
///
/// Kept for the free/paid policy view (`agalma models`); the paid route also
/// requires a live credential, see [`eligible_paid`].
pub fn eligible(class: RouteClass, allow_paid: bool) -> Vec<String> {
    eligible_paid(class, allow_paid, true)
}

/// Eligible candidate ids for `class`. A paid spec is admitted only when
/// `allow_paid` **and** a provider credential is present (`paid_available`);
/// otherwise the route falls back to its free entries.
pub fn eligible_paid(class: RouteClass, allow_paid: bool, paid_available: bool) -> Vec<String> {
    candidates(class)
        .iter()
        .filter(|id| match spec(id) {
            Some(s) => !s.is_paid() || (allow_paid && paid_available),
            None => false,
        })
        .map(|id| (*id).to_string())
        .collect()
}

/// Resolve whether paid routes are enabled from [`ALLOW_PAID_ENV`].
pub fn allow_paid_from_env() -> bool {
    matches!(
        std::env::var(ALLOW_PAID_ENV).ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes")
    )
}

/// The explicit model override (`--model` flag, else `AGALMA_MODEL`), or `None`.
pub fn explicit_model(flag: Option<&str>) -> Option<String> {
    flag.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(MODEL_ENV)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

/// How the resolved model was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteOrigin {
    /// Static cheapest-adequate selection.
    Static,
    /// Explicit `--model`/`AGALMA_MODEL` override.
    Override,
    /// Planner/decision class: static baseline, no model.
    Baseline,
}

impl RouteOrigin {
    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            RouteOrigin::Static => "static",
            RouteOrigin::Override => "override",
            RouteOrigin::Baseline => "baseline",
        }
    }
}

/// A resolved route: class, family, model id, and provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteSelection {
    /// Route class selected.
    pub class: RouteClass,
    /// Task family the route was resolved for.
    pub family: TaskFamily,
    /// Resolved `provider/model` id (`static` for the planner class).
    pub model: String,
    /// How the model was chosen.
    pub origin: RouteOrigin,
    /// Whether paid candidates were eligible at resolution time.
    pub allow_paid: bool,
    /// Metadata for `model`, when it is a table row.
    pub spec: Option<ModelSpec>,
}

impl RouteSelection {
    /// Stable route label (`<class>:<family>`), recorded in usage events.
    pub fn label(&self) -> String {
        format!("{}:{}", self.class.as_str(), self.family.as_str())
    }

    /// Whether the resolved model uses a paid route.
    pub fn is_paid(&self) -> bool {
        self.spec.map(|s| s.is_paid()).unwrap_or(false)
    }

    /// `(cash_usd, quota_usd)` for the resolved model at the given tokens.
    pub fn cash_quota(&self, tokens_in: u64, tokens_out: u64) -> (f64, f64) {
        match &self.spec {
            Some(spec) => spec.cash_quota(tokens_in, tokens_out),
            None => (0.0, 0.0),
        }
    }
}

/// Resolve and durably record the route for `(scope, role, family)`.
///
/// The `route.model` decision is recorded through the ledger `decisions` table
/// (static baseline). The operation id encodes the class and the selected
/// variant (free/paid/override) so recovery reuses the same recorded outcome
/// and a changed configuration is a new decision.
#[allow(clippy::too_many_arguments)]
pub fn resolve_route(
    state_dir: &Path,
    scope: &str,
    role: &str,
    family: TaskFamily,
    allow_paid: bool,
    paid_available: bool,
    override_model: Option<&str>,
) -> Result<RouteSelection, ContractError> {
    let class = route_class(role, family);
    if class == RouteClass::Planner {
        return Ok(RouteSelection {
            class,
            family,
            model: "static".to_string(),
            origin: RouteOrigin::Baseline,
            allow_paid,
            spec: None,
        });
    }

    let override_model = override_model.map(str::trim).filter(|s| !s.is_empty());
    let candidate_ids: Vec<String> = match override_model {
        Some(model) => vec![model.to_string()],
        None => eligible_paid(class, allow_paid, paid_available),
    };
    if candidate_ids.is_empty() {
        return Err(ContractError::KnownFailure(format!(
            "no eligible route candidates for class {} (allow_paid={allow_paid}, paid_available={paid_available})",
            class.as_str()
        )));
    }

    let paid_active = allow_paid && paid_available;
    let fallback_reason = if allow_paid && !paid_available {
        Some("paid route requested but no provider credential is present; using free entries")
    } else {
        None
    };
    let variant = match override_model {
        Some(model) => format!("override-{}", short_hash(model)),
        None => {
            if paid_active {
                "paid".to_string()
            } else {
                "free".to_string()
            }
        }
    };
    let operation = OperationId::new(format!("op:route:{scope}:{}:{variant}", class.as_str()));

    let options: Vec<DecisionOption> = candidate_ids
        .iter()
        .enumerate()
        .map(|(rank, id)| DecisionOption {
            option_id: id.clone(),
            attributes: json!({ "rank": rank }),
        })
        .collect();
    let request = DecisionRequest {
        version: DECISION_API_VERSION,
        operation_id: operation,
        kind: DecisionKind::RouteModel,
        context: json!({
            "class": class.as_str(),
            "family": family.as_str(),
            "role": role,
            "allow_paid": allow_paid,
            "paid_available": paid_available,
            "paid_active": paid_active,
            "fallback_reason": fallback_reason,
            "origin": match override_model {
                Some(_) => RouteOrigin::Override.as_str(),
                None => RouteOrigin::Static.as_str(),
            },
        }),
        artifacts: vec![],
        options,
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: ROUTE_MODEL_BASELINE.to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    };

    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite"))?;
    let mut decider = StaticDecider::new(ledger);
    let outcome = decider.decide(&request, DecisionAttempt::Baseline)?;
    let chosen = match outcome {
        agalma_contracts::DecisionOutcome::Applied {
            chosen: Some(id), ..
        }
        | agalma_contracts::DecisionOutcome::Fallback {
            chosen: Some(id), ..
        } => id,
        other => {
            return Err(ContractError::KnownFailure(format!(
                "route.model produced no choice: {other:?}"
            )))
        }
    };

    let origin = if override_model.is_some() {
        RouteOrigin::Override
    } else {
        RouteOrigin::Static
    };
    Ok(RouteSelection {
        class,
        family,
        spec: spec(&chosen).copied(),
        model: chosen,
        origin,
        allow_paid,
    })
}

/// `(cash_usd, quota_usd)` for a model id at the given tokens (0 when unknown).
pub fn cash_quota(model_id: &str, tokens_in: u64, tokens_out: u64) -> (f64, f64) {
    spec(model_id)
        .map(|s| s.cash_quota(tokens_in, tokens_out))
        .unwrap_or((0.0, 0.0))
}

/// `(cash_usd, quota_usd)` for a completed turn, using the provider-reported
/// cost when available (`agalma-csw.2`).
///
/// For **paid** routes the harness reports the real per-turn dollar cost
/// (`opencode` `data.cost`); that is the honest cash figure and must be shown.
/// The table's notional token cost is the quota figure (Go allowance drain).
/// Free routes are `(0, 0)`. When a paid turn reports no cost, the table
/// notional is used as the best available cash estimate so paid turns never
/// silently read `$0`.
pub fn cash_quota_for_usage(
    model_id: &str,
    reported_cost_usd: f64,
    tokens_in: u64,
    tokens_out: u64,
) -> (f64, f64) {
    let Some(spec) = spec(model_id) else {
        return (reported_cost_usd, 0.0);
    };
    match spec.cash {
        CashClass::Free => (0.0, 0.0),
        CashClass::Subscription => {
            let notional = spec.token_cost_usd(tokens_in, tokens_out);
            let cash = if reported_cost_usd > 0.0 {
                reported_cost_usd
            } else {
                notional
            };
            (cash, notional)
        }
        CashClass::PayPerToken => {
            let notional = spec.token_cost_usd(tokens_in, tokens_out);
            let cash = if reported_cost_usd > 0.0 {
                reported_cost_usd
            } else {
                notional
            };
            (cash, 0.0)
        }
    }
}

/// Render the `agalma models` table plus the active routes and allow-paid state.
pub fn render_models() -> String {
    let allow_paid = allow_paid_from_env();
    let mut out = String::new();
    out.push_str(&format!(
        "agalma model routing (table revision {MODEL_TABLE_REVISION})\n"
    ));
    out.push_str(&format!(
        "allow_paid: {} ({ALLOW_PAID_ENV})\n\n",
        if allow_paid { "on" } else { "off (free-only)" }
    ));
    out.push_str("id\tprovider\tin/out\tcache r/w\tctx\tstrict\tcash\tquota\n");
    for spec in MODEL_TABLE {
        out.push_str(&format!(
            "{}\t{}\t{:.4}/{:.4}\t{:.4}/{:.4}\t{}\t{}\t{}\t{}\n",
            spec.id,
            spec.provider,
            spec.price_in,
            spec.price_out,
            spec.price_cache_read,
            spec.price_cache_write,
            spec.context,
            spec.strict_schema,
            spec.cash.as_str(),
            spec.quota.as_str(),
        ));
    }
    out.push_str("\nactive routes:\n");
    for class in [RouteClass::Builder, RouteClass::Verifier] {
        let eligible = eligible(class, allow_paid);
        out.push_str(&format!(
            "  {} ({}): {}\n",
            class.as_str(),
            if allow_paid {
                "paid allowed"
            } else {
                "free-only"
            },
            if eligible.is_empty() {
                "none".to_string()
            } else {
                eligible.join(" -> ")
            }
        ));
    }
    out.push_str("  planner (static): no model\n");
    out
}

/// Stable 64-bit FNV-1a hash of a string (hex).
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_all_researched_candidates() {
        for id in [
            "opencode/big-pickle",
            "opencode/mimo-v2.6-flash-free",
            "opencode-go/deepseek-v4.1-flash",
            "opencode-go/glm-5.3-flash",
            "opencode-go/qwen3.8-flash",
            "opencode-go/mimo-v2.6-flash",
            "opencode-go/claude-haiku-5-5",
            "opencode-go/gpt-6-luna",
            "opencode-go/mimo-v2.6-pro",
            "opencode-go/deepseek-v4-pro",
        ] {
            assert!(spec(id).is_some(), "missing model spec {id}");
        }
    }

    #[test]
    fn table_prices_match_the_research_snapshot() {
        let haiku = spec("opencode-go/claude-haiku-5-5").expect("haiku");
        assert_eq!((haiku.price_in, haiku.price_out), (0.10, 0.50));
        assert_eq!(haiku.context, 1_000_000);
        assert!(!haiku.strict_schema);

        let luna = spec("opencode-go/gpt-6-luna").expect("luna");
        assert_eq!((luna.price_in, luna.price_out), (0.10, 0.50));
        assert!(luna.strict_schema);

        let big = spec("opencode/big-pickle").expect("big-pickle");
        assert!(!big.is_paid());
        assert_eq!(big.context, 200_000);
    }

    #[test]
    fn route_class_maps_role_and_family() {
        assert_eq!(route_class("builder", TaskFamily::Fix), RouteClass::Builder);
        assert_eq!(
            route_class("builder", TaskFamily::Feature),
            RouteClass::Builder
        );
        assert_eq!(
            route_class("builder", TaskFamily::Bench),
            RouteClass::Builder
        );
        assert_eq!(
            route_class("builder", TaskFamily::Mutation),
            RouteClass::Builder
        );
        assert_eq!(
            route_class("verifier", TaskFamily::Fix),
            RouteClass::Verifier
        );
        assert_eq!(
            route_class("planner", TaskFamily::Feature),
            RouteClass::Planner
        );
    }

    #[test]
    fn paid_candidates_are_skipped_without_opt_in() {
        let free = eligible(RouteClass::Builder, false);
        assert_eq!(
            free,
            vec![
                "opencode/big-pickle".to_string(),
                "opencode/mimo-v2.6-flash-free".to_string()
            ],
            "only free builder candidates without opt-in"
        );
        let paid = eligible(RouteClass::Builder, true);
        assert!(paid.contains(&"opencode-go/deepseek-v4.1-flash".to_string()));

        // Verifier free fallback is the same free coder; paid tier is skipped.
        assert_eq!(
            eligible(RouteClass::Verifier, false),
            vec!["opencode/big-pickle".to_string()]
        );
        assert_eq!(
            eligible(RouteClass::Verifier, true)
                .first()
                .map(String::as_str),
            Some("opencode-go/claude-haiku-5-5")
        );
    }

    #[test]
    fn paid_candidates_require_both_opt_in_and_a_credential() {
        // Opt-in without a token keeps the free entries only.
        assert_eq!(
            eligible_paid(RouteClass::Builder, true, false),
            vec![
                "opencode/big-pickle".to_string(),
                "opencode/mimo-v2.6-flash-free".to_string()
            ],
            "no token -> free fallback even with AGALMA_ALLOW_PAID"
        );
        // Both present: the paid tier leads the verifier list.
        assert_eq!(
            eligible_paid(RouteClass::Verifier, true, true)
                .first()
                .map(String::as_str),
            Some("opencode-go/claude-haiku-5-5")
        );
    }

    #[test]
    fn cash_quota_for_usage_surfaces_reported_paid_cost() {
        // Free route: never any cash.
        assert_eq!(
            cash_quota_for_usage("opencode/big-pickle", 0.0, 1_000_000, 1_000_000),
            (0.0, 0.0)
        );
        // Paid route with a provider-reported cost: cash is the real figure,
        // quota is the notional Go allowance drain.
        let (cash, quota) =
            cash_quota_for_usage("opencode-go/deepseek-v4.1-flash", 0.0042, 1_000, 1_000);
        assert!((cash - 0.0042).abs() < 1e-12, "cash {cash}");
        assert!(quota > 0.0, "quota notional {quota}");
        // Paid route with no reported cost falls back to the table notional.
        let (cash, _) = cash_quota_for_usage("opencode-go/deepseek-v4.1-flash", 0.0, 1_000_000, 0);
        assert!((cash - 0.15).abs() < 1e-9, "cash {cash}");
    }

    #[test]
    fn cash_quota_splits_free_subscription_and_pay_per_token() {
        let big = spec("opencode/big-pickle").expect("big-pickle");
        assert_eq!(big.cash_quota(1_000_000, 1_000_000), (0.0, 0.0));

        let haiku = spec("opencode-go/claude-haiku-5-5").expect("haiku");
        let (cash, quota) = haiku.cash_quota(1_000_000, 1_000_000);
        assert_eq!(cash, 0.0, "subscription cash is $0");
        assert!((quota - 0.60).abs() < 1e-9, "quota notional {quota}");

        let pay = ModelSpec {
            id: "test/pay",
            provider: "test",
            price_in: 1.0,
            price_out: 2.0,
            price_cache_read: 0.0,
            price_cache_write: 0.0,
            context: 1,
            max_output: 1,
            strict_schema: true,
            cash: CashClass::PayPerToken,
            quota: QuotaClass::ZenBalance,
            notes: "test",
        };
        let (cash, quota) = pay.cash_quota(1_000_000, 1_000_000);
        assert!((cash - 3.0).abs() < 1e-9);
        assert_eq!(quota, 0.0);
    }
}
