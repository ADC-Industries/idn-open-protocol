//! A channel's streams: messages and replies read and written, requests and pushes
//! served, files offered, fetched and checked.

use super::*;

pub(super) fn check_type(kind: &str) -> io::Result<()> {
    let ok = !kind.is_empty()
        && kind.len() <= MAX_TYPE
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad message type {kind:?}: lowercase letters, digits, '.', '-', '_'"),
        ))
    }
}

pub(super) fn check_payload(p: &[u8]) -> io::Result<()> {
    if p.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload over 16 MiB: send it as a file",
        ));
    }
    Ok(())
}

pub(super) fn header(op: u8, kind: &str, payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::with_capacity(7 + kind.len() + payload.len());
    h.push(op);
    h.extend_from_slice(&(kind.len() as u16).to_le_bytes());
    h.extend_from_slice(kind.as_bytes());
    h.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    h.extend_from_slice(payload);
    h
}

/// Reads a stream header after its `op` byte: (type, payload).
pub(super) async fn read_message(rs: &mut quinn::RecvStream) -> io::Result<(String, Vec<u8>)> {
    use idn_common::err::invalid_data as bad;
    let mut n2 = [0u8; 2];
    rs.read_exact(&mut n2).await.map_err(io::Error::other)?;
    let tl = u16::from_le_bytes(n2) as usize;
    if tl == 0 || tl > MAX_TYPE {
        return Err(bad("bad type length"));
    }
    let mut t = vec![0u8; tl];
    rs.read_exact(&mut t).await.map_err(io::Error::other)?;
    let kind = String::from_utf8(t).map_err(|_| bad("type is not UTF-8"))?;
    check_type(&kind)?;
    let mut n4 = [0u8; 4];
    rs.read_exact(&mut n4).await.map_err(io::Error::other)?;
    let pl = u32::from_le_bytes(n4) as usize;
    if pl > MAX_PAYLOAD {
        return Err(bad("payload too large"));
    }
    let p = crate::quic::read_len(rs, pl)
        .await
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ended"))?;
    Ok((kind, p))
}

pub(super) async fn write_reply(
    s: &mut quinn::SendStream,
    status: u8,
    body: &[u8],
) -> io::Result<()> {
    let mut m = Vec::with_capacity(5 + body.len());
    m.push(status);
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(body);
    s.write_all(&m).await?;
    Ok(())
}

pub(super) async fn read_reply(rs: &mut quinn::RecvStream) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    rs.read_exact(&mut h).await.map_err(io::Error::other)?;
    let len = u32::from_le_bytes(h[1..5].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reply too large",
        ));
    }
    let body = crate::quic::read_len(rs, len)
        .await
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ended"))?;
    Ok((h[0], body))
}

/// The application's handler runs on a blocking thread (it may do file I/O or wait).
pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| io::Error::other("handler failed"))
}

pub(super) async fn on_uni(
    mut rs: quinn::RecvStream,
    peer: Peer,
    ch: Weak<Channel>,
) -> io::Result<()> {
    let mut op = [0u8; 1];
    rs.read_exact(&mut op).await.map_err(io::Error::other)?;
    if op[0] != OP_MESSAGE {
        let _ = rs.stop(2u32.into());
        return Ok(());
    }
    let (kind, payload) = read_message(&mut rs).await?;
    let Some(ch) = ch.upgrade() else {
        return Ok(());
    };
    let h = ch.handler.clone();
    blocking(move || h.on_message(&peer, &kind, payload)).await
}

pub(super) async fn on_bi(
    mut send: quinn::SendStream,
    mut rs: quinn::RecvStream,
    peer: Peer,
    ch: Weak<Channel>,
    conn: quinn::Connection,
) -> io::Result<()> {
    let mut op = [0u8; 1];
    rs.read_exact(&mut op).await.map_err(io::Error::other)?;
    let (kind, payload) = read_message(&mut rs).await?;
    let Some(ch) = ch.upgrade() else {
        return Ok(());
    };
    match op[0] {
        OP_REQUEST => {
            let (status, body) = match kind.as_str() {
                "idn.status" => (ST_OK, ch.status_xml().into_bytes()),
                "idn.file.push" => on_push(&ch, &peer, &payload, conn).await,
                // The site's media library's requests (§E.12.11) are the application's.
                k if k.starts_with("idn.") && !k.starts_with("idn.library.") => {
                    (ST_UNKNOWN, Vec::new())
                }
                _ => {
                    let h = ch.handler.clone();
                    let p = peer.clone();
                    match blocking(move || h.on_request(&p, &kind, payload)).await? {
                        Reply::Ok(b) => (ST_OK, b),
                        Reply::Error(e) => (ST_ERROR, e.into_bytes()),
                        Reply::Unknown => (ST_UNKNOWN, Vec::new()),
                        Reply::Refused => (ST_REFUSED, Vec::new()),
                    }
                }
            };
            write_reply(&mut send, status, &body).await?;
            let _ = send.finish();
        }
        OP_FETCH => serve_fetch(&ch, &peer, send, &payload).await?,
        _ => {
            let _ = rs.stop(2u32.into());
        }
    }
    Ok(())
}

/// A push offer (§E.12.5): the application accepts with a sink; then this device fetches
/// the file from the peer in the background.
/// The file is fetched back over the connection the offer came on (the offering
/// endpoint: a program's beside its machine's IDN Node), then the device's others.
pub(super) async fn on_push(
    ch: &Arc<Channel>,
    peer: &Peer,
    payload: &[u8],
    conn: quinn::Connection,
) -> (u8, Vec<u8>) {
    let Some(info) = std::str::from_utf8(payload)
        .ok()
        .and_then(parse_one)
        .and_then(|e| parse_file(&e, None))
    else {
        return (ST_ERROR, b"bad offer".to_vec());
    };
    let (h, p, i) = (ch.handler.clone(), peer.clone(), info.clone());
    let Ok(Some(mut sink)) = blocking(move || h.on_push(&p, &i)).await else {
        return (ST_REFUSED, Vec::new());
    };
    let (ch2, peer2) = (Arc::downgrade(ch), peer.clone());
    tokio::task::spawn_blocking(move || {
        let Some(ch) = ch2.upgrade() else { return };
        let r = ch
            .fetch_via(
                Some(conn),
                &peer2.fingerprint,
                &info.id,
                Some(&info),
                &mut *sink,
                &mut |_, _| {},
            )
            .map(|_| ());
        ch.handler.on_received(&peer2, &info, r);
    });
    (ST_OK, Vec::new())
}

/// The single XML element in `s`.
pub(super) fn parse_one(s: &str) -> Option<Element> {
    crate::xml::parse_fragment(s)?.into_iter().next()
}

pub(super) fn file_element(info: &FileInfo) -> Element {
    Element::new("idn_file")
        .with_attr("id", &info.id)
        .with_attr("name", &info.name)
        .with_attr("size", &info.size.to_string())
        .with_attr("version", &info.version)
}

pub(super) fn parse_file(e: &Element, id: Option<&str>) -> Option<FileInfo> {
    if e.name != "idn_file" {
        return None;
    }
    let version = e.attr("version")?.to_owned();
    if version.len() != 64 || !version.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // A name a receiver may write under: never one that leaves its folder (an absolute
    // path, `..`, a backslash, a drive, NUL), whatever the handler does with it.
    let name = e.attr("name").unwrap_or("");
    if name.starts_with('/')
        || name.contains(['\\', '\0'])
        || (cfg!(windows) && name.contains(':'))
        || name.split('/').any(|p| p == "..")
    {
        return None;
    }
    Some(FileInfo {
        id: id.or(e.attr("id"))?.to_owned(),
        name: name.to_owned(),
        size: e.attr("size")?.parse().ok()?,
        version,
    })
}

pub(super) async fn serve_fetch(
    ch: &Arc<Channel>,
    peer: &Peer,
    mut send: quinn::SendStream,
    payload: &[u8],
) -> io::Result<()> {
    let _ = send.set_priority(-1); // files go last (§E.12.5)
    let Some(req) = std::str::from_utf8(payload)
        .ok()
        .and_then(parse_one)
        .filter(|e| e.name == "idn_fetch")
    else {
        return write_reply(&mut send, ST_ERROR, b"bad fetch").await;
    };
    let id = req.attr("id").unwrap_or("").to_owned();
    let path = ch
        .offers
        .lock_ok()
        .get(&id)
        .filter(|(_, at, _)| at.elapsed() < OFFER_LIFE)
        .map(|(p, _, ident)| (p.clone(), *ident));
    let (path, ident) = match path {
        Some((p, ident)) => (Some(p), ident),
        None => {
            let (h, p, i) = (ch.handler.clone(), peer.clone(), id.clone());
            (blocking(move || h.file(&p, &i)).await?, None)
        }
    };
    let Some(path) = path else {
        return write_reply(&mut send, ST_UNKNOWN, b"").await;
    };
    // The application hears how it went: the box that fetched an offered file has it all,
    // or not (a sender's "sent").
    let r = serve_file(ch, peer, &mut send, &id, &req, path, ident).await;
    let served = match &r {
        Ok(size) => Served::Done { size: *size },
        Err(e) => Served::Failed(e.to_string()),
    };
    let (h, p) = (ch.handler.clone(), peer.clone());
    blocking(move || h.on_served(&p, &id, served)).await?;
    r.map(|_| ())
}

/// Serves file `id` at `path` (as identified when offered) for `req`: its size once the
/// peer has read it to the end, else why not (an error answered to the peer included).
async fn serve_file(
    ch: &Arc<Channel>,
    peer: &Peer,
    send: &mut quinn::SendStream,
    id: &str,
    req: &Element,
    path: PathBuf,
    ident: Option<super::FileIdentity>,
) -> io::Result<u64> {
    async fn answer(send: &mut quinn::SendStream, why: &str) -> io::Error {
        let _ = write_reply(send, ST_ERROR, why.as_bytes()).await;
        io::Error::other(why.to_owned())
    }
    let offset: u64 = req.attr("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
    let Ok(_permit) = ch.serving.clone().try_acquire_owned() else {
        return Err(answer(send, "busy: too many transfers").await);
    };
    // Opened first, and the very file offered (bug run 2026-11-24: its path was opened
    // again at the fetch, after a folder on the way could have become a link).
    let mut f = match open_plain(&path, std::fs::OpenOptions::new().read(true), false) {
        Ok(f) if super::is_identified(&f, ident) => f,
        Ok(_) => {
            return Err(answer(
                send,
                "the offered file isn't there any more: offer it again",
            )
            .await);
        }
        Err(e) => return Err(answer(send, &e.to_string()).await),
    };
    let (ch2, id2, path2) = (ch.clone(), id.to_owned(), path.clone());
    let info = match blocking(move || ch2.file_info(&id2, &path2)).await? {
        Ok(i) => i,
        Err(e) => return Err(answer(send, &e.to_string()).await),
    };
    // Resuming a version that no longer exists: start over (status 4).
    let (status, from) = match req.attr("version") {
        Some(v) if v != info.version => (ST_CHANGED, 0),
        _ if offset > info.size => {
            return Err(answer(send, "offset beyond the end").await);
        }
        _ => (ST_OK, offset),
    };
    write_reply(send, status, file_element(&info).to_xml().as_bytes()).await?;
    f.seek(io::SeekFrom::Start(from))?;
    let mut left = info.size - from;
    let mut pace = Pace::default();
    // The file is read on a blocking thread, a megabyte at a time: a slow disk or a NAS
    // must not stall the runtime that also carries live video (the link's streams).
    let mut file = Some(f);
    while left > 0 {
        let want = (left as usize).min(READ_AHEAD);
        let mut f = file.take().expect("kept between reads");
        // (What went so far told on the same thread: the application's progress.)
        let (h, p, i) = (ch.handler.clone(), peer.clone(), id.to_owned());
        let (sent, size) = (info.size - left, info.size);
        let (f2, buf) = blocking(move || {
            if sent > from {
                h.on_served(&p, &i, Served::Progress { sent, size });
            }
            let mut buf = vec![0u8; want];
            let r = f.read_exact(&mut buf).map(|_| buf);
            (f, r)
        })
        .await?;
        file = Some(f2);
        let buf = buf?;
        for piece in buf.chunks(CHUNK) {
            ch.pace(&mut pace, &peer.fingerprint, piece.len() as u64)
                .await;
            send.write_all(piece).await?;
        }
        left -= want as u64;
    }
    let _ = send.finish();
    // Whole: every byte and the stream's end acknowledged (the fetcher reads to the end,
    // §E.12.5); a stop before means it didn't take it all. (A fetcher from before that
    // let the stream go once it had its bytes shows here as stopped: the receiving
    // machine's own transfer state is the one to trust.)
    match send.stopped().await {
        Ok(None) => Ok(info.size),
        Ok(Some(code)) => Err(io::Error::other(format!(
            "{} stopped the transfer (code {})",
            peer.name,
            u64::from(code)
        ))),
        Err(e) => Err(io::Error::other(e)),
    }
}

pub(super) async fn fetch_into(
    conn: quinn::Connection,
    id: &str,
    offered: Option<&FileInfo>,
    sink: &mut dyn FileSink,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<FileInfo> {
    use idn_common::err::invalid_data as bad;
    let held = sink.held()?;
    let mut req = Element::new("idn_fetch").with_attr("id", id);
    if let Some((n, v)) = &held {
        req = req
            .with_attr("offset", &n.to_string())
            .with_attr("version", v);
    } else {
        req = req.with_attr("offset", "0");
    }
    let (mut send, mut recv) = conn.open_bi().await.map_err(io::Error::other)?;
    send.write_all(&header(OP_FETCH, "idn.file", req.to_xml().as_bytes()))
        .await?;
    send.finish().map_err(io::Error::other)?;
    let (status, body) = read_reply(&mut recv).await?;
    let text = String::from_utf8_lossy(&body).into_owned();
    match status {
        ST_OK | ST_CHANGED => {}
        ST_UNKNOWN => return Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        ST_REFUSED => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused")),
        _ => return Err(io::Error::other(text)),
    }
    let info = parse_one(&text)
        .and_then(|e| parse_file(&e, Some(id)))
        .ok_or_else(|| bad("bad file description"))?;
    // A push: the file offered, not another (bug run 2026-11-24: the offer's size was
    // checked for room and its version kept as the file's SHA-256, while the bytes
    // fetched followed the fetch's own description).
    if let Some(o) = offered
        && (o.size != info.size || o.version != info.version)
    {
        return Err(bad("the file fetched isn't the one offered"));
    }
    let mut have = match (status, &held) {
        (ST_OK, Some((n, _))) => *n,
        _ => {
            sink.restart()?;
            0
        }
    };
    sink.begin(&info)?;
    // Hashed as it arrives when the whole file comes in this transfer.
    let mut hash = (have == 0).then(sha2::Sha256::new);
    let mut buf = vec![0u8; CHUNK];
    while have < info.size {
        let n = match recv.read(&mut buf).await.map_err(io::Error::other)? {
            Some(n) => n,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "transfer cut short",
                ));
            }
        };
        if have + n as u64 > info.size {
            return Err(bad("more data than announced"));
        }
        sink.write(&buf[..n])?;
        if let Some(h) = &mut hash {
            h.update(&buf[..n]);
        }
        have += n as u64;
        progress(have, info.size);
    }
    // The stream read to its end, so the holder sees the whole file taken (not a stop;
    // a sender's "sent", §E.12.5).
    if let Ok(Ok(Some(_))) =
        tokio::time::timeout(Duration::from_secs(5), recv.read(&mut buf[..1])).await
    {
        return Err(bad("more data than announced"));
    }
    if let Some(h) = hash
        && idn_common::hex::hex(&h.finalize()) != info.version
    {
        sink.restart()?;
        return Err(bad("SHA-256 mismatch"));
    }
    sink.finish(&info)?;
    Ok(info)
}

/// A file's SHA-256, opened as [`open_plain`] does (`ours`: this account's only).
pub(super) fn sha256_file(path: &Path, ours: bool) -> io::Result<String> {
    let mut f = open_plain(path, std::fs::OpenOptions::new().read(true), ours)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(idn_common::hex::hex(&h.finalize()))
}

pub(super) fn sanitize_field(k: &str) -> String {
    k.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}
