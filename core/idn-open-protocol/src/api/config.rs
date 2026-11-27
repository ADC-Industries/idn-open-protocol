//! Settings for the publisher and its outputs, and starting the publisher.

use super::*;

/// How this process's outputs are announced and protected. Shared by all its outputs.
///
/// The default announces on the local network (mDNS) in the group `public`, without
/// encryption.
#[derive(Debug, Clone)]
pub struct PublisherConfig {
    /// Announce on the local network with mDNS. `None` (the default) means on, unless
    /// a Discovery Server is set: apps configured for a server also browse mDNS and would
    /// list each source twice.
    pub mdns: Option<bool>,
    /// Discovery Server as `host` or `host:port` (default port 5959), for networks where
    /// mDNS doesn't reach every room.
    pub discovery_server: Option<String>,
    /// Groups the outputs belong to. Receivers only list sources in their own groups.
    /// Default: `["public"]`.
    pub groups: Vec<String>,
    /// Encryption with IDN receivers that share the site passphrase. `None` (the
    /// default) is off. See [`SecureConfig`](crate::secure::SecureConfig): with
    /// `require` set, receivers that can't encrypt are refused, NDI receivers included.
    pub secure: Option<crate::secure::SecureConfig>,
    /// With a Discovery Server: report each output's live status there (receivers,
    /// frames per second, encryption, tally) for site-wide monitoring with
    /// [`Monitor`](crate::discovery::Monitor). Default: on. No effect without a server.
    pub monitoring: bool,
    /// The IDN link (`link/1`): IDN receivers connect over one QUIC connection,
    /// encrypted always, on UDP at each output's port. NDI receivers are unaffected.
    /// Default: on, with this device's key in [`LinkConfig::default_dir`]. `None`: off.
    ///
    /// [`LinkConfig::default_dir`]: crate::link::LinkConfig::default_dir
    pub link: Option<crate::link::LinkConfig>,
    /// Serve the host listing port (TCP 5960, §5.2), which NDI tools configured with
    /// this host's address use instead of mDNS. Default: on. If another NDI process on
    /// this host holds the port, that process lists its sources and this one goes
    /// without (logged, not an error).
    pub listing: bool,
    /// Announced nowhere: no mDNS, no Discovery Server, no listing port, not registered
    /// with the IDN Node; receivers reach the outputs by address only (a program's
    /// private feeds, such as the switcher's media players). Default: off.
    pub unlisted: bool,
    /// Announce the outputs as this host's (`HOST (name)`) instead of this machine's:
    /// for sources offered again on behalf of another machine, such as a remote site's
    /// over the site link (spec/extensions/idn-remote.md X.3). `None` (the default): this
    /// machine's name.
    pub host_name: Option<String>,
}

impl PublisherConfig {
    /// Fills in what the site sets for this device (`options/1`, §E.20): `link.enabled`
    /// and `listing.enabled`. Call it first, then set the user's own values: local wins.
    /// `secure.require` needs nothing here: [`SecureConfig`](crate::secure::SecureConfig)
    /// with a site follows it live ([`requires`](crate::secure::SecureConfig::requires)).
    pub fn apply_site(&mut self, site: &crate::trust::Resolved) {
        match site.bool("link.enabled") {
            Some(true) if self.link.is_none() => {
                self.link = Some(crate::link::LinkConfig::default())
            }
            Some(false) => self.link = None,
            _ => {}
        }
        if let Some(v) = site.bool("listing.enabled") {
            self.listing = v;
        }
    }
}

impl Default for PublisherConfig {
    fn default() -> Self {
        PublisherConfig {
            listing: true,
            mdns: None,
            discovery_server: None,
            groups: vec!["public".into()],
            secure: None,
            monitoring: true,
            link: Some(crate::link::LinkConfig::default()),
            unlisted: false,
            host_name: None,
        }
    }
}

impl Publisher {
    /// Starts announcing. Fails if the host's network address can't be determined, the
    /// mDNS socket can't be opened, or the Discovery Server address doesn't resolve.
    pub fn new(cfg: PublisherConfig) -> io::Result<Self> {
        // With the IDN Node's encryption, its machine settings too (spec/extensions/
        // idn-node.md §N.1): the name, the interface and, unless set here, the server.
        let machine = cfg
            .secure
            .as_ref()
            .and_then(|s| s.site.as_ref())
            .and_then(|e| e.node_machine())
            .unwrap_or_default();
        let host = HostInfo::local(machine.interface)?;
        let server = cfg
            .discovery_server
            .clone()
            .or(machine.discovery_server)
            .filter(|_| !cfg.unlisted)
            .as_deref()
            .map(crate::discovery::ds::server_addr)
            .transpose()?;
        let mdns = cfg.mdns.unwrap_or(server.is_none()) && !cfg.unlisted;
        let responder = if mdns {
            Some(Responder::start(host.clone(), Vec::new(), cfg.listing)?)
        } else {
            None
        };
        let listing = (cfg.listing && !cfg.unlisted && responder.is_none())
            .then(|| Listing::start(host.listing_port, Default::default()))
            .flatten();
        Ok(Publisher(Arc::new(PublisherInner {
            display_host: cfg.host_name.or(machine.name).map(|n| n.to_uppercase()),
            host,
            responder,
            listing,
            server,
            groups: cfg.groups,
            secure: cfg.secure,
            monitoring: cfg.monitoring,
            link: cfg.link,
            unlisted: cfg.unlisted,
        })))
    }

    /// The full name an output called `name` gets on the network: `HOST (name)`, with
    /// the host name in capitals, e.g. `CHURCH-PC (Program)`.
    pub fn full_name(&self, name: &str) -> String {
        match &self.0.display_host {
            Some(h) => format!("{h} ({name})"),
            None => format!("{} ({name})", self.0.host.host.to_uppercase()),
        }
    }
}

/// Settings for one output. Start from [`OutputConfig::new`] and change what you need.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// Source name, e.g. `Program`. Receivers see `HOST (name)`.
    pub name: String,
    /// Frame rate as numerator/denominator, e.g. `(30000, 1001)` for 29.97 (the
    /// default). It labels the stream; frames go out as you send them.
    pub frame_rate: (u32, u32),
    /// Carry the alpha channel, for keying graphics over video (lower thirds). Off (the
    /// default): the picture is opaque, which is cheaper to convert, encode and send.
    pub alpha: bool,
    /// Picture quality 1–99. Default 92; 70–80 roughly halves the bit rate of busy
    /// camera pictures. With `target_mbps` it is the upper limit.
    pub quality: u8,
    /// Bit-rate target per receiver in Mbit/s. Quality then adapts each frame, down to
    /// 50, to stay near it. `None` (the default): constant quality.
    pub target_mbps: Option<f64>,
    /// TCP port receivers connect to. 0 (the default) picks a free one; see
    /// [`Output::port`].
    pub port: u16,
    /// How RGBA input is converted to Y'CbCr. The default (BT.709, limited range) is
    /// what NDI receivers expect; change it only for special set-ups.
    pub color: ColorConfig,
    /// The colour description to announce, instead of the one derived from `color`.
    /// Set it for HDR (primaries, transfer, matrix) and for sources above 8 bits
    /// (`bits`), typically together with [`VideoFrame::p216`]. NDI 6 HDR-aware apps
    /// understand the colour space; the bit depth reaches IDN receivers.
    pub color_info: Option<crate::ext::ColorInfo>,
    /// One stream per video frame on IDN links (`frames/1`): a frame that can no longer
    /// arrive in time is abandoned instead of delaying the next ones, which helps most on
    /// Wi-Fi. Used when the receiving input has it on too. Default: off.
    pub frame_streams: bool,
    /// Quality follows each IDN receiver (`adapt/1`): lower while its link is congested
    /// or it reports losses, back up when that clears, never below `quality_floor`
    /// (below it, frames are skipped instead). NDI receivers keep the fixed quality.
    /// Default: off.
    pub adapt: bool,
    /// The lowest quality `adapt` may choose. Default: 50.
    pub quality_floor: u8,
    /// Changed regions only (`regions/1`): IDN receivers with the option on get only the
    /// parts of each picture that changed, so static slides, lyrics and lower thirds cost
    /// almost no bandwidth. A whole picture still goes out every 2 s. Default: off.
    pub regions: bool,
    /// Multicast (§6.4): one stream on the network however many receivers watch, for
    /// receivers that ask for it. `None` (the default, as in NDI): off. With
    /// [`McConfig::default`](crate::mcast::McConfig) it is plain multicast; set
    /// [`Mode::Encrypted`](crate::mcast::Mode) to encrypt it for IDN receivers (`mcast/1`),
    /// or [`Mode::PerReceiver`](crate::mcast::Mode) to keep IDN receivers on their own
    /// encrypted connections. Needs IGMP snooping on the switches; poor on Wi-Fi.
    pub multicast: Option<crate::mcast::McConfig>,
    /// Compressed video (`codec/1`): H.264, HEVC or AV1 from a hardware encoder, for
    /// IDN inputs that want it (remote rooms, Wi-Fi), by default also for 4K pictures to
    /// inputs that can decode it ([`CodecPolicy::Auto`](crate::codec::CodecPolicy)), or
    /// for all that can decode it with `Force`. `None` (the default), or a machine
    /// without a hardware encoder: SpeedHQ on the CPU for everyone. NDI receivers always
    /// get SpeedHQ. See [`codec`](crate::codec).
    pub codec: Option<crate::codec::CodecOutput>,
    /// Names of the sound channels, shown in the IDN Site Manager's routing matrix
    /// (spec/extensions/idn-routing.md); optional, the matrix numbers channels anyway.
    pub audio_labels: Vec<String>,
    /// What the output is, for programs that look for it (§E.2.2): tokens such as
    /// `switcher/1` (a switcher's output with sound), announced with the extension list
    /// in discovery only, never negotiated on a connection. See
    /// [`Source::offers`](crate::discovery::Source::offers).
    pub roles: Vec<String>,
    /// The output takes keyboard and mouse from receivers of its site (R.25, a screen's
    /// source): announced in its capabilities; the events come from
    /// [`Output::recv_control`]. Default: off.
    pub remote_control: bool,
}

impl OutputConfig {
    /// Fills in what the site sets for this output (`options/1`, §E.20): `link.enabled`
    /// is a publisher matter, so here `quality`, `frames.enabled`, `regions.enabled`,
    /// `adapt.enabled`, `adapt.floor`, `mcast.*` and `codec.policy` (the encoder stays
    /// the application's; not when the machine's IDN Node sets an encoding,
    /// [`crate::node::video`]). Call it first, then set the user's own values: local wins.
    pub fn apply_site(&mut self, site: &crate::trust::Resolved) {
        if let Some(q) = site.u32("quality") {
            self.quality = q.clamp(1, 100) as u8;
        }
        if let Some(v) = site.bool("frames.enabled") {
            self.frame_streams = v;
        }
        if let Some(v) = site.bool("regions.enabled") {
            self.regions = v;
        }
        if let Some(v) = site.bool("adapt.enabled") {
            self.adapt = v;
        }
        if let Some(q) = site.u32("adapt.floor") {
            self.quality_floor = q.clamp(1, 100) as u8;
        }
        match site.get("mcast.mode") {
            Some("off") => self.multicast = None,
            Some(mode @ ("plain" | "encrypted" | "per-receiver")) => {
                let mc = self.multicast.get_or_insert_with(Default::default);
                mc.mode = match mode {
                    "encrypted" => crate::mcast::Mode::Encrypted,
                    "per-receiver" => crate::mcast::Mode::PerReceiver,
                    _ => crate::mcast::Mode::Plain,
                };
            }
            _ => {}
        }
        if let Some(mc) = self.multicast.as_mut() {
            if let Some(v) = site.bool("mcast.signed") {
                mc.signatures = v;
            }
            if let Some(secs) = site.u32("mcast.key_interval_s") {
                mc.key_interval = std::time::Duration::from_secs(secs.clamp(60, 86_400) as u64);
            }
        }
        // A machine's own encoding setting (the IDN Node, §N.13) is closer to the output
        // than the site's.
        if let (Some(c), Some(p)) = (self.codec.as_mut(), site.get("codec.policy"))
            && crate::node::video::machine().encoding.is_none()
            && !c.fixed
        {
            c.policy = match p {
                "force" => crate::codec::CodecPolicy::Force,
                "offer" => crate::codec::CodecPolicy::Offer,
                _ => crate::codec::CodecPolicy::Auto,
            };
        }
    }

    /// Defaults: 29.97 fps, opaque, quality 92, constant quality, any free port,
    /// BT.709 limited range, colour description derived from that.
    pub fn new(name: &str) -> Self {
        OutputConfig {
            name: name.into(),
            frame_rate: (30000, 1001),
            alpha: false,
            quality: 92,
            target_mbps: None,
            port: 0,
            color: ColorConfig::default(),
            color_info: None,
            frame_streams: false,
            adapt: false,
            quality_floor: 50,
            regions: false,
            multicast: None,
            codec: None,
            audio_labels: Vec::new(),
            roles: Vec::new(),
            remote_control: false,
        }
    }
}
