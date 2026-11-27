//! Links within a site: connections to its members, trust relayed, the site's CA.

use super::*;

/// Connects a site link to `addr` (the main site's gateway) within `timeout`.
pub fn site_connect(
    addr: SocketAddr,
    link: &LinkConfig,
    secure: &SecureConfig,
    timeout: Duration,
) -> io::Result<SiteConn> {
    let local = site_local(link, secure)?;
    let mut cfg = client_config(&local)?;
    cfg.transport_config(transport(true));
    crate::quic::runtime().block_on(async move {
        let bind: SocketAddr = if addr.is_ipv6() {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        };
        let mut ep = quinn::Endpoint::client(bind)?;
        ep.set_default_client_config(cfg);
        let connecting = ep.connect(addr, SERVER_NAME).map_err(io::Error::other)?;
        let conn = match tokio::time::timeout(timeout, connecting).await {
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("site link: no answer from UDP {addr}"),
                ));
            }
            Ok(Err(e)) => return Err(connect_error(&e)),
            Ok(Ok(c)) => c,
        };
        let c = site_conn(conn.clone(), &local)
            .inspect_err(|e| conn.close(code::REFUSED.into(), e.to_string().as_bytes()))?;
        // The endpoint lives as long as the connection.
        tokio::spawn(async move {
            conn.closed().await;
            ep.wait_idle().await;
        });
        Ok(c)
    })
}

pub(super) fn site_conn(conn: quinn::Connection, local: &Local) -> io::Result<SiteConn> {
    let (kind, cert) = negotiated(&conn, local)?;
    if kind != Kind::Site {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "site link: not site trust",
        ));
    }
    let name = local
        .site
        .as_ref()
        .and_then(|(c, _)| c.anchor.verify(&cert).ok())
        .map(|i| i.name)
        .unwrap_or_default();
    Ok(SiteConn {
        peer: conn.remote_address(),
        peer_name: name,
        peer_fingerprint: fingerprint_of(&cert),
        conn,
        cert: Arc::new(cert.to_vec()),
        enrollment: local.site.as_ref().map(|(_, e)| e.clone()),
    })
}

pub(super) fn site_local(link: &LinkConfig, secure: &SecureConfig) -> io::Result<Arc<Local>> {
    let local = Arc::new(Local::with_protocol(link, Some(secure), SITE_ALPN, true));
    if local.offers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "site link: this machine has no site certificate (enrol it with the site first)",
        ));
    }
    Ok(local)
}

/// The site CA's fingerprint, asked of the authority behind the gateway (a remote site's
/// Discovery Server announces the site under it, so its machines find it, §N.11.4).
pub fn site_ca_via(
    gateway: SocketAddr,
    link: &LinkConfig,
    timeout: Duration,
) -> io::Result<String> {
    // A local TCP pair stands in for a device: the hello goes through, the answer back.
    let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let mut dev = TcpStream::connect(l.local_addr()?)?;
    let (relay_side, _) = l.accept()?;
    let link = link.clone();
    let t = std::thread::spawn(move || {
        relay_trust_to(
            gateway,
            &link,
            relay_side,
            br#"<idn_trust version="2"/>"#.to_vec(),
            timeout,
        )
    });
    dev.set_read_timeout(Some(timeout))?;
    let answer = crate::trust::proto::recv(&mut dev)?
        .ok_or_else(|| io::Error::other("the authority didn't answer"))?;
    drop(dev);
    let _ = t.join();
    let der = answer
        .attr("ca")
        .and_then(crate::trust::proto::unb64)
        .ok_or_else(|| io::Error::other("no CA in the authority's answer"))?;
    let anchor = crate::trust::pki::Anchor::new(der)?;
    Ok(crate::trust::pki::fingerprint(&anchor.ca_der_spki()?))
}

/// Relays each stream of a trust connection to the authority at `up`, both ways.
pub(super) async fn relay_trust(conn: quinn::Connection, up: SocketAddr) {
    let from = conn.remote_address().ip().to_canonical();
    // Streams being carried now: a connection with none for 30 s is closed, its relay
    // slot freed (one kept open with no stream held a slot for good: 32 addresses took all
    // 256, and real remote machines could no longer join through the gateway; bug review
    // 3).
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    loop {
        let (mut send, mut recv) =
            match tokio::time::timeout(Duration::from_secs(30), conn.accept_bi()).await {
                Ok(Ok(s)) => s,
                Ok(Err(_)) => return,
                Err(_) if active.load(std::sync::atomic::Ordering::Relaxed) == 0 => {
                    conn.close(code::REFUSED.into(), b"idle");
                    return;
                }
                Err(_) => continue,
            };
        let Some(slot) = Relaying::take(from) else {
            crate::log::log(&format!(
                "site link: trust relay from {from} refused (too many at once)"
            ));
            let _ = send.reset(code::REFUSED.into());
            continue;
        };
        struct Carrying(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Carrying {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let carrying = Carrying(active.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let _carrying = carrying;
            // A trust connection, nothing else: the stream's first message must be the
            // trust hello (§E.10.2). Before, any bytes were piped to the Discovery Server,
            // whose whole protocol (the site's sources, registering, re-routing) was then
            // open to anyone reaching the gateway.
            let mut first = Vec::new();
            let hello = tokio::time::timeout(Duration::from_secs(5), async {
                let mut b = [0u8; 1024];
                loop {
                    let n = match recv.read(&mut b).await {
                        Ok(Some(n)) => n,
                        _ => return false,
                    };
                    first.extend_from_slice(&b[..n]);
                    if let Some(end) = first.iter().position(|&c| c == 0) {
                        return std::str::from_utf8(&first[..end])
                            .ok()
                            .and_then(crate::xml::parse)
                            .is_some_and(|e| e.name == "idn_trust");
                    }
                    if first.len() > crate::trust::proto::MAX_PLAIN {
                        return false;
                    }
                }
            })
            .await
            .unwrap_or(false);
            if !hello {
                crate::log::log(
                    "site link: a relayed stream that wasn't a trust connection, refused",
                );
                let _ = send.reset(code::REFUSED.into());
                return;
            }
            let Ok(tcp) = tokio::net::TcpStream::connect(up).await else {
                let _ = send.finish();
                return;
            };
            let (mut tr, mut tw) = tcp.into_split();
            let up_way = async {
                if tw.write_all(&first).await.is_err() {
                    return;
                }
                let _ = tokio::io::copy(&mut recv, &mut tw).await;
                let _ = tw.shutdown().await;
            };
            let down_way = async {
                let _ = tokio::io::copy(&mut tr, &mut send).await;
                let _ = send.finish();
            };
            tokio::join!(up_way, down_way);
        });
    }
}

/// Carries one trust connection (§E.10.2) whose hello has been read (`hello`, without
/// its NUL) to the site authority behind the gateway at `gateway`, until either side
/// closes: a remote site's Discovery Server relaying its machines' joining and
/// membership (X.12.2). Each connection has a QUIC connection of its own (open mode,
/// this machine's link key; the trust protocol inside authenticates).
pub fn relay_trust_to(
    gateway: SocketAddr,
    link: &LinkConfig,
    tcp: TcpStream,
    hello: Vec<u8>,
    timeout: Duration,
) -> io::Result<()> {
    let local = Arc::new(Local {
        site: None,
        pass: None,
        identity: link.identity.clone(),
        pairs: Default::default(),
        enrollment: None,
        offers: vec![(Kind::Open, TRUST_ALPN.as_bytes().to_vec())],
    });
    let cfg = client_config(&local)?;
    tcp.set_nonblocking(true)?;
    crate::quic::runtime().block_on(async move {
        let mut ep = quinn::Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?;
        ep.set_default_client_config(cfg);
        let connecting = ep.connect(gateway, SERVER_NAME).map_err(io::Error::other)?;
        let conn = match tokio::time::timeout(timeout, connecting).await {
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("site gateway {gateway}: no answer"),
                ));
            }
            Ok(Err(e)) => return Err(connect_error(&e)),
            Ok(Ok(c)) => c,
        };
        let (mut send, mut recv) = conn.open_bi().await.map_err(io::Error::other)?;
        send.write_all(&hello).await.map_err(io::Error::other)?;
        send.write_all(&[0]).await.map_err(io::Error::other)?;
        let tcp = tokio::net::TcpStream::from_std(tcp)?;
        let (mut tr, mut tw) = tcp.into_split();
        let up_way = async {
            let _ = tokio::io::copy(&mut tr, &mut send).await;
            let _ = send.finish();
        };
        let down_way = async {
            let _ = tokio::io::copy(&mut recv, &mut tw).await;
            let _ = tw.shutdown().await;
        };
        tokio::join!(up_way, down_way);
        conn.close(code::GOING_AWAY.into(), b"done");
        ep.wait_idle().await;
        Ok(())
    })
}
