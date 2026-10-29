use std::path::PathBuf;
use std::time::Duration;

use crate::event::Timings;
use crate::expect::{Capture, Expectation};
use crate::tls::TlsOptions;

/// A single request as written in a `.http` file, before variable resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestSpec {
    /// From `# @name foo`, or the `### title` separator, if either is present.
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    /// Explicit `HTTP/1.1` or `HTTP/2` on the request line, if written.
    pub version: Option<String>,
    /// Kept as an ordered list: duplicate headers are meaningful and the
    /// on-the-wire order is something we promise not to silently rewrite.
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Assertions from `# @expect` lines, in the order written.
    pub expectations: Vec<Expectation>,
    /// Values to lift out of the response for later requests in a run.
    pub captures: Vec<Capture>,
    /// 1-indexed line of the request line in the source file, for diagnostics.
    pub line: usize,
}

impl RequestSpec {
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None => format!("{} {}", self.method, self.url),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    Http11,
    Http2,
}

#[derive(Debug, Clone)]
pub struct SendOptions {
    pub timeout: Duration,
    /// Force a protocol instead of taking whatever ALPN negotiates.
    pub force_version: Option<HttpVersion>,
    pub follow_redirects: bool,
    /// Only consulted when `follow_redirects` is set.
    pub max_redirects: usize,
    /// Where response bodies are streamed. Derived, disposable state.
    pub store_dir: PathBuf,
    pub tls: TlsOptions,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            force_version: None,
            follow_redirects: false,
            max_redirects: 10,
            store_dir: PathBuf::from(".httphub/responses"),
            tls: TlsOptions::default(),
        }
    }
}

/// One hop that was followed on the way to the final response.
#[derive(Debug, Clone)]
pub struct RedirectHop {
    pub url: String,
    pub status: u16,
    pub location: String,
    pub timings: Timings,
}

/// What a send returns: metadata plus a *path*. The body itself is on disk so
/// that a 2 GB response costs us a file handle instead of a heap allocation.
#[derive(Debug, Clone)]
pub struct ResponseHandle {
    /// The url that produced this response, which differs from the request's
    /// when redirects were followed.
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body_path: PathBuf,
    pub body_len: u64,
    pub timings: Timings,
    pub bytes: ByteCounts,
    /// Hops followed to get here, in order. Empty unless following was on.
    pub redirects: Vec<RedirectHop>,
    pub remote_addr: Option<std::net::SocketAddr>,
    pub tls: Option<TlsInfo>,
}

/// What crossed the socket, as measured rather than guessed. `wire_*` is what
/// the network carried; `app_*` is plaintext. Their difference is the cost of
/// TLS and framing, which no other client bothers to show.
#[derive(Debug, Clone, Copy, Default)]
pub struct ByteCounts {
    pub wire_sent: u64,
    pub wire_received: u64,
    pub app_sent: u64,
    pub app_received: u64,
    pub encrypted: bool,
}

#[derive(Debug, Clone)]
pub struct TlsInfo {
    pub protocol: String,
    pub alpn: Option<String>,
}

impl ResponseHandle {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}
