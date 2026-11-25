//! SHT4 cryptographic primitives: X25519 ephemeral DH, HMAC-SHA256 mutual auth,
//! HKDF-SHA256 directional control + SRTP master key derivation, and ChaCha20-Poly1305 AEAD framing.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::transport::wire::{EncryptedFrame, FrameHeader, FrameType};
use crate::transport::{
    TransportError, TransportResult, AEAD_TAG_LEN, MAX_FRAMES_PER_KEY, MAX_PAYLOAD_LEN,
};

type HmacSha256 = Hmac<Sha256>;

const CLIENT_HELLO_MAC_DOMAIN: &[u8] = b"sht3-c-hello";
const SERVER_HELLO_MAC_DOMAIN: &[u8] = b"sht3-s-hello";
const HKDF_INFO_PREFIX: &[u8] = b"sht4-session-keys-v4";

/// Ephemeral X25519 keypair, zeroized automatically on drop.
pub struct EphemeralKeypair {
    secret: StaticSecret,
    public: PublicKey,
}

impl EphemeralKeypair {
    /// Generate a fresh random ephemeral X25519 keypair from `OsRng`.
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Return the 32-byte X25519 public key.
    pub fn public_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Perform X25519 Diffie-Hellman with `peer_public`, consuming the ephemeral secret
    /// and rejecting low-order / all-zero shared secrets.
    pub fn diffie_hellman(self, peer_public: &[u8; 32]) -> TransportResult<SharedSecretBytes> {
        let peer_pk = PublicKey::from(*peer_public);
        let shared = self.secret.diffie_hellman(&peer_pk);
        let raw = shared.to_bytes();
        if raw.iter().all(|&b| b == 0) {
            return Err(TransportError::Crypto(
                "X25519 shared secret is all-zero (invalid peer public key)".into(),
            ));
        }
        Ok(SharedSecretBytes(raw))
    }
}

/// Wrapper around a 32-byte X25519 shared secret that zeroizes on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SharedSecretBytes([u8; 32]);

impl std::fmt::Debug for SharedSecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedSecretBytes([REDACTED])")
    }
}

impl SharedSecretBytes {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Generate a 16-byte random handshake nonce using `OsRng`.
pub fn generate_nonce() -> [u8; 16] {
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Generate a fresh random non-zero, non-`u32::MAX` RTP SSRC using `OsRng`.
pub fn generate_ssrc() -> u32 {
    loop {
        let ssrc = OsRng.next_u32();
        if ssrc != 0 && ssrc != u32::MAX {
            return ssrc;
        }
    }
}

/// Compute `HMAC-SHA256(k_pair, "sht3-c-hello" || client_transcript)`.
pub fn compute_client_hello_mac(k_pair: &[u8; 32], client_transcript: &[u8]) -> [u8; 32] {
    compute_domain_hmac(k_pair, CLIENT_HELLO_MAC_DOMAIN, client_transcript)
}

/// Constant-time verification of `ClientHello` MAC.
pub fn verify_client_hello_mac(
    k_pair: &[u8; 32],
    client_transcript: &[u8],
    expected_mac: &[u8; 32],
) -> bool {
    verify_domain_hmac(
        k_pair,
        CLIENT_HELLO_MAC_DOMAIN,
        client_transcript,
        expected_mac,
    )
}

/// Compute `HMAC-SHA256(k_pair, "sht3-s-hello" || full_transcript)`.
pub fn compute_server_hello_mac(k_pair: &[u8; 32], full_transcript: &[u8]) -> [u8; 32] {
    compute_domain_hmac(k_pair, SERVER_HELLO_MAC_DOMAIN, full_transcript)
}

/// Constant-time verification of `ServerHello` MAC.
pub fn verify_server_hello_mac(
    k_pair: &[u8; 32],
    full_transcript: &[u8],
    expected_mac: &[u8; 32],
) -> bool {
    verify_domain_hmac(
        k_pair,
        SERVER_HELLO_MAC_DOMAIN,
        full_transcript,
        expected_mac,
    )
}

fn compute_domain_hmac(key: &[u8; 32], domain: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = hmac_from_key(key);
    mac.update(domain);
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn verify_domain_hmac(key: &[u8; 32], domain: &[u8], data: &[u8], expected: &[u8; 32]) -> bool {
    let mut mac = hmac_from_key(key);
    mac.update(domain);
    mac.update(data);
    mac.verify_slice(expected).is_ok()
}

/// Construct `Hmac<Sha256>` infallibly from a 32-byte key by zero-padding to the
/// 64-byte SHA-256 block size (RFC 2104 key normalization without `.expect()`).
fn hmac_from_key(key: &[u8; 32]) -> HmacSha256 {
    let mut padded = Zeroizing::new([0u8; 64]);
    padded[..32].copy_from_slice(key);
    <HmacSha256 as KeyInit>::new((&*padded).into())
}

/// Directional control-channel AEAD keys/IVs and 30-byte SRTP master keys derived via HKDF-SHA256.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionKeys {
    pub k_c2s: [u8; 32],
    pub k_s2c: [u8; 32],
    pub iv_c2s: [u8; 4],
    pub iv_s2c: [u8; 4],
    /// Client -> Server SRTP master key (16B AES-128 key + 14B SRTP salt).
    pub srtp_master_key_c2s: [u8; 30],
    /// Server -> Client SRTP master key (16B AES-128 key + 14B SRTP salt).
    pub srtp_master_key_s2c: [u8; 30],
}

impl SessionKeys {
    /// Derive directional AEAD + SRTP master keys bound to the nonces and full handshake transcript.
    pub fn derive(
        shared_dh: &[u8; 32],
        k_pair: &[u8; 32],
        client_nonce: &[u8; 16],
        server_nonce: &[u8; 16],
        full_transcript: &[u8],
    ) -> TransportResult<Self> {
        if shared_dh.iter().all(|&b| b == 0) {
            return Err(TransportError::Crypto(
                "refusing to derive session keys from all-zero DH secret".into(),
            ));
        }

        let mut salt_hasher = Sha256::new();
        salt_hasher.update(client_nonce);
        salt_hasher.update(server_nonce);
        let salt = salt_hasher.finalize();

        let mut ikm = Zeroizing::new([0u8; 64]);
        ikm[..32].copy_from_slice(shared_dh);
        ikm[32..].copy_from_slice(k_pair);

        let transcript_hash = Sha256::digest(full_transcript);
        let mut info = Vec::with_capacity(HKDF_INFO_PREFIX.len() + 32);
        info.extend_from_slice(HKDF_INFO_PREFIX);
        info.extend_from_slice(&transcript_hash);

        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm[..]);
        let mut okm = Zeroizing::new([0u8; 132]);
        hk.expand(&info, &mut okm[..])
            .map_err(|e| TransportError::Crypto(format!("HKDF expand failed: {e}")))?;

        let mut k_c2s = [0u8; 32];
        let mut k_s2c = [0u8; 32];
        let mut iv_c2s = [0u8; 4];
        let mut iv_s2c = [0u8; 4];
        let mut srtp_master_key_c2s = [0u8; 30];
        let mut srtp_master_key_s2c = [0u8; 30];

        k_c2s.copy_from_slice(&okm[0..32]);
        k_s2c.copy_from_slice(&okm[32..64]);
        iv_c2s.copy_from_slice(&okm[64..68]);
        iv_s2c.copy_from_slice(&okm[68..72]);
        srtp_master_key_c2s.copy_from_slice(&okm[72..102]);
        srtp_master_key_s2c.copy_from_slice(&okm[102..132]);

        Ok(Self {
            k_c2s,
            k_s2c,
            iv_c2s,
            iv_s2c,
            srtp_master_key_c2s,
            srtp_master_key_s2c,
        })
    }
}

/// Endpoint role determining send/receive directional key assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

/// Outbound encryption half of an [`AeadSession`], zeroizing its IV and sequence counter on drop.
pub struct AeadSealHalf {
    cipher: ChaCha20Poly1305,
    iv: [u8; 4],
    send_seq: u64,
}

impl Drop for AeadSealHalf {
    fn drop(&mut self) {
        self.iv.zeroize();
        self.send_seq.zeroize();
    }
}

impl AeadSealHalf {
    /// Next outbound sequence number to be assigned.
    pub fn send_seq(&self) -> u64 {
        self.send_seq
    }

    /// Number of frames that can still be sealed before [`MAX_FRAMES_PER_KEY`] is reached.
    pub fn frames_remaining(&self) -> u64 {
        MAX_FRAMES_PER_KEY.saturating_sub(self.send_seq)
    }

    /// Encrypt `plaintext` into an [`EncryptedFrame`] bound to its 16-byte plaintext header AAD.
    pub fn seal_frame(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        plaintext: &[u8],
    ) -> TransportResult<EncryptedFrame> {
        if plaintext.len() > MAX_PAYLOAD_LEN {
            return Err(TransportError::PayloadTooLarge {
                len: plaintext.len(),
                max: MAX_PAYLOAD_LEN,
            });
        }
        if self.send_seq >= MAX_FRAMES_PER_KEY {
            return Err(TransportError::RekeyRequired);
        }

        let header = FrameHeader {
            frame_type,
            flags,
            payload_len: plaintext.len() as u16,
            seq: self.send_seq,
            timestamp_samples,
        };
        let aad = header.to_bytes();
        let nonce_bytes = build_nonce(&self.iv, header.seq);
        let ciphertext_and_tag = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| TransportError::Crypto("ChaCha20-Poly1305 seal failed".into()))?;

        self.send_seq += 1;
        Ok(EncryptedFrame {
            header,
            ciphertext_and_tag,
        })
    }
}

/// Inbound decryption half of an [`AeadSession`], zeroizing its IV and sequence counter on drop.
pub struct AeadOpenHalf {
    cipher: ChaCha20Poly1305,
    iv: [u8; 4],
    recv_expected_seq: u64,
}

impl Drop for AeadOpenHalf {
    fn drop(&mut self) {
        self.iv.zeroize();
        self.recv_expected_seq.zeroize();
    }
}

impl AeadOpenHalf {
    /// Next expected inbound sequence number.
    pub fn recv_expected_seq(&self) -> u64 {
        self.recv_expected_seq
    }

    /// Authenticate and decrypt `frame`, enforcing strict monotonic `seq == expected_seq`.
    pub fn open_frame(&mut self, frame: &EncryptedFrame) -> TransportResult<Vec<u8>> {
        let payload_len = usize::from(frame.header.payload_len);
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(TransportError::PayloadTooLarge {
                len: payload_len,
                max: MAX_PAYLOAD_LEN,
            });
        }
        if frame.ciphertext_and_tag.len() != payload_len + AEAD_TAG_LEN {
            return Err(TransportError::Wire(format!(
                "encrypted frame length {} != payload_len + tag ({})",
                frame.ciphertext_and_tag.len(),
                payload_len + AEAD_TAG_LEN
            )));
        }
        if self.recv_expected_seq >= MAX_FRAMES_PER_KEY {
            return Err(TransportError::RekeyRequired);
        }
        if frame.header.seq != self.recv_expected_seq {
            return Err(TransportError::ReplayDetected {
                expected: self.recv_expected_seq,
                got: frame.header.seq,
            });
        }

        let aad = frame.header.to_bytes();
        let nonce_bytes = build_nonce(&self.iv, frame.header.seq);
        let plaintext = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &frame.ciphertext_and_tag,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                TransportError::Crypto("ChaCha20-Poly1305 authentication failed".into())
            })?;

        self.recv_expected_seq += 1;
        Ok(plaintext)
    }
}

/// Stateful ChaCha20-Poly1305 AEAD session enforcing monotonic per-direction sequence numbers.
pub struct AeadSession {
    seal: AeadSealHalf,
    open: AeadOpenHalf,
}

impl AeadSession {
    /// Construct a directional `AeadSession` from derived [`SessionKeys`].
    pub fn new(role: Role, keys: &SessionKeys) -> Self {
        let (send_key, recv_key, send_iv, recv_iv) = match role {
            Role::Client => (&keys.k_c2s, &keys.k_s2c, keys.iv_c2s, keys.iv_s2c),
            Role::Server => (&keys.k_s2c, &keys.k_c2s, keys.iv_s2c, keys.iv_c2s),
        };
        Self {
            seal: AeadSealHalf {
                cipher: ChaCha20Poly1305::new(Key::from_slice(send_key)),
                iv: send_iv,
                send_seq: 0,
            },
            open: AeadOpenHalf {
                cipher: ChaCha20Poly1305::new(Key::from_slice(recv_key)),
                iv: recv_iv,
                recv_expected_seq: 0,
            },
        }
    }

    /// Split this session into independent outbound (`AeadSealHalf`) and inbound (`AeadOpenHalf`) halves
    /// for concurrent full-duplex operation.
    pub fn split(self) -> (AeadSealHalf, AeadOpenHalf) {
        (self.seal, self.open)
    }

    /// Next outbound sequence number to be assigned.
    pub fn send_seq(&self) -> u64 {
        self.seal.send_seq()
    }

    /// Next expected inbound sequence number.
    pub fn recv_expected_seq(&self) -> u64 {
        self.open.recv_expected_seq()
    }

    /// Number of outbound frames remaining before rekey is required.
    pub fn frames_remaining(&self) -> u64 {
        self.seal.frames_remaining()
    }

    /// Encrypt `plaintext` into an [`EncryptedFrame`] bound to its 16-byte plaintext header AAD.
    pub fn seal_frame(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        plaintext: &[u8],
    ) -> TransportResult<EncryptedFrame> {
        self.seal
            .seal_frame(frame_type, flags, timestamp_samples, plaintext)
    }

    /// Authenticate and decrypt `frame`, enforcing strict monotonic `seq == expected_seq`.
    pub fn open_frame(&mut self, frame: &EncryptedFrame) -> TransportResult<Vec<u8>> {
        self.open.open_frame(frame)
    }
}

#[inline]
fn build_nonce(iv: &[u8; 4], seq: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0..4].copy_from_slice(iv);
    nonce[4..12].copy_from_slice(&seq.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derive_pair() -> (AeadSession, AeadSession, SessionKeys, SessionKeys) {
        let k_pair = [0xAB; 32];
        let c_eph = EphemeralKeypair::generate();
        let s_eph = EphemeralKeypair::generate();
        let c_pk = c_eph.public_bytes();
        let s_pk = s_eph.public_bytes();
        let c_nonce = generate_nonce();
        let s_nonce = generate_nonce();
        let transcript = b"sample-handshake-transcript-v4";

        let c_shared = c_eph.diffie_hellman(&s_pk).unwrap();
        let s_shared = s_eph.diffie_hellman(&c_pk).unwrap();
        assert_eq!(c_shared.as_bytes(), s_shared.as_bytes());

        let c_keys =
            SessionKeys::derive(c_shared.as_bytes(), &k_pair, &c_nonce, &s_nonce, transcript)
                .unwrap();
        let s_keys =
            SessionKeys::derive(s_shared.as_bytes(), &k_pair, &c_nonce, &s_nonce, transcript)
                .unwrap();

        let client = AeadSession::new(Role::Client, &c_keys);
        let server = AeadSession::new(Role::Server, &s_keys);
        (client, server, c_keys, s_keys)
    }

    #[test]
    fn mutual_hmac_verification() {
        let k_pair = [0x42; 32];
        let wrong_key = [0x43; 32];
        let transcript = b"client-hello-bytes";
        let mac = compute_client_hello_mac(&k_pair, transcript);
        assert!(verify_client_hello_mac(&k_pair, transcript, &mac));
        assert!(!verify_client_hello_mac(&wrong_key, transcript, &mac));
        assert!(!verify_client_hello_mac(&k_pair, b"tampered", &mac));

        let smac = compute_server_hello_mac(&k_pair, transcript);
        assert!(verify_server_hello_mac(&k_pair, transcript, &smac));
        // Domain separation: client MAC != server MAC for identical transcript.
        assert_ne!(mac, smac);
    }

    #[test]
    fn session_keys_derive_distinct_srtp_master_keys_and_ssrc_valid() {
        let (_client, _server, c_keys, s_keys) = derive_pair();
        assert_eq!(c_keys.srtp_master_key_c2s, s_keys.srtp_master_key_c2s);
        assert_eq!(c_keys.srtp_master_key_s2c, s_keys.srtp_master_key_s2c);
        assert_ne!(c_keys.srtp_master_key_c2s, c_keys.srtp_master_key_s2c);
        assert_ne!(c_keys.srtp_master_key_c2s, [0u8; 30]);
        assert_ne!(c_keys.srtp_master_key_s2c, [0u8; 30]);

        for _ in 0..32 {
            let ssrc = generate_ssrc();
            assert_ne!(ssrc, 0);
            assert_ne!(ssrc, u32::MAX);
        }
    }

    #[test]
    fn aead_bidirectional_roundtrip_and_tamper_detection() {
        let (mut client, mut server, _, _) = derive_pair();

        let f0 = client
            .seal_frame(FrameType::Control, 0, 0, &[0x01, 75])
            .unwrap();
        let f1 = client
            .seal_frame(FrameType::Ping, 0, 960, &[1, 2, 3, 4, 5, 6, 7, 8])
            .unwrap();

        assert_eq!(server.open_frame(&f0).unwrap(), &[0x01, 75]);

        // Replay of f0 must fail.
        let replay_err = server.open_frame(&f0).unwrap_err();
        assert!(matches!(
            replay_err,
            TransportError::ReplayDetected {
                expected: 1,
                got: 0
            }
        ));

        // Tampering with AAD header (timestamp) must fail AEAD check without advancing seq.
        let mut tampered_aad = f1.clone();
        tampered_aad.header.timestamp_samples = 9999;
        assert!(matches!(
            server.open_frame(&tampered_aad),
            Err(TransportError::Crypto(_))
        ));

        // Tampering with ciphertext must fail AEAD check.
        let mut tampered_ct = f1.clone();
        tampered_ct.ciphertext_and_tag[0] ^= 0xFF;
        assert!(matches!(
            server.open_frame(&tampered_ct),
            Err(TransportError::Crypto(_))
        ));

        // Authentic f1 still succeeds because failed opens did not advance recv_expected_seq.
        assert_eq!(server.open_frame(&f1).unwrap(), &[1, 2, 3, 4, 5, 6, 7, 8]);

        // Reverse direction (server -> client) uses independent directional key.
        let reply = server
            .seal_frame(FrameType::Control, 0, 0, &[0x03, 90, 0])
            .unwrap();
        assert_eq!(client.open_frame(&reply).unwrap(), &[0x03, 90, 0]);
    }

    #[test]
    fn rejects_all_zero_dh_public_key() {
        let eph = EphemeralKeypair::generate();
        let err = eph.diffie_hellman(&[0u8; 32]).unwrap_err();
        assert!(matches!(err, TransportError::Crypto(_)));
    }

    #[test]
    fn split_halves_encrypt_decrypt_and_track_frames_remaining() {
        let (client, server, _, _) = derive_pair();
        let (mut c_seal, mut c_open) = client.split();
        let (mut s_seal, mut s_open) = server.split();

        assert_eq!(c_seal.send_seq(), 0);
        assert_eq!(c_seal.frames_remaining(), MAX_FRAMES_PER_KEY);
        assert_eq!(s_open.recv_expected_seq(), 0);

        let f0 = c_seal
            .seal_frame(FrameType::Control, 0, 0, b"split-ctrl-0")
            .unwrap();
        assert_eq!(c_seal.send_seq(), 1);
        assert_eq!(c_seal.frames_remaining(), MAX_FRAMES_PER_KEY - 1);

        let f1 = s_seal
            .seal_frame(FrameType::Pong, 0, 0, b"split-pong")
            .unwrap();
        assert_eq!(s_seal.frames_remaining(), MAX_FRAMES_PER_KEY - 1);

        assert_eq!(s_open.open_frame(&f0).unwrap(), b"split-ctrl-0");
        assert_eq!(s_open.recv_expected_seq(), 1);

        assert_eq!(c_open.open_frame(&f1).unwrap(), b"split-pong");
        assert_eq!(c_open.recv_expected_seq(), 1);
    }

    #[test]
    fn hmac_from_key_matches_new_from_slice() {
        let key = [0x5C; 32];
        let data = b"rfc2104-equivalence-check";
        let mut legacy = <HmacSha256 as Mac>::new_from_slice(&key).unwrap();
        legacy.update(CLIENT_HELLO_MAC_DOMAIN);
        legacy.update(data);
        let expected: [u8; 32] = legacy.finalize().into_bytes().into();
        assert_eq!(compute_client_hello_mac(&key, data), expected);
    }
}
