//! A source as a webcam (spec/extensions/idn-display.md §D.5): Zoom, Teams, a browser take
//! it like any camera. On Linux through v4l2loopback (the kernel module OBS's virtual camera
//! uses too): this writes pictures to its device as YUYV, at one size the whole time
//! (apps don't follow a camera that changes size), each picture fitted with its aspect
//! kept, black around; while nothing arrives, black. A pipe or file given as the device
//! gets the same pictures as raw YUYV, for another program.

use crate::receive;
#[cfg(any(test, target_os = "linux"))]
use idn_open_protocol::shq::Plane;

/// Black in limited-range Y'CbCr.
#[cfg(any(test, target_os = "linux"))]
const BLACK: (u8, u8) = (16, 128);

/// Packs Y'CbCr planes (any chroma subsampling: 4:2:2, 4:2:0, 4:4:4) as YUYV of
/// `w × h` (`w` even): the picture fitted with its aspect kept, centred, black around.
/// Nearest samples: a webcam's picture, not a scaler's.
#[cfg(any(test, target_os = "linux"))]
pub fn pack_yuyv(planes: &[Plane], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 2);
    for _ in 0..w * h / 2 {
        out.extend_from_slice(&[BLACK.0, BLACK.1, BLACK.0, BLACK.1]);
    }
    let [y, cb, cr] = match planes {
        [y, cb, cr, ..] if y.width > 0 && y.height > 0 && cb.width > 0 && cb.height > 0 => {
            [y, cb, cr]
        }
        _ => return out,
    };
    // Where it goes: as large as fits.
    let k = (w as f64 / y.width as f64).min(h as f64 / y.height as f64);
    let (fw, fh) = (
        ((y.width as f64 * k) as usize).min(w),
        ((y.height as f64 * k) as usize).min(h),
    );
    let (ox, oy) = (((w - fw) / 2) & !1, (h - fh) / 2);
    for row in 0..fh {
        let sy = row * y.height / fh;
        let cy = sy * cb.height / y.height;
        let line = &mut out[((oy + row) * w + ox) * 2..];
        for col in 0..fw {
            let sx = col * y.width / fw;
            line[col * 2] = y.data[sy * y.width + sx];
            // Chroma once a pair, from the pair's first pixel.
            if col % 2 == 0 {
                let cx = sx * cb.width / y.width;
                line[col * 2 + 1] = cb.data[cy * cb.width + cx];
                if col * 2 + 3 < line.len() {
                    line[col * 2 + 3] = cr.data[cy * cr.width + cx];
                }
            }
        }
    }
    out
}

/// What the webcam is: its device (Linux, `/dev/videoN`; `None`: the first v4l2loopback
/// one), size and pictures a second (on Windows the camera's own: 1920×1080 at 30).
// (Its fields read on Linux only: Windows takes the camera's own.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Camera {
    pub device: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Shows the source as the webcam until the process is stopped.
#[cfg(windows)]
pub fn run(cfg: receive::Config, cam: Camera) -> Result<(), String> {
    mf::run(cfg, cam)
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn run(_cfg: receive::Config, _cam: Camera) -> Result<(), String> {
    Err("the virtual camera is built for Linux and Windows 11 so far".into())
}

/// Shows the source as the webcam until the process is stopped.
#[cfg(target_os = "linux")]
pub fn run(cfg: receive::Config, cam: Camera) -> Result<(), String> {
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    use std::time::{Duration, Instant};
    let (w, h) = ((cam.width.max(2) & !1) as usize, cam.height.max(2) as usize);
    let mut dev = v4l2::Loopback::open(cam.device.as_deref(), w as u32, h as u32)?;
    {
        eprintln!(
            "idn-display: virtual camera {} ({}) at {w}x{h}",
            dev.path, dev.card
        );
        let input = Input::open(InputConfig {
            receiver_name: cfg.receiver_name.clone(),
            secure: cfg.secure.clone(),
            discovery_server: cfg.discovery_server.clone(),
            skip_late: true,
            audio_channels: Some(Vec::new()),
            ..InputConfig::new(&cfg.source)
        })
        .map_err(|e| format!("{}: {e}", cfg.source))?;
        let black = pack_yuyv(&[], w, h);
        let mut frame = black.clone();
        let mut last_picture = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        let period = Duration::from_secs_f64(1.0 / cam.fps.clamp(1, 60) as f64);
        let mut next = Instant::now();
        loop {
            // Pictures as they come; the camera at its own steady rate.
            let wait = next.saturating_duration_since(Instant::now());
            if let Some(InputEvent::Video(v)) = input.recv(wait) {
                frame = pack_yuyv(&v.picture.planes, w, h);
                last_picture = Instant::now();
            }
            if Instant::now() < next {
                continue;
            }
            next += period;
            if next < Instant::now() {
                next = Instant::now() + period;
            }
            // Gone for a second: black, not the last picture frozen.
            let shown = if last_picture.elapsed() > Duration::from_secs(1) {
                &black
            } else {
                &frame
            };
            dev.write(shown)?;
        }
    }
}

/// Windows 11: IDN Camera, a Media Foundation virtual camera whose media source (idn-vcam's
/// DLL, run by Windows' Frame Server) shows what this writes to its shared memory.
#[cfg(windows)]
mod mf {
    use super::{Camera, receive};
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    use idn_vcam::shm;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::Win32::System::Memory::*;
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::core::{HSTRING, w};

    /// The shared memory, once the media source has made it (an app opened the camera).
    struct Shared {
        map: HANDLE,
        view: *mut u8,
        /// The view's real size: a picture is never copied past it, whatever the header
        /// (writable by any interactive user) says.
        len: usize,
        width: u32,
        height: u32,
    }

    impl Shared {
        fn open() -> Option<Shared> {
            // This session's (the media source makes one for the session at the screen).
            let mut session = 0u32;
            // SAFETY: plain Win32 calls with a valid out-pointer.
            unsafe {
                windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
                    windows::Win32::System::Threading::GetCurrentProcessId(),
                    &mut session,
                )
                .ok()?;
            }
            let name = HSTRING::from(shm::name(session));
            // SAFETY: Win32 calls; the view is checked before use.
            unsafe {
                let map =
                    OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, &name).ok()?;
                let view = MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
                if view.Value.is_null() {
                    let _ = CloseHandle(map);
                    return None;
                }
                let p = view.Value as *mut u8;
                let head = std::slice::from_raw_parts(p, shm::HEADER);
                let u =
                    |o: usize| u32::from_le_bytes(head[o..o + 4].try_into().unwrap_or_default());
                if head[0..4] != shm::MAGIC {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
                    let _ = CloseHandle(map);
                    return None;
                }
                let mut info = MEMORY_BASIC_INFORMATION::default();
                let len = if VirtualQuery(
                    Some(p as *const _),
                    &mut info,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                ) == 0
                {
                    0
                } else {
                    info.RegionSize
                };
                if len < shm::HEADER {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
                    let _ = CloseHandle(map);
                    return None;
                }
                // The size the media source offers, never the header's (writable by any
                // interactive user: 65535² was allocated every picture and aborted the
                // process). Another size: not ours to fill.
                if (u(shm::OFF_WIDTH), u(shm::OFF_HEIGHT)) != (shm::WIDTH, shm::HEIGHT) {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
                    let _ = CloseHandle(map);
                    return None;
                }
                Some(Shared {
                    map,
                    view: p,
                    len,
                    width: shm::WIDTH,
                    height: shm::HEIGHT,
                })
            }
        }

        /// One NV12 picture of the camera's size.
        fn write(&self, nv12: &[u8]) {
            // SAFETY: the view holds the header and one picture of the size it says.
            unsafe {
                let seq = &*(self.view.add(shm::OFF_SEQ) as *const AtomicU64);
                let written = &*(self.view.add(shm::OFF_WRITTEN) as *const AtomicU64);
                seq.fetch_add(1, Ordering::AcqRel);
                let n = nv12
                    .len()
                    .min(shm::nv12_len(self.width, self.height))
                    .min(self.len - shm::HEADER);
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), self.view.add(shm::HEADER), n);
                seq.fetch_add(1, Ordering::AcqRel);
                written.store(GetTickCount64(), Ordering::Release);
            }
        }
    }

    impl Drop for Shared {
        fn drop(&mut self) {
            // SAFETY: ours.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view as *mut _,
                });
                let _ = CloseHandle(self.map);
            }
        }
    }

    pub fn run(cfg: receive::Config, _cam: Camera) -> Result<(), String> {
        // SAFETY: COM and Media Foundation start for this process.
        let camera = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("Media Foundation: {e}"))?;
            let c = MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                w!("IDN Camera"),
                &HSTRING::from(idn_vcam::CLSID_TEXT),
                None,
            )
            .map_err(|e| format!("IDN Camera: {e} (it needs Windows 11 and the installer's camera part, idn_vcam.dll, registered)"))?;
            c.Start(None).map_err(|e| format!("IDN Camera: {e}"))?;
            c
        };
        eprintln!(
            "idn-display: IDN Camera is on ({}x{} at {}): choose it in the app",
            shm::WIDTH,
            shm::HEIGHT,
            shm::FPS
        );
        // The source is received and decoded only while an app has the camera open: the
        // shared memory is there then (the media source makes it and holds it). Looked for
        // every 500 ms while it isn't; while it is, let go and opened again every 5 s
        // (this program's handle alone keeps it, so an app that closed the camera shows as
        // it being gone), and the source let go with it.
        let open_input = || {
            Input::open(InputConfig {
                receiver_name: cfg.receiver_name.clone(),
                secure: cfg.secure.clone(),
                discovery_server: cfg.discovery_server.clone(),
                skip_late: true,
                audio_channels: Some(Vec::new()),
                ..InputConfig::new(&cfg.source)
            })
            .map_err(|e| format!("{}: {e}", cfg.source))
        };
        let mut input: Option<Input> = None;
        let mut shared: Option<Shared> = None;
        let mut looked: Option<Instant> = None;
        // The last failure to open the source (said once, not at every look).
        let mut open_failed: Option<String> = None;
        loop {
            let every = if shared.is_some() {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(500)
            };
            if looked.is_none_or(|t| t.elapsed() >= every) {
                looked = Some(Instant::now());
                // (Ours let go first: else ours alone would keep it there.)
                drop(shared.take());
                shared = Shared::open();
            }
            if shared.is_none() {
                if input.take().is_some() {
                    eprintln!("idn-display: IDN Camera closed by the app: the source let go");
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if input.is_none() {
                match open_input() {
                    Ok(i) => {
                        input = Some(i);
                        open_failed = None;
                    }
                    // Not the camera's end: the shared memory let go too, and both tried
                    // again at the next look.
                    Err(e) => {
                        if open_failed.as_deref() != Some(e.as_str()) {
                            eprintln!("idn-display: IDN Camera: {e}; trying again");
                            open_failed = Some(e);
                        }
                        drop(shared.take());
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            }
            let Some(s) = &shared else {
                continue;
            };
            if let Some(i) = &input
                && let Some(InputEvent::Video(v)) = i.recv(Duration::from_millis(100))
            {
                s.write(&shm::pack_nv12(&v.picture.planes, s.width, s.height));
            }
            // Kept for the program's life: the camera goes with it.
            let _ = &camera;
        }
    }
}

/// v4l2loopback's device: a video output a program writes pictures to.
#[cfg(target_os = "linux")]
mod v4l2 {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    // struct v4l2_capability: 104 bytes.
    #[repr(C)]
    struct Capability {
        driver: [u8; 16],
        card: [u8; 32],
        bus_info: [u8; 32],
        version: u32,
        capabilities: u32,
        device_caps: u32,
        reserved: [u32; 3],
    }

    // struct v4l2_pix_format, inside struct v4l2_format (its union is 200 bytes, 8-aligned).
    #[repr(C)]
    #[derive(Default)]
    struct PixFormat {
        width: u32,
        height: u32,
        pixelformat: u32,
        field: u32,
        bytesperline: u32,
        sizeimage: u32,
        colorspace: u32,
        private: u32,
        flags: u32,
        ycbcr_enc: u32,
        quantization: u32,
        xfer_func: u32,
    }

    #[repr(C)]
    struct Format {
        kind: u32,
        _pad: u32,
        pix: PixFormat,
        _rest: [u8; 200 - std::mem::size_of::<PixFormat>()],
    }

    const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600;
    const VIDIOC_S_FMT: libc::c_ulong = 0xC0D0_5605;
    const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
    const CAP_VIDEO_OUTPUT: u32 = 0x2;
    const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
    const YUYV: u32 = u32::from_le_bytes(*b"YUYV");
    const FIELD_NONE: u32 = 1;
    const COLORSPACE_REC709: u32 = 3;
    const YCBCR_ENC_709: u32 = 2;
    const QUANTIZATION_LIM_RANGE: u32 = 2;
    const XFER_FUNC_709: u32 = 1;

    const _: () = assert!(std::mem::size_of::<Capability>() == 104);
    const _: () = assert!(std::mem::size_of::<Format>() == 208);

    fn text(b: &[u8]) -> String {
        String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
    }

    pub struct Loopback {
        file: std::fs::File,
        pub path: String,
        pub card: String,
        size: usize,
    }

    /// The device's driver, card and whether it takes pictures (a video output).
    fn query(f: &std::fs::File) -> Option<(String, String, bool)> {
        let mut cap: Capability = unsafe { std::mem::zeroed() };
        // SAFETY: VIDIOC_QUERYCAP fills a v4l2_capability of the size asserted above.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), VIDIOC_QUERYCAP, &mut cap) };
        if r < 0 {
            return None;
        }
        let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        Some((
            text(&cap.driver),
            text(&cap.card),
            caps & CAP_VIDEO_OUTPUT != 0,
        ))
    }

    impl Loopback {
        /// `path`, or the first v4l2loopback device; set to YUYV `w × h`. A pipe or a
        /// file as `path` takes the pictures as they are (raw YUYV, for another program).
        pub fn open(path: Option<&str>, w: u32, h: u32) -> Result<Loopback, String> {
            use std::os::unix::fs::FileTypeExt;
            if let Some(p) = path
                && std::fs::metadata(p).is_ok_and(|m| m.file_type().is_fifo() || m.is_file())
            {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(p)
                    .map_err(|e| format!("{p}: {e}"))?;
                return Ok(Loopback {
                    file,
                    path: p.to_owned(),
                    card: "raw YUYV".into(),
                    size: (w * h * 2) as usize,
                });
            }
            let candidates: Vec<String> = match path {
                Some(p) => vec![p.to_owned()],
                None => {
                    let mut v: Vec<String> = std::fs::read_dir("/dev")
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|e| {
                            e.file_name()
                                .to_str()
                                .filter(|n| n.starts_with("video"))
                                .map(|n| format!("/dev/{n}"))
                        })
                        .collect();
                    v.sort_by_key(|p| {
                        p.trim_start_matches("/dev/video")
                            .parse::<u32>()
                            .unwrap_or(u32::MAX)
                    });
                    v
                }
            };
            let mut seen = Vec::new();
            for p in &candidates {
                let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(p) else {
                    continue;
                };
                let Some((driver, card, output)) = query(&file) else {
                    continue;
                };
                seen.push(format!("{p} ({driver})"));
                if path.is_none() && !driver.contains("loopback") {
                    continue;
                }
                if !output && path.is_some() && !driver.contains("loopback") {
                    return Err(format!("{p} ({driver}) doesn't take pictures"));
                }
                // SAFETY: Format is integers and byte arrays; all zeros is valid.
                let mut fmt: Format = unsafe { std::mem::zeroed() };
                fmt.kind = BUF_TYPE_VIDEO_OUTPUT;
                fmt.pix = PixFormat {
                    width: w,
                    height: h,
                    pixelformat: YUYV,
                    field: FIELD_NONE,
                    bytesperline: w * 2,
                    sizeimage: w * h * 2,
                    colorspace: COLORSPACE_REC709,
                    ycbcr_enc: YCBCR_ENC_709,
                    quantization: QUANTIZATION_LIM_RANGE,
                    xfer_func: XFER_FUNC_709,
                    ..Default::default()
                };
                // SAFETY: VIDIOC_S_FMT reads and fills a v4l2_format of the size asserted.
                if unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_S_FMT, &mut fmt) } < 0 {
                    return Err(format!(
                        "{p}: the format YUYV {w}x{h}: {} (another program writing to it?)",
                        std::io::Error::last_os_error()
                    ));
                }
                if (fmt.pix.width, fmt.pix.height, fmt.pix.pixelformat) != (w, h, YUYV) {
                    return Err(format!(
                        "{p} took {}x{} instead of YUYV {w}x{h}",
                        fmt.pix.width, fmt.pix.height
                    ));
                }
                return Ok(Loopback {
                    file,
                    path: p.clone(),
                    card,
                    size: (w * h * 2) as usize,
                });
            }
            Err(format!(
                "no v4l2loopback device{}: install it (Debian/Ubuntu: sudo apt install v4l2loopback-dkms) and load it with sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"IDN Camera\"",
                if seen.is_empty() {
                    String::new()
                } else {
                    format!(" among {}", seen.join(", "))
                }
            ))
        }

        /// One picture (YUYV, the size set).
        pub fn write(&mut self, yuyv: &[u8]) -> Result<(), String> {
            debug_assert_eq!(yuyv.len(), self.size);
            self.file
                .write_all(yuyv)
                .map_err(|e| format!("{}: {e}", self.path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Plane {
        Plane {
            width: w,
            height: h,
            data: (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .map(|(x, y)| f(x, y))
                .collect(),
        }
    }

    #[test]
    fn nothing_is_black() {
        let b = pack_yuyv(&[], 8, 2);
        assert_eq!(b.len(), 32);
        assert!(b.chunks(4).all(|q| q == [16, 128, 16, 128]));
    }

    #[test]
    fn a_422_picture_packs_as_it_is() {
        // 4×2, Y counts up, Cb/Cr per pair.
        let y = plane(4, 2, |x, y| (10 * y + x) as u8 + 50);
        let cb = plane(2, 2, |x, y| 100 + (x + 2 * y) as u8);
        let cr = plane(2, 2, |x, y| 200 + (x + 2 * y) as u8);
        let out = pack_yuyv(&[y, cb, cr], 4, 2);
        assert_eq!(
            out,
            vec![
                50, 100, 51, 200, 52, 101, 53, 201, 60, 102, 61, 202, 62, 103, 63, 203
            ]
        );
    }

    #[test]
    fn a_420_picture_and_letterboxing() {
        // 4:2:0 at 4×2 onto 8×4: the same shape doubled; onto 8×8: bars above and below.
        let y = plane(4, 2, |x, _| 60 + x as u8);
        let cb = plane(2, 1, |_, _| 90);
        let cr = plane(2, 1, |_, _| 150);
        let wide = pack_yuyv(&[y.clone(), cb.clone(), cr.clone()], 8, 4);
        assert_eq!(&wide[..8], &[60, 90, 60, 150, 61, 90, 61, 150]);
        let tall = pack_yuyv(&[y, cb, cr], 8, 8);
        // 4:2 into 8:8 → 8×4 in the middle: rows 0–1 black, rows 2–5 picture, 6–7 black.
        assert!(tall[..8 * 2 * 2].chunks(4).all(|q| q == [16, 128, 16, 128]));
        assert_eq!(tall[2 * 16], 60);
        assert!(tall[6 * 16..].chunks(4).all(|q| q == [16, 128, 16, 128]));
    }
}
