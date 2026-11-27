//! Link connections: made and accepted, their messages and media, bridged streams.

use super::*;

/// Connects a link to the sender at `addr` (its TCP port number, on UDP) within
/// `timeout`. Media arriving on the link goes to `sink` as `"link"`.
///
/// Errors: `PermissionDenied` when authentication failed in mode `s1` or `s2` (no
/// fallback for this session, §E.11.8); anything else may fall back to the baseline.
pub fn connect(
    addr: SocketAddr,
    link: &LinkConfig,
    secure: Option<&SecureConfig>,
    limits: Limits,
    sink: Sink,
    timeout: Duration,
) -> io::Result<LinkClient> {
    let local = Arc::new(Local::new(link, secure));
    if local.offers.is_empty() {
        return Err(io::Error::other("link: no mode to offer"));
    }
    let cfg = client_config(&local)?;
    crate::quic::runtime().block_on(async move {
        let mut ep = quinn::Endpoint::client(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0))?;
        ep.set_default_client_config(cfg);
        let connecting = ep.connect(addr, SERVER_NAME).map_err(io::Error::other)?;
        let conn = match tokio::time::timeout(timeout, connecting).await {
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("link: no answer from UDP {addr} within {timeout:?}"),
                ));
            }
            Ok(Err(e)) => return Err(connect_error(&e)),
            Ok(Ok(c)) => c,
        };
        let (kind, cert) = match negotiated(&conn, &local) {
            Ok(x) => x,
            Err(e) => {
                conn.close(code::REFUSED.into(), e.to_string().as_bytes());
                return Err(e);
            }
        };
        let (send, recv) = conn.open_bi().await.map_err(|e| connect_error(&e))?;
        let late = Arc::new(AtomicU64::new(0));
        tokio::spawn(receive_media(conn.clone(), limits, sink, late.clone()));
        // Keep the endpoint alive as long as the connection.
        let (ep2, c2) = (ep.clone(), conn.clone());
        let first_close: Arc<std::sync::OnceLock<quinn::ConnectionError>> = Arc::default();
        let fc = first_close.clone();
        tokio::spawn(async move {
            // The first reason, kept: `close_reason` later reports our own close of the
            // bridge instead of the sender's alert.
            let _ = fc.set(c2.closed().await);
            ep2.wait_idle().await;
        });
        let control = bridge(conn.clone(), send, recv).await?;
        Ok(LinkClient {
            control,
            mode: local.mode(kind, &cert),
            peer_fingerprint: fingerprint_of(&cert),
            peer_cert: cert,
            conn,
            late,
            first_close,
            ended: Default::default(),
        })
    })
}

pub(super) async fn accept(
    incoming: quinn::Incoming,
    cfg: quinn::ServerConfig,
    local: &Local,
) -> io::Result<Accepted> {
    let peer = incoming.remote_address();
    let connecting = incoming
        .accept_with(Arc::new(cfg))
        .map_err(|e| io::Error::other(format!("{peer}: {e}")))?;
    let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(io::Error::other(format!("{peer}: {e}"))),
        Err(_) => return Err(io::Error::other(format!("{peer}: handshake took too long"))),
    };
    let (kind, cert) = match negotiated(&conn, local) {
        Ok(x) => x,
        Err(e) => {
            conn.close(code::REFUSED.into(), e.to_string().as_bytes());
            return Err(io::Error::other(format!("{peer}: {e}")));
        }
    };
    let (send, recv) = match tokio::time::timeout(CONTROL_TIMEOUT, conn.accept_bi()).await {
        Ok(Ok(s)) => s,
        _ => {
            conn.close(code::PROTOCOL.into(), b"no control stream");
            return Err(io::Error::other(format!("{peer}: no control stream")));
        }
    };
    let control = bridge(conn.clone(), send, recv).await?;
    Ok(Accepted {
        control,
        peer,
        mode: local.mode(kind, &cert),
        peer_fingerprint: fingerprint_of(&cert),
        peer_cert: cert,
        conn: LinkConn(conn),
    })
}

/// Media streams from the sender (§E.11.5): a 4-byte header, then §2.3.2 messages.
pub(super) async fn receive_media(
    conn: quinn::Connection,
    limits: Limits,
    sink: Sink,
    late: Arc<AtomicU64>,
) {
    // Per-frame streams (§E.13.2): the newest frame delivered, by stream number.
    let newest: Arc<Mutex<Option<u64>>> = Arc::default();
    while let Ok(mut rs) = conn.accept_uni().await {
        let (sink, newest, late) = (sink.clone(), newest.clone(), late.clone());
        tokio::spawn(async move {
            let mut head = [0u8; 4];
            if rs.read_exact(&mut head).await.is_err() {
                return;
            }
            if head[0] == STREAM_REPAIR {
                // Handed to the input unparsed: it checks and orders them (§E.17.6).
                loop {
                    let mut len = [0u8; 4];
                    if rs.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let n = u32::from_le_bytes(len) as usize;
                    if n < 1 || n > limits.max_data_len as usize + 65_536 {
                        let _ = rs.stop(code::PROTOCOL.into());
                        return;
                    }
                    let Some(payload) = crate::quic::read_len(&mut rs, n).await else {
                        return;
                    };
                    let kind = if payload[0] == 2 {
                        MediaKind::Audio
                    } else {
                        MediaKind::Video
                    };
                    let f = Frame {
                        kind: crate::mcast::MESSAGE_KIND,
                        scrambled: false,
                        version: 0,
                        ext: Vec::new(),
                        payload,
                    };
                    sink(kind, "repair", Ok(f));
                }
            }
            let kind = match head[0] {
                STREAM_VIDEO | STREAM_FRAME => MediaKind::Video,
                STREAM_AUDIO => MediaKind::Audio,
                _ => {
                    let _ = rs.stop(code::PROTOCOL.into());
                    return;
                }
            };
            if head[0] == STREAM_FRAME {
                // One message, then the end; abandoned (reset) ones are simply dropped.
                let Some(f) = read_message(&mut rs, &limits).await else {
                    return;
                };
                let n = rs.id().index();
                let fresh = {
                    let mut nw = newest.lock_ok();
                    let fresh = nw.is_none_or(|x| n > x);
                    if fresh {
                        *nw = Some(n);
                    }
                    fresh
                };
                if fresh {
                    sink(kind, "link", f);
                } else {
                    late.fetch_add(1, Ordering::Relaxed); // a newer frame was already shown
                }
                return;
            }
            loop {
                let mut len = [0u8; 4];
                if rs.read_exact(&mut len).await.is_err() {
                    return;
                }
                let msg_len = u32::from_le_bytes(len) as usize;
                let mut hb = [0u8; HEADER_LEN];
                if msg_len < HEADER_LEN || rs.read_exact(&mut hb).await.is_err() {
                    return;
                }
                let h = Header::parse(&hb);
                let body_len = match h.check(&limits) {
                    Ok(n) if HEADER_LEN + n == msg_len => n,
                    Ok(_) => return sink(kind, "link", Err(crate::frame::FrameError::BadText)),
                    Err(e) => return sink(kind, "link", Err(e)),
                };
                let Some(body) = crate::quic::read_len(&mut rs, body_len).await else {
                    return;
                };
                sink(kind, "link", Ok(Frame::from_wire(h, body)));
            }
        });
    }
}

/// One §2.3.2 message from a stream; `None` at its end, or when it was reset.
pub(super) async fn read_message(
    rs: &mut quinn::RecvStream,
    limits: &Limits,
) -> Option<Result<Frame, crate::frame::FrameError>> {
    let mut len = [0u8; 4];
    rs.read_exact(&mut len).await.ok()?;
    let msg_len = u32::from_le_bytes(len) as usize;
    let mut hb = [0u8; HEADER_LEN];
    if msg_len < HEADER_LEN {
        return None;
    }
    rs.read_exact(&mut hb).await.ok()?;
    let h = Header::parse(&hb);
    let body_len = match h.check(limits) {
        Ok(n) if HEADER_LEN + n == msg_len => n,
        Ok(_) => return Some(Err(crate::frame::FrameError::BadText)),
        Err(e) => return Some(Err(e)),
    };
    let body = crate::quic::read_len(rs, body_len).await?;
    Some(Ok(Frame::from_wire(h, body)))
}

/// Sends one video message on a stream of its own (§E.13.2); returns false if its
/// deadline passed first and the stream was reset (its data is no longer retransmitted).
pub(super) async fn send_frame(
    conn: &quinn::Connection,
    priority: i32,
    m: &[u8],
    deadline: Duration,
) -> bool {
    let end = tokio::time::Instant::now() + deadline;
    let Ok(Ok(mut s)) = tokio::time::timeout_at(end, conn.open_uni()).await else {
        return false;
    };
    let _ = s.set_priority(priority);
    let sent = tokio::time::timeout_at(end, async {
        s.write_all(&[STREAM_FRAME, 0, 0, 0]).await?;
        s.write_all(&(m.len() as u32).to_le_bytes()).await?;
        s.write_all(m).await?;
        s.finish()?;
        // Done once the receiver has it all.
        let _ = s.stopped().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await;
    match sent {
        Ok(Ok(())) => true,
        _ => {
            let _ = s.reset(code::LATE.into());
            false
        }
    }
}

/// Bridges a bidirectional stream to a loopback TCP pair and returns the application's
/// end. When the application closes it, the stream is finished and, once the peer has
/// it (or after 2 s), the connection is closed.
pub(super) async fn bridge(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> io::Result<TcpStream> {
    let (app, pipe) = crate::secure::loopback_pair()?;
    let _ = app.set_nodelay(true);
    let _ = pipe.set_nodelay(true);
    pipe.set_nonblocking(true)?;
    let pipe = tokio::net::TcpStream::from_std(pipe)?;
    let (mut pr, mut pw) = pipe.into_split();
    // Control before audio before video (§E.11.5).
    let _ = send.set_priority(2);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut recv, &mut pw).await;
        let _ = pw.shutdown().await;
    });
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut pr, &mut send).await;
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        conn.close(code::DONE.into(), b"");
    });
    Ok(app)
}

/// `xml` (an `ndi_version`) with `token` added to its `x_idn_ext` list.
pub fn announce_token(xml: &str, token: &str) -> String {
    match xml.find("x_idn_ext=\"") {
        Some(i) => {
            let at = i + "x_idn_ext=\"".len();
            let end = xml[at..].find('"').map_or(at, |e| at + e);
            if xml[at..end].split_whitespace().any(|t| t == token) {
                return xml.to_owned();
            }
            let sep = if end > at { " " } else { "" };
            format!("{}{sep}{token}{}", &xml[..end], &xml[end..])
        }
        None => xml.to_owned(),
    }
}

/// Whether the sender turned down this side's certificate after the handshake had
/// completed here (TLS 1.3: the client finishes before the server has checked it), and
/// what to say: e.g. a device revoked while it was offline.
pub(crate) fn refused_by_sender(
    closed: &std::sync::OnceLock<quinn::ConnectionError>,
) -> Option<String> {
    let why = closed.get()?.to_string();
    why.contains("cryptographic handshake failed").then(|| {
        format!(
            "the source refused this device's certificate on the IDN link: it may have been \
             revoked by the site authority, or belong to another site ({why})"
        )
    })
}

/// The path's queueing delay (µs): the smoothed round trip above the lowest seen. Media
/// waiting anywhere between the sender and the receiver (send window, network queues,
/// retransmission) adds to it; a change of route that lowers the base resets the floor.
pub(super) fn queueing(rtt_us: u64, min_rtt_us: u64) -> u64 {
    rtt_us.saturating_sub(min_rtt_us)
}

/// The share of video frames to skip for a queueing delay: none up to the allowance,
/// all at twice the allowance, in proportion between.
pub(super) fn skip_share(delay_us: u64, allowed_us: u64) -> f64 {
    (delay_us.saturating_sub(allowed_us) as f64 / allowed_us.max(1) as f64).clamp(0.0, 1.0)
}
