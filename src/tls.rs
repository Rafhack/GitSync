//! TLS for the gitsync transport (DESIGN.md §11): TLS 1.3 only, both sides
//! show a certificate, and trust comes from pinned keys (the trust list), not
//! from a certificate authority or host names.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, Connection, DigitallySignedStruct, DistinguishedName, Error,
    ServerConfig, SignatureScheme,
};

use crate::identity::{fingerprint_of_cert, Identity};
use crate::trust::TrustList;

/// The name the client asks for. Certificates are not checked against it.
pub const SERVER_NAME: &str = "gitsync";

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Which keys a verifier accepts.
#[derive(Debug, Clone)]
pub enum Accept {
    /// Exactly this key (a client checking the peer it meant to reach).
    Key(String),
    /// Any key in the trust list, read at handshake time (the server).
    TrustList,
    /// Anything: only while pairing, which checks the keys itself.
    Any,
}

/// Accepts a peer's certificate if its key is one we accept, and remembers
/// the key it saw, so errors can say which key it was.
#[derive(Debug)]
pub struct Pinned {
    accept: Accept,
    provider: Arc<CryptoProvider>,
    seen: Mutex<Option<String>>,
}

impl Pinned {
    pub fn new(accept: Accept) -> Arc<Self> {
        Arc::new(Self {
            accept,
            provider: provider(),
            seen: Mutex::new(None),
        })
    }

    /// The fingerprint of the key the peer showed, if it showed one.
    pub fn seen(&self) -> Option<String> {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), Error> {
        let key = fingerprint_of_cert(cert)
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(key.clone());
        let ok = match &self.accept {
            Accept::Key(expected) => *expected == key,
            Accept::TrustList => TrustList::load()
                .map(|list| list.machine_for(&key).is_some())
                .unwrap_or(false),
            Accept::Any => true,
        };
        if ok {
            Ok(())
        } else {
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

pub fn client_config(identity: &Identity, verifier: Arc<Pinned>) -> Result<Arc<ClientConfig>> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.private_key())
        .context("setting up TLS")?;
    Ok(Arc::new(config))
}

pub fn server_config(identity: &Identity, verifier: Arc<Pinned>) -> Result<Arc<ServerConfig>> {
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.private_key())
        .context("setting up TLS")?;
    Ok(Arc::new(config))
}

/// The fingerprint of the key the other side of a finished handshake showed.
pub fn peer_key(conn: &Connection) -> Option<String> {
    conn.peer_certificates()
        .and_then(|certs| certs.first())
        .and_then(|cert| fingerprint_of_cert(cert).ok())
}

/// Copies bytes both ways between a TLS connection and a local program
/// (`input` is what to send, `output` gets what arrives) until the peer
/// closes the connection.
///
/// Two threads: this one blocks on the socket and the other on `input`, so
/// neither direction waits for the other. The connection state is locked only
/// briefly, never during network I/O. With `close_when_input_ends`, the end
/// of `input` closes the connection (the serving side, when upload-pack is
/// done); otherwise only the peer closes it.
pub fn pump(
    conn: Connection,
    sock: TcpStream,
    input: impl Read + Send + 'static,
    mut output: impl Write,
    close_when_input_ends: bool,
) -> Result<()> {
    let conn = Arc::new(Mutex::new(conn));
    // Encrypted records must reach the socket in the order they were made.
    let send = Arc::new(Mutex::new(sock.try_clone()?));

    {
        let (conn, send) = (conn.clone(), send.clone());
        thread::spawn(move || {
            let _ = send_all(&conn, &send, input, close_when_input_ends);
        });
    }

    let mut sock = sock;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Plaintext first: some may have arrived with the handshake.
        let (plain, closed, wants_write) = {
            let mut conn = lock(&conn);
            let mut plain = Vec::new();
            let closed = match conn.reader().read_to_end(&mut plain) {
                Ok(_) => true, // clean close (close_notify)
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => false,
                Err(err) => return Err(err.into()),
            };
            (plain, closed, conn.wants_write())
        };
        if !plain.is_empty() {
            output.write_all(&plain)?;
            output.flush()?;
        }
        if closed {
            return Ok(());
        }
        if wants_write {
            flush_tls(&conn, &send)?;
        }
        let n = match sock.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        let mut conn = lock(&conn);
        let mut data = &buf[..n];
        while !data.is_empty() {
            conn.read_tls(&mut data)?;
            conn.process_new_packets()?;
        }
    }
}

fn send_all(
    conn: &Mutex<Connection>,
    send: &Mutex<TcpStream>,
    mut input: impl Read,
    close_when_input_ends: bool,
) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match input.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        if n == 0 {
            if close_when_input_ends {
                lock(conn).send_close_notify();
                flush_tls(conn, send)?;
                let _ = lock(send).shutdown(Shutdown::Both);
            }
            return Ok(());
        }
        lock(conn).writer().write_all(&buf[..n])?;
        flush_tls(conn, send)?;
    }
}

/// Sends whatever encrypted data the connection has queued.
fn flush_tls(conn: &Mutex<Connection>, send: &Mutex<TcpStream>) -> Result<()> {
    let mut sock = lock(send);
    let mut records = Vec::new();
    {
        let mut conn = lock(conn);
        while conn.wants_write() {
            conn.write_tls(&mut records)?;
        }
    }
    sock.write_all(&records)?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
