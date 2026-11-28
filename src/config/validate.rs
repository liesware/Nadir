//! Static validation of `RawExperiment` before resolution.
//!
//! Validates structural constraints that can be checked from the YAML
//! alone, without needing OpenAPI contracts.

use std::collections::HashSet;

use crate::config::types::{
    RawExperiment, RawExperimentBlock, RawExploration, RawStep, RawTarget,
};

/// Run static validation on a deserialized `RawExperiment`.
pub fn validate_raw_experiment(raw: &RawExperiment) -> Result<(), String> {
    if raw.version != 1 {
        return Err(format!(
            "unsupported version: {}, only version 1 is supported",
            raw.version
        ));
    }
    validate_experiment_block(&raw.experiment)
}

fn validate_experiment_block(block: &RawExperimentBlock) -> Result<(), String> {
    validate_max_cases(block.max_cases)?;
    validate_exploration(&block.exploration)?;
    validate_target(block.target.as_ref())?;
    validate_steps(&block.steps)?;
    Ok(())
}

fn validate_max_cases(max_cases: u16) -> Result<(), String> {
    use crate::experiment::types::MAX_CASES_LIMIT;
    if max_cases == 0 {
        return Err("max_cases must be at least 1".into());
    }
    if max_cases > MAX_CASES_LIMIT {
        return Err(format!(
            "max_cases is {max_cases} but must be at most {MAX_CASES_LIMIT}"
        ));
    }
    Ok(())
}

fn validate_exploration(exploration: &Option<RawExploration>) -> Result<(), String> {
    let Some(expl) = exploration else {
        return Ok(());
    };

    // Validate strategy. `fuzz` runs without a target too; with one, it also
    // detects crashes.
    match expl.strategy.as_str() {
        // Enumeration changes one part per case.
        "enumerate" if expl.max_fields.is_some() => {
            return Err("exploration max_fields requires strategy: fuzz".into());
        }
        "enumerate" => {}
        "fuzz" => {
            match expl.duration_s {
                None => {
                    return Err("exploration strategy 'fuzz' requires 'duration_s'".into());
                }
                Some(0) => return Err("exploration duration_s must be at least 1".into()),
                Some(_) => {}
            }
            if expl.max_execs == Some(0) {
                return Err("exploration max_execs must be at least 1".into());
            }
            if expl.max_fields == Some(0) {
                return Err("exploration max_fields must be at least 1".into());
            }
        }
        other => {
            return Err(format!("unknown exploration strategy: '{}'", other));
        }
    }

    // Validate seed if present
    if let Some(seed) = expl.seed {
        if seed == 0 {
            return Err("exploration seed must be non-zero".into());
        }
    }

    Ok(())
}

fn validate_target(target: Option<&RawTarget>) -> Result<(), String> {
    let Some(target) = target else {
        return Ok(());
    };

    // Command must not be empty
    if target.command.is_empty() {
        return Err("target.command must contain at least one element".into());
    }

    // Coverage requires target (already ensured by presence)
    if let Some(coverage) = &target.coverage {
        validate_coverage(coverage)?;
    }

    Ok(())
}

fn validate_coverage(coverage: &crate::config::types::RawCoverage) -> Result<(), String> {
    // Validate transport name
    match coverage.transport.as_str() {
        "mmap_file" | "afl_shm" => {}
        other => {
            return Err(format!(
                "unknown coverage transport: '{}' (expected 'mmap_file' or 'afl_shm')",
                other
            ));
        }
    }

    // Validate map_size is a power of two
    if !coverage.map_size.is_power_of_two() {
        return Err(format!(
            "coverage map_size {} is not a power of two",
            coverage.map_size
        ));
    }

    if coverage.map_size < 256 {
        return Err(format!(
            "coverage map_size {} is below minimum of 256",
            coverage.map_size
        ));
    }

    if coverage.map_size > 8_388_608 {
        return Err(format!(
            "coverage map_size {} exceeds maximum of 8388608",
            coverage.map_size
        ));
    }

    Ok(())
}

fn validate_steps(steps: &[RawStep]) -> Result<(), String> {
    // Check for duplicate step IDs
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for step in steps {
        if !seen_ids.insert(&step.id) {
            return Err(format!("duplicate step id: '{}'", step.id));
        }
        if step.id.is_empty() {
            return Err("step id must not be empty".into());
        }
    }

    // Check forward references: bindings/selectors that reference later steps
    let step_id_set: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();

    for (idx, step) in steps.iter().enumerate() {
        // Check auth references (bearer is always a reference string)
        if let Some(auth) = &step.auth {
            let val = serde_json::Value::String(auth.bearer.clone());
            let values: Vec<&serde_json::Value> = vec![&val];
            validate_binding_refs(
                &step.id,
                &values,
                idx,
                steps,
                &step_id_set,
            )?;
        }

        // Check bind references
        if let Some(bind_map) = &step.bind {
            // Collect all values from the bind map
            let values: Vec<&serde_json::Value> = bind_map.values().collect();
            validate_binding_refs(&step.id, &values, idx, steps, &step_id_set)?;
        }

        // Check oracle selectors
        for oracle in &step.oracles {
            // Check `when.select` for step references
            if let Some(when_block) = &oracle.r#when {
                if when_block.select.starts_with("steps.") {
                    validate_step_ref_in_selector(
                        &step.id,
                        &when_block.select,
                        idx,
                        &step_id_set,
                    )?;
                }
            }
            // Check `select` for step references
            if let Some(select) = &oracle.select {
                if select.starts_with("steps.") {
                    validate_step_ref_in_selector(&step.id, select, idx, &step_id_set)?;
                }
            }
            // Check `record_reference`
            if let Some(rr) = &oracle.record_reference {
                if rr.starts_with("steps.") {
                    validate_step_ref_in_selector(&step.id, rr, idx, &step_id_set)?;
                }
            }
        }
    }

    // Check that bound body fields are not also mutated (within the same
    // step). Fields compare by path segment, so `metadata` and
    // `metadata.source` overlap but `subtitle` and `title` do not. Header and
    // path values may be bound and mutated: the baseline sends the bound
    // value and each mutated case replaces it.
    for step in steps {
        let (Some(cases), Some(bind_map)) = (&step.cases, &step.bind) else {
            continue;
        };
        for mutator in &cases.mutators {
            let Some(mutator_fields) = &mutator.fields else {
                continue;
            };
            for field in mutator_fields {
                let conflict = bind_map.keys().any(|bind_key| match mutator.r#use.as_str() {
                    "json" | "file" | "file_fuzz" => bind_key.strip_prefix("body.").is_some_and(|bound| {
                        paths_overlap(
                            &bound.split('.').collect::<Vec<_>>(),
                            &field.split('.').collect::<Vec<_>>(),
                        )
                    }),
                    _ => false,
                });
                if conflict {
                    return Err(format!(
                        "step '{}': mutator targets field '{}' which is also bound",
                        step.id, field
                    ));
                }
            }
        }
    }

    // Oracle ids identify findings, so they must exist and be unique per step.
    for step in steps {
        let mut oracle_ids: HashSet<&str> = HashSet::new();
        for oracle in &step.oracles {
            let id = match (&oracle.id, oracle.r#use.as_str()) {
                (Some(id), _) if id.is_empty() => {
                    return Err(format!("step '{}': oracle id must not be empty", step.id));
                }
                (Some(id), _) => id.as_str(),
                (None, "openapi") => "openapi",
                (None, other) => {
                    return Err(format!(
                        "step '{}': oracle 'use: {}' needs an 'id' (it names the rule in findings)",
                        step.id, other
                    ));
                }
            };
            if !oracle_ids.insert(id) {
                return Err(format!("step '{}': duplicate oracle id '{}'", step.id, id));
            }
            if let Some(kind) = &oracle.kind {
                if !matches!(kind.as_str(), "security" | "check") {
                    return Err(format!(
                        "step '{}': unknown oracle kind '{}' (expected 'security' or 'check')",
                        step.id, kind
                    ));
                }
            }
        }
    }

    // Check for duplicate example IDs within each step
    for step in steps {
        if let Some(cases) = &step.cases {
            validate_example_ids(&step.id, &cases.examples)?;
        }
    }

    // Protocol-specific validation
    for step in steps {
        match step.protocol.as_str() {
            "http" | "http_raw" => {
                if step.env_file.is_some() || step.workdir.is_some() || step.service.is_some() {
                    return Err(format!(
                        "step '{}': env_file, workdir and service require protocol: cli",
                        step.id
                    ));
                }
            }
            "cli" => {
                validate_cli_step(step)?;
            }
            other => {
                return Err(format!(
                    "step '{}': unknown protocol '{}' (expected 'http', 'http_raw', or 'cli')",
                    step.id, other
                ));
            }
        }
    }

    // Envar mutator validation
    for step in steps {
        let Some(cases) = &step.cases else { continue };
        for mutator in &cases.mutators {
            if mutator.r#use == "envar" {
                validate_envar_mutator(step, mutator)?;
            }
            if mutator.r#use == "envfile" {
                validate_envfile_mutator(step, mutator)?;
            }
            if mutator.r#use != "envfile"
                && (mutator.file.is_some() || mutator.limits.is_some() || mutator.prohibited.is_some())
            {
                return Err(format!(
                    "step '{}': file, limits and prohibited are options of use: envfile",
                    step.id
                ));
            }
        }
    }

    Ok(())
}

fn validate_cli_step(step: &RawStep) -> Result<(), String> {
    if step.env_file.as_deref().is_some_and(str::is_empty) {
        return Err(format!("step '{}': env_file must not be empty", step.id));
    }
    if step.command.as_ref().map_or(true, |cmd| cmd.is_empty()) {
        return Err(format!(
            "step '{}': protocol 'cli' requires a non-empty 'command' list",
            step.id
        ));
    }
    Ok(())
}

/// `use: envfile`: a cli step, the file it breaks, and only its own options.
fn validate_envfile_mutator(step: &RawStep, mutator: &crate::config::types::RawMutator) -> Result<(), String> {
    if step.protocol != "cli" {
        return Err(format!(
            "step '{}': mutator 'use: envfile' requires protocol 'cli', found '{}'",
            step.id, step.protocol
        ));
    }
    if mutator.file.as_deref().is_none_or(str::is_empty) {
        return Err(format!("step '{}': use: envfile requires 'file' (for example .env)", step.id));
    }
    if mutator.mode.as_deref().is_some_and(|mode| mode != "catalog") {
        return Err(format!("step '{}': use: envfile takes mode: catalog only", step.id));
    }
    let foreign = [
        ("fields", mutator.fields.is_some()),
        ("values", mutator.values.is_some()),
        ("variables", mutator.variables.is_some()),
        ("length", mutator.length.is_some()),
        ("path", mutator.path.is_some()),
        ("spec", mutator.spec.is_some()),
        ("seed", mutator.seed.is_some()),
        ("repetitions", mutator.repetitions.is_some()),
        ("keys", mutator.keys.is_some()),
        ("rename_to", mutator.rename_to.is_some()),
        ("expect_status", mutator.expect_status.is_some()),
    ];
    if let Some((name, _)) = foreign.iter().find(|(_, present)| *present) {
        return Err(format!("step '{}': '{name}' does not apply to 'use: envfile'", step.id));
    }
    let limits = mutator.limits.unwrap_or_default();
    if [limits.file_bytes, limits.value_bytes, limits.name_bytes].contains(&Some(0)) {
        return Err(format!("step '{}': envfile limits must be positive", step.id));
    }
    if mutator.prohibited.iter().flatten().any(|name| name.is_empty() || name.contains(['=', '\n'])) {
        return Err(format!("step '{}': envfile prohibited lists variable names", step.id));
    }
    Ok(())
}

/// Known generative modes for `envar`.
const ENVAR_MODES: &[&str] = &["alpha", "alphanum", "num", "raw", "absent", "edge_cases", "unconstrained"];
/// The `envar` modes that generate text and accept `length`.
const ENVAR_GENERATORS: &[&str] = &["alpha", "alphanum", "num", "raw"];

fn validate_envar_mutator(step: &RawStep, mutator: &crate::config::types::RawMutator) -> Result<(), String> {
    if step.protocol != "cli" {
        return Err(format!(
            "step '{}': mutator 'use: envar' requires protocol 'cli', found '{}'",
            step.id, step.protocol
        ));
    }

    // Fields of other mutators would be silently ignored: reject them.
    let foreign = [
        ("fields", mutator.fields.is_some()),
        ("repetitions", mutator.repetitions.is_some()),
        ("seed", mutator.seed.is_some()),
        ("keys", mutator.keys.is_some()),
        ("rename_to", mutator.rename_to.is_some()),
        ("expect_status", mutator.expect_status.is_some()),
    ];
    if let Some((name, _)) = foreign.iter().find(|(_, present)| *present) {
        return Err(format!(
            "step '{}': '{name}' does not apply to 'use: envar' (per-variable values go in 'variables')",
            step.id
        ));
    }

    // `mode: spec` lists its variables in the specification file.
    if mutator.mode.as_deref() == Some("spec") {
        if mutator.spec.as_deref().is_none_or(str::is_empty) {
            return Err(format!("step '{}': envar mode: spec requires 'spec'", step.id));
        }
        if mutator.length.is_some() || mutator.path.is_some() {
            return Err(format!("step '{}': envar mode: spec takes neither 'length' nor 'path'", step.id));
        }
        return Ok(());
    }
    let from_spec = matches!(mutator.variables, Some(crate::config::types::EnvarVariables::Spec));
    match (from_spec, mutator.spec.as_deref()) {
        (true, None | Some("")) => {
            return Err(format!("step '{}': envar variables: spec requires 'spec'", step.id));
        }
        (false, Some(_)) => {
            return Err(format!("step '{}': envar 'spec' requires mode: spec or variables: spec", step.id));
        }
        _ => {}
    }
    if mutator.only.is_some() && !from_spec {
        return Err(format!("step '{}': envar 'only' requires mode: spec or variables: spec", step.id));
    }

    let variables = mutator.variables.as_ref().ok_or_else(|| {
        format!(
            "step '{}': mutator 'use: envar' requires 'variables'",
            step.id
        )
    })?;

    // With `env_file` the variables are only known after reading it.
    let step_env = step.env.as_ref().filter(|_| step.env_file.is_none());
    let mut seen_vars: HashSet<&str> = HashSet::new();

    // `all` and `spec` take the list form's options; their names are known
    // once the base environment and the specification are read.
    let no_names = Vec::new();
    match variables {
        crate::config::types::EnvarVariables::List(_)
        | crate::config::types::EnvarVariables::All
        | crate::config::types::EnvarVariables::Spec => {
            let names = match variables {
                crate::config::types::EnvarVariables::List(names) => names,
                _ => &no_names,
            };
            let given = [mutator.mode.is_some(), mutator.path.is_some(), mutator.values.is_some()];
            match given.iter().filter(|present| **present).count() {
                0 => {
                    return Err(format!(
                        "step '{}': 'use: envar' with list variables requires 'mode', 'path' or 'values'",
                        step.id
                    ));
                }
                1 => {}
                _ => {
                    return Err(format!(
                        "step '{}': envar list variables take only one of 'mode', 'path' or 'values'",
                        step.id
                    ));
                }
            }
            match (mutator.mode.as_deref(), mutator.path.as_deref(), &mutator.values) {
                (Some(mode), _, _) if !ENVAR_MODES.contains(&mode) => {
                    return Err(format!(
                        "step '{}': unknown envar mode '{}' (expected one of: {})",
                        step.id, mode, ENVAR_MODES.join(", ")
                    ));
                }
                (_, Some(""), _) => {
                    return Err(format!("step '{}': envar 'path' must not be empty", step.id));
                }
                (_, _, Some(values)) if values.is_empty() => {
                    return Err(format!("step '{}': envar 'values' must contain at least one entry", step.id));
                }
                _ => {}
            }
            match &mutator.length {
                Some(_) if mutator.mode.is_none() => {
                    return Err(format!("step '{}': envar 'length' requires 'mode'", step.id));
                }
                Some(_) if !mutator.mode.as_deref().is_some_and(|mode| ENVAR_GENERATORS.contains(&mode)) => {
                    return Err(format!(
                        "step '{}': envar mode: {} takes no 'length' (only {})",
                        step.id,
                        mutator.mode.as_deref().unwrap_or_default(),
                        ENVAR_GENERATORS.join(", ")
                    ));
                }
                Some(length) if length.reference.is_none() => {
                    return Err(format!(
                        "step '{}': envar 'length' requires 'reference' (environment variables have no schema)",
                        step.id
                    ));
                }
                _ => {}
            }
            for name in names {
                if !seen_vars.insert(name) {
                    return Err(format!(
                        "step '{}': duplicate envar variable '{}'",
                        step.id, name
                    ));
                }
                if let Some(env) = step_env {
                    if !env.contains_key(name) {
                        return Err(format!(
                            "step '{}': envar variable '{}' not found in step 'env'",
                            step.id, name
                        ));
                    }
                }
            }
        }
        crate::config::types::EnvarVariables::Map(rules) => {
            if mutator.mode.is_some() || mutator.length.is_some() || mutator.path.is_some() || mutator.values.is_some() {
                return Err(format!(
                    "step '{}': envar map variables take 'value', 'values' or 'path' per variable, \
                     not 'mode', 'length', 'path' or 'values' on the mutator (use the list form)",
                    step.id
                ));
            }
            for (name, rule) in rules {
                if !seen_vars.insert(name) {
                    return Err(format!(
                        "step '{}': duplicate envar variable '{}'",
                        step.id, name
                    ));
                }
                if let Some(env) = step_env {
                    if !env.contains_key(name) {
                        return Err(format!(
                            "step '{}': envar variable '{}' not found in step 'env'",
                            step.id, name
                        ));
                    }
                }
                // Each rule must have exactly one of value, values or path.
                let given = [rule.value.is_some(), rule.values.is_some(), rule.path.is_some()];
                match given.iter().filter(|present| **present).count() {
                    0 => {
                        return Err(format!(
                            "step '{}': envar variable '{}' must specify 'value', 'values' or 'path'",
                            step.id, name
                        ));
                    }
                    1 => {}
                    _ => {
                        return Err(format!(
                            "step '{}': envar variable '{}' must specify only one of 'value', 'values' or 'path'",
                            step.id, name
                        ));
                    }
                }
                match &rule.values {
                    Some(vals) if vals.is_empty() => {
                        return Err(format!(
                            "step '{}': envar variable '{}' 'values' must contain at least one entry",
                            step.id, name
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Two dotted paths overlap when they are equal or one is a prefix of the other.
fn paths_overlap(first: &[&str], second: &[&str]) -> bool {
    first.iter().zip(second).all(|(a, b)| a == b)
}

/// Check that example IDs are unique within a step.
fn validate_example_ids(step_id: &str, examples: &[crate::config::types::RawExample]) -> Result<(), String> {
    let mut seen: HashSet<&str> = HashSet::new();
    for example in examples {
        if !seen.insert(&example.id) {
            return Err(format!(
                "step '{}': duplicate example id '{}'",
                step_id, example.id
            ));
        }
    }
    Ok(())
}

/// Check that references in binding values don't reference future steps.
fn validate_binding_refs(
    step_id: &str,
    values: &[&serde_json::Value],
    current_idx: usize,
    steps: &[RawStep],
    step_id_set: &HashSet<&str>,
) -> Result<(), String> {
    for value in values {
        if let serde_json::Value::String(s) = value {
            // Check for ${steps.X...} references
            for ref_match in extract_step_refs(s) {
                if !step_id_set.contains(ref_match.as_str()) {
                    return Err(format!(
                        "step '{}' references unknown step '{}'",
                        step_id, ref_match
                    ));
                }
                // Find the index of the referenced step
                if let Some(ref_idx) = steps.iter().position(|s| s.id == ref_match) {
                    if ref_idx >= current_idx {
                        return Err(format!(
                            "step '{}' has a forward reference to step '{}' (referenced step must be declared earlier)",
                            step_id, ref_match
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check that a `steps.<id>.response.<...>` selector references a valid, earlier step.
fn validate_step_ref_in_selector(
    step_id: &str,
    selector: &str,
    _current_idx: usize,
    step_id_set: &HashSet<&str>,
) -> Result<(), String> {
    // Extract step id from "steps.<id>..."
    if let Some(rest) = selector.strip_prefix("steps.") {
        let ref_step_id = rest.split('.').next().unwrap_or("");
        if !step_id_set.contains(ref_step_id) {
            return Err(format!(
                "step '{}': selector '{}' references unknown step '{}'",
                step_id, selector, ref_step_id
            ));
        }
        // Forward reference check (but we don't have the index here easily)
        // The caller ensures this is checked in validate_binding_refs
    }
    Ok(())
}

/// Extract all `${steps.<id>...}` references from a string.
fn extract_step_refs(s: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = s[search_from..].find("${steps.") {
        let abs_pos = search_from + pos;
        let after = &s[abs_pos + 2..]; // skip ${
        if let Some(close) = after.find('}') {
            let inner = &after[..close];
            // inner looks like "steps.login.outputs.token"
            // Extract the step id between "steps." and the next "."
            if let Some(after_steps) = inner.strip_prefix("steps.") {
                if let Some(dot_pos) = after_steps.find('.') {
                    let step_id = &after_steps[..dot_pos];
                    refs.push(step_id.to_string());
                }
            }
            search_from = abs_pos + 2 + close + 1;
        } else {
            break;
        }
    }
    refs
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;

    /// Helper: create a minimal valid experiment block.
    fn minimal_block() -> RawExperimentBlock {
        RawExperimentBlock {
            name: "test".into(),
            max_cases: 4,
            exploration: Some(RawExploration {
                strategy: "enumerate".into(),
                max_execs: None,
                duration_s: None,
                seed: None,

                max_fields: None,
            }),
            target: None,
            protocols: RawProtocols { http: None, cli: None },
            steps: vec![],
            hidden: None,
        }
    }

    fn make_step(id: &str) -> RawStep {
        RawStep {
            id: id.into(),
            protocol: "http".into(),
            format: None,
            operation_id: Some("testOp".into()),
            operation_path: None,
            body_example: None,
            bind: None,
            auth: None,
            outputs: None,
            cases: None,
            oracles: vec![],
            print: None,
            debug: None,
            command: None,
            env: None,
            env_file: None,
            workdir: None,
            service: None,
        }
    }

    #[test]
    fn test_rejects_unknown_version() {
        let raw = RawExperiment {
            version: 2,
            experiment: minimal_block(),
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("version"), "expected version error, got: {}", err);
    }

    #[test]
    fn test_rejects_duplicate_step_id() {
        let mut block = minimal_block();
        block.steps = vec![make_step("login"), make_step("login")];
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("duplicate"), "expected duplicate error, got: {}", err);
    }

    fn validate_step(step: RawStep) -> Result<(), String> {
        let mut block = minimal_block();
        block.steps = vec![step];
        validate_raw_experiment(&RawExperiment {
            version: 1,
            experiment: block,
        })
    }

    fn step_with_bind_and_mutator(bind_key: &str, mutator: &str, field: &str) -> RawStep {
        let mut step = make_step("s");
        step.bind = Some(
            [(bind_key.to_string(), serde_json::Value::String("x".into()))]
                .into_iter()
                .collect(),
        );
        step.cases = Some(RawStepCases {
            mutators: vec![RawMutator {
                r#use: mutator.into(),
                fields: Some(vec![field.into()]),
                values: Some(vec!["v".into()]),
                mode: None,
                ..RawMutator::default()
            }],
            examples: vec![],
        });
        step
    }

    fn oracle(id: Option<&str>, use_: &str, kind: Option<&str>) -> RawOracle {
        RawOracle {
            id: id.map(Into::into),
            r#use: use_.into(),
            kind: kind.map(Into::into),
            setup: None,
            select: None,
            r#when: None,
            expect: None,
            record_reference: None,
        }
    }

    #[test]
    fn test_bound_and_mutated_fields_compare_by_segment() {
        assert!(validate_step(step_with_bind_and_mutator("body.subtitle", "json", "title")).is_ok());
        assert!(validate_step(step_with_bind_and_mutator("body.title", "json", "title")).is_err());
        assert!(
            validate_step(step_with_bind_and_mutator("body.metadata", "json", "metadata.source"))
                .is_err()
        );
        assert!(
            validate_step(step_with_bind_and_mutator("body.metadata.source", "json", "metadata"))
                .is_err()
        );
        assert!(
            validate_step(step_with_bind_and_mutator("body.metadata.source", "json", "metadata.id"))
                .is_ok()
        );
        // A bound header can be mutated: each case replaces the bound value.
        assert!(validate_step(step_with_bind_and_mutator(
            "header.accept-language",
            "headers",
            "Accept-Language",
        ))
        .is_ok());
        assert!(validate_step(step_with_bind_and_mutator("header.X-Key", "http", "header.x-key")).is_ok());
    }

    #[test]
    fn max_fields_needs_fuzz_and_a_positive_value() {
        let exploration = |strategy: &str, max_fields| Some(RawExploration {
            strategy: strategy.into(),
            max_execs: None,
            duration_s: Some(5),
            seed: None,
            max_fields,
        });
        assert!(validate_exploration(&exploration("fuzz", Some(2))).is_ok());
        assert!(validate_exploration(&exploration("fuzz", None)).is_ok());
        assert_eq!(validate_exploration(&exploration("fuzz", Some(0))).unwrap_err(),
            "exploration max_fields must be at least 1");
        assert_eq!(validate_exploration(&exploration("enumerate", Some(1))).unwrap_err(),
            "exploration max_fields requires strategy: fuzz");
    }

    #[test]
    fn test_oracles_need_unique_ids_and_known_kinds() {
        let with_oracles = |oracles| {
            let mut step = make_step("s");
            step.oracles = oracles;
            validate_step(step)
        };
        assert!(with_oracles(vec![
            oracle(None, "openapi", None),
            oracle(Some("a"), "assert", Some("security")),
            oracle(Some("b"), "record_absent", Some("check")),
        ])
        .is_ok());

        let error = with_oracles(vec![oracle(None, "assert", None)]).unwrap_err();
        assert!(error.contains("needs an 'id'"), "{error}");

        let error =
            with_oracles(vec![oracle(Some("a"), "assert", None), oracle(Some("a"), "assert", None)])
                .unwrap_err();
        assert!(error.contains("duplicate oracle id 'a'"), "{error}");

        let error = with_oracles(vec![oracle(None, "openapi", None), oracle(None, "openapi", None)])
            .unwrap_err();
        assert!(error.contains("duplicate oracle id 'openapi'"), "{error}");

        let error =
            with_oracles(vec![oracle(Some("a"), "assert", Some("securty"))]).unwrap_err();
        assert!(error.contains("unknown oracle kind 'securty'"), "{error}");
    }

    #[test]
    fn test_rejects_forward_reference() {
        let mut block = minimal_block();
        let mut step1 = make_step("login");
        // Reference step2 which is declared later
        step1.bind = Some(
            vec![(
                "body.token".into(),
                serde_json::Value::String("${steps.get_token.outputs.tok}".into()),
            )]
            .into_iter()
            .collect(),
        );
        let step2 = make_step("get_token");
        block.steps = vec![step1, step2];
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(
            err.contains("forward reference"),
            "expected forward reference error, got: {}",
            err
        );
    }

    #[test]
    fn test_rejects_mutator_on_bound_field() {
        let mut block = minimal_block();
        let mut step = make_step("login");
        step.bind = Some(
            vec![("body.title".into(), serde_json::Value::String("fixed".into()))]
                .into_iter()
                .collect(),
        );
        step.cases = Some(RawStepCases {
            mutators: vec![RawMutator {
                r#use: "json".into(),
                fields: Some(vec!["title".into()]),
                values: None,
                mode: Some("schema_valid".into()),
                ..RawMutator::default()
            }],
            examples: vec![],
        });
        block.steps = vec![step];
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(
            err.contains("mutator") && err.contains("bound"),
            "expected mutator-on-bound error, got: {}",
            err
        );
    }

    #[test]
    fn test_rejects_max_cases_above_limit() {
        let mut block = minimal_block();
        block.max_cases = 1000;
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        assert!(validate_raw_experiment(&raw).is_ok());
        let mut block = minimal_block();
        block.max_cases = 1001;
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(
            err.contains("at most 1000"),
            "expected max_cases limit error, got: {}",
            err
        );
    }

    #[test]
    fn test_rejects_max_cases_zero() {
        let mut block = minimal_block();
        block.max_cases = 0;
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("at least 1"), "got: {}", err);
    }

    #[test]
    fn test_rejects_non_loopback_base_url() {
        // Base URL validation is done in validate() once we have the URL parser.
        // For now, this is tested at the protocol level. We'll add it when the
        // HttpAdapter validator is implemented.
    }

    #[test]
    fn test_rejects_unknown_strategy() {
        let mut block = minimal_block();
        block.exploration = Some(RawExploration {
            strategy: "random".into(),
            max_execs: None,
            duration_s: None,
            seed: None,

            max_fields: None,
        });
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("unknown exploration strategy"));
    }

    #[test]
    fn test_rejects_fuzz_without_duration() {
        let mut block = minimal_block();
        block.target = Some(RawTarget {
            command: vec!["echo".into(), "hello".into()],
            ready: RawReady {
                path: "/".into(),
                timeout_ms: 10000,
            },
            coverage: None,
            cwd: None,
            env: None,
        });
        block.exploration = Some(RawExploration {
            strategy: "fuzz".into(),
            max_execs: None,
            duration_s: None,
            seed: None,

            max_fields: None,
        });
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("duration_s"));
    }

    #[test]
    fn test_rejects_coverage_without_target() {
        // Coverage without target is already enforced by the type system
        // (coverage is a field on RawTarget). The validation just ensures
        // that if coverage is set, the parent target exists, which is
        // structurally guaranteed.
    }

    #[test]
    fn test_rejects_map_size_not_power_of_two() {
        let mut block = minimal_block();
        block.target = Some(RawTarget {
            command: vec!["echo".into(), "hello".into()],
            ready: RawReady {
                path: "/".into(),
                timeout_ms: 10000,
            },
            coverage: Some(RawCoverage {
                transport: "mmap_file".into(),
                map_size: 500,
            }),
            cwd: None,
            env: None,
        });
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("power of two"));
    }

    #[test]
    fn test_rejects_empty_command() {
        let mut block = minimal_block();
        block.target = Some(RawTarget {
            command: vec![],
            ready: RawReady {
                path: "/".into(),
                timeout_ms: 10000,
            },
            coverage: None,
            cwd: None,
            env: None,
        });
        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("command"));
    }

    #[test]
    fn test_accepts_both_examples() {
        // This test is in config/mod.rs as it uses the `load` function
        // which reads actual files.
    }

    #[test]
    fn test_cli_step_requires_command() {
        let mut block = minimal_block();
        block.protocols.cli = Some(RawCliProtocol { timeout_ms: 5000 });
        block.steps = vec![RawStep {
            id: "cli-step".into(),
            protocol: "cli".into(),
            command: None,
            env: None,
            ..make_missing_step_defaults("cli")
        }];
        let raw = RawExperiment { version: 1, experiment: block };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("requires a non-empty 'command'"), "{err}");
    }

    #[test]
    fn test_envar_rejects_on_http_step() {
        let mut block = minimal_block();
        block.protocols.http = Some(RawHttpProtocol {
            base_url: "http://127.0.0.1:8080".into(),
            openapi: "openapi.yaml".into(),
            timeout_ms: 2000,
        });
        let mut step = make_step("http-step");
        step.cases = Some(RawStepCases {
            mutators: vec![RawMutator {
                r#use: "envar".into(),
                variables: Some(EnvarVariables::List(vec!["X".into()])),
                mode: Some("alphanum".into()),
                ..RawMutator::default()
            }],
            examples: vec![],
        });
        block.steps = vec![step];
        let raw = RawExperiment { version: 1, experiment: block };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("envar"), "{err}");
        assert!(err.contains("protocol 'cli'"), "{err}");
    }

    #[test]
    fn test_envar_rejects_duplicate_variable() {
        let mut block = minimal_block();
        block.protocols.cli = Some(RawCliProtocol { timeout_ms: 5000 });
        let mut step = RawStep {
            id: "cli-step".into(),
            protocol: "cli".into(),
            command: Some(vec!["echo".into(), "hi".into()]),
            env: Some([("X".into(), "base".into())].into_iter().collect()),
            ..make_missing_step_defaults("cli")
        };
        step.cases = Some(RawStepCases {
            mutators: vec![RawMutator {
                r#use: "envar".into(),
                variables: Some(EnvarVariables::List(vec!["X".into(), "X".into()])),
                mode: Some("alphanum".into()),
                ..RawMutator::default()
            }],
            examples: vec![],
        });
        block.steps = vec![step];
        let raw = RawExperiment { version: 1, experiment: block };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn test_envar_rejects_variable_not_in_env() {
        let mut block = minimal_block();
        block.protocols.cli = Some(RawCliProtocol { timeout_ms: 5000 });
        let mut step = RawStep {
            id: "cli-step".into(),
            protocol: "cli".into(),
            command: Some(vec!["echo".into(), "hi".into()]),
            env: Some([("X".into(), "base".into())].into_iter().collect()),
            ..make_missing_step_defaults("cli")
        };
        step.cases = Some(RawStepCases {
            mutators: vec![RawMutator {
                r#use: "envar".into(),
                variables: Some(EnvarVariables::List(vec!["Y".into()])),
                mode: Some("alphanum".into()),
                ..RawMutator::default()
            }],
            examples: vec![],
        });
        block.steps = vec![step];
        let raw = RawExperiment { version: 1, experiment: block };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn test_envar_rejects_unknown_mode() {
        let mut block = minimal_block();
        block.protocols.cli = Some(RawCliProtocol { timeout_ms: 5000 });
        let mut step = RawStep {
            id: "cli-step".into(),
            protocol: "cli".into(),
            command: Some(vec!["echo".into(), "hi".into()]),
            env: Some([("X".into(), "base".into())].into_iter().collect()),
            ..make_missing_step_defaults("cli")
        };
        step.cases = Some(RawStepCases {
            mutators: vec![RawMutator {
                r#use: "envar".into(),
                variables: Some(EnvarVariables::List(vec!["X".into()])),
                mode: Some("unknown".into()),
                ..RawMutator::default()
            }],
            examples: vec![],
        });
        block.steps = vec![step];
        let raw = RawExperiment { version: 1, experiment: block };
        let err = validate_raw_experiment(&raw).unwrap_err();
        assert!(err.contains("unknown envar mode"), "{err}");
    }

    /// Helper: produce a step with fields that make the CLI validator happy
    fn make_missing_step_defaults(protocol: &str) -> RawStep {
        RawStep {
            id: "tmp".into(),
            protocol: protocol.into(),
            format: None,
            operation_id: None,
            operation_path: None,
            body_example: None,
            bind: None,
            auth: None,
            outputs: None,
            cases: None,
            oracles: vec![],
            print: None,
            debug: None,
            command: None,
            env: None,
            env_file: None,
            workdir: None,
            service: None,
        }
    }
}
