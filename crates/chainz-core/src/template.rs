//! `${NAME}` templates: one syntax for URLs, header values and explorer keys.
//!
//! Expansion is a single left-to-right pass: a substituted value is never
//! re-scanned, so a value containing `${…}` is inserted literally. An
//! unresolved `${NAME}` is an error; it is never passed through.
//!
//! Lookup order (see [`crate::secrets::VarResolver`]): the built-ins
//! `${CHAIN_ID}` and `${CHAIN}` first, then the process environment, then
//! `secrets.toml`. Built-ins come first because chainz itself exports
//! `CHAIN_ID` into child shells; letting the environment win would make a
//! nested `chainz exec eth` inside `chainz shell base` build base's URLs.

use std::borrow::Cow;

/// A template failed to expand. Never contains a resolved value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("variable ${{{0}}} is not set (set it with `chainz var set {0}` or export it)")]
    Unresolved(String),
    #[error("unterminated `${{` in template")]
    Unterminated,
    #[error("invalid variable name `{0}` in template (use letters, digits and _)")]
    InvalidName(String),
}

/// Whether `name` is a valid variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `input` contains any `${` template.
pub fn has_templates(input: &str) -> bool {
    input.contains("${")
}

/// The variable names `input` references, in order (duplicates kept).
pub fn references(input: &str) -> Result<Vec<&str>, TemplateError> {
    let mut names = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or(TemplateError::Unterminated)?;
        let name = &after[..end];
        if !is_valid_name(name) {
            return Err(TemplateError::InvalidName(name.to_string()));
        }
        names.push(name);
        rest = &after[end + 1..];
    }
    Ok(names)
}

/// Expand every `${NAME}` in `input` through `lookup`, in one pass.
/// `lookup` returns `Ok(None)` for an unknown name, which becomes
/// [`TemplateError::Unresolved`].
pub fn expand<'a, E>(
    input: &'a str,
    mut lookup: impl FnMut(&str) -> Result<Option<Cow<'a, str>>, E>,
) -> Result<Cow<'a, str>, E>
where
    E: From<TemplateError>,
{
    if !has_templates(input) {
        return Ok(Cow::Borrowed(input));
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or(TemplateError::Unterminated)?;
        let name = &after[..end];
        if !is_valid_name(name) {
            return Err(TemplateError::InvalidName(name.to_string()).into());
        }
        match lookup(name)? {
            Some(value) => out.push_str(&value),
            None => return Err(TemplateError::Unresolved(name.to_string()).into()),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(Cow::Owned(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(input: &str, vars: &[(&str, &str)]) -> Result<String, TemplateError> {
        let map: HashMap<_, _> = vars.iter().cloned().collect();
        expand(input, |name| {
            Ok::<_, TemplateError>(map.get(name).map(|v| Cow::Owned(v.to_string())))
        })
        .map(|c| c.into_owned())
    }

    #[test]
    fn expands_in_one_pass_without_rescanning() {
        assert_eq!(
            run(
                "https://x/v3/${KEY}?a=${B}",
                &[("KEY", "k"), ("B", "${KEY}")]
            )
            .unwrap(),
            "https://x/v3/k?a=${KEY}"
        );
        assert_eq!(
            run("no templates $HOME", &[]).unwrap(),
            "no templates $HOME"
        );
    }

    #[test]
    fn unresolved_and_malformed_templates_fail_closed() {
        assert_eq!(
            run("a${MISSING}b", &[]),
            Err(TemplateError::Unresolved("MISSING".into()))
        );
        assert_eq!(run("a${OPEN", &[]), Err(TemplateError::Unterminated));
        assert_eq!(
            run("${a-b}", &[]),
            Err(TemplateError::InvalidName("a-b".into()))
        );
    }

    #[test]
    fn lists_references() {
        assert_eq!(
            references("${A}/${B_2}/${A}").unwrap(),
            vec!["A", "B_2", "A"]
        );
        assert!(references("plain").unwrap().is_empty());
    }
}
