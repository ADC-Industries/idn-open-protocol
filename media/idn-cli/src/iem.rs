//! `idn iem listen`: the phone mixing library (idn-iem.md §I.8) as a test listener on a
//! computer: a desk's IEM mix on the sound card, its status once a second, push-to-talk
//! from the default microphone with Enter.

use idn_iem::receiver::{Config, Receiver, Transport, Trust};
use std::io::BufRead;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: idn iem listen --desk HOST[:PORT] [--token TOKEN | --token - | env IDN_TOKEN]\n\
\x20                      [--mix ID] [--multicast | --unicast] [--ca FILE | --fingerprint SHA256]\n\
\x20                      [--talk] [--buffer MS] [--device NAME] [--no-sound] [--seconds N]\n\
A desk's IEM mix (spec/extensions/idn-iem.md) on this computer, as a phone takes it.\n\
\x20\x20--desk HOST[:PORT]   the desk (no port: 29214 on its own machine, 29215 with --ca/--fingerprint)\n\
\x20\x20--token TOKEN        a site token for the desk (iem:<mix>, control or read); - reads it\n\
\x20\x20                     from standard input (process listings show arguments)\n\
\x20\x20--mix ID             the mix to follow (an iem:<mix> token names its own)\n\
\x20\x20--multicast/--unicast  how the flow comes (default: multicast when it arrives, else unicast)\n\
\x20\x20--ca FILE            the desk's certificate or the site CA (DER or PEM): HTTPS, pinned\n\
\x20\x20--fingerprint FP     the desk's certificate's SHA-256 (as the desk prints it): HTTPS\n\
\x20\x20--talk               push-to-talk from the default microphone: Enter starts, Enter stops\n\
\x20\x20--buffer MS          the jitter buffer's start (default 15)\n\
\x20\x20--device NAME        the sound output (a part of its name)\n\
\x20\x20--no-sound           no sound card: the mix rendered and dropped (status only)\n\
\x20\x20--seconds N          stop after N seconds";

pub fn main() -> std::io::Result<()> {
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    let mut cfg = Config::default();
    let (mut talk, mut no_sound, mut device, mut seconds) =
        (false, false, None::<String>, None::<u64>);
    let mut listen = false;
    let mut token = idn_common::switches::value("IDN_TOKEN");
    while let Some(a) = args.next() {
        match a.as_str() {
            "listen" => listen = true,
            "--desk" => cfg.desk = args.value("--desk"),
            "--token" => token = Some(args.value("--token")),
            "--mix" => cfg.mix = Some(args.value("--mix")),
            "--multicast" => cfg.transport = Transport::Multicast,
            "--unicast" => cfg.transport = Transport::Unicast,
            "--ca" => {
                let path: String = args.value("--ca");
                let b = std::fs::read(&path)
                    .unwrap_or_else(|e| args.fail(&format!("--ca {path}: {e}")));
                cfg.trust = Some(
                    Trust::from_file_bytes(&b)
                        .unwrap_or_else(|e| args.fail(&format!("--ca {path}: {e}"))),
                );
            }
            "--fingerprint" => cfg.trust = Some(Trust::Sha256(args.value("--fingerprint"))),
            "--talk" => talk = true,
            "--buffer" => cfg.buffer_ms = args.value("--buffer"),
            "--device" => device = Some(args.value("--device")),
            "--no-sound" => no_sound = true,
            "--seconds" => seconds = Some(args.value("--seconds")),
            "-h" | "--help" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    if !listen {
        args.fail("idn iem: listen is the only command");
    }
    if cfg.desk.is_empty() {
        args.fail("--desk is required");
    }
    if token.as_deref() == Some("-") {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        token = Some(line.trim().to_owned());
    }
    cfg.token = token.unwrap_or_default();
    cfg.talk = talk;
    let rx = Receiver::connect(cfg).map_err(|e| std::io::Error::other(e.to_string()))?;
    let st = rx.status();
    println!(
        "idn iem: {} (mix {}), {} channels, {}",
        st.desk,
        st.mix.as_deref().unwrap_or("?"),
        rx.channels().len(),
        st.transport.name()
    );
    for c in rx.channels() {
        let side = c.side.map_or(String::new(), |s| format!(" {s:?}"));
        println!("  ch {:>2}  {}{side}", c.id, c.name);
    }
    // The sound card, or nothing at the sound card's pace.
    let _player = if no_sound {
        let r = rx.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut buf = vec![0f32; 480];
            for k in 1u64..=u64::MAX {
                r.render(&mut buf);
                let next = start + Duration::from_micros(5000 * k);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        None
    } else {
        let p =
            idn_iem::play::Player::start(&rx, device.as_deref()).map_err(std::io::Error::other)?;
        println!("playing on {} ({} Hz, {} ch)", p.device, p.rate, p.channels);
        Some(p)
    };
    let _mic = if talk {
        let m = idn_iem::play::Mic::start(&rx, None).map_err(std::io::Error::other)?;
        println!(
            "talkback from {}: Enter to talk, Enter again to stop",
            m.device
        );
        let r = rx.clone();
        std::thread::spawn(move || {
            let mut on = false;
            for _ in std::io::stdin().lock().lines() {
                on = !on;
                let res = if on { r.talk_start() } else { r.talk_stop() };
                match res {
                    Ok(()) => println!("{}", if on { "TALKING" } else { "talk off" }),
                    Err(e) => {
                        println!("talk: {e}");
                        on = false;
                    }
                }
            }
        });
        Some(m)
    } else {
        None
    };
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        while let Some(e) = rx.next_event(Duration::ZERO) {
            println!("event: {}", e.to_json());
        }
        let s = rx.status();
        println!(
            "{} {:<9} buffer {:>5.1} ms (target {:>4.1})  loss {:>4.1}%  rtt {}  latency ~{:.0} ms  \
             concealed {}  late {}  {}",
            if s.connected { "OK  " } else { "WAIT" },
            s.transport.name(),
            s.buffer_ms,
            s.target_ms,
            s.loss_pct,
            s.rtt_ms.map_or("?".into(), |r| format!("{r:.1} ms")),
            s.latency_ms,
            s.counters.concealed,
            s.counters.late,
            if s.talking { "TALKING" } else { "" },
        );
        if seconds.is_some_and(|n| start.elapsed() >= Duration::from_secs(n)) {
            rx.close();
            return Ok(());
        }
    }
}
