//! Pictures and sound between programs on one machine (`machine/1`,
//! spec/extensions/idn-extensions.md §E.26; within one program, [`crate::local`] does without
//! any connection): an output puts each picture's planes and each block of sound, as they
//! are, into rings of shared memory that its receivers on this machine read. Nothing is
//! encoded, encrypted or sent through the network stack for them; the connection carries
//! only setup and control.
//!
//! An output has a control page (the newest picture's and block's numbers, and a doorbell
//! rung after each is written) and two rings: pictures ([`PIC_SLOTS`] slots) and sound
//! ([`AUDIO_SLOTS`]). A slot starts with a stamp (twice the item's number, plus one while it
//! is being written) and the bytes used, then the item. A reader copies the item out and
//! checks the stamp before and after: an item written over meanwhile (the reader was too
//! slow) is dropped, never a mix of two used. A receiver's watcher waits on the doorbell
//! (a futex on Linux; elsewhere it looks every millisecond) and hands the new items' numbers
//! to the input.

use crate::shq::{Format, Fourcc, Picture, Plane};
use idn_common::shm::Region;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::time::Duration;

/// Picture slots: the picture being read, the next ones, and room.
pub const PIC_SLOTS: usize = 4;
/// Sound slots: 128 ms of 2 ms blocks, more of longer ones.
pub const AUDIO_SLOTS: usize = 64;
/// A slot's own header: the stamp and the bytes used.
const SLOT_HEAD: usize = 16;
/// A picture's description in its slot, before its planes.
pub(crate) const PIC_HEAD: usize = 112;
/// A block's description in its slot, before its channels.
const AUDIO_HEAD: usize = 32;
/// The most planes a picture has (Y, Cb, Cr, alpha).
pub const MAX_PLANES: usize = 4;
/// The largest slot made or mapped (an 8K picture with alpha, 4:4:4).
pub const MAX_SLOT_BYTES: usize = 8192 * 8192 * 4 + (1 << 20);
/// The control page's size.
const CONTROL_BYTES: usize = 4096;
/// Where the GPU slots' stamps start in the control page (`gpu/1`, §E.27), and how many
/// there may be.
const GPU_STAMP_AT: usize = 64;
const GPU_STAMPS: usize = 8;
/// A picture's header: its GPU slot plus one (0: none), its number there, and flags.
const PIC_GPU_SLOT: usize = 76;
const PIC_GPU_SEQ: usize = 80;
const PIC_FLAGS: usize = 88;
/// The planes are on the GPU only: the slot holds their sizes, not their samples.
const FLAG_GPU_ONLY: u32 = 1;

fn invalid(m: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_owned())
}

// ---------------------------------------------------------------------------------------
// The control page and its doorbell.

/// An output's control page: the newest picture and block (their numbers) and the bell.
pub struct Control {
    region: Region,
}

impl Control {
    pub fn create() -> std::io::Result<Control> {
        Ok(Control {
            region: Region::create("idn-machine-c", CONTROL_BYTES)?,
        })
    }

    pub fn open(name: &str) -> std::io::Result<Control> {
        Ok(Control {
            region: Region::open(name, CONTROL_BYTES)?,
        })
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }

    fn head(&self, at: usize) -> &AtomicU64 {
        self.region.atomic_u64(at).expect("in the control page")
    }

    fn bell(&self) -> &AtomicU32 {
        // SAFETY: 4-aligned, inside the page, living as long as the region.
        unsafe { &*(self.region.as_ptr().add(16) as *const AtomicU32) }
    }

    /// The newest picture's and block's numbers (0: none yet).
    pub fn heads(&self) -> (u64, u64) {
        (
            self.head(0).load(Ordering::Acquire),
            self.head(8).load(Ordering::Acquire),
        )
    }

    /// Picture `seq` (`sound`: block `seq`) is written: the head moves and the bell rings.
    pub fn publish(&self, sound: bool, seq: u64) {
        self.head(if sound { 8 } else { 0 })
            .store(seq, Ordering::Release);
        self.bell().fetch_add(1, Ordering::Release);
        futex_wake(self.bell());
    }

    /// GPU slot `slot`'s stamp (`gpu/1`, §E.27): twice the number of the picture in it, plus
    /// one while it is being written, as a CPU slot's.
    pub fn gpu_stamp(&self, slot: usize) -> Option<&AtomicU64> {
        (slot < GPU_STAMPS).then(|| self.head(GPU_STAMP_AT + 8 * slot))
    }

    /// Waits up to `timeout` for heads other than `seen`; the heads then.
    pub fn wait(&self, seen: (u64, u64), timeout: Duration) -> (u64, u64) {
        let bell = self.bell().load(Ordering::Acquire);
        let now = self.heads();
        if now != seen {
            return now;
        }
        futex_wait(self.bell(), bell, timeout);
        self.heads()
    }
}

#[cfg(target_os = "linux")]
fn futex_wait(word: &AtomicU32, was: u32, timeout: Duration) {
    let ts = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    // SAFETY: a futex on a word of shared memory (not process-private: other programs
    // wake it); returns at once if the word changed, or on a wake or the timeout.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            was,
            &ts as *const libc::timespec,
            std::ptr::null::<u32>(),
            0,
        );
    }
}

#[cfg(target_os = "linux")]
fn futex_wake(word: &AtomicU32) {
    // SAFETY: as above.
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE, i32::MAX);
    }
}

/// Elsewhere: a look every millisecond (Windows' WaitOnAddress works within one process
/// only).
#[cfg(not(target_os = "linux"))]
fn futex_wait(word: &AtomicU32, was: u32, timeout: Duration) {
    let end = std::time::Instant::now() + timeout;
    while word.load(Ordering::Acquire) == was && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(target_os = "linux"))]
fn futex_wake(_word: &AtomicU32) {}

// ---------------------------------------------------------------------------------------
// Rings of stamped slots.

/// A ring of `slots` slots of `slot_bytes` each.
pub struct Ring {
    region: Region,
    slot_bytes: usize,
    slots: usize,
}

impl Ring {
    /// A new ring whose slots hold `need` bytes, with an eighth more room.
    pub fn create(prefix: &str, need: usize, slots: usize) -> std::io::Result<Ring> {
        let slot_bytes = (need + need / 8 + SLOT_HEAD)
            .clamp(4096, MAX_SLOT_BYTES)
            .next_multiple_of(4096);
        Ok(Ring {
            region: Region::create(prefix, slot_bytes * slots)?,
            slot_bytes,
            slots,
        })
    }

    pub fn open(name: &str, slot_bytes: usize, slots: usize) -> std::io::Result<Ring> {
        if !(4096..=MAX_SLOT_BYTES).contains(&slot_bytes)
            || !slot_bytes.is_multiple_of(4096)
            || !(1..=1024).contains(&slots)
        {
            return Err(invalid("a ring's slots"));
        }
        Ok(Ring {
            region: Region::open(name, slot_bytes * slots)?,
            slot_bytes,
            slots,
        })
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }

    pub fn slot_bytes(&self) -> usize {
        self.slot_bytes
    }

    /// The ring's mapping: where it is in this process, and its length.
    #[cfg(feature = "gpu")]
    pub(crate) fn memory(&self) -> (*mut u8, usize) {
        (self.region.as_ptr(), self.region.len())
    }

    /// Whether `need` bytes fit in a slot.
    pub fn fits(&self, need: usize) -> bool {
        need.saturating_add(SLOT_HEAD) <= self.slot_bytes
    }

    fn base(&self, seq: u64) -> usize {
        (seq as usize % self.slots) * self.slot_bytes
    }

    /// Writes item `seq` from `parts` (one after another); false if it doesn't fit.
    fn write(&self, seq: u64, parts: &[&[u8]]) -> bool {
        let used: usize = parts.iter().map(|p| p.len()).sum();
        self.write_with(seq, used, |slot| {
            let mut at = 0;
            for p in parts {
                slot[at..at + p.len()].copy_from_slice(p);
                at += p.len();
            }
            true
        })
    }

    /// Writes item `seq`, `used` bytes, as `fill` writes them into its slot; one `fill`
    /// says it couldn't write is left unreadable (its stamp odd).
    fn write_with(&self, seq: u64, used: usize, fill: impl FnOnce(&mut [u8]) -> bool) -> bool {
        let base = self.base(seq);
        if !self.fits(used) || base + SLOT_HEAD + used > self.region.len() {
            return false;
        }
        let Some(stamp) = self.region.atomic_u64(base) else {
            return false;
        };
        stamp.store(seq * 2 + 1, Ordering::Relaxed);
        // The odd stamp seen before any of the item's bytes (on every CPU, ARM too).
        fence(Ordering::Release);
        self.region.write(base + 8, &(used as u64).to_le_bytes());
        // SAFETY: the slot is within the mapping (`fits`), and while its stamp is odd no
        // reader takes what it holds.
        let slot = unsafe {
            std::slice::from_raw_parts_mut(self.region.as_ptr().add(base + SLOT_HEAD), used)
        };
        if !fill(slot) {
            return false;
        }
        stamp.store(seq * 2, Ordering::Release);
        true
    }

    /// Item `seq`, as `read` makes it from (the region, where it starts, its length); `None`
    /// if it is gone, being written, or `read` says no.
    fn read<T>(
        &self,
        seq: u64,
        read: impl FnOnce(&Region, usize, usize) -> Option<T>,
    ) -> Option<T> {
        let base = self.base(seq);
        let stamp = self.region.atomic_u64(base)?;
        if stamp.load(Ordering::Acquire) != seq * 2 {
            return None;
        }
        let mut len = [0u8; 8];
        self.region.read(base + 8, &mut len);
        let used = u64::from_le_bytes(len) as usize;
        if !self.fits(used) {
            return None;
        }
        let item = read(&self.region, base + SLOT_HEAD, used)?;
        // Written over while copied: dropped. (The copy done before the stamp is looked
        // at again.)
        fence(Ordering::Acquire);
        (stamp.load(Ordering::Relaxed) == seq * 2).then_some(item)
    }
}

// ---------------------------------------------------------------------------------------
// Pictures.

/// What a picture's slot holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Shared {
    pub fourcc: Fourcc,
    pub width: usize,
    pub height: usize,
    pub frame_rate: (u32, u32),
    pub timecode: i64,
    pub timestamp: i64,
    /// The planes; with [`gpu_only`](Self::gpu_only), their sizes with no samples.
    pub planes: Vec<Plane>,
    pub metadata: Option<String>,
    /// The picture is in the output's GPU ring too (§E.27): (slot, number there).
    pub gpu: Option<(u32, u64)>,
    /// Only there: the planes here are empty.
    pub gpu_only: bool,
}

/// A picture to put in a ring, borrowed from the output's frame.
#[derive(Clone, Copy)]
pub struct Parts<'a> {
    pub fourcc: Fourcc,
    pub width: usize,
    pub height: usize,
    pub frame_rate: (u32, u32),
    pub timecode: i64,
    pub timestamp: i64,
    pub planes: &'a [Plane],
    pub metadata: Option<&'a str>,
    /// In the output's GPU ring too: (slot, number there).
    pub gpu: Option<(u32, u64)>,
    /// Leave the planes' samples out (every reader takes the GPU's).
    pub gpu_only: bool,
}

/// The bytes a picture takes in a slot.
pub fn picture_bytes(planes: &[Plane], metadata: Option<&str>) -> usize {
    PIC_HEAD + planes.iter().map(|p| p.data.len()).sum::<usize>() + metadata.map_or(0, str::len)
}

fn fourcc_code(f: Fourcc) -> [u8; 4] {
    *match f {
        Fourcc::Shq0 => b"SHQ0",
        Fourcc::Shq2 => b"SHQ2",
        Fourcc::Shq4 => b"SHQ4",
        Fourcc::Shq7 => b"SHQ7",
        Fourcc::Shq9 => b"SHQ9",
    }
}

/// Writes picture `seq`; false if it doesn't fit or has too many planes.
pub fn put_picture(ring: &Ring, seq: u64, s: &Parts) -> bool {
    let Some(h) = pic_header(s) else {
        return false;
    };
    let meta = s.metadata.unwrap_or("");
    let mut parts: Vec<&[u8]> = vec![&h];
    if !(s.gpu_only && s.gpu.is_some()) {
        parts.extend(s.planes.iter().map(|p| p.data.as_slice()));
    }
    parts.push(meta.as_bytes());
    ring.write(seq, &parts)
}

/// Writes a picture's planes into the places given (Y, Cb, Cr, each plane's rows packed);
/// says whether it wrote them.
pub type PlaneFill<'a> = dyn FnMut(&mut [&mut [u8]]) -> bool + 'a;

/// As [`put_picture`], `s`'s planes having their sizes only: `fill` writes their samples
/// straight into the slot, each plane's rows packed (from memory the GPU wrote, with no
/// copy in between). False if it doesn't fit, or `fill` says it couldn't.
pub fn put_picture_filled(
    ring: &Ring,
    seq: u64,
    s: &Parts,
    fill: impl FnOnce(&mut [&mut [u8]]) -> bool,
) -> bool {
    let Some(h) = pic_header(&Parts {
        gpu_only: false,
        ..*s
    }) else {
        return false;
    };
    let meta = s.metadata.unwrap_or("");
    let lens: Vec<usize> = s.planes.iter().map(|p| p.width * p.height).collect();
    let used = PIC_HEAD + lens.iter().sum::<usize>() + meta.len();
    ring.write_with(seq, used, |slot| {
        let (head, mut rest) = slot.split_at_mut(PIC_HEAD);
        head.copy_from_slice(&h);
        let mut planes = Vec::with_capacity(lens.len());
        for &len in &lens {
            let (p, r) = rest.split_at_mut(len);
            planes.push(p);
            rest = r;
        }
        let ok = fill(&mut planes);
        rest.copy_from_slice(meta.as_bytes());
        ok
    })
}

/// A picture's header as a ring has it; `None` with too many planes.
fn pic_header(s: &Parts) -> Option<Vec<u8>> {
    if s.planes.len() > MAX_PLANES {
        return None;
    }
    let meta = s.metadata.unwrap_or("");
    let mut h = Vec::with_capacity(PIC_HEAD);
    h.extend_from_slice(&fourcc_code(s.fourcc));
    for v in [
        s.width as u32,
        s.height as u32,
        s.frame_rate.0,
        s.frame_rate.1,
        s.planes.len() as u32,
        meta.len() as u32,
    ] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    h.extend_from_slice(&s.timecode.to_le_bytes());
    h.extend_from_slice(&s.timestamp.to_le_bytes());
    for p in s.planes {
        h.extend_from_slice(&(p.width as u32).to_le_bytes());
        h.extend_from_slice(&(p.height as u32).to_le_bytes());
    }
    h.resize(PIC_HEAD, 0);
    let gpu_only = s.gpu_only && s.gpu.is_some();
    if let Some((slot, seq)) = s.gpu {
        h[PIC_GPU_SLOT..PIC_GPU_SLOT + 4].copy_from_slice(&(slot + 1).to_le_bytes());
        h[PIC_GPU_SEQ..PIC_GPU_SEQ + 8].copy_from_slice(&seq.to_le_bytes());
    }
    h[PIC_FLAGS..PIC_FLAGS + 4]
        .copy_from_slice(&(if gpu_only { FLAG_GPU_ONLY } else { 0 }).to_le_bytes());
    Some(h)
}

/// A picture's header in a ring, its planes' sizes checked.
struct PicHead {
    fourcc: Fourcc,
    width: usize,
    height: usize,
    frame_rate: (u32, u32),
    meta_len: usize,
    timecode: i64,
    timestamp: i64,
    gpu: Option<(u32, u64)>,
    gpu_only: bool,
    sizes: Vec<(usize, usize)>,
}

fn pic_head(r: &Region, at: usize, used: usize) -> Option<PicHead> {
    if used < PIC_HEAD {
        return None;
    }
    let mut h = vec![0u8; PIC_HEAD];
    r.read(at, &mut h);
    let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let i64_at = |i: usize| i64::from_le_bytes(h[i..i + 8].try_into().unwrap());
    let fourcc = Fourcc::from_bytes(h[0..4].try_into().unwrap())?;
    let (width, height) = (u32_at(4) as usize, u32_at(8) as usize);
    let (n, meta_len) = (u32_at(20) as usize, u32_at(24) as usize);
    let gpu = match u32_at(PIC_GPU_SLOT) {
        0 => None,
        k => Some((
            k - 1,
            u64::from_le_bytes(h[PIC_GPU_SEQ..PIC_GPU_SEQ + 8].try_into().unwrap()),
        )),
    };
    if n == 0 || n > MAX_PLANES {
        return None;
    }
    let mut sizes = Vec::with_capacity(n);
    for k in 0..n {
        let (pw, ph) = (u32_at(44 + 8 * k) as usize, u32_at(48 + 8 * k) as usize);
        // The planes as the header's size has them: the first that size, the others no
        // larger (a mismatched writer could make a receiver index past a
        // plane).
        if pw > 16384
            || ph > 16384
            || (k == 0 && (pw, ph) != (width, height))
            || pw > width
            || ph > height
        {
            return None;
        }
        sizes.push((pw, ph));
    }
    Some(PicHead {
        fourcc,
        width,
        height,
        frame_rate: (u32_at(12), u32_at(16)),
        meta_len,
        timecode: i64_at(28),
        timestamp: i64_at(36),
        gpu,
        gpu_only: gpu.is_some() && u32_at(PIC_FLAGS) & FLAG_GPU_ONLY != 0,
        sizes,
    })
}

impl PicHead {
    /// The picture, its planes `planes` and its metadata at `p` (`end` the slot's used end).
    fn picture(self, r: &Region, planes: Vec<Plane>, p: usize, end: usize) -> Option<Shared> {
        if p + self.meta_len > end {
            return None;
        }
        let metadata = (self.meta_len > 0).then(|| {
            let mut m = vec![0u8; self.meta_len];
            r.read(p, &mut m);
            String::from_utf8_lossy(&m).into_owned()
        });
        Some(Shared {
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            frame_rate: self.frame_rate,
            timecode: self.timecode,
            timestamp: self.timestamp,
            planes,
            metadata,
            gpu: self.gpu,
            gpu_only: self.gpu_only,
        })
    }

    fn empty_planes(&self) -> Vec<Plane> {
        self.sizes
            .iter()
            .map(|&(width, height)| Plane {
                width,
                height,
                data: Vec::new(),
            })
            .collect()
    }
}

/// Picture `seq` from its ring.
pub fn get_picture(ring: &Ring, seq: u64) -> Option<Shared> {
    ring.read(seq, |r, at, used| {
        let head = pic_head(r, at, used)?;
        let end = at + used;
        let mut p = at + PIC_HEAD;
        if head.gpu_only {
            let planes = head.empty_planes();
            return head.picture(r, planes, p, end);
        }
        let mut planes = Vec::with_capacity(head.sizes.len());
        for &(pw, ph) in &head.sizes {
            let len = pw * ph;
            if p + len > end {
                return None;
            }
            // Copied straight into memory pictures hand back (not zeroed, not fresh pages:
            // a 1080p picture's 4 MB each time otherwise).
            let mut data = Plane::buffer(len);
            r.read(p, &mut data);
            p += len;
            planes.push(Plane {
                width: pw,
                height: ph,
                data,
            });
        }
        head.picture(r, planes, p, end)
    })
}

/// Picture `seq` from its ring for an input that draws on the GPU: its planes copied from
/// the ring straight into the GPU's staging memory (`up`), the picture's planes then
/// empty, with no copy in this process's memory first. A picture the source has in its
/// GPU ring too (§E.27) comes as [`get_picture`] has it, with no upload.
#[cfg(feature = "gpu")]
pub(crate) fn get_picture_gpu(
    ring: &Ring,
    seq: u64,
    up: &mut crate::gpu::Uploads,
    host: Option<&wgpu::Buffer>,
) -> Option<(Shared, Option<crate::gpu::GpuPicture>)> {
    ring.read(seq, |r, at, used| {
        let head = pic_head(r, at, used)?;
        let end = at + used;
        let mut p = at + PIC_HEAD;
        if head.gpu.is_some() {
            return None;
        }
        let planes_end = head.sizes.iter().map(|&(w, h)| w * h).sum::<usize>() + p;
        if planes_end > end || end > r.len() {
            return None;
        }
        // The ring imported as a GPU buffer (S2): the GPU copies the planes out of it
        // (rows of whole words, within what was imported); the stamp is looked at again
        // after, as for any read.
        if let Some(host) = host {
            let mut at = p;
            let from: Vec<crate::gpu::PlaneAt> = head
                .sizes
                .iter()
                .map(|&(w, h)| {
                    let pl = crate::gpu::PlaneAt {
                        offset: at as u64,
                        row_bytes: w as u32,
                        width: w as u32,
                        height: h as u32,
                    };
                    at += w * h;
                    pl
                })
                .collect();
            let fits = from.iter().all(|pl| {
                pl.offset % 4 == 0
                    && pl.row_bytes % 4 == 0
                    && pl.offset + pl.row_bytes as u64 * pl.height as u64 <= host.size()
            });
            if fits && let Some(g) = up.copy_host(host, &from) {
                let planes = head.empty_planes();
                return Some((head.picture(r, planes, at, end)?, Some(g)));
            }
        }
        let sizes: Vec<(u32, u32)> = head
            .sizes
            .iter()
            .map(|&(w, h)| (w as u32, h as u32))
            .collect();
        let g = up.upload(&sizes, |view, at_gpu| {
            let dst = view.as_raw_element_ptr().as_ptr();
            for (&(pw, ph), d) in head.sizes.iter().zip(at_gpu) {
                let row = d.row_bytes as usize;
                // SAFETY: the plane is within the region (checked above) and within the
                // staging memory (`slot_layout`'s sizes); the two don't overlap.
                unsafe {
                    let src = r.as_ptr().add(p);
                    let dst = dst.add(d.offset as usize);
                    if row == pw {
                        std::ptr::copy_nonoverlapping(src, dst, pw * ph);
                    } else {
                        for y in 0..ph {
                            std::ptr::copy_nonoverlapping(src.add(y * pw), dst.add(y * row), pw);
                        }
                    }
                }
                p += pw * ph;
            }
            true
        })?;
        let planes = head.empty_planes();
        Some((head.picture(r, planes, p, end)?, Some(g)))
    })
    .or_else(|| get_picture(ring, seq).map(|s| (s, None)))
}

impl Shared {
    /// Borrowed, to put in a ring.
    pub fn parts(&self) -> Parts<'_> {
        Parts {
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            frame_rate: self.frame_rate,
            timecode: self.timecode,
            timestamp: self.timestamp,
            planes: &self.planes,
            metadata: self.metadata.as_deref(),
            gpu: self.gpu,
            gpu_only: self.gpu_only,
        }
    }

    /// The video header a connection would have carried with it.
    pub fn header(&self) -> crate::media::VideoHeader {
        crate::media::VideoHeader {
            fourcc: fourcc_code(self.fourcc),
            width: self.width as u32,
            height: self.height as u32,
            frame_rate_n: self.frame_rate.0,
            frame_rate_d: self.frame_rate.1,
            aspect_ratio: self.width as f32 / self.height.max(1) as f32,
            field_id: 0,
            stream: 1,
            timecode: self.timecode,
            timestamp: self.timestamp,
            metadata: self.metadata.clone(),
        }
    }

    /// As a decoded picture.
    pub fn picture(self) -> Picture {
        Picture {
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            format: Format::New,
            quality: 100,
            short_dc: false,
            high_precision: false,
            planes: self.planes,
            precise: None,
            coefficients: None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Sound.

/// A block of sound in a slot: float, every channel the source has.
#[derive(Debug, Clone, PartialEq)]
pub struct Sound {
    pub sample_rate: u32,
    pub timecode: i64,
    pub timestamp: i64,
    pub channels: Vec<Vec<f32>>,
    pub metadata: Option<String>,
}

/// The bytes a block takes in a slot.
pub fn sound_bytes(channels: &[Vec<f32>], metadata: Option<&str>) -> usize {
    AUDIO_HEAD + channels.iter().map(|c| c.len() * 4).sum::<usize>() + metadata.map_or(0, str::len)
}

fn f32_bytes(c: &[f32]) -> &[u8] {
    // SAFETY: f32s as their bytes (the machine's own order: both ends are this machine).
    unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, c.len() * 4) }
}

/// Writes block `seq`; false if it doesn't fit or its channels differ in length.
pub fn put_sound(
    ring: &Ring,
    seq: u64,
    sample_rate: u32,
    timecode: i64,
    timestamp: i64,
    channels: &[Vec<f32>],
    metadata: Option<&str>,
) -> bool {
    let samples = channels.first().map_or(0, Vec::len);
    if channels.is_empty() || channels.len() > 256 || channels.iter().any(|c| c.len() != samples) {
        return false;
    }
    let meta = metadata.unwrap_or("");
    let mut h = Vec::with_capacity(AUDIO_HEAD);
    for v in [
        sample_rate,
        channels.len() as u32,
        samples as u32,
        meta.len() as u32,
    ] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    h.extend_from_slice(&timecode.to_le_bytes());
    h.extend_from_slice(&timestamp.to_le_bytes());
    let mut parts: Vec<&[u8]> = vec![&h];
    parts.extend(channels.iter().map(|c| f32_bytes(c)));
    parts.push(meta.as_bytes());
    ring.write(seq, &parts)
}

/// Block `seq` from its ring.
pub fn get_sound(ring: &Ring, seq: u64) -> Option<Sound> {
    ring.read(seq, |r, at, used| {
        if used < AUDIO_HEAD {
            return None;
        }
        let mut h = [0u8; AUDIO_HEAD];
        r.read(at, &mut h);
        let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        let i64_at = |i: usize| i64::from_le_bytes(h[i..i + 8].try_into().unwrap());
        let (sample_rate, n, samples, meta_len) = (
            u32_at(0),
            u32_at(4) as usize,
            u32_at(8) as usize,
            u32_at(12) as usize,
        );
        // (Checked sizes, and a rate a receiver can take: a ring is another program's
        // memory; deep bug run 2026-11-09.)
        let size = n
            .checked_mul(samples)
            .and_then(|x| x.checked_mul(4))
            .and_then(|x| x.checked_add(AUDIO_HEAD))
            .and_then(|x| x.checked_add(meta_len));
        if n == 0
            || n > 256
            || size.is_none_or(|s| s > used)
            || !crate::media::AUDIO_RATES.contains(&sample_rate)
        {
            return None;
        }
        let mut p = at + AUDIO_HEAD;
        let mut channels = Vec::with_capacity(n);
        for _ in 0..n {
            let mut c = vec![0f32; samples];
            // SAFETY: `samples` f32s' room.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(c.as_mut_ptr() as *mut u8, samples * 4) };
            r.read(p, bytes);
            p += samples * 4;
            channels.push(c);
        }
        let metadata = (meta_len > 0).then(|| {
            let mut m = vec![0u8; meta_len];
            r.read(p, &mut m);
            String::from_utf8_lossy(&m).into_owned()
        });
        Some(Sound {
            sample_rate,
            timecode: i64_at(16),
            timestamp: i64_at(24),
            channels,
            metadata,
        })
    })
}

// ---------------------------------------------------------------------------------------
// The offer, and where receivers are.

/// An output's rings as offered to a receiver on this machine (a control message).
pub fn offer_xml(
    generation: u64,
    control: &Control,
    pic: Option<&Ring>,
    sound: Option<&Ring>,
    gpu: &str,
) -> String {
    let mut x = format!(
        r#"<x_idn_machine gen="{generation}" control="{}""#,
        control.name()
    );
    if let Some(r) = pic {
        x += &format!(r#" ring="{}" slot_bytes="{}""#, r.name(), r.slot_bytes());
    }
    if let Some(r) = sound {
        x += &format!(
            r#" audio_ring="{}" audio_slot_bytes="{}""#,
            r.name(),
            r.slot_bytes()
        );
    }
    x + gpu + "/>"
}

/// Whether `ip` is this machine: loopback, or one of its own addresses.
pub fn is_this_machine(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    let own = |v4: std::net::Ipv4Addr| {
        v4.is_loopback() || idn_common::net::links().iter().any(|l| l.addr == v4)
    };
    match ip {
        std::net::IpAddr::V4(v4) => own(v4),
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(own),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(seq: u64) -> Shared {
        Shared {
            fourcc: Fourcc::Shq2,
            width: 64,
            height: 36,
            frame_rate: (30, 1),
            timecode: seq as i64 * 333_333,
            timestamp: 17_000_000_000_000_000 + seq as i64,
            planes: vec![
                Plane {
                    width: 64,
                    height: 36,
                    data: vec![seq as u8; 64 * 36],
                },
                Plane {
                    width: 32,
                    height: 36,
                    data: vec![128; 32 * 36],
                },
                Plane {
                    width: 32,
                    height: 36,
                    data: vec![129; 32 * 36],
                },
            ],
            metadata: Some("<x/>".into()),
            gpu: None,
            gpu_only: false,
        }
    }

    /// Planes the GPU copies out of imported memory itself (what a ring imported with
    /// `import_host` gives, S2): tried with anonymous memory, which every driver imports.
    #[cfg(all(feature = "gpu", target_os = "linux"))]
    #[test]
    fn planes_copied_by_the_gpu_out_of_imported_memory() {
        let Some(hand) = crate::gpu::open_device("test", true) else {
            eprintln!("no GPU here: not tested");
            return;
        };
        let n = 1 << 20;
        // SAFETY: an anonymous mapping, unmapped after the buffer and its copies are gone.
        let mem = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                n,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        } as *mut u8;
        assert!(!mem.is_null());
        let (w, h) = (64usize, 36usize);
        let planes = picture(3).planes;
        let mut at = 4096usize;
        let mut from = Vec::new();
        for p in &planes {
            // SAFETY: within the mapping.
            unsafe { std::ptr::copy_nonoverlapping(p.data.as_ptr(), mem.add(at), p.data.len()) };
            from.push(crate::gpu::PlaneAt {
                offset: at as u64,
                row_bytes: p.width as u32,
                width: p.width as u32,
                height: p.height as u32,
            });
            at += p.data.len();
        }
        let _ = (w, h);
        // SAFETY: `host` is dropped (and its copy waited for) before the mapping goes.
        match unsafe { crate::gpu::import_host(&hand.device, mem, n) } {
            Ok(Some(host)) => {
                let mut up = crate::gpu::Uploads::new(&hand);
                let g = up.copy_host(&host, &from).expect("copied");
                let back = g.read_planes(&hand).unwrap();
                for (b, p) in back.iter().zip(&planes) {
                    assert_eq!(b, &p.data);
                }
            }
            other => eprintln!("this GPU can't import memory ({other:?}): not tested"),
        }
        // SAFETY: as above.
        unsafe { libc::munmap(mem as *mut _, n) };
    }

    /// Written by a filler straight into the slot (a GPU's mapped read-back): read as a
    /// picture put from memory is.
    #[test]
    fn a_picture_filled_in_place_reads_as_one_put() {
        let ring = Ring::create(
            "idn-test-f",
            picture_bytes(&picture(1).planes, Some("<x/>")),
            PIC_SLOTS,
        )
        .unwrap();
        let want = picture(9);
        let mut sizes = want.clone();
        sizes.planes.iter_mut().for_each(|p| p.data.clear());
        let ok = put_picture_filled(&ring, 9, &sizes.parts(), |planes| {
            for (dst, src) in planes.iter_mut().zip(&want.planes) {
                dst.copy_from_slice(&src.data);
            }
            true
        });
        assert!(ok);
        assert_eq!(get_picture(&ring, 9), Some(want));
        // A filler that couldn't: not written.
        assert!(!put_picture_filled(&ring, 10, &sizes.parts(), |_| false));
    }

    /// Straight from the ring to the GPU (for an input that draws there), its rows where
    /// the slot layout has them; one in the source's GPU ring too isn't uploaded.
    #[cfg(feature = "gpu")]
    #[test]
    fn a_picture_goes_from_the_ring_to_the_gpu() {
        let Some(hand) = crate::gpu::open_device("test", true) else {
            eprintln!("no GPU here: not tested");
            return;
        };
        let ring = Ring::create(
            "idn-test-u",
            picture_bytes(&picture(1).planes, Some("<x/>")),
            PIC_SLOTS,
        )
        .unwrap();
        let mut p = picture(5);
        for (i, b) in p.planes[0].data.iter_mut().enumerate() {
            *b = (i * 7 % 251) as u8;
        }
        assert!(put_picture(&ring, 5, &p.parts()));
        let mut up = crate::gpu::Uploads::new(&hand);
        let (got, g) = get_picture_gpu(&ring, 5, &mut up, None).unwrap();
        let g = g.expect("uploaded");
        assert!(got.planes.iter().all(|p| p.data.is_empty()));
        assert_eq!((got.planes[1].width, got.planes[1].height), (32, 36));
        assert_eq!(got.metadata.as_deref(), Some("<x/>"));
        assert!(g.planes[0].row_bytes as usize > 64);
        let back = g.read_planes(&hand).unwrap();
        for (b, want) in back.iter().zip(&p.planes) {
            assert_eq!(b, &want.data);
        }
        // More, through the staging buffers again (each mapped again after its copy).
        for seq in 10..18u64 {
            let mut q = picture(seq);
            q.planes[0]
                .data
                .iter_mut()
                .for_each(|b| *b = (seq * 9) as u8);
            assert!(put_picture(&ring, seq, &q.parts()));
            let (_, g) = get_picture_gpu(&ring, seq, &mut up, None).unwrap();
            let back = g.unwrap().read_planes(&hand).unwrap();
            assert_eq!(back[0], q.planes[0].data, "picture {seq}");
            assert_eq!(back[2], q.planes[2].data, "picture {seq}");
        }
        // The ring imported as a GPU buffer, where this GPU can: the GPU copies the planes
        // out of it (S2), the same samples.
        let (ptr, len) = ring.memory();
        // SAFETY: `host` is dropped before `ring` (declared after it).
        // (AMD's driver refuses shared memory, a sealed memfd: an error, not tested.)
        let host = unsafe { crate::gpu::import_host(&hand.device, ptr, len) }.unwrap_or_else(|e| {
            eprintln!("shared memory not imported ({e})");
            None
        });
        match &host {
            Some(host) => {
                let mut q = picture(20);
                q.planes[0]
                    .data
                    .iter_mut()
                    .enumerate()
                    .for_each(|(i, b)| *b = (i % 199) as u8);
                assert!(put_picture(&ring, 20, &q.parts()));
                let (got, g) = get_picture_gpu(&ring, 20, &mut up, Some(host)).unwrap();
                assert!(got.planes.iter().all(|p| p.data.is_empty()));
                let back = g.unwrap().read_planes(&hand).unwrap();
                for (b, want) in back.iter().zip(&q.planes) {
                    assert_eq!(b, &want.data, "through the imported ring");
                }
            }
            None => eprintln!("this GPU can't read shared memory: not tested"),
        }
        drop(host);
        // Gone from its slot: none.
        assert!(get_picture_gpu(&ring, 5 + PIC_SLOTS as u64, &mut up, None).is_none());
        // In the source's GPU ring too: its samples here, nothing uploaded.
        p.gpu = Some((0, 1));
        assert!(put_picture(&ring, 6, &p.parts()));
        let (got, g) = get_picture_gpu(&ring, 6, &mut up, None).unwrap();
        assert!(g.is_none());
        assert_eq!(got.planes[0].data, p.planes[0].data);
    }

    #[test]
    fn a_picture_on_the_gpu_only_keeps_its_sizes_not_its_samples() {
        let control = Control::create().unwrap();
        let ring = Ring::create(
            "idn-test-g",
            picture_bytes(&picture(1).planes, Some("<x/>")),
            PIC_SLOTS,
        )
        .unwrap();
        let mut p = picture(7);
        p.gpu = Some((2, 41));
        p.gpu_only = true;
        assert!(put_picture(&ring, 7, &p.parts()));
        let got = get_picture(&ring, 7).unwrap();
        assert_eq!(got.gpu, Some((2, 41)));
        assert!(got.gpu_only);
        assert_eq!(got.planes.len(), 3);
        assert_eq!((got.planes[1].width, got.planes[1].height), (32, 36));
        assert!(got.planes.iter().all(|p| p.data.is_empty()));
        assert_eq!(got.metadata.as_deref(), Some("<x/>"));
        // In both: the samples too.
        p.gpu_only = false;
        assert!(put_picture(&ring, 8, &p.parts()));
        let got = get_picture(&ring, 8).unwrap();
        assert_eq!((got.gpu, got.gpu_only), (Some((2, 41)), false));
        assert_eq!(got.planes[0].data, picture(7).planes[0].data);
        // The GPU slots' stamps are in the control page.
        control.gpu_stamp(2).unwrap().store(82, Ordering::Release);
        assert_eq!(control.gpu_stamp(2).unwrap().load(Ordering::Acquire), 82);
        assert!(control.gpu_stamp(GPU_STAMPS).is_none());
    }

    #[test]
    fn pictures_go_round_the_ring_and_old_ones_are_gone() {
        let need = picture_bytes(&picture(0).planes, Some("<x/>"));
        let w = Ring::create("idn-test-p", need, PIC_SLOTS).unwrap();
        let r = Ring::open(w.name(), w.slot_bytes(), PIC_SLOTS).unwrap();
        for seq in 1..=6u64 {
            assert!(put_picture(&w, seq, &picture(seq).parts()));
            assert_eq!(get_picture(&r, seq), Some(picture(seq)));
        }
        // Six written into four slots: 1 and 2 are written over.
        assert_eq!(get_picture(&r, 2), None);
        assert_eq!(get_picture(&r, 3), Some(picture(3)));
        // Too big for its slot: refused, nothing written.
        let mut big = picture(7);
        big.planes[0].data = vec![0; w.slot_bytes()];
        big.planes[0].height = w.slot_bytes() / 64;
        assert!(!put_picture(&w, 7, &big.parts()));
        assert_eq!(get_picture(&r, 7), None);
        assert!(is_this_machine("127.0.0.1".parse().unwrap()));
        assert!(!is_this_machine("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn sound_goes_round_its_ring_and_the_bell_wakes_a_watcher() {
        let ch = vec![vec![0.25f32; 96], vec![-0.5; 96]];
        let w = Ring::create("idn-test-a", sound_bytes(&ch, Some("<t/>")), AUDIO_SLOTS).unwrap();
        let r = Ring::open(w.name(), w.slot_bytes(), AUDIO_SLOTS).unwrap();
        let c = Control::create().unwrap();
        let watcher = Control::open(c.name()).unwrap();
        let t = std::thread::spawn(move || watcher.wait((0, 0), Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(put_sound(&w, 1, 48000, 10, 20, &ch, Some("<t/>")));
        c.publish(true, 1);
        assert_eq!(t.join().unwrap(), (0, 1), "woken with the new block");
        let got = get_sound(&r, 1).unwrap();
        assert_eq!(got.channels, ch);
        assert_eq!(
            (got.sample_rate, got.timecode, got.timestamp),
            (48000, 10, 20)
        );
        assert_eq!(got.metadata.as_deref(), Some("<t/>"));
        assert!(!put_sound(
            &w,
            2,
            48000,
            0,
            0,
            &[vec![0.0; 3], vec![0.0; 4]],
            None
        ));
    }
}
