//! The site link's listening end: handshakes and trust relays counted, the main site's
//! listener, and messages on a site connection's streams.

use super::*;

impl SiteHandshake {
    pub(super) fn start(
        busy: &Arc<AtomicUsize>,
        per_addr: &Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>>,
        ip: std::net::IpAddr,
    ) -> Option<Self> {
        Self::within(
            busy,
            per_addr,
            ip,
            MAX_HANDSHAKES,
            SITE_HANDSHAKES_PER_ADDRESS,
        )
    }

    /// Counted against `total` in all and `each` per address (also live trust relays).
    fn within(
        busy: &Arc<AtomicUsize>,
        per_addr: &Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>>,
        ip: std::net::IpAddr,
        total: usize,
        each: usize,
    ) -> Option<Self> {
        let mut m = per_addr.lock_ok();
        let mine = m.get(&ip).copied().unwrap_or(0);
        if busy.load(Ordering::Relaxed) >= total || mine >= each {
            return None;
        }
        m.insert(ip, mine + 1);
        busy.fetch_add(1, Ordering::Relaxed);
        Some(SiteHandshake {
            busy: busy.clone(),
            per_addr: per_addr.clone(),
            ip,
        })
    }
}

impl Drop for SiteHandshake {
    fn drop(&mut self) {
        self.busy.fetch_sub(1, Ordering::Relaxed);
        let mut m = self.per_addr.lock_ok();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

impl Relaying {
    pub(super) fn take(ip: std::net::IpAddr) -> Option<Relaying> {
        let mut m = RELAYING.lock_ok();
        let total: usize = m.values().sum();
        let mine = m.get(&ip).copied().unwrap_or(0);
        if total >= RELAYS || mine >= RELAYS_PER_ADDRESS {
            return None;
        }
        m.insert(ip, mine + 1);
        Some(Relaying(ip))
    }
}

impl Drop for Relaying {
    fn drop(&mut self) {
        let mut m = RELAYING.lock_ok();
        if let Some(n) = m.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

impl SiteListener {
    /// Listens on UDP `port` (0: any); each authenticated link goes to `on_accept`, on a
    /// runtime thread (hand it over).
    ///
    /// With `trust_upstream` (the site authority's Discovery Server), it also takes
    /// `idn-trust/1` connections, which need no site certificate (an open link, X.12.2):
    /// each of their streams is relayed byte for byte to the authority as one trust
    /// connection (§E.10.2), for remote machines joining or keeping their membership.
    pub fn bind(
        port: u16,
        link: LinkConfig,
        secure: SecureConfig,
        trust_upstream: Option<SocketAddr>,
        on_accept: impl Fn(SiteConn) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
        let _ = socket2::SockRef::from(&sock).set_recv_buffer_size(8 << 20);
        let _ = socket2::SockRef::from(&sock).set_send_buffer_size(8 << 20);
        let port = sock.local_addr()?.port();
        // The site link, and the trust relay where there is one.
        let site_local =
            move |link: &LinkConfig, secure: &SecureConfig| -> io::Result<Arc<Local>> {
                site_local(link, secure)?;
                let mut l = Local::with_protocol(link, Some(secure), SITE_ALPN, true);
                if trust_upstream.is_some() {
                    l.offers.push((Kind::Open, TRUST_ALPN.as_bytes().to_vec()));
                }
                Ok(Arc::new(l))
            };
        let first = site_local(&link, &secure)?;
        let endpoint = {
            let _g = crate::quic::runtime().enter();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(site_server_config(&first)?),
                sock,
                Arc::new(quinn::TokioRuntime),
            )?
        };
        let ep = endpoint.clone();
        let on_accept = Arc::new(on_accept);
        let busy = Arc::new(AtomicUsize::new(0));
        let per_addr: Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>> = Arc::default();
        // Live trust relays (open mode), counted as handshakes are: one client opening
        // one after another held thousands.
        let relays = Arc::new(AtomicUsize::new(0));
        let relays_per_addr: Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>> = Arc::default();
        crate::quic::runtime().spawn(async move {
            while let Some(incoming) = ep.accept().await {
                // Reached from the internet: the address proven first (a Retry, one round
                // trip), so a flood from forged addresses costs one packet each; then at
                // most MAX_HANDSHAKES at once, SITE_HANDSHAKES_PER_ADDRESS from one address.
                if !incoming.remote_address_validated() && incoming.may_retry() {
                    let _ = incoming.retry();
                    continue;
                }
                let Some(handshake) =
                    SiteHandshake::start(&busy, &per_addr, incoming.remote_address().ip())
                else {
                    incoming.refuse();
                    continue;
                };
                // Credentials read again for each: a renewed certificate or CRL counts.
                let Ok(local) = site_local(&link, &secure) else {
                    incoming.refuse();
                    continue;
                };
                let Ok(cfg) = site_server_config(&local) else {
                    incoming.refuse();
                    continue;
                };
                let on_accept = on_accept.clone();
                let (relays, relays_per_addr) = (relays.clone(), relays_per_addr.clone());
                tokio::spawn(async move {
                    let peer = incoming.remote_address();
                    let r = async {
                        let connecting = incoming
                            .accept_with(Arc::new(cfg))
                            .map_err(io::Error::other)?;
                        let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
                            .await
                            .map_err(|_| io::Error::other("handshake took too long"))?
                            .map_err(io::Error::other)?;
                        drop(handshake);
                        // A trust relay: nothing but trust connections to the authority.
                        if let Some(up) = trust_upstream
                            && negotiated(&conn, &local).is_ok_and(|(k, _)| k == Kind::Open)
                        {
                            return Ok(Err((conn, up)));
                        }
                        site_conn(conn.clone(), &local).map(Ok).inspect_err(|e| {
                            conn.close(code::REFUSED.into(), e.to_string().as_bytes())
                        })
                    }
                    .await;
                    match r {
                        Ok(Ok(c)) => on_accept(c),
                        Ok(Err((conn, up))) => {
                            match SiteHandshake::within(
                                &relays,
                                &relays_per_addr,
                                peer.ip(),
                                MAX_TRUST_RELAYS,
                                SITE_HANDSHAKES_PER_ADDRESS,
                            ) {
                                Some(_held) => relay_trust(conn, up).await,
                                None => conn.close(code::REFUSED.into(), b"too many"),
                            }
                        }
                        Err(e) => crate::log::log(&format!("site link from {peer}: {e}")),
                    }
                });
            }
        });
        Ok(SiteListener { endpoint, port })
    }

    /// The UDP port listened on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for SiteListener {
    fn drop(&mut self) {
        self.endpoint.close(code::GOING_AWAY.into(), b"closed");
    }
}

impl SiteConn {
    /// Opens a stream both ways.
    pub fn open_bi(&self) -> io::Result<(SiteSend, SiteRecv)> {
        let c = self.conn.clone();
        crate::quic::runtime()
            .block_on(async move { c.open_bi().await })
            .map(|(s, r)| (SiteSend(s), SiteRecv(r)))
            .map_err(io::Error::other)
    }

    /// The next stream the peer opened both ways.
    pub fn accept_bi(&self) -> io::Result<(SiteSend, SiteRecv)> {
        let c = self.conn.clone();
        crate::quic::runtime()
            .block_on(async move { c.accept_bi().await })
            .map(|(s, r)| (SiteSend(s), SiteRecv(r)))
            .map_err(io::Error::other)
    }

    /// Opens a stream one way (to the peer).
    pub fn open_uni(&self) -> io::Result<SiteSend> {
        let c = self.conn.clone();
        crate::quic::runtime()
            .block_on(async move { c.open_uni().await })
            .map(SiteSend)
            .map_err(io::Error::other)
    }

    /// The next stream the peer opened one way.
    pub fn accept_uni(&self) -> io::Result<SiteRecv> {
        let c = self.conn.clone();
        crate::quic::runtime()
            .block_on(async move { c.accept_uni().await })
            .map(SiteRecv)
            .map_err(io::Error::other)
    }

    /// The round trip now.
    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    /// Bytes sent and lost so far (for the bandwidth in use and the loss).
    pub fn stats(&self) -> (u64, u64) {
        let s = self.conn.stats();
        (s.udp_tx.bytes, s.path.lost_bytes)
    }

    /// Why it closed, once it has.
    pub fn closed(&self) -> Option<String> {
        self.conn.close_reason().map(|r| r.to_string())
    }

    /// The peer's certificate is no longer good: revoked in the site's current CRL, or
    /// this machine no longer has site credentials. Checked now and then by the link, so
    /// revoking a machine in the Site Manager cuts its link (X.2).
    pub fn revoked(&self) -> bool {
        let Some(e) = &self.enrollment else {
            return false;
        };
        e.creds()
            .is_none_or(|c| c.anchor.verify(&self.cert).is_err())
    }

    /// Closes the link, telling the peer why.
    pub fn close(&self, reason: &str) {
        self.conn.close(code::GOING_AWAY.into(), reason.as_bytes());
    }
}

impl SiteSend {
    /// Sends one message (blocks while the link is full: the stream's back pressure).
    pub fn send(&mut self, msg: &[u8]) -> io::Result<()> {
        let s = &mut self.0;
        crate::quic::runtime().block_on(async move {
            s.write_all(&(msg.len() as u32).to_le_bytes())
                .await
                .map_err(io::Error::other)?;
            s.write_all(msg).await.map_err(io::Error::other)
        })
    }

    /// Ends the stream (the peer reads its end).
    pub fn finish(&mut self) {
        let _ = self.0.finish();
    }
}

impl SiteRecv {
    /// The next message; `None` at the stream's end.
    pub fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        let r = &mut self.0;
        crate::quic::runtime().block_on(async move {
            let mut len = [0u8; 4];
            match r.read_exact(&mut len).await {
                Ok(()) => {}
                Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
                Err(e) => return Err(io::Error::other(e)),
            }
            let n = u32::from_le_bytes(len) as usize;
            if n > SITE_MAX_MESSAGE {
                return Err(io::Error::other(format!("site link: message of {n} bytes")));
            }
            let buf = crate::quic::read_len(r, n)
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ended"))?;
            Ok(Some(buf))
        })
    }
}
