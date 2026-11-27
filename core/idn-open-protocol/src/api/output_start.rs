//! An output's start and end: its name checked, its sender, announcements and worker
//! started, and its withdrawal when dropped.

use super::*;

/// Refuses names that can't be published as given.
pub(super) fn check_name(name: &str, full_name: &str) -> io::Result<()> {
    let bad = |m: String| Err(io::Error::new(io::ErrorKind::InvalidInput, m));
    if name.trim().is_empty() {
        return bad("the output name is empty".into());
    }
    if name.chars().any(char::is_control) {
        return bad(format!(
            "the output name {name:?} contains control characters"
        ));
    }
    if full_name.len() > MAX_FULL_NAME {
        return bad(format!(
            "the output name is too long: \"{full_name}\" is {} bytes, at most {MAX_FULL_NAME} \
             (here the name itself may have {})",
            full_name.len(),
            MAX_FULL_NAME.saturating_sub(full_name.len() - name.len())
        ));
    }
    Ok(())
}

impl Output {
    /// Starts the output and announces it. Fails if the port is taken, the Discovery
    /// Server can't be reached, or the name can't be published: empty, with control
    /// characters, or a full name over [`MAX_FULL_NAME`] bytes.
    pub fn new(publisher: &Publisher, cfg: OutputConfig) -> io::Result<Self> {
        Self::start(publisher, cfg, None)
    }

    /// [`Output::new`], or a routing source when `route` is given ([`Router`]).
    pub(super) fn start(
        publisher: &Publisher,
        mut cfg: OutputConfig,
        route: Option<Arc<crate::sender::Route>>,
    ) -> io::Result<Self> {
        let routing = route.is_some();
        // A rate with a zero part becomes 60000/1001, as NDI 6.3 senders do (§2.4).
        if cfg.frame_rate.0 == 0 || cfg.frame_rate.1 == 0 {
            cfg.frame_rate = (60000, 1001);
        }
        let full_name = publisher.full_name(&cfg.name);
        check_name(&cfg.name, &full_name)?;
        let sender = Arc::new(Sender::start(
            cfg.port,
            SenderConfig {
                name: full_name.clone(),
                quality: cfg.quality,
                capabilities: if cfg.remote_control {
                    vec![(
                        crate::remote::CAPABILITY.0.into(),
                        crate::remote::CAPABILITY.1.into(),
                    )]
                } else {
                    Vec::new()
                },
                multicast: cfg.multicast.is_some(),
                hx: false,
                target_mbps: cfg.target_mbps,
                color: cfg
                    .color_info
                    .clone()
                    .unwrap_or_else(|| crate::ext::ColorInfo::from_config(&cfg.color)),
                secure: publisher.0.secure.clone(),
                link: publisher.0.link.clone(),
                frame_streams: cfg.frame_streams,
                adapt_floor: cfg.adapt.then_some(cfg.quality_floor),
                regions: cfg.regions,
                mc: cfg.multicast.clone().unwrap_or_default(),
                codec: cfg.codec.clone(),
                route,
            },
        )?);
        let p = &publisher.0;
        // The extension list as discovery shows it (§E.2.2), with the link only while it
        // is listening (§E.11.2).
        let ext_list = {
            let l = crate::ext::announced_with(
                p.secure
                    .as_ref()
                    .is_some_and(|s| s.passphrase_key().is_some()),
                p.secure.as_ref().is_some_and(|s| s.site.is_some()),
            );
            if sender.link_on() {
                format!("{l} {}", crate::link::TOKEN)
            } else {
                l
            }
        };
        // Roles go to discovery with the list, not to connections (§E.2.2). A screen that
        // takes control says so, with its machine's key and its site (§E.25: IDN Remote
        // lists such screens, linked or of its site, without connecting).
        let site = p.secure.as_ref().and_then(|s| s.site.as_ref());
        let control: Vec<String> = if cfg.remote_control {
            std::iter::once("control/1".to_owned())
                .chain(
                    site.map(|e| format!("idnfp/{}", crate::trust::pair::short(&e.fingerprint()))),
                )
                .chain(site.and_then(|e| e.ca_id()).map(|ca| format!("idnca/{ca}")))
                .collect()
        } else {
            Vec::new()
        };
        let announced = std::iter::once(ext_list.clone())
            .chain(
                cfg.roles
                    .iter()
                    .filter(|r| !r.is_empty() && !r.contains(char::is_whitespace))
                    .cloned(),
            )
            .chain(control)
            .collect::<Vec<_>>()
            .join(" ");
        let published = Published {
            name: full_name.clone(),
            port: sender.port(),
            groups: p.groups.clone(),
            routing,
            idn_ext: Some(announced),
        };
        if let Some(r) = &p.responder {
            r.add(published.clone());
        } else if let Some(l) = &p.listing {
            l.add(published.clone());
        }
        // Kept registered across Discovery Server restarts; a server that is down at
        // start-up is retried rather than failing the output.
        let registration = p
            .server
            .map(|sv| crate::discovery::ds::SourceRegistration::start(sv, &published));
        let shared = Arc::new(Shared {
            captions: Mutex::new(None),
            slot: Mutex::new(None),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
            submitted: AtomicU64::new(0),
            replaced: AtomicU64::new(0),
            repeated: AtomicU64::new(0),
            size: Mutex::new((0, 0)),
            audio_timecode: Mutex::new(None),
            epoch: Instant::now(),
            format: Mutex::new(LiveFormat {
                alpha: cfg.alpha,
                frame_rate: cfg.frame_rate,
            }),
        });
        // Inputs of this process may take its frames as they are (local.rs); a routing
        // source has none.
        if !routing {
            crate::local::register(&sender);
        }
        // From here on a failure drops `out`, which undoes everything (R1-53).
        let mut out = Output {
            full_name: full_name.clone(),
            shared: shared.clone(),
            worker: None,
            sender: sender.clone(),
            publisher: publisher.clone(),
            _registration: registration,
            advertiser: None,
            status: None,
            _node: p
                .secure
                .as_ref()
                .filter(|_| !p.unlisted)
                .and_then(|s| s.site.as_ref())
                .and_then(|e| e.node_register("output", &full_name, Some(sender.port()))),
            meter: None,
            node_channels: std::sync::atomic::AtomicU32::new(0),
            audio_labels: cfg.audio_labels.clone(),
            color: cfg
                .color_info
                .clone()
                .unwrap_or_else(|| crate::ext::ColorInfo::from_config(&cfg.color)),
        };
        out.meter = out._node.as_ref().map(|r| r.meter());
        if let (Some(sv), true) = (p.server, p.monitoring) {
            let a = crate::discovery::ds::Advertiser::source(
                sv,
                &full_name,
                sender.port(),
                &p.groups,
                crate::discovery::ds::IDN_SOURCE_PROPERTIES,
            );
            let ci = cfg
                .color_info
                .clone()
                .unwrap_or_else(|| crate::ext::ColorInfo::from_config(&cfg.color));
            for (k, v) in [
                ("sdk-version", crate::control::SDK_VERSION.to_owned()),
                ("source-name", full_name.clone()),
                ("source-url", format!("{}:{}", p.host.addr, sender.port())),
                ("video-present", "true".into()),
                (
                    "video-codec",
                    if cfg.alpha { "shq7" } else { "shq2" }.into(),
                ),
                (
                    "video-frame-rate",
                    format!("{}/{}", cfg.frame_rate.0, cfg.frame_rate.1),
                ),
                ("video-frame-type", "progressive".into()),
                ("video-has-alpha", cfg.alpha.to_string()),
                ("video-color-primaries", ci.primaries.clone()),
                ("video-transfer-function", ci.transfer.clone()),
                ("video-matrix-coefficients", ci.matrix.clone()),
                ("x-idn-extensions", ext_list.clone()),
            ] {
                a.set(k, v);
            }
            let props = a.props.clone();
            out.advertiser = Some(a);
            let (shared, sender) = (shared.clone(), sender.clone());
            out.status = Some(
                std::thread::Builder::new()
                    .name(format!("status {}", cfg.name))
                    .spawn(move || report_status(&shared, &sender, &props))?,
            );
        }
        out.worker = Some(
            std::thread::Builder::new()
                .name(format!("output {}", cfg.name))
                .spawn(move || worker(shared, sender, cfg))?,
        );
        Ok(out)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        crate::local::unregister(self.sender.port());
        // Each step that takes over a second is logged: a host's exit that waits on an
        // output can then say which.
        let name = self.full_name.clone();
        let timed = |what: &str, f: &mut dyn FnMut()| {
            let t = Instant::now();
            f();
            if t.elapsed() >= std::time::Duration::from_secs(1) {
                crate::log::log(&format!(
                    "output {name}: closing: {what} took {:.1} s",
                    t.elapsed().as_secs_f64()
                ));
            }
        };
        // A plain drop is a shutdown (no-op if `close` already announced a departure).
        timed("telling the receivers", &mut || {
            self.sender.depart(crate::ext::Departure::Shutdown)
        });
        // Set the flag under the slot lock: the worker checks it under that lock before
        // waiting, so the wake-up can't fall between its check and its wait.
        {
            let _slot = self.shared.slot.lock_ok();
            self.shared.stop.store(true, Ordering::Relaxed);
        }
        self.shared.ready.notify_all();
        let mut worker = self.worker.take();
        timed("its worker", &mut || {
            if let Some(w) = worker.take() {
                let _ = w.join();
            }
        });
        let mut status = self.status.take();
        timed("its status thread", &mut || {
            if let Some(t) = status.take() {
                let _ = t.join();
            }
        });
        let mut advertiser = self.advertiser.take();
        timed("its monitoring advertisement", &mut || {
            drop(advertiser.take())
        });
        let (publisher, full_name, port) = (
            self.publisher.clone(),
            self.full_name.clone(),
            self.sender.port(),
        );
        timed("its announcement", &mut || {
            if let Some(r) = &publisher.0.responder {
                r.remove(&full_name, port);
            } else if let Some(l) = &publisher.0.listing {
                l.remove(&full_name, port);
            }
        });
    }
}
