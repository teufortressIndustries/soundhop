//! Pairing v2: per-device Ed25519 identity, trust store (`TrustLookup`),
//! interactive 4-step TCP Pair Request handshake with 6-digit SAS, and
//! headless one-time code pairing.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::{Rng, RngCore};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::transport::TrustLookup;
use crate::util::{atomic_write, config_dir, lock_or_recover, new_device_id, now_secs};

type HmacSha256 = Hmac<Sha256>;

/// Default TCP port for interactive Pairing v2 handshakes.
pub const DEFAULT_PAIR_PORT: u16 = 4715;
/// Wire version for Pairing v2 messages.
pub const PAIR_PROTOCOL_VERSION: u8 = 3;
/// Pending interactive pair request time-to-live (seconds).
pub const PENDING_REQUEST_TTL_SECS: u64 = 60;
/// Headless one-time pairing code time-to-live (seconds).
pub const HEADLESS_CODE_TTL_SECS: u64 = 120;
/// Maximum simultaneous pending interactive pair requests.
pub const MAX_PENDING_REQUESTS: usize = 3;
/// Per-IP minimum interval between interactive pair requests (seconds).
pub const RATE_LIMIT_WINDOW_SECS: u64 = 10;

const PAIR_INIT_SIG_DOMAIN: &[u8] = b"soundhop-pair-init-v3";
const PAIR_CHALLENGE_SIG_DOMAIN: &[u8] = b"soundhop-pair-challenge-v3";
const PAIR_TRANSCRIPT_DOMAIN: &[u8] = b"soundhop-pair-v3";
const PAIR_HKDF_INFO: &[u8] = b"soundhop-pairwise-v3";
const PAIR_HEADLESS_DOMAIN: &[u8] = b"soundhop-headless-v3";
const PAIR_COMMIT_ACCEPT_DOMAIN: &[u8] = b"accept";
const PAIR_COMMIT_ACK_DOMAIN: &[u8] = b"ack";

/// Maximum wire frame size (64 KiB) to prevent memory exhaustion.
const MAX_FRAME_LEN: usize = 65_536;

/// Errors returned by Pairing v2 operations.
#[derive(Debug, Error)]
pub enum PairingError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("wire protocol error: {0}")]
    Wire(String),

    #[error("invalid signature: {0}")]
    InvalidSignature(String),

    #[error("pair request rejected: {0}")]
    Rejected(String),

    #[error("pair request not found: {0}")]
    NotFound(String),

    #[error("rate limit exceeded: {0}")]
    RateLimited(String),

    #[error("pair request timed out")]
    Timeout,
}

pub type PairingResult<T> = Result<T, PairingError>;

// ---------------------------------------------------------------------------
// Custom serde for [u8; 32] as hex string (also accepts byte sequence)
// ---------------------------------------------------------------------------

mod hex_bytes_32 {
    use super::*;

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum HexOrBytes {
            Hex(String),
            Bytes(Vec<u8>),
        }

        let raw = match HexOrBytes::deserialize(deserializer)? {
            HexOrBytes::Hex(s) => hex::decode(s.trim()).map_err(serde::de::Error::custom)?,
            HexOrBytes::Bytes(b) => b,
        };
        if raw.len() != 32 {
            return Err(serde::de::Error::custom(format!(
                "expected 32 bytes, got {}",
                raw.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Persistent Ed25519 DeviceIdentity
// ---------------------------------------------------------------------------

/// Default path for the persistent per-device Ed25519 identity key.
pub fn identity_key_path() -> PathBuf {
    config_dir().join("identity.key")
}

/// Compute human-readable fingerprint from a 32-byte Ed25519 public key:
/// first 8 bytes of `SHA-256(ed25519_pub)` formatted as `XXXX-XXXX-XXXX-XXXX`.
pub fn compute_fingerprint(ed25519_pub: &[u8; 32]) -> String {
    let digest = Sha256::digest(ed25519_pub);
    format!(
        "{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7]
    )
}

#[derive(Serialize, Deserialize)]
struct PersistedIdentity {
    device_id: String,
    #[serde(with = "hex_bytes_32")]
    ed25519_pub: [u8; 32],
    #[serde(with = "hex_bytes_32")]
    ed25519_secret: [u8; 32],
}

/// Long-term per-device Ed25519 cryptographic identity stored in `identity.key`.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct DeviceIdentity {
    #[zeroize(skip)]
    pub device_id: String,
    #[zeroize(skip)]
    pub ed25519_pub: [u8; 32],
    pub ed25519_secret: [u8; 32],
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id)
            .field("ed25519_pub", &hex::encode(self.ed25519_pub))
            .field("ed25519_secret", &"[REDACTED]")
            .finish()
    }
}

impl PartialEq for DeviceIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.device_id == other.device_id
            && self.ed25519_pub == other.ed25519_pub
            && self.ed25519_secret == other.ed25519_secret
    }
}

impl DeviceIdentity {
    /// Generate a fresh Ed25519 identity for `device_id`.
    pub fn generate(device_id: impl Into<String>) -> Self {
        let mut rng = OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let ed25519_pub = signing_key.verifying_key().to_bytes();
        let ed25519_secret = signing_key.to_bytes();
        let id = device_id.into();
        let device_id = if id.trim().is_empty() {
            new_device_id()
        } else {
            id.trim().to_string()
        };
        Self {
            device_id,
            ed25519_pub,
            ed25519_secret,
        }
    }

    /// Load identity from `path` (default `~/.config/soundhop/identity.key`), creating and
    /// persisting a new Ed25519 keypair with `0600` permissions on Unix if it does not exist.
    pub fn load_or_create(path: Option<&Path>, device_id: Option<&str>) -> PairingResult<Self> {
        let target = path
            .map(Path::to_path_buf)
            .unwrap_or_else(identity_key_path);
        if target.exists() {
            let raw = fs::read_to_string(&target)?;
            let persisted: PersistedIdentity = serde_json::from_str(&raw).map_err(|e| {
                PairingError::Crypto(format!("Cannot parse {}: {e}", target.display()))
            })?;
            let signing_key = SigningKey::from_bytes(&persisted.ed25519_secret);
            let derived_pub = signing_key.verifying_key().to_bytes();
            if derived_pub != persisted.ed25519_pub {
                return Err(PairingError::Crypto(
                    "identity.key public key does not match secret key".into(),
                ));
            }
            let effective_id = if !persisted.device_id.trim().is_empty() {
                persisted.device_id
            } else {
                device_id
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(new_device_id)
            };
            return Ok(Self {
                device_id: effective_id,
                ed25519_pub: persisted.ed25519_pub,
                ed25519_secret: persisted.ed25519_secret,
            });
        }

        let id = device_id
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(new_device_id);
        let identity = Self::generate(id);
        identity.save(Some(&target))?;
        Ok(identity)
    }

    /// Persist identity atomically with `0600` permissions on Unix.
    pub fn save(&self, path: Option<&Path>) -> PairingResult<PathBuf> {
        let target = path
            .map(Path::to_path_buf)
            .unwrap_or_else(identity_key_path);
        let persisted = PersistedIdentity {
            device_id: self.device_id.clone(),
            ed25519_pub: self.ed25519_pub,
            ed25519_secret: self.ed25519_secret,
        };
        let text = serde_json::to_string_pretty(&persisted)
            .map_err(|e| PairingError::Crypto(format!("Cannot serialize identity: {e}")))?;
        atomic_write(&target, &(text + "\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o600));
        }
        Ok(target)
    }

    /// Human-readable fingerprint (`XXXX-XXXX-XXXX-XXXX`).
    pub fn fingerprint(&self) -> String {
        compute_fingerprint(&self.ed25519_pub)
    }

    /// Lowercase hex-encoded 32-byte Ed25519 public key.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.ed25519_pub)
    }

    /// Sign `message` with this device's Ed25519 secret key.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let signing_key = SigningKey::from_bytes(&self.ed25519_secret);
        signing_key.sign(message).to_bytes()
    }

    /// Verify a 64-byte Ed25519 signature over `message`.
    pub fn verify(
        ed25519_pub: &[u8; 32],
        message: &[u8],
        signature: &[u8; 64],
    ) -> PairingResult<()> {
        let verifying_key = VerifyingKey::from_bytes(ed25519_pub)
            .map_err(|e| PairingError::InvalidSignature(format!("invalid public key: {e}")))?;
        let sig = Signature::from_bytes(signature);
        verifying_key
            .verify(message, &sig)
            .map_err(|e| PairingError::InvalidSignature(format!("signature verify failed: {e}")))
    }
}

// ---------------------------------------------------------------------------
// TrustedPeer & TrustStore (implements transport::TrustLookup)
// ---------------------------------------------------------------------------

/// A cryptographically paired peer persisted in `State::trusted_peers`.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct TrustedPeer {
    pub device_id: String,
    pub display_name: String,
    #[serde(with = "hex_bytes_32")]
    pub ed25519_pub: [u8; 32],
    #[serde(with = "hex_bytes_32")]
    pub pairwise_secret: [u8; 32],
    pub fingerprint: String,
    pub paired_at: f64,
}

impl fmt::Debug for TrustedPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedPeer")
            .field("device_id", &self.device_id)
            .field("display_name", &self.display_name)
            .field("ed25519_pub", &hex::encode(self.ed25519_pub))
            .field("pairwise_secret", &"[REDACTED]")
            .field("fingerprint", &self.fingerprint)
            .field("paired_at", &self.paired_at)
            .finish()
    }
}

impl TrustedPeer {
    /// Returns a public JSON representation safe for IPC/status output, omitting `pairwise_secret`.
    pub fn to_public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "device_id": self.device_id,
            "display_name": self.display_name,
            "ed25519_pub_hex": hex::encode(self.ed25519_pub),
            "fingerprint": self.fingerprint,
            "paired_at": self.paired_at,
        })
    }
}

/// Thread-safe trust store backed by [`TrustedPeer`] records, implementing
/// [`crate::transport::TrustLookup`] for SoundHop Transport v3 authentication.
#[derive(Debug, Default)]
pub struct TrustStore {
    peers: RwLock<HashMap<String, TrustedPeer>>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_peers(peers: impl IntoIterator<Item = TrustedPeer>) -> Self {
        let map = peers
            .into_iter()
            .filter(|p| !p.device_id.trim().is_empty() && p.device_id != "lan-group")
            .map(|p| (p.device_id.clone(), p))
            .collect();
        Self {
            peers: RwLock::new(map),
        }
    }

    pub fn upsert(&self, peer: TrustedPeer) {
        if peer.device_id.trim().is_empty() || peer.device_id == "lan-group" {
            return;
        }
        if let Ok(mut guard) = self.peers.write() {
            guard.insert(peer.device_id.clone(), peer);
        }
    }

    pub fn get(&self, device_id: &str) -> Option<TrustedPeer> {
        self.peers
            .read()
            .ok()
            .and_then(|guard| guard.get(device_id).cloned())
    }

    pub fn remove(&self, device_id: &str) -> Option<TrustedPeer> {
        self.peers
            .write()
            .ok()
            .and_then(|mut guard| guard.remove(device_id))
    }

    pub fn contains(&self, device_id: &str) -> bool {
        self.peers
            .read()
            .ok()
            .is_some_and(|guard| guard.contains_key(device_id))
    }

    pub fn list(&self) -> Vec<TrustedPeer> {
        let mut out: Vec<TrustedPeer> = self
            .peers
            .read()
            .ok()
            .map(|guard| guard.values().cloned().collect())
            .unwrap_or_default();
        out.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        });
        out
    }

    pub fn sync_from(&self, peers: &[TrustedPeer]) {
        if let Ok(mut guard) = self.peers.write() {
            guard.clear();
            for p in peers {
                if !p.device_id.trim().is_empty() && p.device_id != "lan-group" {
                    guard.insert(p.device_id.clone(), p.clone());
                }
            }
        }
    }
}

impl TrustLookup for TrustStore {
    fn lookup_peer_key(&self, device_id: &str) -> Option<[u8; 32]> {
        self.peers
            .read()
            .ok()
            .and_then(|guard| guard.get(device_id).map(|p| p.pairwise_secret))
    }
}

// ---------------------------------------------------------------------------
// Wire messages & cryptographic transcript helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairInit {
    version: u8,
    device_id: String,
    display_name: String,
    platform: String,
    ed25519_pub_hex: String,
    x25519_pub_hex: String,
    nonce_hex: String,
    signature_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    headless_code_mac_hex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PairChallengeMsg {
    Challenge {
        version: u8,
        request_id: String,
        device_id: String,
        display_name: String,
        platform: String,
        ed25519_pub_hex: String,
        x25519_pub_hex: String,
        nonce_hex: String,
        signature_hex: String,
    },
    Reject {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PairCommitMsg {
    Accept { commit_mac_hex: String },
    Reject { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairAckMsg {
    ack_mac_hex: String,
}

fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> PairingResult<()> {
    let payload =
        serde_json::to_vec(msg).map_err(|e| PairingError::Wire(format!("serialize error: {e}")))?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(PairingError::Wire(format!(
            "frame too large: {} bytes",
            payload.len()
        )));
    }
    let len_bytes = (payload.len() as u32).to_be_bytes();
    w.write_all(&len_bytes)?;
    w.write_all(&payload)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> PairingResult<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(PairingError::Wire(format!("invalid frame length: {len}")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| PairingError::Wire(format!("deserialize error: {e}")))
}

fn decode_fixed<const N: usize>(hex_str: &str, label: &str) -> PairingResult<[u8; N]> {
    let bytes = hex::decode(hex_str.trim())
        .map_err(|e| PairingError::Wire(format!("invalid hex for {label}: {e}")))?;
    if bytes.len() != N {
        return Err(PairingError::Wire(format!(
            "invalid length for {label}: expected {N}, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn pair_init_signing_payload(
    device_id: &str,
    display_name: &str,
    platform: &str,
    ed25519_pub: &[u8; 32],
    x25519_pub: &[u8; 32],
    nonce: &[u8; 16],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(PAIR_INIT_SIG_DOMAIN);
    out.extend_from_slice(&(device_id.len() as u32).to_be_bytes());
    out.extend_from_slice(device_id.as_bytes());
    out.extend_from_slice(&(display_name.len() as u32).to_be_bytes());
    out.extend_from_slice(display_name.as_bytes());
    out.extend_from_slice(&(platform.len() as u32).to_be_bytes());
    out.extend_from_slice(platform.as_bytes());
    out.extend_from_slice(ed25519_pub);
    out.extend_from_slice(x25519_pub);
    out.extend_from_slice(nonce);
    out
}

#[allow(clippy::too_many_arguments)]
fn pair_challenge_signing_payload(
    device_id: &str,
    display_name: &str,
    platform: &str,
    ed25519_pub: &[u8; 32],
    x25519_pub: &[u8; 32],
    nonce: &[u8; 16],
    init_nonce: &[u8; 16],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(144);
    out.extend_from_slice(PAIR_CHALLENGE_SIG_DOMAIN);
    out.extend_from_slice(&(device_id.len() as u32).to_be_bytes());
    out.extend_from_slice(device_id.as_bytes());
    out.extend_from_slice(&(display_name.len() as u32).to_be_bytes());
    out.extend_from_slice(display_name.as_bytes());
    out.extend_from_slice(&(platform.len() as u32).to_be_bytes());
    out.extend_from_slice(platform.as_bytes());
    out.extend_from_slice(ed25519_pub);
    out.extend_from_slice(x25519_pub);
    out.extend_from_slice(nonce);
    out.extend_from_slice(init_nonce);
    out
}

#[allow(clippy::too_many_arguments)]
fn compute_transcript_hash(
    init_device_id: &str,
    resp_device_id: &str,
    init_ed25519_pub: &[u8; 32],
    resp_ed25519_pub: &[u8; 32],
    init_x25519_pub: &[u8; 32],
    resp_x25519_pub: &[u8; 32],
    init_nonce: &[u8; 16],
    resp_nonce: &[u8; 16],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PAIR_TRANSCRIPT_DOMAIN);
    hasher.update((init_device_id.len() as u32).to_be_bytes());
    hasher.update(init_device_id.as_bytes());
    hasher.update((resp_device_id.len() as u32).to_be_bytes());
    hasher.update(resp_device_id.as_bytes());
    hasher.update(init_ed25519_pub);
    hasher.update(resp_ed25519_pub);
    hasher.update(init_x25519_pub);
    hasher.update(resp_x25519_pub);
    hasher.update(init_nonce);
    hasher.update(resp_nonce);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Compute 6-digit SAS code (`XXX-XXX`) bound to the handshake transcript hash.
pub fn format_sas_code(transcript_hash: &[u8; 32]) -> String {
    let raw = u32::from_be_bytes([
        transcript_hash[0],
        transcript_hash[1],
        transcript_hash[2],
        transcript_hash[3],
    ]) % 1_000_000;
    format!("{:03}-{:03}", raw / 1000, raw % 1000)
}

fn derive_pairwise_secret(
    shared_dh: &[u8; 32],
    transcript_hash: &[u8; 32],
) -> PairingResult<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(Some(transcript_hash), shared_dh);
    let mut okm = [0u8; 32];
    hk.expand(PAIR_HKDF_INFO, &mut okm)
        .map_err(|_| PairingError::Crypto("HKDF-SHA256 expand failed".into()))?;
    Ok(okm)
}

fn compute_commit_mac(
    pairwise_secret: &[u8; 32],
    domain: &[u8],
    transcript_hash: &[u8; 32],
) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(pairwise_secret).expect("HMAC accepts any key size");
    mac.update(domain);
    mac.update(transcript_hash);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn verify_commit_mac(
    pairwise_secret: &[u8; 32],
    domain: &[u8],
    transcript_hash: &[u8; 32],
    expected: &[u8; 32],
) -> bool {
    let mut mac = HmacSha256::new_from_slice(pairwise_secret).expect("HMAC accepts any key size");
    mac.update(domain);
    mac.update(transcript_hash);
    mac.verify_slice(expected).is_ok()
}

fn compute_headless_code_mac(
    code: &str,
    device_id: &str,
    ed25519_pub: &[u8; 32],
    x25519_pub: &[u8; 32],
    nonce: &[u8; 16],
) -> [u8; 32] {
    let normalized = normalize_headless_code(code);
    let mut mac =
        HmacSha256::new_from_slice(normalized.as_bytes()).expect("HMAC accepts any key size");
    mac.update(PAIR_HEADLESS_DOMAIN);
    mac.update(&(device_id.len() as u32).to_be_bytes());
    mac.update(device_id.as_bytes());
    mac.update(ed25519_pub);
    mac.update(x25519_pub);
    mac.update(nonce);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn verify_headless_code_mac(
    code: &str,
    device_id: &str,
    ed25519_pub: &[u8; 32],
    x25519_pub: &[u8; 32],
    nonce: &[u8; 16],
    expected: &[u8; 32],
) -> bool {
    let normalized = normalize_headless_code(code);
    let mut mac =
        HmacSha256::new_from_slice(normalized.as_bytes()).expect("HMAC accepts any key size");
    mac.update(PAIR_HEADLESS_DOMAIN);
    mac.update(&(device_id.len() as u32).to_be_bytes());
    mac.update(device_id.as_bytes());
    mac.update(ed25519_pub);
    mac.update(x25519_pub);
    mac.update(nonce);
    mac.verify_slice(expected).is_ok()
}

fn normalize_headless_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}

fn generate_random_nonce() -> [u8; 16] {
    let mut n = [0u8; 16];
    OsRng.fill_bytes(&mut n);
    n
}

fn generate_request_id() -> String {
    let mut buf = [0u8; 8];
    OsRng.fill_bytes(&mut buf);
    format!("pair-{}", hex::encode(buf))
}

fn x25519_dh(secret: X25519StaticSecret, peer_pub: &[u8; 32]) -> PairingResult<[u8; 32]> {
    let shared = secret.diffie_hellman(&X25519PublicKey::from(*peer_pub));
    let bytes = shared.to_bytes();
    if bytes.iter().all(|&b| b == 0) {
        return Err(PairingError::Crypto(
            "X25519 shared secret is all-zero".into(),
        ));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// PendingPairRequest, HeadlessOneTimeCode & PairingManager
// ---------------------------------------------------------------------------

/// A pending pair request awaiting user confirmation/approval or denial.
///
/// - When `outgoing == false`, this is an **incoming** request on the responder PC.
/// - When `outgoing == true`, this is an **outgoing** request on the initiator PC
///   awaiting mutual 6-digit SAS verification before Step 4 (`PairAckMsg`) is sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingPairRequest {
    pub request_id: String,
    pub device_id: String,
    pub display_name: String,
    pub platform: String,
    pub remote_addr: String,
    pub fingerprint: String,
    pub sas_code: String,
    pub created_at: f64,
    pub expires_at: f64,
    #[serde(default)]
    pub outgoing: bool,
}

/// Generated headless one-time pairing code (valid for 120s).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeadlessPairCode {
    pub code: String,
    pub created_at: f64,
    pub expires_at: f64,
    pub ttl_secs: u64,
}

enum PairDecision {
    Approve,
    Deny,
}

struct PendingEntry {
    info: PendingPairRequest,
    decision_tx: mpsc::Sender<PairDecision>,
    deadline: Instant,
}

struct ActiveHeadlessCode {
    code: String,
    deadline: Instant,
}

type OnPairedCallback = Arc<dyn Fn(TrustedPeer) + Send + Sync>;

/// Coordinates pending interactive pair requests, headless one-time codes,
/// per-IP rate limiting, and the shared [`TrustStore`].
pub struct PairingManager {
    trust_store: Arc<TrustStore>,
    pending: Mutex<HashMap<String, PendingEntry>>,
    rate_limit: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    headless_code: Mutex<Option<ActiveHeadlessCode>>,
    on_paired: Mutex<Option<OnPairedCallback>>,
    pending_ttl: Duration,
    headless_ttl: Duration,
    rate_limit_window: Duration,
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new(Arc::new(TrustStore::new()))
    }
}

impl PairingManager {
    pub fn new(trust_store: Arc<TrustStore>) -> Self {
        Self {
            trust_store,
            pending: Mutex::new(HashMap::new()),
            rate_limit: Mutex::new(HashMap::new()),
            headless_code: Mutex::new(None),
            on_paired: Mutex::new(None),
            pending_ttl: Duration::from_secs(PENDING_REQUEST_TTL_SECS),
            headless_ttl: Duration::from_secs(HEADLESS_CODE_TTL_SECS),
            rate_limit_window: Duration::from_secs(RATE_LIMIT_WINDOW_SECS),
        }
    }

    /// Return a handle to the underlying [`TrustStore`] (implements [`TrustLookup`]).
    pub fn trust_store(&self) -> Arc<TrustStore> {
        Arc::clone(&self.trust_store)
    }

    /// Register a callback invoked whenever a peer completes pairing.
    pub fn set_on_paired<F>(&self, cb: F)
    where
        F: Fn(TrustedPeer) + Send + Sync + 'static,
    {
        *lock_or_recover(&self.on_paired) = Some(Arc::new(cb));
    }

    fn notify_paired(&self, peer: TrustedPeer) {
        self.trust_store.upsert(peer.clone());
        if let Some(cb) = lock_or_recover(&self.on_paired).clone() {
            cb(peer);
        }
    }

    /// Generate a 120-second headless one-time pairing code (`XXXX-XXXX`).
    pub fn generate_one_time_code(&self) -> HeadlessPairCode {
        let mut rng = OsRng;
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut chars = String::with_capacity(9);
        for i in 0..8 {
            if i == 4 {
                chars.push('-');
            }
            let idx = rng.gen_range(0..ALPHABET.len());
            chars.push(ALPHABET[idx] as char);
        }
        let now = now_secs();
        let ttl_secs = self.headless_ttl.as_secs();
        let deadline = Instant::now() + self.headless_ttl;
        *lock_or_recover(&self.headless_code) = Some(ActiveHeadlessCode {
            code: chars.clone(),
            deadline,
        });
        HeadlessPairCode {
            code: chars,
            created_at: now,
            expires_at: now + ttl_secs as f64,
            ttl_secs,
        }
    }

    /// Check and consume the active headless one-time code if `mac_bytes` matches.
    fn try_consume_headless_code(
        &self,
        device_id: &str,
        ed25519_pub: &[u8; 32],
        x25519_pub: &[u8; 32],
        nonce: &[u8; 16],
        mac_bytes: &[u8; 32],
    ) -> bool {
        let mut guard = lock_or_recover(&self.headless_code);
        let Some(active) = guard.as_ref() else {
            return false;
        };
        if Instant::now() >= active.deadline {
            *guard = None;
            return false;
        }
        if verify_headless_code_mac(
            &active.code,
            device_id,
            ed25519_pub,
            x25519_pub,
            nonce,
            mac_bytes,
        ) {
            *guard = None;
            true
        } else {
            false
        }
    }

    fn prune_expired_locked(pending: &mut HashMap<String, PendingEntry>) {
        let now = Instant::now();
        pending.retain(|_, entry| entry.deadline > now);
    }

    fn check_rate_limit(&self, ip: IpAddr) -> PairingResult<()> {
        let now = Instant::now();
        let mut guard = lock_or_recover(&self.rate_limit);
        let timestamps = guard.entry(ip).or_default();
        timestamps.retain(|ts| now.duration_since(*ts) < self.rate_limit_window);
        if timestamps.len() >= 5 {
            return Err(PairingError::RateLimited(format!(
                "too many pair requests from {ip}"
            )));
        }
        timestamps.push(now);
        Ok(())
    }

    fn register_pending(
        &self,
        info: PendingPairRequest,
    ) -> PairingResult<mpsc::Receiver<PairDecision>> {
        let mut guard = lock_or_recover(&self.pending);
        Self::prune_expired_locked(&mut guard);
        if guard.len() >= MAX_PENDING_REQUESTS {
            return Err(PairingError::RateLimited(
                "maximum pending pair requests reached".into(),
            ));
        }
        let (tx, rx) = mpsc::channel();
        let deadline = Instant::now() + self.pending_ttl;
        guard.insert(
            info.request_id.clone(),
            PendingEntry {
                info,
                decision_tx: tx,
                deadline,
            },
        );
        Ok(rx)
    }

    fn remove_pending(&self, request_id: &str) {
        lock_or_recover(&self.pending).remove(request_id);
    }

    /// List currently active (non-expired) pending pair requests.
    pub fn pending_requests(&self) -> Vec<PendingPairRequest> {
        let mut guard = lock_or_recover(&self.pending);
        Self::prune_expired_locked(&mut guard);
        let mut list: Vec<PendingPairRequest> = guard.values().map(|e| e.info.clone()).collect();
        list.sort_by(|a, b| {
            a.created_at
                .partial_cmp(&b.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        list
    }

    /// Approve a pending pair request by `request_id` (or matching 6-digit SAS code).
    pub fn approve(&self, request_id_or_sas: &str) -> PairingResult<PendingPairRequest> {
        let entry = self.take_pending(request_id_or_sas)?;
        let _ = entry.decision_tx.send(PairDecision::Approve);
        Ok(entry.info)
    }

    /// Deny a pending pair request by `request_id` (or matching 6-digit SAS code).
    pub fn deny(&self, request_id_or_sas: &str) -> PairingResult<PendingPairRequest> {
        let entry = self.take_pending(request_id_or_sas)?;
        let _ = entry.decision_tx.send(PairDecision::Deny);
        Ok(entry.info)
    }

    fn take_pending(&self, id_or_sas: &str) -> PairingResult<PendingEntry> {
        let needle = id_or_sas.trim();
        let mut guard = lock_or_recover(&self.pending);
        Self::prune_expired_locked(&mut guard);
        if let Some(entry) = guard.remove(needle) {
            return Ok(entry);
        }
        let normalized_needle = normalize_headless_code(needle);
        let matched_key = guard
            .iter()
            .find(|(_, e)| {
                e.info.sas_code == needle
                    || normalize_headless_code(&e.info.sas_code) == normalized_needle
            })
            .map(|(k, _)| k.clone());
        if let Some(key) = matched_key {
            if let Some(entry) = guard.remove(&key) {
                return Ok(entry);
            }
        }
        Err(PairingError::NotFound(needle.to_string()))
    }

    /// Revoke a trusted peer by `device_id`.
    pub fn revoke(&self, device_id: &str) -> Option<TrustedPeer> {
        self.trust_store.remove(device_id)
    }
}

// ---------------------------------------------------------------------------
// PairingServer (Responder TCP listener)
// ---------------------------------------------------------------------------

/// TCP server listening on `pair_port` (default `4715`) for incoming Pairing v2 requests.
pub struct PairingServer {
    local_addr: SocketAddr,
    stop_flag: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PairingServer {
    /// Bind and start a `PairingServer` on `bind_addr` (e.g. `0.0.0.0:4715` or `127.0.0.1:0`).
    pub fn start(
        bind_addr: SocketAddr,
        identity: DeviceIdentity,
        display_name: String,
        manager: Arc<PairingManager>,
    ) -> PairingResult<Self> {
        let listener = TcpListener::bind(bind_addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_flag);
        let identity = Arc::new(identity);
        let display_name = Arc::new(display_name);

        let handle = thread::spawn(move || {
            while !stop_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, remote_addr)) => {
                        let id_clone = Arc::clone(&identity);
                        let name_clone = Arc::clone(&display_name);
                        let mgr_clone = Arc::clone(&manager);
                        thread::spawn(move || {
                            let _ = handle_responder_connection(
                                stream,
                                remote_addr,
                                &id_clone,
                                &name_clone,
                                &mgr_clone,
                            );
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => {
                        if stop_clone.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        });

        Ok(Self {
            local_addr,
            stop_flag,
            thread: Mutex::new(Some(handle)),
        })
    }

    /// Convenience constructor binding on `0.0.0.0:<port>`.
    pub fn start_on_port(
        port: u16,
        identity: DeviceIdentity,
        display_name: String,
        manager: Arc<PairingManager>,
    ) -> PairingResult<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        Self::start(addr, identity, display_name, manager)
    }

    /// Return the actual bound socket address (useful when binding port `0` in tests).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop the accept loop and join the background thread.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = lock_or_recover(&self.thread).take() {
            let _ = h.join();
        }
    }
}

impl Drop for PairingServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_responder_connection(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    identity: &DeviceIdentity,
    display_name: &str,
    manager: &PairingManager,
) -> PairingResult<TrustedPeer> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    // Step 1: Receive and validate PairInit
    let init: PairInit = read_frame(&mut stream)?;
    if init.version != PAIR_PROTOCOL_VERSION {
        let _ = write_frame(
            &mut stream,
            &PairChallengeMsg::Reject {
                reason: format!("unsupported pairing protocol version {}", init.version),
            },
        );
        return Err(PairingError::Wire("unsupported protocol version".into()));
    }
    if init.device_id.trim().is_empty() || init.device_id == "lan-group" {
        let _ = write_frame(
            &mut stream,
            &PairChallengeMsg::Reject {
                reason: "invalid initiator device_id".into(),
            },
        );
        return Err(PairingError::Wire("invalid initiator device_id".into()));
    }

    let init_ed_pub = decode_fixed::<32>(&init.ed25519_pub_hex, "ed25519_pub")?;
    let init_x_pub = decode_fixed::<32>(&init.x25519_pub_hex, "x25519_pub")?;
    let init_nonce = decode_fixed::<16>(&init.nonce_hex, "nonce")?;
    let init_sig = decode_fixed::<64>(&init.signature_hex, "signature")?;

    let init_sig_payload = pair_init_signing_payload(
        &init.device_id,
        &init.display_name,
        &init.platform,
        &init_ed_pub,
        &init_x_pub,
        &init_nonce,
    );
    if let Err(err) = DeviceIdentity::verify(&init_ed_pub, &init_sig_payload, &init_sig) {
        let _ = write_frame(
            &mut stream,
            &PairChallengeMsg::Reject {
                reason: "invalid initiator signature".into(),
            },
        );
        return Err(err);
    }

    // Check headless one-time code before rate-limiting interactive prompts.
    let headless_approved = if let Some(mac_hex) = init.headless_code_mac_hex.as_deref() {
        let mac_bytes = decode_fixed::<32>(mac_hex, "headless_code_mac")?;
        if manager.try_consume_headless_code(
            &init.device_id,
            &init_ed_pub,
            &init_x_pub,
            &init_nonce,
            &mac_bytes,
        ) {
            true
        } else {
            let _ = write_frame(
                &mut stream,
                &PairChallengeMsg::Reject {
                    reason: "invalid or expired one-time pairing code".into(),
                },
            );
            return Err(PairingError::Rejected(
                "invalid or expired one-time pairing code".into(),
            ));
        }
    } else {
        if let Err(err) = manager.check_rate_limit(remote_addr.ip()) {
            let _ = write_frame(
                &mut stream,
                &PairChallengeMsg::Reject {
                    reason: err.to_string(),
                },
            );
            return Err(err);
        }
        false
    };

    // Step 2: Generate ephemeral X25519 keypair, compute transcript + SAS, send PairChallenge
    let resp_eph_secret = X25519StaticSecret::random_from_rng(OsRng);
    let resp_eph_pub = X25519PublicKey::from(&resp_eph_secret).to_bytes();
    let resp_nonce = generate_random_nonce();
    let resp_platform = std::env::consts::OS;
    let request_id = generate_request_id();

    let mut shared_dh = x25519_dh(resp_eph_secret, &init_x_pub)?;
    let transcript = compute_transcript_hash(
        &init.device_id,
        &identity.device_id,
        &init_ed_pub,
        &identity.ed25519_pub,
        &init_x_pub,
        &resp_eph_pub,
        &init_nonce,
        &resp_nonce,
    );
    let sas_code = format_sas_code(&transcript);

    let challenge_sig_payload = pair_challenge_signing_payload(
        &identity.device_id,
        display_name,
        resp_platform,
        &identity.ed25519_pub,
        &resp_eph_pub,
        &resp_nonce,
        &init_nonce,
    );
    let challenge_sig = identity.sign(&challenge_sig_payload);

    let pending_rx = if headless_approved {
        None
    } else {
        let now = now_secs();
        let pending_info = PendingPairRequest {
            request_id: request_id.clone(),
            device_id: init.device_id.clone(),
            display_name: init.display_name.clone(),
            platform: init.platform.clone(),
            remote_addr: remote_addr.ip().to_string(),
            fingerprint: compute_fingerprint(&init_ed_pub),
            sas_code: sas_code.clone(),
            created_at: now,
            expires_at: now + manager.pending_ttl.as_secs_f64(),
            outgoing: false,
        };
        match manager.register_pending(pending_info) {
            Ok(rx) => Some(rx),
            Err(err) => {
                shared_dh.zeroize();
                let _ = write_frame(
                    &mut stream,
                    &PairChallengeMsg::Reject {
                        reason: err.to_string(),
                    },
                );
                return Err(err);
            }
        }
    };

    write_frame(
        &mut stream,
        &PairChallengeMsg::Challenge {
            version: PAIR_PROTOCOL_VERSION,
            request_id: request_id.clone(),
            device_id: identity.device_id.clone(),
            display_name: display_name.to_string(),
            platform: resp_platform.to_string(),
            ed25519_pub_hex: hex::encode(identity.ed25519_pub),
            x25519_pub_hex: hex::encode(resp_eph_pub),
            nonce_hex: hex::encode(resp_nonce),
            signature_hex: hex::encode(challenge_sig),
        },
    )?;

    // Step 3: Wait for interactive Approve/Deny (or immediate approval for headless code)
    let approved = if headless_approved {
        true
    } else if let Some(rx) = pending_rx {
        match rx.recv_timeout(manager.pending_ttl) {
            Ok(PairDecision::Approve) => true,
            Ok(PairDecision::Deny) => {
                shared_dh.zeroize();
                let _ = write_frame(
                    &mut stream,
                    &PairCommitMsg::Reject {
                        reason: "Pair request denied by user".into(),
                    },
                );
                return Err(PairingError::Rejected("Pair request denied by user".into()));
            }
            Err(_) => {
                manager.remove_pending(&request_id);
                shared_dh.zeroize();
                let _ = write_frame(
                    &mut stream,
                    &PairCommitMsg::Reject {
                        reason: "Pair request timed out".into(),
                    },
                );
                return Err(PairingError::Timeout);
            }
        }
    } else {
        false
    };

    if !approved {
        shared_dh.zeroize();
        return Err(PairingError::Rejected("Pair request not approved".into()));
    }

    // Step 4: Derive pairwise_secret, send commit HMAC, verify initiator ACK HMAC
    let mut pairwise_secret = derive_pairwise_secret(&shared_dh, &transcript)?;
    shared_dh.zeroize();

    let accept_mac = compute_commit_mac(&pairwise_secret, PAIR_COMMIT_ACCEPT_DOMAIN, &transcript);
    write_frame(
        &mut stream,
        &PairCommitMsg::Accept {
            commit_mac_hex: hex::encode(accept_mac),
        },
    )?;

    let ack: PairAckMsg = read_frame(&mut stream)?;
    let ack_mac = decode_fixed::<32>(&ack.ack_mac_hex, "ack_mac")?;
    if !verify_commit_mac(
        &pairwise_secret,
        PAIR_COMMIT_ACK_DOMAIN,
        &transcript,
        &ack_mac,
    ) {
        pairwise_secret.zeroize();
        return Err(PairingError::InvalidSignature(
            "initiator commit ACK MAC mismatch".into(),
        ));
    }

    let trusted = TrustedPeer {
        device_id: init.device_id,
        display_name: init.display_name,
        ed25519_pub: init_ed_pub,
        pairwise_secret,
        fingerprint: compute_fingerprint(&init_ed_pub),
        paired_at: now_secs(),
    };
    manager.notify_paired(trusted.clone());
    Ok(trusted)
}

// ---------------------------------------------------------------------------
// PairingClient (Initiator)
// ---------------------------------------------------------------------------

/// Result of a completed initiator pair request.
#[derive(Debug, Clone)]
pub struct PairRequestOutcome {
    pub request_id: String,
    pub sas_code: String,
    pub peer: TrustedPeer,
}

/// Callback invoked on the initiator once the 6-digit SAS code is computed.
pub type SasCallback = Box<dyn Fn(&str) + Send>;

/// Perform the 4-step Pairing v2 handshake as the initiator against `host:pair_port`.
///
/// - If `headless_code` is `Some(...)`, attaches an HMAC bound to the code for headless
///   auto-approval.
/// - If `on_sas` is `Some(...)`, invokes it as soon as the 6-digit SAS code is computed
///   (while waiting for the responder to approve/deny).
pub fn initiate_pair_request(
    host: &str,
    pair_port: u16,
    identity: &DeviceIdentity,
    display_name: &str,
    headless_code: Option<&str>,
    timeout: Duration,
    on_sas: Option<SasCallback>,
) -> PairingResult<PairRequestOutcome> {
    initiate_pair_request_with_manager(
        host,
        pair_port,
        identity,
        display_name,
        headless_code,
        timeout,
        None,
        on_sas,
    )
}

/// Perform the 4-step Pairing v2 handshake as the initiator against `host:pair_port`,
/// optionally registering an outgoing [`PendingPairRequest`] in `initiator_manager` so
/// **both** devices display the 6-digit SAS code and **both** users must confirm matching
/// codes before Step 4 (`PairAckMsg`) completes (defeating active MITM auto-accept attacks).
#[allow(clippy::too_many_arguments)]
pub fn initiate_pair_request_with_manager(
    host: &str,
    pair_port: u16,
    identity: &DeviceIdentity,
    display_name: &str,
    headless_code: Option<&str>,
    timeout: Duration,
    initiator_manager: Option<&Arc<PairingManager>>,
    on_sas: Option<SasCallback>,
) -> PairingResult<PairRequestOutcome> {
    let addr = format!("{host}:{pair_port}");
    let mut stream = TcpStream::connect_timeout(
        &addr.parse::<SocketAddr>().or_else(|_| {
            use std::net::ToSocketAddrs;
            addr.to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::other(format!("cannot resolve {addr}")))
        })?,
        Duration::from_secs(5),
    )?;
    let _ = stream.set_read_timeout(Some(timeout.max(Duration::from_secs(5))));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    // Step 1: Send PairInit
    let init_eph_secret = X25519StaticSecret::random_from_rng(OsRng);
    let init_eph_pub = X25519PublicKey::from(&init_eph_secret).to_bytes();
    let init_nonce = generate_random_nonce();
    let platform = std::env::consts::OS;

    let init_sig_payload = pair_init_signing_payload(
        &identity.device_id,
        display_name,
        platform,
        &identity.ed25519_pub,
        &init_eph_pub,
        &init_nonce,
    );
    let init_sig = identity.sign(&init_sig_payload);

    let headless_code_mac_hex =
        headless_code
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|code| {
                hex::encode(compute_headless_code_mac(
                    code,
                    &identity.device_id,
                    &identity.ed25519_pub,
                    &init_eph_pub,
                    &init_nonce,
                ))
            });

    let init_msg = PairInit {
        version: PAIR_PROTOCOL_VERSION,
        device_id: identity.device_id.clone(),
        display_name: display_name.to_string(),
        platform: platform.to_string(),
        ed25519_pub_hex: hex::encode(identity.ed25519_pub),
        x25519_pub_hex: hex::encode(init_eph_pub),
        nonce_hex: hex::encode(init_nonce),
        signature_hex: hex::encode(init_sig),
        headless_code_mac_hex,
    };
    write_frame(&mut stream, &init_msg)?;

    // Step 2: Receive and verify PairChallenge
    let challenge: PairChallengeMsg = read_frame(&mut stream)?;
    let (
        request_id,
        resp_device_id,
        resp_display_name,
        resp_platform,
        resp_ed_pub_hex,
        resp_x_pub_hex,
        resp_nonce_hex,
        resp_sig_hex,
    ) = match challenge {
        PairChallengeMsg::Reject { reason } => return Err(PairingError::Rejected(reason)),
        PairChallengeMsg::Challenge {
            version,
            request_id,
            device_id,
            display_name,
            platform,
            ed25519_pub_hex,
            x25519_pub_hex,
            nonce_hex,
            signature_hex,
        } => {
            if version != PAIR_PROTOCOL_VERSION {
                return Err(PairingError::Wire(format!(
                    "unexpected responder protocol version {version}"
                )));
            }
            (
                request_id,
                device_id,
                display_name,
                platform,
                ed25519_pub_hex,
                x25519_pub_hex,
                nonce_hex,
                signature_hex,
            )
        }
    };

    let resp_ed_pub = decode_fixed::<32>(&resp_ed_pub_hex, "responder ed25519_pub")?;
    let resp_x_pub = decode_fixed::<32>(&resp_x_pub_hex, "responder x25519_pub")?;
    let resp_nonce = decode_fixed::<16>(&resp_nonce_hex, "responder nonce")?;
    let resp_sig = decode_fixed::<64>(&resp_sig_hex, "responder signature")?;

    let challenge_sig_payload = pair_challenge_signing_payload(
        &resp_device_id,
        &resp_display_name,
        &resp_platform,
        &resp_ed_pub,
        &resp_x_pub,
        &resp_nonce,
        &init_nonce,
    );
    DeviceIdentity::verify(&resp_ed_pub, &challenge_sig_payload, &resp_sig)?;

    // Compute shared DH secret, transcript hash, and 6-digit SAS code
    let mut shared_dh = x25519_dh(init_eph_secret, &resp_x_pub)?;
    let transcript = compute_transcript_hash(
        &identity.device_id,
        &resp_device_id,
        &identity.ed25519_pub,
        &resp_ed_pub,
        &init_eph_pub,
        &resp_x_pub,
        &init_nonce,
        &resp_nonce,
    );
    let sas_code = format_sas_code(&transcript);
    if let Some(cb) = on_sas {
        cb(&sas_code);
    }

    let has_headless_code = headless_code.is_some_and(|c| !c.trim().is_empty());

    // Step 3: Wait for PairCommitMsg (and mutual local SAS confirmation when managed)
    let commit_mac_hex = if !has_headless_code {
        if let Some(mgr) = initiator_manager {
            let now = now_secs();
            let pending_info = PendingPairRequest {
                request_id: request_id.clone(),
                device_id: resp_device_id.clone(),
                display_name: resp_display_name.clone(),
                platform: resp_platform.clone(),
                remote_addr: addr.clone(),
                fingerprint: compute_fingerprint(&resp_ed_pub),
                sas_code: sas_code.clone(),
                created_at: now,
                expires_at: now + timeout.as_secs_f64(),
                outgoing: true,
            };
            let decision_rx = mgr.register_pending(pending_info)?;
            let mut reader_stream = stream.try_clone()?;
            let (commit_tx, commit_rx) = mpsc::channel::<PairingResult<PairCommitMsg>>();
            thread::spawn(move || {
                let res = read_frame::<_, PairCommitMsg>(&mut reader_stream);
                let _ = commit_tx.send(res);
            });

            let deadline = Instant::now() + timeout;
            let mut local_approved = false;
            let mut remote_accept_mac: Option<String> = None;

            loop {
                if !local_approved {
                    match decision_rx.try_recv() {
                        Ok(PairDecision::Approve) => {
                            local_approved = true;
                        }
                        Ok(PairDecision::Deny) => {
                            shared_dh.zeroize();
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return Err(PairingError::Rejected(
                                "Pair request cancelled — verification code rejected on this device"
                                    .into(),
                            ));
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(mpsc::TryRecvError::Disconnected) => {
                            shared_dh.zeroize();
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return Err(PairingError::Rejected(
                                "Pair request cancelled on this device".into(),
                            ));
                        }
                    }
                }

                if remote_accept_mac.is_none() {
                    match commit_rx.try_recv() {
                        Ok(Ok(PairCommitMsg::Accept { commit_mac_hex })) => {
                            remote_accept_mac = Some(commit_mac_hex);
                        }
                        Ok(Ok(PairCommitMsg::Reject { reason })) => {
                            mgr.remove_pending(&request_id);
                            shared_dh.zeroize();
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return Err(PairingError::Rejected(reason));
                        }
                        Ok(Err(err)) => {
                            mgr.remove_pending(&request_id);
                            shared_dh.zeroize();
                            return Err(err);
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(mpsc::TryRecvError::Disconnected) => {
                            mgr.remove_pending(&request_id);
                            shared_dh.zeroize();
                            return Err(PairingError::Wire(
                                "responder closed pairing connection".into(),
                            ));
                        }
                    }
                }

                if local_approved {
                    if let Some(mac) = remote_accept_mac {
                        break mac;
                    }
                }

                if Instant::now() >= deadline {
                    mgr.remove_pending(&request_id);
                    shared_dh.zeroize();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return Err(PairingError::Timeout);
                }

                thread::sleep(Duration::from_millis(20));
            }
        } else {
            let commit: PairCommitMsg = read_frame(&mut stream)?;
            match commit {
                PairCommitMsg::Accept { commit_mac_hex } => commit_mac_hex,
                PairCommitMsg::Reject { reason } => {
                    shared_dh.zeroize();
                    return Err(PairingError::Rejected(reason));
                }
            }
        }
    } else {
        let commit: PairCommitMsg = read_frame(&mut stream)?;
        match commit {
            PairCommitMsg::Accept { commit_mac_hex } => commit_mac_hex,
            PairCommitMsg::Reject { reason } => {
                shared_dh.zeroize();
                return Err(PairingError::Rejected(reason));
            }
        }
    };

    let mut pairwise_secret = derive_pairwise_secret(&shared_dh, &transcript)?;
    shared_dh.zeroize();

    let commit_mac = decode_fixed::<32>(&commit_mac_hex, "commit_mac")?;
    if !verify_commit_mac(
        &pairwise_secret,
        PAIR_COMMIT_ACCEPT_DOMAIN,
        &transcript,
        &commit_mac,
    ) {
        pairwise_secret.zeroize();
        return Err(PairingError::InvalidSignature(
            "responder commit MAC mismatch".into(),
        ));
    }

    let ack_mac = compute_commit_mac(&pairwise_secret, PAIR_COMMIT_ACK_DOMAIN, &transcript);
    write_frame(
        &mut stream,
        &PairAckMsg {
            ack_mac_hex: hex::encode(ack_mac),
        },
    )?;

    let peer = TrustedPeer {
        device_id: resp_device_id,
        display_name: resp_display_name,
        ed25519_pub: resp_ed_pub,
        pairwise_secret,
        fingerprint: compute_fingerprint(&resp_ed_pub),
        paired_at: now_secs(),
    };
    Ok(PairRequestOutcome {
        request_id,
        sas_code,
        peer,
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_identity_persistence_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("identity.key");

        let created = DeviceIdentity::load_or_create(Some(&key_path), Some("dev-test-1")).unwrap();
        assert!(key_path.exists());
        assert_eq!(created.device_id, "dev-test-1");
        assert_eq!(created.fingerprint().len(), 19); // XXXX-XXXX-XXXX-XXXX

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "identity.key must have 0600 permissions on Unix"
            );
        }

        let reloaded = DeviceIdentity::load_or_create(Some(&key_path), None).unwrap();
        assert_eq!(reloaded, created);

        let sig = reloaded.sign(b"hello soundhop");
        DeviceIdentity::verify(&reloaded.ed25519_pub, b"hello soundhop", &sig).unwrap();
        assert!(DeviceIdentity::verify(&reloaded.ed25519_pub, b"tampered", &sig).is_err());
    }

    #[test]
    fn test_loopback_interactive_pair_approve_and_trust_lookup() {
        let resp_id = DeviceIdentity::generate("responder-dev");
        let init_id = DeviceIdentity::generate("initiator-dev");
        let trust_store = Arc::new(TrustStore::new());
        let manager = Arc::new(PairingManager::new(Arc::clone(&trust_store)));

        let server = PairingServer::start(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            resp_id.clone(),
            "Living Room".to_string(),
            Arc::clone(&manager),
        )
        .unwrap();
        let port = server.local_addr().port();

        let (sas_tx, sas_rx) = mpsc::channel::<String>();
        let init_id_clone = init_id.clone();
        let client_handle = thread::spawn(move || {
            initiate_pair_request(
                "127.0.0.1",
                port,
                &init_id_clone,
                "Laptop",
                None,
                Duration::from_secs(10),
                Some(Box::new(move |sas| {
                    let _ = sas_tx.send(sas.to_string());
                })),
            )
        });

        let init_sas = sas_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(init_sas.len(), 7);
        assert_eq!(&init_sas[3..4], "-");

        // Poll briefly for responder pending request to register
        let pending = loop {
            let list = manager.pending_requests();
            if !list.is_empty() {
                break list;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].device_id, "initiator-dev");
        assert_eq!(pending[0].sas_code, init_sas);

        manager.approve(&pending[0].request_id).unwrap();

        let outcome = client_handle.join().unwrap().unwrap();
        assert_eq!(outcome.sas_code, init_sas);
        assert_eq!(outcome.peer.device_id, "responder-dev");
        assert_eq!(outcome.peer.display_name, "Living Room");
        assert_eq!(outcome.peer.ed25519_pub, resp_id.ed25519_pub);

        // Wait briefly for responder handler to finish committing to TrustStore
        let resp_key = loop {
            if let Some(k) = trust_store.lookup_peer_key("initiator-dev") {
                break k;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(resp_key, outcome.peer.pairwise_secret);

        // Revoke flow
        let revoked = manager.revoke("initiator-dev");
        assert!(revoked.is_some());
        assert!(trust_store.lookup_peer_key("initiator-dev").is_none());

        server.stop();
    }

    #[test]
    fn test_loopback_interactive_pair_deny() {
        let resp_id = DeviceIdentity::generate("responder-deny");
        let init_id = DeviceIdentity::generate("initiator-deny");
        let trust_store = Arc::new(TrustStore::new());
        let manager = Arc::new(PairingManager::new(Arc::clone(&trust_store)));

        let server = PairingServer::start(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            resp_id,
            "Studio".to_string(),
            Arc::clone(&manager),
        )
        .unwrap();
        let port = server.local_addr().port();

        let client_handle = thread::spawn(move || {
            initiate_pair_request(
                "127.0.0.1",
                port,
                &init_id,
                "Guest Laptop",
                None,
                Duration::from_secs(10),
                None,
            )
        });

        let pending = loop {
            let list = manager.pending_requests();
            if !list.is_empty() {
                break list;
            }
            thread::sleep(Duration::from_millis(10));
        };
        manager.deny(&pending[0].request_id).unwrap();

        let res = client_handle.join().unwrap();
        assert!(matches!(res, Err(PairingError::Rejected(_))));
        assert!(trust_store.lookup_peer_key("initiator-deny").is_none());

        server.stop();
    }

    #[test]
    fn test_loopback_headless_one_time_code_flow() {
        let resp_id = DeviceIdentity::generate("pi-speaker");
        let init_id = DeviceIdentity::generate("laptop-headless");
        let trust_store = Arc::new(TrustStore::new());
        let manager = Arc::new(PairingManager::new(Arc::clone(&trust_store)));

        let server = PairingServer::start(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            resp_id,
            "Raspberry Pi".to_string(),
            Arc::clone(&manager),
        )
        .unwrap();
        let port = server.local_addr().port();

        let code_info = manager.generate_one_time_code();
        assert_eq!(code_info.ttl_secs, HEADLESS_CODE_TTL_SECS);
        assert_eq!(code_info.code.len(), 9); // XXXX-XXXX

        // Wrong code must be rejected (and must not consume the valid code)
        let bad_res = initiate_pair_request(
            "127.0.0.1",
            port,
            &init_id,
            "Laptop",
            Some("WRONG-CODE"),
            Duration::from_secs(5),
            None,
        );
        assert!(matches!(bad_res, Err(PairingError::Rejected(_))));

        // Valid code auto-approves without interactive confirmation
        let ok_res = initiate_pair_request(
            "127.0.0.1",
            port,
            &init_id,
            "Laptop",
            Some(&code_info.code),
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert_eq!(ok_res.peer.device_id, "pi-speaker");

        let resp_key = loop {
            if let Some(k) = trust_store.lookup_peer_key("laptop-headless") {
                break k;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(resp_key, ok_res.peer.pairwise_secret);

        // One-time code cannot be reused
        let replay_res = initiate_pair_request(
            "127.0.0.1",
            port,
            &init_id,
            "Laptop",
            Some(&code_info.code),
            Duration::from_secs(5),
            None,
        );
        assert!(matches!(replay_res, Err(PairingError::Rejected(_))));

        server.stop();
    }

    #[test]
    fn test_mutual_sas_verification_requires_both_devices_and_blocks_mitm() {
        let resp_id = DeviceIdentity::generate("resp-mutual");
        let init_id = DeviceIdentity::generate("init-mutual");
        let resp_trust = Arc::new(TrustStore::new());
        let resp_manager = Arc::new(PairingManager::new(Arc::clone(&resp_trust)));
        let init_trust = Arc::new(TrustStore::new());
        let init_manager = Arc::new(PairingManager::new(Arc::clone(&init_trust)));

        let server = PairingServer::start(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            resp_id.clone(),
            "Studio PC".to_string(),
            Arc::clone(&resp_manager),
        )
        .unwrap();
        let port = server.local_addr().port();

        // 1. Both devices show identical SAS code; if responder approves first (or MITM auto-accepts),
        // initiator does NOT complete Step 4 until initiator user ALSO confirms!
        let init_mgr_clone = Arc::clone(&init_manager);
        let init_id_clone = init_id.clone();
        let handle = thread::spawn(move || {
            initiate_pair_request_with_manager(
                "127.0.0.1",
                port,
                &init_id_clone,
                "My Laptop",
                None,
                Duration::from_secs(10),
                Some(&init_mgr_clone),
                None,
            )
        });

        let (init_pending, resp_pending) = loop {
            let i_list = init_manager.pending_requests();
            let r_list = resp_manager.pending_requests();
            if !i_list.is_empty() && !r_list.is_empty() {
                break (i_list, r_list);
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(init_pending.len(), 1);
        assert!(init_pending[0].outgoing);
        assert_eq!(init_pending[0].display_name, "Studio PC");
        assert_eq!(resp_pending.len(), 1);
        assert!(!resp_pending[0].outgoing);
        assert_eq!(resp_pending[0].display_name, "My Laptop");
        assert_eq!(init_pending[0].sas_code, resp_pending[0].sas_code);

        // Responder approves first, but initiator user sees a MITM mismatch and clicks Cancel/Deny:
        resp_manager.approve(&resp_pending[0].request_id).unwrap();
        thread::sleep(Duration::from_millis(40));
        // Responder must NOT have trusted the initiator yet because initiator hasn't confirmed Step 4!
        assert!(resp_trust.lookup_peer_key("init-mutual").is_none());

        init_manager.deny(&init_pending[0].request_id).unwrap();
        let cancelled = handle.join().unwrap();
        assert!(matches!(cancelled, Err(PairingError::Rejected(_))));
        assert!(resp_trust.lookup_peer_key("init-mutual").is_none());
        assert!(init_trust.lookup_peer_key("resp-mutual").is_none());

        // 2. Now both sides confirm matching SAS codes -> pairing succeeds on both ends
        let init_mgr_clone2 = Arc::clone(&init_manager);
        let init_id_clone2 = init_id.clone();
        let handle2 = thread::spawn(move || {
            initiate_pair_request_with_manager(
                "127.0.0.1",
                port,
                &init_id_clone2,
                "My Laptop",
                None,
                Duration::from_secs(10),
                Some(&init_mgr_clone2),
                None,
            )
        });

        let (init_p2, resp_p2) = loop {
            let i_list = init_manager.pending_requests();
            let r_list = resp_manager.pending_requests();
            if !i_list.is_empty() && !r_list.is_empty() {
                break (i_list, r_list);
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(init_p2[0].sas_code, resp_p2[0].sas_code);

        // Initiator confirms first, then responder approves
        init_manager.approve(&init_p2[0].request_id).unwrap();
        resp_manager.approve(&resp_p2[0].request_id).unwrap();

        let outcome = handle2.join().unwrap().unwrap();
        assert_eq!(outcome.peer.device_id, "resp-mutual");
        let resp_key = loop {
            if let Some(k) = resp_trust.lookup_peer_key("init-mutual") {
                break k;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(resp_key, outcome.peer.pairwise_secret);

        server.stop();
    }
}
