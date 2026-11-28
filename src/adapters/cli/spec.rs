//! `envar` `mode: spec`: a service's documented environment variables and,
//! for each, named values that break its rule.
//!
//! # Design notes
//!
//! **Chosen:** the specification is a small YAML file written from the
//! service's documentation, `VARIABLE: { <type>: <options>, used_when: … }`.
//! Each type has a fixed catalog of invalid values with stable names, like
//! `mode: schema` for JSON fields, so a saved case names what it sent and
//! `replay` rebuilds it. Textual values carry a `nadir-` marker so they can
//! be searched for in the program's output without false matches.

use std::path::Path;

use serde_yaml::Value;

/// One variable's rule.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecRule {
    pub kind: SpecType,
    /// The variable is only read when these other variables have these values.
    pub used_when: Vec<(String, String)>,
    /// Catalog cases the documentation allows (for example `empty` when an
    /// empty value means the default).
    pub skip: Vec<String>,
    /// No default: without the variable the program must refuse to start.
    /// Otherwise an `unset` case expects it to start with its default.
    pub required: bool,
}

/// The case that removes the variable from the environment. It only runs
/// when the base environment sets the variable (otherwise it is the base).
pub const UNSET_CASE: &str = "unset";

/// A file case built from the variable's own file in the working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepared {
    /// The file with this mode (`max_mode` plus one forbidden bit).
    Mode(u32),
    /// The file padded with newlines past `max_size`.
    Size(u64),
}

/// Group/other read and write bits, with their case names.
const PERMISSION_BITS: &[(&str, u32)] = &[
    ("group_readable", 0o040),
    ("other_readable", 0o004),
    ("group_writable", 0o020),
    ("other_writable", 0o002),
];

/// The prepared cases of a rule: a permission case per bit beyond
/// `max_mode`, and `too_large` with `max_size`.
pub fn prepared(kind: &SpecType) -> Vec<(&'static str, Prepared)> {
    let SpecType::File { max_mode, max_size, .. } = kind else {
        return Vec::new();
    };
    let mut cases = Vec::new();
    if let Some(max_mode) = max_mode {
        for (name, bit) in PERMISSION_BITS {
            if max_mode & bit == 0 {
                cases.push((*name, Prepared::Mode((max_mode & 0o666) | bit)));
            }
        }
    }
    if let Some(size) = max_size {
        cases.push(("too_large", Prepared::Size(*size)));
    }
    cases
}

/// Where a prepared case's file goes in the working directory.
pub fn prepared_path(variable: &str, case: &str) -> String {
    format!("nadir-files/{variable}/{case}")
}

/// Every case of `variable`, in order: the catalog, the prepared file cases
/// and `unset` (`None`: the variable removed). The prepared and `unset`
/// cases need the variable in the base environment: they start from its
/// file, or remove it.
pub fn cases_for(
    variable: &str,
    rule: &SpecRule,
    base_env: Option<&std::collections::HashMap<String, String>>,
) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> =
        cases(rule).into_iter().map(|(name, value)| (name.to_owned(), Some(value))).collect();
    if base_env.is_some_and(|env| env.contains_key(variable)) {
        for (name, _) in prepared(&rule.kind) {
            if !rule.skip.iter().any(|skipped| skipped == name) {
                out.push((name.to_owned(), Some(prepared_path(variable, name))));
            }
        }
    }
    if unset_applies(variable, rule, base_env) {
        out.push((UNSET_CASE.to_owned(), None));
    }
    out
}

/// Build the prepared files of `variable` in `workdir` from its file
/// (`base_value`, relative to the working directory). A missing file
/// leaves them out: those cases then find no file.
pub fn prepare(workdir: &Path, variable: &str, rule: &SpecRule, base_value: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let source = workdir.join(base_value);
    if !source.is_file() {
        return Ok(());
    }
    for (name, how) in prepared(&rule.kind) {
        let destination = workdir.join(prepared_path(variable, name));
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &destination)?;
        match how {
            Prepared::Mode(mode) => std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode))?,
            Prepared::Size(size) => {
                // Trailing newlines keep the content otherwise valid.
                let mut bytes = std::fs::read(&destination)?;
                let wanted = usize::try_from(size).unwrap_or(usize::MAX).saturating_add(1);
                if bytes.len() < wanted {
                    bytes.resize(wanted, b'\n');
                }
                std::fs::write(&destination, bytes)?;
            }
        }
    }
    Ok(())
}

/// Whether the `unset` case applies to `name` with this base environment.
pub fn unset_applies(name: &str, rule: &SpecRule, base_env: Option<&std::collections::HashMap<String, String>>) -> bool {
    base_env.is_some_and(|env| env.contains_key(name)) && !rule.skip.iter().any(|skipped| skipped == UNSET_CASE)
}

/// What a documented variable accepts.
#[derive(Debug, Clone, PartialEq)]
pub enum SpecType {
    Enum(Vec<String>),
    Bool,
    Int { min: Option<i64>, max: Option<i64> },
    /// `max_mode`: the most permissive mode the documentation allows (one
    /// case per group/other read or write bit beyond it); `max_size`: the
    /// largest size in bytes (a `too_large` case). Both are prepared from
    /// the variable's own file.
    File { readable: bool, writable: bool, max_mode: Option<u32>, max_size: Option<u64> },
    Dir,
    Filename,
    SocketAddr,
    HostPort,
    Url { schemes: Vec<String>, query: bool, fragment: bool },
    HttpPath,
    Hex { length: Option<usize> },
    Domain,
    Dsn { schemes: Vec<String> },
    Nonempty,
}

/// Files the working directory gets so `unreadable` and `regular_file`
/// cases point at something real.
pub const UNREADABLE_FILE: &str = "nadir-unreadable";
pub const REGULAR_FILE: &str = "nadir-file";

/// The `absolute` value of `filename`: a file in the system temp directory.
fn absolute_file() -> String {
    std::env::temp_dir().join("nadir-absolute-file").to_string_lossy().into_owned()
}

/// Load a specification, keeping the file's order.
pub fn load(path: &Path) -> Result<Vec<(String, SpecRule)>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<(String, SpecRule)>, String> {
    let document: Value = serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    let Value::Mapping(entries) = document else {
        return Err("expected a map of VARIABLE: rule".into());
    };
    entries
        .into_iter()
        .map(|(name, rule)| {
            let name = name.as_str().ok_or("variable names must be strings")?.to_owned();
            let rule = parse_rule(&rule).map_err(|error| format!("{name}: {error}"))?;
            Ok((name, rule))
        })
        .collect()
}

fn parse_rule(rule: &Value) -> Result<SpecRule, String> {
    let Value::Mapping(fields) = rule else {
        return Err("expected { <type>: <options> }".into());
    };
    let mut kind = None;
    let mut used_when = Vec::new();
    let mut skip = Vec::new();
    let mut required = false;
    for (key, options) in fields {
        let key = key.as_str().ok_or("keys must be strings")?;
        if key == "required" {
            required = options.as_bool().ok_or("required must be true or false")?;
            continue;
        }
        if key == "skip" {
            let Value::Sequence(names) = options else {
                return Err("skip must list case names".into());
            };
            skip = names.iter().map(scalar).collect::<Result<_, _>>()?;
            continue;
        }
        if key == "used_when" {
            let Value::Mapping(conditions) = options else {
                return Err("used_when must be a map of VARIABLE: value".into());
            };
            for (variable, value) in conditions {
                used_when.push((scalar(variable)?, scalar(value)?));
            }
            continue;
        }
        if kind.is_some() {
            return Err(format!("more than one type (found '{key}')"));
        }
        kind = Some(parse_type(key, options)?);
    }
    let kind = kind.ok_or("missing type (enum, bool, int, file, dir, filename, socket_addr, host_port, url, http_path, hex, domain, dsn, nonempty)")?;
    let mut all: Vec<&str> = cases(&SpecRule { kind: kind.clone(), used_when: Vec::new(), skip: Vec::new(), required })
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    all.push(UNSET_CASE);
    all.extend(prepared(&kind).into_iter().map(|(name, _)| name));
    if let Some(unknown) = skip.iter().find(|name| !all.contains(&name.as_str())) {
        return Err(format!("skip: '{unknown}' is not a case of this type (cases: {})", all.join(", ")));
    }
    Ok(SpecRule { kind, used_when, skip, required })
}

fn scalar(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        _ => Err("expected a text, number or boolean".into()),
    }
}

fn option<'a>(options: &'a Value, key: &str) -> Option<&'a Value> {
    options.as_mapping()?.get(Value::String(key.into()))
}

fn flag(options: &Value, key: &str, default: bool) -> Result<bool, String> {
    match option(options, key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("'{key}' must be true or false")),
    }
}

fn texts(options: &Value, key: &str) -> Result<Vec<String>, String> {
    match option(options, key) {
        None => Ok(Vec::new()),
        Some(Value::Sequence(items)) => items.iter().map(scalar).collect(),
        Some(_) => Err(format!("'{key}' must be a list")),
    }
}

fn number(options: &Value, key: &str) -> Result<Option<i64>, String> {
    match option(options, key) {
        None => Ok(None),
        Some(value) => value.as_i64().map(Some).ok_or_else(|| format!("'{key}' must be an integer")),
    }
}

fn parse_type(key: &str, options: &Value) -> Result<SpecType, String> {
    let known_options = |allowed: &[&str]| -> Result<(), String> {
        if let Some(fields) = options.as_mapping() {
            for name in fields.keys() {
                let name = name.as_str().unwrap_or_default();
                if !allowed.contains(&name) {
                    return Err(format!("{key}: unknown option '{name}'"));
                }
            }
        }
        Ok(())
    };
    Ok(match key {
        "enum" => {
            let Value::Sequence(values) = options else {
                return Err("enum must list its values".into());
            };
            let values: Vec<String> = values.iter().map(scalar).collect::<Result<_, _>>()?;
            if values.is_empty() {
                return Err("enum must list at least one value".into());
            }
            SpecType::Enum(values)
        }
        "int" => {
            known_options(&["min", "max"])?;
            SpecType::Int { min: number(options, "min")?, max: number(options, "max")? }
        }
        "file" => {
            known_options(&["readable", "writable", "max_mode", "max_size"])?;
            let max_mode = match option(options, "max_mode") {
                None => None,
                // "0600" (or 600): octal digits.
                Some(value) => {
                    let text = match value {
                        Value::String(text) => text.clone(),
                        Value::Number(number) => number.to_string(),
                        _ => String::new(),
                    };
                    let mode = u32::from_str_radix(text.trim_start_matches("0o"), 8)
                        .ok()
                        .filter(|mode| *mode <= 0o777)
                        .ok_or("file: max_mode must be an octal mode such as \"0600\"")?;
                    Some(mode)
                }
            };
            let max_size = match number(options, "max_size")? {
                None => None,
                Some(size) if size > 0 => Some(size as u64),
                Some(_) => return Err("file: max_size must be positive".into()),
            };
            SpecType::File {
                readable: flag(options, "readable", true)?,
                writable: flag(options, "writable", false)?,
                max_mode,
                max_size,
            }
        }
        "url" => {
            known_options(&["schemes", "query", "fragment"])?;
            let mut schemes = texts(options, "schemes")?;
            if schemes.is_empty() {
                schemes = vec!["http".into(), "https".into()];
            }
            SpecType::Url { schemes, query: flag(options, "query", true)?, fragment: flag(options, "fragment", true)? }
        }
        "hex" => {
            known_options(&["length"])?;
            SpecType::Hex { length: number(options, "length")?.map(|length| length.max(0) as usize) }
        }
        "dsn" => {
            known_options(&["schemes"])?;
            SpecType::Dsn { schemes: texts(options, "schemes")? }
        }
        simple @ ("bool" | "dir" | "filename" | "socket_addr" | "host_port" | "http_path" | "domain" | "nonempty") => {
            known_options(&[])?;
            match simple {
                "bool" => SpecType::Bool,
                "dir" => SpecType::Dir,
                "filename" => SpecType::Filename,
                "socket_addr" => SpecType::SocketAddr,
                "host_port" => SpecType::HostPort,
                "http_path" => SpecType::HttpPath,
                "domain" => SpecType::Domain,
                _ => SpecType::Nonempty,
            }
        }
        other => return Err(format!("unknown type '{other}'")),
    })
}

/// The invalid values of a rule, `(stable name, value)`, in catalog order.
pub fn cases(rule: &SpecRule) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = vec![("empty", String::new())];
    let mut push = |name: &'static str, value: String| out.push((name, value));
    match &rule.kind {
        SpecType::Enum(values) => {
            push("other_word", "nadir-invalid".into());
            let first = &values[0];
            let upper = first.to_uppercase();
            if !values.contains(&upper) {
                push("uppercase", upper);
            }
            push("padded", format!(" {first} "));
        }
        SpecType::Bool => {
            push("uppercase", "TRUE".into());
            push("numeric", "1".into());
            push("word", "yes".into());
        }
        SpecType::Int { min, max } => {
            if let Some(min) = min {
                push("below_min", min.saturating_sub(1).to_string());
            }
            if let Some(max) = max {
                push("above_max", max.saturating_add(1).to_string());
            }
            if min.is_none_or(|min| min.saturating_sub(1) != -1) {
                push("negative", "-1".into());
            }
            push("fraction", "1.5".into());
            push("not_a_number", "nadir-nan".into());
            push("overflow", "99999999999999999999999".into());
        }
        SpecType::File { readable, .. } => {
            push("missing", "nadir-missing/data.file".into());
            push("directory", ".".into());
            if *readable {
                push("unreadable", UNREADABLE_FILE.into());
            }
        }
        SpecType::Dir => {
            push("missing", "nadir-missing-dir".into());
            push("regular_file", REGULAR_FILE.into());
        }
        SpecType::Filename => {
            push("with_separator", "nadir/file".into());
            push("dot_dot", "..".into());
            // A path the program could write if it joins without checking:
            // an absolute path replaces the base directory. It points into
            // the system temp directory, so an accepted value stays harmless.
            push("absolute", absolute_file());
        }
        SpecType::SocketAddr => {
            push("no_port", "127.0.0.1".into());
            push("port_out_of_range", "127.0.0.1:99999".into());
            push("not_an_address", "nadir-not-an-address".into());
        }
        SpecType::HostPort => {
            push("no_port", "nadir-host".into());
            push("port_out_of_range", "localhost:99999".into());
            push("not_an_address", "nadir host:80".into());
        }
        SpecType::Url { schemes, query, fragment } => {
            let scheme = schemes.first().map_or("http", String::as_str);
            push("no_scheme", "127.0.0.1:3000".into());
            push("wrong_scheme", "ftp://127.0.0.1:3000".into());
            if !query {
                push("with_query", format!("{scheme}://127.0.0.1:3000/?nadir=1"));
            }
            if !fragment {
                push("with_fragment", format!("{scheme}://127.0.0.1:3000/#nadir"));
            }
        }
        SpecType::HttpPath => {
            push("no_leading_slash", "nadir-path".into());
            push("with_space", "/nadir path".into());
        }
        SpecType::Hex { length } => {
            if let Some(length) = length.filter(|length| *length > 1) {
                push("short", "a".repeat(length - 1));
                push("long", "a".repeat(length + 1));
                push("not_hex", "z".repeat(length));
            } else {
                push("not_hex", "nadir-not-hex".into());
            }
        }
        SpecType::Domain => {
            push("with_space", "nadir host.local".into());
            push("too_long", format!("{}.local", "a".repeat(248)));
        }
        SpecType::Dsn { .. } => {
            push("wrong_scheme", "mysql://nadir:nadir@127.0.0.1:1/nadir".into());
            push("not_a_url", "nadir-not-a-dsn".into());
        }
        SpecType::Nonempty => {}
    }
    // Every documented value is text: bytes that are not UTF-8 break any type.
    out.push(("non_unicode", super::osvalue::non_unicode()));
    out.retain(|(name, _)| !rule.skip.iter().any(|skipped| skipped == name));
    out
}

/// The variables whose `used_when` the base environment satisfies, and the
/// skipped ones with the reason.
pub fn applicable<'a>(
    spec: &'a [(String, SpecRule)],
    base_env: &std::collections::HashMap<String, String>,
) -> (Vec<&'a (String, SpecRule)>, Vec<String>) {
    let mut used = Vec::new();
    let mut skipped = Vec::new();
    for entry in spec {
        let unmet: Vec<String> = entry
            .1
            .used_when
            .iter()
            .filter(|(variable, value)| base_env.get(variable) != Some(value))
            .map(|(variable, value)| format!("{variable}={value}"))
            .collect();
        if unmet.is_empty() {
            used.push(entry);
        } else {
            skipped.push(format!("{} (used when {})", entry.0, unmet.join(", ")));
        }
    }
    (used, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_types_options_and_conditions_in_file_order() {
        let spec = parse(r#"
MODE: { enum: [dev, prod] }
CERT: { file: { readable: true }, used_when: { MODE: prod } }
TIMEOUT: { int: { min: 1, max: 30000 } }
KEY: { hex: { length: 64 } }
URL: { url: { query: false, fragment: false } }
FLAG: { bool: {} }
"#).unwrap();
        let names: Vec<&str> = spec.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["MODE", "CERT", "TIMEOUT", "KEY", "URL", "FLAG"]);
        assert_eq!(spec[0].1.kind, SpecType::Enum(vec!["dev".into(), "prod".into()]));
        assert_eq!(spec[1].1.used_when, vec![("MODE".into(), "prod".into())]);
        assert_eq!(spec[2].1.kind, SpecType::Int { min: Some(1), max: Some(30000) });
        assert_eq!(spec[4].1.kind, SpecType::Url { schemes: vec!["http".into(), "https".into()], query: false, fragment: false });
    }

    #[test]
    fn rejects_bad_specifications() {
        assert!(parse("A: { nope: {} }").unwrap_err().contains("unknown type 'nope'"));
        assert!(parse("A: { enum: [] }").unwrap_err().contains("at least one value"));
        assert!(parse("A: { bool: {}, int: {} }").unwrap_err().contains("more than one type"));
        assert!(parse("A: { int: { mni: 1 } }").unwrap_err().contains("unknown option 'mni'"));
        assert!(parse("A: { used_when: { B: x } }").unwrap_err().contains("missing type"));
        assert!(parse("- A").unwrap_err().contains("expected a map"));
        assert!(parse("A: { bool: {}, skip: [nope] }").unwrap_err().contains("skip: 'nope' is not a case"));
    }

    #[test]
    fn catalogs_have_stable_names_and_concrete_values() {
        let rule = |kind| SpecRule { kind, used_when: Vec::new(), skip: Vec::new(), required: false };
        let names = |kind| cases(&rule(kind)).into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        let nonutf8 = super::super::osvalue::non_unicode();
        assert_eq!(
            cases(&rule(SpecType::Enum(vec!["dev".into(), "prod".into()]))),
            vec![
                ("empty", "".into()), ("other_word", "nadir-invalid".into()), ("uppercase", "DEV".into()),
                ("padded", " dev ".into()), ("non_unicode", nonutf8.clone()),
            ]
        );
        assert_eq!(
            cases(&rule(SpecType::Int { min: Some(1), max: Some(30000) })),
            vec![
                ("empty", "".into()), ("below_min", "0".into()), ("above_max", "30001".into()),
                ("negative", "-1".into()), ("fraction", "1.5".into()), ("not_a_number", "nadir-nan".into()),
                ("overflow", "99999999999999999999999".into()), ("non_unicode", nonutf8),
            ]
        );
        // `negative` is skipped when it equals `below_min`.
        assert!(!names(SpecType::Int { min: Some(0), max: None }).contains(&"negative"));
        assert_eq!(names(SpecType::Hex { length: Some(64) }), ["empty", "short", "long", "not_hex", "non_unicode"]);
        assert_eq!(cases(&rule(SpecType::Hex { length: Some(4) }))[1].1, "aaa");
        assert_eq!(
            names(SpecType::File { readable: true, writable: true, max_mode: None, max_size: None }),
            ["empty", "missing", "directory", "unreadable", "non_unicode"]
        );
        assert_eq!(
            names(SpecType::Url { schemes: vec!["http".into()], query: true, fragment: true }),
            ["empty", "no_scheme", "wrong_scheme", "non_unicode"]
        );
        assert_eq!(names(SpecType::Nonempty), ["empty", "non_unicode"]);
        let filename = cases(&rule(SpecType::Filename));
        assert_eq!(
            filename.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            ["empty", "with_separator", "dot_dot", "absolute", "non_unicode"]
        );
        assert!(Path::new(&filename[3].1).is_absolute() && filename[3].1.ends_with("nadir-absolute-file"), "{filename:?}");
        let lenient = parse("L: { enum: [info, warn], skip: [uppercase, padded, non_unicode] }").unwrap();
        assert_eq!(cases(&lenient[0].1).into_iter().map(|(name, _)| name).collect::<Vec<_>>(), ["empty", "other_word"]);
    }


    #[test]
    fn file_limits_prepare_permission_and_size_cases_from_the_variable_file() {
        use std::os::unix::fs::PermissionsExt;
        let spec = parse("KEY: { file: { max_mode: \"0600\", max_size: 16 } }\nPUB: { file: { max_mode: \"0755\" } }").unwrap();
        let names = |kind| prepared(kind).into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        assert_eq!(names(&spec[0].1.kind), ["group_readable", "other_readable", "group_writable", "other_writable", "too_large"]);
        assert_eq!(names(&spec[1].1.kind), ["group_writable", "other_writable"]);
        assert_eq!(prepared(&spec[0].1.kind)[0].1, Prepared::Mode(0o640));
        assert!(parse("K: { file: { max_mode: \"9\" } }").unwrap_err().contains("octal"));

        let base: std::collections::HashMap<String, String> = [("KEY".to_owned(), "key.txt".to_owned())].into();
        let cases = cases_for("KEY", &spec[0].1, Some(&base));
        assert!(cases.contains(&("too_large".into(), Some("nadir-files/KEY/too_large".into()))), "{cases:?}");
        assert_eq!(cases.last(), Some(&("unset".into(), None)));
        // Without the variable in the base there is no file to start from.
        assert!(!cases_for("KEY", &spec[0].1, None).iter().any(|(name, _)| name == "too_large"));

        let dir = std::env::temp_dir().join(format!("nadir-prepare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("key.txt"), "abc\n").unwrap();
        prepare(&dir, "KEY", &spec[0].1, "key.txt").unwrap();
        let mode = |case| std::fs::metadata(dir.join(prepared_path("KEY", case))).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode("group_readable"), mode("other_writable")), (0o640, 0o602));
        let large = std::fs::read(dir.join(prepared_path("KEY", "too_large"))).unwrap();
        assert_eq!((large.len(), &large[..4]), (17, &b"abc\n"[..]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unset_runs_only_for_variables_the_base_sets_unless_skipped() {
        let spec = parse("A: { bool: {}, required: true }\nB: { bool: {}, skip: [unset] }\nC: { bool: {} }").unwrap();
        assert!(spec[0].1.required && !spec[2].1.required);
        let base: std::collections::HashMap<String, String> = [("A", "true"), ("B", "true")]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        assert!(unset_applies("A", &spec[0].1, Some(&base)));
        assert!(!unset_applies("B", &spec[1].1, Some(&base)), "skipped");
        assert!(!unset_applies("C", &spec[2].1, Some(&base)), "not in the base: unset is the base");
        assert!(parse("A: { bool: {}, required: yes }").unwrap_err().contains("required must be true or false"));
    }

    #[test]
    fn used_when_skips_variables_the_base_does_not_read() {
        let spec = parse("MODE: { enum: [dev, prod] }\nCERT: { file: {}, used_when: { MODE: prod } }").unwrap();
        let base = std::collections::HashMap::from([("MODE".to_string(), "dev".to_string())]);
        let (used, skipped) = applicable(&spec, &base);
        assert_eq!(used.len(), 1);
        assert_eq!(skipped, ["CERT (used when MODE=prod)"]);
    }
}
