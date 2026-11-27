//! What the client sends besides pings, in order: the talk commands, the microphone's
//! packets between them, and the app's other commands. Nothing of a talk is lost to a
//! dead zone:
//!
//! - **Held while away**: while not connected the packets wait here, up to the
//!   configuration's `hold_mic_s` (30 s); past it the oldest waiting packet goes, counted.
//! - **Kept until confirmed**: a packet or command written into a link that then dies
//!   without a word (cellular) never arrived. So what was sent is kept until a `pong`
//!   confirms it: the server handles a connection's messages in order, so a `pong` means
//!   everything sent before its `ping` arrived. While anything is unconfirmed the session
//!   pings every second, so at most about a second is sent twice after a silent drop.
//! - **After a reconnect** the unconfirmed go back in front of the rest and the queue is
//!   put right for a server with no talk on ([`Outbox::normalize`]): each talk is started
//!   (again) before its first packet, once, with its own command. The same when the
//!   server ends a talk of ours while we are connected and its packets were still coming
//!   ("no sound" after a dead zone on QUIC, where the connection survives): the packets
//!   it dropped for want of a talk go again. Then everything goes in order, a backlog at
//!   [`PACE`] (4× real time; the server takes packets as they come and has no mark for
//!   late ones: listeners get a late burst, which their jitter buffers play in full and
//!   then catch up on).
//! - **Which talk the server means**: its `talking` messages name none, so the talks
//!   started are numbered: a `talking on`, `busy` or refusal answers the oldest start
//!   sent and not yet answered; a `talking off` of the server's own (a time limit, taken,
//!   no sound) ends the talk it last confirmed. Only that talk's waiting packets go; a
//!   newer talk the app started meanwhile is untouched.

use std::collections::VecDeque;
use std::time::Duration;

/// One packet every 5 ms while a backlog is sent (4× real time).
pub(crate) const PACE: Duration = Duration::from_millis(5);
/// Packets waiting up to which they go at once (live talk), not paced.
pub(crate) const LIVE: usize = 2;
/// Packets waiting kept at least, whatever `hold_mic_s` (a second of live talk).
const KEEP_AT_LEAST: usize = 50;
/// Sent packets kept unconfirmed at most (30 s: a link stalled that long without
/// dropping); past it the oldest is let go (not counted as dropped: it was sent).
const UNCONFIRMED_MAX: usize = 1_500;
/// Talk commands remembered for starting their talk again.
const TALKS_KEPT: usize = 8;

/// A talk's number (the app's starts, counted from 1; 0: none).
pub(crate) type TalkId = u64;

/// One thing to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    /// A `talk` command: started (`on`) or ended, for talk `id`.
    Talk { id: TalkId, json: String, on: bool },
    /// A microphone message (a `1`, then the Opus packet) of talk `talk`.
    Sound { talk: TalkId, bytes: Vec<u8> },
    /// Any other command of the app's (join, message, sos…), as JSON.
    Cmd(String),
}

impl Item {
    fn is_sound(&self) -> bool {
        matches!(self, Item::Sound { .. })
    }
}

#[derive(Debug)]
pub(crate) struct Outbox {
    queue: VecDeque<Item>,
    /// Sent, numbered, not yet confirmed.
    unconfirmed: VecDeque<(u64, Item)>,
    /// The count sent at each `ping` not yet answered, oldest first.
    marks: VecDeque<u64>,
    sent: u64,
    /// Talk starts sent and not yet answered (`talking on`, `busy`, an error), oldest
    /// first; the talk the server has on (answered `talking on`).
    awaiting: VecDeque<TalkId>,
    server_talk: Option<TalkId>,
    /// Each talk's start command, the newest few.
    talks: VecDeque<(TalkId, String)>,
    next_talk: TalkId,
    /// Packets waiting kept at most (`hold_mic_s`); packets waiting and unconfirmed.
    max: usize,
    waiting_sounds: usize,
    unconfirmed_sounds: usize,
    pub dropped: u64,
}

impl Outbox {
    /// Holding `hold_mic_s` of 20 ms packets.
    pub fn new(hold_mic_s: u64) -> Outbox {
        Outbox {
            queue: VecDeque::new(),
            unconfirmed: VecDeque::new(),
            marks: VecDeque::new(),
            sent: 0,
            awaiting: VecDeque::new(),
            server_talk: None,
            talks: VecDeque::new(),
            next_talk: 0,
            max: (hold_mic_s as usize * 50).max(KEEP_AT_LEAST),
            waiting_sounds: 0,
            unconfirmed_sounds: 0,
            dropped: 0,
        }
    }

    /// A new talk's number, its start command kept.
    pub fn new_talk(&mut self, json: &str) -> TalkId {
        self.next_talk += 1;
        self.talks.push_back((self.next_talk, json.to_owned()));
        while self.talks.len() > TALKS_KEPT {
            self.talks.pop_front();
        }
        self.next_talk
    }

    fn talk_json(&self, id: TalkId) -> Option<String> {
        self.talks
            .iter()
            .find(|(t, _)| *t == id)
            .map(|(_, j)| j.clone())
    }

    /// Packets held (waiting or unconfirmed).
    pub fn held(&self) -> usize {
        self.waiting_sounds + self.unconfirmed_sounds
    }

    /// Items to send.
    pub fn waiting(&self) -> usize {
        self.queue.len()
    }

    pub fn unconfirmed(&self) -> bool {
        !self.unconfirmed.is_empty()
    }

    /// Past the hold, the oldest waiting packets go, counted.
    fn trim(&mut self) {
        while self.waiting_sounds > self.max {
            let Some(k) = self.queue.iter().position(Item::is_sound) else {
                break;
            };
            self.queue.remove(k);
            self.waiting_sounds -= 1;
            self.dropped += 1;
        }
    }

    /// Queued behind what waits.
    pub fn push(&mut self, item: Item) {
        if item.is_sound() {
            self.waiting_sounds += 1;
        }
        self.queue.push_back(item);
        self.trim();
    }

    /// A packet that couldn't be held at all (holding is off and the link is down).
    pub fn drop_one(&mut self) {
        self.dropped += 1;
    }

    /// The next to send (left in place until [`Outbox::sent`]).
    pub fn front(&self) -> Option<&Item> {
        self.queue.front()
    }

    /// The front one went into the connection.
    pub fn sent(&mut self) {
        let Some(i) = self.queue.pop_front() else {
            return;
        };
        match &i {
            Item::Sound { .. } => {
                self.waiting_sounds -= 1;
                self.unconfirmed_sounds += 1;
            }
            Item::Talk { id, on: true, .. } => self.awaiting.push_back(*id),
            _ => {}
        }
        self.sent += 1;
        self.unconfirmed.push_back((self.sent, i));
        // A link stalled for long: the oldest sent packet is let go (it was sent).
        while self.unconfirmed_sounds > UNCONFIRMED_MAX {
            let Some(k) = self.unconfirmed.iter().position(|(_, i)| i.is_sound()) else {
                break;
            };
            self.unconfirmed.remove(k);
            self.unconfirmed_sounds -= 1;
        }
    }

    /// A `ping` goes now.
    pub fn ping(&mut self) {
        self.marks.push_back(self.sent);
    }

    /// Its `pong` came: what was sent before that `ping` arrived.
    pub fn pong(&mut self) {
        let Some(m) = self.marks.pop_front() else {
            return;
        };
        while self.unconfirmed.front().is_some_and(|(n, _)| *n <= m) {
            if let Some((_, i)) = self.unconfirmed.pop_front()
                && i.is_sound()
            {
                self.unconfirmed_sounds -= 1;
            }
        }
    }

    /// `talking on`: the oldest start sent is the server's talk now.
    pub fn talking_on(&mut self) {
        self.server_talk = self.awaiting.pop_front().or(self.server_talk);
    }

    /// The talk ended by the app's own doing (released, replaced).
    pub fn released(&mut self) {
        self.server_talk = None;
    }

    /// A start refused (`busy`, an error): which talk.
    pub fn refused(&mut self) -> Option<TalkId> {
        self.awaiting.pop_front()
    }

    /// The server ended its talk for a reason of its own: which talk (none if none was
    /// on: an end of an older one, e.g. "link lost" for the connection before).
    pub fn server_ended(&mut self) -> Option<TalkId> {
        self.server_talk.take()
    }

    /// Unconfirmed talk items and packets back in front (`all`: commands too).
    fn take_back(&mut self, all: bool) {
        let mut keep = VecDeque::new();
        let mut back = Vec::new();
        for (n, i) in self.unconfirmed.drain(..) {
            if all || !matches!(i, Item::Cmd(_)) {
                back.push(i);
            } else {
                keep.push_back((n, i));
            }
        }
        self.unconfirmed = keep;
        for i in back.into_iter().rev() {
            if i.is_sound() {
                self.unconfirmed_sounds -= 1;
                self.waiting_sounds += 1;
            }
            self.queue.push_front(i);
        }
    }

    /// The queue put right for a server with no talk of ours on: a talk started before
    /// its first packet (again, with its own command; once), an end with nothing on
    /// dropped, a start ended before any packet dropped with its end. `held`: the talk
    /// the app holds, started at the end if nothing before did (its packets come later).
    pub fn normalize(&mut self, held: Option<TalkId>) {
        let mut cur: Option<TalkId> = None;
        let mut out: VecDeque<Item> = VecDeque::new();
        for i in std::mem::take(&mut self.queue) {
            match i {
                Item::Talk { id, on: true, .. } if cur == Some(id) => {}
                Item::Talk { id, on: true, .. } => {
                    cur = Some(id);
                    out.push_back(i);
                }
                Item::Talk { on: false, .. } if cur.is_none() => {}
                Item::Talk { on: false, .. } => {
                    // A start with nothing after it: neither goes.
                    if matches!(out.back(), Some(Item::Talk { on: true, .. })) {
                        out.pop_back();
                    } else {
                        out.push_back(i);
                    }
                    cur = None;
                }
                Item::Sound { talk, .. } => {
                    if cur != Some(talk) {
                        match self.talk_json(talk) {
                            Some(json) => out.push_back(Item::Talk {
                                id: talk,
                                json,
                                on: true,
                            }),
                            None => {
                                // (A talk too old to start again: its packets go.)
                                self.waiting_sounds -= 1;
                                self.dropped += 1;
                                continue;
                            }
                        }
                        cur = Some(talk);
                    }
                    out.push_back(i);
                }
                Item::Cmd(_) => out.push_back(i),
            }
        }
        if let Some(h) = held
            && cur != Some(h)
            && let Some(json) = self.talk_json(h)
        {
            out.push_back(Item::Talk {
                id: h,
                json,
                on: true,
            });
        }
        self.queue = out;
    }

    /// The connection is gone: everything unconfirmed goes again, in front, and the
    /// queue is put right (the server ended our talk).
    pub fn requeue(&mut self, held: Option<TalkId>) {
        self.marks.clear();
        self.awaiting.clear();
        self.server_talk = None;
        self.take_back(true);
        self.normalize(held);
        self.trim();
    }

    /// The server ended talk `t` while connected ("no sound", "link lost"): its packets
    /// it dropped go again after a new start. False when none of its packets wait (the
    /// talk is over then).
    pub fn interrupted(&mut self, t: TalkId) -> bool {
        // (Commands stay: they arrived, and their pongs will say so.)
        self.take_back(false);
        let pending = self
            .queue
            .iter()
            .any(|i| matches!(i, Item::Sound { talk, .. } if *talk == t));
        self.normalize(None);
        pending
    }

    /// Talk `t` ended for good (refused, taken, its time limit): what waits of it goes,
    /// counted; what was sent of it is let go.
    pub fn end_talk(&mut self, t: TalkId) {
        let mine = |i: &Item| match i {
            Item::Talk { id, .. } => *id == t,
            Item::Sound { talk, .. } => *talk == t,
            Item::Cmd(_) => false,
        };
        self.queue.retain(|i| !mine(i));
        let sounds_now = self.queue.iter().filter(|i| i.is_sound()).count();
        self.dropped += (self.waiting_sounds - sounds_now) as u64;
        self.waiting_sounds = sounds_now;
        self.unconfirmed.retain(|(_, i)| !mine(i));
        self.unconfirmed_sounds = self
            .unconfirmed
            .iter()
            .filter(|(_, i)| i.is_sound())
            .count();
        self.awaiting.retain(|a| *a != t);
        if self.server_talk == Some(t) {
            self.server_talk = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: &str = r#"{"t":"talk","on":true,"channel":3}"#;
    const ON7: &str = r#"{"t":"talk","on":true,"channel":7}"#;
    const OFF: &str = r#"{"t":"talk","on":false}"#;

    fn on(o: &mut Outbox, json: &str) -> TalkId {
        let id = o.new_talk(json);
        o.push(Item::Talk {
            id,
            json: json.into(),
            on: true,
        });
        id
    }

    fn off(id: TalkId) -> Item {
        Item::Talk {
            id,
            json: OFF.into(),
            on: false,
        }
    }

    fn snd(talk: TalkId, n: u8) -> Item {
        Item::Sound {
            talk,
            bytes: vec![1, n],
        }
    }

    /// Sends everything waiting: what went, in order (talks as "on N"/"off", packets as
    /// their byte, commands as "cmd").
    fn send_all(o: &mut Outbox) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(i) = o.front().cloned() {
            o.sent();
            out.push(match i {
                Item::Talk { id, on: true, .. } => format!("on {id}"),
                Item::Talk { on: false, .. } => "off".into(),
                Item::Sound { bytes, .. } => bytes[1].to_string(),
                Item::Cmd(_) => "cmd".into(),
            });
        }
        out
    }

    #[test]
    fn what_a_pong_confirms_is_let_go_and_the_rest_is_sent_again_after_a_drop() {
        let mut o = Outbox::new(30);
        let t = on(&mut o, ON);
        o.push(snd(t, 1));
        o.push(snd(t, 2));
        o.push(Item::Cmd(r#"{"t":"join","channel":3}"#.into()));
        assert_eq!(send_all(&mut o).len(), 4);
        o.talking_on();
        o.ping();
        o.push(snd(t, 3));
        o.push(snd(t, 4));
        send_all(&mut o);
        o.ping();
        // The first ping answered: the talk, packets 1–2 and the join arrived; 3–4 not known.
        o.pong();
        assert_eq!(o.held(), 2);
        // The link dies; packet 5 is made while away.
        o.push(snd(t, 5));
        o.requeue(Some(t));
        assert_eq!(
            send_all(&mut o),
            ["on 1", "3", "4", "5"],
            "the talk again first, then everything not confirmed, in order"
        );
        // An unanswered ping from the dead link doesn't confirm anything later.
        o.ping();
        o.pong();
        assert_eq!((o.held(), o.dropped), (0, 0));
    }

    #[test]
    fn a_talk_released_while_away_still_sends_its_packets_then_ends() {
        let mut o = Outbox::new(30);
        let t = on(&mut o, ON);
        send_all(&mut o);
        o.ping();
        o.pong();
        // Away: the driver talks, then lets go of the button.
        o.push(snd(t, 1));
        o.push(snd(t, 2));
        o.push(off(t));
        o.requeue(None);
        assert_eq!(send_all(&mut o), ["on 1", "1", "2", "off"]);
        // Confirmed off: a later drop starts nothing (nothing held).
        o.ping();
        o.pong();
        o.requeue(None);
        assert_eq!(o.waiting(), 0);
        // A held talk with no packets waiting is started again at the end.
        o.requeue(Some(t));
        assert_eq!(send_all(&mut o), ["on 1"]);
    }

    #[test]
    fn a_drop_while_restarting_starts_the_talk_once() {
        let mut o = Outbox::new(30);
        let t = on(&mut o, ON);
        o.push(snd(t, 1));
        o.push(snd(t, 2));
        // Sent, then the link dies: [on, 1, 2] again.
        send_all(&mut o);
        o.requeue(Some(t));
        // Half of that sent again, then the link dies once more.
        o.sent();
        o.sent();
        o.push(snd(t, 3));
        o.requeue(Some(t));
        assert_eq!(
            send_all(&mut o),
            ["on 1", "1", "2", "3"],
            "one start, in front"
        );
    }

    #[test]
    fn a_restart_uses_the_talk_its_packets_belong_to() {
        let mut o = Outbox::new(30);
        let a = on(&mut o, ON);
        o.push(snd(a, 1));
        o.push(off(a));
        let b = on(&mut o, ON7);
        o.push(snd(b, 2));
        send_all(&mut o);
        o.requeue(Some(b));
        let q: Vec<Item> = o.queue.iter().cloned().collect();
        assert_eq!(
            q[0],
            Item::Talk {
                id: a,
                json: ON.into(),
                on: true
            }
        );
        assert_eq!(send_all(&mut o), ["on 1", "1", "off", "on 2", "2"]);
    }

    #[test]
    fn a_quiet_end_while_connected_sends_the_dropped_packets_again() {
        let mut o = Outbox::new(30);
        let t = on(&mut o, ON);
        send_all(&mut o);
        o.talking_on();
        o.push(Item::Cmd(r#"{"t":"sos"}"#.into()));
        o.push(snd(t, 1));
        o.push(snd(t, 2));
        send_all(&mut o);
        o.ping();
        // A dead zone on QUIC: the server ended the talk ("no sound") and dropped 1–2.
        assert_eq!(o.server_ended(), Some(t));
        assert!(o.interrupted(t));
        assert_eq!(
            send_all(&mut o),
            ["on 1", "1", "2"],
            "the sos isn't sent twice"
        );
        // The ping's pong confirms the sos, not the packets sent again after it.
        o.pong();
        assert_eq!(o.held(), 2);
        // A late "link lost" for an older talk: nothing on, nothing ended.
        assert_eq!(o.server_ended(), None);
    }

    #[test]
    fn a_late_end_of_an_old_talk_leaves_a_newer_one() {
        let mut o = Outbox::new(30);
        let a = on(&mut o, ON);
        send_all(&mut o);
        o.talking_on();
        o.push(snd(a, 1));
        o.push(off(a));
        let b = on(&mut o, ON7);
        o.push(snd(b, 2));
        // The server's time limit for a, before any of this was sent.
        assert_eq!(o.server_ended(), Some(a));
        o.end_talk(a);
        assert_eq!(o.dropped, 1);
        assert_eq!(send_all(&mut o), ["on 2", "2"]);
        // b is refused (busy): its packets go too.
        assert_eq!(o.refused(), Some(b));
    }

    #[test]
    fn a_start_with_nothing_before_its_end_is_not_sent_again() {
        let mut o = Outbox::new(30);
        let t = on(&mut o, ON);
        o.push(off(t));
        o.requeue(None);
        assert_eq!(o.waiting(), 0);
    }

    #[test]
    fn the_hold_is_bounded_and_counts_only_what_never_went() {
        let mut o = Outbox::new(1);
        let t = on(&mut o, ON);
        for n in 0..60u8 {
            o.push(snd(t, n));
        }
        assert_eq!(o.held(), 50);
        assert_eq!(o.dropped, 10);
        // The oldest went; the talk command stays.
        assert!(matches!(o.front(), Some(Item::Talk { on: true, .. })));
        o.sent();
        assert_eq!(o.front(), Some(&snd(t, 10)));
        // A talk ended for good drops what waits of it.
        o.end_talk(t);
        assert_eq!((o.held(), o.dropped), (0, 60));
    }
}
