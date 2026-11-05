//! This machine's identity for the gitsync transport (DESIGN.md §11): an
//! Ed25519 key pair, created on first use and stored per user, not per repo.
//!
//! Only the private key is stored. The self-signed certificate TLS needs is
//! made from it at load time; peers pin the public key, not the certificate,
//! so a new certificate for the same key changes nothing.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

const KEY_FILE: &str = "identity.key";

/// The folder holding the identity and the trust list.
/// `GITSYNC_CONFIG_DIR` overrides it (tests give each "machine" its own).
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("GITSYNC_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if cfg!(windows) {
        let appdata = var("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(appdata).join("gitsync"));
    }
    let home = var("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        let home = home.context("HOME is not set")?;
        return Ok(home.join("Library/Application Support/gitsync"));
    }
    match var("XDG_CONFIG_HOME") {
        Some(dir) => Ok(PathBuf::from(dir).join("gitsync")),
        None => Ok(home.context("HOME is not set")?.join(".config/gitsync")),
    }
}

/// This machine's key and the certificate TLS shows for it.
pub struct Identity {
    key_der: Vec<u8>,
    pub cert: CertificateDer<'static>,
    /// What peers pin and users compare (`7Q4K-M2XD-...`).
    pub fingerprint: String,
}

impl Identity {
    /// Loads the identity from [`config_dir`], creating it the first time.
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_in(&config_dir()?)
    }

    pub fn load_or_create_in(dir: &Path) -> Result<Self> {
        let path = dir.join(KEY_FILE);
        let pem = match fs::read_to_string(&path) {
            Ok(pem) => pem,
            Err(err) if err.kind() == ErrorKind::NotFound => create_key(dir, &path)?,
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let key = KeyPair::from_pem(&pem)
            .with_context(|| format!("{} is not a valid key", path.display()))?;
        let cert = CertificateParams::new(vec!["gitsync".to_string()])?
            .self_signed(&key)
            .context("making this machine's certificate")?;
        let cert = cert.der().clone();
        Ok(Self {
            key_der: key.serialize_der(),
            fingerprint: fingerprint_of_cert(&cert)?,
            cert,
        })
    }

    /// The fingerprint of an existing identity, without creating one.
    pub fn existing_fingerprint() -> Result<Option<String>> {
        let dir = config_dir()?;
        if !dir.join(KEY_FILE).exists() {
            return Ok(None);
        }
        Ok(Some(Self::load_or_create_in(&dir)?.fingerprint))
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }
}

/// Creates a new key at `path`. Two processes starting at once (a watch and
/// a fetch) must end up with the same key: the file is written under a
/// private name and linked into place, which fails if the other won.
fn create_key(dir: &Path, path: &Path) -> Result<String> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let pem = KeyPair::generate_for(&PKCS_ED25519)
        .context("generating this machine's key")?
        .serialize_pem();
    let tmp = dir.join(format!("{KEY_FILE}.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(pem.as_bytes())?;
    file.sync_all()?;
    drop(file);
    let linked = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(pem),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(fs::read_to_string(path)?),
        Err(err) => Err(err).with_context(|| format!("writing {}", path.display())),
    }
}

/// The fingerprint of the public key in a certificate.
pub fn fingerprint_of_cert(cert: &CertificateDer<'_>) -> Result<String> {
    let cert = webpki::EndEntityCert::try_from(cert)
        .map_err(|err| anyhow::anyhow!("unreadable certificate: {err:?}"))?;
    Ok(fingerprint(cert.subject_public_key_info().as_ref()))
}

/// SHA-256 of a public key (its SubjectPublicKeyInfo), in base32, in groups
/// of 4 characters.
pub fn fingerprint(spki_der: &[u8]) -> String {
    fingerprint_groups(&base32(&Sha256::digest(spki_der)))
}

/// A fingerprint as typed by a person: dashes and spaces dropped, uppercase.
/// Fails unless it is a whole fingerprint.
pub fn normalize(fingerprint: &str) -> Result<String> {
    let text: String = fingerprint
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = text.len() == 52 && text.bytes().all(|b| ALPHABET.contains(&b));
    if !valid {
        bail!("{fingerprint:?} is not a gitsync fingerprint (`gitsync id` prints one)");
    }
    Ok(fingerprint_groups(&text))
}

fn fingerprint_groups(text: &str) -> String {
    let groups: Vec<&str> = text
        .as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).expect("ascii"))
        .collect();
    groups.join("-")
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32, without padding.
fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc4648() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn fingerprints_are_grouped_and_normalized() {
        let fp = fingerprint(b"some key");
        assert_eq!(fp.len(), 52 + 12);
        assert_eq!(fp.split('-').count(), 13);
        assert_eq!(normalize(&fp).unwrap(), fp);
        assert_eq!(normalize(&fp.replace('-', "").to_lowercase()).unwrap(), fp);
        assert!(normalize("7Q4K-M2XD").is_err());
        assert!(normalize(&format!("1{}", &fp[1..])).is_err());
    }

    #[test]
    fn identity_is_created_once_and_kept() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = Identity::load_or_create_in(dir.path()).unwrap();
        let again = Identity::load_or_create_in(dir.path()).unwrap();
        assert_eq!(first.fingerprint, again.fingerprint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let other = tempfile::TempDir::new().unwrap();
        let different = Identity::load_or_create_in(other.path()).unwrap();
        assert_ne!(first.fingerprint, different.fingerprint);
    }
}
