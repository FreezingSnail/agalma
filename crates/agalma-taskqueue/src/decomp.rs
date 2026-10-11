//! Decomposition schema + validator (CP.1, `agalma-i5h.2`).
//!
//! Pure functions over bd child records. Each rule emits [`Violation`]s carrying
//! a stable rule name (design: `docs/planning-pipeline.md`, "Validator rules").
//! The validator only judges: it never edits tickets.
//!
//! Inputs are [`ChildTask`] records built from `bd list --parent <epic> --all`
//! (see [`crate::BdTaskQueue::plan_children`]) so deferred/staged children are
//! included. The `directive/v0` intake fence is parsed with the existing
//! [`crate::block::parse_task_block`] — the YAML grammar is never duplicated.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::block::parse_task_block;

/// Fixed child-description headings, in required order.
pub const HEADINGS: [&str; 6] = [
    "# Scope",
    "# Files",
    "# Contract",
    "# Invariants",
    "# Non-goals",
    "# MACHINE",
];

/// Default minimum children per decomposition.
pub const MIN_CHILDREN: usize = 8;
/// Maximum children per decomposition.
pub const MAX_CHILDREN: usize = 12;
/// Prose word budget for a child description (Contract/MACHINE excluded).
pub const MAX_PROSE_WORDS: usize = 400;
/// Word budget for a child's acceptance commands.
pub const MAX_ACCEPTANCE_WORDS: usize = 80;
/// Design word budget for a low-tier child.
pub const MAX_LOW_DESIGN_WORDS: usize = 150;

/// Rule names (stable identifiers carried on every [`Violation`]).
pub const RULE_COUNT_BUDGET: &str = "count-budget";
pub const RULE_SCHEMA_SECTION: &str = "schema-section";
pub const RULE_CAP_PROSE: &str = "cap-prose";
pub const RULE_CAP_ACCEPTANCE: &str = "cap-acceptance";
pub const RULE_CAP_DESIGN: &str = "cap-design";
pub const RULE_TIER_MISMATCH: &str = "tier-mismatch";
pub const RULE_INTERIM_VALUE: &str = "interim-value";
pub const RULE_UNPROVIDED_SYMBOL: &str = "unprovided-symbol";
pub const RULE_GRAPH_CYCLE: &str = "graph-cycle";
pub const RULE_GRAPH_WIDTH: &str = "graph-width";
pub const RULE_INTAKE_BLOCK: &str = "intake-block";

/// Phrases that betray interim/placeholder work (design rule `interim-value`).
const INTERIM_PHRASES: [&str; 4] = [
    "until then",
    "placeholder",
    "for now pass nil",
    "temporary stub",
];

/// Difficulty markers: any substring here forces the `high` tier. These are a
/// structural heuristic standing in for the judgment parts of the tier rule
/// (async/process lifecycle, external-format parsing, data migration, and
/// performance constraints). Matching is case-insensitive over the whole
/// description.
const DIFFICULTY_MARKERS: [&str; 22] = [
    "async",
    "await",
    "tokio",
    "future",
    "subprocess",
    "process",
    "spawn",
    "daemon",
    "signal",
    "lifecycle",
    "parse",
    "parser",
    "deserialize",
    "yaml",
    "json",
    "toml",
    "csv",
    "migrat",
    "backfill",
    "performance",
    "latency",
    "throughput",
];

/// A child ticket as the validator sees it: the shape bd exposes for a
/// decomposition child. `depends_on` carries the child ids this child is blocked
/// by (bd `blocks` edges), never the parent edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildTask {
    pub id: String,
    pub title: String,
    pub description: String,
    pub design: Option<String>,
    pub labels: Vec<String>,
    /// Ids of sibling children this child depends on (bd `blocks` edges).
    pub depends_on: Vec<String>,
}

/// One rule failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Stable rule name ([`RULE_COUNT_BUDGET`], ...).
    pub rule: &'static str,
    /// Human-readable detail (names the offending child when applicable).
    pub detail: String,
}

impl Violation {
    fn new(rule: &'static str, detail: impl Into<String>) -> Self {
        Violation {
            rule,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.rule, self.detail)
    }
}

/// Validator parameters. `allow_below_min` is the human verdict bit for
/// `count-budget` (below the minimum only fails when it is `false`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidateParams {
    pub allow_below_min: bool,
    pub min_children: usize,
    pub min_width: usize,
}

impl Default for ValidateParams {
    fn default() -> Self {
        ValidateParams {
            allow_below_min: false,
            min_children: MIN_CHILDREN,
            min_width: 1,
        }
    }
}

/// Derived difficulty tier (never judged; see [`derive_tier`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Low,
    High,
}

impl Tier {
    /// The label/tier token (`low`/`high`).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::High => "high",
        }
    }
}

/// Validate a whole decomposition, returning every violation in rule order
/// (count budget, graph, then per-child rules in child order).
pub fn validate(children: &[ChildTask], params: &ValidateParams) -> Vec<Violation> {
    let mut violations = Vec::new();
    check_count_budget(children, params, &mut violations);
    check_graph(children, params, &mut violations);
    for child in children {
        check_schema_section(child, &mut violations);
        check_cap_prose(child, &mut violations);
        check_cap_acceptance(child, &mut violations);
        check_cap_design(child, &mut violations);
        check_tier_mismatch(child, &mut violations);
        check_interim_value(child, &mut violations);
        check_unprovided_symbol(child, children, &mut violations);
        check_intake_block(child, &mut violations);
    }
    violations
}

/// True when any comment carries an explicit human verdict marker. The CLI reads
/// the epic's comments and folds this into [`ValidateParams::allow_below_min`].
pub fn has_human_verdict(comments: &[String]) -> bool {
    comments.iter().any(|comment| {
        let lower = comment.to_lowercase();
        lower.contains("verdict:") || lower.contains("decomp-verdict")
    })
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

fn check_count_budget(children: &[ChildTask], params: &ValidateParams, out: &mut Vec<Violation>) {
    let count = children.len();
    if count > MAX_CHILDREN {
        out.push(Violation::new(
            RULE_COUNT_BUDGET,
            format!("{count} children exceeds maximum {MAX_CHILDREN}"),
        ));
    } else if count < params.min_children && !params.allow_below_min {
        out.push(Violation::new(
            RULE_COUNT_BUDGET,
            format!(
                "{count} children below minimum {} (no human verdict comment)",
                params.min_children
            ),
        ));
    }
}

fn check_schema_section(child: &ChildTask, out: &mut Vec<Violation>) {
    let found: Vec<&str> = child
        .description
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            HEADINGS.iter().copied().find(|h| *h == trimmed)
        })
        .collect();
    if found != HEADINGS.to_vec() {
        out.push(Violation::new(
            RULE_SCHEMA_SECTION,
            format!(
                "child {}: expected headings {:?} in order, found {:?}",
                child.id, HEADINGS, found
            ),
        ));
    }
}

fn check_cap_prose(child: &ChildTask, out: &mut Vec<Violation>) {
    let words = prose_word_count(&child.description);
    if words > MAX_PROSE_WORDS {
        out.push(Violation::new(
            RULE_CAP_PROSE,
            format!(
                "child {}: description prose {words} words exceeds {MAX_PROSE_WORDS}",
                child.id
            ),
        ));
    }
}

fn check_cap_acceptance(child: &ChildTask, out: &mut Vec<Violation>) {
    let Ok(Some(block)) = parse_task_block(&child.description) else {
        return; // intake-block reports the parse failure.
    };
    // Each acceptance command must be a single plain-ASCII line.
    if let Some(bad) = block
        .acceptance
        .iter()
        .find(|cmd| !cmd.is_ascii() || cmd.contains('\n') || cmd.contains('\r'))
    {
        out.push(Violation::new(
            RULE_CAP_ACCEPTANCE,
            format!(
                "child {}: acceptance {bad:?} is not a single plain-ASCII line",
                child.id
            ),
        ));
    }
    let words: usize = block
        .acceptance
        .iter()
        .map(|cmd| cmd.split_whitespace().count())
        .sum();
    if words > MAX_ACCEPTANCE_WORDS {
        out.push(Violation::new(
            RULE_CAP_ACCEPTANCE,
            format!(
                "child {}: acceptance {words} words exceeds {MAX_ACCEPTANCE_WORDS}",
                child.id
            ),
        ));
    }
}

fn check_cap_design(child: &ChildTask, out: &mut Vec<Violation>) {
    if derive_tier(child) != Tier::Low {
        return;
    }
    let words = child
        .design
        .as_deref()
        .map(|design| design.split_whitespace().count())
        .unwrap_or(0);
    if words > MAX_LOW_DESIGN_WORDS {
        out.push(Violation::new(
            RULE_CAP_DESIGN,
            format!(
                "child {}: low-tier design {words} words exceeds {MAX_LOW_DESIGN_WORDS}",
                child.id
            ),
        ));
    }
}

fn check_tier_mismatch(child: &ChildTask, out: &mut Vec<Violation>) {
    let declared: Vec<&str> = child
        .labels
        .iter()
        .filter(|label| label.starts_with("difficulty:"))
        .map(String::as_str)
        .collect();
    if declared.len() != 1 {
        out.push(Violation::new(
            RULE_TIER_MISMATCH,
            format!(
                "child {}: expected exactly one difficulty:* label, found {declared:?}",
                child.id
            ),
        ));
        return;
    }
    let derived = derive_tier(child);
    let expected = format!("difficulty:{}", derived.as_str());
    if declared[0] != expected {
        out.push(Violation::new(
            RULE_TIER_MISMATCH,
            format!(
                "child {}: label {} != derived tier {}",
                child.id, declared[0], expected
            ),
        ));
    }
}

fn check_interim_value(child: &ChildTask, out: &mut Vec<Violation>) {
    let mut haystack = child.description.to_lowercase();
    if let Some(design) = &child.design {
        haystack.push('\n');
        haystack.push_str(&design.to_lowercase());
    }
    for phrase in INTERIM_PHRASES {
        if haystack.contains(phrase) {
            out.push(Violation::new(
                RULE_INTERIM_VALUE,
                format!("child {}: interim phrasing {phrase:?}", child.id),
            ));
        }
    }
}

fn check_unprovided_symbol(child: &ChildTask, all: &[ChildTask], out: &mut Vec<Violation>) {
    let machine = parse_machine(&child.description);
    if machine.consumes.is_empty() {
        return;
    }
    // Symbols this child can see: its own provides plus every transitive
    // dependency's provides (following bd `blocks` edges within the set).
    let mut reachable: Vec<String> = machine.provides.clone();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = child.depends_on.clone();
    while let Some(dep) = stack.pop() {
        if !seen.insert(dep.clone()) {
            continue;
        }
        if let Some(dep_child) = all.iter().find(|c| c.id == dep) {
            reachable.extend(parse_machine(&dep_child.description).provides);
            stack.extend(dep_child.depends_on.clone());
        }
    }
    for symbol in &machine.consumes {
        if !reachable.iter().any(|provided| provided == symbol) {
            out.push(Violation::new(
                RULE_UNPROVIDED_SYMBOL,
                format!(
                    "child {}: consumes {symbol:?} is not provided by itself or a transitive dependency",
                    child.id
                ),
            ));
        }
    }
}

fn check_graph(children: &[ChildTask], params: &ValidateParams, out: &mut Vec<Violation>) {
    if let Some(cycle) = find_cycle(children) {
        out.push(Violation::new(
            RULE_GRAPH_CYCLE,
            format!("dependency cycle: {}", cycle.join(" -> ")),
        ));
        return; // width is undefined under a cycle
    }
    let widths = wave_widths(children);
    let median = median_width(&widths);
    if median < params.min_width as f64 {
        out.push(Violation::new(
            RULE_GRAPH_WIDTH,
            format!(
                "median dependency-wave width {median} below minimum {} (waves {:?})",
                params.min_width, widths
            ),
        ));
    }
}

fn check_intake_block(child: &ChildTask, out: &mut Vec<Violation>) {
    match parse_task_block(&child.description) {
        Ok(Some(block)) => {
            if block.directive != "directive/v0" {
                out.push(Violation::new(
                    RULE_INTAKE_BLOCK,
                    format!(
                        "child {}: directive {:?} != directive/v0",
                        child.id, block.directive
                    ),
                ));
            }
            if block.acceptance.is_empty() {
                out.push(Violation::new(
                    RULE_INTAKE_BLOCK,
                    format!("child {}: acceptance list is empty", child.id),
                ));
            }
            if block.budget_usd.is_none() {
                out.push(Violation::new(
                    RULE_INTAKE_BLOCK,
                    format!("child {}: budget_usd is missing", child.id),
                ));
            }
        }
        Ok(None) => out.push(Violation::new(
            RULE_INTAKE_BLOCK,
            format!("child {}: no directive/v0 YAML fence", child.id),
        )),
        Err(err) => out.push(Violation::new(
            RULE_INTAKE_BLOCK,
            format!("child {}: {err}", child.id),
        )),
    }
}

// ---------------------------------------------------------------------------
// Tier derivation
// ---------------------------------------------------------------------------

/// Derive the difficulty tier from structure. `low` iff exactly one
/// implementation file is listed in MACHINE `files:`, `provides:` is empty, and
/// the description carries no difficulty marker.
///
/// Judgment parts of the design rule (fully-specified contract, ≲100 changed
/// lines) are not machine-checkable and are deliberately not modeled; the marker
/// scan stands in for "async/process lifecycle, external-format parsing, data
/// migration, performance constraint".
pub fn derive_tier(child: &ChildTask) -> Tier {
    let machine = parse_machine(&child.description);
    let low = machine.files.len() == 1
        && machine.provides.is_empty()
        && !has_difficulty_marker(&child.description);
    if low {
        Tier::Low
    } else {
        Tier::High
    }
}

fn has_difficulty_marker(description: &str) -> bool {
    let lower = strip_yaml_fence(description).to_lowercase();
    DIFFICULTY_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Drop the ```` ```yaml ```` intake fence body so its language tag (`yaml`) or
/// scalar values never masquerade as structural difficulty markers.
fn strip_yaml_fence(description: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in description.lines() {
        let trimmed = line.trim();
        if !in_fence && trimmed == "```yaml" {
            in_fence = true;
            continue;
        }
        if in_fence {
            if trimmed == "```" {
                in_fence = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Description parsing helpers
// ---------------------------------------------------------------------------

/// The MACHINE section keys the validator reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Machine {
    provides: Vec<String>,
    consumes: Vec<String>,
    files: Vec<String>,
    /// Parsed for completeness (the design lists `tier:` as a MACHINE key); the
    /// `tier-mismatch` rule judges the `difficulty:*` label, not this token.
    #[allow(dead_code)]
    tier: Option<String>,
}

/// Parse the `# MACHINE` section: one `key: value` per line, values a
/// comma/whitespace list (`[]` and blanks are empty).
fn parse_machine(description: &str) -> Machine {
    let mut machine = Machine::default();
    let mut in_machine = false;
    for line in description.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            in_machine = trimmed == "# MACHINE";
            continue;
        }
        if !in_machine {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim() {
            "provides" => machine.provides = parse_list(value),
            "consumes" => machine.consumes = parse_list(value),
            "files" => machine.files = parse_list(value),
            "tier" => machine.tier = Some(value.trim().to_string()),
            _ => {}
        }
    }
    machine
}

/// Split a MACHINE list value: strip an optional `[...]` wrapper, then split on
/// commas/whitespace and drop empties and one layer of quotes.
fn parse_list(value: &str) -> Vec<String> {
    let inner = value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|item| item.trim().trim_matches('"').trim_matches('\''))
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Word count of the description prose, excluding the `# Contract` and
/// `# MACHINE` sections (and their heading lines).
fn prose_word_count(description: &str) -> usize {
    let mut excluded = false;
    let mut words = 0;
    for line in description.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            excluded = trimmed == "# Contract" || trimmed == "# MACHINE";
            if !excluded {
                words += trimmed.split_whitespace().count();
            }
            continue;
        }
        if excluded {
            continue;
        }
        words += line.split_whitespace().count();
    }
    words
}

// ---------------------------------------------------------------------------
// Graph helpers (deps restricted to ids inside the decomposition)
// ---------------------------------------------------------------------------

fn dep_adjacency<'a>(children: &'a [ChildTask]) -> HashMap<&'a str, Vec<&'a str>> {
    let ids: HashSet<&str> = children.iter().map(|c| c.id.as_str()).collect();
    children
        .iter()
        .map(|child| {
            let deps = child
                .depends_on
                .iter()
                .filter(|dep| ids.contains(dep.as_str()))
                .map(String::as_str)
                .collect();
            (child.id.as_str(), deps)
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Color {
    White,
    Gray,
    Black,
}

/// Return one dependency cycle as a path (`a -> b -> a`), if any.
fn find_cycle(children: &[ChildTask]) -> Option<Vec<String>> {
    let deps = dep_adjacency(children);
    let mut color: HashMap<&str, Color> =
        children.iter().map(|c| (c.id.as_str(), Color::White)).collect();
    for child in children {
        if color[child.id.as_str()] == Color::White {
            let mut stack = Vec::new();
            if let Some(cycle) = dfs_cycle(child.id.as_str(), &deps, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

fn dfs_cycle<'a>(
    node: &'a str,
    deps: &HashMap<&'a str, Vec<&'a str>>,
    color: &mut HashMap<&'a str, Color>,
    stack: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    color.insert(node, Color::Gray);
    stack.push(node);
    for dep in deps.get(node).map(Vec::as_slice).unwrap_or(&[]) {
        match color.get(*dep).copied() {
            Some(Color::Gray) => {
                let start = stack.iter().position(|s| s == dep).unwrap_or(0);
                let mut cycle: Vec<String> =
                    stack[start..].iter().map(|s| s.to_string()).collect();
                cycle.push((*dep).to_string());
                return Some(cycle);
            }
            Some(Color::White) => {
                if let Some(cycle) = dfs_cycle(dep, deps, color, stack) {
                    return Some(cycle);
                }
            }
            _ => {}
        }
    }
    stack.pop();
    color.insert(node, Color::Black);
    None
}

/// Dependency-wave widths: wave 0 has no (in-set) deps; a node sits one past its
/// deepest dependency. `widths[i]` is the number of nodes at wave `i`.
fn wave_widths(children: &[ChildTask]) -> Vec<usize> {
    let deps = dep_adjacency(children);
    let mut memo: HashMap<&str, usize> = HashMap::new();
    let mut widths: Vec<usize> = Vec::new();
    for child in children {
        let wave = node_wave(child.id.as_str(), &deps, &mut memo);
        if widths.len() <= wave {
            widths.resize(wave + 1, 0);
        }
        widths[wave] += 1;
    }
    widths
}

fn node_wave<'a>(
    node: &'a str,
    deps: &HashMap<&'a str, Vec<&'a str>>,
    memo: &mut HashMap<&'a str, usize>,
) -> usize {
    if let Some(&wave) = memo.get(node) {
        return wave;
    }
    let wave = deps
        .get(node)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|dep| node_wave(dep, deps, memo))
        .max()
        .map(|deepest| deepest + 1)
        .unwrap_or(0);
    memo.insert(node, wave);
    wave
}

/// Median of a set of wave widths (average of the two middle values when even;
/// `0.0` for an empty set).
fn median_width(widths: &[usize]) -> f64 {
    if widths.is_empty() {
        return 0.0;
    }
    let mut sorted = widths.to_vec();
    sorted.sort_unstable();
    let len = sorted.len();
    if len % 2 == 1 {
        sorted[len / 2] as f64
    } else {
        (sorted[len / 2 - 1] + sorted[len / 2]) as f64 / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child with no dependencies; callers override fields per rule.
    fn child(id: &str, description: &str) -> ChildTask {
        ChildTask {
            id: id.to_string(),
            title: id.to_string(),
            description: description.to_string(),
            design: None,
            labels: vec!["staged".to_string(), "difficulty:low".to_string()],
            depends_on: Vec::new(),
        }
    }

    /// A description that satisfies every per-child rule at low tier. The YAML
    /// fence lives under `# MACHINE`, so it is excluded from prose and never
    /// leaks into the marker scan as extra structure.
    fn low_child(id: &str) -> ChildTask {
        let description = format!(
            "# Scope\nDo exactly one small thing for {id}.\n\
             \n# Files\ncrates/foo/src/{id}.rs\n\
             \n# Contract\nRun the acceptance command.\n\
             \n# Invariants\nNo behavior change outside scope.\n\
             \n# Non-goals\nNo refactor.\n\
             \n# MACHINE\nprovides:\nconsumes:\nfiles: crates/foo/src/{id}.rs\ntier: low\n\
             \n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - cargo test -p foo\nbudget_usd: 2.00\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n"
        );
        let mut c = child(id, &description);
        c.design = Some("Small, single-file change; no new interface.".to_string());
        c
    }

    fn rules(violations: &[Violation]) -> Vec<&'static str> {
        violations.iter().map(|v| v.rule).collect()
    }

    #[test]
    fn a_clean_low_child_has_no_violations() {
        let children = vec![low_child("a")];
        let params = ValidateParams {
            allow_below_min: true,
            ..ValidateParams::default()
        };
        let violations = validate(&children, &params);
        assert!(violations.is_empty(), "unexpected: {violations:?}");
    }

    #[test]
    fn count_budget_table() {
        // Above the maximum always fails.
        let children: Vec<ChildTask> = (0..13).map(|i| low_child(&format!("c{i}"))).collect();
        assert_eq!(
            rules(&validate(&children, &ValidateParams::default())),
            vec![RULE_COUNT_BUDGET]
        );

        // Below the minimum fails without a verdict...
        let children: Vec<ChildTask> = (0..7).map(|i| low_child(&format!("c{i}"))).collect();
        assert_eq!(
            rules(&validate(&children, &ValidateParams::default())),
            vec![RULE_COUNT_BUDGET]
        );

        // ...and passes with one.
        let params = ValidateParams {
            allow_below_min: true,
            min_children: MIN_CHILDREN,
            min_width: 1,
        };
        assert!(validate(&children, &params).is_empty());

        // A raised minimum is honored.
        let children: Vec<ChildTask> = (0..9).map(|i| low_child(&format!("c{i}"))).collect();
        let params = ValidateParams {
            min_children: 10,
            ..ValidateParams::default()
        };
        assert_eq!(rules(&validate(&children, &params)), vec![RULE_COUNT_BUDGET]);
    }

    #[test]
    fn schema_section_reports_missing_and_misordered_headings() {
        let missing = child("c", "# Scope\nx\n# Files\ny\n# MACHINE\nfiles: a\n");
        assert_eq!(
            rules(&validate(&[missing], &ValidateParams::default())),
            vec![RULE_COUNT_BUDGET, RULE_SCHEMA_SECTION, RULE_INTAKE_BLOCK]
        );

        let mut reordered = low_child("c");
        reordered.description = reordered
            .description
            .replace("# Files", "# Contract")
            .replace("# Contract", "# Files");
        assert!(rules(&validate(&[reordered], &ValidateParams::default()))
            .contains(&RULE_SCHEMA_SECTION));
    }

    #[test]
    fn cap_prose_excludes_contract_and_machine() {
        // A short Scope passes...
        let mut under = low_child("c");
        under.description = format!(
            "# Scope\n{}\n# Files\na.rs\n# Contract\nw w w\n# Invariants\nx\n# Non-goals\nx\n# MACHINE\nfiles: a.rs\n",
            "word ".repeat(100)
        );
        assert!(!rules(&validate(&[under], &ValidateParams::default())).contains(&RULE_CAP_PROSE));

        // ...an oversized Scope does not. The Contract/MACHINE body is large but
        // excluded, proving the exclusion.
        let mut over = low_child("c");
        over.description = format!(
            "# Scope\n{}\n# Files\na.rs\n# Contract\n{}\n# Invariants\nx\n# Non-goals\nx\n# MACHINE\nfiles: a.rs\n",
            "word ".repeat(500),
            "excluded ".repeat(1000)
        );
        assert!(rules(&validate(&[over], &ValidateParams::default())).contains(&RULE_CAP_PROSE));
    }

    #[test]
    fn cap_prose_ignores_the_yaml_fence_under_machine() {
        // The full low_child description (fence included) is well under 400
        // prose words because MACHINE is excluded.
        let c = low_child("c");
        assert!(prose_word_count(&c.description) < MAX_PROSE_WORDS);
    }

    #[test]
    fn cap_acceptance_rejects_too_many_words_and_non_ascii() {
        let long = "compile ".repeat(80).trim().to_string(); // 80 words
        let mut over = low_child("c");
        over.description = over.description.replace(
            "  - cargo test -p foo",
            &format!("  - cargo test -p foo\n  - {long} extra"),
        );
        assert!(rules(&validate(&[over], &ValidateParams::default()))
            .contains(&RULE_CAP_ACCEPTANCE));

        let mut non_ascii = low_child("c");
        non_ascii
            .description
            .push_str("# note\n");
        non_ascii.description = non_ascii
            .description
            .replace("cargo test -p foo", "cargo test \u{2013}p foo");
        assert!(rules(&validate(&[non_ascii], &ValidateParams::default()))
            .contains(&RULE_CAP_ACCEPTANCE));
    }

    #[test]
    fn cap_design_applies_only_to_low_tier() {
        let long_design = "word ".repeat(151);
        let mut low = low_child("c");
        low.design = Some(long_design.clone());
        assert!(rules(&validate(&[low], &ValidateParams::default())).contains(&RULE_CAP_DESIGN));

        // Same design on a high-tier child (two files) is not capped.
        let mut high = child(
            "c",
            "# Scope\nx\n# Files\na.rs b.rs\n# Contract\nx\n# Invariants\nx\n# Non-goals\nx\n# MACHINE\nprovides: p\nfiles: a.rs b.rs\n",
        );
        high.labels = vec!["staged".to_string(), "difficulty:high".to_string()];
        high.design = Some(long_design);
        assert!(!rules(&validate(&[high], &ValidateParams::default())).contains(&RULE_CAP_DESIGN));
    }

    #[test]
    fn tier_mismatch_detects_label_and_count_problems() {
        let mut wrong = low_child("c");
        wrong.labels = vec!["staged".to_string(), "difficulty:high".to_string()];
        assert!(rules(&validate(&[wrong], &ValidateParams::default()))
            .contains(&RULE_TIER_MISMATCH));

        let mut none = low_child("c");
        none.labels = vec!["staged".to_string()];
        assert!(rules(&validate(&[none], &ValidateParams::default()))
            .contains(&RULE_TIER_MISMATCH));

        let mut two = low_child("c");
        two.labels = vec![
            "difficulty:low".to_string(),
            "difficulty:high".to_string(),
        ];
        assert!(rules(&validate(&[two], &ValidateParams::default()))
            .contains(&RULE_TIER_MISMATCH));
    }

    #[test]
    fn derived_tier_is_high_for_markers_or_many_files() {
        assert_eq!(derive_tier(&low_child("c")), Tier::Low);

        // A difficulty marker flips to high.
        let mut marker = low_child("c");
        marker.description = marker.description.replace("one small thing", "spawn a subprocess");
        assert_eq!(derive_tier(&marker), Tier::High);

        // Two files flip to high.
        let mut two_files = low_child("c");
        two_files.description = two_files.description.replace(
            "files: crates/foo/src/c.rs",
            "files: crates/foo/src/c.rs crates/foo/src/d.rs",
        );
        assert_eq!(derive_tier(&two_files), Tier::High);

        // A non-empty provides flips to high.
        let mut provides = low_child("c");
        provides.description = provides
            .description
            .replace("provides:\n", "provides: foo.bar\n");
        assert_eq!(derive_tier(&provides), Tier::High);
    }

    #[test]
    fn interim_value_scan_covers_all_four_phrases() {
        for phrase in INTERIM_PHRASES {
            let mut c = low_child("c");
            c.description = format!("{} {phrase} x\n{}", c.description, "");
            assert!(
                rules(&validate(&[c], &ValidateParams::default())).contains(&RULE_INTERIM_VALUE),
                "phrase {phrase:?} not caught"
            );
        }
    }

    #[test]
    fn unprovided_symbol_requires_self_or_transitive_provider() {
        // consumer consumes `p`, provider provides it, consumer depends on provider.
        let provider = child(
            "p",
            "# Scope\nx\n# Files\na.rs\n# Contract\nx\n# Invariants\nx\n# Non-goals\nx\n# MACHINE\nprovides: sym.a\nfiles: a.rs\n",
        );
        let mut consumer = child(
            "c",
            "# Scope\nx\n# Files\nb.rs\n# Contract\nx\n# Invariants\nx\n# Non-goals\nx\n# MACHINE\nconsumes: sym.a\nfiles: b.rs\n",
        );
        consumer.depends_on = vec!["p".to_string()];
        consumer.labels = vec!["staged".to_string(), "difficulty:high".to_string()];
        let mut provider_high = provider;
        provider_high.labels = vec!["staged".to_string(), "difficulty:high".to_string()];
        let violations = validate(&[provider_high, consumer.clone()], &ValidateParams::default());
        assert!(
            !rules(&violations).contains(&RULE_UNPROVIDED_SYMBOL),
            "transitive provider should satisfy consumes: {violations:?}"
        );

        // Without the dep edge, the symbol is unprovided.
        let mut orphan = consumer;
        orphan.depends_on = Vec::new();
        let violations = validate(&[orphan], &ValidateParams::default());
        assert!(rules(&violations).contains(&RULE_UNPROVIDED_SYMBOL));
    }

    #[test]
    fn graph_cycle_is_reported() {
        let mut a = low_child("a");
        let mut b = low_child("b");
        a.depends_on = vec!["b".to_string()];
        b.depends_on = vec!["a".to_string()];
        let violations = validate(&[a, b], &ValidateParams::default());
        assert!(rules(&violations).contains(&RULE_GRAPH_CYCLE));
        assert!(!rules(&violations).contains(&RULE_GRAPH_WIDTH));
    }

    #[test]
    fn graph_width_uses_median_wave_width() {
        // Fully serial chain: widths = [1, 1, 1] -> median 1.
        let mut a = low_child("a");
        let mut b = low_child("b");
        b.depends_on = vec!["a".to_string()];
        let mut c = low_child("c");
        c.depends_on = vec!["b".to_string()];
        assert!(!rules(&validate(&[a.clone(), b.clone(), c.clone()], &ValidateParams::default()))
            .contains(&RULE_GRAPH_WIDTH));

        // min_width 2 fails the serial chain...
        let params = ValidateParams {
            min_width: 2,
            ..ValidateParams::default()
        };
        assert!(rules(&validate(&[a, b, c], &params)).contains(&RULE_GRAPH_WIDTH));
    }

    #[test]
    fn median_width_handles_odd_even_and_empty() {
        assert_eq!(median_width(&[]), 0.0);
        assert_eq!(median_width(&[3]), 3.0);
        assert_eq!(median_width(&[1, 1, 5]), 1.0);
        assert_eq!(median_width(&[2, 4]), 3.0);
    }

    #[test]
    fn intake_block_checks_fence_directive_acceptance_budget() {
        // Valid fence passes.
        assert!(!rules(&validate(&[low_child("c")], &ValidateParams::default()))
            .contains(&RULE_INTAKE_BLOCK));

        // No fence.
        let mut no_fence = low_child("c");
        no_fence.description = no_fence
            .description
            .split("```yaml")
            .next()
            .unwrap()
            .to_string();
        assert!(rules(&validate(&[no_fence], &ValidateParams::default()))
            .contains(&RULE_INTAKE_BLOCK));

        // Missing budget.
        let mut no_budget = low_child("c");
        no_budget.description = no_budget
            .description
            .lines()
            .filter(|l| !l.starts_with("budget_usd:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rules(&validate(&[no_budget], &ValidateParams::default()))
            .contains(&RULE_INTAKE_BLOCK));

        // Wrong directive.
        let mut wrong = low_child("c");
        wrong.description = wrong
            .description
            .replace("directive: directive/v0", "directive: directive/v1");
        assert!(rules(&validate(&[wrong], &ValidateParams::default()))
            .contains(&RULE_INTAKE_BLOCK));
    }

    #[test]
    fn human_verdict_detection() {
        assert!(has_human_verdict(&["decomp-verdict: approved 7".to_string()]));
        assert!(has_human_verdict(&["Verdict: accept".to_string()]));
        assert!(!has_human_verdict(&["looks fine".to_string()]));
        assert!(!has_human_verdict(&[]));
    }

    #[test]
    fn parse_machine_reads_list_values() {
        let machine = parse_machine(
            "# MACHINE\nprovides: a, b\nconsumes: [c, d]\nfiles: x.rs y.rs\ntier: high\n",
        );
        assert_eq!(machine.provides, vec!["a", "b"]);
        assert_eq!(machine.consumes, vec!["c", "d"]);
        assert_eq!(machine.files, vec!["x.rs", "y.rs"]);
        assert_eq!(machine.tier.as_deref(), Some("high"));
    }
}
