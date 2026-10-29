//! Parser for the `.http` file format.
//!
//! Deliberately a superset of the format VS Code's REST Client and the
//! JetBrains HTTP Client already read, so the same file opens in tools people
//! have today. The source of truth is readable text that `git diff` renders
//! usefully -- not a JSON blob that produces unmergeable conflicts.
//!
//! ```text
//! ### Create a user
//! # @name create_user
//! POST {{base_url}}/users HTTP/1.1
//! Content-Type: application/json
//!
//! {"name": "ada"}
//! ```

use crate::error::{Error, Result};
use crate::model::RequestSpec;

const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

/// Parse every request in a `.http` document, in file order.
pub fn parse(source: &str, file_label: &str) -> Result<Vec<RequestSpec>> {
    let mut requests = Vec::new();
    let mut block: Vec<(usize, &str)> = Vec::new();
    let mut pending_title: Option<String> = None;

    for (idx, raw) in source.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if let Some(title) = separator_title(line) {
            if let Some(spec) = parse_block(&block, pending_title.take(), file_label)? {
                requests.push(spec);
            }
            block.clear();
            pending_title = title;
            continue;
        }
        block.push((idx + 1, line));
    }

    if let Some(spec) = parse_block(&block, pending_title.take(), file_label)? {
        requests.push(spec);
    }

    Ok(requests)
}

/// `###`, optionally followed by a human title.
fn separator_title(line: &str) -> Option<Option<String>> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("###") {
        return None;
    }
    let title = trimmed.trim_start_matches('#').trim();
    Some(if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    })
}

fn parse_block(
    lines: &[(usize, &str)],
    title: Option<String>,
    file_label: &str,
) -> Result<Option<RequestSpec>> {
    let mut spec = RequestSpec {
        name: title,
        ..Default::default()
    };

    let mut iter = lines.iter().peekable();

    // Leading comments and blanks. `# @name`, `# @expect` and `# @capture` are
    // metadata rather than commentary: assertions belong in the same file as
    // the request, where they can be reviewed and diffed.
    while let Some((line_no, line)) = iter.peek() {
        let line_no = *line_no;
        let t = line.trim();
        if t.is_empty() {
            iter.next();
            continue;
        }
        if let Some(rest) = t.strip_prefix('#').map(str::trim) {
            if let Some(name) = rest.strip_prefix("@name") {
                spec.name = Some(name.trim().trim_matches('"').to_string());
            } else if let Some(body) = rest.strip_prefix("@expect") {
                let parsed = crate::expect::parse_expectation(body.trim()).map_err(|message| {
                    Error::Parse {
                        file: file_label.to_string(),
                        line: line_no,
                        message,
                    }
                })?;
                spec.expectations.push(parsed);
            } else if let Some(body) = rest.strip_prefix("@capture") {
                let parsed = crate::expect::parse_capture(body.trim()).map_err(|message| {
                    Error::Parse {
                        file: file_label.to_string(),
                        line: line_no,
                        message,
                    }
                })?;
                spec.captures.push(parsed);
            }
            iter.next();
            continue;
        }
        if t.starts_with("//") {
            iter.next();
            continue;
        }
        break;
    }

    let Some((line_no, request_line)) = iter.next().copied() else {
        return Ok(None); // Comment-only or empty block: not an error.
    };

    let (method, url, version) = parse_request_line(request_line, line_no, file_label)?;
    spec.line = line_no;
    spec.method = method;
    spec.url = url;
    spec.version = version;

    // Headers, until a blank line or end of block.
    for (line_no, line) in iter.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        let t = line.trim();
        if t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        // A continuation line folds into the previous header value.
        if (line.starts_with(' ') || line.starts_with('\t')) && !spec.headers.is_empty() {
            let last = spec.headers.last_mut().expect("non-empty");
            last.1.push(' ');
            last.1.push_str(t);
            continue;
        }
        let Some((name, value)) = t.split_once(':') else {
            return Err(Error::Parse {
                file: file_label.to_string(),
                line: *line_no,
                message: format!("expected a `Name: value` header, found `{t}`"),
            });
        };
        spec.headers
            .push((name.trim().to_string(), value.trim().to_string()));
    }

    // Everything left is the body, verbatim apart from trailing blank lines.
    let body: Vec<&str> = iter.map(|(_, l)| *l).collect();
    if !body.is_empty() {
        let joined = body.join("\n");
        let trimmed = joined.trim_end();
        if !trimmed.is_empty() {
            spec.body = Some(trimmed.to_string());
        }
    }

    Ok(Some(spec))
}

fn parse_request_line(
    line: &str,
    line_no: usize,
    file_label: &str,
) -> Result<(String, String, Option<String>)> {
    let mut parts = line.split_whitespace();
    let first = parts.next().ok_or_else(|| Error::Parse {
        file: file_label.to_string(),
        line: line_no,
        message: "empty request line".to_string(),
    })?;

    let upper = first.to_ascii_uppercase();
    let (method, url) = if METHODS.contains(&upper.as_str()) {
        let url = parts.next().ok_or_else(|| Error::Parse {
            file: file_label.to_string(),
            line: line_no,
            message: format!("`{upper}` is missing a url"),
        })?;
        (upper, url.to_string())
    } else {
        // A bare url means GET, same as the editor clients allow.
        (String::from("GET"), first.to_string())
    };

    let version = parts
        .next()
        .filter(|v| v.to_ascii_uppercase().starts_with("HTTP/"))
        .map(|v| v.to_ascii_uppercase());

    Ok((method, url, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_headers_and_body() {
        let src = "### Create a user\n\
                   # @name create_user\n\
                   POST {{base_url}}/users HTTP/1.1\n\
                   Content-Type: application/json\n\
                   Authorization: Bearer {{token}}\n\
                   \n\
                   {\"name\": \"ada\"}\n";
        let reqs = parse(src, "t.http").unwrap();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.name.as_deref(), Some("create_user"));
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "{{base_url}}/users");
        assert_eq!(r.version.as_deref(), Some("HTTP/1.1"));
        assert_eq!(r.headers.len(), 2);
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.body.as_deref(), Some("{\"name\": \"ada\"}"));
    }

    #[test]
    fn splits_multiple_requests_and_keeps_titles() {
        let src = "### one\nGET http://a.test/1\n\n### two\nGET http://a.test/2\n";
        let reqs = parse(src, "t.http").unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].name.as_deref(), Some("one"));
        assert_eq!(reqs[1].name.as_deref(), Some("two"));
        assert_eq!(reqs[1].url, "http://a.test/2");
    }

    #[test]
    fn bare_url_defaults_to_get() {
        let reqs = parse("http://a.test/health\n", "t.http").unwrap();
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].url, "http://a.test/health");
    }

    #[test]
    fn preserves_duplicate_headers_in_order() {
        let src = "GET http://a.test/\nX-T: 1\nX-T: 2\n";
        let reqs = parse(src, "t.http").unwrap();
        let values: Vec<&str> = reqs[0].headers.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, vec!["1", "2"]);
    }

    #[test]
    fn body_keeps_internal_blank_lines() {
        let src = "POST http://a.test/\n\nline1\n\nline3\n";
        let reqs = parse(src, "t.http").unwrap();
        assert_eq!(reqs[0].body.as_deref(), Some("line1\n\nline3"));
    }

    #[test]
    fn malformed_header_reports_its_line() {
        let src = "GET http://a.test/\nnot-a-header\n";
        match parse(src, "t.http") {
            Err(Error::Parse { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn collects_expectations_and_captures_in_order() {
        let src = "### Create
                   # @name create
                   # @expect status 201
                   # @expect json /id != 0
                   # @capture id = json /id
                   # a plain comment
                   POST http://a.test/users
";
        let reqs = parse(src, "t.http").unwrap();
        assert_eq!(reqs[0].expectations.len(), 2);
        assert_eq!(reqs[0].expectations[0].describe(), "status 201");
        assert_eq!(reqs[0].expectations[1].describe(), "json /id != 0");
        assert_eq!(reqs[0].captures.len(), 1);
        assert_eq!(reqs[0].captures[0].describe(), "id = json /id");
    }

    #[test]
    fn a_bad_expectation_reports_its_own_line() {
        let src = "### x
# @name x
# @expect vibes good
GET http://a.test/
";
        match parse(src, "t.http") {
            Err(Error::Parse { line, message, .. }) => {
                assert_eq!(line, 3);
                assert!(message.contains("unknown expectation"), "{message}");
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn requests_without_directives_have_none() {
        let reqs = parse("GET http://a.test/
", "t.http").unwrap();
        assert!(reqs[0].expectations.is_empty());
        assert!(reqs[0].captures.is_empty());
    }

    #[test]
    fn comment_only_file_yields_no_requests() {
        assert!(parse("# just a note\n\n###\n# another\n", "t.http")
            .unwrap()
            .is_empty());
    }
}
