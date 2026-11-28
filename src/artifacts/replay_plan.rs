use std::collections::BTreeMap;

use super::{CaseRecord, EnvFileRef, EnvarRef, FuzzCaseRef, MalformRef, RegressionPlan};
use crate::experiment::types::JsonExample;
use crate::mutation::{text::CorpusCaseRef, RegressionVariant};

#[derive(Debug)]
pub(crate) struct ReplayPlan {
    pub version: u32,
    pub experiment_sha256: String,
    pub openapi_fingerprints: BTreeMap<String, String>,
    pub case: SavedCase,
}

#[derive(Debug)]
pub(crate) enum SavedCase {
    Baseline,
    Variant(RegressionVariant),
    Example(SavedExample),
    Fuzz(FuzzCaseRef),
    Corpus(CorpusCaseRef),
    Malform(MalformRef),
    Envar(EnvarRef),
    EnvFile(EnvFileRef),
}

#[derive(Debug)]
pub(crate) enum SavedExample {
    Ordinary {
        id: String,
        step_id: Option<String>,
    },
    Json {
        id: String,
        step_id: String,
        action: JsonExample,
    },
}

impl SavedCase {
    fn kind(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Variant(_) => "variant",
            Self::Example(SavedExample::Ordinary { .. }) => "example",
            Self::Example(SavedExample::Json { .. }) => "JSON example",
            Self::Fuzz(_) => "fuzz",
            Self::Corpus(_) => "corpus",
            Self::Malform(_) => "malform",
            Self::Envar(_) => "envar",
            Self::EnvFile(_) => "envfile",
        }
    }
}

impl CaseRecord {
    pub(crate) fn into_replay_plan(self) -> Result<ReplayPlan, String> {
        let version = self.artifact_version;
        if !matches!(
            version,
            1 | Self::CURRENT_VERSION
                | Self::CORPUS_VERSION
                | Self::JSON_EXAMPLE_VERSION
                | Self::MALFORM_VERSION
                | Self::ENVAR_VERSION
                | Self::ENVFILE_VERSION
        ) {
            return Err(format!("unsupported case record version {version}"));
        }

        let RegressionPlan {
            experiment_sha256,
            openapi_fingerprints,
            variant,
            example_id,
            example_step_id,
            fuzz,
            corpus,
            json_example,
            malform,
            envar,
            envfile,
        } = self
            .regression_plan
            .ok_or("record has no replayable regression plan")?;

        for (required_version, present, error) in [
            (
                Self::ENVAR_VERSION,
                envar.is_some(),
                "invalid envar case record version",
            ),
            (
                Self::ENVFILE_VERSION,
                envfile.is_some(),
                "invalid envfile case record version",
            ),
            (
                Self::MALFORM_VERSION,
                malform.is_some(),
                "invalid malform case record version",
            ),
            (
                Self::CORPUS_VERSION,
                corpus.is_some(),
                "invalid corpus case record version",
            ),
            (
                Self::JSON_EXAMPLE_VERSION,
                json_example.is_some(),
                "invalid JSON example case record",
            ),
        ] {
            if (version == required_version) != present {
                return Err(error.into());
            }
        }

        let example = match (example_id, example_step_id, json_example) {
            (Some(id), Some(step_id), Some(action)) => Some(SavedExample::Json {
                id,
                step_id,
                action,
            }),
            (_, _, Some(_)) => return Err("invalid JSON example case record".into()),
            (Some(id), step_id, None) => Some(SavedExample::Ordinary { id, step_id }),
            (None, Some(_), None) => {
                return Err("saved case has an example step ID without an example ID".into());
            }
            (None, None, None) => None,
        };

        let mut cases = [
            variant.map(SavedCase::Variant),
            example.map(SavedCase::Example),
            fuzz.map(SavedCase::Fuzz),
            corpus.map(SavedCase::Corpus),
            malform.map(SavedCase::Malform),
            envar.map(SavedCase::Envar),
            envfile.map(SavedCase::EnvFile),
        ]
        .into_iter()
        .flatten();
        let case = cases.next().unwrap_or(SavedCase::Baseline);
        if let Some(other) = cases.next() {
            let kinds = std::iter::once(case.kind())
                .chain(std::iter::once(other.kind()))
                .chain(cases.map(|case| case.kind()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("saved case has conflicting case kinds: {kinds}"));
        }

        Ok(ReplayPlan {
            version,
            experiment_sha256,
            openapi_fingerprints,
            case,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn decode(version: u32, plan: Value) -> Result<ReplayPlan, String> {
        let mut record = json!({
            "artifact_version": version,
            "case_id": "saved",
            "case_number": 0,
            "step_results": [],
            "coverage_snapshot_sha256": null
        });
        if let Value::Object(mut fields) = plan {
            fields.insert("experiment_sha256".into(), json!("saved-hash"));
            record["regression_plan"] = Value::Object(fields);
        }
        serde_json::from_value::<CaseRecord>(record)
            .unwrap()
            .into_replay_plan()
    }

    fn versioned_cases() -> [(u32, Value, &'static str, &'static str); 5] {
        [
            (
                3,
                json!({"corpus": {
                    "step_id": "step", "field": "name", "corpus_index": 0,
                    "iteration": 0, "line": 1, "sha256": "file-hash"
                }}),
                "corpus",
                "invalid corpus case record version",
            ),
            (
                4,
                json!({
                    "example_id": "example", "example_step_id": "step",
                    "json_example": {"kind": "rename", "key": "old", "destination": "new"}
                }),
                "JSON example",
                "invalid JSON example case record",
            ),
            (
                5,
                json!({"malform": {"step_id": "step", "name": "case"}}),
                "malform",
                "invalid malform case record version",
            ),
            (
                6,
                json!({"envar": {"step_id": "step", "variable": "VAR", "case": {"value": 0}}}),
                "envar",
                "invalid envar case record version",
            ),
            (
                7,
                json!({"envfile": {"step_id": "step", "name": "case"}}),
                "envfile",
                "invalid envfile case record version",
            ),
        ]
    }

    #[test]
    fn accepts_cases_from_all_supported_versions() {
        let ordinary_cases = [
            (
                1,
                json!({"variant": {"step_id": "step", "selector": "body.name", "choice": 0}}),
                "variant",
            ),
            (
                2,
                json!({"variant": {"step_id": "step", "selector": "body.name", "variant_id": {"kind": "empty"}}}),
                "variant",
            ),
            (2, json!({"variant": null}), "baseline"),
        ];
        let versioned = versioned_cases().map(|(version, fields, kind, _)| (version, fields, kind));
        for (version, fields, kind) in ordinary_cases.into_iter().chain(versioned) {
            let plan = decode(version, fields).unwrap();
            assert_eq!(plan.version, version);
            assert_eq!(plan.case.kind(), kind);
            assert_eq!(plan.experiment_sha256, "saved-hash");
            assert!(plan.openapi_fingerprints.is_empty());
        }
    }

    #[test]
    fn keeps_defaults_for_legacy_examples_and_random_cases() {
        let example = decode(1, json!({"example_id": "original"})).unwrap();
        assert!(matches!(
            example.case,
            SavedCase::Example(SavedExample::Ordinary { id, step_id: None }) if id == "original"
        ));
        let fuzz = decode(2, json!({"fuzz": {"seed": 12, "iteration": 2}})).unwrap();
        assert!(matches!(
            fuzz.case,
            SavedCase::Fuzz(FuzzCaseRef {
                seed: 12,
                iteration: 2,
                generator: 1,
                max_fields: None
            })
        ));
    }

    #[test]
    fn rejects_missing_plans_and_unsupported_versions() {
        assert_eq!(
            decode(2, Value::Null).unwrap_err(),
            "record has no replayable regression plan",
        );
        assert_eq!(
            decode(999, Value::Null).unwrap_err(),
            "unsupported case record version 999",
        );
    }

    #[test]
    fn rejects_missing_or_mismatched_version_fields() {
        for (version, fields, _, error) in versioned_cases() {
            assert_eq!(decode(version, json!({})).unwrap_err(), error);
            assert_eq!(decode(2, fields).unwrap_err(), error);
        }
    }

    #[test]
    fn rejects_incomplete_examples() {
        let (_, json_example, _, error) = &versioned_cases()[1];
        for missing in ["example_id", "example_step_id"] {
            let mut fields = json_example.clone();
            fields.as_object_mut().unwrap().remove(missing);
            assert_eq!(decode(4, fields).unwrap_err(), *error);
        }
        assert_eq!(
            decode(2, json!({"example_step_id": "step"})).unwrap_err(),
            "saved case has an example step ID without an example ID",
        );
    }

    #[test]
    fn rejects_multiple_case_kinds() {
        for (version, mut fields, kind, _) in versioned_cases() {
            fields["fuzz"] = json!({"seed": 12, "iteration": 2});
            let error = decode(version, fields).unwrap_err();
            assert_eq!(
                error
                    .strip_prefix("saved case has conflicting case kinds: ")
                    .unwrap()
                    .split(", ")
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from(["fuzz", kind]),
            );
        }
        let error = decode(2, json!({
            "variant": {"step_id": "step", "selector": "body.name", "variant_id": {"kind": "empty"}},
            "example_id": "example",
            "fuzz": {"seed": 12, "iteration": 2}
        })).unwrap_err();
        assert_eq!(
            error,
            "saved case has conflicting case kinds: variant, example, fuzz"
        );
    }
}
