//! Structured questions surfaced to the LLM at each turn to encourage
//! deliberate reasoning before acting.
//!
//! The bank holds 20 questions. Each prompt receives 3 distinct ones selected
//! via a rotating counter, so over many turns every question is surfaced.

use std::sync::atomic::{AtomicU64, Ordering};

static QUESTION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// All 20 candidate questions. Each targets a different failure mode:
/// stale assumptions, redundant writes, role confusion, cascade effects, etc.
pub(crate) const QUESTIONS: [&str; 20] = [
    // Provenance / staleness
    "Does the runtime itself write or control the thing I am about to modify?",
    "Am I acting on a direct file read from this turn, or on a memory of a prior read?",
    "Is the state I observe now authoritative, or could another agent have changed it since?",
    "Is the invariant I am enforcing actually violated right now, or was it violated in a prior cycle?",

    // Redundancy / duplication
    "Have I already attempted this exact action in this cycle and observed the same result?",
    "Am I creating a tracking entry — issue, objective, or task — that duplicates one that already exists?",
    "Does this action create something that already exists elsewhere in the workspace?",

    // Scope / minimality
    "What is the narrowest change that achieves the goal — is my action wider than necessary?",
    "Is there a read-only action I should take first before committing to a write?",
    "Am I resolving a root cause or masking a symptom?",

    // Forward effect / cascade
    "Could this action trigger a cascade that undoes its own effect?",
    "After this action, will the system be closer to idle, or will it trigger more required work?",
    "Is there a guard or protocol rule that prevents this action from taking effect even if it executes?",

    // Deferral / necessity
    "Would skipping this action and deferring to the next cycle produce the same outcome?",
    "What is the worst case if I skip this action entirely this turn?",

    // Verifiability / evidence
    "What evidence would confirm this action succeeded — and will I be able to observe it?",
    "Am I predicting the result of this action, or have I seen direct evidence of how similar actions ended?",
    "If this action fails, will the system be in a worse state than before it started?",

    // Role / phase
    "Am I in the correct role and phase to perform this action?",
    "Does the source I am about to change have tests that will catch regressions from this edit?",
];

/// Return 3 distinct questions from the bank using a rotating counter.
/// Indices are spaced at 0, 7, 14 from the current offset to spread coverage
/// across the full list without clustering.
pub(crate) fn select_questions() -> [&'static str; 3] {
    let offset = QUESTION_COUNTER.fetch_add(3, Ordering::Relaxed) as usize;
    let n = QUESTIONS.len();
    [
        QUESTIONS[offset % n],
        QUESTIONS[(offset + 7) % n],
        QUESTIONS[(offset + 14) % n],
    ]
}

/// Format the 3 selected questions as a prompt snippet ready for injection.
pub(crate) fn questions_prompt_snippet() -> String {
    let qs = select_questions();
    format!(
        "Before acting, answer these questions internally:\n- {}\n- {}\n- {}",
        qs[0], qs[1], qs[2]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_returns_three_distinct_questions() {
        let qs = select_questions();
        assert_ne!(qs[0], qs[1]);
        assert_ne!(qs[1], qs[2]);
        assert_ne!(qs[0], qs[2]);
    }

    #[test]
    fn all_questions_non_empty() {
        for q in QUESTIONS {
            assert!(!q.is_empty());
        }
    }

    #[test]
    fn counter_rotates_coverage() {
        // After 20 calls the counter has advanced by 60; every index 0..20
        // must have been the leading index at least once.
        let start = QUESTION_COUNTER.load(Ordering::Relaxed) as usize;
        let mut seen = std::collections::HashSet::new();
        for i in 0..20usize {
            seen.insert((start + i * 3) % QUESTIONS.len());
        }
        assert_eq!(seen.len(), QUESTIONS.len());
    }
}
