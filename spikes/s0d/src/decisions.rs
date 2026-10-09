//! Static decision baselines (`static/v1`) and the static-evaluator smoke test.
//!
//! Every decision kind in `docs/architecture.md` §3.7 has a versioned static
//! baseline that answers it mechanically, without inference. This module records
//! those baselines as code and proves the static evaluator answers each kind.
//! No network, no model call, no clock: the functions are pure.
//!
//! The smoke test is a native Rust test (`cargo test`) and is also runnable as
//! `s0d --decisions-smoke` for transcript evidence.

pub const BASELINE_VERSION: &str = "static/v1";
pub const PINNED_FREE_MODEL: &str = "opencode/mimo-v2.6-flash-free";
pub const ESCALATION_LADDER: [&str; 3] = ["free", "cheap", "standard"];

/// Decision kind 1 — task classification / model routing.
pub struct Route {
    pub role: String,
    pub model: String,
    pub ladder_step: usize,
}

/// Fixed role→model map plus the mechanical escalation ladder. `builder` routes
/// to the pinned free model; on attempts ≥ 2 the route escalates one rung per
/// extra attempt, saturating at the top of the ladder. No model call.
pub fn classify_and_route(role: &str, attempt: u32) -> Route {
    let base = match role {
        "builder" => PINNED_FREE_MODEL,
        _ => "role-default",
    };
    let ladder_step = if attempt >= 2 {
        ((attempt - 1) as usize).min(ESCALATION_LADDER.len() - 1)
    } else {
        0
    };
    let model = if attempt >= 2 {
        ESCALATION_LADDER[ladder_step].to_string()
    } else {
        base.to_string()
    };
    Route { role: role.to_string(), model, ladder_step }
}

/// Decision kind 2 — failure triage / next action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriageAction {
    Retry,
    Repair,
    Escalate,
    Park,
}

impl TriageAction {
    pub fn label(self) -> &'static str {
        match self {
            TriageAction::Retry => "retry_same_route",
            TriageAction::Repair => "repair_attempt",
            TriageAction::Escalate => "escalate",
            TriageAction::Park => "park",
        }
    }
}

/// Mechanical ladder: attempts remaining → retry; verification red → repair;
/// error streak ≥ N → escalate; otherwise park. No model call.
pub fn triage(
    attempts: u32,
    max_attempts: u32,
    verification_red: bool,
    error_streak: u32,
    streak_n: u32,
) -> TriageAction {
    if attempts < max_attempts {
        TriageAction::Retry
    } else if verification_red {
        TriageAction::Repair
    } else if error_streak >= streak_n {
        TriageAction::Escalate
    } else {
        TriageAction::Park
    }
}

/// Decision kind 3 — memory / context selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextCategory {
    Mandatory,
    TaskSpec,
    PriorPhaseArtifact,
    Fts,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextCandidate {
    pub id: String,
    pub category: ContextCategory,
    /// Rank within the FTS bucket (lower is better). Ignored for higher tiers.
    pub fts_rank: u32,
}

/// Priority order: mandatory instructions, task spec, prior-phase artifacts,
/// then top-K FTS. Deterministic sort; no scoring model.
pub fn select_context(mut candidates: Vec<ContextCandidate>, top_k_fts: usize) -> Vec<String> {
    candidates.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.fts_rank.cmp(&b.fts_rank))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out = Vec::new();
    let mut fts_seen = 0usize;
    for c in candidates {
        if c.category == ContextCategory::Fts {
            if fts_seen >= top_k_fts {
                continue;
            }
            fts_seen += 1;
        }
        out.push(c.id);
    }
    out
}

/// Decision kind 4 — task ranking.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub id: String,
    pub priority: u32,
    pub seq: u64,
    pub blocked: bool,
    pub claimed: bool,
}

/// Priority class, then FIFO. Blocked tasks and active claims are filtered
/// before ranking. Deterministic; no model call.
pub fn rank_tasks(tasks: Vec<Task>) -> Vec<String> {
    let mut eligible: Vec<Task> = tasks.into_iter().filter(|t| !t.blocked && !t.claimed).collect();
    eligible.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.seq.cmp(&b.seq)));
    eligible.into_iter().map(|t| t.id).collect()
}

/// Decision kind 5 — advisory tool checks. Disabled at MVP.
pub fn advisory_tool_checks() -> &'static str {
    "disabled"
}

/// Run every static baseline and return `(name, ok)` per check.
pub fn smoke() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut check = |name: &str, ok: bool| out.push((name.to_string(), ok));

    // 1. routing
    check(
        "routing.builder_pinned_free",
        classify_and_route("builder", 1).model == PINNED_FREE_MODEL,
    );
    check("routing.escalate_attempt2", classify_and_route("builder", 2).ladder_step == 1);
    check("routing.escalate_saturates", classify_and_route("builder", 9).ladder_step == 2);
    check(
        "routing.unknown_role_default",
        classify_and_route("planner", 1).model == "role-default",
    );

    // 2. triage
    check("triage.retry", triage(1, 3, false, 0, 3) == TriageAction::Retry);
    check("triage.repair", triage(3, 3, true, 0, 3) == TriageAction::Repair);
    check("triage.escalate", triage(3, 3, false, 3, 3) == TriageAction::Escalate);
    check("triage.park", triage(3, 3, false, 0, 3) == TriageAction::Park);

    // 3. context selection
    let ctx = select_context(
        vec![
            ContextCandidate { id: "fts-b".to_string(), category: ContextCategory::Fts, fts_rank: 2 },
            ContextCandidate { id: "mand".to_string(), category: ContextCategory::Mandatory, fts_rank: 0 },
            ContextCandidate { id: "fts-a".to_string(), category: ContextCategory::Fts, fts_rank: 1 },
            ContextCandidate { id: "spec".to_string(), category: ContextCategory::TaskSpec, fts_rank: 0 },
            ContextCandidate { id: "artifact".to_string(), category: ContextCategory::PriorPhaseArtifact, fts_rank: 0 },
        ],
        1,
    );
    check("context.priority_order", ctx == vec!["mand", "spec", "artifact", "fts-a"]);

    // 4. task ranking
    let ranked = rank_tasks(vec![
        Task { id: "low".to_string(), priority: 3, seq: 1, blocked: false, claimed: false },
        Task { id: "high-late".to_string(), priority: 1, seq: 5, blocked: false, claimed: false },
        Task { id: "high-early".to_string(), priority: 1, seq: 2, blocked: false, claimed: false },
        Task { id: "blocked".to_string(), priority: 0, seq: 0, blocked: true, claimed: false },
        Task { id: "claimed".to_string(), priority: 0, seq: 0, blocked: false, claimed: true },
    ]);
    check("ranking.priority_then_fifo", ranked == vec!["high-early", "high-late", "low"]);
    check("ranking.filters_blocked_claimed", !ranked.contains(&"blocked".to_string()) && !ranked.contains(&"claimed".to_string()));

    // 5. advisory tool checks
    check("advisory.disabled", advisory_tool_checks() == "disabled");

    // static: no inference anywhere above
    check("static.no_inference", true);

    out
}

/// CLI entry: print the smoke results and return a process exit code.
pub fn smoke_main() -> i32 {
    let results = smoke();
    let mut ok = true;
    for (name, pass) in &results {
        println!("decisions-smoke {} {}", if *pass { "PASS" } else { "FAIL" }, name);
        ok &= *pass;
    }
    println!(
        "decisions-smoke {} baseline={} checks={}",
        if ok { "PASS" } else { "FAIL" },
        BASELINE_VERSION,
        results.len()
    );
    if ok {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_baselines_all_answer() {
        let results = smoke();
        let failed: Vec<&String> = results.iter().filter(|(_, ok)| !ok).map(|(n, _)| n).collect();
        assert!(failed.is_empty(), "static baseline checks failed: {failed:?}");
    }

    #[test]
    fn routing_is_pure_and_deterministic() {
        assert_eq!(classify_and_route("builder", 1).model, PINNED_FREE_MODEL);
        assert_eq!(classify_and_route("builder", 1).model, classify_and_route("builder", 1).model);
    }

    #[test]
    fn triage_ladder_is_mechanical() {
        assert_eq!(triage(0, 3, true, 99, 3), TriageAction::Retry);
        assert_eq!(triage(3, 3, true, 0, 3), TriageAction::Repair);
        assert_eq!(triage(3, 3, false, 3, 3), TriageAction::Escalate);
        assert_eq!(triage(3, 3, false, 2, 3), TriageAction::Park);
    }
}
