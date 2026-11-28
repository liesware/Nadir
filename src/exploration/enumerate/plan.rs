use crate::adapters::{cli::envbreak, http_raw::Malformation};
use crate::artifacts::EnvarCase;
use crate::evaluation::case::{EvaluatedCase, ExecutedPlan};
use crate::execution::runner::{RunError, WorkflowRunner};
use crate::experiment::types::{
    envar_replacement, ExampleCase, Experiment, MutatorDecl, ReplacementValue, Step,
};
use crate::mutation::text::Corpus;
use crate::mutation::{CasePlan, PartDescriptor, RegressionVariant, VariantGenerator};

pub(super) struct DeterministicPlan<'a> {
    variants: Vec<CasePlan>,
    examples: Vec<(&'a str, &'a ExampleCase)>,
    corpora: Vec<(&'a str, usize, &'a Corpus)>,
    envfiles: Vec<envbreak::DeclaredCase<'a>>,
    malformations: Vec<(&'a str, Malformation)>,
    envars: Vec<(String, String, EnvarCase, Option<String>)>,
}

pub(super) enum PlannedCase<'a> {
    Variant(PartDescriptor, ReplacementValue),
    Example(&'a str, &'a ExampleCase),
    Corpus(&'a str, usize, &'a Corpus, u16),
    EnvFile(envbreak::DeclaredCase<'a>),
    Malform(&'a str, Malformation),
    Envar(String, String, EnvarCase, Option<String>),
}

impl<'a> DeterministicPlan<'a> {
    pub(super) fn new(
        experiment: &'a Experiment,
        baseline: impl Fn(&Step, &str) -> Option<ReplacementValue>,
    ) -> Self {
        let steps = &experiment.steps;
        let examples = steps
            .iter()
            .flat_map(|step| step.examples.iter().map(|case| (step.id.as_str(), case)))
            .collect();
        let corpora = steps
            .iter()
            .flat_map(|step| {
                step.corpora
                    .iter()
                    .enumerate()
                    .map(|(index, corpus)| (step.id.as_str(), index, corpus))
            })
            .collect();
        let malformations = steps
            .iter()
            .flat_map(|step| {
                step.mutators.iter().flat_map(|mutator| {
                    let cases = match mutator {
                        MutatorDecl::Malform { malformations, .. } => malformations.as_slice(),
                        _ => &[],
                    };
                    cases.iter().copied().map(|case| (step.id.as_str(), case))
                })
            })
            .collect();
        Self {
            variants: VariantGenerator::enumerate_with(experiment, baseline),
            examples,
            corpora,
            envfiles: envbreak::enumerate_cases(experiment),
            malformations,
            envars: envar_enumerated_cases(experiment),
        }
    }

    pub(super) fn planned_count(&self) -> usize {
        let corpus_cases = self
            .corpora
            .iter()
            .map(|(_, _, corpus)| usize::from(corpus.repetitions))
            .fold(0, usize::saturating_add);
        self.variants
            .len()
            .saturating_add(self.examples.len())
            .saturating_add(corpus_cases)
            .saturating_add(self.envfiles.len())
            .saturating_add(self.malformations.len())
            .saturating_add(self.envars.len())
    }

    pub(super) fn variants_exceed_slots(&self, slots: usize) -> bool {
        self.variants.len() > slots
    }

    pub(super) fn into_cases(self, slots: usize) -> impl Iterator<Item = PlannedCase<'a>> {
        let variants = self.variants.into_iter().take(slots).filter_map(|plan| {
            plan.descriptors
                .into_iter()
                .zip(plan.replacements)
                .find_map(|(descriptor, value)| Some(PlannedCase::Variant(descriptor, value?)))
        });
        let examples = self
            .examples
            .into_iter()
            .map(|(step, example)| PlannedCase::Example(step, example));
        let corpora = self.corpora.into_iter().flat_map(|(step, index, corpus)| {
            (0..corpus.repetitions)
                .map(move |iteration| PlannedCase::Corpus(step, index, corpus, iteration))
        });
        variants
            .chain(examples)
            .chain(corpora)
            .chain(self.envfiles.into_iter().map(PlannedCase::EnvFile))
            .chain(
                self.malformations
                    .into_iter()
                    .map(|(step, case)| PlannedCase::Malform(step, case)),
            )
            .chain(
                self.envars
                    .into_iter()
                    .map(|(step, name, case, value)| PlannedCase::Envar(step, name, case, value)),
            )
    }
}

impl PlannedCase<'_> {
    pub(super) fn execute(
        self,
        runner: &mut WorkflowRunner,
        experiment: &Experiment,
    ) -> Result<EvaluatedCase, RunError> {
        let (execution, plan) = match self {
            Self::Variant(descriptor, value) => {
                let replay = RegressionVariant::from_replacement(experiment, &descriptor, &value);
                let execution = runner.execute_variant(
                    &descriptor.step_id,
                    &descriptor.canonical_selector,
                    value.clone(),
                )?;
                let plan = ExecutedPlan::Variant {
                    step_id: descriptor.step_id,
                    selector: descriptor.canonical_selector,
                    replacement: value,
                    replay,
                };
                (execution, plan)
            }
            Self::Example(step_id, example) => {
                let execution = runner.execute_example(step_id, example)?;
                let plan = ExecutedPlan::Example {
                    example_id: example.id.clone(),
                    step_id: step_id.to_owned(),
                    expect_status: example.expect_status,
                    json_example: example.json_example().cloned(),
                };
                (execution, plan)
            }
            Self::Corpus(step_id, index, corpus, iteration) => {
                let (reference, value) = corpus.case(step_id, index, iteration);
                let selector = format!("body.{}", reference.field);
                let execution = runner.execute_variant(step_id, &selector, value)?;
                (execution, ExecutedPlan::Corpus(reference))
            }
            Self::EnvFile(declaration) => {
                let (execution, evidence) = runner.execute_envfile(declaration)?;
                return Ok(EvaluatedCase::new_envfile(
                    experiment,
                    execution,
                    evidence,
                    &runner.schema_cache,
                ));
            }
            Self::Malform(step_id, malformation) => {
                let execution = runner.execute_malform(step_id, malformation)?;
                let plan = ExecutedPlan::Malform {
                    step_id: step_id.to_owned(),
                    malformation,
                };
                (execution, plan)
            }
            Self::Envar(step_id, variable, case, value) => {
                let execution =
                    runner.execute_variant(&step_id, &variable, envar_replacement(&value))?;
                let plan = ExecutedPlan::Envar {
                    step_id,
                    variable,
                    case,
                    value,
                };
                (execution, plan)
            }
        };
        Ok(EvaluatedCase::new(
            experiment,
            execution,
            plan,
            &runner.schema_cache,
        ))
    }
}

/// Every enumerated `envar` case, `(step, variable, case, value)`: each
/// `value`/`values` entry, then each invalid value of each `mode: spec` rule.
pub(crate) fn envar_enumerated_cases(
    experiment: &Experiment,
) -> Vec<(String, String, crate::artifacts::EnvarCase, Option<String>)> {
    use crate::experiment::types::{EnvarVariableRule, MutatorDecl};
    let mut cases = Vec::new();
    for step in &experiment.steps {
        for mutator in &step.mutators {
            let MutatorDecl::Envar { variables, .. } = mutator else {
                continue;
            };
            for var in variables {
                let mut push = |case, value: Option<String>| {
                    cases.push((step.id.clone(), var.name.clone(), case, value))
                };
                match &var.rule {
                    EnvarVariableRule::Fixed(value) => {
                        push(EnvarCase::Value(0), Some(value.clone()))
                    }
                    EnvarVariableRule::Values(values) => {
                        for (index, value) in values.iter().enumerate() {
                            push(EnvarCase::Value(index), Some(value.clone()));
                        }
                    }
                    EnvarVariableRule::Spec(rule) => {
                        for (name, value) in crate::adapters::cli::spec::cases_for(
                            &var.name,
                            rule,
                            step.env_config.as_ref(),
                        ) {
                            match value {
                                Some(value) => push(EnvarCase::Spec(name), Some(value)),
                                None => push(EnvarCase::Unset, None),
                            }
                        }
                    }
                    EnvarVariableRule::File { sha256, lines, .. } => {
                        for (index, value) in lines.iter().enumerate() {
                            push(
                                EnvarCase::Line {
                                    line: index + 1,
                                    sha256: sha256.clone(),
                                },
                                Some(value.clone()),
                            );
                        }
                    }
                    EnvarVariableRule::Absent => push(EnvarCase::Unset, None),
                    EnvarVariableRule::Catalog { unconstrained } => {
                        for (name, value) in crate::mutation::envar_catalog(*unconstrained) {
                            push(EnvarCase::Catalog(name), Some(value));
                        }
                    }
                    // Generative modes are handled by the fuzz phase.
                    EnvarVariableRule::Mode(_) | EnvarVariableRule::Text(_) => {}
                }
            }
        }
    }
    cases
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn plans_all_groups_in_existing_case_order() {
        let root = std::env::temp_dir().join(format!("nadir-plan-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let directory = TestDirectory(root);
        let root = &directory.0;
        fs::write(root.join("values.txt"), "Second\nFirst\n").unwrap();
        fs::write(root.join("corpus.txt"), "One\nTwo\n").unwrap();
        fs::write(root.join("settings.env"), "LABEL=baseline\n").unwrap();
        fs::write(
            root.join("openapi.yaml"),
            r#"
openapi: '3.1.0'
paths:
  /item:
    post:
      operationId: item
      requestBody:
        content:
          application/json:
            schema: {type: object}
      responses:
        '200': {description: OK}
"#,
        )
        .unwrap();
        let path = root.join("experiment.yaml");
        fs::write(
            &path,
            r#"
version: 1
experiment:
  name: deterministic-order
  max_cases: 20
  protocols:
    http: {base_url: 'http://127.0.0.1:8080', openapi: openapi.yaml}
  steps:
    - id: http
      protocol: http
      operation_id: item
      body_example: {title: baseline}
      cases:
        mutators:
          - {use: json, fields: [title], path: values.txt}
          - {use: file, fields: [title], path: corpus.txt, repetitions: 2}
        examples:
          - {id: second, field: title, value: Second, expect_status: 200}
          - {id: first, field: title, value: First, expect_status: 200}
    - id: wire
      protocol: http_raw
      operation_id: item
      cases:
        mutators:
          - {use: malform, only: [short_length, bare_lf]}
    - id: cli
      protocol: cli
      command: [/bin/true]
      env: {LABEL: baseline}
      workdir: {copy: [settings.env]}
      cases:
        mutators:
          - {use: envar, variables: [LABEL], values: [Second, First]}
          - {use: envfile, file: settings.env, only: [no_equals, duplicate_same]}
"#,
        )
        .unwrap();
        let configuration = crate::config::load_resolved(&path).unwrap();
        let experiment = &configuration.experiment;
        let plan = DeterministicPlan::new(experiment, |_, _| None);
        assert_eq!(plan.planned_count(), 12);
        assert!(!plan.variants_exceed_slots(2));
        assert!(plan.variants_exceed_slots(1));

        let cases: Vec<_> = plan
            .into_cases(2)
            .map(|case| match case {
                PlannedCase::Variant(_, ReplacementValue::JsonString(value)) => {
                    format!("variant {value}")
                }
                PlannedCase::Variant(_, value) => panic!("unexpected variant {value:?}"),
                PlannedCase::Example(_, example) => format!("example {}", example.id),
                PlannedCase::Corpus(_, index, _, iteration) => {
                    format!("corpus {index}/{iteration}")
                }
                PlannedCase::EnvFile(case) => format!("envfile {}", case.prepare().evidence().name),
                PlannedCase::Malform(_, case) => format!("malform {}", case.name()),
                PlannedCase::Envar(_, _, EnvarCase::Value(index), Some(value)) => {
                    format!("envar {index}/{value}")
                }
                PlannedCase::Envar(_, _, case, value) => {
                    panic!("unexpected envar {case:?} {value:?}")
                }
            })
            .collect();
        assert_eq!(
            cases,
            [
                "variant Second",
                "variant First",
                "example second",
                "example first",
                "corpus 0/0",
                "corpus 0/1",
                "envfile duplicate_same",
                "envfile no_equals",
                "malform short_length",
                "malform bare_lf",
                "envar 0/Second",
                "envar 1/First",
            ],
        );
    }
}
