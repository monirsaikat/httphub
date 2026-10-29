//! The CLI face of httphub.
//!
//! This binary holds argument parsing, terminal rendering and exit codes, and
//! nothing else. Every capability lives in `httphub-core` so the GUI can offer
//! exactly the same one without a second implementation.
//!
//! Exit codes, because CI depends on them:
//!   0  sent, and (with --fail) the status was not an error
//!   1  assertion or --fail check did not pass
//!   2  transport failure: dns, connect, tls, timeout
//!   3  configuration or parse failure, nothing was sent

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use httphub_core::engine;
use httphub_core::run::{self, RequestOutcome, RunOptions, RunReport};
use httphub_core::event::{RequestEvent, Timings};
use httphub_core::model::{ByteCounts, HttpVersion, RequestSpec, ResponseHandle, SendOptions};
use httphub_core::tls::TlsOptions;
use httphub_core::vars::Scope;
use httphub_core::{format, Error};

const MAX_PRINT: u64 = 64 * 1024;

#[derive(Parser)]
#[command(
    name = "httphub",
    version,
    about = "Local-first HTTP client. Your requests are files you own."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send one request from a .http file.
    Send {
        /// Path to a .http file.
        file: PathBuf,
        /// Which request to send, by @name or title. Defaults to the first.
        #[arg(short, long)]
        request: Option<String>,
        /// Environment from httphub.toml.
        #[arg(short, long)]
        env: Option<String>,
        /// Override a variable: --var id=42. Wins over the environment.
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
        /// Pin the protocol instead of letting ALPN choose.
        #[arg(long = "http", value_enum)]
        http: Option<CliVersion>,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Follow redirects. Off by default: seeing the 301 is often the point.
        #[arg(short = 'L', long = "location")]
        follow: bool,
        /// Give up after this many hops.
        #[arg(long, default_value_t = 10, value_name = "N")]
        max_redirects: usize,
        /// Accept any server certificate. Waives trust, not signature checks.
        #[arg(long)]
        insecure: bool,
        /// Extra CA certificates (PEM), added to the web roots rather than
        /// replacing them.
        #[arg(long, value_name = "FILE")]
        ca: Option<PathBuf>,
        /// Client certificate chain (PEM) for mutual TLS. Needs --key.
        #[arg(long, value_name = "FILE")]
        cert: Option<PathBuf>,
        /// Private key (PEM) for --cert.
        #[arg(long, value_name = "FILE")]
        key: Option<PathBuf>,
        /// Send this name in SNI and verify against it, instead of the url's
        /// host. For reaching a gateway by address.
        #[arg(long, value_name = "HOST")]
        sni: Option<String>,
        /// Machine-readable output on stdout, so `| jq` works.
        #[arg(long)]
        json: bool,
        /// Print only the response body.
        #[arg(long)]
        body_only: bool,
        /// Exit 1 when the status is 4xx or 5xx.
        #[arg(long)]
        fail: bool,
        /// Print every engine event as it happens.
        #[arg(long)]
        trace: bool,
    },
    /// Run every request under a path and check their @expect assertions.
    Run {
        /// File or directory to run. Defaults to the working directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short, long)]
        env: Option<String>,
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
        /// Output shape. `junit` writes XML for CI to collect.
        #[arg(long, value_enum, default_value_t = Reporter::Human)]
        reporter: Reporter,
        /// Stop at the first failing request.
        #[arg(long)]
        bail: bool,
        /// Per-request timeout, seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        #[arg(short = 'L', long = "location")]
        follow: bool,
        #[arg(long)]
        insecure: bool,
        #[arg(long, value_name = "FILE")]
        ca: Option<PathBuf>,
        #[arg(long)]
        trace: bool,
    },
    /// Parse a .http file and show what the engine would send. No network.
    Parse {
        file: PathBuf,
        #[arg(short, long)]
        env: Option<String>,
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Reporter {
    Human,
    Json,
    Junit,
}

#[derive(Copy, Clone, ValueEnum)]
enum CliVersion {
    #[value(name = "1.1")]
    Http11,
    #[value(name = "2")]
    Http2,
}

impl From<CliVersion> for HttpVersion {
    fn from(v: CliVersion) -> Self {
        match v {
            CliVersion::Http11 => HttpVersion::Http11,
            CliVersion::Http2 => HttpVersion::Http2,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("httphub: {err}");
            ExitCode::from(exit_code_for(&err))
        }
    }
}

fn exit_code_for(err: &Error) -> u8 {
    match err {
        Error::Parse { .. }
        | Error::UnresolvedVar(_)
        | Error::NoSuchRequest(_)
        | Error::NoRequests(_)
        | Error::InvalidUrl { .. }
        | Error::Config(_) => 3,
        _ => 2,
    }
}

async fn run(cli: Cli) -> Result<ExitCode, Error> {
    match cli.command {
        Command::Parse {
            file,
            env,
            vars,
            json,
        } => {
            let spec = resolve_one(&file, None, env.as_deref(), &vars)?;
            if json {
                println!("{}", spec_to_json(&spec));
            } else {
                print_spec(&spec);
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Run {
            path,
            env,
            vars,
            reporter,
            bail,
            timeout,
            follow,
            insecure,
            ca,
            trace,
        } => {
            let files = run::discover(&path)?;
            if files.is_empty() {
                return Err(Error::Config(format!(
                    "no .http files under {}",
                    path.display()
                )));
            }

            let tls = TlsOptions {
                insecure,
                ca_file: ca,
                ..Default::default()
            };
            if tls.insecure {
                eprintln!("warning: --insecure, certificate trust is not being checked");
            }

            let opts = RunOptions {
                send: SendOptions {
                    timeout: Duration::from_secs(timeout),
                    force_version: None,
                    follow_redirects: follow,
                    max_redirects: 10,
                    store_dir: store_dir(&path),
                    tls,
                },
                bail,
            };

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let printer = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if trace {
                        eprintln!("  · {}", describe(&event));
                    }
                }
            });

            let scope = build_scope(&path, env.as_deref(), &vars)?;
            let report = run::run(&files, &scope, &opts, Some(&tx)).await?;
            drop(tx);
            let _ = printer.await;

            match reporter {
                Reporter::Human => print_run(&report),
                Reporter::Json => println!("{}", run_json(&report)),
                Reporter::Junit => println!("{}", run_junit(&report)),
            }

            Ok(if report.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }

        Command::Send {
            file,
            request,
            env,
            vars,
            http,
            timeout,
            follow,
            max_redirects,
            insecure,
            ca,
            cert,
            key,
            sni,
            json,
            body_only,
            fail,
            trace,
        } => {
            let spec = resolve_one(&file, request.as_deref(), env.as_deref(), &vars)?;

            let tls = TlsOptions {
                insecure,
                ca_file: ca,
                client_cert: cert,
                client_key: key,
                sni,
            };
            tls.validate()?;
            if tls.insecure {
                eprintln!("warning: --insecure, certificate trust is not being checked");
            }

            let opts = SendOptions {
                timeout: Duration::from_secs(timeout),
                force_version: http.map(Into::into),
                follow_redirects: follow,
                max_redirects,
                store_dir: store_dir(&file),
                tls,
            };

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let printer = tokio::spawn(async move {
                let mut seen = Vec::new();
                while let Some(event) = rx.recv().await {
                    if trace {
                        eprintln!("  · {}", describe(&event));
                    }
                    seen.push(event);
                }
                seen
            });

            if !json && !body_only {
                eprintln!("{} {}", spec.method, spec.url);
            }

            let result = engine::send(&spec, &opts, Some(&tx)).await;
            drop(tx);
            let _ = printer.await;
            let response = result?;

            if json {
                println!("{}", response_to_json(&spec, &response));
            } else if body_only {
                dump_body(&response, u64::MAX)?;
            } else {
                print_response(&response)?;
            }

            Ok(if fail && response.status >= 400 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

/// Load a file, pick a request, and resolve its variables.
fn resolve_one(
    file: &Path,
    name: Option<&str>,
    env: Option<&str>,
    overrides: &[String],
) -> Result<RequestSpec, Error> {
    let source = std::fs::read_to_string(file)?;
    let label = file.display().to_string();
    let specs = format::parse(&source, &label)?;

    let spec = match name {
        Some(wanted) => specs
            .into_iter()
            .find(|s| s.name.as_deref() == Some(wanted))
            .ok_or_else(|| Error::NoSuchRequest(wanted.to_string()))?,
        None => specs
            .into_iter()
            .next()
            .ok_or_else(|| Error::NoRequests(file.to_path_buf()))?,
    };

    build_scope(file, env, overrides)?.resolve(&spec)
}

/// Environment file first, then `--var`, which wins.
fn build_scope(from: &Path, env: Option<&str>, overrides: &[String]) -> Result<Scope, Error> {
    let mut scope = Scope::new();
    if let Some(env) = env {
        scope.extend(load_env(from, env)?);
    }
    for pair in overrides {
        let (k, v) = pair.split_once('=').ok_or_else(|| Error::Parse {
            file: "--var".to_string(),
            line: 0,
            message: format!("expected KEY=VALUE, got `{pair}`"),
        })?;
        scope.set(k, v);
    }
    Ok(scope)
}

/// Walk up from the request file looking for `httphub.toml`, the way git finds
/// its root. Environments are plain tables of strings.
fn load_env(from: &Path, env: &str) -> Result<BTreeMap<String, String>, Error> {
    let manifest = find_up(from, "httphub.toml").ok_or_else(|| Error::Parse {
        file: "httphub.toml".to_string(),
        line: 0,
        message: format!("no httphub.toml found above {}, so --env {env} cannot resolve", from.display()),
    })?;

    let text = std::fs::read_to_string(&manifest)?;
    let value: toml::Value = text.parse().map_err(|e: toml::de::Error| Error::Parse {
        file: manifest.display().to_string(),
        line: 0,
        message: e.to_string(),
    })?;

    let table = value
        .get("env")
        .and_then(|e| e.get(env))
        .and_then(|e| e.as_table())
        .ok_or_else(|| Error::Parse {
            file: manifest.display().to_string(),
            line: 0,
            message: format!("no [env.{env}] table"),
        })?;

    Ok(table
        .iter()
        .map(|(k, v)| {
            let v = match v {
                toml::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), v)
        })
        .collect())
}

fn find_up(from: &Path, filename: &str) -> Option<PathBuf> {
    let start = if from.is_dir() {
        from.to_path_buf()
    } else {
        from.parent()?.to_path_buf()
    };
    let mut dir = std::fs::canonicalize(&start).unwrap_or(start);
    loop {
        let candidate = dir.join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Derived state sits beside the workspace, never inside it.
fn store_dir(from: &Path) -> PathBuf {
    match find_up(from, "httphub.toml").and_then(|m| m.parent().map(Path::to_path_buf)) {
        Some(root) => root.join(".httphub").join("responses"),
        None => PathBuf::from(".httphub").join("responses"),
    }
}

// --- rendering -----------------------------------------------------------

fn print_spec(spec: &RequestSpec) {
    println!("{} {}", spec.method, spec.url);
    if let Some(name) = &spec.name {
        println!("  name: {name}");
    }
    for (k, v) in &spec.headers {
        println!("  {k}: {v}");
    }
    if let Some(body) = &spec.body {
        println!();
        println!("{body}");
    }
}

fn print_response(response: &ResponseHandle) -> Result<(), Error> {
    for (i, hop) in response.redirects.iter().enumerate() {
        eprintln!(
            "   {}. {} {}  ->  {}  ({})",
            i + 1,
            hop.status,
            hop.url,
            hop.location,
            format_duration(hop.timings.total.unwrap_or_default())
        );
    }
    if !response.redirects.is_empty() {
        eprintln!("   -> {}", response.url);
    }

    eprintln!(
        "{} {} {}  {}  in {}",
        arrow(),
        response.status,
        response.status_text,
        response.version,
        format_duration(response.timings.total.unwrap_or_default())
    );

    if let Some(tls) = &response.tls {
        eprintln!(
            "  {} {}",
            tls.protocol,
            tls.alpn.as_deref().unwrap_or("no alpn")
        );
    }

    eprintln!("  {}", waterfall(&response.timings));
    eprintln!("  {}", byte_line(&response.bytes));
    eprintln!();
    for (k, v) in &response.headers {
        eprintln!("  {k}: {v}");
    }
    eprintln!();

    dump_body(response, MAX_PRINT)?;
    Ok(())
}

fn arrow() -> &'static str {
    "->"
}

fn waterfall(t: &Timings) -> String {
    let phases = t.phases();
    if phases.is_empty() {
        return "no timings".to_string();
    }
    phases
        .iter()
        .map(|(label, d)| format!("{label} {}", format_duration(*d)))
        .collect::<Vec<_>>()
        .join("  ")
}

/// Plaintext first, since that is what the user wrote and read. The wire
/// figures only appear when TLS makes them differ.
fn byte_line(b: &ByteCounts) -> String {
    let base = format!(
        "sent {}  received {}",
        format_bytes(b.app_sent),
        format_bytes(b.app_received)
    );
    if b.encrypted {
        format!(
            "{base}  (wire {} up / {} down)",
            format_bytes(b.wire_sent),
            format_bytes(b.wire_received)
        )
    } else {
        base
    }
}

/// Bodies live on disk; printing is a read, never a buffer we grew ourselves.
fn dump_body(response: &ResponseHandle, limit: u64) -> Result<(), Error> {
    if response.body_len == 0 {
        return Ok(());
    }

    if response.body_len > limit {
        eprintln!(
            "  [{} body not printed; full response at {}]",
            format_bytes(response.body_len),
            response.body_path.display()
        );
        return Ok(());
    }

    let bytes = std::fs::read(&response.body_path)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let is_json = response
        .content_type()
        .map(|ct| ct.contains("json"))
        .unwrap_or(false);

    if is_json {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap_or_default())?;
            return Ok(());
        }
    }

    out.write_all(&bytes)?;
    if !bytes.ends_with(b"\n") {
        writeln!(out)?;
    }
    Ok(())
}

fn spec_to_json(spec: &RequestSpec) -> String {
    let headers: serde_json::Map<String, serde_json::Value> = spec
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    serde_json::json!({
        "name": spec.name,
        "method": spec.method,
        "url": spec.url,
        "version": spec.version,
        "headers": headers,
        "body": spec.body,
        "line": spec.line,
    })
    .to_string()
}

fn response_to_json(spec: &RequestSpec, response: &ResponseHandle) -> String {
    let headers: serde_json::Map<String, serde_json::Value> = response
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();

    let ms = |d: Option<Duration>| {
        d.map(|d| serde_json::json!(d.as_secs_f64() * 1000.0))
            .unwrap_or(serde_json::Value::Null)
    };

    serde_json::json!({
        "request": { "name": spec.name, "method": spec.method, "url": spec.url },
        "url": response.url,
        "redirects": response.redirects.iter().map(|h| serde_json::json!({
            "url": h.url,
            "status": h.status,
            "location": h.location,
            "total_ms": h.timings.total.map(|d| d.as_secs_f64() * 1000.0),
        })).collect::<Vec<_>>(),
        "status": response.status,
        "status_text": response.status_text,
        "version": response.version,
        "headers": headers,
        "remote_addr": response.remote_addr.map(|a| a.to_string()),
        "tls": response.tls.as_ref().map(|t| serde_json::json!({
            "protocol": t.protocol,
            "alpn": t.alpn,
        })),
        "bytes": {
            "app_sent": response.bytes.app_sent,
            "app_received": response.bytes.app_received,
            "wire_sent": response.bytes.wire_sent,
            "wire_received": response.bytes.wire_received,
            "encrypted": response.bytes.encrypted,
        },
        "body_path": response.body_path,
        "body_len": response.body_len,
        "timings_ms": {
            "dns": ms(response.timings.dns),
            "connect": ms(response.timings.connect),
            "tls": ms(response.timings.tls),
            "sent": ms(response.timings.sent),
            "ttfb": ms(response.timings.ttfb),
            "total": ms(response.timings.total),
        },
    })
    .to_string()
}

fn describe(event: &RequestEvent) -> String {
    match event {
        RequestEvent::Resolving { host } => format!("resolving {host}"),
        RequestEvent::Resolved { addrs, at } => format!(
            "resolved {} at {}",
            addrs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            format_duration(*at)
        ),
        RequestEvent::Connecting { addr } => format!("connecting to {addr}"),
        RequestEvent::Connected { addr, at } => {
            format!("connected to {addr} at {}", format_duration(*at))
        }
        RequestEvent::TlsEstablished { protocol, alpn, at } => format!(
            "tls {protocol} alpn={} at {}",
            alpn.as_deref().unwrap_or("none"),
            format_duration(*at)
        ),
        RequestEvent::RequestSent { bytes, at } => {
            format!("sent ~{} at {}", format_bytes(*bytes as u64), format_duration(*at))
        }
        RequestEvent::FirstByte { status, at } => {
            format!("first byte, status {status} at {}", format_duration(*at))
        }
        RequestEvent::BodyChunk { bytes, total } => format!(
            "body chunk {} ({} so far)",
            format_bytes(*bytes as u64),
            format_bytes(*total)
        ),
        RequestEvent::Redirect {
            hop,
            status,
            to,
            at,
            ..
        } => format!("redirect {hop}: {status} -> {to} at {}", format_duration(*at)),
        RequestEvent::Complete { at, body_len } => format!(
            "complete, {} in {}",
            format_bytes(*body_len),
            format_duration(*at)
        ),
        RequestEvent::Failed { message, at } => {
            format!("failed after {}: {message}", format_duration(*at))
        }
    }
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.0}us", d.as_micros())
    } else if ms < 10.0 {
        format!("{ms:.1}ms")
    } else if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let n_f = n as f64;
    if n_f < KB {
        format!("{n} B")
    } else if n_f < KB * KB {
        format!("{:.1} KB", n_f / KB)
    } else if n_f < KB * KB * KB {
        format!("{:.1} MB", n_f / (KB * KB))
    } else {
        format!("{:.2} GB", n_f / (KB * KB * KB))
    }
}

// --- run reporters -------------------------------------------------------

/// Human output goes to stderr, so `--reporter json` is the only thing on
/// stdout and a pipe stays clean either way.
fn print_run(report: &RunReport) {
    let mut current: Option<&Path> = None;

    for outcome in &report.outcomes {
        if current != Some(outcome.file.as_path()) {
            if current.is_some() {
                eprintln!();
            }
            eprintln!("{}", display_path(&outcome.file));
            current = Some(outcome.file.as_path());
        }

        let status = match outcome.status {
            Some(s) => s.to_string(),
            None => "---".to_string(),
        };
        eprintln!(
            "  {}  {:<26} {:>3}  {:>8}  {}",
            if outcome.passed() { "ok  " } else { "FAIL" },
            clip(&outcome.label, 26),
            status,
            format_duration(outcome.elapsed),
            check_summary(outcome),
        );

        if let Some(error) = &outcome.error {
            eprintln!("          {error}");
        }
        for check in outcome.checks.iter().filter(|c| !c.passed()) {
            eprintln!(
                "          {} -- {}",
                check.expectation,
                check.failure.as_deref().unwrap_or("failed")
            );
        }
        if !outcome.captured.is_empty() {
            let shown: Vec<String> = outcome
                .captured
                .iter()
                .map(|(k, v)| format!("{k}={}", clip(v, 32)))
                .collect();
            eprintln!("          captured {}", shown.join(", "));
        }
    }

    eprintln!();
    eprintln!(
        "{} requests, {} passed, {} failed  |  {} checks, {} failed  in {}",
        report.outcomes.len(),
        report.outcomes.len() - report.failed_requests(),
        report.failed_requests(),
        report.total_checks(),
        report.failed_checks(),
        format_duration(report.elapsed),
    );
    if report.stopped_early {
        eprintln!("stopped early: --bail");
    }
}

fn check_summary(outcome: &RequestOutcome) -> String {
    let failed = outcome.checks.iter().filter(|c| !c.passed()).count();
    match (outcome.checks.len(), failed) {
        (0, _) => String::new(),
        (1, 0) => "1 check".to_string(),
        (n, 0) => format!("{n} checks"),
        (n, f) => format!("{f}/{n} checks failed"),
    }
}

/// Reports read better, and diff better between machines, with one separator.
fn display_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn clip(text: &str, width: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if flat.chars().count() <= width {
        flat
    } else {
        format!("{}...", flat.chars().take(width.saturating_sub(3)).collect::<String>())
    }
}

fn run_json(report: &RunReport) -> String {
    let outcomes: Vec<serde_json::Value> = report
        .outcomes
        .iter()
        .map(|o| {
            serde_json::json!({
                "file": display_path(&o.file),
                "label": o.label,
                "method": o.method,
                "url": o.url,
                "status": o.status,
                "elapsed_ms": o.elapsed.as_secs_f64() * 1000.0,
                "passed": o.passed(),
                "error": o.error,
                "checks": o.checks.iter().map(|c| serde_json::json!({
                    "expectation": c.expectation,
                    "passed": c.passed(),
                    "failure": c.failure,
                })).collect::<Vec<_>>(),
                "captured": o.captured.iter().map(|(k, v)| {
                    (k.clone(), serde_json::Value::String(v.clone()))
                }).collect::<serde_json::Map<String, serde_json::Value>>(),
            })
        })
        .collect();

    serde_json::json!({
        "passed": report.passed(),
        "requests": report.outcomes.len(),
        "failed_requests": report.failed_requests(),
        "checks": report.total_checks(),
        "failed_checks": report.failed_checks(),
        "elapsed_ms": report.elapsed.as_secs_f64() * 1000.0,
        "stopped_early": report.stopped_early,
        "outcomes": outcomes,
    })
    .to_string()
}

/// JUnit XML, because that is what every CI system already knows how to read.
fn run_junit(report: &RunReport) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuites tests=\"{}\" failures=\"{}\" time=\"{:.3}\">\n",
        report.outcomes.len(),
        report.failed_requests(),
        report.elapsed.as_secs_f64()
    ));

    for (file, outcomes) in group_by_file(report) {
        let failures = outcomes.iter().filter(|o| !o.passed()).count();
        let time: f64 = outcomes.iter().map(|o| o.elapsed.as_secs_f64()).sum();
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\" time=\"{time:.3}\">\n",
            xml(&display_path(&file)),
            outcomes.len(),
        ));

        for outcome in outcomes {
            out.push_str(&format!(
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\"",
                xml(&outcome.label),
                xml(&display_path(&file)),
                outcome.elapsed.as_secs_f64()
            ));

            let mut problems: Vec<String> = Vec::new();
            if let Some(error) = &outcome.error {
                problems.push(format!("could not send: {error}"));
            }
            for check in outcome.checks.iter().filter(|c| !c.passed()) {
                problems.push(format!(
                    "{} -- {}",
                    check.expectation,
                    check.failure.as_deref().unwrap_or("failed")
                ));
            }

            if problems.is_empty() {
                out.push_str("/>\n");
            } else {
                out.push_str(">\n");
                out.push_str(&format!(
                    "      <failure message=\"{}\">{}</failure>\n",
                    xml(&problems[0]),
                    xml(&problems.join("\n"))
                ));
                out.push_str("    </testcase>\n");
            }
        }
        out.push_str("  </testsuite>\n");
    }

    out.push_str("</testsuites>");
    out
}

/// Outcomes keep file order, so grouping is a fold rather than a sort.
fn group_by_file(report: &RunReport) -> Vec<(PathBuf, Vec<&RequestOutcome>)> {
    let mut grouped: Vec<(PathBuf, Vec<&RequestOutcome>)> = Vec::new();
    for outcome in &report.outcomes {
        match grouped.last_mut() {
            Some((file, bucket)) if *file == outcome.file => bucket.push(outcome),
            _ => grouped.push((outcome.file.clone(), vec![outcome])),
        }
    }
    grouped
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
