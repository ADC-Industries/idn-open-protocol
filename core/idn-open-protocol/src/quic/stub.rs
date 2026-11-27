//! The keyless stub handshake that replaces TLS (spec §4.3): its four messages. The
//! transport parameters (§4.4) inside them are quinn's, plus `0xBAAD`.

use super::wire::WireError;

pub const ALPN: &[u8] = b"NDI_SDK_v5";
/// Private transport parameter: disable 1-RTT packet protection (§4.2.3).
pub const TP_DISABLE_1RTT_PROTECTION: u64 = 0xBAAD;

pub const MSG_CLIENT_HELLO: u8 = 1;
pub const MSG_CLIENT_HANDSHAKE: u8 = 2;
pub const MSG_SERVER_INITIAL: u8 = 3;
pub const MSG_SERVER_HANDSHAKE: u8 = 4;

pub(crate) const EXT_SERVER_NAME: u16 = 0x0000;
pub(crate) const EXT_ALPN: u16 = 0x0010;
pub(crate) const EXT_TRANSPORT_PARAMS: u16 = 0x0039;

pub(crate) fn msg(t: u8, body: &[u8], padded: bool) -> Vec<u8> {
    let len = body.len() + if padded { 3 } else { 0 };
    let mut out = vec![t, (len >> 16) as u8, (len >> 8) as u8, len as u8];
    out.extend_from_slice(body);
    if padded {
        out.extend_from_slice(&[0; 3]);
    }
    out
}

pub(crate) fn ext(out: &mut Vec<u8>, t: u16, data: &[u8]) {
    out.extend_from_slice(&t.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

pub(crate) fn alpn_ext() -> Vec<u8> {
    let mut d = ((ALPN.len() + 1) as u16).to_be_bytes().to_vec();
    d.push(ALPN.len() as u8);
    d.extend_from_slice(ALPN);
    d
}

/// Server-initial (type 3, §4.3.2).
pub fn server_initial(hs_secret: &[u8; 32]) -> Vec<u8> {
    let mut body = vec![0u8];
    body.extend_from_slice(hs_secret);
    msg(MSG_SERVER_INITIAL, &body, true)
}

/// Server-handshake (type 4, §4.3.3), with encoded transport parameters `tps`.
pub fn server_handshake(one_rtt_secret: &[u8; 32], tps: &[u8]) -> Vec<u8> {
    let mut exts = Vec::new();
    ext(&mut exts, EXT_ALPN, &alpn_ext());
    ext(&mut exts, EXT_TRANSPORT_PARAMS, tps);
    let cert = [0u8; 3];
    let mut body = one_rtt_secret.to_vec();
    body.extend_from_slice(&(cert.len() as u16).to_le_bytes());
    body.extend_from_slice(&(exts.len() as u16).to_le_bytes());
    body.extend_from_slice(&cert);
    body.extend_from_slice(&exts);
    msg(MSG_SERVER_HANDSHAKE, &body, true)
}

/// Client-hello (type 1, §4.3.1), with encoded transport parameters `tps`.
pub fn client_hello(server_name: &str, tps: &[u8]) -> Vec<u8> {
    let mut body = vec![0x03, 0x02];
    body.extend_from_slice(&[0; 32]);
    body.push(0); // session id
    body.extend_from_slice(&[0, 0]); // cipher suites
    body.extend_from_slice(&[1, 0]); // compression methods
    let mut exts = Vec::new();
    let mut sni = ((server_name.len() + 3) as u16).to_be_bytes().to_vec();
    sni.push(0);
    sni.extend_from_slice(&(server_name.len() as u16).to_be_bytes());
    sni.extend_from_slice(server_name.as_bytes());
    ext(&mut exts, EXT_SERVER_NAME, &sni);
    ext(&mut exts, EXT_ALPN, &alpn_ext());
    ext(&mut exts, EXT_TRANSPORT_PARAMS, tps);
    body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
    body.extend_from_slice(&exts);
    msg(MSG_CLIENT_HELLO, &body, false)
}

/// Client-handshake (type 2, §4.3.4). The 3 padding bytes may hold any value; NDI 6.3
/// clients send `02 00 00`, which this reproduces for byte-identical traffic.
pub fn client_handshake() -> Vec<u8> {
    let mut m = msg(MSG_CLIENT_HANDSHAKE, &[1], true);
    m[5] = 0x02;
    m
}

/// Splits one complete message off the front of a CRYPTO stream: (type, body). For types
/// 2–4 the body includes the 3 trailing padding bytes, which callers ignore.
pub fn take_message(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = (buf[1] as usize) << 16 | (buf[2] as usize) << 8 | buf[3] as usize;
    let body = buf.get(4..4 + len)?;
    Some((buf[0], body, 4 + len))
}

/// What a server needs from a client-hello; the transport parameters are left encoded
/// (quinn decodes them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub server_name: Option<String>,
    pub alpn: Vec<Vec<u8>>,
    pub tps: Option<Vec<u8>>,
}

pub fn parse_client_hello(body: &[u8]) -> Result<ClientHello, WireError> {
    let bad = WireError::BadPacket;
    let mut p = body;
    let mut take = |n: usize| -> Result<&[u8], WireError> {
        if p.len() < n {
            return Err(WireError::Truncated);
        }
        let (h, t) = p.split_at(n);
        p = t;
        Ok(h)
    };
    take(2 + 32)?;
    let sid = take(1)?[0] as usize;
    take(sid)?;
    let cs = u16::from_be_bytes(take(2)?.try_into().unwrap()) as usize;
    take(cs)?;
    let cm = take(1)?[0] as usize;
    take(cm)?;
    let el = u16::from_be_bytes(take(2)?.try_into().unwrap()) as usize;
    let mut exts = take(el)?;
    let mut hello = ClientHello {
        server_name: None,
        alpn: Vec::new(),
        tps: None,
    };
    while !exts.is_empty() {
        if exts.len() < 4 {
            return Err(bad);
        }
        let t = u16::from_be_bytes([exts[0], exts[1]]);
        let l = u16::from_be_bytes([exts[2], exts[3]]) as usize;
        let d = exts.get(4..4 + l).ok_or(bad)?;
        exts = &exts[4 + l..];
        match t {
            EXT_SERVER_NAME if d.len() >= 5 => {
                let nl = u16::from_be_bytes([d[3], d[4]]) as usize;
                hello.server_name = d
                    .get(5..5 + nl)
                    .map(|n| String::from_utf8_lossy(n).into_owned());
            }
            EXT_ALPN if d.len() >= 2 => {
                let mut l = &d[2..];
                while let Some((&n, rest)) = l.split_first() {
                    let n = n as usize;
                    hello.alpn.push(rest.get(..n).ok_or(bad)?.to_vec());
                    l = &rest[n..];
                }
            }
            EXT_TRANSPORT_PARAMS => hello.tps = Some(d.to_vec()),
            _ => {}
        }
    }
    Ok(hello)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_roundtrip() {
        let tps = [0x01, 0x02, 0x47, 0xd0];
        let m = client_hello("172.31.250.2", &tps);
        let (t, body, used) = take_message(&m).unwrap();
        assert_eq!((t, used), (MSG_CLIENT_HELLO, m.len()));
        let h = parse_client_hello(body).unwrap();
        assert_eq!(h.server_name.as_deref(), Some("172.31.250.2"));
        assert_eq!(h.alpn, vec![ALPN.to_vec()]);
        assert_eq!(h.tps.as_deref(), Some(&tps[..]));
    }

    #[test]
    fn client_handshake_matches_vector_4() {
        // §4.5 vector 4: CRYPTO 06 00 08, then `02 000004 01 020000`.
        assert_eq!(
            idn_common::hex::hex(&client_handshake()),
            "0200000401020000"
        );
    }

    #[test]
    fn server_initial_matches_vector_2() {
        // §4.5 vector 2 carries `03 000024 00 <secret> 000000`.
        let secret: [u8; 32] = idn_common::hex::unhex(
            "d394aed40946b9db7288e80eca44bc6eaf8da5ac6b46f3b70dbdb597a16c2be6",
        )
        .unwrap()
        .try_into()
        .unwrap();
        let m = server_initial(&secret);
        let want =
            "0300002400d394aed40946b9db7288e80eca44bc6eaf8da5ac6b46f3b70dbdb597a16c2be6000000";
        assert_eq!(idn_common::hex::hex(&m), want);
    }

    #[test]
    fn server_handshake_layout() {
        let m = server_handshake(&[7; 32], &[0x01, 0x02, 0x47, 0xd0]);
        let (t, body, _) = take_message(&m).unwrap();
        assert_eq!(t, MSG_SERVER_HANDSHAKE);
        assert_eq!(u16::from_le_bytes([body[32], body[33]]), 3);
        let e = u16::from_le_bytes([body[34], body[35]]) as usize;
        assert_eq!(body.len(), 36 + 3 + e + 3);
    }
}
