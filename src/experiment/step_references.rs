use std::cmp::Ordering;
use std::collections::HashMap;

use crate::config::RawStep;

pub(crate) struct StepReferences<'a> {
    steps: HashMap<&'a str, (usize, &'a RawStep)>,
}

pub(crate) enum StepReferenceError {
    Unknown,
    Forward,
}

impl<'a> StepReferences<'a> {
    pub(crate) fn new(steps: &'a [RawStep]) -> Self {
        Self {
            steps: steps
                .iter()
                .enumerate()
                .map(|(position, step)| (step.id.as_str(), (position, step)))
                .collect(),
        }
    }

    pub(crate) fn contains(&self, step: &str) -> bool {
        self.steps.contains_key(step)
    }

    pub(crate) fn check(
        &self,
        current: usize,
        referenced: &str,
        allow_current: bool,
    ) -> Result<(Ordering, &'a RawStep), StepReferenceError> {
        let (position, step) = self
            .steps
            .get(referenced)
            .ok_or(StepReferenceError::Unknown)?;
        let relation = position.cmp(&current);
        match relation {
            Ordering::Less => Ok((relation, step)),
            Ordering::Equal if allow_current => Ok((relation, step)),
            _ => Err(StepReferenceError::Forward),
        }
    }

    pub(crate) fn declares_output(&self, step: &str, output: &str) -> bool {
        self.steps
            .get(step)
            .and_then(|(_, step)| step.outputs.as_ref())
            .is_some_and(|outputs| outputs.contains_key(output))
    }
}
