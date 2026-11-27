//! Sound over WebRTC (§V.8): one UDP port for every client (ICE-lite, DTLS-SRTP), one
//! thread driving each client's connection. A client's first audio track is its
//! microphone (its packets go to the hub while it talks); the others are its talker slots,
//! written with each talker's packets as they came.

use crate::hub::{Hub, RtcSink, SLOTS, Sid};
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

/// Clients over WebRTC at once.
const MAX_CLIENTS: usize = 500;

enum Cmd {
    Offer {
        sid: Sid,
        sdp: String,
        peer: IpAddr,
        reply: SyncSender<Result<String, String>>,
    },
    Forget(Sid),
    Send {
        sid: Sid,
        slot: usize,
        data: Vec<u8>,
    },
}

/// How the rest of Voice reaches the WebRTC thread.
#[derive(Clone)]
pub struct RtcHandle {
    tx: SyncSender<Cmd>,
    pub port: u16,
}

impl RtcHandle {
    /// A client's SDP offer: the answer (its connection then made on this port).
    pub fn offer(&self, sid: Sid, sdp: String, peer: IpAddr) -> Result<String, String> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(Cmd::Offer {
                sid,
                sdp,
                peer,
                reply,
            })
            .map_err(|_| "WebRTC stopped".to_owned())?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "WebRTC didn't answer".to_owned())?
    }
}

struct Sink(SyncSender<Cmd>);

impl RtcSink for Sink {
    fn send(&self, sid: Sid, slot: usize, data: &[u8]) {
        // Dropped when the WebRTC thread is behind: the hand-off was unbounded, and a
        // stalled thread grew the Site Manager without limit (bug round E, E5-2).
        let _ = self.0.try_send(Cmd::Send {
            sid,
            slot,
            data: data.to_vec(),
        });
    }

    fn forget(&self, sid: Sid) {
        let _ = self.0.send(Cmd::Forget(sid));
    }
}

struct Client {
    sid: Sid,
    rtc: Rtc,
    /// Audio media in the offer's order: the first the microphone, the rest the slots.
    mids: Vec<Mid>,
    pt: Option<Pt>,
    connected: bool,
    created: Instant,
    timeout: Instant,
    /// The address its candidate gave it: the destination of its packets.
    local: SocketAddr,
}

impl Client {
    fn mic(&self) -> Option<Mid> {
        self.mids.first().copied()
    }

    fn slot(&self, k: usize) -> Option<Mid> {
        self.mids.get(1 + k).copied()
    }
}

/// The address a client is told to send to: `public`, else this machine's address
/// towards the client.
fn local_towards(peer: IpAddr) -> io::Result<IpAddr> {
    use idn_common::net;
    let ip = net::local_addr_towards(peer)
        .ok_or_else(|| idn_common::err::other(format_args!("no route to {peer}")))?;
    // Firefox won't use 127.0.0.1 for WebRTC: the machine's own address instead.
    Ok(if ip.is_loopback() {
        net::outgoing_ip().unwrap_or(ip)
    } else {
        ip
    })
}

/// Binds `addr` and starts the thread.
pub fn start(addr: SocketAddr, public: Option<IpAddr>, hub: Arc<Hub>) -> io::Result<RtcHandle> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
    let socket = UdpSocket::bind(addr)?;
    let port = socket.local_addr()?.port();
    let (tx, rx) = sync_channel(4096);
    hub.set_rtc(Arc::new(Sink(tx.clone())));
    let weak = Arc::downgrade(&hub);
    std::thread::Builder::new()
        .name("idn-voice-rtc".into())
        .spawn(move || run(socket, rx, weak, public))?;
    Ok(RtcHandle { tx, port })
}

fn run(socket: UdpSocket, rx: Receiver<Cmd>, weak: std::sync::Weak<Hub>, public: Option<IpAddr>) {
    let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    let mut clients: Vec<Client> = Vec::new();
    let mut buf = vec![0u8; 2000];
    let epoch = Instant::now();
    loop {
        // Voice let go (its Site Manager stopped): the port too.
        let Some(hub) = weak.upgrade() else { return };
        // Commands: offers, sound to send, clients gone.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Offer {
                    sid,
                    sdp,
                    peer,
                    reply,
                }) => {
                    clients.retain(|c| c.sid != sid);
                    let r = if clients.len() >= MAX_CLIENTS {
                        Err("too many WebRTC clients".into())
                    } else {
                        accept(
                            sid,
                            &sdp,
                            public.map_or_else(|| local_towards(peer), Ok),
                            port,
                        )
                        .map(|(c, answer)| {
                            clients.push(c);
                            answer
                        })
                    };
                    let _ = reply.send(r);
                }
                Ok(Cmd::Forget(sid)) => clients.retain(|c| c.sid != sid),
                Ok(Cmd::Send { sid, slot, data }) => {
                    if let Some(c) = clients.iter_mut().find(|c| c.sid == sid && c.connected)
                        && let (Some(mid), Some(pt)) = (c.slot(slot), c.pt)
                        && let Some(w) = c.rtc.writer(mid)
                    {
                        let t = MediaTime::new(
                            epoch.elapsed().as_micros() as u64 * 48 / 1000,
                            Frequency::FORTY_EIGHT_KHZ,
                        );
                        let _ = w.write(pt, Instant::now(), t, data);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        // Each client's output: packets to send, events, its next timeout.
        let mut gone = Vec::new();
        for c in clients.iter_mut() {
            loop {
                match c.rtc.poll_output() {
                    Ok(Output::Timeout(t)) => {
                        c.timeout = t;
                        break;
                    }
                    Ok(Output::Transmit(t)) => {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(e)) => match e {
                        Event::Connected => {
                            c.connected = true;
                            hub.set_rtc_media(c.sid, true);
                        }
                        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                            gone.push(c.sid);
                            break;
                        }
                        Event::MediaAdded(m) if m.kind == MediaKind::Audio => {
                            if c.mids.len() <= SLOTS {
                                c.mids.push(m.mid);
                            }
                            if c.pt.is_none()
                                && m.direction != Direction::Inactive
                                && let Some(w) = c.rtc.writer(m.mid)
                            {
                                c.pt = w
                                    .payload_params()
                                    .find(|p| p.spec().codec == Codec::Opus)
                                    .map(|p| p.pt());
                            }
                        }
                        Event::MediaData(d) if Some(d.mid) == c.mic() => {
                            hub.media_in(c.sid, &d.data);
                        }
                        _ => {}
                    },
                    Err(_) => {
                        gone.push(c.sid);
                        break;
                    }
                }
            }
            if !c.connected && c.created.elapsed() > Duration::from_secs(20) {
                gone.push(c.sid);
            }
        }
        for sid in gone {
            clients.retain(|c| c.sid != sid);
            hub.set_rtc_media(sid, false);
        }
        // Wait for a packet until the soonest timeout (at most 5 ms: sound to send).
        let now = Instant::now();
        let soonest = clients
            .iter()
            .map(|c| c.timeout)
            .min()
            .unwrap_or(now + Duration::from_millis(5));
        let wait = soonest
            .saturating_duration_since(now)
            .clamp(Duration::from_millis(1), Duration::from_millis(5));
        let _ = socket.set_read_timeout(Some(wait));
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                // The client it is for: each checked with its own address as the
                // destination (the socket is on every address).
                let now = Instant::now();
                for c in clients.iter_mut() {
                    let Ok(contents) = buf[..n].try_into() else {
                        break;
                    };
                    let input = Input::Receive(
                        now,
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: c.local,
                            contents,
                        },
                    );
                    if c.rtc.accepts(&input) {
                        let _ = c.rtc.handle_input(input);
                        break;
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
        let now = Instant::now();
        for c in clients.iter_mut().filter(|c| c.timeout <= now) {
            let _ = c.rtc.handle_input(Input::Timeout(now));
        }
    }
}

/// A client's connection from its offer: the client and the SDP answer.
fn accept(
    sid: Sid,
    sdp: &str,
    ip: io::Result<IpAddr>,
    port: u16,
) -> Result<(Client, String), String> {
    let ip = ip.map_err(|e| e.to_string())?;
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|e| format!("offer: {e}"))?;
    let mut rtc = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    let local = SocketAddr::new(ip, port);
    rtc.add_local_candidate(Candidate::host(local, "udp").map_err(|e| e.to_string())?);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| format!("offer: {e}"))?;
    Ok((
        Client {
            sid,
            rtc,
            mids: Vec::new(),
            pt: None,
            connected: false,
            created: Instant::now(),
            timeout: Instant::now(),
            local,
        },
        answer.to_sdp_string(),
    ))
}
