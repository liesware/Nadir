//! Signature feedback for LibAFL.
//!
//! Computes a stable `CaseSignature` from step outcomes, status codes,
//! and verdict oracle IDs. Excludes response bodies, dynamic IDs, tokens,
//! timestamps, and exact durations.
//!
//! Full LibAFL `Feedback` trait integration comes in Phase 5a. Phase 3d
//! provides the building blocks: signature construction and novelty checks.

use crate::execution::runner::CaseResult;

// ── Signature ──────────────────────────────────────────────────────

/// Stable, deterministic summary of a case result.
///
/// Excluded from the signature: response bodies, dynamic IDs, tokens,
/// timestamps, and exact durations.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CaseSignature {
    /// One signature entry per step in declaration order.
    pub steps: Vec<StepSignature>,
}

/// Signature for a single step.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct StepSignature {
    pub step_id: String,
    /// Outcome kind (completed, skipped, transport_error, etc.)
    pub outcome_kind: String,
    /// HTTP status code, if available.
    pub status: Option<u16>,
    /// Oracle verdicts: (oracle_id, verdict_kind) for each oracle.
    pub verdicts: Vec<(String, String)>,
}

/// Build a `CaseSignature` from a `CaseResult`.
pub fn build_case_signature(case: &CaseResult) -> CaseSignature {
    let steps: Vec<StepSignature> = case
        .step_results
        .iter()
        .map(|sr| {
            let outcome_kind = match sr.outcome {
                crate::execution::runner::StepOutcome::Completed => "completed",
                crate::execution::runner::StepOutcome::Skipped { .. } => "skipped",
                crate::execution::runner::StepOutcome::TransportError { .. } => {
                    "transport_error"
                }
                crate::execution::runner::StepOutcome::InputError { .. } => "input_error",
                crate::execution::runner::StepOutcome::ExtractionError { .. } => {
                    "extraction_error"
                }
            }
            .to_string();

            let status = sr.observation.as_ref().map(|o| o.status);

            // Verdicts are evaluated later (in the oracle phase).
            // For signature, we use empty — verdicts get merged in during
            // oracle evaluation when the oracles are associated with steps.
            let verdicts = Vec::new();

            StepSignature {
                step_id: sr.step_id.clone(),
                outcome_kind,
                status,
                verdicts,
            }
        })
        .collect();

    CaseSignature { steps }
}

/// Merge oracle verdicts into an existing signature.
pub fn merge_verdicts(
    sig: &mut CaseSignature,
    step_id: &str,
    oracle_id: &str,
    verdict_kind: &str,
) {
    if let Some(step_sig) = sig.steps.iter_mut().find(|s| s.step_id == step_id) {
        step_sig
            .verdicts
            .push((oracle_id.to_string(), verdict_kind.to_string()));
    }
}

// ── SignatureFeedback ──────────────────────────────────────────────

/// LibAFL feedback that reports novelty based on the case signature.
///
/// The signature is used alongside coverage map feedback via `feedback_or`.
/// A new signature means the case produced a previously unseen combination
/// of step outcomes and status codes.
#[derive(Debug, Default)]
pub struct SignatureFeedback {
    /// The last signature produced by the executor.
    last_sig: Option<CaseSignature>,
}

impl SignatureFeedback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a case result produces a novel signature.
    /// Returns `true` if the signature is new or different from the last one.
    pub fn is_novel(&self, case: &CaseResult) -> bool {
        let sig = build_case_signature(case);
        self.last_sig.as_ref().is_none_or(|old| *old != sig)
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::http::transport::HttpObservation;
    use crate::execution::runner::{StepOutcome, StepResult};
    use std::collections::HashMap;

    #[test]
    fn test_signature_deterministic() {
        let step1 = StepResult {
            step_id: "login".into(),
            outcome: StepOutcome::Completed,
            observation: Some(HttpObservation {
                status: 200,
                headers: vec![],
                body: b"{\"token\":\"abc\"}".to_vec(),
                truncated: false,
            }),
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        };

        let case = CaseResult {
            case_id: "case-1".into(),
            step_results: vec![step1],
            was_executed: true,
            requests_sent: 0,
        };

        let sig1 = build_case_signature(&case);
        let sig2 = build_case_signature(&case);
        assert_eq!(sig1, sig2, "signature should be deterministic");
    }

    #[test]
    fn test_signature_differs_on_status() {
        let step_ok = StepResult {
            step_id: "login".into(),
            outcome: StepOutcome::Completed,
            observation: Some(HttpObservation {
                status: 200,
                headers: vec![],
                body: vec![],
                truncated: false,
            }),
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        };

        let step_err = StepResult {
            step_id: "login".into(),
            outcome: StepOutcome::Completed,
            observation: Some(HttpObservation {
                status: 401,
                headers: vec![],
                body: vec![],
                truncated: false,
            }),
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        };

        let sig_ok = build_case_signature(&CaseResult {
            case_id: "ok".into(),
            step_results: vec![step_ok],
            was_executed: true,
            requests_sent: 0,
        });
        let sig_err = build_case_signature(&CaseResult {
            case_id: "err".into(),
            step_results: vec![step_err],
            was_executed: true,
            requests_sent: 0,
        });

        assert_ne!(sig_ok, sig_err, "different status codes should differ");
    }

    #[test]
    fn test_signature_excludes_body() {
        let step_a = StepResult {
            step_id: "s1".into(),
            outcome: StepOutcome::Completed,
            observation: Some(HttpObservation {
                status: 200,
                headers: vec![],
                body: b"body-A".to_vec(),
                truncated: false,
            }),
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        };
        let step_b = StepResult {
            step_id: "s1".into(),
            outcome: StepOutcome::Completed,
            observation: Some(HttpObservation {
                status: 200,
                headers: vec![],
                body: b"different-body".to_vec(),
                truncated: false,
            }),
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        };

        let sig_a = build_case_signature(&CaseResult {
            case_id: "a".into(),
            step_results: vec![step_a],
            was_executed: true,
            requests_sent: 0,
        });
        let sig_b = build_case_signature(&CaseResult {
            case_id: "b".into(),
            step_results: vec![step_b],
            was_executed: true,
            requests_sent: 0,
        });

        assert_eq!(sig_a, sig_b, "signature should exclude body content");
    }
}
