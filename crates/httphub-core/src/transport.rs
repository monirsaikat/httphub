//! An instrumented socket wrapper.
//!
//! `hyper` will tell us when a response head has been parsed, but not when our
//! request actually left the machine, and not how many bytes crossed the wire.
//! Those are exactly the numbers a client like this exists to show, so we
//! measure them ourselves by sitting between hyper and the socket.
//!
//! Two meters are used per request: one around the TCP stream, counting real
//! wire bytes (encrypted, if TLS is in play), and one around the TLS stream,
//! counting plaintext. The difference between them is the cost of encryption
//! and framing, which is otherwise invisible.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Sentinel for "this never happened". Micros since the send began otherwise.
const UNSET: u64 = u64::MAX;

#[derive(Debug)]
pub struct Meter {
    start: Instant,
    written: AtomicU64,
    read: AtomicU64,
    first_write_us: AtomicU64,
    last_write_us: AtomicU64,
    first_read_us: AtomicU64,
}

impl Meter {
    pub fn new(start: Instant) -> Arc<Self> {
        Arc::new(Self {
            start,
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            first_write_us: AtomicU64::new(UNSET),
            last_write_us: AtomicU64::new(UNSET),
            first_read_us: AtomicU64::new(UNSET),
        })
    }

    fn note_write(&self, n: usize) {
        let at = self.elapsed_us();
        self.written.fetch_add(n as u64, Ordering::Relaxed);
        let _ = self
            .first_write_us
            .compare_exchange(UNSET, at, Ordering::Relaxed, Ordering::Relaxed);
        self.last_write_us.store(at, Ordering::Relaxed);
    }

    fn note_read(&self, n: usize) {
        let at = self.elapsed_us();
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        let _ = self
            .first_read_us
            .compare_exchange(UNSET, at, Ordering::Relaxed, Ordering::Relaxed);
    }

    fn elapsed_us(&self) -> u64 {
        self.start.elapsed().as_micros().min(UNSET as u128 - 1) as u64
    }

    pub fn bytes_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn bytes_read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }

    /// When the most recent write happened. Sampled right after a response
    /// head arrives, this is when the request finished going out -- for both
    /// HTTP/1.1 and HTTP/2, where the preface and settings precede it.
    pub fn last_write(&self) -> Option<Duration> {
        at(&self.last_write_us)
    }

    pub fn first_write(&self) -> Option<Duration> {
        at(&self.first_write_us)
    }

    pub fn first_read(&self) -> Option<Duration> {
        at(&self.first_read_us)
    }
}

fn at(cell: &AtomicU64) -> Option<Duration> {
    match cell.load(Ordering::Relaxed) {
        UNSET => None,
        us => Some(Duration::from_micros(us)),
    }
}

/// Wraps a stream and reports every byte through to a [`Meter`].
pub struct Metered<S> {
    inner: S,
    meter: Arc<Meter>,
}

impl<S> Metered<S> {
    pub fn new(inner: S, meter: Arc<Meter>) -> Self {
        Self { inner, meter }
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Metered<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            let n = buf.filled().len().saturating_sub(before);
            if n > 0 {
                this.meter.note_read(n);
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Metered<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            this.meter.note_write(*n);
        }
        polled
    }

    // TLS and hyper both write vectored when they can; missing this would
    // undercount everything.
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = &polled {
            this.meter.note_write(*n);
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn counts_bytes_and_timestamps_both_directions() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            socket.read_exact(&mut buf).await.unwrap();
            socket.write_all(b"pong!!").await.unwrap();
        });

        let meter = Meter::new(Instant::now());
        let mut stream = Metered::new(
            tokio::net::TcpStream::connect(addr).await.unwrap(),
            meter.clone(),
        );

        stream.write_all(b"ping!").await.unwrap();
        let mut got = [0u8; 6];
        stream.read_exact(&mut got).await.unwrap();

        assert_eq!(&got, b"pong!!");
        assert_eq!(meter.bytes_written(), 5);
        assert_eq!(meter.bytes_read(), 6);
        assert!(meter.first_write().is_some());
        assert!(meter.last_write().is_some());
        assert!(meter.first_read().is_some());
        assert!(meter.first_read() >= meter.first_write());
    }

    #[test]
    fn unused_meter_reports_nothing() {
        let meter = Meter::new(Instant::now());
        assert_eq!(meter.bytes_written(), 0);
        assert!(meter.first_write().is_none());
        assert!(meter.first_read().is_none());
    }
}
