use crate::execution::runner::{CaseResult, StepOutcome};
use crate::experiment::selectors::ObservationSelector;
use crate::experiment::types::{
    ExpectCondition, Experiment, OracleDecl, OracleType, ReplacementValue,
};
use crate::mutation::RegressionVariant;

use super::schema_validator::SchemaValidatorCache;

/// Implicit rule added to `mode: schema` cases whose value the schema forbids.
pub(crate) const SCHEMA_REJECTION_RULE: &str = "schema:rejects-invalid";
use super::{evaluate_check, OracleContext, OracleEvaluation, Verdict};

pub struct EvaluatedCase {
    pub execution: CaseResult,
    pub plan: ExecutedPlan,
    pub(crate) checks: Vec<Vec<CaseCheck>>,
}

pub enum ExecutedPlan {
    Baseline,
    Variant {
        step_id: String,
        selector: String,
        replacement: ReplacementValue,
        replay: Option<RegressionVariant>,
    },
    /// A random case from `strategy: fuzz`, reproducible from its seed and iteration.
    Fuzz {
        step_id: String,
        replacements: Vec<(String, ReplacementValue)>,
        seed: u64,
        iteration: u64,
    },
    /// A case generated from a `cases.examples` entry.
    Example {
        example_id: String,
        step_id: String,
        selector: String,
        replacement: ReplacementValue,
        expect_status: u16,
    },
}

pub(crate) struct CaseCheck {
    pub oracle: OracleDecl,
    pub evaluation: OracleEvaluation,
    pub record_observation_on_pass: bool,
}

impl EvaluatedCase {
    pub(crate) fn new(
        experiment: &Experiment,
        execution: CaseResult,
        plan: ExecutedPlan,
        schema_cache: &SchemaValidatorCache,
    ) -> Self {
        let outputs = std::collections::HashMap::new();
        let checks = execution
            .step_results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let step = experiment
                    .steps
                    .iter()
                    .find(|step| step.id == result.step_id);
                let operation_id = step.map(|s| s.operation_id.as_str()).unwrap_or("");
                let declared_statuses = step.map(|s| s.declared_statuses.as_slice()).unwrap_or(&[]);
                let context = OracleContext {
                    current_step: result,
                    previous_steps: &execution.step_results[..index],
                    case_id: &execution.case_id,
                    step_outputs: &outputs,
                    operation_id,
                    declared_statuses,
                    schema_cache,
                };
                let mut checks: Vec<_> = step
                    .into_iter()
                    .flat_map(|step| &step.oracles)
                    .map(|oracle| CaseCheck {
                        oracle: oracle.clone(),
                        evaluation: evaluate_check(oracle, &context),
                        record_observation_on_pass: false,
                    })
                    .collect();
                // A schema-invalid value must be rejected by the step that received it.
                if let ExecutedPlan::Variant {
                    step_id,
                    replay: Some(variant),
                    ..
                } = &plan
                {
                    if step_id == &result.step_id && variant.expects_rejection() {
                        let oracle = OracleDecl {
                            id: SCHEMA_REJECTION_RULE.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Rejected),
                            record_reference: None,
                        };
                        checks.push(CaseCheck {
                            evaluation: evaluate_check(&oracle, &context),
                            record_observation_on_pass: true,
                            oracle,
                        });
                    }
                }
                if let ExecutedPlan::Example {
                    example_id,
                    step_id,
                    expect_status,
                    ..
                } = &plan
                {
                    if step_id == &result.step_id {
                        let oracle = OracleDecl {
                            id: format!("example:{example_id}"),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Equals(serde_json::Value::from(
                                *expect_status,
                            ))),
                            record_reference: None,
                        };
                        checks.push(CaseCheck {
                            evaluation: evaluate_check(&oracle, &context),
                            record_observation_on_pass: true,
                            oracle,
                        });
                    }
                }
                checks
            })
            .collect();
        Self {
            execution,
            plan,
            checks,
        }
    }

    pub(crate) fn setup_failure(&self) -> Option<(usize, usize)> {
        if !matches!(self.plan, ExecutedPlan::Baseline) {
            return None;
        }
        self.checks.iter().enumerate().find_map(|(step, checks)| {
            checks
                .iter()
                .position(|check| check.oracle.setup && check.evaluation.verdict != Verdict::Pass)
                .map(|check| (step, check))
        })
    }

    pub(crate) fn is_incomplete(&self) -> bool {
        self.execution.step_results.iter().any(|step| {
            step.outcome != StepOutcome::Completed
                || step
                    .observation
                    .as_ref()
                    .is_none_or(|response| response.truncated)
        }) || self
            .checks
            .iter()
            .flatten()
            .any(|check| check.evaluation.verdict == Verdict::Inconclusive)
    }

    pub(crate) fn failures(
        &self,
    ) -> impl Iterator<Item = (usize, usize, &super::ViolationCategory)> + '_ {
        self.checks
            .iter()
            .enumerate()
            .flat_map(move |(step, checks)| {
                checks
                    .iter()
                    .enumerate()
                    .filter_map(move |(check, result)| match &result.evaluation.verdict {
                        Verdict::Fail { category } => Some((step, check, category)),
                        _ => None,
                    })
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Passed,
    Failed,
    Incomplete,
}

impl RunStatus {
    pub(crate) fn classify(setup_failed: bool, has_findings: bool, incomplete: bool) -> Self {
        match (setup_failed, has_findings, incomplete) {
            (true, _, _) | (false, false, true) => Self::Incomplete,
            (false, true, _) => Self::Failed,
            (false, false, false) => Self::Passed,
        }
    }

    pub(crate) fn for_case(case: &EvaluatedCase) -> Self {
        Self::classify(
            case.setup_failure().is_some(),
            case.failures().next().is_some(),
            case.is_incomplete(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::RunStatus;

    #[test]
    fn classifies_combined_outcomes() {
        assert_eq!(RunStatus::classify(false, false, false), RunStatus::Passed);
        assert_eq!(
            RunStatus::classify(false, false, true),
            RunStatus::Incomplete
        );
        assert_eq!(RunStatus::classify(false, true, true), RunStatus::Failed);
        assert_eq!(RunStatus::classify(true, true, true), RunStatus::Incomplete);
    }
}
