//! The send engine.
//!
//! Built on `hyper` at the connection level rather than on a convenience
//! client, because the control that buys us is the whole point: per-request
//! protocol choice, real per-phase timings, the exact bytes on the wire, and
//! response bodies that stream to disk instead of into our own heap. A client
//! that borrows the browser's network stack can never offer any of that.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::error::{Error, Result};
use crate::event::{EventSink, RequestEvent, Timings};
use crate::model::{HttpVersion, RequestSpec, ResponseHandle, SendOptions, TlsInfo};

/// Send one fully resolved request. `events` receives progress as it happens;
/// the returned handle is metadata plus a path to the body on disk.
pub async fn send(
    spec: &RequestSpec,
    opts: &SendOptions,
    events: Option<&EventSink>,
) -> Result<ResponseHandle> {
    let start = Instant::now();
    let outcome = match tokio::time::timeout(opts.timeout, exchange(spec, opts, events, start)).await
    {
        Ok(result) => result,
        Err(_) => Err(Error::Timeout(opts.timeout)),
    };

    if let Err(err) = &outcome {
        emit(
            events,
            RequestEvent::Failed {
                message: err.to_string(),
                at: start.elapsed(),
            },
        );
    }
    outcome
}

async fn exchange(
    spec: &RequestSpec,
    opts: &SendOptions,
    events: Option<&EventSink>,
    start: Instant,
) -> Result<ResponseHandle> {
    let mut timings = Timings::default();

    let uri: Uri = spec.url.parse().map_err(|e: http::uri::InvalidUri| Error::InvalidUrl {
        url: spec.url.clone(),
        message: e.to_string(),
    })?;
    let scheme = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
    let secure = scheme == "https";
    let host = uri
        .host()
        .ok_or_else(|| Error::InvalidUrl {
            url: spec.url.clone(),
            message: "no host; urls must be absolute".to_string(),
        })?
        .to_string();
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });

    // --- DNS -------------------------------------------------------------
    emit(events, RequestEvent::Resolving { host: host.clone() });
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| Error::Transport(format!("dns lookup for {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::DnsEmpty { host });
    }
    timings.dns = Some(start.elapsed());
    emit(
        events,
        RequestEvent::Resolved {
            addrs: addrs.clone(),
            at: start.elapsed(),
        },
    );

    // --- TCP, trying each address in turn --------------------------------
    let mut last_err = None;
    let mut connected = None;
    for addr in &addrs {
        emit(events, RequestEvent::Connecting { addr: *addr });
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                connected = Some((stream, *addr));
                break;
            }
            Err(e) => last_err = Some((e, *addr)),
        }
    }
    let (tcp, remote_addr) = connected.ok_or_else(|| {
        let (e, addr) = last_err.expect("at least one address was tried");
        Error::Transport(format!("connect to {addr}: {e}"))
    })?;
    tcp.set_nodelay(true).ok();
    timings.connect = Some(start.elapsed());
    emit(
        events,
        RequestEvent::Connected {
            addr: remote_addr,
            at: start.elapsed(),
        },
    );

    // --- TLS, then protocol selection ------------------------------------
    // ALPN decides unless the request pinned a version, which is what makes
    // "reproduce this bug on HTTP/1.1" a flag rather than a rebuild.
    let mut tls_info = None;
    let sender = if secure {
        let tls = tls_connector(opts.force_version)
            .connect(
                rustls::pki_types::ServerName::try_from(host.clone())
                    .map_err(|e| Error::Tls(format!("invalid sni name {host}: {e}")))?,
                tcp,
            )
            .await
            .map_err(|e| Error::Tls(e.to_string()))?;

        let (_, session) = tls.get_ref();
        let alpn = session
            .alpn_protocol()
            .map(|p| String::from_utf8_lossy(p).to_string());
        let protocol = session
            .protocol_version()
            .map(describe_tls_version)
            .unwrap_or("unknown")
            .to_string();
        tls_info = Some(TlsInfo {
            protocol: protocol.clone(),
            alpn: alpn.clone(),
        });
        timings.tls = Some(start.elapsed());
        emit(
            events,
            RequestEvent::TlsEstablished {
                protocol,
                alpn: alpn.clone(),
                at: start.elapsed(),
            },
        );

        let h2 = match opts.force_version {
            Some(v) => v == HttpVersion::Http2,
            None => alpn.as_deref() == Some("h2"),
        };
        handshake(TokioIo::new(tls), h2).await?
    } else {
        // Plaintext h2 only on request: prior-knowledge h2c, no upgrade dance.
        let h2 = opts.force_version == Some(HttpVersion::Http2);
        handshake(TokioIo::new(tcp), h2).await?
    };

    // --- Request ----------------------------------------------------------
    let is_h2 = matches!(sender, Sender::H2(_));
    let request = build_request(spec, &uri, is_h2)?;
    let approx_bytes = approx_head_len(&request) + spec.body.as_ref().map_or(0, |b| b.len());

    // Stamped before the await on purpose: `send_request` resolves when the
    // response *head* arrives, so timing it there would bill the server's
    // think time to our upload. Exact flush timing needs an instrumented IO
    // wrapper around the socket -- see README, "instrumented transport".
    timings.sent = Some(start.elapsed());
    emit(
        events,
        RequestEvent::RequestSent {
            bytes: approx_bytes,
            at: start.elapsed(),
        },
    );

    let response = match sender {
        Sender::H1(mut tx) => tx.send_request(request).await?,
        Sender::H2(mut tx) => tx.send_request(request).await?,
    };

    let status = response.status();
    timings.ttfb = Some(start.elapsed());
    emit(
        events,
        RequestEvent::FirstByte {
            status: status.as_u16(),
            at: start.elapsed(),
        },
    );

    let version = format!("{:?}", response.version());
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect();

    // --- Body, straight to disk ------------------------------------------
    let (body_path, body_len) = drain_to_disk(response, opts, spec, events).await?;
    timings.total = Some(start.elapsed());
    emit(
        events,
        RequestEvent::Complete {
            at: start.elapsed(),
            body_len,
        },
    );

    Ok(ResponseHandle {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        version,
        headers,
        body_path,
        body_len,
        timings,
        remote_addr: Some(remote_addr),
        tls: tls_info,
    })
}

enum Sender {
    H1(http1::SendRequest<Full<Bytes>>),
    H2(http2::SendRequest<Full<Bytes>>),
}

async fn handshake<S>(io: TokioIo<S>, h2: bool) -> Result<Sender>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    if h2 {
        let (tx, conn) = http2::handshake(TokioExecutor::new(), io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(Sender::H2(tx))
    } else {
        let (tx, conn) = http1::handshake(io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(Sender::H1(tx))
    }
}

fn build_request(spec: &RequestSpec, uri: &Uri, h2: bool) -> Result<Request<Full<Bytes>>> {
    let method = Method::from_bytes(spec.method.as_bytes())
        .map_err(|e| Error::Transport(format!("bad method {}: {e}", spec.method)))?;

    // HTTP/1.1 takes origin-form (`/users?page=1`); HTTP/2 needs the full uri
    // so hyper can derive `:scheme` and `:authority`.
    let target = if h2 {
        uri.clone()
    } else {
        uri.path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
            .parse()
            .map_err(|e: http::uri::InvalidUri| Error::InvalidUrl {
                url: uri.to_string(),
                message: e.to_string(),
            })?
    };

    let mut builder = Request::builder().method(method).uri(target);

    // `append`, not `insert`: duplicate headers are legitimate and a client
    // that collapses them cannot test a server that cares.
    if let Some(map) = builder.headers_mut() {
        for (name, value) in &spec.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| Error::Transport(format!("bad header name {name}: {e}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|e| Error::Transport(format!("bad header value: {e}")))?;
            map.append(name, value);
        }

        // hyper at the connection level adds nothing for us, which is what we
        // want: the file decides what goes on the wire. HTTP/1.1 does require
        // a Host, so supply one only when the request did not -- overriding it
        // by hand is how you test virtual hosts and routing.
        if !h2 && !map.contains_key(http::header::HOST) {
            if let Some(authority) = uri.authority() {
                map.insert(
                    http::header::HOST,
                    HeaderValue::from_str(authority.as_str())
                        .map_err(|e| Error::Transport(format!("bad host header: {e}")))?,
                );
            }
        }
    }

    let body = spec
        .body
        .as_ref()
        .map(|b| Full::new(Bytes::from(b.clone())))
        .unwrap_or_else(|| Full::new(Bytes::new()));

    Ok(builder.body(body)?)
}

async fn drain_to_disk(
    response: http::Response<Incoming>,
    opts: &SendOptions,
    spec: &RequestSpec,
    events: Option<&EventSink>,
) -> Result<(PathBuf, u64)> {
    tokio::fs::create_dir_all(&opts.store_dir).await?;
    let path = opts.store_dir.join(response_filename(spec));
    let mut file = tokio::io::BufWriter::new(tokio::fs::File::create(&path).await?);

    let mut body = response.into_body();
    let mut total: u64 = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(chunk) = frame.data_ref() {
            file.write_all(chunk).await?;
            total += chunk.len() as u64;
            emit(
                events,
                RequestEvent::BodyChunk {
                    bytes: chunk.len(),
                    total,
                },
            );
        }
    }
    file.flush().await?;

    Ok((path, total))
}

fn response_filename(spec: &RequestSpec) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let slug: String = spec
        .label()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    format!("{stamp}-{slug}.bin")
}

fn approx_head_len(request: &Request<Full<Bytes>>) -> usize {
    let line = request.method().as_str().len()
        + request.uri().path_and_query().map_or(1, |p| p.as_str().len())
        + "  HTTP/1.1\r\n".len();
    let headers: usize = request
        .headers()
        .iter()
        .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
        .sum();
    line + headers + 2
}

fn describe_tls_version(v: rustls::ProtocolVersion) -> &'static str {
    match v {
        rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3",
        rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2",
        _ => "unknown",
    }
}

fn emit(events: Option<&EventSink>, event: RequestEvent) {
    if let Some(sink) = events {
        // A dropped receiver means nobody is rendering; that is not an error.
        let _ = sink.send(event);
    }
}

/// One cached config per ALPN offer. Pinning a version has to change what we
/// advertise, not just how we read the reply -- otherwise the server answers
/// in h2 and we try to parse it as HTTP/1.1.
fn tls_connector(force: Option<HttpVersion>) -> tokio_rustls::TlsConnector {
    static NEGOTIATED: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    static ONLY_H1: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    static ONLY_H2: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

    let (cell, alpn): (&OnceLock<Arc<rustls::ClientConfig>>, Vec<Vec<u8>>) = match force {
        None => (&NEGOTIATED, vec![b"h2".to_vec(), b"http/1.1".to_vec()]),
        Some(HttpVersion::Http11) => (&ONLY_H1, vec![b"http/1.1".to_vec()]),
        Some(HttpVersion::Http2) => (&ONLY_H2, vec![b"h2".to_vec()]),
    };

    let config = cell.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = alpn;
        Arc::new(config)
    });
    tokio_rustls::TlsConnector::from(config.clone())
}
