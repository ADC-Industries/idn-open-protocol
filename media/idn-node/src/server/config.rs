//! The machine settings' types: the settings, program rules and approval, the site clock,
//! and a change of settings.

use super::*;

/// The machine settings (§N.1), kept in `node.xml` in the state directory (readable by
/// the service only: it holds the passphrase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The device name (default: the host name).
    pub name: String,
    /// The Discovery Server hosting the site authority, if any.
    pub server: Option<String>,
    /// The network interface programs should use (an IPv4 address), if chosen.
    pub interface: Option<String>,
    /// The site passphrase, stretched ([`SiteKey::psk`]): `secure/1`, and joining
    /// without approval. The passphrase itself is never stored.
    pub psk: Option<[u8; 32]>,
    /// "Require encryption" for the machine's programs.
    pub require: bool,
    /// The panel's port on 127.0.0.1.
    pub panel_port: u16,
    /// The panel and LAN panel ports as `node.xml` has them, while a command line's
    /// override is in effect: saved instead of the override (any settings change made
    /// that run's ports permanent, deep bug run 2026-11-09).
    pub saved_ports: Option<(u16, u16)>,
    /// The LAN panel's password, as an Argon2id hash; `None`: the LAN panel is off
    /// (§N.11.3).
    pub lan_password: Option<String>,
    /// Look for a Site Manager on the LAN when no server is set (§N.11.4).
    pub auto_site: bool,
    /// The first-run wizard is done or skipped (§N.22): a new machine shows it; a machine
    /// set up before it existed doesn't.
    pub welcomed: bool,
    /// The LAN panel's port (§N.11.3).
    pub lan_port: u16,
    /// The machine's video settings for its programs (§N.13).
    pub video: VideoSettings,
    /// This computer's sound bridges, run by the session agent (§N.14).
    pub audio: AudioBridges,
    /// Its screens and webcams as IDN sources, run the same way (R.19).
    pub capture: CaptureBridges,
    /// The site link to another location, run by the Node service (R.18).
    pub sitelink: SiteLinkSetting,
    /// AES67 streams to and from IDN, run by the Node service (§N.14.5).
    pub aes67: Aes67Bridges,
    /// Listening on phones, run by the Node service (§N.19).
    pub listen: ListenSetting,
    /// IDN Data, run by the Node service (§N.24).
    pub data: DataSetting,
    /// Open Media Transport bridges, run by the Node service (§N.27).
    pub omt: OmtBridges,
    /// Live captions, run by the Node service (§N.28).
    pub captions: CaptionsSetting,
    /// A network-audio device and the desk in its rings, run by the Node service (R.33).
    pub netaudio: NetAudioSetting,
    /// Routers, run by the Node service (§N.29).
    pub routers: RoutersSetting,
    /// The IDN Site Manager, run by the Node service (§N.12: this computer runs the site).
    pub sitemanager: SiteManagerSetting,
    /// The switcher and the mixer started at log-on (§N.14.4).
    pub apps: AppsSetting,
    /// The services switched off, kept with their settings (§N.14.4).
    pub services_off: ServicesOff,
    /// Services switched on or off for now from the panel (not kept: until the Node
    /// starts again, then each as `services_off` / its own switch says).
    pub services_now: ServicesNow,
    /// Keep the computer from sleeping while a service runs (on unless turned off).
    pub keep_awake: bool,
    /// The site clock (spec/extensions/idn-clock.md).
    pub clock: ClockSettings,
    /// This machine's sample rate when it isn't the site's (`node::rate`, §N.40): `None`,
    /// the site option `audio.rate`, else 48 kHz.
    pub audio_rate: Option<u32>,
    /// Ask before a program not seen before uses this machine's identity (§N.8.1).
    pub approve_programs: bool,
    /// Other computers may ask this one to show a PIN for a link (IDN Remote, §N.32).
    /// On unless turned off.
    pub link_requests: bool,
    /// Programs allowed or blocked (§N.8.1).
    pub programs: Vec<ProgramRule>,
    /// The PIN that leaves the panels' simple mode (§N.20), as an Argon2id hash; `None`:
    /// leaving takes just the button.
    pub panel_pin: Option<String>,
    /// The PIN is the Site Manager's: the panels (and this machine) can't change it.
    pub panel_site_only: bool,
    /// Locked by the site (N4, §N.31): settings, services and routes change only from
    /// the Site Manager.
    pub site_lock: bool,
    /// The product that installs and updates this machine's IDN Node itself (`Example Player`;
    /// §N.23): the Site Manager's IDN Media updates refused. `None`: not managed (a marker
    /// file in the programs folder names one too: [`crate::update::managed_by`]).
    pub update_managed_by: Option<String>,
}

/// A program allowed or blocked by the operator (§N.8.1), by its executable's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramRule {
    /// The executable's full path, as the operating system reports it.
    pub path: String,
    /// Allowed (else blocked).
    pub allow: bool,
    /// The name it gave when it was decided on.
    pub app: String,
}

/// Where a program stands with the Node (§N.8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// May use the machine's identity: approval is off, it is allowed, or it is one of
    /// the IDN programs installed with the Node.
    Allowed,
    /// Connected, but signs nothing until the operator allows it.
    Pending,
    /// Blocked by the operator: refused.
    Blocked,
}

impl Approval {
    pub(super) fn name(self) -> &'static str {
        match self {
            Approval::Allowed => "allowed",
            Approval::Pending => "pending",
            Approval::Blocked => "blocked",
        }
    }
}

/// Program rules as kept in `node.xml`: one a line, `allow|block TAB path TAB name`.
pub(super) fn rules_text(r: &[ProgramRule]) -> String {
    r.iter()
        .map(|p| {
            format!(
                "{}\t{}\t{}",
                if p.allow { "allow" } else { "block" },
                p.path,
                p.app
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn parse_rules(t: &str) -> Vec<ProgramRule> {
    t.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let allow = match f.next()? {
                "allow" => true,
                "block" => false,
                _ => return None,
            };
            let path = f.next().filter(|p| !p.is_empty())?.to_owned();
            Some(ProgramRule {
                path,
                allow,
                app: f.next().unwrap_or("").to_owned(),
            })
        })
        .collect()
}

/// The IDN programs installed with the Node (`idn…` in IDN's programs folder: its own
/// executable's, or the one a program hosting it set): allowed without asking. Not the
/// whole folder: on Linux that is /usr/bin.
pub(super) fn installed_with_node(exe: &str) -> bool {
    let p = Path::new(exe);
    let named_idn = p
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().starts_with("idn"));
    let here = idn_common::supervisor::programs_dir();
    named_idn
        && here.is_some_and(|h| {
            p.parent()
                .is_some_and(|d| same_path(&d.to_string_lossy(), &h.to_string_lossy()))
        })
}

/// Two program paths the same: letter case aside on Windows (the same program started as
/// `C:\Program Files\…` and `c:\program files\…` didn't match its rule; bug run B).
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// The site clock's settings (idn-clock.md §C.2, §C.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSettings {
    /// Run PTP (default on).
    pub on: bool,
    /// The PTP domain (the site option `clock.domain`, default 0).
    pub domain: u8,
    /// priority1, when set; else 200 on the machine hosting the site's server (its Site
    /// Manager), 248 elsewhere.
    pub priority1: Option<u8>,
    /// PTP v2 (default), v1 (the clock of Dante-compatible equipment without AES67:
    /// follows its leader, or leads where none is), or both as a bridge (idn-clock.md
    /// §C.7, roadmap R.31).
    pub protocol: idn_open_protocol::ptp::Protocol,
}

impl Default for ClockSettings {
    fn default() -> Self {
        ClockSettings {
            on: true,
            domain: 0,
            priority1: None,
            protocol: idn_open_protocol::ptp::Protocol::V2,
        }
    }
}

/// The LAN panel's port (§N.11.3).
pub const LAN_PANEL_PORT: u16 = idn_common::ports::NODE_LAN_PANEL;

/// A change of settings: `None` leaves a setting as it is; for the optional ones,
/// `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct Change {
    pub name: Option<String>,
    /// Ask before new programs use the machine's identity (§N.8.1).
    pub approve_programs: Option<bool>,
    /// Other computers may ask for a link (§N.32).
    pub link_requests: Option<bool>,
    /// A program rule: (`allow`, `block` or `forget`, the executable's path, its name).
    pub program: Option<(String, String, String)>,
    pub server: Option<Option<String>>,
    pub interface: Option<Option<String>>,
    /// The passphrase in clear (stretched before it is kept).
    pub passphrase: Option<Option<String>>,
    pub require: Option<bool>,
    /// The authority's one-time administrator code (not kept).
    pub admin_code: Option<String>,
    /// The LAN panel's password in clear (hashed before it is kept).
    pub lan_password: Option<Option<String>>,
    /// The LAN panel's password already hashed (from the Site Manager).
    pub lan_password_hash: Option<Option<String>>,
    /// The simple-mode PIN in clear (hashed before it is kept; §N.20).
    pub panel_pin: Option<Option<String>>,
    /// The simple-mode PIN already hashed (from the Site Manager).
    pub panel_pin_hash: Option<Option<String>>,
    /// Only the Site Manager may change the PIN.
    pub panel_site_only: Option<bool>,
    /// The change comes from the Site Manager (it may change a PIN the site holds).
    pub from_site: bool,
    /// Lock or unlock the machine's settings to the site (N4, §N.31): the Site Manager's.
    pub site_lock: Option<bool>,
    /// The services switched off, the whole list (one id a line; §N.14.4): a copy or a
    /// template from the Site Manager (§N.21).
    pub services_off: Option<String>,
    pub auto_site: Option<bool>,
    /// The first-run wizard done (or wanted again: `false`).
    pub welcomed: Option<bool>,
    /// Video settings to change, by name (`threads`, `parallel_decode`, `encoding`,
    /// `decoding`, `ndi`; an empty value unsets one): [`VideoSettings::set`].
    pub video: Vec<(String, String)>,
    /// Sound bridges to change, by name (`mic`, `mic_name`, `mic_device`, `apps`):
    /// [`AudioBridges::set`].
    pub audio: Vec<(String, String)>,
    /// Capture bridges to change (`screens`, `webcams`): [`CaptureBridges::set`].
    pub capture: Vec<(String, String)>,
    /// Site link settings to change (`mode`, `address`, `port`, `limit`, `groups`):
    /// [`SiteLinkSetting::set`].
    pub sitelink: Vec<(String, String)>,
    /// AES67 streams to change (`sends`, `recvs`): [`Aes67Bridges::set`].
    pub aes67: Vec<(String, String)>,
    /// Listening settings to change (`channels`, `port`, `title`, `kbps`):
    /// [`ListenSetting::set`].
    pub listen: Vec<(String, String)>,
    /// IDN Data settings to change (`on`, `port`): [`DataSetting::set`].
    pub data: Vec<(String, String)>,
    /// OMT bridges to change (`sends`, `recvs`): [`OmtBridges::set`].
    pub omt: Vec<(String, String)>,
    /// Captions settings to change: [`CaptionsSetting::set`].
    pub captions: Vec<(String, String)>,
    /// Network-audio settings to change: [`NetAudioSetting::set`].
    pub netaudio: Vec<(String, String)>,
    /// Routers settings to change (`list`, `route`): [`RoutersSetting::set`].
    pub routers: Vec<(String, String)>,
    /// Site Manager settings to change (`on`, `port`): [`SiteManagerSetting::set`].
    pub sitemanager: Vec<(String, String)>,
    /// Autostart settings to change (`switcher`, `mixer`): [`AppsSetting::set`].
    pub apps: Vec<(String, String)>,
    /// A service switched on or off by its id (§N.14.4): both now and whether it starts
    /// by itself (the command line's and the Site Manager's switch).
    pub service: Option<(String, bool)>,
    /// A service switched on or off for now only (the panel's switch).
    pub service_now: Option<(String, bool)>,
    /// Whether a service starts by itself (at start-up, or when someone logs on).
    pub service_auto: Option<(String, bool)>,
    /// Keep the computer awake while services run.
    pub keep_awake: Option<bool>,
    /// The site clock on or off.
    pub clock: Option<bool>,
    pub clock_domain: Option<u8>,
    /// PTP v2, v1 or both (a bridge).
    pub clock_protocol: Option<idn_open_protocol::ptp::Protocol>,
    /// priority1; `Some(None)`: automatic.
    pub clock_priority: Option<Option<u8>>,
    /// The machine's own sample rate; `Some(None)`: the site's again.
    pub audio_rate: Option<Option<u32>>,
    /// Who installs and updates this machine's IDN Node (`update.managed_by`, §N.23): a
    /// product's name; empty: not managed.
    pub update_managed_by: Option<String>,
}
