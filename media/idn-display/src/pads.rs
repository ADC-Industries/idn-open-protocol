//! Gamepads to the source (R.25 stage 2): up to four, each one's state sent as it changes,
//! in `remote::PAD_BUTTONS`' order with the axes as `<p>` has them (sticks −32768–32767,
//! y down; triggers 0–32767). Linux reads the pads' event devices (the account needs to
//! read /dev/input, as games do); Windows reads XInput's four.

use crate::receive::Link;
use idn_common::sync::LockOk as _;
use idn_open_protocol::remote::Control;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Reads the gamepads on a thread of its own until the program ends.
pub fn start(link: Arc<Link>) {
    let _ = std::thread::Builder::new()
        .name("idn-display-pads".into())
        .spawn(move || {
            // Whether it went: one not sent (control off for a moment) is sent again, not
            // taken as said (a trigger held stayed held there; bug round E, E8-8).
            let send = |id: usize, buttons: u32, axes: [i16; 6]| -> bool {
                if link.controllable.load(Ordering::Relaxed)
                    && let Some(i) = link.input.lock_ok().as_ref()
                {
                    i.send_control(&[Control::Pad {
                        id: id as u8,
                        buttons,
                        axes,
                    }]);
                    return true;
                }
                false
            };
            // Whether the source takes control now (Windows asks the pads slowly while not).
            let active = || link.controllable.load(Ordering::Relaxed);
            if let Err(e) = run(send, active) {
                eprintln!("idn-display: gamepads: {e}");
            }
        });
}

#[cfg(target_os = "linux")]
fn run(
    send: impl Fn(usize, u32, [i16; 6]) -> bool,
    _active: impl Fn() -> bool,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    /// One pad: its device, where it is, its axes' ranges and its state.
    struct Pad {
        file: std::fs::File,
        path: std::path::PathBuf,
        ranges: [(i32, i32); 6],
        has_triggers: bool,
        buttons: u32,
        axes: [i16; 6],
        sent: Option<(u32, [i16; 6])>,
    }

    const EV_KEY: u16 = 0x01;
    const EV_ABS: u16 = 0x03;
    const BTN_SOUTH: u16 = 0x130;
    const BTN_TL2: u16 = 0x138;
    const BTN_TR2: u16 = 0x139;
    const ABS_HAT0X: u16 = 0x10;
    const ABS_HAT0Y: u16 = 0x11;
    // The buttons in `PAD_BUTTONS`' order (as the Linux source makes them again).
    const KEYS: [u16; 15] = [
        0x130, 0x131, 0x134, 0x133, 0x136, 0x137, 0x13a, 0x13b, 0x13c, 0x13d, 0x13e, 0x220, 0x221,
        0x222, 0x223,
    ];
    // The axes in `<p>`'s order: left x, y, right x, y, left and right triggers.
    const AXES: [u16; 6] = [0x00, 0x01, 0x03, 0x04, 0x02, 0x05];
    const UP: u32 = 1 << 11;
    const DOWN: u32 = 1 << 12;
    const LEFT: u32 = 1 << 13;
    const RIGHT: u32 = 1 << 14;

    fn ioc_read(nr: u64, size: usize) -> u64 {
        (2 << 30) | ((size as u64) << 16) | ((b'E' as u64) << 8) | nr
    }
    fn bit(bits: &[u8], n: u16) -> bool {
        bits.get(n as usize / 8)
            .is_some_and(|b| b >> (n % 8) & 1 == 1)
    }
    /// The device at `path` if it is a gamepad (not one this machine's own source made).
    fn open(path: &std::path::Path) -> Option<Pad> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let fd = file.as_raw_fd();
        let mut keys = [0u8; 0x300 / 8];
        let mut name = [0u8; 128];
        // SAFETY: each buffer is as long as the size its request gives.
        unsafe {
            if libc::ioctl(
                fd,
                ioc_read(0x20 + EV_KEY as u64, keys.len()) as _,
                keys.as_mut_ptr(),
            ) < 0
            {
                return None;
            }
            libc::ioctl(fd, ioc_read(0x06, name.len()) as _, name.as_mut_ptr());
        }
        let name = String::from_utf8_lossy(&name);
        if !bit(&keys, BTN_SOUTH) || name.starts_with("IDN Gamepad") {
            return None;
        }
        let mut ranges = [(0, 0); 6];
        for (k, &a) in AXES.iter().enumerate() {
            let mut info = [0i32; 6];
            // SAFETY: `input_absinfo` is six 32-bit numbers.
            if unsafe { libc::ioctl(fd, ioc_read(0x40 + a as u64, 24) as _, info.as_mut_ptr()) }
                == 0
            {
                ranges[k] = (info[1], info[2]);
            }
        }
        Some(Pad {
            file,
            path: path.to_owned(),
            has_triggers: ranges[4].1 > ranges[4].0,
            ranges,
            buttons: 0,
            axes: [0; 6],
            sent: None,
        })
    }

    let mut pads: Vec<Option<Pad>> = (0..4).map(|_| None).collect();
    let mut scanned = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    loop {
        // New pads, every two seconds.
        if scanned.elapsed() >= Duration::from_secs(2) {
            scanned = Instant::now();
            let mut paths: Vec<_> = std::fs::read_dir("/dev/input")?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("event"))
                })
                .collect();
            paths.sort();
            for path in paths {
                if pads.iter().flatten().any(|p| p.path == path) {
                    continue;
                }
                let Some(slot) = pads.iter().position(Option::is_none) else {
                    break;
                };
                if let Some(pad) = open(&path) {
                    eprintln!("idn-display: gamepad {} ({})", slot + 1, path.display());
                    pads[slot] = Some(pad);
                }
            }
        }
        let mut fds: Vec<libc::pollfd> = pads
            .iter()
            .flatten()
            .map(|p| libc::pollfd {
                fd: p.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if fds.is_empty() {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        // SAFETY: `fds` is as long as the count given.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 500) };
        for (n, slot) in pads.iter_mut().enumerate() {
            let Some(pad) = slot else { continue };
            // `input_event`: a `timeval`, then type, code and value (16 bytes on 32-bit
            // systems, 24 on 64-bit ones).
            const EV: usize = std::mem::size_of::<libc::input_event>();
            let mut buf = [0u8; EV * 64];
            let got = std::io::Read::read(&mut pad.file, &mut buf);
            let got = match got {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                // Unplugged: its buttons let go there too.
                Err(_) => {
                    if pad.sent.is_some() {
                        send(n, 0, [0; 6]);
                    }
                    eprintln!("idn-display: gamepad {} gone", n + 1);
                    *slot = None;
                    continue;
                }
            };
            for ev in buf[..got].as_chunks::<EV>().0 {
                let at = EV - 8;
                let kind = u16::from_ne_bytes([ev[at], ev[at + 1]]);
                let code = u16::from_ne_bytes([ev[at + 2], ev[at + 3]]);
                let value = i32::from_ne_bytes([ev[at + 4], ev[at + 5], ev[at + 6], ev[at + 7]]);
                match kind {
                    EV_KEY => {
                        let set = |b: &mut u32, mask: u32| {
                            if value != 0 { *b |= mask } else { *b &= !mask }
                        };
                        if let Some(k) = KEYS.iter().position(|&c| c == code) {
                            set(&mut pad.buttons, 1 << k);
                        } else if !pad.has_triggers && (code == BTN_TL2 || code == BTN_TR2) {
                            let k = if code == BTN_TL2 { 4 } else { 5 };
                            pad.axes[k] = if value != 0 { i16::MAX } else { 0 };
                        }
                    }
                    EV_ABS if code == ABS_HAT0X => {
                        pad.buttons &= !(LEFT | RIGHT);
                        pad.buttons |= if value < 0 {
                            LEFT
                        } else if value > 0 {
                            RIGHT
                        } else {
                            0
                        };
                    }
                    EV_ABS if code == ABS_HAT0Y => {
                        pad.buttons &= !(UP | DOWN);
                        pad.buttons |= if value < 0 {
                            UP
                        } else if value > 0 {
                            DOWN
                        } else {
                            0
                        };
                    }
                    EV_ABS => {
                        if let Some(k) = AXES.iter().position(|&c| c == code) {
                            let (lo, hi) = pad.ranges[k];
                            if hi > lo {
                                let t = (value - lo) as f64 / (hi - lo) as f64;
                                pad.axes[k] = if k < 4 {
                                    (t * 65535.0 - 32768.0).round().clamp(-32768.0, 32767.0) as i16
                                } else {
                                    (t * 32767.0).round().clamp(0.0, 32767.0) as i16
                                };
                            }
                        }
                    }
                    // A report's end: the state sent if it changed.
                    0 if code == 0 => {
                        let now = (pad.buttons, pad.axes);
                        if pad.sent != Some(now) && send(n, now.0, now.1) {
                            pad.sent = Some(now);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(windows)]
fn run(
    send: impl Fn(usize, u32, [i16; 6]) -> bool,
    active: impl Fn() -> bool,
) -> std::io::Result<()> {
    use std::time::{Duration, Instant};
    use windows::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD_BUTTON_FLAGS, XINPUT_STATE, XInputGetState,
    };

    // XInput's buttons in `PAD_BUTTONS`' order (it has no guide button: "mode" stays up).
    const BUTTONS: [u16; 15] = [
        0x1000, 0x2000, 0x4000, 0x8000, 0x0100, 0x0200, 0x0020, 0x0010, 0, 0x0040, 0x0080, 0x0001,
        0x0002, 0x0004, 0x0008,
    ];
    /// A slot: plugged in, its last packet and what was last sent.
    #[derive(Clone, Copy, Default)]
    struct Slot {
        on: bool,
        packet: u32,
        sent: Option<(u32, [i16; 6])>,
    }
    let mut pads = [Slot::default(); 4];
    let mut scanned = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    loop {
        // Unplugged slots are slow to ask: every two seconds.
        let scan = scanned.elapsed() >= Duration::from_secs(2);
        if scan {
            scanned = Instant::now();
        }
        for (n, pad) in pads.iter_mut().enumerate() {
            if !pad.on && !scan {
                continue;
            }
            let mut st = XINPUT_STATE::default();
            // SAFETY: `st` is the state XInput fills.
            let ok = unsafe { XInputGetState(n as u32, &mut st) } == 0;
            if !ok {
                if pad.on {
                    if pad.sent.is_some() {
                        send(n, 0, [0; 6]);
                    }
                    eprintln!("idn-display: gamepad {} gone", n + 1);
                    *pad = Slot::default();
                }
                continue;
            }
            if !pad.on {
                eprintln!("idn-display: gamepad {}", n + 1);
                pad.on = true;
            }
            if pad.sent.is_some() && st.dwPacketNumber == pad.packet {
                continue;
            }
            pad.packet = st.dwPacketNumber;
            let g = st.Gamepad;
            let mut buttons = 0u32;
            for (bit, &b) in BUTTONS.iter().enumerate() {
                if b != 0
                    && g.wButtons & XINPUT_GAMEPAD_BUTTON_FLAGS(b) != XINPUT_GAMEPAD_BUTTON_FLAGS(0)
                {
                    buttons |= 1 << bit;
                }
            }
            // XInput's y is up; `<p>`'s is down.
            let flip = |v: i16| (-(v as i32)).clamp(-32768, 32767) as i16;
            let trigger = |v: u8| (v as i32 * 32767 / 255) as i16;
            let axes = [
                g.sThumbLX,
                flip(g.sThumbLY),
                g.sThumbRX,
                flip(g.sThumbRY),
                trigger(g.bLeftTrigger),
                trigger(g.bRightTrigger),
            ];
            if pad.sent != Some((buttons, axes)) {
                pad.sent = send(n, buttons, axes).then_some((buttons, axes));
            }
        }
        // XInput is asked, not told: 250 times a second while a pad is plugged in and the
        // source takes control; else twice a second (a pad plugged in is still seen within
        // the 2 s scan, and its state sent once control comes back).
        let busy = pads.iter().any(|p| p.on) && active();
        std::thread::sleep(Duration::from_millis(if busy { 4 } else { 500 }));
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run(
    _send: impl Fn(usize, u32, [i16; 6]) -> bool,
    _active: impl Fn() -> bool,
) -> std::io::Result<()> {
    Err(std::io::Error::other("not on this system yet"))
}
