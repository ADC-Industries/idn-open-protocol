//! The Wayland side's state: the outputs, the session's buffer constraints and the
//! frame's events, as the desktop sends them.

use super::*;

/// The session's buffer constraints (sent again whole whenever they change).
#[derive(Default, Clone, Debug)]
pub(super) struct Constraints {
    pub(super) size: (u32, u32),
    pub(super) shm: Vec<u32>,
    pub(super) dev: Option<u64>,
    pub(super) dma: Vec<(u32, Vec<u64>)>,
}

#[derive(Default)]
pub(super) struct OutputInfo {
    pub(super) wl: Option<WlOutput>,
    pub(super) name: String,
    pub(super) pos: (i32, i32),
    pub(super) mode: (i32, i32),
    pub(super) logical: Option<((i32, i32), (i32, i32))>,
    /// The monitor's turn (wl_output's transform, 0–7).
    pub(super) transform: u32,
}

pub(super) enum FrameState {
    Idle,
    Waiting,
    Ready,
    Failed(u32),
}

pub(super) struct St {
    pub(super) outputs: Vec<OutputInfo>,
    pub(super) pending: Constraints,
    pub(super) constraints: Option<Constraints>,
    pub(super) changed: bool,
    pub(super) stopped: bool,
    pub(super) frame: FrameState,
    /// The transform the desktop applied to the buffer's contents (wl_output's: a
    /// rotated or flipped monitor), this frame's.
    pub(super) transform: u32,
}

impl Dispatch<WlRegistry, GlobalListContents> for St {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, usize> for St {
    fn event(
        st: &mut Self,
        _: &WlOutput,
        e: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outputs.get_mut(*i) else {
            return;
        };
        match e {
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Geometry { x, y, transform, .. } => {
                o.pos = (x, y);
                o.transform = match transform {
                    WEnum::Value(t) => u32::from(t),
                    WEnum::Unknown(u) => u,
                } & 7;
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    o.mode = (width, height);
                }
            }
            _ => {}
        }
    }
}

impl OutputInfo {
    /// Where it is in the desktop (logical), for the pointer.
    pub(super) fn area(&self) -> Option<(i32, i32, u32, u32)> {
        match self.logical {
            Some(((x, y), (w, h))) if w > 0 && h > 0 => Some((x, y, w as u32, h as u32)),
            _ if self.mode.0 > 0 && self.mode.1 > 0 => Some((
                self.pos.0,
                self.pos.1,
                self.mode.0 as u32,
                self.mode.1 as u32,
            )),
            _ => None,
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for St {
    fn event(
        st: &mut Self,
        _: &ZxdgOutputV1,
        e: zxdg_output_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outputs.get_mut(*i) else {
            return;
        };
        let l = o.logical.get_or_insert(((0, 0), (0, 0)));
        match e {
            zxdg_output_v1::Event::LogicalPosition { x, y } => l.0 = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => l.1 = (width, height),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for St {
    fn event(
        st: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        e: session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            session_v1::Event::BufferSize { width, height } => st.pending.size = (width, height),
            session_v1::Event::ShmFormat { format } => {
                let raw = match format {
                    WEnum::Value(f) => u32::from(f),
                    WEnum::Unknown(u) => u,
                };
                st.pending.shm.push(drm_of_shm(raw));
            }
            session_v1::Event::DmabufDevice { device } => st.pending.dev = dev_of(&device),
            session_v1::Event::DmabufFormat { format, modifiers } => {
                let mods = modifiers
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_ne_bytes(*c))
                    .collect();
                st.pending.dma.push((format, mods));
            }
            session_v1::Event::Done => {
                st.constraints = Some(std::mem::take(&mut st.pending));
                st.changed = true;
            }
            session_v1::Event::Stopped => st.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for St {
    fn event(
        st: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        e: frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            frame_v1::Event::Ready => st.frame = FrameState::Ready,
            frame_v1::Event::Transform { transform } => {
                st.transform = match transform {
                    WEnum::Value(t) => u32::from(t),
                    WEnum::Unknown(u) => u,
                } & 7
            }
            frame_v1::Event::Failed { reason } => {
                st.frame = FrameState::Failed(match reason {
                    WEnum::Value(r) => u32::from(r),
                    WEnum::Unknown(u) => u,
                })
            }
            _ => {}
        }
    }
}

delegate_noop!(St: ignore WlShm);
delegate_noop!(St: WlShmPool);
delegate_noop!(St: ignore WlBuffer);
delegate_noop!(St: ignore ZwpLinuxDmabufV1);
delegate_noop!(St: ignore ZwpLinuxBufferParamsV1);
delegate_noop!(St: ZxdgOutputManagerV1);
delegate_noop!(St: ExtImageCopyCaptureManagerV1);
delegate_noop!(St: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(St: ExtImageCaptureSourceV1);
