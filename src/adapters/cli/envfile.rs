//! Read a `.env` file into the base environment of a `protocol: cli` step.
//!
//! One `NAME=VALUE` per line, LF or CRLF, empty lines and full-line `#`
//! comments. Spaces and tabs around names and values are removed. A value
//! that starts with a quote ends at the matching quote and keeps its
//! contents. No escapes and no interpolation: `${NAME}` stays literal. An
//! optional leading `export ` is accepted. Duplicate names are errors.

use std::path::Path;

/// The file's assignments in order, or the first problem with its line.
pub fn load(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut lines_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (index, raw_line) in text.split('\n').enumerate() {
        let number = index + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let trimmed = line.trim_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").map_or(trimmed, |rest| rest.trim_start_matches([' ', '\t']));
        let (name, value) = assignment
            .split_once('=')
            .ok_or_else(|| format!("line {number}: expected NAME=VALUE"))?;
        let name = name.trim_matches([' ', '\t']);
        let valid_name = name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_');
        if !valid_name {
            return Err(format!("line {number}: invalid variable name"));
        }
        let value = unquote(value.trim_matches([' ', '\t'])).map_err(|reason| format!("line {number}: {reason}"))?;
        if let Some(first) = lines_of.insert(name.to_owned(), number) {
            return Err(format!("line {number}: '{name}' is already defined on line {first}"));
        }
        assignments.push((name.to_owned(), value));
    }
    Ok(assignments)
}

fn unquote(value: &str) -> Result<String, &'static str> {
    let Some(quote) = value.chars().next().filter(|first| *first == '"' || *first == '\'') else {
        return Ok(value.to_owned());
    };
    let rest = &value[1..];
    let end = rest.find(quote).ok_or("unclosed quote")?;
    if !rest[end + 1..].trim_matches([' ', '\t']).is_empty() {
        return Err("only spaces may follow a closing quote");
    }
    Ok(rest[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_assignments_quotes_comments_and_export() {
        let text = "# comment\r\nVECTIS_MODE=dev\r\n\n  export VECTIS_A = spaced value \nVECTIS_B=\"keep  # this ${X}\"  \nVECTIS_C='single'\nVECTIS_D=\nVECTIS_E=a=b#c";
        assert_eq!(parse(text).unwrap(), vec![
            ("VECTIS_MODE".into(), "dev".into()),
            ("VECTIS_A".into(), "spaced value".into()),
            ("VECTIS_B".into(), "keep  # this ${X}".into()),
            ("VECTIS_C".into(), "single".into()),
            ("VECTIS_D".into(), "".into()),
            ("VECTIS_E".into(), "a=b#c".into()),
        ]);
    }

    #[test]
    fn reports_the_line_of_each_problem() {
        assert_eq!(parse("A=1\nnot an assignment").unwrap_err(), "line 2: expected NAME=VALUE");
        assert_eq!(parse("1A=x").unwrap_err(), "line 1: invalid variable name");
        assert_eq!(parse("A=\"open").unwrap_err(), "line 1: unclosed quote");
        assert_eq!(parse("A='x' y").unwrap_err(), "line 1: only spaces may follow a closing quote");
        assert_eq!(parse("A=1\n\nA=2").unwrap_err(), "line 3: 'A' is already defined on line 1");
    }
}
