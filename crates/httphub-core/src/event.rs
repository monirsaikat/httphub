use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;

/// Progress of a single request. The CLI prints these as a waterfall; the GUI
/// forwards the same variants to the webview and draws one. Adding a face to
/// httphub must never mean adding a second way to observe a request.
#[derive(Debug, Clone)]
pub enum RequestEvent {
    Resolving { host: String },
    Resolved { addrs: Vec<SocketAddr>, at: Duration },
    Connecting { addr: SocketAddr },
    Connected { addr: SocketAddr, at: Duration },
    TlsEstablished { protocol: String, alpn: Option<String>, at: Duration },
    RequestSent { bytes: usize, at: Duration },
    FirstByte { status: u16, at: Duration },
    BodyChunk { bytes: usize, total: u64 },
    Redirect {
        hop: usize,
        status: u16,
        from: String,
        to: String,
        at: Duration,
    },
    Complete { at: Duration, body_len: u64 },
    Failed { message: String, at: Duration },
}

pub type EventSink = mpsc::UnboundedSender<RequestEvent>;

/// Cumulative offsets from the moment the send began. `None` means that phase
/// did not happen (no TLS on plaintext, no body on a HEAD).
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    pub dns: Option<Duration>,
    pub connect: Option<Duration>,
    pub tls: Option<Duration>,
    pub sent: Option<Duration>,
    pub ttfb: Option<Duration>,
    pub total: Option<Duration>,
}

impl Timings {
    /// Phase durations rather than offsets: what a waterfall actually draws.
    pub fn phases(&self) -> Vec<(&'static str, Duration)> {
        let mut out = Vec::new();
        let mut cursor = Duration::ZERO;
        for (label, offset) in [
            ("dns", self.dns),
            ("tcp", self.connect),
            ("tls", self.tls),
            ("send", self.sent),
            ("wait", self.ttfb),
            ("download", self.total),
        ] {
            if let Some(offset) = offset {
                out.push((label, offset.saturating_sub(cursor)));
                cursor = offset;
            }
        }
        out
    }
}
