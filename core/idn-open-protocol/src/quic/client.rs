//! QUIC client for the sender role (spec chapters 2 and 4), on quinn with the NDI stub
//! crypto (`quic/ndi_crypto.rs`): connects to a receiver and sends media messages on
//! two streams. quinn provides loss recovery, congestion control, pacing, keep-alives
//! and the idle timeout.

use super::ndi_crypto;
use super::server::{close_reason, stream_token};
use idn_common::sync::LockOk as _;
use std::collections::BTreeMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// No packet from the receiver for this long ends the connection (§4.4).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// A PING after this long without traffic keeps a quiet connection alive.
const KEEPALIVE: Duration = Duration::from_secs(5);
/// Bytes quinn may hold unacknowledged across streams: a few frames, so a receiver that
/// is behind makes the sender skip frames (see [`Client::unacked`]) rather than queue
/// seconds of video.
const SEND_WINDOW: u64 = 2 << 20;
/// How long the receiver may take to grant a media stream.
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().unwrap()));
    t.keep_alive_interval(Some(KEEPALIVE));
    t.send_window(SEND_WINDOW);
    // The receiver sends nothing on our streams and opens none.
    t.max_concurrent_bidi_streams(0u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    Arc::new(t)
}

struct MediaStream {
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    task: tokio::task::JoinHandle<()>,
}

/// A connection to one receiver.
pub struct Client {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    streams: BTreeMap<u64, MediaStream>,
    /// Bytes queued with `send_message` and not yet taken by quinn.
    pending: Arc<AtomicU64>,
    /// Why a stream write failed, if one did.
    failed: Arc<Mutex<Option<String>>>,
}

impl Client {
    /// Connects to `peer` and completes the stub handshake within `timeout`.
    pub fn connect(peer: SocketAddr, timeout: Duration) -> io::Result<Self> {
        let sock = UdpSocket::bind(if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        let _ = socket2::SockRef::from(&sock).set_send_buffer_size(8 << 20);
        let mut ep = quinn::EndpointConfig::default();
        // The client's source connection ID is empty (§4.1).
        ep.cid_generator(|| Box::new(quinn_proto::RandomConnectionIdGenerator::new(0)));
        ep.grease_quic_bit(false);
        let endpoint = {
            let _g = super::runtime().enter();
            quinn::Endpoint::new(ep, None, sock, Arc::new(quinn::TokioRuntime))?
        };
        let mut cc = quinn::ClientConfig::new(Arc::new(ndi_crypto::ClientConfig));
        // An 8-byte original destination connection ID (§4.1).
        cc.initial_dst_cid_provider(Arc::new(|| {
            quinn_proto::ConnectionId::new(&crate::random::secure_random::<8>())
        }));
        cc.transport_config(transport());
        // The server name is the receiver's address as text (§4.3.1). (Inside the
        // runtime: this spawns the connection's driver.)
        let connecting = {
            let _g = super::runtime().enter();
            endpoint
                .connect_with(cc, peer, &peer.ip().to_string())
                .map_err(io::Error::other)?
        };
        let conn = super::runtime().block_on(async {
            match tokio::time::timeout(timeout, connecting).await {
                Ok(Ok(c)) => Ok(c),
                Ok(Err(e)) => Err(io::Error::other(close_reason(&e))),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "QUIC handshake timed out",
                )),
            }
        })?;
        Ok(Client {
            endpoint,
            conn,
            streams: BTreeMap::new(),
            pending: Arc::default(),
            failed: Arc::default(),
        })
    }

    /// Opens a media stream: stream id `4·n` (client-initiated bidirectional; open video
    /// before audio) carrying the token for `quic_id` (§2.3.1).
    pub fn open_media_stream(&mut self, n: u64, quic_id: u32) -> u64 {
        let id = 4 * n;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (conn, pending, failed) =
            (self.conn.clone(), self.pending.clone(), self.failed.clone());
        // Streams are opened here, in call order, so that video gets id 0 and audio 4. A
        // receiver that grants no streams fails the connection (see `closed`) instead of
        // blocking the caller forever.
        let opened = super::runtime()
            .block_on(async { tokio::time::timeout(STREAM_OPEN_TIMEOUT, conn.open_bi()).await });
        let task = super::runtime().spawn(async move {
            let fail = |e: String| *failed.lock_ok() = Some(e);
            let (mut send, _recv) = match opened {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return fail(close_reason(&e)),
                Err(_) => return fail("the receiver granted no media stream".into()),
            };
            if let Err(e) = send.write_all(&stream_token(quic_id)).await {
                return fail(e.to_string());
            }
            while let Some(m) = rx.recv().await {
                let r = send.write_all(&m).await;
                pending.fetch_sub(m.len() as u64, Ordering::Relaxed);
                if let Err(e) = r {
                    return fail(e.to_string());
                }
            }
            // Closed by `close`: finish the stream and give the data a moment to arrive.
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        });
        self.streams.insert(id, MediaStream { tx, task });
        id
    }

    /// Queues one media message (`msg_len` prefix + frame bytes, §2.3.2) on a stream.
    pub fn send_message(&mut self, stream: u64, frame_bytes: &[u8]) {
        let Some(s) = self.streams.get(&stream) else {
            return;
        };
        let mut m = Vec::with_capacity(4 + frame_bytes.len());
        m.extend_from_slice(&(frame_bytes.len() as u32).to_le_bytes());
        m.extend_from_slice(frame_bytes);
        let len = m.len() as u64;
        self.pending.fetch_add(len, Ordering::Relaxed);
        if s.tx.send(m).is_err() {
            self.pending.fetch_sub(len, Ordering::Relaxed);
        }
    }

    /// Internal state, for diagnosing stalls in tests.
    #[doc(hidden)]
    pub fn debug_state(&self) -> String {
        let s = self.conn.stats();
        format!(
            "pending {} rtt {:?} cwnd {} lost {} sent {}",
            self.unacked(),
            s.path.rtt,
            s.path.cwnd,
            s.path.lost_packets,
            s.udp_tx.bytes
        )
    }

    /// Bytes queued but not yet taken by quinn (which holds at most `SEND_WINDOW` more
    /// unacknowledged). Senders skip new frames while this is large.
    pub fn unacked(&self) -> u64 {
        self.pending.load(Ordering::Relaxed)
    }

    /// Why the connection ended ("idle timeout" when the receiver went silent).
    pub fn closed(&self) -> Option<String> {
        if let Some(e) = self.conn.close_reason() {
            return Some(close_reason(&e));
        }
        self.failed.lock_ok().clone()
    }

    /// Lets time pass (quinn runs on its own threads). Kept for callers that poll.
    pub fn poll_once(&mut self) -> io::Result<()> {
        std::thread::sleep(Duration::from_millis(2));
        Ok(())
    }

    /// Sends what is queued, then closes the connection (error 0).
    pub fn close(&mut self) {
        let tasks: Vec<_> = std::mem::take(&mut self.streams)
            .into_values()
            .map(|s| s.task)
            .collect();
        super::runtime().block_on(async {
            for t in tasks {
                let _ = tokio::time::timeout(Duration::from_secs(3), t).await;
            }
        });
        self.conn.close(0u32.into(), b"");
        let ep = self.endpoint.clone();
        super::runtime().block_on(async {
            let _ = tokio::time::timeout(Duration::from_millis(200), ep.wait_idle()).await;
        });
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receiver_that_grants_no_stream_fails_the_connection() {
        // A receiver with the NDI profile that allows no bidirectional streams.
        let mut t = quinn::TransportConfig::default();
        t.max_concurrent_bidi_streams(0u32.into());
        let mut sc = quinn::ServerConfig::with_crypto(Arc::new(ndi_crypto::ServerConfig));
        sc.transport_config(Arc::new(t));
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = {
            let _g = super::super::runtime().enter();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(sc),
                sock,
                Arc::new(quinn::TokioRuntime),
            )
            .unwrap()
        };
        let ep = server.clone();
        super::super::runtime().spawn(async move {
            while let Some(i) = ep.accept().await {
                tokio::spawn(async move {
                    if let Ok(c) = i.await {
                        let _ = c.closed().await;
                    }
                });
            }
        });
        let mut c = Client::connect(server.local_addr().unwrap(), Duration::from_secs(5)).unwrap();
        let t0 = std::time::Instant::now();
        c.open_media_stream(0, 1);
        let end = t0 + Duration::from_secs(10);
        while c.closed().is_none() && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            c.closed().as_deref(),
            Some("the receiver granted no media stream")
        );
        assert!(t0.elapsed() < Duration::from_secs(7), "{:?}", t0.elapsed());
    }
}
