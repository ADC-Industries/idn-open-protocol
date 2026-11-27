//! A channel's connections: opened, identified and served, one request at a time, the
//! endpoint's accept loop, and the reaper of idle connections.

use super::*;

impl Channel {
    /// The live connections to device `fp`, one per endpoint of it: its announced
    /// endpoint's first (opened if needed), then those it connected from, most recently
    /// used first. Connections to devices a CRL received since revokes are closed.
    pub(super) fn connections(&self, fp: &str) -> io::Result<Vec<quinn::Connection>> {
        let entry = self.enrollment.directory_entry(fp);
        let main_port = entry.as_ref().map(|e| e.address.port());
        let (main, others) = {
            let anchor = self.enrollment.creds().map(|c| c.anchor.clone());
            let mut conns = self.conns.lock_ok();
            let list = conns.get_mut(fp).map(std::mem::take).unwrap_or_default();
            let mut kept = Vec::new();
            for c in list {
                let open = c.conn.close_reason().is_none();
                // Still trusted? A CRL received since may revoke the peer (§E.10.6).
                let valid = anchor.as_ref().is_some_and(|a| a.verify(&c.cert).is_ok());
                if open && !valid {
                    c.conn.close(3u32.into(), b"revoked");
                }
                if open && valid {
                    kept.push(c);
                }
            }
            let main = kept.iter().find(|c| Some(c.port) == main_port).map(|c| {
                *c.last_used.lock_ok() = Instant::now();
                c.conn.clone()
            });
            let mut others: Vec<(Instant, quinn::Connection)> = kept
                .iter()
                .filter(|c| Some(c.port) != main_port)
                .map(|c| (*c.last_used.lock_ok(), c.conn.clone()))
                .collect();
            others.sort_by_key(|o| std::cmp::Reverse(o.0));
            if kept.is_empty() {
                conns.remove(fp);
            } else {
                conns.insert(fp.to_owned(), kept);
            }
            (main, others.into_iter().map(|(_, c)| c).collect::<Vec<_>>())
        };
        let main = match (main, entry) {
            (Some(c), _) => Ok(c),
            // (Not again and again while another endpoint serves.)
            (None, Some(_))
                if !others.is_empty()
                    && self
                        .open_failed
                        .lock_ok()
                        .get(fp)
                        .is_some_and(|at| at.elapsed() < OPEN_RETRY) =>
            {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "announced endpoint not answering",
                ))
            }
            (None, Some(e)) => {
                let r = self.open(fp, e);
                let mut failed = self.open_failed.lock_ok();
                match &r {
                    Ok(_) => {
                        failed.remove(fp);
                    }
                    Err(_) => {
                        failed.retain(|_, at| at.elapsed() < OPEN_RETRY);
                        failed.insert(fp.to_owned(), Instant::now());
                    }
                }
                r
            }
            (None, None) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no such device in the directory (not enrolled, or not online)",
            )),
        };
        match main {
            Ok(c) => Ok(std::iter::once(c).chain(others).collect()),
            Err(e) if others.is_empty() => Err(e),
            Err(_) => Ok(others),
        }
    }

    /// Notes connection `conn` to device `fp` as just used (for the idle close and the
    /// most-recently-used order; bug run 2026-11-24: only the announced endpoint's was).
    pub(super) fn touch(&self, fp: &str, conn: &quinn::Connection) {
        let id = conn.stable_id();
        if let Some(c) = self
            .conns
            .lock_ok()
            .get(fp)
            .and_then(|l| l.iter().find(|c| c.conn.stable_id() == id))
        {
            *c.last_used.lock_ok() = Instant::now();
        }
    }

    /// A new connection to device `fp`'s announced endpoint.
    fn open(&self, fp: &str, entry: DirEntry) -> io::Result<quinn::Connection> {
        let ep = self.endpoint.clone();
        let conn = crate::quic::runtime().block_on(async move {
            let connecting = ep
                .connect(entry.address, "idn-site")
                .map_err(io::Error::other)?;
            match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
                Ok(Ok(c)) => Ok(c),
                Ok(Err(e)) => Err(io::Error::other(format!("channel to {}: {e}", entry.name))),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("channel to {}: no answer (UDP blocked?)", entry.name),
                )),
            }
        })?;
        let (peer, cert) = self.identify(&conn)?;
        if peer.fingerprint != fp {
            conn.close(0u32.into(), b"wrong device");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "another device answered at that address",
            ));
        }
        let kept = self.register(conn.clone(), peer.clone(), cert, true);
        if kept.stable_id() == conn.stable_id() {
            // The peer may open streams on it too (a push's fetch comes back this way).
            self.serve_connection(conn.clone(), peer);
        }
        Ok(kept)
    }

    /// The peer's identity from its certificate, checked against the CA and the CRL.
    fn identify(&self, conn: &quinn::Connection) -> io::Result<(Peer, Vec<u8>)> {
        let cert = conn
            .peer_identity()
            .and_then(|i| {
                i.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
                    .ok()
            })
            .and_then(|c| c.first().map(|c| c.to_vec()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "no certificate"))?;
        let creds = self
            .enrollment
            .creds()
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "not enrolled"))?;
        let info = creds.anchor.verify(&cert)?;
        let peer = Peer {
            fingerprint: info.fingerprint,
            name: info.name,
            kind: info.kind,
            admin: info.admin,
        };
        Ok((peer, cert))
    }

    /// Keeps a connection and serves the peer's streams on it. With two connections to
    /// the same endpoint of a device, the one opened by the lower fingerprint stays
    /// (§E.12.3); connections to its other endpoints (a program's beside its IDN Node's)
    /// stay beside it.
    fn register(
        &self,
        conn: quinn::Connection,
        peer: Peer,
        cert: Vec<u8>,
        outgoing: bool,
    ) -> quinn::Connection {
        let port = conn.remote_address().port();
        let mut conns = self.conns.lock_ok();
        let list = conns.entry(peer.fingerprint.clone()).or_default();
        list.retain(|c| c.conn.close_reason().is_none());
        if let Some(i) = list.iter().position(|c| c.port == port) {
            let old = &list[i];
            let opener = |out: bool| if out { &self.my_fp } else { &peer.fingerprint };
            if opener(old.outgoing) < opener(outgoing) {
                conn.close(0u32.into(), b"duplicate");
                return old.conn.clone();
            }
            old.conn.close(0u32.into(), b"duplicate");
            list.remove(i);
        }
        if list.len() >= MAX_ENDPOINTS
            && let Some(i) = (0..list.len()).min_by_key(|&i| *list[i].last_used.lock_ok())
        {
            list.remove(i)
                .conn
                .close(0u32.into(), b"too many endpoints");
        }
        list.push(Conn {
            conn: conn.clone(),
            cert,
            outgoing,
            port,
            last_used: Arc::new(Mutex::new(Instant::now())),
        });
        conn
    }

    fn serve_connection(&self, conn: quinn::Connection, peer: Peer) {
        let weak = self.me.clone();
        let last_used = self
            .conns
            .lock_ok()
            .get(&peer.fingerprint)
            .and_then(|l| l.iter().find(|c| c.conn.stable_id() == conn.stable_id()))
            .map(|c| c.last_used.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(Instant::now())));
        let (c1, p1, w1, l1) = (conn.clone(), peer.clone(), weak.clone(), last_used.clone());
        crate::quic::runtime().spawn(async move {
            while let Ok(rs) = c1.accept_uni().await {
                *l1.lock_ok() = Instant::now();
                let (p, w) = (p1.clone(), w1.clone());
                tokio::spawn(async move {
                    let _ = on_uni(rs, p, w).await;
                });
            }
        });
        crate::quic::runtime().spawn(async move {
            while let Ok((send, recv)) = conn.accept_bi().await {
                *last_used.lock_ok() = Instant::now();
                let (p, w, c) = (peer.clone(), weak.clone(), conn.clone());
                tokio::spawn(async move {
                    let _ = on_bi(send, recv, p, w, c).await;
                });
            }
        });
    }

    /// The description of a file this device serves, with its SHA-256 (cached while the
    /// file is unchanged).
    pub(super) fn file_info(&self, id: &str, path: &Path) -> io::Result<FileInfo> {
        // Not through a link swapped in since it was offered.
        let meta = open_plain(path, std::fs::OpenOptions::new().read(true), false)?.metadata()?;
        let key = (path.to_owned(), meta.len(), meta.modified().ok());
        let cached = self.hashes.lock_ok().get(&key).cloned();
        let version = match cached {
            Some(v) => v,
            None => {
                let v = sha256_file(path, false)?;
                let mut h = self.hashes.lock_ok();
                if h.len() >= 256 {
                    h.clear();
                }
                h.insert(key, v.clone());
                v
            }
        };
        Ok(FileInfo {
            id: id.to_owned(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size: meta.len(),
            version,
        })
    }
}

/// One request over `conn` (the header and payload in `req`), its answer within `timeout`.
pub(super) fn request_on(
    conn: quinn::Connection,
    req: &[u8],
    timeout: Duration,
) -> io::Result<Reply> {
    let req = req.to_vec();
    crate::quic::runtime().block_on(async move {
        let r = tokio::time::timeout(timeout, async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(io::Error::other)?;
            send.write_all(&req).await?;
            send.finish().map_err(io::Error::other)?;
            let (status, body) = read_reply(&mut recv).await?;
            Ok::<_, io::Error>(match status {
                ST_OK => Reply::Ok(body),
                ST_UNKNOWN => Reply::Unknown,
                ST_REFUSED => Reply::Refused,
                _ => Reply::Error(String::from_utf8_lossy(&body).into_owned()),
            })
        })
        .await;
        r.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "no answer in time")))
    })
}

/// The connection closed (or was never usable): another endpoint may still answer.
pub(super) fn is_closed(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| {
        i.is::<quinn::ConnectionError>()
            || i.is::<quinn::WriteError>()
            || i.is::<quinn::ReadError>()
            || i.is::<quinn::ReadExactError>()
    })
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"closed");
        // Its endpoint out of the site's directory (bug run B).
        if let Some(p) = self.announced {
            self.enrollment.withdraw_endpoint(p);
        }
    }
}

pub(super) fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_concurrent_bidi_streams(64u32.into());
    t.max_concurrent_uni_streams(64u32.into());
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((32u32 << 20).into());
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().unwrap()));
    // A background transfer must never push live video aside (§E.12.5).
    t.congestion_controller_factory(Arc::new(LedbatConfig));
    Arc::new(t)
}

pub(super) async fn accept_loop(endpoint: quinn::Endpoint, ch: Weak<Channel>) {
    while let Some(incoming) = endpoint.accept().await {
        let ch = ch.clone();
        tokio::spawn(async move {
            let Ok(Ok(conn)) = tokio::time::timeout(CONNECT_TIMEOUT, incoming).await else {
                return;
            };
            let Some(ch) = ch.upgrade() else { return };
            // The device certificate was checked against the CA and the CRL during the
            // handshake; any enrolled device may talk to any other (§E.12.1).
            match ch.identify(&conn) {
                Ok((peer, cert)) => {
                    let kept = ch.register(conn.clone(), peer.clone(), cert, false);
                    if kept.stable_id() == conn.stable_id() {
                        ch.serve_connection(conn, peer);
                    }
                }
                Err(e) => conn.close(3u32.into(), e.to_string().as_bytes()),
            }
        });
    }
}

/// Closes connections idle for [`IDLE_CLOSE`], and those to devices a newer CRL
/// revokes (checked every second, §E.10.6).
pub(super) async fn reaper(ch: Weak<Channel>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(ch) = ch.upgrade() else { return };
        let anchor = ch.enrollment.creds().map(|c| c.anchor.clone());
        let mut conns = ch.conns.lock_ok();
        for list in conns.values_mut() {
            list.retain(|c| {
                let revoked = anchor.as_ref().is_none_or(|a| a.verify(&c.cert).is_err());
                let idle = c.last_used.lock_ok().elapsed() >= IDLE_CLOSE;
                if revoked {
                    c.conn.close(3u32.into(), b"revoked");
                } else if idle {
                    c.conn.close(0u32.into(), b"idle");
                }
                !(revoked || idle || c.conn.close_reason().is_some())
            });
        }
        conns.retain(|_, l| !l.is_empty());
    }
}
