//! Every environment variable the programs read, in one place, so that all of them say
//! "off" one way (scattered through the code, they had three). The switches that turn
//! something on or off take one set of values: `0`, `off`, `false` or `no` (any case) is
//! off; anything else set is on. A test checks that every `IDN_*` name in the code is here.

/// What a variable is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// On unless set off: for comparing against the way it was done before.
    Default,
    /// Off unless set on.
    Opt,
    /// A value (a path, an address, a number, a choice).
    Value,
    /// A secret: read once, never logged.
    Secret,
    /// Read when building, not by the programs.
    Build,
    /// For tests, benchmarks and lab runs only.
    Test,
}

/// A variable: its name, its kind, and what it does (who reads it).
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    pub name: &'static str,
    pub kind: Kind,
    pub what: &'static str,
}

const fn s(name: &'static str, kind: Kind, what: &'static str) -> Switch {
    Switch { name, kind, what }
}

use Kind::*;

/// All of them, by name.
pub const ALL: &[Switch] = &[
    s(
        "IDN_ADMIN_PASSWORD",
        Secret,
        "Site Manager | Industrial: the first administrator's password",
    ),
    s(
        "IDN_AOIP_CAPS",
        Value,
        "the network audio device: plain leaves its sample-rate and encoding capability bits clear (a controller then says it doesn't support them; the default sets them, shown as fixed)",
    ),
    s(
        "IDN_API_KEY",
        Secret,
        "Site Manager | Industrial: the API key",
    ),
    s(
        "IDN_APPLIANCE",
        Opt,
        "the IDN Node on an IDN Appliance (R.29): its welcome joins a site or makes one here, no \"not now\" (§N.11.7)",
    ),
    s(
        "IDN_AUDIO_HOST",
        Value,
        "idn-audio: the sound system to use (ALSA, PipeWire, JACK, WASAPI, ASIO)",
    ),
    s(
        "IDN_AUDIO_PLUGINS",
        Test,
        "plugin host test: a folder holding IDN.clap and IDN.vst3 (IDN Send, IDN Return)",
    ),
    s(
        "IDN_BACKUP_PASSPHRASE",
        Secret,
        "Site Manager: the passphrase site backups are sealed with",
    ),
    s(
        "IDN_BENCH_NO_FTZ",
        Test,
        "desk_bench: 1 leaves denormals as they are",
    ),
    s("IDN_BENCH_TRACE", Test, "desk_bench: 1 traces one block"),
    s(
        "IDN_BLOCKED_SIGNAL_CHILD",
        Test,
        "shutdown test: marks its child process",
    ),
    s("IDN_BUILD", Build, "the build's tag (Build TAG)"),
    s("IDN_BUILD_DATE", Build, "the build's date"),
    s(
        "IDN_CAPTIONS_SCORES",
        Opt,
        "captions: log the lines sent with whisper's scores",
    ),
    s(
        "IDN_CAPTIONS_WORKER",
        Value,
        "captions: the program whisper runs in at low priority (`PROGRAM --idn-captions-worker`; default idn next to the program, else on PATH); off: in the program itself",
    ),
    s(
        "IDN_CAPTURE_GPU",
        Default,
        "idn capture: pictures made planar on the GPU while receivers take them there (§E.27)",
    ),
    s(
        "IDN_CARD_ADMIN_PIN",
        Secret,
        "idn-release: the YubiKey's admin PIN",
    ),
    s("IDN_CARD_PIN", Secret, "idn-release: the YubiKey's PIN"),
    s(
        "IDN_CLOCK",
        Default,
        "the Node: the site clock (PTP); off runs none (tests)",
    ),
    s(
        "IDN_CLOCK_CRASH_TEST",
        Test,
        "the desk's and the switcher's clocks: one panic about 5 s in, to see them restart",
    ),
    s(
        "IDN_CLOCK_PORTS",
        Value,
        "the Node: the PTP ports (event,general), for tests",
    ),
    s(
        "IDN_CPU_IDCT",
        Value,
        "SpeedHQ decoding on the CPU: f32 (fast) or f64 (exact) inverse DCT",
    ),
    s(
        "IDN_CRASH_TEST",
        Test,
        "crash reports: crash on purpose to test them",
    ),
    s(
        "IDN_CWASIO_DIR",
        Value,
        "ASIO on Linux: another folder than /etc/cwASIO for cwASIO's list of drivers (asio-interface.md §A.5)",
    ),
    s(
        "IDN_DATA",
        Value,
        "Site Manager | Industrial: its data folder",
    ),
    s(
        "IDN_DATA_PANEL_KEY",
        Secret,
        "IDN Data: the panel key handed from the Node",
    ),
    s(
        "IDN_DECODER",
        Value,
        "video decoders: auto, gpu or software",
    ),
    s(
        "IDN_DESK_BLOCK",
        Value,
        "the IDN Mixer: this machine's default block for a desk with none saved (32, 64, 128 or 256; the appliance: 64)",
    ),
    s(
        "IDN_DISPLAY_GPU_DECODE",
        Default,
        "idn-display: SpeedHQ's inverse DCT on its GPU (§D.1)",
    ),
    s(
        "IDN_DISPLAY_IDCT",
        Value,
        "idn-display: the GPU's inverse DCT in f32 (else f64 where the GPU has it)",
    ),
    s("IDN_FUZZ_ITERS", Test, "fuzz tests: how many inputs"),
    s(
        "IDN_GPU_DOWNLOADS",
        Value,
        "decoders: GPU pictures downloaded at once (default 2)",
    ),
    s(
        "IDN_GPU_ENCODE",
        Default,
        "recording and streaming: pictures drawn into the encoder on the GPU (§E.27)",
    ),
    s(
        "IDN_GPU_ENCODER",
        Value,
        "the GPU encoder tried first: vaapi, nvenc, nvenc-cuda, vulkan, amf, qsv",
    ),
    s(
        "IDN_GPU_SHQ",
        Default,
        "outputs: SpeedHQ encoded on the GPU (§E.27)",
    ),
    s(
        "IDN_HTTP",
        Value,
        "Site Manager | Industrial: its web address",
    ),
    s(
        "IDN_INDUSTRIAL_ACTIONS",
        Value,
        "IDN Node | Industrial (Linux service): its actions only from this root-owned file",
    ),
    s(
        "IDN_INPUT_RECORD",
        Test,
        "remote input recorded, not sent to the screen (tests)",
    ),
    s(
        "IDN_KEY_PASSPHRASE",
        Secret,
        "idn-release: the key file's passphrase",
    ),
    s(
        "IDN_LANE_THROUGH",
        Default,
        "sound lanes on the consumer's clock (by site time, at its rate) pass their samples through unresampled",
    ),
    s(
        "IDN_LINK_CC",
        Value,
        "the site link: its congestion control",
    ),
    s(
        "IDN_LISTEN_PASSWORD",
        Secret,
        "idn codec listen: the listeners' password",
    ),
    s(
        "IDN_MACHINE",
        Default,
        "machine/1: shared memory between programs on one machine (§E.26)",
    ),
    s(
        "IDN_MALLOC_ARENAS",
        Value,
        "glibc's most allocator arenas in IDN's long-running programs (1-64; default 4)",
    ),
    s(
        "IDN_MALLOC_MMAP_KB",
        Value,
        "glibc's fixed mmap threshold in IDN's long-running programs, KiB (16-32768; default 256)",
    ),
    s(
        "IDN_MALLOC_TUNE",
        Default,
        "glibc's allocator kept from holding freed memory (idn_common::memory; off: glibc's own behaviour)",
    ),
    s(
        "IDN_MEDIA_PORTS",
        Value,
        "media ports, one per connection: FIRST-LAST (default 29300-29499), or off (the system picks)",
    ),
    s(
        "IDN_MQTT",
        Value,
        "Site Manager | Industrial: its MQTT address",
    ),
    s(
        "IDN_NETAUDIO_UNCHECKED",
        Test,
        "the IDN Node: offers the network audio device settings not yet checked on real equipment (R.32 stage 2; lab runs only)",
    ),
    s("IDN_NODE", Value, "the IDN Node's endpoint; off: none"),
    s(
        "IDN_NODE_INPUT",
        Secret,
        "idn-node set KEY=-: the secret value read instead of standard input (a LAN password, a site password)",
    ),
    s(
        "IDN_OBS_CLANG_ARGS",
        Build,
        "OBS plugin: extra arguments for its bindings",
    ),
    s(
        "IDN_OBS_IDCT",
        Value,
        "OBS plugin: the GPU's inverse DCT in f32",
    ),
    s("IDN_OBS_INCLUDE", Build, "OBS plugin: OBS's headers"),
    s("IDN_OBS_LIB", Build, "OBS plugin: OBS's libraries"),
    s(
        "IDN_OBS_SHQ_GPU",
        Value,
        "OBS plugin: SpeedHQ on the GPU (on) or the CPU (off); else as the source says",
    ),
    s(
        "IDN_ODBC_LIB",
        Build,
        "IDN Industrial with the odbc feature: where libodbc is",
    ),
    s("IDN_PASSPHRASE", Secret, "the site's passphrase"),
    s(
        "IDN_PLUGINS_NO_SHM",
        Opt,
        "plugin host: sound through its pipe, not shared memory",
    ),
    s("IDN_PLUGIN_HOST", Value, "the plugin host program to start"),
    s(
        "IDN_PUBLIC_URLS",
        Value,
        "Site Manager | Industrial: its public addresses",
    ),
    s(
        "IDN_QUIC",
        Value,
        "Site Manager | Industrial: its QUIC address",
    ),
    s(
        "IDN_REAL_PLUGINS",
        Test,
        "plugin host test: a folder of real plugins",
    ),
    s(
        "IDN_RELEASE_MAX_GB",
        Value,
        "IDN Site Manager: the largest release file it takes, in GiB (default 8)",
    ),
    s(
        "IDN_REQUIRE_FFMPEG",
        Test,
        "HX tests: fail, not skip, without FFmpeg",
    ),
    s("IDN_ROUTING_LOG", Opt, "routing: log each route's notes"),
    s(
        "IDN_SHARE_DIRECT",
        Default,
        "Wayland screens: read straight from the compositor where it allows (else the portal)",
    ),
    s(
        "IDN_SHARE_DMABUF",
        Default,
        "Wayland screens: the desktop's pictures taken as DMA-BUF on the GPU",
    ),
    s(
        "IDN_SHOT",
        Test,
        "plugin host test: screenshots of plugin windows",
    ),
    s(
        "IDN_SHQ_GPU_MOST_MB",
        Value,
        "tests: the largest picture SpeedHQ decoding takes on the GPU, in MB (larger ones on the CPU)",
    ),
    s(
        "IDN_SITE_HOST_NAME",
        Value,
        "the name of the machine a Site Manager runs on (its Node sets it; a restore)",
    ),
    s(
        "IDN_SITE_HOST_SPKI",
        Value,
        "the key of the machine a Site Manager runs on, hex (its Node sets it; a restore)",
    ),
    s(
        "IDN_SITE_MANAGER_DIR",
        Value,
        "the Site Manager's state folder",
    ),
    s("IDN_SITE_PASSWORD", Secret, "the Site Manager's password"),
    s(
        "IDN_SITE_SERVICE",
        Test,
        "the site's mDNS service type (tests: one of their own)",
    ),
    s(
        "IDN_SQLITE_ODBC",
        Test,
        "IDN Industrial's SQL tests: the SQLite ODBC driver's library",
    ),
    s(
        "IDN_STALLED_HOOK_CHILD",
        Test,
        "shutdown test: marks its child process",
    ),
    s(
        "IDN_STATE_DIR",
        Value,
        "where the programs keep their state",
    ),
    s(
        "IDN_SUPERVISED",
        Opt,
        "set by the IDN Node's supervisor for the programs it runs: a program ending with 75 (supervisor::AGAIN_NOW) is started again at once (a desk moved to a new sample rate)",
    ),
    s(
        "IDN_SWITCHER_COEFS",
        Default,
        "switcher: SpeedHQ cameras reconstructed on its GPU",
    ),
    s(
        "IDN_SWITCHER_FILTERS_CPU",
        Opt,
        "switcher: inputs' filters on the CPU",
    ),
    s(
        "IDN_SWITCHER_GPU_COPIES",
        Opt,
        "switcher: on an integrated GPU, copy pictures as on a card of its own",
    ),
    s(
        "IDN_SWITCHER_GPU_INPUTS",
        Default,
        "switcher: inputs given its GPU (pictures kept there, §E.27)",
    ),
    s(
        "IDN_SWITCHER_GPU_MIXES",
        Default,
        "switcher: mixes sent from the GPU while taken there",
    ),
    s(
        "IDN_SWITCHER_IDCT",
        Value,
        "switcher: the GPU's inverse DCT in f32",
    ),
    s(
        "IDN_SWITCHER_LOCAL",
        Default,
        "switcher: its own file players in-process",
    ),
    s(
        "IDN_SWITCHER_RING_READ",
        Default,
        "switcher: the program read back straight into shared memory",
    ),
    s(
        "IDN_SWITCHER_SCENES_CPU",
        Opt,
        "switcher: scenes drawn on the CPU",
    ),
    s(
        "IDN_SWITCHER_SOFTWARE_GPU",
        Opt,
        "switcher: allow a software GPU",
    ),
    s(
        "IDN_SYNC_MAX_NEED_MS",
        Value,
        "sync: the most a receiver may ask to wait",
    ),
    s(
        "IDN_TEST_IDCT",
        Test,
        "GPU tests: the inverse DCT's precision",
    ),
    s("IDN_TEST_LOG", Test, "tests: the library's log printed"),
    s(
        "IDN_TEST_PLAIN_NDI",
        Test,
        "tests: talk the plain NDI-Compatible protocol (no IDN extensions)",
    ),
    s(
        "IDN_TOKEN",
        Secret,
        "idn iem listen: the desk token when --token isn't given",
    ),
    s(
        "IDN_TRUST_PROXY",
        Value,
        "Site Manager | Industrial: addresses of tunnels or proxies whose forwarded-address headers are believed (comma-separated; loopback always)",
    ),
    s(
        "IDN_UPDATE_CHECK_S",
        Value,
        "the Node: seconds between update checks",
    ),
    s("IDN_VAAPI_DEVICE", Value, "VA-API: the render node"),
    s(
        "IDN_VIEW_HOSTS",
        Value,
        "viewing and listening pages without a password: other names they answer to (comma-separated)",
    ),
    s(
        "IDN_VIEW_PASSWORD",
        Secret,
        "idn codec: the viewers' password",
    ),
    s(
        "IDN_VOICE_CERT_SHA256",
        Value,
        "IDN Voice: the SHA-256 of the certificate handsets reach it by (a proxy's), told to handsets at enrolment (idn-voice.md §V.13)",
    ),
    s(
        "IDN_VOICE_FEED_TOKEN",
        Secret,
        "idn codec voice-feed / voice-out: the token the Site Manager gave the program (§V.9)",
    ),
    s(
        "IDN_VOICE_HTTPS",
        Value,
        "IDN Voice in Industrial: its HTTPS listener, and IDN's link on the same UDP address (ADDR:PORT, PORT, or off; default 0.0.0.0:29203)",
    ),
    s(
        "IDN_VOICE_PORT",
        Value,
        "IDN Site Manager: Voice's HTTPS port (default the panel's + 1; off: no Voice)",
    ),
    s(
        "IDN_VOICE_PUBLIC",
        Value,
        "Voice: the address WebRTC clients are told to send to (this server's public address)",
    ),
    s(
        "IDN_VOICE_UDP",
        Value,
        "Voice: the WebRTC UDP port (default 0.0.0.0:29207; off: WebSocket only)",
    ),
    s(
        "IDN_VOICE_URLS",
        Value,
        "IDN Voice: the addresses handsets are told to reach it at (comma-separated https:// URLs)",
    ),
    s(
        "IDN_VULKAN_ENCODE",
        Default,
        "Vulkan Video encoders (FFmpeg's *_vulkan) offered",
    ),
    s(
        "IDN_WASAPI_LOW",
        Default,
        "idn-audio on Windows: the low-latency shared mode (IAudioClient3) under 10 ms",
    ),
    s("IDN_WORKER_THREADS", Value, "the worker pool's threads"),
];

/// The variable named `name` as registered; panics (in tests and debug builds) for one
/// that isn't, so a new one can't be read without being listed.
fn known(name: &str) -> &'static Switch {
    let found = ALL.iter().find(|s| s.name == name);
    debug_assert!(
        found.is_some(),
        "{name} is not in idn_common::switches::ALL"
    );
    found.unwrap_or(&ALL[0])
}

/// Whether `v` says off: the off words of [`crate::text::parse_on_off`] (`0`, `off`,
/// `false`, `no`, any case), the one list settings and switches share.
pub fn says_off(v: &str) -> bool {
    crate::text::parse_on_off(v) == Some(false)
}

/// Whether the switch `name` is on: a [`Kind::Default`] one unless set off, any other
/// only when set and not off.
pub fn on(name: &str) -> bool {
    let k = known(name).kind;
    match std::env::var(name) {
        Ok(v) => !says_off(&v),
        Err(_) => k == Kind::Default,
    }
}

/// Whether the switch `name` is off (`!on`).
pub fn off(name: &str) -> bool {
    !on(name)
}

/// The variable `name`'s value, if set and not empty.
pub fn value(name: &str) -> Option<String> {
    known(name);
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Secret switch `name` if set, else the first line typed on standard input after
/// `prompt` (printed on standard output), its line end trimmed: a passphrase for a
/// command run by hand or from a script.
pub fn secret_or_ask(name: &str, prompt: &str) -> std::io::Result<String> {
    if let Some(v) = value(name) {
        return Ok(v);
    }
    println!("{prompt}");
    let mut l = String::new();
    std::io::stdin().read_line(&mut l)?;
    Ok(l.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_once_each_in_order() {
        for w in ALL.windows(2) {
            assert!(w[0].name < w[1].name, "{} before {}", w[0].name, w[1].name);
        }
    }

    #[test]
    fn off_is_said_one_way() {
        for v in ["0", "off", "OFF", "false", "No", " 0 "] {
            assert!(says_off(v), "{v}");
        }
        for v in ["1", "on", "yes", "true", "f32"] {
            assert!(!says_off(v), "{v}");
        }
    }

    /// Every `IDN_*` name in the code's sources is listed (so none grows up unknown again).
    #[test]
    fn every_name_in_the_code_is_listed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut missing = std::collections::BTreeSet::new();
        let mut stack = vec![
            root.join("core"),
            root.join("media"),
            root.join("industrial"),
            root.join("tools"),
        ];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n != "target") {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap_or_default();
                    for part in text.split("\"IDN_").skip(1) {
                        let name: String = part
                            .chars()
                            .take_while(|c| {
                                c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_'
                            })
                            .collect();
                        if !name.is_empty()
                            && part[name.len()..].starts_with('"')
                            && !ALL.iter().any(|s| s.name[4..] == name)
                        {
                            missing.insert(format!("IDN_{name} ({})", p.display()));
                        }
                    }
                }
            }
        }
        assert!(missing.is_empty(), "not in switches::ALL: {missing:?}");
    }
}
