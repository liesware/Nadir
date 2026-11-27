//! Finding deduplication and evidence attachment.
//!
//! # Design notes
//!
//! **Chosen:** deduplication is a pure function of `GroupingKey`. No global
//! state or side effects. The dedup map is a `HashMap<GroupingKey, FindingGroup>`.

use std::collections::HashMap;

use crate::evaluation::Verdict;
use crate::exploration::libafl::feedback::CaseSignature;
use crate::mutation::CasePlan;

// ── Grouping key ───────────────────────────────────────────────────

/// Unique key for deduplicating findings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GroupingKey {
    /// An oracle failure, grouped by contract fingerprint + violation details.
    OracleFailure {
        contract_fingerprint: [u8; 32],
        kind: FindingKind,
        step_id: String,
        oracle_id: String,
        violation_category: String,
    },
    /// A process exit, grouped by build fingerprint + exit details.
    ProcessExit {
        build_fingerprint: [u8; 32],
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingKind {
    OracleFailure,
    ProcessExit,
}

// ── Finding group ──────────────────────────────────────────────────

/// A deduplicated finding group.
#[derive(Debug, Clone)]
pub struct FindingGroup {
    pub key: GroupingKey,
    pub first_occurrence: FindingOccurrence,
    pub occurrence_count: u32,
    /// At most 16 representative signatures.
    pub representative_signatures: Vec<CaseSignature>,
    pub original_plan: Option<CasePlan>,
    pub minimized_plan: Option<ReductionResult>,
}

#[derive(Debug, Clone)]
pub struct FindingOccurrence {
    pub case_id: String,
    pub step_id: String,
    pub oracle_id: Option<String>,
    pub verdict: Option<Verdict>,
}

// ── Reduction ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReductionResult {
    pub status: ReductionStatus,
    pub best_plan: CasePlan,
    pub executions_used: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReductionStatus {
    Unchanged,
    Reduced,
    BudgetExhausted,
}

// ── Dedup map ──────────────────────────────────────────────────────

/// Deduplication map: groups findings by their grouping key.
#[derive(Default)]
pub struct DedupMap {
    groups: HashMap<GroupingKey, FindingGroup>,
}

impl DedupMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a finding occurrence.
    pub fn insert(
        &mut self,
        key: GroupingKey,
        occurrence: FindingOccurrence,
        signature: Option<CaseSignature>,
        plan: Option<CasePlan>,
    ) {
        let max_sigs = 16;

        match self.groups.get_mut(&key) {
            Some(group) => {
                group.occurrence_count += 1;
                if let Some(sig) = signature {
                    if group.representative_signatures.len() < max_sigs {
                        group.representative_signatures.push(sig);
                    }
                }
            }
            None => {
                let sigs = signature.into_iter().collect();
                self.groups.insert(
                    key.clone(),
                    FindingGroup {
                        key,
                        first_occurrence: occurrence,
                        occurrence_count: 1,
                        representative_signatures: sigs,
                        original_plan: plan,
                        minimized_plan: None,
                    },
                );
            }
        }
    }

    /// Get all finding groups.
    pub fn groups(&self) -> impl Iterator<Item = &FindingGroup> {
        self.groups.values()
    }

    /// Number of distinct finding groups.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key() -> GroupingKey {
        GroupingKey::OracleFailure {
            contract_fingerprint: [0u8; 32],
            kind: FindingKind::OracleFailure,
            step_id: "s1".into(),
            oracle_id: "o1".into(),
            violation_category: "conformance".into(),
        }
    }

    #[test]
    fn test_stable_grouping() {
        let mut map = DedupMap::new();
        let key = make_key();

        map.insert(
            key.clone(),
            FindingOccurrence {
                case_id: "case-1".into(),
                step_id: "s1".into(),
                oracle_id: Some("o1".into()),
                verdict: None,
            },
            None,
            None,
        );

        map.insert(
            key.clone(),
            FindingOccurrence {
                case_id: "case-2".into(),
                step_id: "s1".into(),
                oracle_id: Some("o1".into()),
                verdict: None,
            },
            None,
            None,
        );

        assert_eq!(map.len(), 1);
        let group = map.groups().next().unwrap();
        assert_eq!(group.occurrence_count, 2);
    }

    #[test]
    fn test_distinct_categories() {
        let mut map = DedupMap::new();

        map.insert(
            GroupingKey::OracleFailure {
                contract_fingerprint: [0u8; 32],
                kind: FindingKind::OracleFailure,
                step_id: "s1".into(),
                oracle_id: "o1".into(),
                violation_category: "conformance".into(),
            },
            FindingOccurrence {
                case_id: "c1".into(),
                step_id: "s1".into(),
                oracle_id: Some("o1".into()),
                verdict: None,
            },
            None,
            None,
        );

        map.insert(
            GroupingKey::OracleFailure {
                contract_fingerprint: [0u8; 32],
                kind: FindingKind::OracleFailure,
                step_id: "s2".into(),
                oracle_id: "o2".into(),
                violation_category: "security".into(),
            },
            FindingOccurrence {
                case_id: "c2".into(),
                step_id: "s2".into(),
                oracle_id: Some("o2".into()),
                verdict: None,
            },
            None,
            None,
        );

        assert_eq!(map.len(), 2);
    }
}
