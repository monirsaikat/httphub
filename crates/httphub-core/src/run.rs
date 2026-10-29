//! Collection runs.
//!
//! The CI story, in the same binary as the interactive one. Newman is a
//! separate project that lags the GUI it serves, and Postman's newer CLI wants
//! an account; either way the thing that runs in CI is not the thing people
//! use at their desk. Here it is one engine, one file format, one binary.
//!
//! Requests run in file order, and captures from one are visible to the next,
//! so a login-then-use chain needs no scripting.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::engine;
use crate::error::Result;
use crate::event::EventSink;
use crate::format;
use crate::model::SendOptions;
use crate::vars::Scope;

/// Bodies above this are not read back for assertions; a run should not blow
/// up on a request that happens to download a release artifact.
const MAX_ASSERTABLE_BODY: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub send: SendOptions,
    /// Stop at the first failing request instead of running the rest.
    pub bail: bool,
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub expectation: String,
    /// `None` when satisfied; otherwise what was actually seen.
    pub failure: Option<String>,
}

impl CheckOutcome {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct RequestOutcome {
    pub file: PathBuf,
    pub label: String,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub elapsed: Duration,
    pub checks: Vec<CheckOutcome>,
    pub captured: Vec<(String, String)>,
    /// Set when the request could not be sent or resolved at all.
    pub error: Option<String>,
}

impl RequestOutcome {
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.checks.iter().all(CheckOutcome::passed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub outcomes: Vec<RequestOutcome>,
    pub elapsed: Duration,
    /// True when `bail` cut the run short.
    pub stopped_early: bool,
}

impl RunReport {
    pub fn passed(&self) -> bool {
        self.outcomes.iter().all(RequestOutcome::passed)
    }

    pub fn failed_requests(&self) -> usize {
        self.outcomes.iter().filter(|o| !o.passed()).count()
    }

    pub fn total_checks(&self) -> usize {
        self.outcomes.iter().map(|o| o.checks.len()).sum()
    }

    pub fn failed_checks(&self) -> usize {
        self.outcomes
            .iter()
            .flat_map(|o| &o.checks)
            .filter(|c| !c.passed())
            .count()
    }
}

/// Collect `.http` files under a path, sorted so that a run is reproducible
/// and a diff of two reports lines up.
pub fn discover(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    if root.is_file() {
        found.push(root.to_path_buf());
        return Ok(found);
    }
    walk(root, &mut found)?;
    found.sort();
    Ok(found)
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Derived state and version control are not request collections.
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }

        if path.is_dir() {
            walk(&path, found)?;
        } else if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("http") || e.eq_ignore_ascii_case("rest"))
            .unwrap_or(false)
        {
            found.push(path);
        }
    }
    Ok(())
}

/// Run every request in `files`, in order, sharing one variable scope.
///
/// A request that cannot be sent is recorded and the run continues, because a
/// report covering every request is more useful than stopping at the first
/// network blip -- unless `bail` says otherwise.
pub async fn run(
    files: &[PathBuf],
    base: &Scope,
    opts: &RunOptions,
    events: Option<&EventSink>,
) -> Result<RunReport> {
    let started = Instant::now();
    let mut scope = base.clone();
    let mut report = RunReport::default();

    'files: for file in files {
        let source = std::fs::read_to_string(file)?;
        let label = file.display().to_string();
        let specs = format::parse(&source, &label)?;

        for spec in specs {
            let began = Instant::now();

            // Resolution can fail here even though the file parsed: an earlier
            // capture may not have produced the variable this one needs.
            let resolved = match scope.resolve(&spec) {
                Ok(r) => r,
                Err(e) => {
                    report.outcomes.push(RequestOutcome {
                        file: file.clone(),
                        label: spec.label(),
                        method: spec.method.clone(),
                        url: spec.url.clone(),
                        status: None,
                        elapsed: began.elapsed(),
                        checks: Vec::new(),
                        captured: Vec::new(),
                        error: Some(e.to_string()),
                    });
                    if opts.bail {
                        report.stopped_early = true;
                        break 'files;
                    }
                    continue;
                }
            };

            let mut outcome = RequestOutcome {
                file: file.clone(),
                label: resolved.label(),
                method: resolved.method.clone(),
                url: resolved.url.clone(),
                status: None,
                elapsed: Duration::ZERO,
                checks: Vec::new(),
                captured: Vec::new(),
                error: None,
            };

            match engine::send(&resolved, &opts.send, events).await {
                Ok(response) => {
                    outcome.status = Some(response.status);
                    outcome.url = response.url.clone();

                    let body = read_body(&response.body_path, response.body_len);

                    for expectation in &resolved.expectations {
                        outcome.checks.push(CheckOutcome {
                            expectation: expectation.describe(),
                            failure: expectation.check(&response, &body).err(),
                        });
                    }

                    // Captures feed the next request, so a failed capture is a
                    // failed check rather than a silent empty variable.
                    for capture in &resolved.captures {
                        match capture.extract(&response, &body) {
                            Ok(value) => {
                                scope.set(&capture.var, &value);
                                outcome.captured.push((capture.var.clone(), value));
                            }
                            Err(why) => outcome.checks.push(CheckOutcome {
                                expectation: format!("capture {}", capture.describe()),
                                failure: Some(why),
                            }),
                        }
                    }
                }
                Err(e) => outcome.error = Some(e.to_string()),
            }

            outcome.elapsed = began.elapsed();
            let failed = !outcome.passed();
            report.outcomes.push(outcome);

            if failed && opts.bail {
                report.stopped_early = true;
                break 'files;
            }
        }
    }

    report.elapsed = started.elapsed();
    Ok(report)
}

/// Assertions need bytes, but a run should survive a large download.
fn read_body(path: &Path, len: u64) -> Vec<u8> {
    if len == 0 || len > MAX_ASSERTABLE_BODY {
        return Vec::new();
    }
    std::fs::read(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("httphub-run-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, body: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovery_is_recursive_sorted_and_skips_derived_state() {
        let tree = TempTree::new("discover");
        tree.file("b/second.http", "GET http://a.test/2\n");
        tree.file("a/first.http", "GET http://a.test/1\n");
        tree.file("notes.md", "not a request\n");
        tree.file(".httphub/responses/cached.http", "GET http://a.test/x\n");
        tree.file("target/debug/junk.http", "GET http://a.test/x\n");
        tree.file("legacy.rest", "GET http://a.test/3\n");

        let found = discover(&tree.0).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&tree.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert_eq!(names, vec!["a/first.http", "b/second.http", "legacy.rest"]);
    }

    #[test]
    fn a_single_file_discovers_as_itself() {
        let tree = TempTree::new("single");
        tree.file("one.http", "GET http://a.test/1\n");
        let path = tree.0.join("one.http");
        assert_eq!(discover(&path).unwrap(), vec![path]);
    }

    #[test]
    fn report_counts_distinguish_requests_from_checks() {
        let report = RunReport {
            outcomes: vec![
                RequestOutcome {
                    file: PathBuf::from("a.http"),
                    label: "ok".into(),
                    method: "GET".into(),
                    url: "http://a.test/".into(),
                    status: Some(200),
                    elapsed: Duration::ZERO,
                    checks: vec![CheckOutcome {
                        expectation: "status 200".into(),
                        failure: None,
                    }],
                    captured: Vec::new(),
                    error: None,
                },
                RequestOutcome {
                    file: PathBuf::from("b.http"),
                    label: "bad".into(),
                    method: "GET".into(),
                    url: "http://a.test/".into(),
                    status: Some(500),
                    elapsed: Duration::ZERO,
                    checks: vec![
                        CheckOutcome {
                            expectation: "status 200".into(),
                            failure: Some("got 500".into()),
                        },
                        CheckOutcome {
                            expectation: "duration < 100".into(),
                            failure: None,
                        },
                    ],
                    captured: Vec::new(),
                    error: None,
                },
            ],
            elapsed: Duration::from_millis(5),
            stopped_early: false,
        };

        assert!(!report.passed());
        assert_eq!(report.failed_requests(), 1);
        assert_eq!(report.total_checks(), 3);
        assert_eq!(report.failed_checks(), 1);
    }

    #[test]
    fn a_transport_error_fails_the_request_with_no_checks() {
        let outcome = RequestOutcome {
            file: PathBuf::from("a.http"),
            label: "down".into(),
            method: "GET".into(),
            url: "http://a.test/".into(),
            status: None,
            elapsed: Duration::ZERO,
            checks: Vec::new(),
            captured: Vec::new(),
            error: Some("connect refused".into()),
        };
        assert!(!outcome.passed());
    }
}
