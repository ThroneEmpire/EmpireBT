//! Serving connections, over plain HTTP or HTTPS, within limits: at most
//! [`MAX_CONNECTIONS`] at once, TLS handshakes and request headers that
//! must arrive within seconds (so idle clients cannot hold connections),
//! and a graceful stop.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

/// Connections served at once; more are closed as they arrive.
const MAX_CONNECTIONS: usize = 256;
/// Time a client has to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Time a client has to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long stopping waits for open requests.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The address a request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClientAddr(pub IpAddr);

/// A certificate for HTTPS, ready to serve.
pub(crate) struct Tls {
    pub acceptor: TlsAcceptor,
    /// SHA-256 of the certificate, as browsers show it.
    pub fingerprint: String,
}

/// Loads a PEM certificate chain and private key.
pub(crate) fn load_tls(certificate: &Path, key: &Path) -> Result<Tls, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(certificate)
        .and_then(|certificates| certificates.collect())
        .map_err(|error| format!("cannot read certificate {}: {error}", certificate.display()))?;
    let first = certificates
        .first()
        .ok_or_else(|| format!("{} holds no certificate", certificate.display()))?;
    let fingerprint = fingerprint(first.as_ref());
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|error| format!("cannot read private key {}: {error}", key.display()))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| error.to_string())?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(|error| format!("certificate and key do not work together: {error}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Tls {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        fingerprint,
    })
}

/// Makes a self-signed certificate for `names` (host names and addresses)
/// unless one is already at `certificate`. The key is readable by its
/// owner only.
pub(crate) fn ensure_self_signed(
    certificate: &Path,
    key: &Path,
    names: Vec<String>,
) -> Result<(), String> {
    if certificate.exists() && key.exists() {
        return Ok(());
    }
    let generated = rcgen::generate_simple_self_signed(names)
        .map_err(|error| format!("cannot make a certificate: {error}"))?;
    write_private(key, generated.signing_key.serialize_pem().as_bytes())?;
    std::fs::write(certificate, generated.cert.pem())
        .map_err(|error| format!("cannot save {}: {error}", certificate.display()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes))
        .map_err(|error| format!("cannot save {}: {error}", path.display()))
}

/// `AA:BB:…` form of a certificate's SHA-256 digest.
pub(crate) fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Counts a connection until dropped.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Accepts and serves connections until `stop` turns true.
pub(crate) async fn run(
    listener: tokio::net::TcpListener,
    tls: Option<TlsAcceptor>,
    router: axum::Router,
    mut stop: watch::Receiver<bool>,
) {
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let (stream, address) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                // Out of file descriptors and the like: wait it out.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = stop.changed() => break,
        };
        if active.load(Ordering::Acquire) >= MAX_CONNECTIONS {
            continue; // dropping it closes it
        }
        active.fetch_add(1, Ordering::AcqRel);
        let slot = Slot(Arc::clone(&active));
        let (router, tls, stop) = (router.clone(), tls.clone(), stop.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let _ = stream.set_nodelay(true);
            let client = ClientAddr(canonical(address).ip());
            match tls {
                Some(acceptor) => {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        serve_connection(stream, client, router, stop).await;
                    }
                }
                None => serve_connection(stream, client, router, stop).await,
            }
        });
    }
    // Let open requests finish, briefly.
    let deadline = tokio::time::Instant::now() + STOP_GRACE;
    while active.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn serve_connection<I>(
    io: I,
    client: ClientAddr,
    router: axum::Router,
    mut stop: watch::Receiver<bool>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |mut request: hyper::Request<_>| {
        request.extensions_mut().insert(client);
        let mut router = router.clone();
        async move { tower_service::Service::call(&mut router, request).await }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .serve_connection(TokioIo::new(io), service);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        _ = stop.changed() => {
            connection.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(STOP_GRACE, connection).await;
        }
    }
}

/// IPv4-mapped IPv6 addresses as the IPv4 addresses they are.
fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::from((ip, v6.port())),
            None => address,
        },
        SocketAddr::V4(_) => address,
    }
}
