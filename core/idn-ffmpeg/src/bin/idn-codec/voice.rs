//! `idn-codec voice-feed`: an IDN source fed into an IEM channel of IDN Voice
//! (spec/extensions/idn-voice.md §V.9), as the Site Manager starts it; the token in
//! `IDN_VOICE_FEED_TOKEN`. Ends when the Site Manager closes its standard input.

pub(super) fn voice_feed(mut args: idn_open_protocol::cli::Args) {
    let (mut source, mut url, mut vox) = (None::<String>, None::<String>, None::<u32>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => url = Some(args.value("--url")),
            "--vox" => vox = Some(args.value("--vox")),
            s if !s.starts_with("--") && source.is_none() => source = Some(s.to_owned()),
            _ => args.help(),
        }
    }
    let source = source.unwrap_or_else(|| args.fail("the IDN source to feed"));
    let url = url.unwrap_or_else(|| args.fail("--url ws://127.0.0.1:PORT/voice/ws"));
    let token = token(&args);
    parent_pipe();
    let stop = std::sync::atomic::AtomicBool::new(false);
    if let Err(e) = idn_ffmpeg::voice_feed::run(&source, &url, &token, vox, &stop) {
        eprintln!("idn-codec: {e}");
        std::process::exit(1);
    }
}

/// `idn-codec voice-out`: an IDN Voice channel as an IDN source (§V.10), as the Site
/// Manager starts it.
pub(super) fn voice_out(mut args: idn_open_protocol::cli::Args) {
    let (mut name, mut url) = (None::<String>, None::<String>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => url = Some(args.value("--url")),
            "--name" => name = Some(args.value("--name")),
            _ => args.help(),
        }
    }
    let url = url.unwrap_or_else(|| args.fail("--url ws://127.0.0.1:PORT/voice/ws"));
    let name = name.unwrap_or_else(|| args.fail("--name: the source's name"));
    let token = token(&args);
    parent_pipe();
    let stop = std::sync::atomic::AtomicBool::new(false);
    if let Err(e) = idn_ffmpeg::voice_out::run(&url, &token, &name, &stop) {
        eprintln!("idn-codec: {e}");
        std::process::exit(1);
    }
}

fn token(args: &idn_open_protocol::cli::Args) -> String {
    idn_common::switches::value("IDN_VOICE_FEED_TOKEN")
        .unwrap_or_else(|| args.fail("IDN_VOICE_FEED_TOKEN (the Site Manager sets it)"))
}

/// The Site Manager holds our standard input: closed, we end.
fn parent_pipe() {
    std::thread::spawn(|| {
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin();
        while std::io::Read::read(&mut stdin, &mut buf).is_ok_and(|n| n > 0) {}
        std::process::exit(0);
    });
}
