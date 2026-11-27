use std::collections::HashSet;

use super::progress::EvidenceKind;
use crate::adapters::http::transport::{FailurePhase, TransportReason};
use crate::config::ResolvedExperiment;
use crate::evaluation::case::EvaluatedCase;
use crate::execution::diagnostic::{self, ExecutionOutcome};
use crate::storage;

type TransportKey = (String, Option<TransportReason>, Option<FailurePhase>);

#[derive(Default)]
pub(super) struct IncidentRecorder {
    saved_transport: HashSet<TransportKey>,
    transport_errors: u64,
}

impl IncidentRecorder {
    pub(super) fn transport_errors(&self) -> u64 {
        self.transport_errors
    }

    pub(super) fn record_case(
        &mut self,
        configuration: &ResolvedExperiment,
        case: &EvaluatedCase,
        number: u32,
        secrets: &[String],
        oracle_evidence: bool,
        mut show: impl FnMut(EvidenceKind, String),
    ) {
        let mut record = None;
        for incident in diagnostic::for_case(case) {
            let transport_key = match incident.outcome {
                ExecutionOutcome::ProcessExit { .. } => None,
                ExecutionOutcome::TransportError { reason, phase } => {
                    self.transport_errors += 1;
                    let key = (incident.step_id.to_owned(), reason, phase);
                    if self.saved_transport.contains(&key) {
                        continue;
                    }
                    Some(key)
                }
            };
            let record = record
                .get_or_insert_with(|| storage::case_record(configuration, case, number, secrets));
            let label = format!("{} · {}", incident.outcome.kind(), incident.step_id);
            let display_kind = if transport_key.is_some() {
                EvidenceKind::Transport
            } else {
                EvidenceKind::Crash
            };
            match storage::write_execution_diagnostic(&configuration.directory, record, &incident) {
                Ok(path) => {
                    if let Some(key) = transport_key {
                        self.saved_transport.insert(key);
                    }
                    show(display_kind, format!("{label} → {}", path.display()));
                }
                Err(error) => show(
                    display_kind,
                    format!("{label} → diagnostic not written: {error}"),
                ),
            }
        }
        if oracle_evidence && record.is_none() {
            let record = storage::case_record(configuration, case, number, secrets);
            let label = super::crash_label(&record);
            let row = match storage::write_crash_record(&configuration.directory, &record) {
                Ok(path) => format!("{label} → {}", path.display()),
                Err(error) => format!("{label} → crash file not written: {error}"),
            };
            show(EvidenceKind::Crash, row);
        }
    }
}
