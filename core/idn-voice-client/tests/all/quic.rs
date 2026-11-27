//! QUIC against a server of the tests' own speaking the link's framing for `idn-voice/1`
//! (docs/VOICE-CLIENT.md §12; Voice's own listener is tested in quic_server.rs): the
//! certificate pinned, a throwaway client certificate shown when asked (this fake asks;
//! Voice's listener doesn't), the `auth` frame, sound both ways (the fake sends the
//! microphone back as another talker's), and a wrong pin refused before anything is sent.
//! (A session moving from the WebSocket to it: quic_server.rs, as Voice must offer it.)

use crate::common::{Ear, block_rms, client, speak};
use idn_common::sync::LockOk as _;
use idn_voice_client::{AudioFrame, Event, Transport};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const S5: Duration = Duration::from_secs(5);

/// Takes any client certificate, recording the signature scheme it signed with.
#[derive(Debug)]
struct AnyCert {
    provider: Arc<CryptoProvider>,
    schemes: Arc<Mutex<Vec<SignatureScheme>>>,
}

impl ClientCertVerifier for AnyCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.schemes.lock_ok().push(d.scheme);
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// The fake link listener.
pub(crate) struct Fake {
    pub(crate) addr: SocketAddr,
    /// Its certificate's SHA-256, hex.
    pub(crate) pin: String,
    /// Every `auth` frame it got.
    pub(crate) auths: Arc<Mutex<Vec<Value>>>,
    schemes: Arc<Mutex<Vec<SignatureScheme>>>,
    /// Off: each sign-in is answered with an error.
    #[cfg_attr(not(feature = "quic-server-tests"), allow(dead_code))]
    pub(crate) open: Arc<AtomicBool>,
    _endpoint: quinn::Endpoint,
}

async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Option<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.ok()?;
    let len = u32::from_le_bytes(len) as usize;
    assert!((1..=65_536).contains(&len), "frame length {len}");
    let mut b = vec![0u8; len];
    r.read_exact(&mut b).await.ok()?;
    let kind = b.remove(0);
    Some((kind, b))
}

async fn write<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, payload: &[u8]) -> bool {
    let mut b = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
    b.push(kind);
    b.extend_from_slice(payload);
    w.write_all(&b).await.is_ok()
}

pub(crate) fn fake(rt: &tokio::runtime::Runtime) -> Fake {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["idn-link".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let der = cert.der().clone();
    let pin: String = ring::digest::digest(&ring::digest::SHA256, der.as_ref())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let schemes: Arc<Mutex<Vec<SignatureScheme>>> = Arc::default();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(Arc::new(AnyCert {
            provider,
            schemes: schemes.clone(),
        }))
        .with_single_cert(
            vec![der],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    tls.alpn_protocols = vec![b"idn-voice/1".to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let sc = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let endpoint = rt
        .block_on(async { quinn::Endpoint::server(sc, "127.0.0.1:0".parse().unwrap()) })
        .unwrap();
    let addr = endpoint.local_addr().unwrap();
    let auths: Arc<Mutex<Vec<Value>>> = Arc::default();
    let open = Arc::new(AtomicBool::new(true));
    // Where the link is, as the server's hello says it (§12).
    let quic = json!({"port": addr.port(), "addrs": [addr.to_string()], "cert_sha256": pin});
    let (ep, a2, o2) = (endpoint.clone(), auths.clone(), open.clone());
    rt.spawn(async move {
        while let Some(inc) = ep.accept().await {
            tokio::spawn(serve(inc, a2.clone(), o2.clone(), quic.clone()));
        }
    });
    Fake {
        addr,
        pin,
        auths,
        schemes,
        open,
        _endpoint: endpoint,
    }
}

/// One connection: `auth`, then `hello`; pings answered, talks acknowledged, the
/// microphone's packets sent back as talker 9's in channel 3.
async fn serve(
    inc: quinn::Incoming,
    auths: Arc<Mutex<Vec<Value>>>,
    open: Arc<AtomicBool>,
    quic: Value,
) {
    let Ok(conn) = inc.await else { return };
    let Ok((mut send, mut recv)) = conn.accept_bi().await else {
        return;
    };
    let Some((0, auth)) = read(&mut recv).await else {
        return;
    };
    let auth: Value = serde_json::from_slice(&auth).unwrap();
    auths.lock_ok().push(auth.clone());
    if !open.load(Ordering::SeqCst) {
        write(
            &mut send,
            0,
            br#"{"t":"error","what":"auth","error":"QUIC is off on this site: use the WebSocket"}"#,
        )
        .await;
        let _ = send.finish();
        tokio::time::sleep(Duration::from_millis(200)).await;
        return;
    }
    let hello = json!({
        "t": "hello", "sid": 7,
        "me": {"name": "Handset Truck 9", "key": "handset:h1", "user": true,
               "handset": {"id": "h1", "name": "Truck 9"}, "driver": null},
        "channels": [{"id": 3, "name": "Crew", "jitter_start_ms": 60, "jitter_max_ms": 2000}],
        "joined": [{"channel": 3, "may_talk": true}],
        "link": {"ping_s": 1, "idle_timeout_s": 30, "resume_s": 120,
                 "jitter_start_ms": 60, "jitter_max_ms": 2000, "quic": quic,
                 "link_connect_ms": 2000, "link_retry_s": 60, "link_keepalive_ms": 5000},
        "media": "websocket", "resume": "fake-token",
        "resumed": auth["resume"].is_string(),
    });
    if !write(&mut send, 0, hello.to_string().as_bytes()).await {
        return;
    }
    let mut seq = 0u32;
    while let Some((kind, b)) = read(&mut recv).await {
        let ok = match kind {
            0 => {
                let v: Value = serde_json::from_slice(&b).unwrap();
                let reply = match (v["t"].as_str(), v["on"].as_bool()) {
                    (Some("ping"), _) => json!({"t": "pong"}),
                    (Some("talk"), Some(true)) => json!({"t": "talking", "on": true}),
                    (Some("talk"), _) => json!({"t": "talking", "on": false, "why": "released"}),
                    _ => continue,
                };
                write(&mut send, 0, reply.to_string().as_bytes()).await
            }
            1 if b.first() == Some(&1) => {
                seq += 1;
                let f = AudioFrame {
                    kind: 1,
                    talker: 9,
                    channel: 3,
                    seq,
                    packet: &b[1..],
                };
                write(&mut send, 1, &f.to_bytes()).await
            }
            _ => true,
        };
        if !ok {
            break;
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn quic_first_pinned_signed_in_and_sound_both_ways() {
    let rt = runtime();
    let f = fake(&rt);
    let (c, ev) = client(
        &json!({"credential": "idnv1.h1.secret"}),
        json!({"link": [f.addr.to_string()], "link_cert_sha256": f.pin}),
    );
    ev.wait("connected", 0, S5, |e| {
        matches!(e, Event::Connected(Transport::Quic))
    });
    let (_, resumed, _) = ev.hello(0);
    assert!(!resumed);
    let auth = f.auths.lock_ok()[0].clone();
    assert_eq!(auth["t"], "auth");
    assert_eq!(auth["credential"], "idnv1.h1.secret");
    assert_eq!(auth["app"], "voice-client tests");
    assert!(auth.get("resume").is_none(), "{auth}");
    // The client signed with its throwaway certificate (the link asks every client).
    assert_eq!(f.schemes.lock_ok().as_slice(), [SignatureScheme::ED25519]);

    let ear = Ear::start(c.clone());
    c.talk_start(3u32);
    ev.wait("talk on", 0, S5, |e| {
        matches!(e, Event::Talk { on: true, .. })
    });
    speak(&c, 600);
    c.talk_stop();
    std::thread::sleep(Duration::from_millis(400));
    let rms = block_rms(&ear.samples());
    drop(ear);
    let loud = rms.iter().filter(|&&r| r > 0.1).count();
    assert!(loud >= 25, "{loud} loud blocks for 30 sent");
    assert_eq!(c.transport(), Some(Transport::Quic));
    let r = c.resume_state().unwrap();
    assert_eq!(r.token, "fake-token");
    assert!(r.have.starts_with("9:"), "{r:?}");
    c.close();
}

#[test]
fn a_wrong_pin_is_refused_before_the_credential_goes() {
    let rt = runtime();
    let f = fake(&rt);
    let (c, ev) = client(
        &json!({"credential": "idnv1.h1.secret"}),
        json!({"link": [f.addr.to_string()], "link_cert_sha256": "00".repeat(32), "transport": "quic"}),
    );
    ev.wait("reconnecting", 0, S5, |e| {
        matches!(e, Event::Reconnecting { .. })
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        f.auths.lock_ok().is_empty(),
        "the credential went to a wrong server"
    );
    assert!(!ev.all().iter().any(|e| matches!(e, Event::Connected(_))));
    assert_eq!(c.transport(), None);
    c.close();
}
