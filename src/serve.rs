//! Serving this repo to paired peers over the gitsync transport (DESIGN.md
//! §11). `gitsync watch` listens on a TCP port; for each connection:
//!
//! 1. TLS 1.3; the peer's key must be in the trust list, or the connection
//!    is dropped before anything else runs.
//! 2. One request line, `gitsync/1 upload-pack <repo id>`, answered with
//!    `ok` or `error <reason>` (so the fetching side can say what is wrong).
//! 3. `git upload-pack` for this repo, piped to the connection.
//!
//! Only upload-pack is ever started (never receive-pack, never a shell), so a
//! paired peer can read this repo and nothing else.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rustls::{Connection, ServerConnection};
use tracing::{debug, info, warn};

use crate::discovery::repo_id;
use crate::git::Git;
use crate::identity::Identity;
use crate::tls::{peer_key, pump, server_config, Accept, Pinned};
use crate::trust::TrustList;

/// The only request there is.
pub const PROTOCOL: &str = "gitsync/1";
const MAX_CONNECTIONS: usize = 8;
const MAX_LINE: usize = 256;
/// For the handshake and the request line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Once git runs: a fetch that stalls this long is given up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// The request line for fetching `repo`.
pub fn request(repo: &str) -> String {
    format!("{PROTOCOL} upload-pack {repo}\n")
}

/// The repo id from a request line, or why it is not a valid request.
pub fn parse_request(line: &str) -> Result<&str, String> {
    let parts: Vec<&str> = line.trim_end_matches(['\r', '\n']).split(' ').collect();
    match parts.as_slice() {
        [PROTOCOL, "upload-pack", repo]
            if (1..=64).contains(&repo.len()) && repo.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            Ok(repo)
        }
        [PROTOCOL, ..] => Err("unsupported request".into()),
        _ => Err("unsupported protocol version; update gitsync on both machines".into()),
    }
}

/// Reads one `\n`-terminated line of at most [`MAX_LINE`] bytes, byte by
/// byte so nothing after it is consumed.
pub fn read_line(stream: &mut impl Read) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < MAX_LINE {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        line.push(byte[0]);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "no complete request line",
    ))
}

/// Starts listening on `port` (0: any free port) and serving in the
/// background. Returns the port.
pub fn start(git: &Git, port: u16) -> Result<u16> {
    let identity = Arc::new(Identity::load_or_create()?);
    let (listeners, port) = listen(port)?;
    let server = Arc::new(Server {
        git: git.clone(),
        identity,
        active: AtomicUsize::new(0),
        warned: Mutex::new(HashSet::new()),
    });
    for listener in listeners {
        let server = server.clone();
        thread::spawn(move || server.accept(listener));
    }
    Ok(port)
}

/// Listens on `port` (0: any free port) over IPv6 and IPv4, and returns the
/// listeners and the port. One `[::]` socket takes both on Linux and macOS,
/// but only IPv6 on Windows, so IPv4 gets its own socket on the same port
/// wherever the first did not take it already.
pub fn listen(port: u16) -> Result<(Vec<TcpListener>, u16)> {
    let v4 = |port| TcpListener::bind(SocketAddr::from(([0u8; 4], port)));
    match TcpListener::bind(SocketAddr::from(([0u16; 8], port))) {
        Ok(v6) => {
            let port = v6.local_addr()?.port();
            let mut listeners = vec![v6];
            // Fails (address in use) where [::] already covers IPv4.
            if let Ok(v4) = v4(port) {
                listeners.push(v4);
            }
            Ok((listeners, port))
        }
        // No IPv6 on this machine.
        Err(_) => {
            let v4 = v4(port).with_context(|| format!("listening on TCP port {port}"))?;
            let port = v4.local_addr()?.port();
            Ok((vec![v4], port))
        }
    }
}

impl Server {
    fn accept(self: Arc<Self>, listener: TcpListener) {
        let server = self;
        for sock in listener.incoming() {
            let Ok(sock) = sock else { continue };
            if server.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                server.active.fetch_sub(1, Ordering::SeqCst);
                debug!("too many connections; dropping one");
                continue;
            }
            let server = server.clone();
            thread::spawn(move || {
                let peer = sock.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(err) = server.handle(sock) {
                    debug!("connection from {peer}: {err:#}");
                }
                server.active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}

struct Server {
    git: Git,
    identity: Arc<Identity>,
    active: AtomicUsize,
    /// Unpaired keys already logged, to log each once.
    warned: Mutex<HashSet<String>>,
}

impl Server {
    fn handle(&self, mut sock: TcpStream) -> Result<()> {
        sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        sock.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let verifier = Pinned::new(Accept::TrustList);
        let mut conn = ServerConnection::new(server_config(&self.identity, verifier.clone())?)?;
        let mut stream = rustls::Stream::new(&mut conn, &mut sock);

        let line = match read_line(&mut stream) {
            Ok(line) => line,
            Err(err) => {
                if let Some(key) = verifier.seen() {
                    self.warn_unpaired(&key);
                }
                return Err(err.into());
            }
        };
        let answer = match parse_request(&line) {
            Ok(repo) if Some(repo) == repo_id(&self.git)?.as_deref() => Ok(()),
            Ok(_) => Err("this machine does not serve that repo here".to_string()),
            Err(reason) => Err(reason),
        };
        match &answer {
            Ok(()) => stream.write_all(b"ok\n")?,
            Err(reason) => stream.write_all(format!("error {reason}\n").as_bytes())?,
        }
        stream.flush()?;
        if let Err(reason) = answer {
            conn.send_close_notify();
            let _ = conn.complete_io(&mut sock);
            bail!("refused: {reason}");
        }

        let conn = Connection::Server(conn);
        let machine = peer_key(&conn)
            .and_then(|key| {
                TrustList::load()
                    .ok()?
                    .machine_for(&key)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        debug!("serving {machine}");
        sock.set_read_timeout(Some(IDLE_TIMEOUT))?;
        sock.set_write_timeout(Some(IDLE_TIMEOUT))?;

        let git_dir = self.git.git_dir()?;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(self.git.workdir())
            .args([
                // What we fetched from others and our sync bookkeeping are
                // not offered.
                "-c",
                "uploadpack.hideRefs=refs/gitsync-peers",
                "-c",
                "uploadpack.hideRefs=refs/gitsync-state",
                "upload-pack",
                "--strict",
            ])
            .arg(&git_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting git upload-pack")?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let pumped = pump(conn, sock, stdout, stdin, true);
        // The peer is gone or upload-pack is done; either way it ends now.
        let status = child.wait()?;
        pumped?;
        if !status.success() {
            warn!("git upload-pack for {machine} failed ({status})");
        }
        Ok(())
    }

    fn warn_unpaired(&self, key: &str) {
        let paired = TrustList::load()
            .map(|list| list.machine_for(key).is_some())
            .unwrap_or(true);
        if !paired
            && self
                .warned
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(key.into())
        {
            info!(
                "a machine that is not paired tried to fetch (key {key}); to sync with it, \
                 run `gitsync pair` here and `gitsync pair <code>` there"
            );
        }
    }
}
