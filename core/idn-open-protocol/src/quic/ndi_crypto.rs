//! The NDI QUIC profile's "crypto" (spec chapter 4) as a quinn crypto provider: the keyless
//! stub handshake (§4.3) instead of TLS 1.3, no header protection (§4.2.1), Initial and
//! Handshake packets carrying a 16-byte trailer of `K` and zeros where an AEAD tag would
//! be (§4.2.2), and 1-RTT packets with no protection at all (§4.2.3, transport parameter
//! `0xBAAD`). The connection is neither confidential nor authenticated, as in NDI.
//!
//! The 1-RTT keys report no confidentiality limit, which is what the vendored quinn-proto
//! patch keys on to skip its routine key update (NDI peers never update keys).

use super::stub::{
    self, ALPN, EXT_ALPN, EXT_TRANSPORT_PARAMS, MSG_CLIENT_HANDSHAKE, MSG_CLIENT_HELLO,
    MSG_SERVER_HANDSHAKE, MSG_SERVER_INITIAL, TP_DISABLE_1RTT_PROTECTION,
};
use super::wire::{initial_secret, read_varint, trailer_key, write_varint};
use bytes::BytesMut;
use quinn_proto::crypto::{
    self, CryptoError, ExportKeyingMaterialError, HeaderKey, KeyPair, Keys, PacketKey,
    UnsupportedVersion,
};
use quinn_proto::transport_parameters::TransportParameters;
use quinn_proto::{ConnectError, ConnectionId, Side, TransportError, TransportErrorCode};
use std::any::Any;
use std::sync::Arc;

/// QUIC version 1, the only one in the profile.
const QUIC_V1: u32 = 1;

fn err(code: TransportErrorCode, reason: &str) -> TransportError {
    TransportError {
        code,
        frame: None,
        reason: reason.into(),
    }
}

fn protocol_violation(reason: &str) -> TransportError {
    err(TransportErrorCode::PROTOCOL_VIOLATION, reason)
}

/// No header protection (§4.2.1).
struct NoHeaderProtection;

impl HeaderKey for NoHeaderProtection {
    fn decrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}
    fn encrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}
    fn sample_size(&self) -> usize {
        0
    }
}

/// Initial and Handshake packets: plaintext, then `K` and 8 zero bytes (§4.2.2). A
/// packet whose trailer does not start with `K` is discarded.
struct Trailer([u8; 8]);

impl PacketKey for Trailer {
    fn encrypt(&self, _packet: u64, buf: &mut [u8], _header_len: usize) {
        let n = buf.len();
        buf[n - 16..n - 8].copy_from_slice(&self.0);
        buf[n - 8..].fill(0);
    }

    fn decrypt(
        &self,
        _packet: u64,
        _header: &[u8],
        payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        let n = payload.len();
        if n < 16 || payload[n - 16..n - 8] != self.0 {
            return Err(CryptoError);
        }
        payload.truncate(n - 16);
        Ok(())
    }

    fn tag_len(&self) -> usize {
        16
    }

    fn confidentiality_limit(&self) -> u64 {
        u64::MAX
    }

    fn integrity_limit(&self) -> u64 {
        u64::MAX
    }
}

/// 1-RTT packets: no protection and no tag (§4.2.3). Unlimited, so never key-updated.
struct Plain;

impl PacketKey for Plain {
    fn encrypt(&self, _packet: u64, _buf: &mut [u8], _header_len: usize) {}

    fn decrypt(
        &self,
        _packet: u64,
        _header: &[u8],
        _payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        Ok(())
    }

    fn tag_len(&self) -> usize {
        0
    }

    fn confidentiality_limit(&self) -> u64 {
        u64::MAX
    }

    fn integrity_limit(&self) -> u64 {
        u64::MAX
    }
}

fn trailer_keys(k: [u8; 8]) -> Keys {
    Keys {
        header: KeyPair {
            local: Box::new(NoHeaderProtection),
            remote: Box::new(NoHeaderProtection),
        },
        packet: KeyPair {
            local: Box::new(Trailer(k)),
            remote: Box::new(Trailer(k)),
        },
    }
}

fn plain_keys() -> Keys {
    Keys {
        header: KeyPair {
            local: Box::new(NoHeaderProtection),
            remote: Box::new(NoHeaderProtection),
        },
        packet: KeyPair {
            local: Box::new(Plain),
            remote: Box::new(Plain),
        },
    }
}

fn initial_keys(dst_cid: &ConnectionId) -> Keys {
    trailer_keys(trailer_key(&initial_secret(dst_cid)))
}

/// Our transport parameters as quinn encodes them, plus `0xBAAD` (§4.4).
fn encode_params(params: &TransportParameters) -> Vec<u8> {
    let mut v = Vec::new();
    params.write(&mut v);
    write_varint(&mut v, TP_DISABLE_1RTT_PROTECTION);
    write_varint(&mut v, 0);
    v
}

/// Whether encoded transport parameters include `0xBAAD`.
fn has_baad(mut b: &[u8]) -> bool {
    while !b.is_empty() {
        let (Ok(id), Ok(len)) = (read_varint(&mut b), read_varint(&mut b)) else {
            return false;
        };
        if id == TP_DISABLE_1RTT_PROTECTION {
            return true;
        }
        let Some(rest) = b.get(len as usize..) else {
            return false;
        };
        b = rest;
    }
    false
}

/// The peer's transport parameters: quinn's decoding (unknown ones skipped), after
/// checking `0xBAAD` is there (§4.2.3: without it 1-RTT behaviour is unspecified).
fn decode_params(our_side: Side, b: &[u8]) -> Result<TransportParameters, TransportError> {
    if !has_baad(b) {
        return Err(err(
            TransportErrorCode::TRANSPORT_PARAMETER_ERROR,
            "transport parameter 0xBAAD missing",
        ));
    }
    TransportParameters::read(our_side, &mut &b[..]).map_err(TransportError::from)
}

/// Extensions `u16 type, u16 len, data`: (ALPN list, transport parameters).
/// Offered or selected protocols, and the encoded transport parameters if present.
type Exts<'a> = (Vec<Vec<u8>>, Option<&'a [u8]>);

fn parse_exts(mut exts: &[u8]) -> Result<Exts<'_>, TransportError> {
    let bad = || protocol_violation("bad stub handshake extensions");
    let (mut alpn, mut tps) = (Vec::new(), None);
    while !exts.is_empty() {
        if exts.len() < 4 {
            return Err(bad());
        }
        let t = u16::from_be_bytes([exts[0], exts[1]]);
        let l = u16::from_be_bytes([exts[2], exts[3]]) as usize;
        let d = exts.get(4..4 + l).ok_or_else(bad)?;
        exts = &exts[4 + l..];
        match t {
            EXT_ALPN if d.len() >= 2 => {
                let mut l = &d[2..];
                while let Some((&n, rest)) = l.split_first() {
                    alpn.push(rest.get(..n as usize).ok_or_else(bad)?.to_vec());
                    l = &rest[n as usize..];
                }
            }
            EXT_TRANSPORT_PARAMS => tps = Some(d),
            _ => {}
        }
    }
    Ok((alpn, tps))
}

fn random32() -> [u8; 32] {
    crate::random::secure_random::<32>()
}

/// Handshake messages arrive as a byte stream: whole messages are taken off the front.
#[derive(Default)]
struct Inbox(Vec<u8>);

/// Largest stub handshake message accepted. Real ones are a few hundred bytes; the 24-bit
/// length would otherwise let an unauthenticated peer make us buffer 16 MB.
const MAX_HANDSHAKE_MESSAGE: usize = 64 << 10;

impl Inbox {
    /// Appends handshake bytes; fails if the message being received is too large.
    fn push(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.0.extend_from_slice(buf);
        if let [_, a, b, c, ..] = self.0[..] {
            let len = (a as usize) << 16 | (b as usize) << 8 | c as usize;
            if len > MAX_HANDSHAKE_MESSAGE {
                return Err(protocol_violation("stub handshake message too large"));
            }
        }
        Ok(())
    }

    fn next(&mut self) -> Option<(u8, Vec<u8>)> {
        let (t, body, used) = stub::take_message(&self.0)?;
        let m = (t, body.to_vec());
        self.0.drain(..used);
        Some(m)
    }
}

// ---------------------------------------------------------------------------------------
// Client (the NDI sender).

/// Client-side configuration: the stub handshake, offering only `NDI_SDK_v5`.
pub(crate) struct ClientConfig;

impl crypto::ClientConfig for ClientConfig {
    fn start_session(
        self: Arc<Self>,
        version: u32,
        server_name: &str,
        params: &TransportParameters,
    ) -> Result<Box<dyn crypto::Session>, ConnectError> {
        if version != QUIC_V1 {
            return Err(UnsupportedVersion.into());
        }
        Ok(Box::new(ClientSession {
            hello: Some(stub::client_hello(server_name, &encode_params(params))),
            inbox: Inbox::default(),
            hs_secret: None,
            hs_keys_given: false,
            peer_params: None,
            reply_due: false,
            done: false,
        }))
    }
}

struct ClientSession {
    /// The client-hello, until written.
    hello: Option<Vec<u8>>,
    inbox: Inbox,
    hs_secret: Option<[u8; 32]>,
    hs_keys_given: bool,
    peer_params: Option<TransportParameters>,
    /// The server-handshake was read: the client-handshake and the 1-RTT keys are due.
    reply_due: bool,
    done: bool,
}

impl crypto::Session for ClientSession {
    fn initial_keys(&self, dst_cid: &ConnectionId, _side: Side) -> Keys {
        initial_keys(dst_cid)
    }

    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        self.peer_params
            .is_some()
            .then(|| Box::new(ALPN.to_vec()) as Box<dyn Any>)
    }

    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        None
    }

    fn early_crypto(&self) -> Option<(Box<dyn HeaderKey>, Box<dyn PacketKey>)> {
        None
    }

    fn early_data_accepted(&self) -> Option<bool> {
        None
    }

    fn is_handshaking(&self) -> bool {
        // Done once the server-handshake is in: the reply follows in the same round.
        !(self.reply_due || self.done)
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        self.inbox.push(buf)?;
        let mut ready = false;
        while let Some((t, body)) = self.inbox.next() {
            match t {
                MSG_SERVER_INITIAL if self.hs_secret.is_none() => {
                    let s = body
                        .get(1..33)
                        .ok_or_else(|| protocol_violation("short server-initial"))?;
                    self.hs_secret = Some(s.try_into().unwrap());
                }
                MSG_SERVER_HANDSHAKE if self.peer_params.is_none() => {
                    if body.len() < 36 {
                        return Err(protocol_violation("short server-handshake"));
                    }
                    let c = u16::from_le_bytes([body[32], body[33]]) as usize;
                    let e = u16::from_le_bytes([body[34], body[35]]) as usize;
                    let exts = body
                        .get(36 + c..36 + c + e)
                        .ok_or_else(|| protocol_violation("bad server-handshake"))?;
                    let (alpn, tps) = parse_exts(exts)?;
                    // A protocol we did not offer: close with 0x100 (§4.3.1).
                    if alpn.first().map(Vec::as_slice) != Some(ALPN) {
                        return Err(err(TransportErrorCode::crypto(0), "unexpected ALPN"));
                    }
                    let tps = tps.ok_or_else(|| {
                        err(
                            TransportErrorCode::crypto(0x6d),
                            "transport parameters missing",
                        )
                    })?;
                    self.peer_params = Some(decode_params(Side::Client, tps)?);
                    self.reply_due = true;
                    ready = true;
                }
                _ => return Err(protocol_violation("unexpected stub handshake message")),
            }
        }
        Ok(ready)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        Ok(self.peer_params)
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys> {
        if let Some(h) = self.hello.take() {
            buf.extend_from_slice(&h);
            return None;
        }
        if let (Some(s), false) = (self.hs_secret, self.hs_keys_given) {
            self.hs_keys_given = true;
            return Some(trailer_keys(trailer_key(&s)));
        }
        if self.reply_due {
            self.reply_due = false;
            self.done = true;
            buf.extend_from_slice(&stub::client_handshake());
            return Some(plain_keys());
        }
        None
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Box<dyn PacketKey>>> {
        Some(KeyPair {
            local: Box::new(Plain),
            remote: Box::new(Plain),
        })
    }

    fn is_valid_retry(&self, _orig: &ConnectionId, _header: &[u8], _payload: &[u8]) -> bool {
        false // never used by NDI
    }

    fn export_keying_material(
        &self,
        _output: &mut [u8],
        _label: &[u8],
        _context: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        Err(ExportKeyingMaterialError)
    }
}

// ---------------------------------------------------------------------------------------
// Server (the NDI receiver).

/// Server-side configuration: the stub handshake, selecting `NDI_SDK_v5`.
pub(crate) struct ServerConfig;

impl crypto::ServerConfig for ServerConfig {
    fn initial_keys(
        &self,
        version: u32,
        dst_cid: &ConnectionId,
    ) -> Result<Keys, UnsupportedVersion> {
        if version != QUIC_V1 {
            return Err(UnsupportedVersion);
        }
        Ok(initial_keys(dst_cid))
    }

    fn retry_tag(&self, _version: u32, _orig: &ConnectionId, _packet: &[u8]) -> [u8; 16] {
        [0; 16] // retry is not used
    }

    fn start_session(
        self: Arc<Self>,
        _version: u32,
        params: &TransportParameters,
    ) -> Box<dyn crypto::Session> {
        Box::new(ServerSession {
            own_params: encode_params(params),
            inbox: Inbox::default(),
            peer_params: None,
            hs_secret: random32(),
            sent: 0,
            done: false,
        })
    }
}

struct ServerSession {
    own_params: Vec<u8>,
    inbox: Inbox,
    peer_params: Option<TransportParameters>,
    hs_secret: [u8; 32],
    /// 0: nothing sent; 1: server-initial; 2: server-handshake.
    sent: u8,
    done: bool,
}

impl crypto::Session for ServerSession {
    fn initial_keys(&self, dst_cid: &ConnectionId, _side: Side) -> Keys {
        initial_keys(dst_cid)
    }

    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        self.peer_params
            .is_some()
            .then(|| Box::new(ALPN.to_vec()) as Box<dyn Any>)
    }

    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        None
    }

    fn early_crypto(&self) -> Option<(Box<dyn HeaderKey>, Box<dyn PacketKey>)> {
        None
    }

    fn early_data_accepted(&self) -> Option<bool> {
        None
    }

    fn is_handshaking(&self) -> bool {
        !self.done
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        self.inbox.push(buf)?;
        let mut ready = false;
        while let Some((t, body)) = self.inbox.next() {
            match t {
                MSG_CLIENT_HELLO if self.peer_params.is_none() => {
                    let h = stub::parse_client_hello(&body)
                        .map_err(|_| protocol_violation("bad client-hello"))?;
                    // NDI_SDK_v5 must be offered, others are ignored; refuse with TLS
                    // alert 120 (0x178) otherwise, as NDI 6.3 does (§4.3.1).
                    if !h.alpn.iter().any(|a| a.as_slice() == ALPN) {
                        return Err(err(
                            TransportErrorCode::crypto(120),
                            "no application protocol",
                        ));
                    }
                    let tps = h.tps.ok_or_else(|| {
                        err(
                            TransportErrorCode::crypto(0x6d),
                            "transport parameters missing",
                        )
                    })?;
                    self.peer_params = Some(decode_params(Side::Server, &tps)?);
                    ready = true;
                }
                MSG_CLIENT_HANDSHAKE if self.sent == 2 => {
                    if body.first() != Some(&1) {
                        return Err(err(
                            TransportErrorCode::crypto(40),
                            "client-handshake failed",
                        ));
                    }
                    self.done = true;
                }
                _ => return Err(protocol_violation("unexpected stub handshake message")),
            }
        }
        Ok(ready)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        Ok(self.peer_params)
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys> {
        self.peer_params?; // nothing to answer before the client-hello
        match self.sent {
            // In the Initial space, opening the Handshake space with K(secret).
            0 => {
                self.sent = 1;
                buf.extend_from_slice(&stub::server_initial(&self.hs_secret));
                Some(trailer_keys(trailer_key(&self.hs_secret)))
            }
            // In the Handshake space, opening 1-RTT.
            1 => {
                self.sent = 2;
                // The 1-RTT secret is unused without 1-RTT protection.
                buf.extend_from_slice(&stub::server_handshake(&random32(), &self.own_params));
                Some(plain_keys())
            }
            _ => None,
        }
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Box<dyn PacketKey>>> {
        Some(KeyPair {
            local: Box::new(Plain),
            remote: Box::new(Plain),
        })
    }

    fn is_valid_retry(&self, _orig: &ConnectionId, _header: &[u8], _payload: &[u8]) -> bool {
        false
    }

    fn export_keying_material(
        &self,
        _output: &mut [u8],
        _label: &[u8],
        _context: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        Err(ExportKeyingMaterialError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_matches_the_profile_vectors() {
        // §4.5 vector 1: Initial K for DCID c7c4f1cae8a56a27.
        let dcid = ConnectionId::new(&idn_common::hex::unhex("c7c4f1cae8a56a27").unwrap());
        let k = trailer_key(&initial_secret(&dcid));
        assert_eq!(idn_common::hex::hex(&k), "3aedaf0e60e5312a");
        let key = Trailer(k);
        let mut pkt = vec![0xAA; 30];
        key.encrypt(0, &mut pkt, 5);
        assert_eq!(&pkt[14..22], &k);
        assert_eq!(&pkt[22..], &[0; 8]);
        let mut payload = BytesMut::from(&pkt[5..]);
        key.decrypt(0, &pkt[..5], &mut payload).unwrap();
        assert_eq!(payload.len(), 30 - 5 - 16);
        // A wrong K is discarded.
        let mut bad = BytesMut::from(&pkt[5..]);
        assert!(Trailer([0; 8]).decrypt(0, &pkt[..5], &mut bad).is_err());
    }

    #[test]
    fn baad_is_added_and_required() {
        // Defaults, as decoded from an empty list (the constructor is private to quinn).
        let defaults = TransportParameters::read(Side::Server, &mut &[][..]).unwrap();
        let v = encode_params(&defaults);
        assert!(has_baad(&v));
        assert!(decode_params(Side::Server, &v).is_ok());
        let mut without = Vec::new();
        defaults.write(&mut without);
        assert!(!has_baad(&without));
        assert!(decode_params(Side::Server, &without).is_err());
    }

    #[test]
    fn oversized_handshake_messages_are_refused() {
        let mut inbox = Inbox::default();
        assert!(inbox.push(&[1, 0, 1, 0]).is_ok()); // 256 bytes: fine
        let mut inbox = Inbox::default();
        assert!(inbox.push(&[1, 0xff, 0xff, 0xff]).is_err()); // 16 MB claimed
    }
}
