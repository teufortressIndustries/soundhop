//! SHT3 cryptographic primitives: X25519 ephemeral DH, HMAC-SHA256 mutual auth,
//! HKDF-SHA256 directional key derivation, and ChaCha20-Poly1305 AEAD framing.

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
const HKDF_INFO_PREFIX: &[u8] = b"sht3-session-keys-v3";

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
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts 32-byte keys");
    mac.update(domain);
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn verify_domain_hmac(key: &[u8; 32], domain: &[u8], data: &[u8], expected: &[u8; 32]) -> bool {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts 32-byte keys");
    mac.update(domain);
    mac.update(data);
    mac.verify_slice(expected).is_ok()
}

/// Directional session keys & 4-byte nonce prefixes derived via HKDF-SHA256.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionKeys {
    pub k_c2s: [u8; 32],
    pub k_s2c: [u8; 32],
    pub iv_c2s: [u8; 4],
    pub iv_s2c: [u8; 4],
}

impl SessionKeys {
    /// Derive directional keys and IVs bound to the nonces and full handshake transcript.
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
        let mut okm = Zeroizing::new([0u8; 72]);
        hk.expand(&info, &mut okm[..])
            .map_err(|e| TransportError::Crypto(format!("HKDF expand failed: {e}")))?;

        let mut k_c2s = [0u8; 32];
        let mut k_s2c = [0u8; 32];
        let mut iv_c2s = [0u8; 4];
        let mut iv_s2c = [0u8; 4];

        k_c2s.copy_from_slice(&okm[0..32]);
        k_s2c.copy_from_slice(&okm[32..64]);
        iv_c2s.copy_from_slice(&okm[64..68]);
        iv_s2c.copy_from_slice(&okm[68..72]);

        Ok(Self {
            k_c2s,
            k_s2c,
            iv_c2s,
            iv_s2c,
        })
    }
}

/// Endpoint role determining send/receive directional key assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

/// Stateful ChaCha20-Poly1305 AEAD session enforcing monotonic per-direction sequence numbers.
pub struct AeadSession {
    send_cipher: ChaCha20Poly1305,
    recv_cipher: ChaCha20Poly1305,
    send_iv: [u8; 4],
    recv_iv: [u8; 4],
    send_seq: u64,
    recv_expected_seq: u64,
}

impl Drop for AeadSession {
    fn drop(&mut self) {
        self.send_iv.zeroize();
        self.recv_iv.zeroize();
        self.send_seq.zeroize();
        self.recv_expected_seq.zeroize();
    }
}

impl AeadSession {
    /// Construct a directional `AeadSession` from derived [`SessionKeys`].
    pub fn new(role: Role, keys: &SessionKeys) -> Self {
        let (send_key, recv_key, send_iv, recv_iv) = match role {
            Role::Client => (&keys.k_c2s, &keys.k_s2c, keys.iv_c2s, keys.iv_s2c),
            Role::Server => (&keys.k_s2c, &keys.k_c2s, keys.iv_s2c, keys.iv_c2s),
        };
        Self {
            send_cipher: ChaCha20Poly1305::new(Key::from_slice(send_key)),
            recv_cipher: ChaCha20Poly1305::new(Key::from_slice(recv_key)),
            send_iv,
            recv_iv,
            send_seq: 0,
            recv_expected_seq: 0,
        }
    }

    /// Next outbound sequence number to be assigned.
    pub fn send_seq(&self) -> u64 {
        self.send_seq
    }

    /// Next expected inbound sequence number.
    pub fn recv_expected_seq(&self) -> u64 {
        self.recv_expected_seq
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
        let nonce_bytes = build_nonce(&self.send_iv, header.seq);
        let ciphertext_and_tag = self
            .send_cipher
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
        let nonce_bytes = build_nonce(&self.recv_iv, frame.header.seq);
        let plaintext = self
            .recv_cipher
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

    fn derive_pair() -> (AeadSession, AeadSession) {
        let k_pair = [0xAB; 32];
        let c_eph = EphemeralKeypair::generate();
        let s_eph = EphemeralKeypair::generate();
        let c_pk = c_eph.public_bytes();
        let s_pk = s_eph.public_bytes();
        let c_nonce = generate_nonce();
        let s_nonce = generate_nonce();
        let transcript = b"sample-handshake-transcript";

        let c_shared = c_eph.diffie_hellman(&s_pk).unwrap();
        let s_shared = s_eph.diffie_hellman(&c_pk).unwrap();
        assert_eq!(c_shared.as_bytes(), s_shared.as_bytes());

        let c_keys =
            SessionKeys::derive(c_shared.as_bytes(), &k_pair, &c_nonce, &s_nonce, transcript)
                .unwrap();
        let s_keys =
            SessionKeys::derive(s_shared.as_bytes(), &k_pair, &c_nonce, &s_nonce, transcript)
                .unwrap();

        (
            AeadSession::new(Role::Client, &c_keys),
            AeadSession::new(Role::Server, &s_keys),
        )
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
    fn aead_bidirectional_roundtrip_and_tamper_detection() {
        let (mut client, mut server) = derive_pair();

        let f0 = client
            .seal_frame(FrameType::AudioOpus, 0, 0, b"opus-frame-0")
            .unwrap();
        let f1 = client
            .seal_frame(FrameType::AudioOpus, 0, 960, b"opus-frame-1")
            .unwrap();

        assert_eq!(server.open_frame(&f0).unwrap(), b"opus-frame-0");

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
        assert_eq!(server.open_frame(&f1).unwrap(), b"opus-frame-1");

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
}
