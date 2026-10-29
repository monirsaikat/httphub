//! Assertions and captures, written in the request file itself.
//!
//! Postman's tests live in a GUI textarea as sandboxed JavaScript, which means
//! they cannot be reviewed, diffed, or run without the GUI's runtime. Here an
//! assertion is a line in the same file as the request:
//!
//! ```text
//! ### Create a user
//! # @expect status 201
//! # @expect header content-type contains application/json
//! # @expect json /name == ada
//! # @expect duration < 500
//! # @capture id = json /id
//! POST {{base_url}}/users
//! ```
//!
//! A declarative set on purpose. Anything that needs real logic belongs in the
//! scripting layer, not in a half-language grown out of comments.

use crate::model::ResponseHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    Status(StatusMatch),
    Header {
        name: String,
        op: TextOp,
        value: Option<String>,
    },
    Body {
        op: TextOp,
        value: String,
    },
    Json {
        pointer: String,
        op: ValueOp,
        value: String,
    },
    MaxDuration {
        ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusMatch {
    Exact(u16),
    /// `2xx`, stored as the leading digit.
    Class(u16),
    Below(u16),
    AtLeast(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOp {
    Exists,
    Missing,
    Equals,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueOp {
    Eq,
    Ne,
    Contains,
}

/// Pull a value out of a response into a variable for later requests, which is
/// what makes a login-then-use chain work without scripting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub var: String,
    pub source: CaptureSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    Json(String),
    Header(String),
    Status,
    Body,
}

impl Expectation {
    /// How the assertion is shown in reports: the directive, normalised.
    pub fn describe(&self) -> String {
        match self {
            Expectation::Status(m) => format!("status {}", describe_status(m)),
            Expectation::Header { name, op, value } => match (op, value) {
                (TextOp::Exists, _) => format!("header {name} exists"),
                (TextOp::Missing, _) => format!("header {name} missing"),
                (op, Some(v)) => format!("header {name} {} {v}", describe_text_op(*op)),
                (op, None) => format!("header {name} {}", describe_text_op(*op)),
            },
            Expectation::Body { op, value } => {
                format!("body {} {value}", describe_text_op(*op))
            }
            Expectation::Json {
                pointer,
                op,
                value,
            } => format!("json {pointer} {} {value}", describe_value_op(*op)),
            Expectation::MaxDuration { ms } => format!("duration < {ms}"),
        }
    }

    /// `Ok(())` when satisfied, `Err(explanation)` when not. The explanation is
    /// the whole value of a test runner, so it says what was actually seen.
    pub fn check(&self, response: &ResponseHandle, body: &[u8]) -> Result<(), String> {
        match self {
            Expectation::Status(m) => {
                let got = response.status;
                let ok = match m {
                    StatusMatch::Exact(want) => got == *want,
                    StatusMatch::Class(digit) => got / 100 == *digit,
                    StatusMatch::Below(limit) => got < *limit,
                    StatusMatch::AtLeast(limit) => got >= *limit,
                };
                if ok {
                    Ok(())
                } else {
                    Err(format!("got {got}"))
                }
            }

            Expectation::Header { name, op, value } => {
                let got = response.header(name);
                match (op, got, value) {
                    (TextOp::Exists, Some(_), _) => Ok(()),
                    (TextOp::Exists, None, _) => Err("header absent".into()),
                    (TextOp::Missing, None, _) => Ok(()),
                    (TextOp::Missing, Some(v), _) => Err(format!("present, {v}")),
                    (_, None, _) => Err("header absent".into()),
                    (TextOp::Equals, Some(got), Some(want)) => {
                        if got == want {
                            Ok(())
                        } else {
                            Err(format!("got {got}"))
                        }
                    }
                    (TextOp::Contains, Some(got), Some(want)) => {
                        if got.contains(want.as_str()) {
                            Ok(())
                        } else {
                            Err(format!("got {got}"))
                        }
                    }
                    (_, Some(_), None) => Err("no value to compare against".into()),
                }
            }

            Expectation::Body { op, value } => {
                let text = String::from_utf8_lossy(body);
                let ok = match op {
                    TextOp::Contains => text.contains(value.as_str()),
                    TextOp::Equals => text.trim() == value.as_str(),
                    TextOp::Exists => !text.is_empty(),
                    TextOp::Missing => text.is_empty(),
                };
                if ok {
                    Ok(())
                } else {
                    Err(format!("body was {}", preview(&text)))
                }
            }

            Expectation::Json {
                pointer,
                op,
                value,
            } => {
                let parsed: serde_json::Value = serde_json::from_slice(body)
                    .map_err(|e| format!("body is not json: {e}"))?;
                let found = parsed
                    .pointer(pointer)
                    .ok_or_else(|| format!("no value at {pointer}"))?;
                let got = scalar_to_string(found);
                let ok = match op {
                    ValueOp::Eq => got == *value,
                    ValueOp::Ne => got != *value,
                    ValueOp::Contains => got.contains(value.as_str()),
                };
                if ok {
                    Ok(())
                } else {
                    Err(format!("got {got}"))
                }
            }

            Expectation::MaxDuration { ms } => {
                let took = response
                    .timings
                    .total
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if took < *ms {
                    Ok(())
                } else {
                    Err(format!("took {took}ms"))
                }
            }
        }
    }
}

impl Capture {
    /// `Ok(value)`, or an explanation of why there was nothing to take.
    pub fn extract(&self, response: &ResponseHandle, body: &[u8]) -> Result<String, String> {
        match &self.source {
            CaptureSource::Status => Ok(response.status.to_string()),
            CaptureSource::Body => Ok(String::from_utf8_lossy(body).trim().to_string()),
            CaptureSource::Header(name) => response
                .header(name)
                .map(str::to_string)
                .ok_or_else(|| format!("no {name} header to capture")),
            CaptureSource::Json(pointer) => {
                let parsed: serde_json::Value = serde_json::from_slice(body)
                    .map_err(|e| format!("body is not json: {e}"))?;
                parsed
                    .pointer(pointer)
                    .map(scalar_to_string)
                    .ok_or_else(|| format!("no value at {pointer}"))
            }
        }
    }

    pub fn describe(&self) -> String {
        match &self.source {
            CaptureSource::Json(p) => format!("{} = json {p}", self.var),
            CaptureSource::Header(h) => format!("{} = header {h}", self.var),
            CaptureSource::Status => format!("{} = status", self.var),
            CaptureSource::Body => format!("{} = body", self.var),
        }
    }
}

/// Parse the text after `# @expect`.
pub fn parse_expectation(rest: &str) -> Result<Expectation, String> {
    let mut words = rest.split_whitespace();
    let kind = words.next().ok_or("empty @expect")?;
    let tail: Vec<&str> = words.collect();

    match kind.to_ascii_lowercase().as_str() {
        "status" => {
            let spec = tail.join("");
            if spec.is_empty() {
                return Err("expect status <code|2xx|<400|>=200>".into());
            }
            Ok(Expectation::Status(parse_status(&spec)?))
        }

        "header" => {
            let name = tail.first().ok_or("expect header <name> <op> [value]")?;
            let op = tail.get(1).copied().unwrap_or("exists");
            let value = if tail.len() > 2 {
                Some(tail[2..].join(" "))
            } else {
                None
            };
            let op = parse_text_op(op)?;
            if matches!(op, TextOp::Equals | TextOp::Contains) && value.is_none() {
                return Err(format!("expect header {name} {op:?} needs a value"));
            }
            Ok(Expectation::Header {
                name: name.to_string(),
                op,
                value,
            })
        }

        "body" => {
            let op = parse_text_op(tail.first().copied().unwrap_or("exists"))?;
            Ok(Expectation::Body {
                op,
                value: tail.get(1..).map(|t| t.join(" ")).unwrap_or_default(),
            })
        }

        "json" => {
            let pointer = tail.first().ok_or("expect json <pointer> <op> <value>")?;
            let op = match tail.get(1).copied() {
                Some("==") | Some("=") => ValueOp::Eq,
                Some("!=") => ValueOp::Ne,
                Some("contains") => ValueOp::Contains,
                Some(other) => return Err(format!("unknown json comparison `{other}`")),
                None => return Err("expect json <pointer> <op> <value>".into()),
            };
            let value = tail
                .get(2..)
                .map(|t| t.join(" "))
                .ok_or("expect json <pointer> <op> <value>")?;
            Ok(Expectation::Json {
                pointer: normalise_pointer(pointer),
                op,
                value: unquote(&value),
            })
        }

        "duration" => {
            // `duration < 500`, and the `<` is optional noise.
            let spec: String = tail.join("");
            let digits: String = spec.chars().filter(|c| c.is_ascii_digit()).collect();
            digits
                .parse()
                .map(|ms| Expectation::MaxDuration { ms })
                .map_err(|_| "expect duration < <milliseconds>".to_string())
        }

        other => Err(format!(
            "unknown expectation `{other}`; try status, header, body, json or duration"
        )),
    }
}

/// Parse the text after `# @capture`, which is `name = source`.
pub fn parse_capture(rest: &str) -> Result<Capture, String> {
    let (var, source) = rest
        .split_once('=')
        .ok_or("capture <name> = <json <pointer>|header <name>|status|body>")?;
    let var = var.trim();
    if var.is_empty() {
        return Err("capture needs a variable name".into());
    }

    let mut words = source.split_whitespace();
    let kind = words.next().ok_or("capture needs a source")?;
    let arg = words.next();

    let source = match (kind.to_ascii_lowercase().as_str(), arg) {
        ("json", Some(p)) => CaptureSource::Json(normalise_pointer(p)),
        ("header", Some(h)) => CaptureSource::Header(h.to_string()),
        ("status", _) => CaptureSource::Status,
        ("body", _) => CaptureSource::Body,
        ("json", None) => return Err("capture json needs a pointer, e.g. /token".into()),
        ("header", None) => return Err("capture header needs a name".into()),
        (other, _) => return Err(format!("unknown capture source `{other}`")),
    };

    Ok(Capture {
        var: var.to_string(),
        source,
    })
}

fn parse_status(spec: &str) -> Result<StatusMatch, String> {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix(">=") {
        return rest
            .parse()
            .map(StatusMatch::AtLeast)
            .map_err(|_| format!("bad status `{spec}`"));
    }
    if let Some(rest) = spec.strip_prefix('<') {
        return rest
            .parse()
            .map(StatusMatch::Below)
            .map_err(|_| format!("bad status `{spec}`"));
    }
    if let Some(digit) = spec.strip_suffix("xx") {
        return digit
            .parse()
            .map(StatusMatch::Class)
            .map_err(|_| format!("bad status class `{spec}`"));
    }
    spec.parse()
        .map(StatusMatch::Exact)
        .map_err(|_| format!("bad status `{spec}`"))
}

fn parse_text_op(op: &str) -> Result<TextOp, String> {
    match op.to_ascii_lowercase().as_str() {
        "exists" | "present" => Ok(TextOp::Exists),
        "missing" | "absent" => Ok(TextOp::Missing),
        "equals" | "==" | "=" | "is" => Ok(TextOp::Equals),
        "contains" | "~" => Ok(TextOp::Contains),
        other => Err(format!("unknown comparison `{other}`")),
    }
}

/// Accept `/a/b`, `a.b` and `$.a.b`, all meaning the same JSON Pointer.
fn normalise_pointer(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with('/') {
        return trimmed.to_string();
    }
    let stripped = trimmed
        .trim_start_matches('$')
        .trim_start_matches('.')
        .replace('.', "/");
    format!("/{stripped}")
}

fn unquote(value: &str) -> String {
    let t = value.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// Scalars compare as their plain text; containers as compact JSON.
fn scalar_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn preview(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() > 60 {
        format!("{}...", flat.chars().take(60).collect::<String>())
    } else {
        flat
    }
}

fn describe_status(m: &StatusMatch) -> String {
    match m {
        StatusMatch::Exact(c) => c.to_string(),
        StatusMatch::Class(d) => format!("{d}xx"),
        StatusMatch::Below(c) => format!("< {c}"),
        StatusMatch::AtLeast(c) => format!(">= {c}"),
    }
}

fn describe_text_op(op: TextOp) -> &'static str {
    match op {
        TextOp::Exists => "exists",
        TextOp::Missing => "missing",
        TextOp::Equals => "==",
        TextOp::Contains => "contains",
    }
}

fn describe_value_op(op: ValueOp) -> &'static str {
    match op {
        ValueOp::Eq => "==",
        ValueOp::Ne => "!=",
        ValueOp::Contains => "contains",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Timings;
    use crate::model::ByteCounts;
    use std::time::Duration;

    fn response(status: u16, headers: &[(&str, &str)], ms: u64) -> ResponseHandle {
        ResponseHandle {
            url: "https://a.test/".into(),
            status,
            status_text: "".into(),
            version: "HTTP/1.1".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body_path: Default::default(),
            body_len: 0,
            timings: Timings {
                total: Some(Duration::from_millis(ms)),
                ..Default::default()
            },
            bytes: ByteCounts::default(),
            redirects: Vec::new(),
            remote_addr: None,
            tls: None,
        }
    }

    #[test]
    fn status_forms_all_parse_and_check() {
        let r = response(201, &[], 0);
        for (src, pass) in [
            ("status 201", true),
            ("status 2xx", true),
            ("status < 400", true),
            ("status >= 300", false),
            ("status 404", false),
        ] {
            let parsed = parse_expectation(src).unwrap();
            assert_eq!(parsed.check(&r, b"").is_ok(), pass, "{src}");
        }
    }

    #[test]
    fn header_contains_reports_what_it_saw() {
        let r = response(200, &[("content-type", "application/json; charset=utf-8")], 0);
        assert!(parse_expectation("header content-type contains json")
            .unwrap()
            .check(&r, b"")
            .is_ok());
        let err = parse_expectation("header content-type contains xml")
            .unwrap()
            .check(&r, b"")
            .unwrap_err();
        assert!(err.contains("application/json"), "{err}");
    }

    #[test]
    fn missing_header_fails_a_comparison() {
        let r = response(200, &[], 0);
        assert!(parse_expectation("header etag equals abc")
            .unwrap()
            .check(&r, b"")
            .is_err());
        assert!(parse_expectation("header etag missing")
            .unwrap()
            .check(&r, b"")
            .is_ok());
    }

    #[test]
    fn json_pointer_accepts_three_spellings() {
        let r = response(200, &[], 0);
        let body = br#"{"user": {"name": "ada", "id": 42, "admin": true}}"#;
        for src in [
            "json /user/name == ada",
            "json user.name == ada",
            "json $.user.name == ada",
        ] {
            assert!(parse_expectation(src).unwrap().check(&r, body).is_ok(), "{src}");
        }
        assert!(parse_expectation("json /user/id == 42")
            .unwrap()
            .check(&r, body)
            .is_ok());
        assert!(parse_expectation("json /user/admin == true")
            .unwrap()
            .check(&r, body)
            .is_ok());
    }

    #[test]
    fn json_failure_shows_the_actual_value() {
        let r = response(200, &[], 0);
        let err = parse_expectation("json /name == ada")
            .unwrap()
            .check(&r, br#"{"name": "bob"}"#)
            .unwrap_err();
        assert!(err.contains("bob"), "{err}");
    }

    #[test]
    fn missing_pointer_is_distinct_from_a_mismatch() {
        let r = response(200, &[], 0);
        let err = parse_expectation("json /nope == x")
            .unwrap()
            .check(&r, br#"{"name": "bob"}"#)
            .unwrap_err();
        assert!(err.contains("no value at /nope"), "{err}");
    }

    #[test]
    fn duration_bound_checks_the_total() {
        let fast = response(200, &[], 10);
        let slow = response(200, &[], 900);
        let exp = parse_expectation("duration < 500").unwrap();
        assert!(exp.check(&fast, b"").is_ok());
        assert!(exp.check(&slow, b"").unwrap_err().contains("900ms"));
    }

    #[test]
    fn quoted_values_lose_their_quotes() {
        let r = response(200, &[], 0);
        assert!(parse_expectation(r#"json /name == "ada""#)
            .unwrap()
            .check(&r, br#"{"name": "ada"}"#)
            .is_ok());
    }

    #[test]
    fn unknown_directives_are_rejected_with_help() {
        let err = parse_expectation("vibes good").unwrap_err();
        assert!(err.contains("unknown expectation"), "{err}");
    }

    #[test]
    fn captures_read_json_headers_and_status() {
        let r = response(201, &[("etag", "W/\"9\"")], 0);
        let body = br#"{"token": "abc123", "n": 7}"#;
        assert_eq!(
            parse_capture("tok = json /token").unwrap().extract(&r, body),
            Ok("abc123".into())
        );
        assert_eq!(
            parse_capture("n = json /n").unwrap().extract(&r, body),
            Ok("7".into())
        );
        assert_eq!(
            parse_capture("tag = header etag").unwrap().extract(&r, body),
            Ok("W/\"9\"".into())
        );
        assert_eq!(
            parse_capture("code = status").unwrap().extract(&r, body),
            Ok("201".into())
        );
    }

    #[test]
    fn capture_without_an_equals_is_rejected() {
        assert!(parse_capture("token json /token").is_err());
    }

    #[test]
    fn describe_round_trips_for_reports() {
        assert_eq!(
            parse_expectation("status 2xx").unwrap().describe(),
            "status 2xx"
        );
        assert_eq!(
            parse_expectation("json user.name == ada").unwrap().describe(),
            "json /user/name == ada"
        );
    }
}
