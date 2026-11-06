//! `gitsync connect`: the fetching side of the gitsync transport (DESIGN.md
//! §11). `git fetch` runs it through git's `ext::` transport and talks the git
//! protocol over its stdin and stdout; it carries that over TLS to the peer's
//! `gitsync watch`, after checking the peer shows the key paired under its
//! name.

use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rustls::pki_types::ServerName;
use rustls::{AlertDescription, ClientConnection, Connection};

use crate::identity::Identity;
use crate::serve::{read_line, request};
use crate::tls::{client_config, pump, Accept, Pinned, SERVER_NAME};
use crate::trust::TrustList;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connects to `machine` at `addr` (`host:port`) for `repo` and pipes
/// stdin/stdout through, until the peer closes the connection.
pub fn connect(machine: &str, addr: &str, repo: &str, service: &str) -> Result<()> {
    if service != "git-upload-pack" {
        bail!("only fetching is possible over the gitsync transport");
    }
    let Some(key) = TrustList::load()?.key_for(machine).map(str::to_string) else {
        bail!(
            "this machine is not paired with {machine}: run `gitsync pair` on {machine} \
             and `gitsync pair <code>` here"
        );
    };
    let identity = Identity::load_or_create()?;

    let target = addr
        .to_socket_addrs()
        .with_context(|| format!("bad address {addr:?}"))?
        .next()
        .with_context(|| format!("no address for {addr:?}"))?;
    let mut sock = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT)
        .with_context(|| format!("connecting to {machine} at {addr}"))?;
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;

    let verifier = Pinned::new(Accept::Key(key.clone()));
    let server = ServerName::try_from(SERVER_NAME).expect("valid name");
    let mut conn = ClientConnection::new(client_config(&identity, verifier.clone())?, server)?;
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);
    let answer = stream
        .write_all(request(repo).as_bytes())
        .and_then(|()| stream.flush())
        .and_then(|()| read_line(&mut stream));
    let answer = match answer {
        Ok(answer) => answer,
        Err(err) => bail!(explain(&err, machine, addr, &key, verifier.seen())),
    };
    if let Some(reason) = answer.strip_prefix("error ") {
        bail!("{machine}: {reason}");
    }
    if answer != "ok" {
        bail!("{machine} at {addr} answered something unexpected");
    }

    sock.set_read_timeout(None)?;
    sock.set_write_timeout(None)?;
    pump(
        Connection::Client(conn),
        sock,
        io::stdin(),
        io::stdout().lock(),
        false,
    )
}

/// What went wrong while connecting, in words.
fn explain(
    err: &io::Error,
    machine: &str,
    addr: &str,
    paired_key: &str,
    seen: Option<String>,
) -> String {
    if let Some(seen) = seen.filter(|seen| seen != paired_key) {
        return format!(
            "{machine} at {addr} showed key {seen}, not the key paired as {machine} \
             ({paired_key}); if {machine} was reinstalled, pair the two machines again"
        );
    }
    let tls = err
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>());
    if let Some(rustls::Error::AlertReceived(
        AlertDescription::CertificateUnknown
        | AlertDescription::AccessDenied
        | AlertDescription::BadCertificate
        | AlertDescription::CertificateRequired
        | AlertDescription::UnknownCA
        | AlertDescription::DecryptError,
    )) = tls
    {
        return format!(
            "{machine} does not trust this machine: pair them again (`gitsync pair` on \
             {machine}, `gitsync pair <code>` here)"
        );
    }
    format!("connecting to {machine} at {addr}: {err}")
}
