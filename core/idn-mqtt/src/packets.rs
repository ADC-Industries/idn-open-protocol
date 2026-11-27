//! MQTT packets on a connection: the first read (its version), read and written by version, packet ids, a disconnect.

use super::*;

pub(super) fn take_pkid(next: &mut u16, inflight: &BTreeMap<u16, u64>) -> u16 {
    loop {
        let id = *next;
        *next = next.wrapping_add(1).max(1);
        if !inflight.contains_key(&id) {
            return id;
        }
    }
}

pub(super) fn disconnect(code: DisconnectReasonCode) -> Packet {
    Packet::Disconnect(Disconnect {
        reason_code: code,
        properties: None,
    })
}

/// The first packet, and whether its client speaks 3.1.1 ([`v311`]): told by the
/// CONNECT's protocol level, in its first 12 bytes (a CONNECT has at least 14).
pub(super) async fn read_first(
    rd: &mut (impl AsyncRead + Unpin),
    buf: &mut BytesMut,
) -> io::Result<(Packet, bool)> {
    while buf.len() < 12 {
        buf.reserve(4096);
        if rd.read_buf(buf).await? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
        }
    }
    let v4 = v311::is_v311_connect(buf);
    // (Before any login: a CONNECT of at most 64 KB, not the 16 MB a logged-in client may
    // send; bug review 3.)
    Ok((read_packet_max(rd, buf, v4, MAX_CONNECT).await?, v4))
}

/// The largest CONNECT taken (its will and login included).
const MAX_CONNECT: u32 = 64 << 10;

/// A packet, read as MQTT 5 or (`v4`) as 3.1.1 into what it means in MQTT 5.
pub(super) async fn read_packet(
    rd: &mut (impl AsyncRead + Unpin),
    buf: &mut BytesMut,
    v4: bool,
) -> io::Result<Packet> {
    read_packet_max(rd, buf, v4, MAX_PACKET).await
}

/// [`read_packet`], at most `max` bytes.
async fn read_packet_max(
    rd: &mut (impl AsyncRead + Unpin),
    buf: &mut BytesMut,
    v4: bool,
    max: u32,
) -> io::Result<Packet> {
    loop {
        if v4 {
            if let Some(p) = v311::read(buf, max as usize)? {
                return Ok(p);
            }
        } else {
            match Packet::read(buf, Some(max)) {
                Ok(p) => return Ok(p),
                Err(mb::Error::InsufficientBytes(_)) => {}
                Err(e) => return Err(err(e)),
            }
        }
        buf.reserve(4096);
        if rd.read_buf(buf).await? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
        }
    }
}

pub(super) async fn write_packet(
    wr: &mut (impl AsyncWrite + Unpin),
    v4: bool,
    p: &Packet,
) -> io::Result<()> {
    let mut b = BytesMut::new();
    if v4 {
        if !v311::write(p, &mut b, MAX_PACKET as usize)? {
            return Ok(());
        }
    } else {
        p.write(&mut b, Some(MAX_PACKET)).map_err(err)?;
    }
    tokio::time::timeout(WRITE_TIMEOUT, async {
        wr.write_all(&b).await?;
        wr.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the client isn't reading"))?
}
