//! The SMTP password's second lock.
//!
//! File permissions on the database (see [`TursoStore::open`]) stop a casual
//! `ls` on the server from reading the password, but a copy of the database
//! file — a backup, a snapshot, an `scp` by someone who should not have had
//! it — carries the plaintext right along with it. This module is the fix:
//! the column holds ciphertext, and the key that opens it lives in a second
//! file (`in.key`, beside the database) that a database backup does not
//! automatically include.
//!
//! The envelope is `v1:` followed by base64 of a 24-byte XChaCha20-Poly1305
//! nonce and the sealed bytes it protects. The prefix is what lets
//! [`TursoStore::open`] tell a value we sealed apart from a plaintext
//! password left behind by a deployment from before this module existed —
//! see the migration note there. There is deliberately no `v2` yet; add one
//! the day the algorithm changes, and keep `open` accepting both prefixes for
//! as long as ciphertext under the old one might still be sitting in a row.
//!
//! Losing the key file — a backup restored without its sibling, most likely —
//! is not a crash. [`open`] returns `None` on anything it cannot make sense
//! of: wrong key, damaged ciphertext, a value that never was one of ours.
//! The caller in [`TursoStore::smtp_password`] turns that into "no password
//! set", which is the same screen an admin sees on a workspace that never had
//! a sender configured, and the same fix: type the password in again.

use std::io;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as base64;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::Rng;

/// A key is 32 raw bytes; nothing here derives it from a password, because
/// there is no password to derive it from — it is the thing that protects
/// one.
pub const KEY_BYTES: usize = 32;
pub type Key = [u8; KEY_BYTES];

const PREFIX: &str = "v1:";
const NONCE_BYTES: usize = 24;

/// True for a value already in our envelope — the test the migration-at-open
/// logic uses to skip a row it has already sealed.
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(PREFIX)
}

/// Encrypts `plaintext` under `key`, returning the envelope ready to store.
///
/// A fresh random nonce is drawn every call, so sealing the same password
/// twice gives two different strings. XChaCha20-Poly1305's 24-byte nonce is
/// wide enough that "draw at random and never track reuse" is safe for the
/// volume of writes a settings panel produces in a workspace's lifetime.
pub fn seal(key: &Key, plaintext: &str) -> String {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .expect("XChaCha20-Poly1305 cannot fail to encrypt a plaintext this small");
    let mut payload = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&ciphertext);
    format!("{PREFIX}{}", base64.encode(payload))
}

/// Reverses [`seal`]. `None` covers every way this can fail to come back —
/// not our prefix, not valid base64, too short to hold a nonce, wrong key,
/// or a tampered/truncated ciphertext that fails the Poly1305 tag check —
/// and every one of those is meant to degrade to "no password", never panic
/// or bubble an error past the mailer.
pub fn open(key: &Key, sealed: &str) -> Option<String> {
    let body = sealed.strip_prefix(PREFIX)?;
    let payload = base64.decode(body).ok()?;
    if payload.len() < NONCE_BYTES {
        return None;
    }
    let (nonce_bytes, ciphertext) = payload.split_at(NONCE_BYTES);
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XNonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}

/// Reads the key at `path`, generating and writing a fresh one (mode 0600,
/// where the platform has modes) if the file is not there yet.
///
/// Called once, at [`TursoStore::open`] — the key is loaded, not re-read per
/// query, so a corrupted key file is a boot-time failure, not a per-send one.
pub fn load_or_create_key(path: &Path) -> io::Result<Key> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let key: Key = bytes.try_into().map_err(|bytes: Vec<u8>| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} holds {} bytes, not the {KEY_BYTES} of a key",
                        path.display(),
                        bytes.len()
                    ),
                )
            })?;
            Ok(key)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut key = [0u8; KEY_BYTES];
            rand::rng().fill_bytes(&mut key);
            std::fs::write(path, key)?;
            restrict(path)?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

/// 0600 on a freshly written file holding key material or a live credential.
#[cfg(unix)]
pub fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        let mut k = [0u8; KEY_BYTES];
        rand::rng().fill_bytes(&mut k);
        k
    }

    #[test]
    fn roundtrip() {
        let k = key();
        let sealed = seal(&k, "hunter2");
        assert!(is_sealed(&sealed));
        assert_eq!(open(&k, &sealed).as_deref(), Some("hunter2"));
    }

    #[test]
    fn tamper_fails() {
        let k = key();
        let mut sealed = seal(&k, "hunter2").into_bytes();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        let sealed = String::from_utf8(sealed).unwrap();
        assert_eq!(open(&k, &sealed), None);
    }

    #[test]
    fn wrong_key_fails() {
        let sealed = seal(&key(), "hunter2");
        assert_eq!(open(&key(), &sealed), None);
    }

    #[test]
    fn non_envelope_fails() {
        let k = key();
        assert_eq!(open(&k, "hunter2"), None);
        assert_eq!(open(&k, ""), None);
        assert_eq!(open(&k, "v1:not-base64!!"), None);
    }

    #[test]
    fn load_or_create_key_persists_and_restricts() {
        let dir = std::env::temp_dir().join(format!("in-key-test-{}", unique_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in.key");
        let first = load_or_create_key(&path).unwrap();
        let second = load_or_create_key(&path).unwrap();
        assert_eq!(first, second, "a second open reuses the same key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A tiny stand-in so this module does not reach for the `ulid` crate just
    // to name a scratch directory.
    fn unique_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }
}
