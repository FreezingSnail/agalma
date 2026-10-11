//! Canonical role-prompt sanity (CP.2, `agalma-i5h.3`).
//!
//! The concierge/designer prompts are shipped with the conductor and embedded
//! via `include_str!`; these tests assert they exist, carry the output-contract
//! markers the conductor parses, and never name a model or effort level (model
//! assignment is single-source in the routing table).

use agalma_conductor::{CONCIERGE_PROMPT, DESIGNER_PROMPT};

/// Model identifiers or effort tokens that must never appear in a prompt file.
const FORBIDDEN_TOKENS: &[&str] = &[
    "deepseek",
    "gpt-6",
    "claude",
    "haiku",
    "luna",
    "sonnet",
    "opus",
    "big-pickle",
    "mimo",
    "glm",
    "qwen",
];

#[test]
fn concierge_prompt_is_present_and_carries_its_contract() {
    assert!(
        CONCIERGE_PROMPT.trim().len() > 200,
        "concierge prompt is empty or trivially short"
    );
    for marker in ["epic:", "design:", "handoff:"] {
        assert!(
            CONCIERGE_PROMPT.contains(marker),
            "concierge prompt missing contract marker {marker:?}"
        );
    }
    // The concierge never decomposes or files children.
    assert!(CONCIERGE_PROMPT.contains("NEVER") && CONCIERGE_PROMPT.contains("child"));
    assert_no_forbidden_tokens(CONCIERGE_PROMPT);
}

#[test]
fn designer_prompt_is_present_and_carries_its_contract() {
    assert!(
        DESIGNER_PROMPT.trim().len() > 500,
        "designer prompt is empty or trivially short"
    );
    for marker in [
        "designed:",
        "subtasks:",
        "deps:",
        "staged:",
        "difficulty:",
        "# MACHINE",
        "directive/v0",
        "plan-check",
    ] {
        assert!(
            DESIGNER_PROMPT.contains(marker),
            "designer prompt missing contract marker {marker:?}"
        );
    }
    // The designer never implements, reviews, repairs, undefers, or claims.
    assert!(DESIGNER_PROMPT.contains("NEVER"));
    assert_no_forbidden_tokens(DESIGNER_PROMPT);
}

#[test]
fn designer_prompt_lists_the_fixed_child_headings_in_order() {
    let headings = [
        "# Scope",
        "# Files",
        "# Contract",
        "# Invariants",
        "# Non-goals",
        "# MACHINE",
    ];
    let mut cursor = 0usize;
    for heading in headings {
        let found = DESIGNER_PROMPT[cursor..]
            .find(heading)
            .unwrap_or_else(|| panic!("designer prompt missing heading {heading:?}"));
        cursor += found + heading.len();
    }
}

fn assert_no_forbidden_tokens(prompt: &str) {
    let lower = prompt.to_lowercase();
    for token in FORBIDDEN_TOKENS {
        assert!(
            !lower.contains(token),
            "prompt names a forbidden model/effort token {token:?}"
        );
    }
}
