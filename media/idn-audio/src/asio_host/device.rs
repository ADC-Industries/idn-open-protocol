//! The host, its devices and their streams, as cpal sees them.

use super::*;

#[derive(Clone)]
pub(super) struct AsioHost;

impl AsioHost {
    /// The default for a direction: the driver running in this program if it has it;
    /// else the first by name known to work or not yet asked, wrappers last. Nothing is
    /// loaded to choose.
    fn default_for(&self, input: bool) -> Option<AsioDevice> {
        let all: Vec<AsioDevice> = self.devices().ok()?.collect();
        let running = ACTIVE.lock_ok().clone();
        if let Some(d) = all.iter().find(|d| Some(&d.id) == running.as_ref())
            && d.usable(input) != Some(false)
        {
            return Some(d.clone());
        }
        let wrapper = |d: &AsioDevice| {
            let n = d.name.to_lowercase();
            WRAPPERS.iter().any(|w| n.contains(w))
        };
        let mut ok: Vec<&AsioDevice> = all
            .iter()
            .filter(|d| d.usable(input) != Some(false))
            .collect();
        ok.sort_by_key(|d| wrapper(d));
        ok.first().map(|d| (*d).clone())
    }
}

impl HostTrait for AsioHost {
    type Device = AsioDevice;
    type Devices = std::vec::IntoIter<AsioDevice>;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Self::Devices, Error> {
        Ok(drivers()
            .into_iter()
            .filter(|d| !is_idn_driver(&d.id))
            .map(|d| AsioDevice {
                name: d.name,
                id: d.id,
            })
            .collect::<Vec<_>>()
            .into_iter())
    }

    fn default_input_device(&self) -> Option<AsioDevice> {
        self.default_for(true)
    }

    fn default_output_device(&self) -> Option<AsioDevice> {
        self.default_for(false)
    }
}

/// One installed driver.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AsioDevice {
    name: String,
    /// What loads it (Windows: its class id; Linux: its library).
    id: DriverId,
}

impl std::fmt::Display for AsioDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl AsioDevice {
    /// The probe kept, if still fresh (listing: however old: a driver that failed isn't
    /// loaded again for each listing).
    fn kept(&self) -> Option<Result<Probe, Error>> {
        let k = kept().lock_ok();
        let (until, p) = k.now.get(&self.id)?;
        (LISTING.get() || Instant::now() < *until).then(|| p.clone())
    }

    /// The probe, kept or asked. While another driver runs here: its last good answer
    /// (it stays listed with its formats; opening it says it is busy).
    fn probe(&self) -> Result<Probe, Error> {
        if let Some(p) = self.kept() {
            return p;
        }
        if LISTING.get()
            && let Some(g) = kept().lock_ok().good.get(&self.id)
        {
            return Ok(g.clone());
        }
        let id = self.id.clone();
        let p = engine_call(move |tx| Req::Probe(id, tx));
        match p {
            Err(e) if e.message() == Some(OTHER_RUNNING) => {
                match kept().lock_ok().good.get(&self.id) {
                    Some(g) => Ok(g.clone()),
                    None => Err(e),
                }
            }
            p => p,
        }
    }

    /// Whether it can be a default for a direction: known to have it (`Some(true)`), known
    /// not to or not to work (`Some(false)`), or not asked yet (`None`).
    fn usable(&self, input: bool) -> Option<bool> {
        match self.kept()? {
            Ok(p) => Some(if input { p.inputs > 0 } else { p.outputs > 0 }),
            Err(_) => Some(false),
        }
    }

    fn ranges(&self, input: bool) -> Result<Vec<SupportedStreamConfigRange>, Error> {
        let p = self.probe()?;
        let ch = if input { p.inputs } else { p.outputs };
        if ch == 0 {
            return Ok(Vec::new());
        }
        let size = SupportedBufferSize::Range {
            min: p.frames.0,
            max: p.frames.1,
        };
        // Every count from all its channels down to one (a stream opens its first N: the
        // desk taking 8 of a 64-channel card converted all 64 each buffer; bug run
        // 2026-11-25), all of them first.
        let most = ch.min(ChannelCount::MAX as usize) as ChannelCount;
        Ok(p.rates
            .iter()
            .flat_map(|r| {
                (1..=most).rev().map(move |n| {
                    SupportedStreamConfigRange::new(n, *r, *r, size, SampleFormat::F32)
                })
            })
            .collect())
    }

    fn default_config(&self, input: bool) -> Result<SupportedStreamConfig, Error> {
        let p = self.probe()?;
        let ch = if input { p.inputs } else { p.outputs };
        if ch == 0 {
            return Err(err(
                ErrorKind::UnsupportedConfig,
                format!(
                    "{}: no {}",
                    self.name,
                    if input { "inputs" } else { "outputs" }
                ),
            ));
        }
        Ok(SupportedStreamConfig::new(
            ch.min(ChannelCount::MAX as usize) as ChannelCount,
            p.rate,
            SupportedBufferSize::Range {
                min: p.frames.0,
                max: p.frames.1,
            },
            SampleFormat::F32,
        ))
    }

    fn open(
        &self,
        input: bool,
        config: StreamConfig,
        format: SampleFormat,
        cb: Callback,
        error: ErrCb,
    ) -> Result<AsioStream, Error> {
        if format != SampleFormat::F32 {
            return Err(err(
                ErrorKind::UnsupportedConfig,
                "ASIO streams are f32 here",
            ));
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let paused = Arc::new(AtomicBool::new(true));
        let open = Open {
            id: self.id.clone(),
            input,
            channels: config.channels as usize,
            rate: config.sample_rate,
            frames: match config.buffer_size {
                BufferSize::Fixed(n) => Some(n),
                BufferSize::Default => None,
            },
            slot: Slot {
                id,
                channels: config.channels as usize,
                paused: paused.clone(),
                cb,
                dead: false,
            },
            error,
        };
        match engine_call(move |tx| Req::Open(Box::new(open), tx)) {
            Ok(frames) => Ok(AsioStream {
                input,
                id,
                paused,
                frames,
                start: engine().start,
            }),
            Err(e) => {
                // An answer too late (or none): whatever the engine opened for this
                // stream is closed after it (the queue keeps the order).
                post(Req::Close {
                    input,
                    id,
                    tx: None,
                });
                Err(e)
            }
        }
    }
}

impl DeviceTrait for AsioDevice {
    type SupportedInputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
    type SupportedOutputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
    type Stream = AsioStream;

    fn description(&self) -> Result<DeviceDescription, Error> {
        Ok(DeviceDescriptionBuilder::new(&self.name).build())
    }

    fn id(&self) -> Result<DeviceId, Error> {
        Ok(DeviceId::new(cpal::HostId::Custom, &self.name))
    }

    /// Known without loading it, else assumed (opening it says: a busy driver is found and
    /// reported busy, not missing).
    fn supports_input(&self) -> bool {
        !matches!(self.kept(), Some(Ok(p)) if p.inputs == 0)
    }

    fn supports_output(&self) -> bool {
        !matches!(self.kept(), Some(Ok(p)) if p.outputs == 0)
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        self.ranges(true).map(|v| v.into_iter())
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        self.ranges(false).map(|v| v.into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.default_config(true)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.default_config(false)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        format: SampleFormat,
        data: D,
        error: E,
        _timeout: Option<Duration>,
    ) -> Result<AsioStream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.open(
            true,
            config,
            format,
            Callback::In(Box::new(data)),
            Box::new(error),
        )
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        format: SampleFormat,
        data: D,
        error: E,
        _timeout: Option<Duration>,
    ) -> Result<AsioStream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.open(
            false,
            config,
            format,
            Callback::Out(Box::new(data)),
            Box::new(error),
        )
    }
}

/// A stream: paused until played; dropped, its slot closes (the other direction's goes on).
pub struct AsioStream {
    pub(super) input: bool,
    pub(super) id: u64,
    pub(super) paused: Arc<AtomicBool>,
    /// The driver's buffer size now (set again after a reset).
    pub(super) frames: Arc<AtomicU32>,
    pub(super) start: Instant,
}

impl StreamTrait for AsioStream {
    fn play(&self) -> Result<(), Error> {
        self.paused.store(false, Ordering::Release);
        Ok(())
    }

    fn pause(&self) -> Result<(), Error> {
        self.paused.store(true, Ordering::Release);
        Ok(())
    }

    fn now(&self) -> StreamInstant {
        instant(self.start.elapsed())
    }

    fn buffer_size(&self) -> Result<FrameCount, Error> {
        Ok(self.frames.load(Ordering::Relaxed))
    }
}

impl Drop for AsioStream {
    fn drop(&mut self) {
        let (input, id) = (self.input, self.id);
        // From the engine or the buffer switch: closed without waiting (waiting there
        // would wait on itself).
        if NO_WAIT.get() {
            post(Req::Close {
                input,
                id,
                tx: None,
            });
            return;
        }
        let _ = engine_call(move |tx| Req::Close {
            input,
            id,
            tx: Some(tx),
        });
    }
}
