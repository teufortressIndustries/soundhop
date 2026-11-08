//! PIN & Pre-Shared Key (PSK) cryptographic pairing, HMAC-SHA256 verification,
//! and PulseAudio 256-byte cookie synchronization.

use std::fs;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::util::{config_dir, ensure_dir};

type HmacSha256 = Hmac<Sha256>;

/// Standard PulseAudio native protocol cookie size (256 bytes).
pub const PA_COOKIE_SIZE: usize = 256;

/// Generate a random 6-digit numeric pairing PIN (`"000000"`..`"999999"`).
pub fn generate_pairing_pin() -> String {
    let n: u32 = rand::thread_rng().gen_range(100_000..1_000_000);
    format!("{n:06}")
}

/// Derive a 32-byte hex key from a passphrase or PIN and salt.
pub fn derive_key_hex(secret: &str, context: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"soundhop-v2-kdf:");
    hasher.update(context.as_bytes());
    hasher.update(b":");
    hasher.update(secret.trim().as_bytes());
    hex::encode(hasher.finalize())
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
    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(soundhop_cookie_path);
    if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
    }
    fs::write(&target, cookie)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o600));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pin_and_hmac_verification() {
        let pin = generate_pairing_pin();
        assert_eq!(pin.len(), 6);
        assert!(pin.chars().all(|c| c.is_ascii_digit()));

        let key = derive_key_hex("my-secret-passphrase", "lan");
        let payload = b"soundhop:v2:offer:peer123";
        let sig = compute_hmac_hex(&key, payload);
        assert!(verify_hmac_hex(&key, payload, &sig));
        assert!(!verify_hmac_hex(&key, b"tampered", &sig));
        assert!(!verify_hmac_hex("wrong-key", payload, &sig));
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
