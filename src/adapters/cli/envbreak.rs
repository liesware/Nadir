//! `use: envfile`: one case per way to break a `.env`-style file that a cli
//! step copies into its working directory (`workdir.copy`).
//!
//! Each case starts from the file as copied and breaks it one way. Name
//! cases append a line; value cases change the value of the last
//! assignment; file cases change the whole file. The program must refuse
//! (`envfile:refuses`), say where (the line, or the file for file-level
//! problems) and never print the value: every injected value carries a
//! distinct `nadir-…` token to look for.

/// What a case does to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    DuplicateSame,
    DuplicateOther,
    UnknownName,
    ForeignName,
    LowercaseName,
    NoEquals,
    ExportPrefix,
    Prohibited,
    NulByte,
    LoneCr,
    UnclosedQuote,
    TextAfterQuote,
    InvalidUtf8,
    ValueTooLong,
    NameTooLong,
    FileTooLarge,
    Unreadable,
    Directory,
}

/// One named case of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileCase {
    /// Stable name, saved in case records (`prohibited:NAME` for each
    /// prohibited variable).
    pub name: String,
    kind: Kind,
    /// The variable of `prohibited`, or the limit of a size case.
    arg: Option<String>,
}

/// Documented size limits, in bytes. Each one adds its case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub file_bytes: Option<usize>,
    pub value_bytes: Option<usize>,
    pub name_bytes: Option<usize>,
}

/// The broken file as the working directory gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// These bytes; `unreadable` also removes every permission.
    Bytes { bytes: Vec<u8>, unreadable: bool },
    /// A directory where the file was.
    Directory,
}

/// Where the program's error should point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Line(usize),
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub content: Content,
    pub location: Location,
    /// The token the output must not contain, when a value was injected.
    pub token: Option<&'static str>,
}

#[derive(Clone, Copy)]
pub(crate) struct DeclaredCase<'a> {
    step_id: &'a str,
    file: &'a str,
    base: &'a [u8],
    case: &'a EnvFileCase,
}

#[derive(Clone, Copy)]
pub(crate) struct CaseEvidence<'a> {
    pub step_id: &'a str,
    pub name: &'a str,
    pub file: &'a str,
    pub location: Location,
    pub token: Option<&'static str>,
}

pub(crate) struct PreparedCase<'a> {
    declaration: DeclaredCase<'a>,
    applied: Applied,
}

impl<'a> DeclaredCase<'a> {
    pub(crate) fn prepare(self) -> PreparedCase<'a> {
        PreparedCase {
            declaration: self,
            applied: apply(self.base, self.case),
        }
    }
}

impl<'a> PreparedCase<'a> {
    pub(crate) fn evidence(&self) -> CaseEvidence<'a> {
        CaseEvidence {
            step_id: self.declaration.step_id,
            name: &self.declaration.case.name,
            file: self.declaration.file,
            location: self.applied.location,
            token: self.applied.token,
        }
    }

    pub(crate) fn write_to(&self, workdir: &std::path::Path) -> std::io::Result<()> {
        write(&workdir.join(self.declaration.file), &self.applied)
    }
}

fn selected_cases(step: &crate::experiment::types::Step) -> impl Iterator<Item = DeclaredCase<'_>> {
    use crate::experiment::types::MutatorDecl;

    let mut first_declarations = std::collections::HashMap::new();
    step.mutators
        .iter()
        .filter_map(|mutator| match mutator {
            MutatorDecl::EnvFile { file, base, cases } => Some((file, base, cases)),
            _ => None,
        })
        .flat_map(|(file, base, cases)| {
            cases.iter().map(|case| DeclaredCase { step_id: &step.id, file, base, case })
        })
        .map(move |declaration| {
            *first_declarations
                .entry(declaration.case.name.as_str())
                .or_insert(declaration)
        })
}

pub(crate) fn find_case<'a>(
    experiment: &'a crate::experiment::types::Experiment,
    step_id: &str,
    name: &str,
) -> Option<DeclaredCase<'a>> {
    let step = experiment.steps.iter().find(|step| step.id == step_id)?;
    selected_cases(step).find(|declaration| declaration.case.name == name)
}

pub(crate) fn enumerate_cases(
    experiment: &crate::experiment::types::Experiment,
) -> Vec<DeclaredCase<'_>> {
    experiment.steps.iter().flat_map(selected_cases).collect()
}

const NAMES: &[(&str, Kind)] = &[
    ("duplicate_same", Kind::DuplicateSame),
    ("duplicate_other", Kind::DuplicateOther),
    ("unknown_name", Kind::UnknownName),
    ("foreign_name", Kind::ForeignName),
    ("lowercase_name", Kind::LowercaseName),
    ("no_equals", Kind::NoEquals),
    ("export_prefix", Kind::ExportPrefix),
    ("nul_byte", Kind::NulByte),
    ("lone_cr", Kind::LoneCr),
    ("unclosed_quote", Kind::UnclosedQuote),
    ("text_after_quote", Kind::TextAfterQuote),
    ("invalid_utf8", Kind::InvalidUtf8),
    ("unreadable", Kind::Unreadable),
    ("directory", Kind::Directory),
];

/// The catalog in order: the fixed cases, one `prohibited:NAME` per
/// prohibited variable, and one case per documented limit.
pub fn catalog(limits: Limits, prohibited: &[String]) -> Vec<EnvFileCase> {
    let mut cases: Vec<EnvFileCase> = NAMES
        .iter()
        .map(|(name, kind)| EnvFileCase { name: (*name).into(), kind: *kind, arg: None })
        .collect();
    cases.extend(prohibited.iter().map(|variable| EnvFileCase {
        name: format!("prohibited:{variable}"),
        kind: Kind::Prohibited,
        arg: Some(variable.clone()),
    }));
    for (limit, name, kind) in [
        (limits.value_bytes, "value_too_long", Kind::ValueTooLong),
        (limits.name_bytes, "name_too_long", Kind::NameTooLong),
        (limits.file_bytes, "file_too_large", Kind::FileTooLarge),
    ] {
        if let Some(limit) = limit {
            cases.push(EnvFileCase { name: name.into(), kind, arg: Some(limit.to_string()) });
        }
    }
    cases
}

/// The names `only` may list (prohibited and size cases included).
pub fn known_names(cases: &[EnvFileCase]) -> Vec<&str> {
    cases.iter().map(|case| case.name.as_str()).collect()
}

/// The assignments of the file: (line index, name, start of the value).
fn assignments(lines: &[Vec<u8>]) -> Vec<(usize, Vec<u8>, usize)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.iter().position(|byte| !matches!(byte, b' ' | b'\t'))?;
            if line[trimmed] == b'#' {
                return None;
            }
            let equals = line.iter().position(|byte| *byte == b'=')?;
            let name = line[trimmed..equals].iter().copied().filter(|byte| !matches!(byte, b' ' | b'\t')).collect();
            Some((index, name, equals + 1))
        })
        .collect()
}

/// Whether the file has an assignment to break; checked when resolving.
pub fn has_assignment(base: &[u8]) -> bool {
    !assignments(&split(base)).is_empty()
}

fn split(base: &[u8]) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = base.split(|byte| *byte == b'\n').map(<[u8]>::to_vec).collect();
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

fn join(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// Break `base` the way `case` says. `base` must have an assignment.
pub fn apply(base: &[u8], case: &EnvFileCase) -> Applied {
    let mut lines = split(base);
    let found = assignments(&lines);
    let (first_index, first_name, _) = found.first().cloned().unwrap_or_default();
    let (last_index, _, last_value) = found.last().cloned().unwrap_or_default();
    // The program's own prefix (`VECTIS_`), so unknown names look like its own.
    let prefix: Vec<u8> = first_name
        .iter()
        .position(|byte| *byte == b'_')
        .map_or_else(Vec::new, |end| first_name[..=end].to_vec());
    let named = |name: &[u8], value: &[u8]| [name, b"=", value].concat();
    let limit = || case.arg.as_deref().and_then(|text| text.parse::<usize>().ok()).unwrap_or_default();

    let append = |line: Vec<u8>, token| {
        let mut lines = split(base);
        lines.push(line);
        Applied { content: Content::Bytes { bytes: join(&lines), unreadable: false }, location: Location::Line(lines.len()), token }
    };
    match case.kind {
        Kind::DuplicateSame => {
            let line = split(base)[first_index].clone();
            append(line, None)
        }
        Kind::DuplicateOther => append(named(&first_name, b"nadir-dup-value"), Some("nadir-dup-value")),
        Kind::UnknownName => append(named(&[&prefix[..], b"NADIR_UNKNOWN"].concat(), b"nadir-unknown-value"), Some("nadir-unknown-value")),
        Kind::ForeignName => append(named(b"NADIR_FOREIGN", b"nadir-foreign-value"), Some("nadir-foreign-value")),
        Kind::LowercaseName => append(named(&first_name.to_ascii_lowercase(), b"nadir-lower-value"), Some("nadir-lower-value")),
        Kind::NoEquals => append([&prefix[..], b"NADIR_NO_EQUALS"].concat(), None),
        Kind::Prohibited => {
            let variable = case.arg.clone().unwrap_or_default();
            append(named(variable.as_bytes(), b"nadir-prohibited-value"), Some("nadir-prohibited-value"))
        }
        Kind::NameTooLong => {
            let padding = limit().saturating_add(1).saturating_sub(prefix.len()).max(1);
            append(named(&[prefix.clone(), vec![b'N'; padding]].concat(), b"nadir-long-name"), Some("nadir-long-name"))
        }
        Kind::ExportPrefix => {
            lines[first_index] = [&b"export "[..], &lines[first_index]].concat();
            Applied { content: Content::Bytes { bytes: join(&lines), unreadable: false }, location: Location::Line(first_index + 1), token: None }
        }
        Kind::NulByte | Kind::LoneCr | Kind::UnclosedQuote | Kind::TextAfterQuote | Kind::InvalidUtf8 | Kind::ValueTooLong => {
            let (value, token, location): (Vec<u8>, _, _) = match case.kind {
                Kind::NulByte => (b"nadir-nul\0value".to_vec(), "nadir-nul", Location::Line(last_index + 1)),
                Kind::LoneCr => (b"nadir-cr\rvalue".to_vec(), "nadir-cr", Location::Line(last_index + 1)),
                Kind::UnclosedQuote => (b"\"nadir-unclosed-value".to_vec(), "nadir-unclosed-value", Location::Line(last_index + 1)),
                Kind::TextAfterQuote => (b"\"nadir-quoted\" nadir-after".to_vec(), "nadir-after", Location::Line(last_index + 1)),
                // Invalid UTF-8 makes the whole file unreadable as text.
                Kind::InvalidUtf8 => (b"nadir-utf8\xff\xfe".to_vec(), "nadir-utf8", Location::File),
                _ => {
                    let mut value = b"nadir-long".to_vec();
                    value.resize(limit().saturating_add(1).max(value.len()), b'x');
                    (value, "nadir-long", Location::Line(last_index + 1))
                }
            };
            lines[last_index] = [&lines[last_index][..last_value], &value[..]].concat();
            Applied { content: Content::Bytes { bytes: join(&lines), unreadable: false }, location, token: Some(token) }
        }
        Kind::FileTooLarge => {
            let mut bytes = join(&lines);
            let pad = b"# nadir padding: the file exceeds its documented size limit\n";
            while bytes.len() <= limit() {
                bytes.extend_from_slice(pad);
            }
            Applied { content: Content::Bytes { bytes, unreadable: false }, location: Location::File, token: None }
        }
        Kind::Unreadable => Applied {
            content: Content::Bytes { bytes: join(&lines), unreadable: true },
            location: Location::File,
            token: None,
        },
        Kind::Directory => Applied { content: Content::Directory, location: Location::File, token: None },
    }
}

/// Write the broken file at `path` (replacing the copied one).
pub fn write(path: &std::path::Path, applied: &Applied) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    match &applied.content {
        Content::Directory => std::fs::create_dir(path),
        Content::Bytes { bytes, unreadable } => {
            std::fs::write(path, bytes)?;
            if *unreadable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &[u8] = b"# comment\nAPP_MODE=dev\n\nAPP_LEVEL=info\n";

    fn case(name: &str, cases: &[EnvFileCase]) -> EnvFileCase {
        cases.iter().find(|case| case.name == name).cloned().unwrap()
    }

    fn text(applied: &Applied) -> String {
        match &applied.content {
            Content::Bytes { bytes, .. } => String::from_utf8_lossy(bytes).into_owned(),
            Content::Directory => "<dir>".into(),
        }
    }

    #[test]
    fn names_append_a_line_values_change_the_last_assignment() {
        let cases = catalog(Limits { file_bytes: Some(100), value_bytes: Some(8), name_bytes: Some(12) }, &["APP_SECRET".into()]);
        let names = known_names(&cases);
        assert_eq!(names[..3], ["duplicate_same", "duplicate_other", "unknown_name"]);
        assert!(names.contains(&"prohibited:APP_SECRET") && names.ends_with(&["value_too_long", "name_too_long", "file_too_large"]));

        let applied = apply(BASE, &case("duplicate_same", &cases));
        assert_eq!(text(&applied), "# comment\nAPP_MODE=dev\n\nAPP_LEVEL=info\nAPP_MODE=dev\n");
        assert_eq!((applied.location, applied.token), (Location::Line(5), None));
        let applied = apply(BASE, &case("unknown_name", &cases));
        assert!(text(&applied).ends_with("APP_NADIR_UNKNOWN=nadir-unknown-value\n"));
        assert_eq!(applied.token, Some("nadir-unknown-value"));
        assert!(text(&apply(BASE, &case("lowercase_name", &cases))).ends_with("app_mode=nadir-lower-value\n"));
        assert!(text(&apply(BASE, &case("prohibited:APP_SECRET", &cases))).ends_with("APP_SECRET=nadir-prohibited-value\n"));
        let export = apply(BASE, &case("export_prefix", &cases));
        assert_eq!((text(&export).lines().nth(1), export.location), (Some("export APP_MODE=dev"), Location::Line(2)));

        let applied = apply(BASE, &case("lone_cr", &cases));
        assert_eq!(text(&applied), "# comment\nAPP_MODE=dev\n\nAPP_LEVEL=nadir-cr\rvalue\n");
        assert_eq!(applied.location, Location::Line(4));
        assert_eq!(apply(BASE, &case("invalid_utf8", &cases)).location, Location::File);
        // Sizes go one byte past the limit.
        assert!(text(&apply(BASE, &case("value_too_long", &cases))).ends_with("APP_LEVEL=nadir-long\n"));
        assert!(text(&apply(BASE, &case("name_too_long", &cases))).contains("APP_NNNNNNNNN=nadir-long-name"));
        let large = apply(BASE, &case("file_too_large", &cases));
        assert!(matches!(&large.content, Content::Bytes { bytes, .. } if bytes.len() > 100));
        assert_eq!(apply(BASE, &case("directory", &cases)).content, Content::Directory);
        assert!(matches!(apply(BASE, &case("unreadable", &cases)).content, Content::Bytes { unreadable: true, .. }));
    }

    #[test]
    fn without_limits_or_prohibited_there_are_no_size_cases() {
        let names: Vec<String> = catalog(Limits::default(), &[]).into_iter().map(|case| case.name).collect();
        assert_eq!(names.len(), 14);
        assert!(!names.iter().any(|name| name.contains("too_l") || name.starts_with("prohibited")));
        assert!(!has_assignment(b"# only a comment\n\n") && has_assignment(BASE));
    }
}
