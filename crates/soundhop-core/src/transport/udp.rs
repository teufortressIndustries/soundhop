//! Encrypted SHT3-over-UDP audio data plane.
//!
//! Reuses `ChaCha20-Poly1305` AEAD framing and `transport/wire.rs` 16-byte `FrameHeader` AAD,
//! sets DSCP EF (`0xB8`, WMM `AC_VO` Voice priority) via `socket2`, and supports single-encode
//! multi-unicast fan-out while keeping TCP `SHT3` for handshake, control, telemetry, and fallback.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, SockRef, Socket, Type};

use crate::transport::crypto::{AeadOpenHalf, AeadSealHalf, ReplayWindow};
use crate::transport::wire::{DecryptedFrame, EncryptedFrame, FrameHeader, FrameType};
use crate::transport::{
    TransportError, TransportResult, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN,
};

/// IP DSCP Expedited Forwarding (DSCP 46 << 2 = `0xB8`), mapped to Wi-Fi WMM Voice (`AC_VO`).
pub const DSCP_EF: u8 = 0xB8;

const MAX_UDP_DATAGRAM_LEN: usize = FRAME_HEADER_LEN + MAX_PAYLOAD_LEN + AEAD_TAG_LEN;

/// Best-effort configuration of DSCP EF (`0xB8`) on a UDP socket for Wi-Fi WMM Voice priority.
pub fn set_dscp_ef(socket: &UdpSocket) {
    let sock_ref = SockRef::from(socket);
    if let Ok(local) = socket.local_addr() {
        match local {
            SocketAddr::V4(_) => {
                let _ = sock_ref.set_tos(u32::from(DSCP_EF));
            }
            SocketAddr::V6(_) => {
                let _ = sock_ref.set_tclass_v6(u32::from(DSCP_EF));
            }
        }
    } else {
        let _ = sock_ref.set_tos(u32::from(DSCP_EF));
    }
}

/// Bind a UDP socket on `addr` with `SO_REUSEADDR` and DSCP EF (`0xB8`) QoS marking.
pub fn bind_dscp_ef_udp(addr: SocketAddr) -> TransportResult<UdpSocket> {
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = socket.set_reuse_address(true);
    socket.bind(&addr.into())?;
    let std_sock: UdpSocket = socket.into();
    set_dscp_ef(&std_sock);
    Ok(std_sock)
}

/// Authenticated unicast destination for [`Sht3UdpSender`] multi-unicast fan-out.
pub struct UdpTarget {
    pub peer_id: String,
    pub addr: SocketAddr,
    pub seal: AeadSealHalf,
    pub frames_sent: u64,
    pub bytes_sent: u64,
}

/// Encrypted SHT3-over-UDP sender supporting single-encode multi-unicast fan-out.
pub struct Sht3UdpSender {
    socket: UdpSocket,
    targets: Vec<UdpTarget>,
    wire_scratch: Vec<u8>,
}

impl Sht3UdpSender {
    /// Bind a new DSCP EF (`0xB8`) UDP sender socket on `bind_addr`.
    pub fn bind(bind_addr: SocketAddr) -> TransportResult<Self> {
        let socket = bind_dscp_ef_udp(bind_addr)?;
        Ok(Self {
            socket,
            targets: Vec::new(),
            wire_scratch: Vec::with_capacity(MAX_UDP_DATAGRAM_LEN),
        })
    }

    /// Wrap an existing [`UdpSocket`], applying DSCP EF (`0xB8`) QoS marking.
    pub fn from_socket(socket: UdpSocket) -> Self {
        set_dscp_ef(&socket);
        Self {
            socket,
            targets: Vec::new(),
            wire_scratch: Vec::with_capacity(MAX_UDP_DATAGRAM_LEN),
        }
    }

    pub fn local_addr(&self) -> TransportResult<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// Add or replace an authenticated unicast peer target.
    pub fn add_target(&mut self, peer_id: impl Into<String>, addr: SocketAddr, seal: AeadSealHalf) {
        let peer_id = peer_id.into();
        if let Some(existing) = self.targets.iter_mut().find(|t| t.peer_id == peer_id) {
            existing.addr = addr;
            existing.seal = seal;
            return;
        }
        self.targets.push(UdpTarget {
            peer_id,
            addr,
            seal,
            frames_sent: 0,
            bytes_sent: 0,
        });
    }

    /// Remove a target by `peer_id`. Returns `true` if a target was removed.
    pub fn remove_target(&mut self, peer_id: &str) -> bool {
        let before = self.targets.len();
        self.targets.retain(|t| t.peer_id != peer_id);
        self.targets.len() != before
    }

    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    /// Minimum remaining AEAD frames before rekey across all registered targets.
    pub fn frames_remaining_min(&self) -> u64 {
        self.targets
            .iter()
            .map(|t| t.seal.frames_remaining())
            .min()
            .unwrap_or(u64::MAX)
    }

    fn seal_and_send_target(
        socket: &UdpSocket,
        wire_scratch: &mut Vec<u8>,
        target: &mut UdpTarget,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        encoded_audio: &[u8],
    ) -> TransportResult<usize> {
        let frame = target
            .seal
            .seal_frame(frame_type, flags, timestamp_samples, encoded_audio)?;
        wire_scratch.clear();
        wire_scratch.reserve(FRAME_HEADER_LEN + frame.ciphertext_and_tag.len());
        wire_scratch.extend_from_slice(&frame.header.to_bytes());
        wire_scratch.extend_from_slice(&frame.ciphertext_and_tag);
        let sent = socket.send_to(wire_scratch, target.addr)?;
        target.frames_sent += 1;
        target.bytes_sent += encoded_audio.len() as u64;
        Ok(sent)
    }

    /// Encrypt and send one already-encoded audio frame to a single registered `peer_id`.
    pub fn send_to_peer(
        &mut self,
        peer_id: &str,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        encoded_audio: &[u8],
    ) -> TransportResult<usize> {
        let Some(target) = self.targets.iter_mut().find(|t| t.peer_id == peer_id) else {
            return Err(TransportError::Closed);
        };
        Self::seal_and_send_target(
            &self.socket,
            &mut self.wire_scratch,
            target,
            frame_type,
            flags,
            timestamp_samples,
            encoded_audio,
        )
    }

    /// Single-encode multi-unicast fan-out: seals `encoded_audio` per peer with its own
    /// `ChaCha20-Poly1305` `AeadSealHalf` and sends each datagram over the DSCP EF UDP socket.
    pub fn fanout_encoded_audio(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        encoded_audio: &[u8],
    ) -> Vec<(String, TransportResult<usize>)> {
        let mut results = Vec::with_capacity(self.targets.len());
        for target in &mut self.targets {
            let res = Self::seal_and_send_target(
                &self.socket,
                &mut self.wire_scratch,
                target,
                frame_type,
                flags,
                timestamp_samples,
                encoded_audio,
            );
            results.push((target.peer_id.clone(), res));
        }
        results
    }
}

/// Encrypted SHT3-over-UDP receiver with 64-frame sliding-window replay protection.
pub struct Sht3UdpReceiver {
    socket: UdpSocket,
    open: AeadOpenHalf,
    replay: ReplayWindow,
    recv_buf: Vec<u8>,
}

impl Sht3UdpReceiver {
    /// Bind a DSCP EF (`0xB8`) UDP receiver socket on `addr`.
    pub fn bind(addr: SocketAddr, open: AeadOpenHalf) -> TransportResult<Self> {
        let socket = bind_dscp_ef_udp(addr)?;
        Ok(Self {
            socket,
            open,
            replay: ReplayWindow::new(),
            recv_buf: vec![0u8; MAX_UDP_DATAGRAM_LEN],
        })
    }

    pub fn local_addr(&self) -> TransportResult<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> TransportResult<()> {
        self.socket.set_read_timeout(dur)?;
        Ok(())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> TransportResult<()> {
        self.socket.set_nonblocking(nonblocking)?;
        Ok(())
    }

    /// Receive, authenticate, and decrypt one SHT3-over-UDP datagram.
    pub fn recv_frame(&mut self) -> TransportResult<(DecryptedFrame, SocketAddr)> {
        let (len, src) = self.socket.recv_from(&mut self.recv_buf)?;
        if len < FRAME_HEADER_LEN + AEAD_TAG_LEN {
            return Err(TransportError::Wire(format!(
                "truncated SHT3 UDP datagram: {len} bytes"
            )));
        }
        let mut hdr_bytes = [0u8; FRAME_HEADER_LEN];
        hdr_bytes.copy_from_slice(&self.recv_buf[..FRAME_HEADER_LEN]);
        let header = FrameHeader::from_bytes(&hdr_bytes)?;
        let expected_len = FRAME_HEADER_LEN + usize::from(header.payload_len) + AEAD_TAG_LEN;
        if len != expected_len {
            return Err(TransportError::Wire(format!(
                "SHT3 UDP datagram length {len} != expected {expected_len}"
            )));
        }
        let encrypted = EncryptedFrame {
            header,
            ciphertext_and_tag: self.recv_buf[FRAME_HEADER_LEN..len].to_vec(),
        };
        let payload = self
            .open
            .open_frame_unordered(&encrypted, &mut self.replay)?;
        Ok((DecryptedFrame { header, payload }, src))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::crypto::{
        generate_nonce, AeadSession, EphemeralKeypair, Role, SessionKeys,
    };

    fn derive_udp_pair() -> (AeadSealHalf, AeadOpenHalf) {
        let k_pair = [0xCD; 32];
        let c_eph = EphemeralKeypair::generate();
        let s_eph = EphemeralKeypair::generate();
        let c_pk = c_eph.public_bytes();
        let s_pk = s_eph.public_bytes();
        let c_nonce = generate_nonce();
        let s_nonce = generate_nonce();
        let transcript = b"udp-test-transcript";

        let c_shared = c_eph.diffie_hellman(&s_pk).unwrap();
        let s_shared = s_eph.diffie_hellman(&c_pk).unwrap();
        let c_keys =
            SessionKeys::derive(c_shared.as_bytes(), &k_pair, &c_nonce, &s_nonce, transcript)
                .unwrap()
                .derive_udp_keys()
                .unwrap();
        let s_keys =
            SessionKeys::derive(s_shared.as_bytes(), &k_pair, &c_nonce, &s_nonce, transcript)
                .unwrap()
                .derive_udp_keys()
                .unwrap();

        let (seal, _) = AeadSession::new(Role::Client, &c_keys).split();
        let (_, open) = AeadSession::new(Role::Server, &s_keys).split();
        (seal, open)
    }

    #[test]
    fn dscp_ef_udp_socket_binds_and_marks_tos() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let sender = Sht3UdpSender::bind(addr).unwrap();
        let bound = sender.local_addr().unwrap();
        assert_ne!(bound.port(), 0);

        #[cfg(target_os = "linux")]
        {
            let tos = SockRef::from(&sender.socket).tos().unwrap();
            assert_eq!(tos, u32::from(DSCP_EF));
        }
    }

    #[test]
    fn udp_encrypted_roundtrip_out_of_order_and_replay_protection() {
        let (mut seal, open) = derive_udp_pair();
        let mut receiver = Sht3UdpReceiver::bind("127.0.0.1:0".parse().unwrap(), open).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let raw_sock = bind_dscp_ef_udp("127.0.0.1:0".parse().unwrap()).unwrap();

        let f0 = seal
            .seal_frame(FrameType::AudioOpus, 0, 0, b"frame-0")
            .unwrap();
        let f1 = seal
            .seal_frame(FrameType::AudioOpus, 0, 240, b"frame-1")
            .unwrap();
        let f2 = seal
            .seal_frame(FrameType::AudioOpus, 0, 480, b"frame-2")
            .unwrap();

        let serialize = |f: &EncryptedFrame| -> Vec<u8> {
            let mut buf = Vec::new();
            buf.extend_from_slice(&f.header.to_bytes());
            buf.extend_from_slice(&f.ciphertext_and_tag);
            buf
        };

        // Deliver out of order: f0, f2, f1, then duplicate f1.
        raw_sock.send_to(&serialize(&f0), recv_addr).unwrap();
        raw_sock.send_to(&serialize(&f2), recv_addr).unwrap();
        raw_sock.send_to(&serialize(&f1), recv_addr).unwrap();
        raw_sock.send_to(&serialize(&f1), recv_addr).unwrap();

        // Also send a tampered f2 copy (invalid AEAD tag) — must be rejected without poisoning window.
        let mut tampered = serialize(&f2);
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        raw_sock.send_to(&tampered, recv_addr).unwrap();

        let (d0, _) = receiver.recv_frame().unwrap();
        assert_eq!(d0.payload, b"frame-0");
        assert_eq!(d0.header.seq, 0);

        let (d2, _) = receiver.recv_frame().unwrap();
        assert_eq!(d2.payload, b"frame-2");
        assert_eq!(d2.header.seq, 2);

        let (d1, _) = receiver.recv_frame().unwrap();
        assert_eq!(d1.payload, b"frame-1");
        assert_eq!(d1.header.seq, 1);

        let dup_err = receiver.recv_frame().unwrap_err();
        assert!(matches!(
            dup_err,
            TransportError::ReplayDetected { got: 1, .. }
        ));

        let tamper_err = receiver.recv_frame().unwrap_err();
        assert!(matches!(
            tamper_err,
            TransportError::ReplayDetected { got: 2, .. } | TransportError::Crypto(_)
        ));
    }

    #[test]
    fn udp_single_encode_multi_unicast_fanout() {
        let (seal_a, open_a) = derive_udp_pair();
        let (seal_b, open_b) = derive_udp_pair();

        let mut rx_a = Sht3UdpReceiver::bind("127.0.0.1:0".parse().unwrap(), open_a).unwrap();
        let mut rx_b = Sht3UdpReceiver::bind("127.0.0.1:0".parse().unwrap(), open_b).unwrap();
        rx_a.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        rx_b.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        let mut sender = Sht3UdpSender::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        sender.add_target("peer-a", rx_a.local_addr().unwrap(), seal_a);
        sender.add_target("peer-b", rx_b.local_addr().unwrap(), seal_b);
        assert_eq!(sender.target_count(), 2);

        let encoded_once = b"single-encoded-5ms-opus-packet";
        let results = sender.fanout_encoded_audio(FrameType::AudioOpus, 0, 240, encoded_once);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, r)| r.is_ok()));

        let (da, _) = rx_a.recv_frame().unwrap();
        let (db, _) = rx_b.recv_frame().unwrap();
        assert_eq!(da.payload, encoded_once);
        assert_eq!(db.payload, encoded_once);
        assert_eq!(da.header.timestamp_samples, 240);
        assert_eq!(db.header.timestamp_samples, 240);
    }
}
