//! `gitsync pair`: two machines start trusting each other, once, by a short
//! code (DESIGN.md §11).
//!
//! One machine waits with a code; the other connects with the same code. Over
//! a TLS connection that checks no certificates yet, both run SPAKE2 with the
//! code as the password: it only gives both sides the same key if they used
//! the same code, and an eavesdropper learns nothing about the code. Each side
//! then proves it has that key with an HMAC over both certificate
//! fingerprints *as it saw them*. A machine in the middle would have swapped
//! certificates, so the fingerprints differ and pairing fails. The waiting
//! side takes one attempt only, so an attacker gets one guess in a million.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::discovery::{best_address, mdns_hostname};
use crate::identity::{fingerprint_of_cert, Identity};
use crate::serve::listen;
use crate::snapshot::validate_machine_id;
use crate::tls::{client_config, provider, server_config, Accept, Pinned, SERVER_NAME};
use crate::trust::TrustList;

pub const SERVICE_TYPE: &str = "_gitsync-pair._tcp.local.";
const WAIT: Duration = Duration::from_secs(120);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const LABEL: &[u8] = b"gitsync-pair-v1";

/// A machine paired with this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub machine: String,
    pub key: String,
    /// Entries of the trust list this one replaced.
    pub replaced: Vec<String>,
}

/// A new random code, `482-913`.
pub fn new_code() -> Result<String> {
    let random = provider().secure_random;
    loop {
        let mut bytes = [0u8; 4];
        random
            .fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("no randomness from the OS"))?;
        let n = u32::from_be_bytes(bytes);
        // Rejection sampling: every code equally likely.
        if n < u32::MAX - u32::MAX % 1_000_000 {
            let n = n % 1_000_000;
            return Ok(format!("{:03}-{:03}", n / 1000, n % 1000));
        }
    }
}

/// The six digits of a code as typed (`482-913`, `482913`, `482 913`).
pub fn parse_code(code: &str) -> Result<String> {
    let digits: String = code.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{code:?} is not a pairing code (six digits, like 482-913)");
    }
    Ok(digits)
}

/// The waiting side: prints a code, waits for the other machine (at most two
/// minutes, one attempt), and trusts it if the codes matched.
pub fn wait(machine: &str, port: u16, announce: bool) -> Result<Paired> {
    let identity = Identity::load_or_create()?;
    let code = new_code()?;
    let (listeners, port) = listen(port)?;
    // Keep the announcement up while waiting; dropping the daemon ends it.
    let _mdns = if announce {
        match announce_pairing(machine, &identity.fingerprint, port) {
            Ok(mdns) => Some(mdns),
            Err(err) => {
                eprintln!(
                    "note: cannot announce over mDNS ({err:#}); use --host on the other machine"
                );
                None
            }
        }
    } else {
        None
    };

    println!("this machine: {machine}");
    println!("  key  {}", identity.fingerprint);
    println!();
    println!("pairing code: {code}");
    println!("on the other machine, in the repo, run:");
    println!("  gitsync pair {code}");
    match local_ip() {
        Some(ip) => println!(
            "(where multicast is blocked: gitsync pair {code} --host {})",
            SocketAddr::new(ip, port)
        ),
        None => {
            println!("(where multicast is blocked: add --host <this machine's address>:{port})")
        }
    }
    println!("waiting up to 2 minutes...");
    let _ = std::io::stdout().flush();

    for listener in &listeners {
        listener.set_nonblocking(true)?;
    }
    let deadline = Instant::now() + WAIT;
    let sock = 'wait: loop {
        for listener in &listeners {
            match listener.accept() {
                Ok((sock, _)) => break 'wait sock,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err.into()),
            }
        }
        if Instant::now() > deadline {
            bail!("no machine paired within 2 minutes; run `gitsync pair` again");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(listeners); // one attempt only
    let peer = serve_exchange(sock, &identity, machine, &parse_code(&code)?)
        .context("pairing failed (a mistyped code ends the attempt: run `gitsync pair` again)")?;
    trust(peer)
}

/// The joining side: finds the waiting machine (over mDNS, or at `host`)
/// and pairs with it using `code`.
pub fn join(machine: &str, code: &str, host: Option<&str>) -> Result<Paired> {
    let digits = parse_code(code)?;
    let identity = Identity::load_or_create()?;
    let target = match host {
        Some(host) => host
            .to_socket_addrs()
            .with_context(|| format!("bad address {host:?} (it needs the port: host:port)"))?
            .next()
            .with_context(|| format!("no address for {host:?}"))?,
        None => find_waiting(machine)?,
    };
    let sock = TcpStream::connect_timeout(&target, Duration::from_secs(10))
        .with_context(|| format!("connecting to {target}"))?;
    let peer = join_exchange(sock, &identity, machine, &digits).context("pairing failed")?;
    trust(peer)
}

fn trust(peer: Peer) -> Result<Paired> {
    let mut list = TrustList::load()?;
    let replaced = list.add(&peer.machine, &peer.key)?;
    list.save()?;
    Ok(Paired {
        machine: peer.machine,
        key: peer.key,
        replaced: replaced
            .into_iter()
            .map(|t| format!("{} ({})", t.machine, t.key))
            .collect(),
    })
}

/// The other machine, as learned during pairing.
struct Peer {
    machine: String,
    key: String,
}

fn serve_exchange(
    mut sock: TcpStream,
    identity: &Identity,
    ours: &str,
    code: &str,
) -> Result<Peer> {
    sock.set_nonblocking(false)?;
    sock.set_read_timeout(Some(EXCHANGE_TIMEOUT))?;
    sock.set_write_timeout(Some(EXCHANGE_TIMEOUT))?;
    let mut conn = ServerConnection::new(server_config(identity, Pinned::new(Accept::Any))?)?;
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);

    let (spake, our_msg) = start(code);
    let their_msg = read_frame(&mut stream)?;
    let their_name = name(read_frame(&mut stream)?, ours)?;
    write_frame(&mut stream, &our_msg)?;
    write_frame(&mut stream, ours.as_bytes())?;
    let key = spake
        .finish(&their_msg)
        .map_err(|_| anyhow::anyhow!("the key exchange failed"))?;
    let their_key = seen_key(stream.conn.peer_certificates())?;
    let transcript = transcript(&identity.fingerprint, &their_key, ours, &their_name);

    write_frame(&mut stream, &tag(&key, b"server", &transcript))?;
    let their_tag = read_frame(&mut stream)?;
    if their_tag.is_empty() {
        bail!("the codes did not match");
    }
    verify(&key, b"client", &transcript, &their_tag)?;
    write_frame(&mut stream, b"ok")?;
    stream.conn.send_close_notify();
    let _ = stream.flush();
    Ok(Peer {
        machine: their_name,
        key: their_key,
    })
}

fn join_exchange(mut sock: TcpStream, identity: &Identity, ours: &str, code: &str) -> Result<Peer> {
    sock.set_read_timeout(Some(EXCHANGE_TIMEOUT))?;
    sock.set_write_timeout(Some(EXCHANGE_TIMEOUT))?;
    let server = ServerName::try_from(SERVER_NAME).expect("valid name");
    let mut conn =
        ClientConnection::new(client_config(identity, Pinned::new(Accept::Any))?, server)?;
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);

    let (spake, our_msg) = start(code);
    write_frame(&mut stream, &our_msg)?;
    write_frame(&mut stream, ours.as_bytes())?;
    let their_msg = read_frame(&mut stream)?;
    let their_name = name(read_frame(&mut stream)?, ours)?;
    let key = spake
        .finish(&their_msg)
        .map_err(|_| anyhow::anyhow!("the key exchange failed"))?;
    let their_key = seen_key(stream.conn.peer_certificates())?;
    let transcript = transcript(&their_key, &identity.fingerprint, &their_name, ours);

    let their_tag = read_frame(&mut stream)?;
    if let Err(err) = verify(&key, b"server", &transcript, &their_tag) {
        // Tell the waiting side, so it can say so too.
        let _ = write_frame(&mut stream, b"");
        return Err(err);
    }
    write_frame(&mut stream, &tag(&key, b"client", &transcript))?;
    if read_frame(&mut stream)? != b"ok" {
        bail!("the other machine did not confirm");
    }
    Ok(Peer {
        machine: their_name,
        key: their_key,
    })
}

fn start(code: &str) -> (Spake2<Ed25519Group>, Vec<u8>) {
    Spake2::<Ed25519Group>::start_symmetric(&Password::new(code.as_bytes()), &SpakeId::new(LABEL))
}

/// Both fingerprints and both names, as one side saw them (server first).
fn transcript(server_key: &str, client_key: &str, server: &str, client: &str) -> Vec<u8> {
    let mut out = LABEL.to_vec();
    for part in [server_key, client_key, server, client] {
        out.extend_from_slice(&(part.len() as u16).to_be_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

fn mac(key: &[u8], role: &[u8], transcript: &[u8]) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    mac.update(role);
    mac.update(transcript);
    mac
}

fn tag(key: &[u8], role: &[u8], transcript: &[u8]) -> Vec<u8> {
    mac(key, role, transcript).finalize().into_bytes().to_vec()
}

fn verify(key: &[u8], role: &[u8], transcript: &[u8], tag: &[u8]) -> Result<()> {
    mac(key, role, transcript)
        .verify_slice(tag)
        .map_err(|_| anyhow::anyhow!("the codes did not match"))
}

fn seen_key(certs: Option<&[rustls::pki_types::CertificateDer<'static>]>) -> Result<String> {
    let cert = certs
        .and_then(|c| c.first())
        .context("the other machine showed no certificate")?;
    fingerprint_of_cert(cert)
}

fn name(bytes: Vec<u8>, ours: &str) -> Result<String> {
    let name = String::from_utf8(bytes).context("bad machine name")?;
    validate_machine_id(&name).context("the other machine's name")?;
    if name == ours {
        bail!(
            "both machines are called {name:?}; set a different machine_id in \
             .git/gitsync.toml on one of them"
        );
    }
    Ok(name)
}

fn write_frame(stream: &mut impl Write, data: &[u8]) -> Result<()> {
    stream.write_all(&(data.len() as u16).to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream
        .read_exact(&mut len)
        .context("the other machine closed the connection")?;
    let len = u16::from_be_bytes(len) as usize;
    if len > 1024 {
        bail!("oversized message");
    }
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data)?;
    Ok(data)
}

fn announce_pairing(machine: &str, key: &str, port: u16) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let txt = HashMap::from([
        ("machine".to_string(), machine.to_string()),
        ("key".to_string(), key.to_string()),
    ]);
    let info = ServiceInfo::new(SERVICE_TYPE, machine, &mdns_hostname(), "", port, txt)
        .context("building mDNS announcement")?
        .enable_addr_auto();
    daemon.register(info).context("announcing over mDNS")?;
    Ok(daemon)
}

/// The machine waiting with `gitsync pair`, found over mDNS.
fn find_waiting(ours: &str) -> Result<SocketAddr> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let events = daemon.browse(SERVICE_TYPE).context("browsing mDNS")?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut found: HashMap<String, (String, SocketAddr)> = HashMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let machine = info
                    .txt_properties
                    .get_property_val_str("machine")
                    .unwrap_or_default()
                    .to_string();
                let addresses: Vec<IpAddr> =
                    info.addresses.iter().map(|a| a.to_ip_addr()).collect();
                if machine == ours {
                    continue;
                }
                if let Some(ip) = best_address(&addresses) {
                    found.insert(
                        info.fullname.clone(),
                        (machine, SocketAddr::new(ip, info.port)),
                    );
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    let mut found: Vec<_> = found.into_values().collect();
    match found.len() {
        0 => bail!(
            "found no machine waiting for pairing on the LAN; run `gitsync pair` on the other \
             machine first, or give its address with --host (it prints it)"
        ),
        1 => Ok(found.remove(0).1),
        _ => {
            found.sort();
            let list: Vec<String> = found.iter().map(|(m, a)| format!("{m} ({a})")).collect();
            bail!(
                "several machines are waiting for pairing: {}; pick one with --host",
                list.join(", ")
            )
        }
    }
}

/// This machine's address on the LAN, for the `--host` hint. Connecting a
/// UDP socket sends nothing; it only picks the outgoing interface.
fn local_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    Some(sock.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_look_right_and_parse() {
        for _ in 0..20 {
            let code = new_code().unwrap();
            assert_eq!(code.len(), 7);
            assert_eq!(&code[3..4], "-");
            assert_eq!(parse_code(&code).unwrap(), code.replace('-', ""));
        }
        assert_eq!(parse_code("482 913").unwrap(), "482913");
        assert!(parse_code("48291").is_err());
        assert!(parse_code("abc-def").is_err());
    }

    #[test]
    fn tags_bind_role_and_transcript() {
        let key = b"shared key";
        let t = transcript("A", "B", "desktop", "laptop");
        let server = tag(key, b"server", &t);
        assert!(verify(key, b"server", &t, &server).is_ok());
        // A reflected tag, or one over other fingerprints, does not pass.
        assert!(verify(key, b"client", &t, &server).is_err());
        let swapped = transcript("X", "B", "desktop", "laptop");
        assert!(verify(key, b"server", &swapped, &server).is_err());
        assert!(verify(b"other key", b"server", &t, &server).is_err());
    }
}
