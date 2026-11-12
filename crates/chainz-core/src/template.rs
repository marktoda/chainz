//! `${NAME}` templates: one syntax for URLs, header values and explorer keys.
//!
//! Expansion is a single left-to-right pass: a substituted value is never
//! re-scanned, so a value containing `${…}` is inserted literally. An
//! unresolved `${NAME}` is an error; it is never passed through.
//!
//! Lookup order (see `secrets::VarResolver`): the built-ins
//! `${CHAIN_ID}` and `${CHAIN}` first, then the process environment, then
//! `secrets.toml`. Built-ins come first because chainz itself exports
//! `CHAIN_ID` into child shells; letting the environment win would make a
//! nested `chainz exec eth` inside a `chainz exec base` subshell build base's URLs.
//! Likewise, inside a chainz context (`CHAINZ_CHAIN` set) the environment
//! is skipped for the exec contract's own variables.

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
pub(crate) fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A valid variable name made from `text`: every character outside
/// `[A-Za-z0-9_]` becomes `_`, and a leading `_` is added when it would be
/// empty or start with a digit.
pub(crate) fn clean_name(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// `input` with every `${…}` replaced by `x`: its shape without the
/// values, for structural checks (is it a URL, what is its host).
pub(crate) fn blanked(input: &str) -> String {
    spans(input)
        .map(|span| match span {
            Span::Text(text) => text,
            Span::Var(_) | Span::Unterminated(_) => "x",
        })
        .collect()
}

/// Whether `input` contains any `${` template.
pub(crate) fn has_templates(input: &str) -> bool {
    input.contains("${")
}

/// One piece of a template, as [`spans`] yields it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Span<'a> {
    /// Literal text.
    Text(&'a str),
    /// `${NAME}`: the text between the braces, not yet validated.
    Var(&'a str),
    /// A `${` with no closing `}`: the rest of the input from `${` on.
    Unterminated(&'a str),
}

/// The one `${…}` scanner: split `input` into literal text and variable
/// references, left to right. Never fails; callers decide what an invalid
/// name or an unterminated `${` means.
pub(crate) fn spans(input: &str) -> impl Iterator<Item = Span<'_>> {
    let mut rest = input;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let Some(start) = rest.find("${") else {
            return Some(Span::Text(std::mem::take(&mut rest)));
        };
        if start > 0 {
            let text = &rest[..start];
            rest = &rest[start..];
            return Some(Span::Text(text));
        }
        let after = &rest[2..];
        match after.find('}') {
            Some(end) => {
                rest = &after[end + 1..];
                Some(Span::Var(&after[..end]))
            }
            None => Some(Span::Unterminated(std::mem::take(&mut rest))),
        }
    })
}

/// The variable names `input` references, in order (duplicates kept).
pub fn references(input: &str) -> Result<Vec<&str>, TemplateError> {
    let mut names = Vec::new();
    for span in spans(input) {
        match span {
            Span::Text(_) => {}
            Span::Var(name) if is_valid_name(name) => names.push(name),
            Span::Var(name) => return Err(TemplateError::InvalidName(name.to_string())),
            Span::Unterminated(_) => return Err(TemplateError::Unterminated),
        }
    }
    Ok(names)
}

/// Expand every `${NAME}` in `input` through `lookup`, in one pass.
/// `lookup` returns `Ok(None)` for an unknown name, which becomes
/// [`TemplateError::Unresolved`].
pub(crate) fn expand<'a, 'v, E>(
    input: &'a str,
    mut lookup: impl FnMut(&str) -> Result<Option<Cow<'v, str>>, E>,
) -> Result<Cow<'a, str>, E>
where
    E: From<TemplateError>,
{
    if !has_templates(input) {
        return Ok(Cow::Borrowed(input));
    }
    let mut out = String::with_capacity(input.len());
    for span in spans(input) {
        match span {
            Span::Text(text) => out.push_str(text),
            Span::Var(name) if !is_valid_name(name) => {
                return Err(TemplateError::InvalidName(name.to_string()).into());
            }
            Span::Var(name) => match lookup(name)? {
                Some(value) => out.push_str(&value),
                None => return Err(TemplateError::Unresolved(name.to_string()).into()),
            },
            Span::Unterminated(_) => return Err(TemplateError::Unterminated.into()),
        }
    }
    Ok(Cow::Owned(out))
}

/// Rename variable references in one pass: every `${OLD}` whose name is a
/// key of `renames` becomes `${NEW}`; a new name is never renamed again.
/// Everything else, malformed templates included, is copied unchanged.
pub(crate) fn rename_vars<S: std::hash::BuildHasher>(
    input: &str,
    renames: &std::collections::HashMap<String, String, S>,
) -> String {
    let mut out = String::with_capacity(input.len());
    for span in spans(input) {
        match span {
            Span::Text(text) | Span::Unterminated(text) => out.push_str(text),
            Span::Var(name) => {
                out.push_str("${");
                out.push_str(renames.get(name).map_or(name, String::as_str));
                out.push('}');
            }
        }
    }
    out
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
    fn spans_split_text_and_variables() {
        assert_eq!(
            spans("a${B}c${}${D").collect::<Vec<_>>(),
            [
                Span::Text("a"),
                Span::Var("B"),
                Span::Text("c"),
                Span::Var(""),
                Span::Unterminated("${D")
            ]
        );
        assert_eq!(spans("").count(), 0);
        assert_eq!(
            spans("${A}${B}").collect::<Vec<_>>(),
            [Span::Var("A"), Span::Var("B")]
        );
    }

    #[test]
    fn renames_in_one_pass() {
        let renames: HashMap<String, String> = [("A-B", "A_B"), ("A_B", "A_B_2")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(
            rename_vars("x/${A-B}/${A_B}/${C}/${A-B", &renames),
            "x/${A_B}/${A_B_2}/${C}/${A-B"
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
