//! Encrypted multicast (`mcast/1`, spec/extensions/idn-extensions.md §E.17): the §6.4
//! multicast stream, each packet sealed with ChaCha20-Poly1305 under a group key that
//! travels only inside receivers' links, and each message signed by the output.
use idn_common::sync::LockOk as _;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The token a sender in encrypted mode lists on link sessions (§E.17.2).
pub const TOKEN: &str = "mcast/1";
const MAGIC: u8 = 0xE1;
/// Sealed datagram header: magic, stream, key number, packet number (§E.17.3).
pub const HEADER: usize = 12;
const TAG: usize = 16;
/// Bytes a sealed datagram adds to a §6.3.3 packet.
pub const OVERHEAD: usize = HEADER + TAG;
/// The message number after each message (§E.17.4).
pub const NUMBER: usize = 8;
/// The Ed25519 signature after the number, when signing.
pub const SIGNATURE: usize = 64;
/// Messages kept for re-sending (§E.17.6): this long, and at most this many bytes.
pub const HISTORY: Duration = Duration::from_secs(1);
const HISTORY_BYTES: usize = 48 << 20;
/// The most messages one re-send request asks for; a longer gap is given up.
pub const MAX_GAP: u64 = 64;
/// Internal frame kind carrying a verified-later multicast message to the input's worker:
/// payload = stream byte, then the message with its trailer.
#[doc(hidden)]
pub const MESSAGE_KIND: u16 = 0x7ff0;
const CONTEXT: &[u8] = b"idn-mcast/1";
/// A new key is used this long after it is sent (§E.17.5).
pub const SWITCH_DELAY: Duration = Duration::from_secs(1);
/// Keys a receiver keeps.
const KEPT: usize = 3;
/// Streams: video, low video, audio.
pub const STREAMS: usize = 3;

/// Who gets multicast from an output that sends it (§E.17.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every receiver that asks gets plain multicast, as in NDI.
    #[default]
    Plain,
    /// IDN receivers on a link get encrypted multicast; every other receiver gets
    /// unicast, so no plain copy is on the network.
    Encrypted,
    /// IDN receivers on a link get their own encrypted link streams; NDI receivers get
    /// plain multicast.
    PerReceiver,
}

/// An output's multicast settings (§E.17.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McConfig {
    /// Who gets multicast. Default: [`Mode::Plain`].
    pub mode: Mode,
    /// Sign each message, so one receiver cannot forge frames for the others. On by
    /// default; a site that trusts all its receivers may turn it off.
    pub signatures: bool,
    /// How often the group key changes (it also changes whenever a receiver leaves).
    pub key_interval: Duration,
    /// Tests: leave every n-th message off the network (still re-sendable). 0: none.
    #[doc(hidden)]
    pub test_drop_every: u64,
}

impl Default for McConfig {
    fn default() -> Self {
        McConfig {
            mode: Mode::Plain,
            signatures: true,
            key_interval: Duration::from_secs(30 * 60),
            test_drop_every: 0,
        }
    }
}

/// The stream byte of an announced stream type (§E.17.3).
pub fn stream_of(kind: &str) -> Option<u8> {
    match kind {
        "vmchq" => Some(0),
        "vmclq" => Some(1),
        "amc" => Some(2),
        _ => None,
    }
}

fn aead(raw: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, raw).expect("32-byte key"))
}

fn nonce(stream: u8, pn: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = stream;
    n[4..].copy_from_slice(&pn.to_le_bytes());
    Nonce::assume_unique_for_key(n)
}

/// What is signed: SHA-256 of the context, stream, message number and message.
fn signed_digest(stream: u8, number: u64, msg: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(CONTEXT);
    h.update([stream]);
    h.update(number.to_le_bytes());
    h.update(msg);
    h.finalize().into()
}

struct Key {
    n: u16,
    raw: [u8; 32],
    key: LessSafeKey,
    packets: [u64; STREAMS],
}

impl Key {
    fn new(n: u16) -> Self {
        let raw = crate::random::secure_random::<32>();
        Key {
            n,
            raw,
            key: aead(&raw),
            packets: [0; STREAMS],
        }
    }
}

struct GroupState {
    current: Key,
    pending: Option<(Key, Instant)>,
    /// When the pending change was first asked for: it switches by two delays after.
    asked: Instant,
    since: Instant,
    next_n: u16,
    messages: [u64; STREAMS],
}

/// A message kept for re-sending: (number, message with trailer, when sent).
type Kept = (u64, Arc<Vec<u8>>, Instant);

/// An output's encrypted multicast group (sender side, §E.17.2–§E.17.5).
#[doc(hidden)]
pub struct Group {
    cfg: McConfig,
    state: Mutex<GroupState>,
    /// Bumped whenever a key is made: sessions send receivers the keys again.
    version: AtomicU64,
    signer: Option<Ed25519KeyPair>,
    members: AtomicUsize,
    key_changes: AtomicU64,
    /// Recent messages per stream, for re-sending: (number, message with trailer, when).
    history: Mutex<[std::collections::VecDeque<Kept>; STREAMS]>,
    resent: AtomicU64,
}

impl Group {
    /// A group with a fresh key, and a fresh signing key if signatures are on.
    pub fn new(cfg: McConfig) -> Arc<Self> {
        let signer = cfg.signatures.then(|| {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("system random");
            Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("fresh key")
        });
        let first = u16::from_le_bytes(crate::random::secure_random::<2>());
        Arc::new(Group {
            cfg,
            state: Mutex::new(GroupState {
                current: Key::new(first),
                pending: None,
                asked: Instant::now(),
                since: Instant::now(),
                next_n: first.wrapping_add(1),
                messages: [0; STREAMS],
            }),
            version: AtomicU64::new(0),
            signer,
            members: AtomicUsize::new(0),
            key_changes: AtomicU64::new(0),
            history: Mutex::default(),
            resent: AtomicU64::new(0),
        })
    }

    /// Changes whenever a key is made.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Key changes so far.
    pub fn key_changes(&self) -> u64 {
        self.key_changes.load(Ordering::Relaxed)
    }

    /// The `x_idn_mc_key` messages a receiver needs now: the current key, and the next
    /// one if a change is pending.
    pub fn key_messages(&self) -> Vec<String> {
        let st = self.state.lock_ok();
        let sign = self.signer.as_ref().map_or(String::new(), |s| {
            format!(
                r#" sign="{}""#,
                idn_common::text::base64(s.public_key().as_ref())
            )
        });
        std::iter::once(&st.current)
            .chain(st.pending.as_ref().map(|(k, _)| k))
            .map(|k| {
                format!(
                    r#"<x_idn_mc_key n="{}" key="{}"{sign}/>"#,
                    k.n,
                    idn_common::text::base64(k.raw)
                )
            })
            .collect()
    }

    /// Makes a new key, used after [`SWITCH_DELAY`]. A pending one is replaced: the
    /// receiver that just left was given it.
    pub fn change_key(&self) {
        let mut st = self.state.lock_ok();
        let n = st.next_n;
        st.next_n = n.wrapping_add(1);
        // Not later than two delays after the first change asked for: receivers leaving
        // one after another put the switch off for ever, the ones gone still holding the
        // key in use (deep bug run 2026-11-09).
        let now = Instant::now();
        let at = match st.pending.as_ref() {
            // (The fresh key given out for at least a quarter of the delay, when it can.)
            Some((_, at)) => (*at)
                .max(now + SWITCH_DELAY / 4)
                .min(st.asked + 2 * SWITCH_DELAY),
            None => {
                st.asked = now;
                now + SWITCH_DELAY
            }
        };
        st.pending = Some((Key::new(n), at));
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Switches to a pending key once it is due, and starts a change every interval.
    pub fn tick(&self) {
        let interval_due = {
            let mut st = self.state.lock_ok();
            if st
                .pending
                .as_ref()
                .is_some_and(|(_, at)| Instant::now() >= *at)
            {
                st.current = st.pending.take().unwrap().0;
                st.since = Instant::now();
                self.key_changes.fetch_add(1, Ordering::Relaxed);
            }
            st.pending.is_none() && st.since.elapsed() >= self.cfg.key_interval
        };
        if interval_due {
            self.change_key();
        }
    }

    /// A message with its trailer (§E.17.4): its number, and its signature when
    /// signing. Kept for re-sending (§E.17.6).
    pub fn sign_message(&self, stream: u8, msg: &[u8]) -> Arc<Vec<u8>> {
        let number = {
            let mut st = self.state.lock_ok();
            st.messages[stream as usize] += 1;
            st.messages[stream as usize]
        };
        let mut out = Vec::with_capacity(msg.len() + NUMBER + SIGNATURE);
        out.extend_from_slice(msg);
        out.extend_from_slice(&number.to_le_bytes());
        if let Some(signer) = &self.signer {
            out.extend_from_slice(signer.sign(&signed_digest(stream, number, msg)).as_ref());
        }
        let out = Arc::new(out);
        let mut h = self.history.lock_ok();
        let now = Instant::now();
        h[stream as usize].push_back((number, out.clone(), now));
        let mut bytes: usize = h.iter().flatten().map(|m| m.1.len()).sum();
        for q in h.iter_mut() {
            while q
                .front()
                .is_some_and(|m| now - m.2 > HISTORY || bytes > HISTORY_BYTES)
            {
                bytes -= q.pop_front().unwrap().1.len();
            }
        }
        out
    }

    /// The kept messages `from .. from + count` of `stream`, for a receiver that lost
    /// them (§E.17.6).
    pub fn resend(&self, stream: u8, from: u64, count: u64) -> Vec<Arc<Vec<u8>>> {
        let Some(q) = self.history.lock_ok().get(stream as usize).cloned() else {
            return Vec::new();
        };
        let out: Vec<_> = q
            .iter()
            .filter(|m| m.0 >= from && m.0 < from.saturating_add(count.min(MAX_GAP)))
            .map(|m| m.1.clone())
            .collect();
        self.resent.fetch_add(out.len() as u64, Ordering::Relaxed);
        out
    }

    /// Tests: whether to leave this message (from [`sign_message`](Self::sign_message))
    /// off the network.
    #[doc(hidden)]
    pub fn test_drop(&self, message: &[u8]) -> bool {
        let n = self.cfg.test_drop_every;
        let off = message
            .len()
            .saturating_sub(NUMBER + self.signer.as_ref().map_or(0, |_| SIGNATURE));
        n > 0
            && message
                .get(off..off + NUMBER)
                .is_some_and(|b| u64::from_le_bytes(b.try_into().unwrap()).is_multiple_of(n))
    }

    /// Messages re-sent so far.
    pub fn resent(&self) -> u64 {
        self.resent.load(Ordering::Relaxed)
    }

    /// Seals one §6.3.3 packet under the current key (§E.17.3).
    pub fn seal(&self, stream: u8, packet: &[u8]) -> Vec<u8> {
        let mut st = self.state.lock_ok();
        let k = &mut st.current;
        let pn = k.packets[stream as usize];
        k.packets[stream as usize] += 1;
        let mut header = [0u8; HEADER];
        header[0] = MAGIC;
        header[1] = stream;
        header[2..4].copy_from_slice(&k.n.to_le_bytes());
        header[4..].copy_from_slice(&pn.to_le_bytes());
        let mut out = Vec::with_capacity(OVERHEAD + packet.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(packet);
        let tag = k
            .key
            .seal_in_place_separate_tag(nonce(stream, pn), Aad::from(header), &mut out[HEADER..])
            .expect("packet within AEAD limits");
        out.extend_from_slice(tag.as_ref());
        out
    }

    /// Counts a receiver on encrypted multicast until the guard is dropped; its leaving
    /// changes the key.
    pub fn join(self: &Arc<Self>) -> Member {
        self.members.fetch_add(1, Ordering::Relaxed);
        Member(self.clone())
    }

    /// Receivers on encrypted multicast now.
    pub fn members(&self) -> usize {
        self.members.load(Ordering::Relaxed)
    }
}

/// A receiver on encrypted multicast (§E.17.5).
#[doc(hidden)]
pub struct Member(Arc<Group>);

impl Drop for Member {
    fn drop(&mut self) {
        self.0.members.fetch_sub(1, Ordering::Relaxed);
        self.0.change_key();
    }
}

/// A receiver's keys for one session (§E.17.2): the newest three, the output's signing
/// key, and the highest message number verified per stream.
#[doc(hidden)]
pub struct Keyring {
    keys: Mutex<Vec<(u16, LessSafeKey)>>,
    signer: Mutex<Option<Vec<u8>>>,
    last: Mutex<[u64; STREAMS]>,
    rejected: Arc<AtomicUsize>,
}

/// Messages older than this many before the newest are refused as replays; nearer ones
/// are sorted out by the [`Sequencer`].
const REPLAY_WINDOW: u64 = 1024;

impl Keyring {
    /// `rejected` counts datagrams that don't open and messages that don't verify.
    pub fn new(rejected: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Keyring {
            keys: Mutex::default(),
            signer: Mutex::default(),
            last: Mutex::default(),
            rejected,
        })
    }

    /// Takes an `x_idn_mc_key` message; `false` if malformed.
    pub fn add(&self, e: &crate::control::Element) -> bool {
        let n = e.attr("n").and_then(|v| v.parse::<u16>().ok());
        let raw = e
            .attr("key")
            .and_then(|k| idn_common::text::unbase64_r(k).ok())
            .and_then(|k| <[u8; 32]>::try_from(k).ok());
        let sign = match e.attr("sign").map(idn_common::text::unbase64_r) {
            None => None,
            Some(Ok(k)) if k.len() == 32 => Some(k),
            Some(_) => return false,
        };
        let (Some(n), Some(raw)) = (n, raw) else {
            return false;
        };
        {
            let mut s = self.signer.lock_ok();
            if *s != sign {
                // Another output (or its restart): message numbers start again.
                *self.last.lock_ok() = [0; STREAMS];
                *s = sign;
            }
        }
        let mut keys = self.keys.lock_ok();
        keys.retain(|(k, _)| *k != n);
        keys.push((n, aead(&raw)));
        if keys.len() > KEPT {
            keys.remove(0);
        }
        true
    }

    /// Whether any key arrived yet.
    pub fn has_keys(&self) -> bool {
        !self.keys.lock_ok().is_empty()
    }

    fn reject<T>(&self) -> Option<T> {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Opens a sealed datagram of `stream` into its §6.3.3 packet.
    pub fn open(&self, stream: u8, d: &[u8]) -> Option<Vec<u8>> {
        if d.len() < OVERHEAD || d[0] != MAGIC || d[1] != stream {
            return self.reject();
        }
        let n = u16::from_le_bytes([d[2], d[3]]);
        let pn = u64::from_le_bytes(d[4..12].try_into().unwrap());
        let header: [u8; HEADER] = d[..HEADER].try_into().unwrap();
        let mut body = d[HEADER..].to_vec();
        let keys = self.keys.lock_ok();
        let Some((_, key)) = keys.iter().find(|(k, _)| *k == n) else {
            drop(keys);
            return self.reject();
        };
        match key.open_in_place(nonce(stream, pn), Aad::from(header), &mut body) {
            Ok(plain) => {
                let len = plain.len();
                body.truncate(len);
                Some(body)
            }
            Err(_) => {
                drop(keys);
                self.reject()
            }
        }
    }

    /// Checks a reassembled message's trailer (§E.17.4): the message's length without
    /// it and its number, or `None` (counted) for a bad signature or an old replay. With
    /// a signing key from the link, the signature is required.
    pub fn check(&self, stream: u8, msg: &[u8]) -> Option<(usize, u64)> {
        let pk = self.signer.lock_ok().clone();
        let trailer = NUMBER + if pk.is_some() { SIGNATURE } else { 0 };
        let Some(body) = msg.len().checked_sub(trailer) else {
            return self.reject();
        };
        let number = u64::from_le_bytes(msg[body..body + NUMBER].try_into().unwrap());
        if let Some(pk) = pk {
            let digest = signed_digest(stream, number, &msg[..body]);
            if UnparsedPublicKey::new(&ED25519, &pk)
                .verify(&digest, &msg[body + NUMBER..])
                .is_err()
            {
                return self.reject();
            }
        }
        let mut last = self.last.lock_ok();
        let l = last.get_mut(stream as usize)?;
        if number.saturating_add(REPLAY_WINDOW) <= *l {
            drop(last);
            return self.reject();
        }
        *l = (*l).max(number);
        Some((body, number))
    }
}

/// Puts one stream's messages back in order (§E.17.6): after a gap, later messages wait
/// (at most `hold`) for the missing ones to be re-sent; with no loss nothing waits.
#[doc(hidden)]
pub struct Sequencer<T> {
    expected: Option<u64>,
    missing: std::collections::BTreeSet<u64>,
    held: std::collections::BTreeMap<u64, (T, Instant)>,
}

impl<T> Default for Sequencer<T> {
    fn default() -> Self {
        Sequencer {
            expected: None,
            missing: Default::default(),
            held: Default::default(),
        }
    }
}

impl<T> Sequencer<T> {
    /// Takes message `n`: the messages now in order, and the range to ask for again
    /// (first number, count), if a new gap opened.
    pub fn push(&mut self, n: u64, item: T, hold: Duration) -> (Vec<T>, Option<(u64, u64)>) {
        let Some(exp) = self.expected else {
            self.expected = Some(n.saturating_add(1));
            return (vec![item], None);
        };
        if n < exp || self.held.contains_key(&n) {
            return (Vec::new(), None); // a duplicate, or given up on
        }
        if n == exp {
            self.missing.remove(&n);
            self.expected = Some(n.saturating_add(1));
            let mut out = vec![item];
            out.extend(self.release());
            return (out, None);
        }
        if n - exp > MAX_GAP {
            // Too far ahead to repair (a long outage): start again from here.
            let mut out: Vec<T> = std::mem::take(&mut self.held)
                .into_values()
                .map(|v| v.0)
                .collect();
            out.push(item);
            self.missing.clear();
            self.expected = Some(n.saturating_add(1));
            return (out, None);
        }
        let new: Vec<u64> = (exp..n)
            .filter(|k| !self.missing.contains(k) && !self.held.contains_key(k))
            .collect();
        self.missing.extend(&new);
        self.held.insert(n, (item, Instant::now() + hold));
        let ask = new.first().zip(new.last()).map(|(&a, &b)| (a, b - a + 1));
        (Vec::new(), ask)
    }

    /// Gives up on gaps whose wait is over: the messages released.
    pub fn expire(&mut self, now: Instant) -> Vec<T> {
        let Some((&first, _)) = self.held.iter().find(|(_, v)| v.1 <= now) else {
            return Vec::new();
        };
        let first = self.held.keys().next().copied().unwrap_or(first);
        self.missing.retain(|&k| k > first);
        self.expected = Some(first);
        self.release()
    }

    /// When the oldest wait ends, if any message waits.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.held.values().map(|v| v.1).min()
    }

    fn release(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(exp) = self.expected {
            let Some((item, _)) = self.held.remove(&exp) else {
                break;
            };
            self.missing.remove(&exp);
            out.push(item);
            self.expected = Some(exp.saturating_add(1));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring_for(g: &Group) -> (Arc<Keyring>, Arc<AtomicUsize>) {
        let rejected = Arc::new(AtomicUsize::new(0));
        let k = Keyring::new(rejected.clone());
        for m in g.key_messages() {
            assert!(k.add(&crate::control::parse_element(&m).unwrap()), "{m}");
        }
        (k, rejected)
    }

    #[test]
    fn packets_open_only_with_the_key_and_untampered() {
        let g = Group::new(McConfig::default());
        let (k, rejected) = keyring_for(&g);
        let packet = b"a 6.3.3 packet".to_vec();
        let d = g.seal(0, &packet);
        assert_eq!(d.len(), packet.len() + OVERHEAD);
        assert!(
            !d.windows(packet.len()).any(|w| w == packet),
            "no clear text"
        );
        assert_eq!(k.open(0, &d).unwrap(), packet);
        // Another stream, a flipped bit, another output's key: dropped and counted.
        assert!(k.open(1, &d).is_none());
        let mut bad = d.clone();
        bad[20] ^= 1;
        assert!(k.open(0, &bad).is_none());
        let other = Group::new(McConfig::default());
        assert!(k.open(0, &other.seal(0, &packet)).is_none());
        assert_eq!(rejected.load(Ordering::Relaxed), 3);
        // Packet numbers advance, so nonces never repeat.
        assert_ne!(g.seal(0, &packet)[4..12], d[4..12]);
    }

    #[test]
    fn signed_messages_verify_once_and_forgeries_fail() {
        let g = Group::new(McConfig::default());
        let (k, rejected) = keyring_for(&g);
        let m1 = g.sign_message(2, b"frame one");
        let m2 = g.sign_message(2, b"frame two");
        assert_eq!(m1.len(), 9 + NUMBER + SIGNATURE);
        assert_eq!(k.check(2, &m1), Some((9, 1)));
        assert_eq!(k.check(2, &m2), Some((9, 2)));
        // Another stream, altered content, a message signed by another output.
        assert_eq!(k.check(0, &g.sign_message(2, b"x")), None);
        let mut altered = g.sign_message(2, b"frame six").to_vec();
        altered[0] ^= 1;
        assert_eq!(k.check(2, &altered), None);
        assert_eq!(
            k.check(2, &Group::new(McConfig::default()).sign_message(2, b"f")),
            None
        );
        assert_eq!(rejected.load(Ordering::Relaxed), 3);
        // Signatures off: only the number, and nothing to verify.
        let plain = Group::new(McConfig {
            signatures: false,
            ..McConfig::default()
        });
        let (k2, _) = keyring_for(&plain);
        let m = plain.sign_message(0, b"abc");
        assert_eq!(m.len(), 3 + NUMBER);
        assert_eq!(k2.check(0, &m), Some((3, 1)));
        // Kept for re-sending.
        assert_eq!(g.resend(2, 1, 2), [m1.clone(), m2.clone()]);
        assert_eq!(g.resend(2, 50, 3), Vec::<Arc<Vec<u8>>>::new());
    }

    #[test]
    fn the_sequencer_waits_for_resent_messages_then_gives_up() {
        let hold = Duration::from_millis(30);
        let mut s = Sequencer::default();
        assert_eq!(s.push(10, 'a', hold), (vec!['a'], None));
        assert_eq!(s.push(11, 'b', hold), (vec!['b'], None));
        // 12 and 13 lost: 14 waits, and both are asked for once.
        assert_eq!(s.push(14, 'e', hold), (vec![], Some((12, 2))));
        assert_eq!(s.push(15, 'f', hold), (vec![], None));
        assert!(s.next_deadline().is_some());
        assert_eq!(s.push(12, 'c', hold), (vec!['c'], None));
        assert_eq!(s.push(13, 'd', hold), (vec!['d', 'e', 'f'], None));
        assert_eq!(s.next_deadline(), None);
        assert_eq!(s.push(13, 'd', hold), (vec![], None), "a duplicate");
        // 17 never comes back: 18 goes once its wait is over.
        assert_eq!(s.push(16, 'g', hold), (vec!['g'], None));
        assert_eq!(s.push(18, 'i', hold), (vec![], Some((17, 1))));
        assert_eq!(s.expire(Instant::now()), vec![]);
        assert_eq!(s.expire(Instant::now() + hold), vec!['i']);
        assert_eq!(s.push(17, 'h', hold), (vec![], None), "too late");
        // A long outage: no repair, straight on.
        assert_eq!(s.push(500, 'z', hold), (vec!['z'], None));
        // Numbers at the very end of the range (a hostile peer): no overflow.
        assert_eq!(s.push(u64::MAX, 'm', hold), (vec!['m'], None));
        s.push(u64::MAX, 'm', hold);
        let g = Group::new(McConfig::default());
        assert!(g.resend(0, u64::MAX, u64::MAX).is_empty());
    }

    #[test]
    fn key_changes_switch_after_the_delay_and_keep_old_keys_briefly() {
        let g = Group::new(McConfig {
            key_interval: Duration::from_millis(50),
            ..McConfig::default()
        });
        let (k, _) = keyring_for(&g);
        let before = g.seal(0, b"p");
        std::thread::sleep(Duration::from_millis(60));
        g.tick(); // interval due: a key is made, not used yet
        assert_eq!(g.key_messages().len(), 2);
        assert_eq!(g.seal(0, b"p")[2..4], before[2..4]);
        for m in g.key_messages() {
            k.add(&crate::control::parse_element(&m).unwrap());
        }
        // A receiver leaves meanwhile: the pending key is replaced by a fresh one.
        let v = g.version();
        drop(g.join());
        assert!(g.version() > v);
        for m in g.key_messages() {
            k.add(&crate::control::parse_element(&m).unwrap());
        }
        std::thread::sleep(SWITCH_DELAY);
        g.tick();
        let after = g.seal(0, b"p");
        assert_ne!(after[2..4], before[2..4]);
        assert_eq!(g.key_changes(), 1);
        assert_eq!(k.open(0, &after).unwrap(), b"p");
        assert_eq!(
            k.open(0, &before).unwrap(),
            b"p",
            "the previous key is kept"
        );
    }
}
