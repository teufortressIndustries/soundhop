//! SoundHop Transport v3 (`SHT3`): encrypted Opus & PCM audio transport.
//!
//! Provides:
//! - Mutual-authenticated, forward-secret handshake (`X25519` + per-device `K_pair` HMAC-SHA256 + HKDF-SHA256)
//! - `ChaCha20-Poly1305` AEAD frame encryption with 16-byte plaintext header AAD and strict replay protection
//! - `AudioCodec` abstraction with zero-dep `PcmCodec` and pure-Rust `OpusCodec` (behind default `opus` feature)
//! - Adaptive jitter buffer with PLC and overrun handling
//! - Standalone DSP helpers (`apply_gain_i16`, `LinearResampler`, `PlaybackResampler`)
//! - TCP `SHT3` server (`Sht3Listener` / `Sht3ServerConnection`) and client (`Sht3ClientConnection`)

pub mod client;
pub mod codec;
pub mod crypto;
pub mod dsp;
pub mod jitter;
pub mod server;
pub mod wire;

use std::collections::HashMap;
use std::sync::RwLock;

use thiserror::Error;

pub use client::{Sht3ClientConfig, Sht3ClientConnection};
#[cfg(feature = "opus")]
pub use codec::OpusCodec;
pub use codec::{create_codec, AudioCodec, AudioProfile, PcmCodec};
pub use crypto::{AeadSession, EphemeralKeypair, Role, SessionKeys};
pub use dsp::{apply_gain_buffer, apply_gain_i16, LinearResampler, PlaybackResampler};
pub use jitter::{JitterBuffer, JitterConfig, JitterStats, PopOutcome};
pub use server::{Sht3Listener, Sht3ServerConfig, Sht3ServerConnection};
pub use wire::{
    AudioParams, ClientHello, CodecId, ControlMessage, DecryptedFrame, EncryptedFrame, FrameHeader,
    FrameType, HandshakeStatus, PingPayload, PongPayload, ServerHello, FLAG_DISCONTINUITY,
    FLAG_DTX, FLAG_FEC,
};

/// 4-byte protocol magic for SoundHop Transport v3.
pub const SHT3_MAGIC: [u8; 4] = *b"SHT3";
/// Wire protocol version.
pub const SHT3_VERSION: u8 = 3;
/// Default dedicated TCP port for SHT3 (leaving 4713/tcp for legacy PA-TCP).
pub const DEFAULT_V3_PORT: u16 = 4714;
/// Maximum encrypted frame payload length in bytes (pre-allocation cap).
pub const MAX_PAYLOAD_LEN: usize = 8192;
/// Fixed plaintext frame header length in bytes (used as AEAD AAD).
pub const FRAME_HEADER_LEN: usize = 16;
/// Poly1305 authentication tag length in bytes.
pub const AEAD_TAG_LEN: usize = 16;
/// Maximum number of frames per directional key before rekey is required (~93h @ 50fps).
pub const MAX_FRAMES_PER_KEY: u64 = 1 << 24;

/// Result alias for transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Errors returned by the SHT3 transport layer.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("wire protocol error: {0}")]
    Wire(String),

    #[error("codec error: {0}")]
    Codec(String),

    #[error("authentication rejected: {0}")]
    AuthRejected(String),

    #[error("replay or out-of-order frame detected: expected seq {expected}, got {got}")]
    ReplayDetected { expected: u64, got: u64 },

    #[error("payload length {len} exceeds maximum allowed {max} bytes")]
    PayloadTooLarge { len: usize, max: usize },

    #[error("session frame limit reached; rekey required")]
    RekeyRequired,

    #[error("unsupported audio profile or codec: {0}")]
    UnsupportedProfile(String),

    #[error("connection closed")]
    Closed,
}

/// Trust store lookup interface used by the SHT3 server to authenticate paired peers.
pub trait TrustLookup: Send + Sync {
    /// Look up the 32-byte pairwise pre-shared key `K_pair` for `device_id`.
    /// Returns `None` if the peer is unknown or revoked.
    fn lookup_peer_key(&self, device_id: &str) -> Option<[u8; 32]>;
}

/// Thread-safe in-memory [`TrustLookup`] implementation for tests and lightweight embeds.
#[derive(Debug, Default)]
pub struct InMemoryTrustStore {
    peers: RwLock<HashMap<String, [u8; 32]>>,
}

impl InMemoryTrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, device_id: impl Into<String>, k_pair: [u8; 32]) {
        if let Ok(mut guard) = self.peers.write() {
            guard.insert(device_id.into(), k_pair);
        }
    }

    pub fn remove(&self, device_id: &str) -> Option<[u8; 32]> {
        self.peers
            .write()
            .ok()
            .and_then(|mut guard| guard.remove(device_id))
    }
}

impl TrustLookup for InMemoryTrustStore {
    fn lookup_peer_key(&self, device_id: &str) -> Option<[u8; 32]> {
        self.peers
            .read()
            .ok()
            .and_then(|guard| guard.get(device_id).copied())
    }
}
