//! Redirect handling.
//!
//! Following a redirect means rewriting a request, and the rewrite rules are
//! where clients quietly differ from each other. They are here, in one place,
//! with the reasoning attached, because "why did my POST become a GET" and
//! "why did my token go to another host" both deserve an answer a user can
//! read.

use http::Uri;

use crate::error::{Error, Result};
use crate::model::RequestSpec;

/// Statuses that carry a `Location` we are willing to follow.
pub const REDIRECT_STATUSES: &[u16] = &[301, 302, 303, 307, 308];

pub fn is_redirect(status: u16) -> bool {
    REDIRECT_STATUSES.contains(&status)
}

/// Rewrite a request to point at the redirect target.
///
/// Two rules worth knowing about:
///
///   * 303 always becomes a GET, and 301/302 do for anything that was not a
///     GET or HEAD. That is what every browser and curl do, however much the
///     original specs waffled, so matching it is less surprising than being
///     right in isolation.
///   * `Authorization` and `Cookie` are dropped when the origin changes. A
///     client that forwards credentials across hosts is one bad redirect away
///     from handing a token to someone else's server.
pub fn rewrite(spec: &RequestSpec, status: u16, target: &str) -> RequestSpec {
    let mut next = spec.clone();
    let cross_origin = origin_of(&spec.url) != origin_of(target);
    next.url = target.to_string();

    let demote = status == 303
        || ((status == 301 || status == 302) && spec.method != "GET" && spec.method != "HEAD");

    if demote {
        next.method = "GET".to_string();
        next.body = None;
        next.headers
            .retain(|(k, _)| !is_payload_header(k));
    }

    if cross_origin {
        next.headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("cookie"));
    }

    // Any Host from the original request described the original target. The
    // engine supplies the right one for the new url.
    next.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("host"));

    next
}

fn is_payload_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-type")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("content-encoding")
}

/// `scheme://authority`, or the input if it will not parse.
fn origin_of(url: &str) -> String {
    match url.parse::<Uri>() {
        Ok(uri) => format!(
            "{}://{}",
            uri.scheme_str().unwrap_or(""),
            uri.authority().map(|a| a.as_str()).unwrap_or("")
        ),
        Err(_) => url.to_string(),
    }
}

/// Turn a `Location` value into an absolute url against the request it came
/// from. Servers send all four forms in practice, and relative ones are common
/// enough that treating them as absolute produces baffling failures.
pub fn resolve(base: &str, location: &str) -> Result<String> {
    let location = location.trim();
    if location.is_empty() {
        return Err(Error::Transport("redirect with an empty Location".into()));
    }

    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(normalize(location));
    }

    let base_uri: Uri = base.parse().map_err(|e: http::uri::InvalidUri| Error::InvalidUrl {
        url: base.to_string(),
        message: e.to_string(),
    })?;
    let scheme = base_uri.scheme_str().unwrap_or("http");
    let authority = base_uri
        .authority()
        .map(|a| a.as_str())
        .ok_or_else(|| Error::InvalidUrl {
            url: base.to_string(),
            message: "cannot resolve a relative redirect against a url with no host".into(),
        })?;

    // Scheme-relative: //other.host/path
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(normalize(&format!("{scheme}://{rest}")));
    }

    // Absolute path: /new/place
    if location.starts_with('/') {
        return Ok(normalize(&format!("{scheme}://{authority}{location}")));
    }

    // Relative to the current directory: ../sibling
    let path = base_uri.path();
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Ok(normalize(&format!("{scheme}://{authority}{dir}{location}")))
}

/// Collapse `.` and `..` in the path, which relative Locations produce and
/// which servers are entitled to reject.
fn normalize(url: &str) -> String {
    let Ok(uri) = url.parse::<Uri>() else {
        return url.to_string();
    };
    let Some(authority) = uri.authority() else {
        return url.to_string();
    };
    let scheme = uri.scheme_str().unwrap_or("http");
    let path = uri.path();
    if !path.contains("./") && !path.ends_with("/.") && !path.ends_with("/..") {
        return url.to_string();
    }

    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut rebuilt = format!("/{}", out.join("/"));
    if trailing && !rebuilt.ends_with('/') {
        rebuilt.push('/');
    }
    match uri.query() {
        Some(q) => format!("{scheme}://{authority}{rebuilt}?{q}"),
        None => format!("{scheme}://{authority}{rebuilt}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post() -> RequestSpec {
        RequestSpec {
            method: "POST".into(),
            url: "https://a.test/one".into(),
            headers: vec![
                ("Authorization".into(), "Bearer t".into()),
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "application/json".into()),
            ],
            body: Some("{}".into()),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_every_location_form() {
        let base = "https://a.test/dir/page?x=1";
        assert_eq!(resolve(base, "https://b.test/x").unwrap(), "https://b.test/x");
        assert_eq!(resolve(base, "//b.test/x").unwrap(), "https://b.test/x");
        assert_eq!(resolve(base, "/root").unwrap(), "https://a.test/root");
        assert_eq!(resolve(base, "sibling").unwrap(), "https://a.test/dir/sibling");
        assert_eq!(resolve(base, "../up").unwrap(), "https://a.test/up");
    }

    #[test]
    fn empty_location_is_an_error() {
        assert!(resolve("https://a.test/", "  ").is_err());
    }

    #[test]
    fn post_becomes_get_on_303_and_loses_its_body() {
        let next = rewrite(&post(), 303, "https://a.test/two");
        assert_eq!(next.method, "GET");
        assert!(next.body.is_none());
        assert!(next.header("content-type").is_none());
        assert_eq!(next.header("accept"), Some("application/json"));
    }

    #[test]
    fn post_survives_307_intact() {
        let next = rewrite(&post(), 307, "https://a.test/two");
        assert_eq!(next.method, "POST");
        assert_eq!(next.body.as_deref(), Some("{}"));
        assert_eq!(next.header("content-type"), Some("application/json"));
    }

    #[test]
    fn same_origin_keeps_credentials() {
        let next = rewrite(&post(), 307, "https://a.test/two");
        assert_eq!(next.header("authorization"), Some("Bearer t"));
    }

    #[test]
    fn cross_origin_drops_credentials() {
        let next = rewrite(&post(), 307, "https://evil.test/two");
        assert!(next.header("authorization").is_none());
    }

    #[test]
    fn a_different_scheme_counts_as_cross_origin() {
        let mut spec = post();
        spec.url = "http://a.test/one".into();
        let next = rewrite(&spec, 307, "https://a.test/one");
        assert!(next.header("authorization").is_none());
    }

    #[test]
    fn stale_host_header_is_dropped() {
        let mut spec = post();
        spec.headers.push(("Host".into(), "old.test".into()));
        let next = rewrite(&spec, 307, "https://a.test/two");
        assert!(next.header("host").is_none());
    }

    #[test]
    fn only_real_redirect_statuses_follow() {
        assert!(is_redirect(301));
        assert!(is_redirect(308));
        assert!(!is_redirect(200));
        assert!(!is_redirect(304));
    }
}
