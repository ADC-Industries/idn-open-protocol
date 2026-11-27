//! What crosses the link (§I.6): each item on a stream of its own, as `channel/1`'s
//! messages (§E.12.4) with an id and the sender's time, and an answer on the same stream
//! once the receiver has stored it (or, for a request, answered it).
//!
//! Item: `op` (u8), id (16 bytes), time (i64 LE, ms since 1970 at the sender), type
//! length (u16 LE) and type (UTF-8, at most 128 bytes), payload length (u32 LE, at most
//! 16 MiB) and payload. Answer: status (u8), length (u32 LE), payload.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A message: stored, then passed on (§I.6.1).
pub const OP_MESSAGE: u8 = 1;
/// A request: the answer is the application's (§I.6.1).
pub const OP_REQUEST: u8 = 2;
/// A batch of values, a JSON array (§I.6.2).
pub const OP_VALUES: u8 = 3;
/// A file (stage 1: whole, up to the payload limit).
pub const OP_FILE: u8 = 4;
/// Keeps a WebSocket's path open (Cloudflare closes one idle for about 100 s).
pub const OP_PING: u8 = 9;
/// The Node's hello after connecting (JSON: name, edition, version).
pub const OP_HELLO: u8 = 10;
/// Joining with a token (§I.5).
pub const OP_JOIN: u8 = 11;
/// Settings from the Site Manager (JSON).
pub const OP_CONFIG: u8 = 12;
/// A remote-support screen stream the Node opens (§I.11.3): the first item carries the
/// session id as its payload, then each later item is one encoded picture; the stream
/// stays open until the session ends. Not stored.
pub const OP_SCREEN: u8 = 13;

/// Stored (or answered) as asked.
pub const ST_OK: u8 = 0;
/// Failed; the payload says why (UTF-8).
pub const ST_ERROR: u8 = 1;
/// No one here handles this type.
pub const ST_UNKNOWN: u8 = 2;
/// Refused.
pub const ST_REFUSED: u8 = 3;
/// Seen before (by id): already stored, nothing done again.
pub const ST_DUPLICATE: u8 = 4;
/// No answer in time (a request to an application that is away).
pub const ST_TIMEOUT: u8 = 5;

pub const MAX_TYPE: usize = 128;
pub const MAX_PAYLOAD: usize = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub op: u8,
    pub id: [u8; 16],
    /// Milliseconds since 1970 when it was made, at its sender.
    pub time_ms: i64,
    pub kind: String,
    pub payload: Vec<u8>,
}

impl Item {
    /// A new item with a fresh id, made now.
    pub fn new(op: u8, kind: &str, payload: Vec<u8>) -> Item {
        Item {
            op,
            id: idn_open_protocol::random::secure_random(),
            time_ms: now_ms(),
            kind: kind.to_owned(),
            payload,
        }
    }

    pub fn id_hex(&self) -> String {
        idn_common::hex::hex(&self.id)
    }
}

/// Milliseconds since 1970, as items and records carry them.
pub fn now_ms() -> i64 {
    idn_common::epoch::millis() as i64
}

use idn_common::err::invalid_data as bad;

/// A type only the Site Manager itself sends (§I.7: `idn.` is the protocol's, `remote.`
/// the remote support's, §I.11): refused from applications (the HTTP API's commands and
/// MQTT `cmd/`), which would otherwise update a machine, type on it or show its screen
/// without the role, the banner or the audit those need.
pub fn reserved_kind(k: &str) -> bool {
    k.starts_with("idn.") || k.starts_with("remote.")
}

/// Valid type: lower-case letters, digits, `.`, `-`, `_`; not empty; at most 128 bytes.
pub fn valid_kind(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= MAX_TYPE
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

pub async fn write_item(w: &mut (impl AsyncWrite + Unpin), it: &Item) -> io::Result<()> {
    if it.kind.len() > MAX_TYPE || it.payload.len() > MAX_PAYLOAD {
        return Err(bad("item too large"));
    }
    let mut b = Vec::with_capacity(32 + it.kind.len() + it.payload.len());
    b.push(it.op);
    b.extend_from_slice(&it.id);
    b.extend_from_slice(&it.time_ms.to_le_bytes());
    b.extend_from_slice(&(it.kind.len() as u16).to_le_bytes());
    b.extend_from_slice(it.kind.as_bytes());
    b.extend_from_slice(&(it.payload.len() as u32).to_le_bytes());
    b.extend_from_slice(&it.payload);
    w.write_all(&b).await?;
    w.flush().await
}

pub async fn read_item(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Item> {
    let op = r.read_u8().await?;
    let mut id = [0u8; 16];
    r.read_exact(&mut id).await?;
    let time_ms = r.read_i64_le().await?;
    let tl = r.read_u16_le().await? as usize;
    if tl > MAX_TYPE {
        return Err(bad("type too long"));
    }
    let mut t = vec![0u8; tl];
    r.read_exact(&mut t).await?;
    let kind = String::from_utf8(t).map_err(|_| bad("type isn't text"))?;
    let pl = r.read_u32_le().await? as usize;
    if pl > MAX_PAYLOAD {
        return Err(bad("payload too large"));
    }
    // Grown as it comes, not made at the size claimed (a join link, before any token,
    // reserved 16 MB by saying so; bug run 4).
    let mut payload = Vec::with_capacity(pl.min(64 << 10));
    (&mut *r).take(pl as u64).read_to_end(&mut payload).await?;
    if payload.len() < pl {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the item was cut short",
        ));
    }
    Ok(Item {
        op,
        id,
        time_ms,
        kind,
        payload,
    })
}

pub async fn write_answer(
    w: &mut (impl AsyncWrite + Unpin),
    status: u8,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(bad("answer too large"));
    }
    let mut b = Vec::with_capacity(5 + payload.len());
    b.push(status);
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload);
    w.write_all(&b).await?;
    w.flush().await?;
    w.shutdown().await
}

pub async fn read_answer(r: &mut (impl AsyncRead + Unpin)) -> io::Result<(u8, Vec<u8>)> {
    let status = r.read_u8().await?;
    let n = r.read_u32_le().await? as usize;
    if n > MAX_PAYLOAD {
        return Err(bad("answer too large"));
    }
    let mut p = vec![0u8; n];
    r.read_exact(&mut p).await?;
    Ok((status, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn items_and_answers_cross_a_stream() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let it = Item::new(OP_MESSAGE, "batch.ticket", b"{\"n\":1}".to_vec());
        write_item(&mut a, &it).await.unwrap();
        assert_eq!(read_item(&mut b).await.unwrap(), it);
        write_answer(&mut b, ST_OK, b"").await.unwrap();
        assert_eq!(read_answer(&mut a).await.unwrap(), (ST_OK, vec![]));
        assert!(valid_kind("plant.silo-levels_2"));
        assert!(!valid_kind("Plant.Silo") && !valid_kind("") && !valid_kind(&"a".repeat(129)));
    }
}
