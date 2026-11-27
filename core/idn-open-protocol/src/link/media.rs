//! Link sessions at work: a connection's media and repair streams, the sender's server,
//! the receiver's client, and the link announced in `x_idn_ext`.

use super::*;

impl LinkConn {
    /// Opens the video and audio streams (§E.11.5). With `per_frame`, each video message
    /// goes on a stream of its own with a deadline instead (`frames/1`, §E.13.2).
    pub fn media(&self, per_frame: bool) -> LinkMedia {
        let failed: Arc<Mutex<Option<String>>> = Default::default();
        let pending = Arc::new(AtomicU64::new(0));
        let deadline_us = Arc::new(AtomicU64::new(66_667));
        let resets = Arc::new(AtomicU64::new(0));
        let _g = crate::quic::runtime().enter();
        let pending_audio = Arc::new(AtomicU64::new(0));
        let open = |ty: u8, priority: i32, pending: Arc<AtomicU64>| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            let (conn, failed) = (self.0.clone(), failed.clone());
            tokio::spawn(async move {
                let r: Result<(), String> = async {
                    let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
                    let _ = s.set_priority(priority);
                    s.write_all(&[ty, 0, 0, 0])
                        .await
                        .map_err(|e| e.to_string())?;
                    while let Some(m) = rx.recv().await {
                        let r = async {
                            s.write_all(&(m.len() as u32).to_le_bytes()).await?;
                            s.write_all(&m).await
                        }
                        .await;
                        pending.fetch_sub(m.len() as u64 + 4, Ordering::Relaxed);
                        r.map_err(|e| e.to_string())?;
                    }
                    let _ = s.finish();
                    Ok(())
                }
                .await;
                if let Err(e) = r {
                    failed.lock_ok().get_or_insert(e);
                }
            });
            tx
        };
        let video = if per_frame {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            let (conn, pending, deadline_us, resets) = (
                self.0.clone(),
                pending.clone(),
                deadline_us.clone(),
                resets.clone(),
            );
            tokio::spawn(async move {
                // Newer frames get higher priority, so a backlog never delays the current
                // one (§E.13.2).
                let mut priority: i32 = 2;
                while let Some(m) = rx.recv().await {
                    priority = priority.wrapping_add(1).max(2);
                    // Never under four round trips: each lost packet costs a round trip
                    // to repair, and two frame periods is less than that on a WAN (at
                    // 30 ms RTT nearly every frame was abandoned).
                    let deadline = Duration::from_micros(deadline_us.load(Ordering::Relaxed))
                        .max(conn.rtt() * 4);
                    let (conn, pending, resets) = (conn.clone(), pending.clone(), resets.clone());
                    tokio::spawn(async move {
                        let len = m.len() as u64 + 4;
                        if !send_frame(&conn, priority, &m, deadline).await {
                            resets.fetch_add(1, Ordering::Relaxed);
                        }
                        pending.fetch_sub(len, Ordering::Relaxed);
                    });
                }
            });
            tx
        } else {
            open(STREAM_VIDEO, 0, pending.clone())
        };
        LinkMedia {
            video,
            audio: open(STREAM_AUDIO, 1, pending_audio.clone()),
            conn: self.0.clone(),
            min_rtt_us: AtomicU64::new(u64::MAX),
            pace: Mutex::new((0.0, Instant::now())),
            failed,
            pending,
            pending_audio,
            deadline_us,
            resets,
        }
    }

    /// This side's address on the link, as the receiver reached it.
    pub fn local_ip(&self) -> Option<std::net::IpAddr> {
        self.0.local_ip()
    }

    /// The stream for re-sending lost multicast messages (§E.17.6).
    pub fn repairs(&self) -> LinkRepairs {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let conn = self.0.clone();
        let _g = crate::quic::runtime().enter();
        tokio::spawn(async move {
            let Ok(mut s) = conn.open_uni().await else {
                return;
            };
            // Ahead of the media: a repair is only useful soon.
            let _ = s.set_priority(10);
            if s.write_all(&[STREAM_REPAIR, 0, 0, 0]).await.is_err() {
                return;
            }
            while let Some(m) = rx.recv().await {
                if s.write_all(&(m.len() as u32).to_le_bytes()).await.is_err()
                    || s.write_all(&m).await.is_err()
                {
                    return;
                }
            }
            let _ = s.finish();
        });
        LinkRepairs { tx }
    }

    /// Closes the connection with an application code and reason (§E.11.6).
    pub fn close(&self, code: u32, reason: &str) {
        self.0.close(code.into(), reason.as_bytes());
    }

    /// Why the connection ended, once it has.
    pub fn closed(&self) -> Option<String> {
        self.0
            .close_reason()
            .map(|e| crate::quic::server::close_reason(&e))
    }
}

impl LinkRepairs {
    /// Queues a re-sent message: the multicast stream byte, then the message with its
    /// trailer. `false` if too many are waiting (they would be too late anyway).
    pub fn send(&self, stream: u8, message: &[u8]) -> bool {
        let mut m = Vec::with_capacity(1 + message.len());
        m.push(stream);
        m.extend_from_slice(message);
        self.tx.try_send(m).is_ok()
    }
}

impl LinkMedia {
    /// Queues one media message (frame bytes); returns false if it was skipped. As on the
    /// other QUIC paths, new video is skipped while about two frames are waiting (at
    /// least 512 kB), audio only far behind: drop frames, never stall.
    pub fn send(&self, is_video: bool, frame_bytes: Vec<u8>) -> bool {
        let len = frame_bytes.len() as u64 + 4;
        // Each kind is judged by its own backlog: one counter for both let audio waiting
        // on a slow link (audio goes first, and uncompressed stereo alone is 3 Mbit/s)
        // keep every video frame skipped, so a narrow link showed sound and no picture
        // at all. Audio is dropped only far behind (about 2 s).
        let (pending, tx, limit) = if is_video {
            (
                &self.pending,
                &self.video,
                (2 * len).clamp(512 << 10, 16 << 20),
            )
        } else {
            (&self.pending_audio, &self.audio, 2 << 20)
        };
        if pending.load(Ordering::Relaxed) >= limit {
            return false;
        }
        // A receiver behind on a saturated path shows as queueing delay: the round trip
        // grows by the time media waits in the connection's send window, in the network's
        // queues or in retransmission (seconds behind live, nothing
        // in the sender's own queue). Video is skipped in proportion as the delay grows
        // past its allowance (two frame periods, at least 150 ms), all of it at twice
        // that; audio is dropped past half a second. Skipping every frame while the delay
        // is above a line halved the frame rate the link had room for and left gaps of
        // 1.5 s, since the audio alone kept the delay near the line.
        let rtt_us = self.conn.rtt().as_micros() as u64;
        let min = self
            .min_rtt_us
            .fetch_min(rtt_us, Ordering::Relaxed)
            .min(rtt_us);
        let delay = queueing(rtt_us, min);
        if is_video {
            let allowed = self.deadline_us.load(Ordering::Relaxed).max(150_000);
            let share = skip_share(delay, allowed);
            let mut p = self.pace.lock_ok();
            // A frame every half second at least while the delay is below twice the
            // allowance, so a receiver behind still sees the picture move.
            let overdue = p.1.elapsed() >= Duration::from_millis(500) && share < 1.0;
            p.0 += share;
            if p.0 >= 1.0 && !overdue {
                p.0 -= 1.0;
                return false;
            }
            p.0 = p.0.min(1.0);
            p.1 = Instant::now();
        } else if delay > 500_000 {
            return false;
        }
        pending.fetch_add(len, Ordering::Relaxed);
        if tx.try_send(frame_bytes).is_err() {
            pending.fetch_sub(len, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Why a media stream failed, once one has.
    pub fn failed(&self) -> Option<String> {
        self.failed.lock_ok().clone()
    }

    /// Per-frame streams (§E.13.2): how long a frame may take before it is abandoned.
    pub fn set_deadline(&self, d: Duration) {
        self.deadline_us
            .store(d.as_micros().max(1) as u64, Ordering::Relaxed);
    }

    /// Per-frame streams: frames abandoned at their deadline so far.
    pub fn abandoned(&self) -> u64 {
        self.resets.load(Ordering::Relaxed)
    }
}

impl LinkServer {
    /// Listens on UDP `port`. Each accepted session is handed to `on_accept` (on a
    /// runtime thread: it should only hand it over). Credentials are read again for
    /// every connection, so an enrolment completing later is used at once.
    pub fn bind(
        port: u16,
        link: LinkConfig,
        secure: Option<SecureConfig>,
        on_accept: impl Fn(Accepted) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
        Self::bind_socket(sock, link, secure, on_accept)
    }

    /// As [`Self::bind`], on a socket already bound (a sender's UDP twin of its TCP port,
    /// kept from when it was found free).
    pub fn bind_socket(
        sock: UdpSocket,
        link: LinkConfig,
        secure: Option<SecureConfig>,
        on_accept: impl Fn(Accepted) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        // One socket serves every receiver: room for bursts to all of them, so that one
        // slow path's queued datagrams don't hold the others back.
        let _ = socket2::SockRef::from(&sock).set_recv_buffer_size(8 << 20);
        let _ = socket2::SockRef::from(&sock).set_send_buffer_size(8 << 20);
        let port = sock.local_addr()?.port();
        let first = Arc::new(Local::new(&link, secure.as_ref()));
        let endpoint = {
            let _g = crate::quic::runtime().enter();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(server_config(&first)?),
                sock,
                Arc::new(quinn::TokioRuntime),
            )?
        };
        let ep = endpoint.clone();
        let on_accept = Arc::new(on_accept);
        let busy = Arc::new(AtomicUsize::new(0));
        crate::quic::runtime().spawn(async move {
            while let Some(incoming) = ep.accept().await {
                if SIMULATE_BLOCKED.load(Ordering::Relaxed) {
                    incoming.ignore();
                    continue;
                }
                if busy.load(Ordering::Relaxed) >= MAX_HANDSHAKES {
                    incoming.refuse();
                    continue;
                }
                let local = Arc::new(Local::new(&link, secure.as_ref()));
                let Ok(cfg) = server_config(&local) else {
                    incoming.refuse();
                    continue;
                };
                let (on_accept, busy) = (on_accept.clone(), busy.clone());
                busy.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let slow = SIMULATE_SLOW_MS.load(Ordering::Relaxed);
                    if slow > 0 {
                        tokio::time::sleep(Duration::from_millis(slow)).await;
                    }
                    let r = accept(incoming, cfg, &local).await;
                    busy.fetch_sub(1, Ordering::Relaxed);
                    match r {
                        Ok(a) => on_accept(a),
                        Err(e) => crate::log::log(&format!("link: {e}")),
                    }
                });
            }
        });
        Ok(LinkServer { endpoint, port })
    }

    /// The UDP port listened on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for LinkServer {
    fn drop(&mut self) {
        self.endpoint.close(code::GOING_AWAY.into(), b"closed");
    }
}

impl LinkClient {
    /// A watch on how the connection ends, for [`refused_by_sender`] after the client
    /// was handed on.
    pub(crate) fn watch(&self) -> Arc<std::sync::OnceLock<quinn::ConnectionError>> {
        self.first_close.clone()
    }

    /// Why the link ended, once it has: the sender's close, or no answer from it for
    /// the idle time (a cable pull, power loss).
    pub fn close_reason(&self) -> Option<String> {
        use quinn::ConnectionError as C;
        let reason = self
            .first_close
            .get()
            .cloned()
            .or_else(|| self.conn.close_reason())?;
        Some(match reason {
            C::TimedOut => "no answer from the sender on the IDN link (idle timeout)".into(),
            C::ApplicationClosed(c) if u64::from(c.error_code) == u64::from(code::GOING_AWAY) => {
                "the sender is going away".into()
            }
            C::ApplicationClosed(_) => "sender closed the IDN link".into(),
            other => refused_by_sender(&self.first_close)
                .unwrap_or_else(|| format!("IDN link lost: {other}")),
        })
    }

    /// Per-frame streams (§E.13.2): frames dropped because a newer one was delivered
    /// first.
    pub fn late_frames(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }

    /// Closes the connection with an application code and reason (§E.11.6).
    pub fn close(&self, code: u32, reason: &str) {
        self.conn.close(code.into(), reason.as_bytes());
    }
}

/// Whether a peer's `x_idn_ext` list (or hint) offers the link.
pub fn offered(list: &str) -> bool {
    list.split_whitespace().any(|t| t == TOKEN)
}

/// `xml` (an `ndi_version`) with `link/1` added to its `x_idn_ext` list.
pub fn announce(xml: &str) -> String {
    announce_token(xml, TOKEN)
}
