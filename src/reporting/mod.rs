//! Human-readable run summary rendering.
//!
//! Renders a campaign result as text for the CLI.

use crate::exploration::campaign::CampaignResult;

/// Render a human-readable summary of a campaign.
pub fn render_summary(result: &CampaignResult) -> String {
    let mut s = String::new();

    s.push_str("═══════════════════════════════════\n");
    s.push_str("  Nadir Run Summary\n");
    s.push_str("═══════════════════════════════════\n\n");

    if let Some(ref baseline) = result.baseline {
        s.push_str(&format!("Baseline case: {}\n", baseline.case_id));
        s.push_str(&format!("  Steps: {}\n", baseline.step_results.len()));
    }

    s.push_str(&format!(
        "\nEnumeration cases: {}\n",
        result.enumeration_cases.len()
    ));
    s.push_str(&format!(
        "Campaign execs: {}\n",
        result.run_counters.campaign_execs
    ));
    s.push_str(&format!(
        "Total cases: {}\n",
        result.run_counters.cases_executed
    ));

    s.push_str(&format!(
        "\nFindings: {}\n",
        result.run_counters.findings_count
    ));
    for group in result.findings.groups() {
        s.push_str(&format!(
            "  - {} (×{})\n",
            format_group_key(&group.key),
            group.occurrence_count
        ));
    }

    s.push_str(&format!("\nStop reason: {:?}\n", result.stop_reason));

    s.push_str("\nPhases:\n");
    s.push_str(&format!(
        "  Enumeration completed: {}\n",
        result.phases.enumeration_completed
    ));
    s.push_str(&format!(
        "  Campaign started: {}\n",
        result.phases.campaign_started
    ));

    s
}

fn format_group_key(key: &crate::finding::GroupingKey) -> String {
    match key {
        crate::finding::GroupingKey::OracleFailure {
            step_id,
            oracle_id,
            violation_category,
            ..
        } => {
            format!(
                "oracle failure [{}/{}] {}",
                step_id, oracle_id, violation_category
            )
        }
        crate::finding::GroupingKey::ProcessExit {
            exit_code, signal, ..
        } => {
            format!("process exit (code={:?}, signal={:?})", exit_code, signal)
        }
    }
}

use std::path::Path;

use crate::artifacts::{CaseRecord, VerdictRecord};
use crate::evaluation::{case::EvaluatedCase, Verdict};
use crate::exploration::EnumerationResult;
use crate::storage::WrittenRun;

pub(crate) fn print_check(step_id: &str, step_outcome: &str, check: &VerdictRecord) {
    if let Some(location) = &check.schema_location {
        eprintln!(
            "step {step_id}, rule {}: {}; {} ({location})",
            check.oracle_id,
            check.verdict,
            check.reason.as_deref().unwrap_or(step_outcome)
        );
        return;
    }
    eprintln!(
        "step {step_id}, rule {}: {}; expected {}, observed {}",
        check.oracle_id,
        check.verdict,
        check.expected.as_deref().unwrap_or("pass"),
        check
            .observed
            .as_deref()
            .or(check.reason.as_deref())
            .unwrap_or(step_outcome),
    );
}

pub(crate) fn print_regression_run(path: &Path, result: &EnumerationResult, written: &WrittenRun) {
    let cases = written
        .record
        .baseline_case
        .iter()
        .chain(&written.record.cases);
    for (evaluated, case) in std::iter::once(&result.baseline_case)
        .chain(&result.cases)
        .zip(cases.clone())
    {
        print_case_diagnostics(evaluated, case, false);
    }
    // Group findings by case so each case's record and replay lines print once.
    let mut case_ids: Vec<&str> = Vec::new();
    for finding in &written.record.findings {
        if !case_ids.contains(&finding.case_id.as_str()) {
            case_ids.push(&finding.case_id);
        }
    }
    for case_id in case_ids {
        let Some(case) = cases.clone().find(|case| case.case_id == case_id) else {
            continue;
        };
        for finding in written
            .record
            .findings
            .iter()
            .filter(|finding| finding.case_id == case_id)
        {
            if finding.kind == "process_exit" {
                eprintln!(
                    "step {}: target process exited ({})",
                    finding.step_id.as_deref().unwrap_or("?"),
                    crate::target::launch::describe_exit(finding.exit_code, finding.signal)
                );
            } else if let Some(step) = case
                .step_results
                .iter()
                .find(|step| Some(&step.step_id) == finding.step_id.as_ref())
            {
                if let Some(check) = step.oracle_verdicts.iter().find(|check| {
                    Some(&check.oracle_id) == finding.oracle_id.as_ref()
                        && check.category == finding.violation_category
                        && check.verdict == "fail"
                }) {
                    print_check(&step.step_id, &step.outcome, check);
                }
            }
            if finding.occurrence_count > 1 {
                let others: Vec<String> = cases
                    .clone()
                    .filter(|other| other.case_id != case.case_id)
                    .filter(|other| {
                        other.step_results.iter().any(|step| {
                            Some(&step.step_id) == finding.step_id.as_ref()
                                && step.oracle_verdicts.iter().any(|check| {
                                    Some(&check.oracle_id) == finding.oracle_id.as_ref()
                                        && check.category == finding.violation_category
                                        && check.verdict == "fail"
                                })
                        })
                    })
                    .map(case_label)
                    .collect();
                if !others.is_empty() {
                    eprintln!("  same rule also failed in: {}", others.join(", "));
                }
            }
        }
        if let Some(location) = written.case_path(case.case_number) {
            eprintln!("case record: {}", location.display());
            if case.regression_plan.is_some() {
                eprintln!(
                    "replay: nadir replay {} --case {}",
                    shell_path(path),
                    shell_path(location)
                );
            }
        }
    }
    let kept = result.fuzz.as_ref().map_or(0, |fuzz| fuzz.kept);
    eprintln!("Enumeration cases: {}", written.record.cases.len() - kept);
    if let Some(fuzz) = &result.fuzz {
        eprintln!(
            "Random cases: {} (kept {}, seed {})",
            fuzz.executed, fuzz.kept, fuzz.seed
        );
        if fuzz.incomplete > 0 {
            eprintln!(
                "  {} random cases got no usable response (timeouts or dropped connections)",
                fuzz.incomplete
            );
        }
    }
    eprintln!(
        "Findings: {}\nStop reason: {:?}\nRun record: {}",
        written.record.counters.findings_count,
        result.stop_reason,
        written.run_file.display(),
    );
}

/// `0004.json (null_value)`: a case file and the variant it sent.
fn case_label(case: &CaseRecord) -> String {
    let variant = case
        .regression_plan
        .as_ref()
        .and_then(|plan| plan.variant.as_ref())
        .and_then(|variant| serde_json::to_value(variant).ok())
        .and_then(|variant| {
            let id = variant.get("variant_id")?;
            let kind = id.get("kind")?.as_str()?.to_owned();
            Some(match id.get("option") {
                Some(option) => format!("{kind} {option}"),
                None => kind,
            })
        });
    let random = case
        .regression_plan
        .as_ref()
        .and_then(|plan| plan.fuzz)
        .map(|fuzz| format!("random {}", fuzz.iteration));
    match variant.or(random) {
        Some(label) => format!("{:04}.json ({label})", case.case_number),
        None => format!("{:04}.json", case.case_number),
    }
}

pub(crate) fn print_replayed_case(evaluated: &EvaluatedCase, record: &CaseRecord) {
    eprintln!("Replayed case: {}", record.case_id);
    print_case_diagnostics(evaluated, record, true);
}

fn print_case_diagnostics(evaluated: &EvaluatedCase, case: &CaseRecord, show_failures: bool) {
    let mut printed_interruption = false;
    for (result, step) in evaluated
        .execution
        .step_results
        .iter()
        .zip(&case.step_results)
    {
        if let Some(reason) = step.transport_reason {
            if let Some(phase) = step.transport_phase {
                eprintln!(
                    "step {}: {} ({reason}, {phase})",
                    step.step_id, step.outcome
                );
            } else {
                eprintln!("step {}: {} ({reason})", step.step_id, step.outcome);
            }
            printed_interruption = true;
        } else if !printed_interruption
            && (result.outcome != crate::execution::runner::StepOutcome::Completed
                || result
                    .observation
                    .as_ref()
                    .is_some_and(|response| response.truncated))
        {
            eprintln!("step {}: {}", step.step_id, step.outcome);
            printed_interruption = true;
        }
    }
    if let Some((step, check)) = evaluated.setup_failure() {
        let step = &case.step_results[step];
        print_check(&step.step_id, &step.outcome, &step.oracle_verdicts[check]);
        if show_failures {
            eprintln!("Stop reason: SetupCheckFailed");
        }
        return;
    }
    for (step, checks) in case.step_results.iter().zip(&evaluated.checks) {
        for (check, record) in checks.iter().zip(&step.oracle_verdicts) {
            if check.evaluation.verdict == Verdict::Inconclusive
                || (show_failures && matches!(check.evaluation.verdict, Verdict::Fail { .. }))
            {
                print_check(&step.step_id, &step.outcome, record);
            }
        }
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
