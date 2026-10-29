//! TLS configuration.
//!
//! The reason to own the TLS layer rather than inherit it: testing an API
//! means talking to staging boxes with self-signed certs, internal CAs,
//! mutual-auth gateways, and hosts that are only reachable under a different
//! SNI than their address. A client that cannot do those things sends people
//! back to curl.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};

use crate::error::{Error, Result};

/// Per-request TLS behaviour. The default is "verify properly against the
/// platform's web roots", and every departure from that is explicit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsOptions {
    /// Accept any certificate. Loud by design: callers should say so.
    pub insecure: bool,
    /// Extra CA certificates, PEM. Added to the web roots, not replacing them.
    pub ca_file: Option<PathBuf>,
    /// Client certificate chain, PEM, for mutual TLS.
    pub client_cert: Option<PathBuf>,
    /// Private key for `client_cert`, PEM.
    pub client_key: Option<PathBuf>,
    /// Send this name in SNI and verify against it, instead of the url's host.
    pub sni: Option<String>,
}

impl TlsOptions {
    /// Whether the cached default configs can be used instead of building one.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        match (&self.client_cert, &self.client_key) {
            (Some(_), None) => Err(Error::Config(
                "--cert needs --key: a client certificate is useless without its private key"
                    .into(),
            )),
            (None, Some(_)) => Err(Error::Config(
                "--key needs --cert: a private key alone identifies nothing".into(),
            )),
            _ => Ok(()),
        }
    }

    /// The name to put in SNI and verify against.
    pub fn server_name<'a>(&'a self, host: &'a str) -> &'a str {
        self.sni.as_deref().unwrap_or(host)
    }
}

/// Build a config for the given ALPN offer. Callers that pass default options
/// should prefer the cached configs in `engine`; this allocates and parses.
pub fn build_config(alpn: Vec<Vec<u8>>, opts: &TlsOptions) -> Result<Arc<ClientConfig>> {
    opts.validate()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca) = &opts.ca_file {
        let added = load_certs(ca)?;
        if added.is_empty() {
            return Err(Error::Config(format!(
                "{} contained no certificates",
                ca.display()
            )));
        }
        for cert in added {
            roots
                .add(cert)
                .map_err(|e| Error::Tls(format!("{}: {e}", ca.display())))?;
        }
    }

    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?;

    let builder = if opts.insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
    } else {
        builder.with_root_certificates(roots)
    };

    let mut config = match (&opts.client_cert, &opts.client_key) {
        (Some(cert), Some(key)) => {
            let chain = load_certs(cert)?;
            if chain.is_empty() {
                return Err(Error::Config(format!(
                    "{} contained no certificates",
                    cert.display()
                )));
            }
            builder
                .with_client_auth_cert(chain, load_key(key)?)
                .map_err(|e| Error::Tls(format!("client certificate rejected: {e}")))?
        }
        _ => builder.with_no_client_auth(),
    };

    config.alpn_protocols = alpn;
    Ok(Arc::new(config))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    // A path that does not resolve is a configuration mistake, not a transport
    // failure: nothing was sent, and the exit code should say so.
    let pem = read_pem(path)?;
    rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Tls(format!("{}: {e}", path.display())))
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let pem = read_pem(path)?;
    rustls_pemfile::private_key(&mut pem.as_slice())
        .map_err(|e| Error::Tls(format!("{}: {e}", path.display())))?
        .ok_or_else(|| Error::Config(format!("{} contained no private key", path.display())))
}

fn read_pem(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

/// Verifies nothing, on purpose, for `--insecure`.
///
/// Signature checking is still delegated to the provider so that a broken
/// handshake still looks broken; it is certificate *trust* we are waiving,
/// which is the narrow thing people actually want against a staging box.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpn() -> Vec<Vec<u8>> {
        vec![b"http/1.1".to_vec()]
    }

    #[test]
    fn default_options_build() {
        let opts = TlsOptions::default();
        assert!(opts.is_default());
        assert!(build_config(alpn(), &opts).is_ok());
    }

    #[test]
    fn insecure_builds_and_is_not_default() {
        let opts = TlsOptions {
            insecure: true,
            ..Default::default()
        };
        assert!(!opts.is_default());
        assert!(build_config(alpn(), &opts).is_ok());
    }

    #[test]
    fn cert_without_key_is_rejected_before_any_io() {
        let opts = TlsOptions {
            client_cert: Some(PathBuf::from("nonexistent.pem")),
            ..Default::default()
        };
        assert!(matches!(opts.validate(), Err(Error::Config(_))));
    }

    #[test]
    fn missing_ca_file_is_a_config_error_not_a_transport_one() {
        let opts = TlsOptions {
            ca_file: Some(PathBuf::from("definitely-not-here.pem")),
            ..Default::default()
        };
        assert!(matches!(build_config(alpn(), &opts), Err(Error::Config(_))));
    }

    #[test]
    fn sni_overrides_the_host() {
        let opts = TlsOptions {
            sni: Some("internal.example".into()),
            ..Default::default()
        };
        assert_eq!(opts.server_name("10.0.0.1"), "internal.example");
        assert_eq!(TlsOptions::default().server_name("a.test"), "a.test");
    }
}
