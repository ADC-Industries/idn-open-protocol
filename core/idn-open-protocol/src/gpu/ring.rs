//! The output's ring of exported slots, and an input's imported ones.

use idn_common::sync::LockOk as _;
use super::*;

// The output's ring.

/// One exported slot: the buffer and its memory as exported.
pub(super) struct Slot {
    pub(super) buffer: wgpu::Buffer,
    pub(super) exported: Exported,
}

/// An output's GPU ring ([`GPU_SLOTS`] slots of `slot_bytes`), and the shader that copies
/// a picture into a slot.
pub struct GpuRing {
    pub(super) hand: GpuHandIn,
    pub(super) id: String,
    pub(super) slots: Vec<Slot>,
    pub(super) slot_bytes: u64,
    pub(super) repack: Repack,
}

impl GpuRing {
    /// A ring on `hand`'s GPU whose slots hold `need` bytes (laid out by [`slot_layout`]);
    /// `Err` where this GPU (or this system) can't share memory.
    pub fn create(hand: &GpuHandIn, need: u64) -> io::Result<GpuRing> {
        let slot_bytes = need.next_multiple_of(ROW_ALIGN);
        if slot_bytes > MAX_SLOT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gpu/1: a slot that large",
            ));
        }
        let id = device_id(&hand.device).ok_or_else(|| unsupported("not a GPU that shares"))?;
        let mut slots = Vec::with_capacity(GPU_SLOTS);
        for _ in 0..GPU_SLOTS {
            let (buffer, exported) = export(&hand.device, slot_bytes)?;
            slots.push(Slot { buffer, exported });
        }
        Ok(GpuRing {
            repack: Repack::new(&hand.device),
            hand: hand.clone(),
            id,
            slots,
            slot_bytes,
        })
    }

    /// The bytes a slot holds.
    pub fn slot_bytes(&self) -> u64 {
        self.slot_bytes
    }

    /// The offer's attributes (§E.27.1), added to `x_idn_machine`.
    pub fn offer_attrs(&self) -> String {
        let names: Vec<&str> = self
            .slots
            .iter()
            .map(|s| s.exported.name.as_str())
            .collect();
        let s0 = &self.slots[0].exported;
        // (What keeps each name: dropped with the ring.)
        let _ = &s0.keep;
        format!(
            r#" gpu="{}" gpu_slots="{}" gpu_bytes="{}" gpu_memory="{}" gpu_alloc="{}""#,
            self.id,
            names.join(","),
            self.slot_bytes,
            s0.memory_type,
            s0.alloc_size
        )
    }

    /// Copies the planes `from` of `src` (a buffer of this ring's device, `STORAGE`) into
    /// slot `slot`, laid out as `to`, and waits for the GPU to be done with it.
    pub fn write(
        &self,
        slot: usize,
        src: &wgpu::Buffer,
        from: &[PlaneAt],
        to: &[PlaneAt],
    ) -> io::Result<()> {
        let dst = &self.slots[slot % self.slots.len()].buffer;
        let sub = self.repack.run(&self.hand, src, dst, from, to)?;
        wait(&self.hand.device, Some(sub))
    }
}

/// A receiver's view of an output's GPU ring: the imported slots and buffers to copy
/// pictures out into.
pub struct GpuSlots {
    pub(super) hand: GpuHandIn,
    pub(super) slots: Vec<wgpu::Buffer>,
    pub(super) bytes: u64,
    /// Buffers handed out earlier, taken back when no picture holds them any more.
    pub(super) pool: std::sync::Mutex<Buffers>,
}

impl GpuSlots {
    /// The ring an offer (an `x_idn_machine` element's attributes, `offer`) has, on `hand`'s
    /// GPU; `Ok(None)` when
    /// the offer has none or it is on another GPU.
    pub fn open<'a>(
        hand: &GpuHandIn,
        offer: impl Fn(&str) -> Option<&'a str>,
    ) -> io::Result<Option<GpuSlots>> {
        let (Some(id), Some(names)) = (offer("gpu"), offer("gpu_slots")) else {
            return Ok(None);
        };
        if device_id(&hand.device).as_deref() != Some(id) {
            return Ok(None);
        }
        let num = |k: &str| offer(k).and_then(|v| v.parse::<u64>().ok());
        let (Some(bytes), Some(memory_type), Some(alloc)) =
            (num("gpu_bytes"), num("gpu_memory"), num("gpu_alloc"))
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gpu/1: an offer",
            ));
        };
        let names: Vec<&str> = names.split(',').collect();
        // (Whole rows' worth: copies on the GPU go in words, and an odd size failed wgpu's
        // checks, which panic.)
        if names.len() != GPU_SLOTS
            || !(ROW_ALIGN..=MAX_SLOT_BYTES).contains(&bytes)
            || !bytes.is_multiple_of(ROW_ALIGN)
            || (alloc != 0 && alloc < bytes)
            || alloc > MAX_SLOT_BYTES * 2
            || memory_type >= 32
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gpu/1: an offer",
            ));
        }
        let mut slots = Vec::with_capacity(GPU_SLOTS);
        for n in names {
            slots.push(import(&hand.device, n, bytes, memory_type as u32, alloc)?);
        }
        Ok(Some(GpuSlots {
            hand: hand.clone(),
            slots,
            bytes,
            // (Storage: a receiver's compositor copies planes out with `PlaneCopy`.)
            pool: std::sync::Mutex::new(Buffers::new(
                "gpu/1 picture",
                wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::STORAGE,
                4,
            )),
        }))
    }

    /// A buffer for a copy: one from the pool no picture holds any more, or a new one.
    pub(super) fn buffer(&self) -> std::sync::Arc<wgpu::Buffer> {
        self.pool.lock_ok().get(&self.hand.device, self.bytes)
    }

    /// Slot `slot` copied into a buffer of the receiver's own, waited for (the caller then
    /// checks the slot's stamp again, as for the CPU ring).
    pub fn copy_out(&self, slot: usize, planes: Vec<PlaneAt>) -> io::Result<GpuPicture> {
        let src = &self.slots[slot % self.slots.len()];
        let used = planes
            .iter()
            // (The sender's numbers: saturating, so a huge offset can't wrap past the check.)
            .map(|p| {
                p.offset
                    .saturating_add(p.row_bytes as u64 * p.height as u64)
            })
            .max()
            .unwrap_or(0);
        if used.next_multiple_of(4) > self.bytes {
            return Err(io::Error::other("gpu/1: a picture larger than its slot"));
        }
        let dst = self.buffer();
        let mut enc = self.hand.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, 0, &dst, 0, used.next_multiple_of(4));
        let sub = self.hand.queue.submit([enc.finish()]);
        wait(&self.hand.device, Some(sub))?;
        Ok(GpuPicture {
            buffer: dst,
            planes,
        })
    }
}

/// Buffers pictures from memory are written into on their way to the GPU (a picture from
/// a source's shared-memory ring, §E.26, for an input given a GPU): its planes copied from
/// the ring straight into the GPU's staging memory, laid out as [`slot_layout`] lays
/// them, with no copy in this process's memory first.
pub(crate) struct Uploads {
    pub(super) hand: GpuHandIn,
    pub(super) pool: Buffers,
    pub(super) staging: Staging,
    /// Copying from a ring imported as a GPU buffer ([`import_host`]).
    pub(super) copy: Option<Repack>,
}

impl Uploads {
    pub(crate) fn new(hand: &GpuHandIn) -> Uploads {
        Uploads {
            hand: hand.clone(),
            pool: Buffers::new(
                "picture from memory",
                wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::STORAGE,
                4,
            ),
            staging: Staging::new("pictures from memory up"),
            copy: None,
        }
    }

    /// A picture whose planes are `from` in `host` (a ring imported with [`import_host`]),
    /// copied by the GPU into a buffer laid out as a slot, waited for: no CPU copy.
    pub(crate) fn copy_host(
        &mut self,
        host: &wgpu::Buffer,
        from: &[PlaneAt],
    ) -> Option<GpuPicture> {
        let sizes: Vec<(u32, u32)> = from.iter().map(|p| (p.width, p.height)).collect();
        let (planes, bytes) = slot_layout(&sizes);
        let buffer = self.pool.get(&self.hand.device, bytes);
        let copy = self
            .copy
            .get_or_insert_with(|| Repack::new(&self.hand.device));
        let sub = copy.run(&self.hand, host, &buffer, from, &planes).ok()?;
        wait(&self.hand.device, Some(sub)).ok()?;
        Some(GpuPicture { buffer, planes })
    }

    /// A picture of planes `sizes`, `write` given the staging memory and where each plane
    /// goes in it (it says whether it wrote them all); the copy runs on the GPU before
    /// anything submitted later, so it isn't waited for.
    pub(crate) fn upload(
        &mut self,
        sizes: &[(u32, u32)],
        write: impl FnOnce(&mut wgpu::WriteOnly<'_, [u8]>, &[PlaneAt]) -> bool,
    ) -> Option<GpuPicture> {
        let (planes, bytes) = slot_layout(sizes);
        let buffer = self.pool.get(&self.hand.device, bytes);
        let ok = self
            .staging
            .upload(
                &self.hand,
                bytes,
                |view| write(view, &planes),
                |enc, up| enc.copy_buffer_to_buffer(up, 0, &buffer, 0, bytes),
            )
            .ok()?;
        ok.then_some(GpuPicture { buffer, planes })
    }
}
