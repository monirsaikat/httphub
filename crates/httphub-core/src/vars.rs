use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::model::RequestSpec;

/// Variable scope for one send. Later sources win, so a `--var` on the command
/// line overrides an environment file, which overrides a workspace default.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    values: HashMap<String, String>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn extend<I, K, V>(&mut self, pairs: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in pairs {
            self.values.insert(k.into(), v.into());
        }
        self
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.values.insert(key.into(), value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Replace every `{{name}}` in `input`. Unknown names are an error rather
    /// than an empty string: silently sending `/users/` when you meant
    /// `/users/42` is the kind of thing that costs an afternoon.
    pub fn substitute(&self, input: &str) -> Result<String> {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;

        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                // An unterminated `{{` is literal text, not a broken template.
                out.push_str(&rest[start..]);
                return Ok(out);
            };
            let name = after[..end].trim();
            match self.get(name) {
                Some(value) => out.push_str(value),
                None => return Err(Error::UnresolvedVar(name.to_string())),
            }
            rest = &after[end + 2..];
        }

        out.push_str(rest);
        Ok(out)
    }

    /// Resolve a whole request. Headers and body are templated too, which is
    /// what makes `Authorization: Bearer {{token}}` work.
    pub fn resolve(&self, spec: &RequestSpec) -> Result<RequestSpec> {
        Ok(RequestSpec {
            name: spec.name.clone(),
            method: spec.method.clone(),
            url: self.substitute(&spec.url)?,
            version: spec.version.clone(),
            headers: spec
                .headers
                .iter()
                .map(|(k, v)| Ok((k.clone(), self.substitute(v)?)))
                .collect::<Result<Vec<_>>>()?,
            body: spec.body.as_deref().map(|b| self.substitute(b)).transpose()?,
            expectations: spec.expectations.clone(),
            captures: spec.captures.clone(),
            line: spec.line,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        let mut s = Scope::new();
        s.set("base_url", "https://api.example.com").set("id", "42");
        s
    }

    #[test]
    fn substitutes_and_trims_names() {
        assert_eq!(
            scope().substitute("{{base_url}}/users/{{ id }}").unwrap(),
            "https://api.example.com/users/42"
        );
    }

    #[test]
    fn unknown_variable_is_an_error() {
        assert!(matches!(
            scope().substitute("{{nope}}"),
            Err(Error::UnresolvedVar(name)) if name == "nope"
        ));
    }

    #[test]
    fn unterminated_braces_stay_literal() {
        assert_eq!(scope().substitute("a {{ b").unwrap(), "a {{ b");
    }

    #[test]
    fn json_body_braces_survive() {
        let body = r#"{"id": "{{id}}", "nested": {"k": "v"}}"#;
        assert_eq!(
            scope().substitute(body).unwrap(),
            r#"{"id": "42", "nested": {"k": "v"}}"#
        );
    }
}
