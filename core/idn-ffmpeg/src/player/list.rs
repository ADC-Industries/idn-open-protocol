//! A playlist as one output (vMix-style lists): videos, sound files and pictures played one
//! after another on the same source, so a receiver (a switcher's input) follows the list
//! without changing what it takes. A video or a sound plays to its end; a picture shows
//! for its seconds. By hand (next, previous, a place), looping or stopping at the end, in
//! order or shuffled. Items are cut from one to the next, sent as SpeedHQ (no pass-through:
//! one list holds files of different codecs).

use super::{Cmd, Control, Play, PlayerConfig, PlayerState, PlayerStatus};
use crate::Ffmpeg;
use crate::ffi::Demuxer;
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{Output, OutputConfig, Publisher};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Items in a list, at most.
pub const MAX_ITEMS: usize = 1000;

/// One item: a file, and how long it shows when it is a picture.
#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub path: String,
    /// Seconds a picture shows (a video or a sound plays to its end).
    pub seconds: f32,
}

/// How a list plays.
#[derive(Debug, Clone, PartialEq)]
pub struct ListConfig {
    /// Back to the first after the last (otherwise the last picture stays).
    pub looped: bool,
    /// In a random order (a new one each time round).
    pub shuffle: bool,
    /// Start paused on the first item's first picture.
    pub paused: bool,
}

impl Default for ListConfig {
    fn default() -> Self {
        ListConfig {
            looped: true,
            shuffle: false,
            paused: false,
        }
    }
}

/// A list's status, from [`ListPlayer::status`].
#[derive(Debug, Clone, PartialEq)]
pub struct ListStatus {
    /// The place (in the list as given) of the item showing.
    pub index: usize,
    pub count: usize,
    /// The item showing, and its player's status.
    pub item: Option<String>,
    pub player: Option<PlayerStatus>,
    pub paused: bool,
    /// It reached the end of its last item and doesn't loop.
    pub ended: bool,
    /// The last item that couldn't be played, and why (it is skipped).
    pub error: Option<String>,
}

enum ListCmd {
    Next,
    Previous,
    Goto(usize),
    Pause,
    Resume,
    Restart,
    Items(Vec<ListItem>),
}

struct ListCtl {
    cmds: Mutex<VecDeque<ListCmd>>,
    status: Mutex<ListStatus>,
    stop: AtomicBool,
}

/// A playlist playing as an output. Dropping it stops the output.
pub struct ListPlayer {
    out: Arc<Output>,
    ctl: Arc<ListCtl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A picture file, by its name (shown for its seconds, not played).
pub fn is_picture(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".bmp", ".webp", ".tif", ".tiff", ".gif",
    ]
    .iter()
    .any(|e| p.ends_with(e))
}

impl ListPlayer {
    /// Starts an output (`out_cfg`; sent as SpeedHQ) playing `items`.
    pub fn open(
        publisher: &Publisher,
        mut out_cfg: OutputConfig,
        items: Vec<ListItem>,
        ff: Arc<Ffmpeg>,
        cfg: ListConfig,
    ) -> io::Result<Self> {
        if items.len() > MAX_ITEMS {
            return Err(idn_common::err::invalid_input(format!(
                "at most {MAX_ITEMS} items"
            )));
        }
        out_cfg.codec = None;
        let out = Arc::new(Output::new(publisher, out_cfg)?);
        let ctl = Arc::new(ListCtl {
            cmds: Mutex::default(),
            status: Mutex::new(ListStatus {
                index: 0,
                count: items.len(),
                item: None,
                player: None,
                paused: cfg.paused,
                ended: false,
                error: None,
            }),
            stop: AtomicBool::new(false),
        });
        let (o, c) = (out.clone(), ctl.clone());
        let thread = std::thread::Builder::new()
            .name("idn-list-player".into())
            .spawn(move || run(&o, &c, items, &ff, &cfg))?;
        Ok(ListPlayer {
            out,
            ctl,
            thread: Some(thread),
        })
    }

    /// The output the list plays on.
    pub fn output(&self) -> &Arc<Output> {
        &self.out
    }

    fn cmd(&self, c: ListCmd) {
        self.ctl.cmds.lock_ok().push_back(c);
    }

    pub fn next(&self) {
        self.cmd(ListCmd::Next);
    }

    pub fn previous(&self) {
        self.cmd(ListCmd::Previous);
    }

    /// Plays the item at `place` (in the list as given).
    pub fn goto(&self, place: usize) {
        self.cmd(ListCmd::Goto(place));
    }

    pub fn pause(&self) {
        self.cmd(ListCmd::Pause);
    }

    pub fn resume(&self) {
        self.cmd(ListCmd::Resume);
    }

    /// The item showing, from its start.
    pub fn restart(&self) {
        self.cmd(ListCmd::Restart);
    }

    /// Replaces the items; the one showing goes on if it is still in the list.
    pub fn set_items(&self, items: Vec<ListItem>) -> Result<(), String> {
        if items.len() > MAX_ITEMS {
            return Err(format!("at most {MAX_ITEMS} items"));
        }
        self.cmd(ListCmd::Items(items));
        Ok(())
    }

    pub fn status(&self) -> ListStatus {
        self.ctl.status.lock_ok().clone()
    }
}

impl Drop for ListPlayer {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The order a list's `n` items play in.
fn order(n: usize, shuffle: bool) -> Vec<usize> {
    let mut o: Vec<usize> = (0..n).collect();
    if shuffle {
        let mut x = idn_common::epoch::now().as_nanos() as u64 | 1;
        for i in (1..n).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            o.swap(i, (x % (i as u64 + 1)) as usize);
        }
    }
    o
}

/// An item playing: its player's control and thread.
struct Playing {
    ctl: Arc<Control>,
    thread: std::thread::JoinHandle<()>,
    picture: bool,
    /// How long it has shown (a picture's, not counting pauses), and since when.
    shown: Duration,
    since: Option<Instant>,
}

impl Playing {
    fn stop(self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Starts `item` on `out`.
fn start(
    item: &ListItem,
    out: &Arc<Output>,
    ff: &Arc<Ffmpeg>,
    paused: bool,
) -> io::Result<Playing> {
    let demux = Demuxer::open(&item.path)?;
    let video = demux.streams.iter().position(|s| s.video);
    let audio = demux.streams.iter().position(|s| !s.video);
    if video.is_none() && audio.is_none() {
        return Err(io::Error::other("no video or audio"));
    }
    let picture = is_picture(&item.path);
    if let Some(v) = video {
        let (n, d) = demux.streams[v].frame_rate;
        if n > 0 && d > 0 && !picture {
            out.set_frame_rate((n as u32, d as u32));
        }
    }
    let ctl = Arc::new(Control {
        cmds: Mutex::default(),
        status: Mutex::new(PlayerStatus {
            state: if paused {
                PlayerState::Paused
            } else {
                PlayerState::Playing
            },
            position: Duration::ZERO,
            duration: demux.duration_us.map(|us| Duration::from_micros(us as u64)),
            video: None,
            pass_through: Err("a list".into()),
            decoder: None,
            decode_ms: 0.0,
            shifted: Duration::ZERO,
            late_pictures: 0,
            caught_up: 0,
        }),
        stop: AtomicBool::new(false),
        #[cfg(feature = "gpu")]
        gpu: Mutex::new(None),
    });
    let cfg = PlayerConfig {
        // A picture is a one-picture file sent again and again while it shows.
        looped: picture,
        pass_through: false,
        compressed: false,
        paused,
        decoder: picture.then_some(crate::DecoderChoice::Software),
        ..Default::default()
    };
    let (o, c, f) = (out.clone(), ctl.clone(), ff.clone());
    let thread = std::thread::Builder::new()
        .name("idn-list-item".into())
        .spawn(move || {
            let mut p = Play::new(demux, video, audio, None, o, c.clone(), cfg, f);
            if let Err(e) = p.run() {
                c.status.lock_ok().state = PlayerState::Failed(e.to_string());
            }
        })?;
    Ok(Playing {
        ctl,
        thread,
        picture,
        shown: Duration::ZERO,
        since: (!paused).then(Instant::now),
    })
}

fn run(
    out: &Arc<Output>,
    ctl: &ListCtl,
    mut items: Vec<ListItem>,
    ff: &Arc<Ffmpeg>,
    cfg: &ListConfig,
) {
    let mut ord = order(items.len(), cfg.shuffle);
    let mut at = 0usize;
    let mut paused = cfg.paused;
    // The item to start; `None` while at the end (or with no items).
    let mut want = (!items.is_empty()).then_some(0usize);
    let mut now: Option<Playing> = None;
    let set_status = |items: &[ListItem],
                      ord: &[usize],
                      at: usize,
                      now: &Option<Playing>,
                      paused: bool,
                      ended: bool| {
        let mut s = ctl.status.lock_ok();
        s.count = items.len();
        s.index = ord.get(at).copied().unwrap_or(0);
        s.item = ord
            .get(at)
            .and_then(|&i| items.get(i))
            .map(|i| i.path.clone());
        s.player = now.as_ref().map(|p| p.ctl.status.lock_ok().clone());
        s.paused = paused;
        s.ended = ended;
    };
    let mut ended = false;
    while !ctl.stop.load(Ordering::Relaxed) {
        if let Some(place) = want.take() {
            if let Some(p) = now.take() {
                p.stop();
            }
            at = place;
            ended = false;
            if let Some(item) = ord.get(at).and_then(|&i| items.get(i)) {
                match start(item, out, ff, paused) {
                    Ok(p) => {
                        now = Some(p);
                        ctl.status.lock_ok().error = None;
                    }
                    Err(e) => {
                        // Skipped, after a moment (a list of missing files doesn't spin).
                        ctl.status.lock_ok().error = Some(format!("{}: {e}", item.path));
                        std::thread::sleep(Duration::from_millis(500));
                        want = next_place(at, ord.len(), cfg.looped);
                        ended = want.is_none();
                    }
                }
            }
        }
        // Commands.
        let cmds: Vec<ListCmd> = ctl.cmds.lock_ok().drain(..).collect();
        for c in cmds {
            let n = ord.len();
            match c {
                ListCmd::Next if n > 0 => want = Some((at + 1) % n),
                ListCmd::Previous if n > 0 => want = Some((at + n - 1) % n),
                ListCmd::Goto(i) => want = ord.iter().position(|&p| p == i),
                ListCmd::Restart if n > 0 => want = Some(at),
                ListCmd::Pause => {
                    paused = true;
                    if let Some(p) = now.as_mut() {
                        p.ctl.cmds.lock_ok().push_back(Cmd::Pause);
                        if let Some(t) = p.since.take() {
                            p.shown += t.elapsed();
                        }
                    }
                }
                ListCmd::Resume => {
                    paused = false;
                    if let Some(p) = now.as_mut() {
                        p.ctl.cmds.lock_ok().push_back(Cmd::Resume);
                        p.since.get_or_insert_with(Instant::now);
                    }
                    if ended && n > 0 {
                        want = Some(0);
                    }
                }
                ListCmd::Items(new) => {
                    // The item showing keeps playing where it is now in the list.
                    let current = ord
                        .get(at)
                        .and_then(|&i| items.get(i))
                        .map(|i| i.path.clone());
                    items = new;
                    ord = order(items.len(), cfg.shuffle);
                    match current.and_then(|c| items.iter().position(|i| i.path == c)) {
                        Some(i) => at = ord.iter().position(|&p| p == i).unwrap_or(0),
                        None => {
                            at = 0;
                            want = (!items.is_empty()).then_some(0);
                            if items.is_empty()
                                && let Some(p) = now.take()
                            {
                                p.stop();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // The item showing: finished?
        if want.is_none()
            && let Some(p) = now.as_ref()
        {
            let state = p.ctl.status.lock_ok().state.clone();
            let done = match state {
                PlayerState::Ended => true,
                PlayerState::Failed(e) => {
                    ctl.status.lock_ok().error = Some(e);
                    true
                }
                _ => {
                    let item = ord.get(at).and_then(|&i| items.get(i));
                    p.picture
                        && item.is_some_and(|i| {
                            p.shown + p.since.map_or(Duration::ZERO, |t| t.elapsed())
                                >= Duration::from_secs_f32(i.seconds.max(0.1))
                        })
                }
            };
            if done {
                want = next_place(at, ord.len(), cfg.looped);
                if want == Some(0) && cfg.shuffle {
                    ord = order(items.len(), true);
                }
                if want.is_none() {
                    // The end: the last picture stays (the output keeps it), playing stops.
                    ended = true;
                    if let Some(p) = now.take() {
                        p.stop();
                    }
                }
            }
        }
        set_status(&items, &ord, at, &now, paused, ended);
        if want.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(p) = now.take() {
        p.stop();
    }
}

/// The place after `at` of `n`: the next, the first again when looping, else none.
fn next_place(at: usize, n: usize, looped: bool) -> Option<usize> {
    if at + 1 < n {
        Some(at + 1)
    } else if looped && n > 0 {
        Some(0)
    } else {
        None
    }
}
