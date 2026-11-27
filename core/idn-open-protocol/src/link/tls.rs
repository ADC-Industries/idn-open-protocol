//! A link's TLS and QUIC setup: configurations, the transport, what was negotiated,
//! errors explained.

use super::*;

pub(super) fn server_config(local: &Arc<Local>) -> io::Result<quinn::ServerConfig> {
    // One configuration per connection: the resolver's choice feeds the request's hint.
    let provider = crate::secure::crypto_provider();
    let chosen: Arc<Mutex<Option<Kind>>> = Arc::default();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_client_cert_verifier(Arc::new(ClientVerifier {
            inner: Verifier {
                local: local.clone(),
                algs: provider.signature_verification_algorithms,
            },
            chosen: chosen.clone(),
            hints: KINDS.map(|k| vec![mode_hint(k)]),
        }))
        .with_cert_resolver(Arc::new(ServerCert {
            local: local.clone(),
            provider: provider.clone(),
            chosen,
        }));
    tls.alpn_protocols = local.offers.iter().map(|(_, a)| a.clone()).collect();
    tls.max_early_data_size = 0;
    tls.send_tls13_tickets = 0;
    let crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut c = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    c.transport_config(transport(false));
    c.migration(true);
    Ok(c)
}

pub(super) fn client_config(local: &Arc<Local>) -> io::Result<quinn::ClientConfig> {
    let provider = crate::secure::crypto_provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier {
            local: local.clone(),
            algs: provider.signature_verification_algorithms,
        }))
        .with_client_cert_resolver(Arc::new(ClientCert {
            local: local.clone(),
            provider: provider.clone(),
        }));
    tls.alpn_protocols = local.offers.iter().map(|(_, a)| a.clone()).collect();
    // No 0-RTT (a replayed control message could repeat a command), and no resumption
    // for now: a full handshake costs one round trip on a LAN.
    tls.enable_early_data = false;
    tls.resumption = rustls::client::Resumption::disabled();
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(io::Error::other)?;
    let mut c = quinn::ClientConfig::new(Arc::new(crypto));
    c.transport_config(transport(false));
    Ok(c)
}

/// A link's QUIC transport; `site`: a site link over the internet (X.12), BBR.
pub(super) fn transport(site: bool) -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.stream_receive_window((16u32 << 20).into());
    t.receive_window((32u32 << 20).into());
    t.send_window(SEND_WINDOW);
    // The receiver opens the control stream (a few more for later uses, R.7); the
    // sender opens the media streams.
    t.max_concurrent_bidi_streams(4u32.into());
    // Per-frame streams (§E.13) keep a few frames in flight besides the audio stream.
    t.max_concurrent_uni_streams(32u32.into());
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().unwrap()));
    // Congestion control: BBR on site links, where random loss over a real round trip
    // made CUBIC take it for congestion (X.12.9: 2% loss at 40 ms, 5 pictures a second
    // against BBR's 30); quinn's default (CUBIC) on the LAN. IDN_LINK_CC=cubic|bbr|newreno
    // overrides.
    let cc = std::env::var("IDN_LINK_CC").ok().filter(|v| !v.is_empty());
    match cc.as_deref().unwrap_or(if site { "bbr" } else { "cubic" }) {
        "bbr" => {
            t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
        }
        "newreno" => {
            t.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default()));
        }
        _ => {}
    }
    Arc::new(t)
}

/// The negotiated mode and the peer's certificate, checked against each other: the
/// certificate must belong to the mode the ALPN names (§E.11.4).
pub(super) fn negotiated(conn: &quinn::Connection, local: &Local) -> io::Result<(Kind, Vec<u8>)> {
    let deny = |m: &str| io::Error::new(io::ErrorKind::PermissionDenied, m.to_owned());
    let alpn = conn
        .handshake_data()
        .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|d| d.protocol)
        .ok_or_else(|| deny("no mode negotiated"))?;
    let kind = local
        .offers
        .iter()
        .find(|(_, a)| *a == alpn)
        .map(|(k, _)| *k)
        .ok_or_else(|| deny("the peer chose a mode this device did not offer"))?;
    let cert = conn
        .peer_identity()
        .and_then(|i| i.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|c| c.first().map(|c| c.to_vec()))
        .ok_or_else(|| deny("no peer certificate"))?;
    let fits = match kind {
        Kind::Pair => local.linked(&cert, Some(&alpn)),
        k => local.classify(&cert) == Some(k),
    };
    if !fits {
        return Err(deny(
            "the peer's certificate does not match the negotiated mode",
        ));
    }
    Ok((kind, cert))
}

pub(super) fn fingerprint_of(cert: &[u8]) -> String {
    crate::secure::ed25519_public_key(cert)
        .map(|k| crate::trust::pki::fingerprint(&crate::trust::pki::spki(&k)))
        .unwrap_or_default()
}

/// The mode, as the server names it in its certificate request (the
/// `certificate_authorities` hint): the client is asked for its certificate before it
/// has seen the server's, so this is how it knows which one to send.
pub(super) fn mode_hint(k: Kind) -> DistinguishedName {
    let cn = match k {
        Kind::Site => "idn-link s2",
        Kind::Pair => "idn-link pair",
        Kind::Passphrase => "idn-link s1",
        Kind::Open => "idn-link open",
    }
    .as_bytes();
    // Name ::= SEQUENCE { SET { SEQUENCE { id-at-commonName, UTF8String } } }
    let atv = [
        &[0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, cn.len() as u8][..],
        cn,
    ]
    .concat();
    let set = [&[0x30, atv.len() as u8][..], &atv].concat();
    let rdn = [&[0x31, set.len() as u8][..], &set].concat();
    DistinguishedName::from([&[0x30, rdn.len() as u8][..], &rdn].concat())
}

/// How a failed connection attempt should be treated (§E.11.8).
pub(super) fn connect_error(e: &quinn::ConnectionError) -> io::Error {
    use quinn::ConnectionError as C;
    // TLS alerts: no_application_protocol (120) means no common mode; certificate alerts
    // (42–49) a peer that failed authentication.
    let alert = |code: quinn::TransportErrorCode| -> Option<u8> {
        let c = u64::from(code);
        (0x100..0x200).contains(&c).then(|| (c - 0x100) as u8)
    };
    let kind = match e {
        C::TimedOut => io::ErrorKind::TimedOut,
        C::TransportError(t) if alert(t.code).is_some_and(auth_alert) => {
            io::ErrorKind::PermissionDenied
        }
        C::ConnectionClosed(c) if alert(c.error_code).is_some_and(auth_alert) => {
            io::ErrorKind::PermissionDenied
        }
        C::ApplicationClosed(c) if u64::from(c.error_code) == u64::from(code::REFUSED) => {
            io::ErrorKind::PermissionDenied
        }
        _ => io::ErrorKind::Other,
    };
    // The Node refused to sign for this program (it waits for approval, or is blocked):
    // its reason as it is, not inside a TLS error.
    let text = e.to_string();
    if let Some(i) = text.find(crate::trust::pki::NODE_SIGNING) {
        let reason = text[i + crate::trust::pki::NODE_SIGNING.len()..].trim_start_matches(": ");
        return io::Error::new(io::ErrorKind::PermissionDenied, reason.to_owned());
    }
    io::Error::new(kind, format!("link: {e}"))
}
