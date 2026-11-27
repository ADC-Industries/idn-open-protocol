//! An input's settings: the transports it may use and [`InputConfig`].

use super::*;

/// Ways the source may send media. The source picks one of those offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The IDN link (`link/1`): with IDN sources, one QUIC connection for everything,
    /// always encrypted. Other sources ignore it. On by default.
    Link,
    /// QUIC over UDP: the default and usually best choice.
    Quic,
    /// Several TCP connections, for networks that block UDP.
    Tcp,
    /// Plain UDP with error correction.
    Udp,
    /// Multicast: one stream shared by all receivers, if the source enables it and the
    /// network supports it.
    Multicast,
}

/// Settings for [`Input::open`]. Start from [`InputConfig::new`] and change what you
/// need.
#[derive(Debug, Clone)]
pub struct InputConfig {
    /// What to receive: a full source name `HOST (name)` as listed by
    /// [`discovery`](crate::discovery), or an address `ip:port` / `host:port` (an `ip`
    /// alone means port 5961).
    pub source: String,
    /// This receiver's name as the source sees it (the full name is `HOST (name)`).
    /// Default: `receiver`.
    pub receiver_name: String,
    /// Media transports to offer, in no particular order. Default: the IDN link and
    /// QUIC. Empty: media on the control connection (TCP), which works through any
    /// firewall that allows the connection at all.
    pub transports: Vec<Transport>,
    /// Local UDP port for QUIC media. 0 (the default) picks any free port; set one if a
    /// firewall must allow it.
    pub quic_port: u16,
    /// Ask for the source's low-bandwidth stream (about 640 pixels wide), e.g. for
    /// multiviewer tiles. Can be changed later with [`Input::set_low_bandwidth`].
    pub low_bandwidth: bool,
    /// Ask for the older SpeedHQ format. Only for testing very old senders.
    pub legacy: bool,
    /// Look names up with this Discovery Server (`host` or `host:port`) instead of mDNS.
    pub discovery_server: Option<String>,
    /// The IPv4 address of the network interface to use for discovery and multicast.
    /// `None`: all interfaces.
    pub interface: Option<Ipv4Addr>,
    /// Keep trying while the source is missing or after it goes away (every 2 s, faster
    /// right after an IDN source restarts). Default: on.
    pub reconnect: bool,
    /// Also convert decoded pictures to RGBA ([`VideoIn::rgba`]) with this alpha
    /// convention. `None` (the default): Y'CbCr planes only, e.g. for converting on the
    /// GPU.
    pub rgba: Option<AlphaMode>,
    /// How Y'CbCr is converted to RGBA when `rgba` is set, where the source doesn't say:
    /// what a source announces wins (range and alpha convention from IDN sources, the
    /// matrix from `ndi_color_info`, e.g. BT.2020). The default matches NDI. The transfer
    /// function (PQ, HLG) is not converted; [`VideoIn::color`] reports it.
    pub color: ColorConfig,
    /// How many media events may wait unread. When the application falls behind, the
    /// oldest are dropped ([`InputStats::dropped`]); state events are never dropped.
    /// Default: 8.
    pub queue: usize,
    /// Skip decoding a picture when a newer one has already arrived, so that on an
    /// overloaded machine the input shows current pictures at a lower rate instead of
    /// falling behind ([`InputStats::late`]). Turn it off to decode every frame, e.g. for
    /// recording. Default: on.
    pub skip_late: bool,
    /// The most pictures a second this input hands over, (numerator, denominator): of a
    /// source at twice that or more, every second picture (or third, …) is left out
    /// before decoding (SpeedHQ, taken in-process) or after it (compressed video, by
    /// decoders that do so: `idn-ffmpeg`'s `Ffmpeg::with_picture_rate`), counted in
    /// [`InputStats::thinned`]. For a program at one rate (a switcher's 2160p30 program
    /// and a 4K60 camera). `None` (the default): every picture.
    pub picture_rate: Option<(u32, u32)>,
    /// Ask IDN sources for bit-exact float audio instead of the standard compressed
    /// audio. NDI sources ignore the request. Default: on (every IDN
    /// receiver lossless): about 3 Mbit/s for stereo instead of 1.5, for every channel the
    /// receiver takes (all of them unless [`audio_channels`](Self::audio_channels) says).
    /// Not asked for by a preview (`low_bandwidth` or `picture_every`), nor on multicast,
    /// which carries the standard audio to everyone.
    pub lossless_audio: bool,
    /// Encryption with IDN sources that share the site passphrase; see
    /// [`secure`](crate::secure). `None` (the default) is off.
    pub secure: Option<crate::secure::SecureConfig>,
    /// Also reconstruct pictures with 3 extra fraction bits ([`Picture::precise`]), for
    /// 10-bit or HDR output. Costs some decode time. Default: off.
    pub precise: bool,
    /// [`precise`](Self::precise) only for a source in this HDR transfer (an HDR
    /// program's inputs: its other sources are lifted from 8 bits anyway, and stay as
    /// coefficients for the GPU, decoded at half size where asked). Default: `None`.
    pub precise_hdr: Option<crate::color::Transfer>,
    /// Deliver SpeedHQ 4:2:2 and 4:2:0 pictures (with or without alpha, progressive,
    /// full size) as dequantised coefficients ([`Picture::coefficients`], `planes` empty)
    /// instead of samples, for an application that reconstructs them on the GPU (the
    /// switcher, the OBS plugin):
    /// the CPU then only reads the bitstream, 36–42% of a 2160p picture's decode saved.
    /// Pictures that can't (other layouts, fields, half size, [`precise`] from a source
    /// above 8 bits or HDR, regions, RGBA output) come as samples. Default: off.
    ///
    /// [`precise`]: InputConfig::precise
    pub coefficients: bool,
    /// With [`discovery_server`](InputConfig::discovery_server) set: report this input's
    /// live status there (source, frames per second, drops, encryption) for site-wide
    /// monitoring with [`Monitor`](crate::discovery::Monitor). It can't be switched to
    /// another source remotely. Default: on.
    pub monitoring: bool,
    /// When the source is an output of this same process (`127.0.0.1:PORT`): take its
    /// pictures and sound as they are, with no encode, connection or decode (a switcher
    /// playing a file). Nothing reaches the output (tally, metadata, size and bandwidth
    /// requests go nowhere) and RGBA isn't made. Default: off (receive over the network,
    /// as from any other source).
    pub local: bool,
    /// The largest picture this input decodes; larger ones are rejected unread
    /// ([`InputStats::video_errors`]). A picture's size is the sender's claim, and a few
    /// bytes can claim a huge one: each costs its full size in memory and decode time.
    /// Default: 4096 × 4096, at most 4096 × 2160 pixels (DCI 4K, either orientation).
    pub max_picture: shq::Limits,
    /// This device's key and the keys of sources seen before, for the IDN link
    /// ([`Transport::Link`]). `None` (the default): [`LinkConfig::default`].
    ///
    /// [`LinkConfig::default`]: crate::link::LinkConfig::default
    pub link: Option<crate::link::LinkConfig>,
    /// One stream per video frame on IDN links (`frames/1`): a frame that can no longer
    /// arrive in time is abandoned instead of delaying the next ones, which helps most on
    /// Wi-Fi. Used when the source has it on too. Default: off.
    pub frame_streams: bool,
    /// Frame-accurate presentation with other displays of a sync group (`sync/1`): each
    /// frame is handed over at its presentation time, the same moment on every display
    /// of the group. `None` (the default): off, frames are handed over on arrival. See
    /// [`sync`](crate::sync).
    pub sync: Option<crate::sync::SyncConfig>,
    /// The largest picture this input needs, (width, height): IDN sources scale to fit
    /// inside it (`adapt/1`), saving bandwidth and decoding, e.g. for a multiviewer tile.
    /// Pictures still twice its size or more (NDI sources, which can't scale, e.g. a 4K
    /// camera on a 1080p screen) are decoded at half size: about two thirds of the CPU
    /// time on camera-like content (every coefficient is still read), and a quarter of
    /// the samples to convert and hand over. Change it later with [`Input::set_max_size`]. `None` (the default):
    /// full size.
    pub max_size: Option<(u32, u32)>,
    /// One picture in this many is enough (`adapt/1`'s `x_idn_every`, 2 to 600): IDN
    /// sources send no others, nor make them (a preview's picture every few seconds).
    /// NDI sources ignore it. `None` (the default): every picture.
    pub picture_every: Option<u32>,
    /// A video decoder for compressed video (`codec/1`, and NDI|HX sources): H.264,
    /// HEVC or AV1 is then handed over as pictures, like SpeedHQ. With
    /// [`CodecUse::Prefer`](crate::codec::CodecUse) or `Allow`, IDN sources are asked for
    /// it. `None` (the default): compressed video is handed over as
    /// [`InputEvent::Compressed`]. See [`codec`](crate::codec).
    pub codec: Option<crate::codec::CodecInput>,
    /// Changed regions only (`regions/1`): an IDN source with the option on sends only
    /// the parts of each picture that changed (a static slide costs almost nothing).
    /// Pictures are handed over whole as usual. Default: off.
    pub regions: bool,
    /// Decode two SpeedHQ pictures at once (each also split across the worker pool, see
    /// [`set_worker_threads`](crate::set_worker_threads)), so that an input keeps up with
    /// 4K at 50 or 60 frames a second where one picture takes longer than a frame
    /// interval to decode. Pictures are still handed over in order. Off: one at a time,
    /// late pictures skipped instead ([`skip_late`](InputConfig::skip_late)). Change it
    /// later with [`Input::set_parallel_decode`]. `None` (the default): the machine's
    /// setting in the IDN Node ([`crate::node::video`]) when the input opens, else on.
    pub parallel_decode: Option<bool>,
    /// Ask for video. Off: sound and metadata only, for an input that only plays sound
    /// (the source sends no pictures). Default: on.
    pub receive_video: bool,
    /// Take a source on this machine's pictures through its shared memory (`machine/1`,
    /// §E.26) when it offers it. Off: over the connection, as from another machine (to
    /// measure a transport here). Default: on (`IDN_MACHINE=off` turns it off for the
    /// whole program).
    pub machine: bool,
    /// This application's GPU (its wgpu device): a source on this machine on the same GPU
    /// that has its pictures there hands them over GPU to GPU (`gpu/1`, §E.27), and
    /// [`VideoIn::gpu`] has them, with no samples in [`VideoIn::picture`]'s planes. For an
    /// application that draws on the GPU. Default: none.
    #[cfg(feature = "gpu")]
    pub gpu: Option<crate::gpu::GpuHandIn>,
    /// The source's audio channels to receive, numbered from 0, in this order
    /// (`channels/1`, §E.21): IDN sources send only those, NDI sources everything, so
    /// see [`AudioIn::source_channels`] for what a block holds. `None` (the default):
    /// every channel. Change it later with [`Input::set_audio_channels`].
    pub audio_channels: Option<Vec<u16>>,
}

impl InputConfig {
    /// Whether to ask for lossless sound (`audio-f32`): as
    /// [`lossless_audio`](Self::lossless_audio) says, except for a preview (the
    /// low-bandwidth stream, or a picture now and then: the multiview's tiles, the IDN
    /// Remote's thumbnails), whose sound is thrown away or only metered.
    pub(crate) fn wants_lossless(&self) -> bool {
        self.lossless_audio && !self.low_bandwidth && self.picture_every.is_none()
    }

    /// Fills in what the site sets for this input (`options/1`, §E.20): `link.enabled`,
    /// `frames.enabled`,
    /// `regions.enabled`, `codec.use` / `codec.prefer` (when a decoder is configured),
    /// `sync.group` / `sync.delay`. Call it first, then set the user's own values: local
    /// wins. `secure.require` is followed live by the input itself
    /// ([`SecureConfig::requires`](crate::secure::SecureConfig::requires)), for the
    /// input named by its receiver name.
    pub fn apply_site(&mut self, site: &crate::trust::Resolved) {
        // The link is used when it is among the transports (`link` only holds its
        // settings), so the site's switch acts on the list.
        match site.bool("link.enabled") {
            Some(true) => {
                if !self.transports.contains(&Transport::Link) {
                    self.transports.insert(0, Transport::Link);
                }
                if self.link.is_none() {
                    self.link = Some(crate::link::LinkConfig::default());
                }
            }
            Some(false) => self.transports.retain(|t| *t != Transport::Link),
            None => {}
        }
        if let Some(v) = site.bool("frames.enabled") {
            self.frame_streams = v;
        }
        if let Some(v) = site.bool("regions.enabled") {
            self.regions = v;
        }
        if let Some(c) = self.codec.as_mut() {
            if let Some(u) = site.get("codec.use") {
                c.use_ = match u {
                    "never" => crate::codec::CodecUse::Never,
                    "prefer" => crate::codec::CodecUse::Prefer,
                    _ => crate::codec::CodecUse::Allow,
                };
            }
            if let Some(list) = site.list("codec.prefer") {
                c.prefer = list
                    .iter()
                    .filter_map(|n| crate::codec::Codec::from_name(n))
                    .collect();
            }
        }
        match site.get("sync.group") {
            Some("") => self.sync = None,
            Some(g) => {
                let delay = self
                    .sync
                    .as_ref()
                    .map_or(crate::sync::SyncDelay::Auto, |s| s.delay);
                self.sync = Some(crate::sync::SyncConfig::new(g, delay));
            }
            None => {}
        }
        if let (Some(s), Some(d)) = (self.sync.as_mut(), site.get("sync.delay")) {
            let parsed = match d.split_once(':') {
                None if d == "auto" => Some(crate::sync::SyncDelay::Auto),
                Some(("frames", n)) => n.parse().ok().map(crate::sync::SyncDelay::Frames),
                Some(("ms", n)) => n.parse().ok().map(crate::sync::SyncDelay::Millis),
                _ => None,
            };
            if let Some(p) = parsed {
                s.delay = p;
            }
        }
    }

    /// Defaults for receiving `source`: QUIC, full quality, reconnect on, planes only
    /// (no RGBA), a queue of 8, no encryption.
    pub fn new(source: &str) -> Self {
        InputConfig {
            source: source.into(),
            receiver_name: "receiver".into(),
            transports: vec![Transport::Link, Transport::Quic],
            quic_port: 0,
            low_bandwidth: false,
            legacy: false,
            discovery_server: None,
            interface: None,
            reconnect: true,
            rgba: None,
            color: ColorConfig::default(),
            queue: 8,
            skip_late: true,
            picture_rate: None,
            parallel_decode: None,
            receive_video: true,
            machine: true,
            #[cfg(feature = "gpu")]
            gpu: None,
            audio_channels: None,
            lossless_audio: true,
            secure: None,
            precise: false,
            precise_hdr: None,
            coefficients: false,
            monitoring: true,
            local: false,
            max_picture: shq::Limits {
                max_width: 4096,
                max_height: 4096,
                max_pixels: 4096 * 2160,
            },
            link: None,
            frame_streams: false,
            sync: None,
            max_size: None,
            picture_every: None,
            codec: None,
            regions: false,
        }
    }
}
