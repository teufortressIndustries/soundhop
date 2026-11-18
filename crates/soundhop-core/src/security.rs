//! PIN & Pre-Shared Key (PSK) cryptographic pairing, HMAC-SHA256 verification,
//! and PulseAudio 256-byte cookie synchronization.

use std::fs;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;

use crate::util::{config_dir, ensure_dir};

type HmacSha256 = Hmac<Sha256>;

/// Standard PulseAudio native protocol cookie size (256 bytes).
pub const PA_COOKIE_SIZE: usize = 256;

/// Default length of generated alphanumeric pairing codes (`A-Z`, `0-9`, ~62 bits of entropy).
pub const PAIRING_PIN_LEN: usize = 12;

/// Number of PBKDF2-HMAC-SHA256 iterations used to stretch pairing secrets.
pub const PBKDF2_ITERATIONS: u32 = 60_000;

const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Generate a random 12-character alphanumeric pairing code (`A-Z`, `0-9`).
pub fn generate_pairing_pin() -> String {
    let mut rng = rand::thread_rng();
    (0..PAIRING_PIN_LEN)
        .map(|_| {
            let idx = rng.gen_range(0..PAIRING_ALPHABET.len());
            PAIRING_ALPHABET[idx] as char
        })
        .collect()
}

/// Derive a 32-byte hex key from a passphrase or PIN and context using PBKDF2-HMAC-SHA256.
pub fn derive_key_hex(secret: &str, context: &str) -> String {
    let salt = format!("soundhop-v2-kdf:{context}");
    let password = secret.trim().as_bytes();
    let mut mac = HmacSha256::new_from_slice(password).expect("HMAC accepts keys of any size");
    mac.update(salt.as_bytes());
    mac.update(&1u32.to_be_bytes());
    let mut u = mac.finalize().into_bytes();
    let mut out = u;
    for _ in 1..PBKDF2_ITERATIONS {
        let mut round_mac =
            HmacSha256::new_from_slice(password).expect("HMAC accepts keys of any size");
        round_mac.update(&u);
        u = round_mac.finalize().into_bytes();
        for (o, b) in out.iter_mut().zip(u.iter()) {
            *o ^= *b;
        }
    }
    hex::encode(out)
}

/// Constant-time byte-slice equality comparison.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Compute an HMAC-SHA256 signature hex for a message given a key (hex or raw string).
pub fn compute_hmac_hex(key: &str, message: &[u8]) -> String {
    let key_bytes = hex::decode(key.trim()).unwrap_or_else(|_| key.trim().as_bytes().to_vec());
    let mut mac = HmacSha256::new_from_slice(&key_bytes).expect("HMAC accepts keys of any size");
    mac.update(message);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verification of an HMAC-SHA256 signature hex.
pub fn verify_hmac_hex(key: &str, message: &[u8], signature_hex: &str) -> bool {
    let Ok(sig_bytes) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    let key_bytes = hex::decode(key.trim()).unwrap_or_else(|_| key.trim().as_bytes().to_vec());
    let Ok(mut mac) = HmacSha256::new_from_slice(&key_bytes) else {
        return false;
    };
    mac.update(message);
    mac.verify_slice(&sig_bytes).is_ok()
}

/// Derive a deterministic 256-byte PulseAudio auth cookie from a shared key.
pub fn derive_pa_cookie(shared_key: &str) -> [u8; PA_COOKIE_SIZE] {
    let mut cookie = [0u8; PA_COOKIE_SIZE];
    for chunk_idx in 0..(PA_COOKIE_SIZE / 32) {
        let mut mac =
            HmacSha256::new_from_slice(shared_key.as_bytes()).expect("HMAC accepts any key size");
        mac.update(b"soundhop-pa-cookie-block:");
        mac.update(&(chunk_idx as u32).to_be_bytes());
        let block = mac.finalize().into_bytes();
        cookie[chunk_idx * 32..(chunk_idx + 1) * 32].copy_from_slice(&block);
    }
    cookie
}

/// Generate a random 256-byte PulseAudio auth cookie.
pub fn random_pa_cookie() -> [u8; PA_COOKIE_SIZE] {
    let mut cookie = [0u8; PA_COOKIE_SIZE];
    rand::thread_rng().fill(&mut cookie[..]);
    cookie
}

/// Default path where SoundHop stores its managed PulseAudio cookie.
pub fn soundhop_cookie_path() -> PathBuf {
    config_dir().join("pulse-cookie")
}

/// Write a 256-byte PulseAudio cookie to disk with `0o600` permissions.
pub fn write_pa_cookie(
    path: Option<&Path>,
    cookie: &[u8; PA_COOKIE_SIZE],
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(soundhop_cookie_path);
    if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&target)?;
    file.write_all(cookie)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o600));
    }
    Ok(target)
}

/// Ensure a 256-byte PulseAudio cookie exists on disk with `0o600` permissions.
/// Derives from `shared_key` when provided; otherwise preserves a valid existing
/// cookie or generates a fresh random 256-byte cookie.
pub fn ensure_pa_cookie(shared_key: Option<&str>) -> std::io::Result<PathBuf> {
    ensure_pa_cookie_at(None, shared_key)
}

/// Same as `ensure_pa_cookie`, targeting an optional custom path.
pub fn ensure_pa_cookie_at(
    path: Option<&Path>,
    shared_key: Option<&str>,
) -> std::io::Result<PathBuf> {
    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(soundhop_cookie_path);
    if let Some(key) = shared_key.map(str::trim).filter(|k| !k.is_empty()) {
        let cookie = derive_pa_cookie(key);
        return write_pa_cookie(Some(&target), &cookie);
    }
    if target.exists()
        && fs::metadata(&target)
            .map(|m| m.len() == PA_COOKIE_SIZE as u64)
            .unwrap_or(false)
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o600));
        }
        return Ok(target);
    }
    let cookie = random_pa_cookie();
    write_pa_cookie(Some(&target), &cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pin_and_hmac_verification() {
        let pin = generate_pairing_pin();
        assert_eq!(pin.len(), PAIRING_PIN_LEN);
        assert!(pin
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));

        let key = derive_key_hex("my-secret-passphrase", "lan");
        assert_eq!(key.len(), 64);
        assert_eq!(key, derive_key_hex("my-secret-passphrase", "lan"));
        assert_ne!(key, derive_key_hex("different-passphrase", "lan"));

        let payload = b"soundhop:v2:offer:peer123";
        let sig = compute_hmac_hex(&key, payload);
        assert!(verify_hmac_hex(&key, payload, &sig));
        assert!(!verify_hmac_hex(&key, b"tampered", &sig));
        assert!(!verify_hmac_hex("wrong-key", payload, &sig));
    }

    #[test]
    fn test_constant_time_eq_and_ensure_pa_cookie() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcd"));

        let dir = tempfile::tempdir().unwrap();
        let cookie_file = dir.path().join("pulse-cookie");
        let p = ensure_pa_cookie_at(Some(&cookie_file), None).unwrap();
        assert_eq!(p, cookie_file);
        let first = fs::read(&cookie_file).unwrap();
        assert_eq!(first.len(), PA_COOKIE_SIZE);

        // Calling again without shared_key preserves the valid existing cookie
        ensure_pa_cookie_at(Some(&cookie_file), None).unwrap();
        assert_eq!(fs::read(&cookie_file).unwrap(), first);

        // Calling with shared_key derives and overwrites deterministically
        ensure_pa_cookie_at(Some(&cookie_file), Some("shared-key-xyz")).unwrap();
        assert_eq!(
            fs::read(&cookie_file).unwrap(),
            derive_pa_cookie("shared-key-xyz").to_vec()
        );
    }

    #[test]
    fn test_pa_cookie_derivation_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let cookie_file = dir.path().join("cookie");
        let c1 = derive_pa_cookie("shared-secret-1");
        let c2 = derive_pa_cookie("shared-secret-1");
        let c3 = derive_pa_cookie("shared-secret-2");
        assert_eq!(c1, c2);
        assert_ne!(c1, c3);

        write_pa_cookie(Some(&cookie_file), &c1).unwrap();
        let read_back = fs::read(&cookie_file).unwrap();
        assert_eq!(read_back.len(), PA_COOKIE_SIZE);
        assert_eq!(&read_back[..], &c1[..]);
    }
}
