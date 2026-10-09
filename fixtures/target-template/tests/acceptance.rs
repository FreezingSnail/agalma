//! Acceptance test for the M0 fixture task (`fix-answer`).
//!
//! Fails against the seed (`answer() == 41`); passes once the builder fixes it.

use target_template::answer;

#[test]
fn answer_is_42() {
    assert_eq!(answer(), 42);
}
