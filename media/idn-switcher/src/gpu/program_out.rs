//! The program out of the compositor: read back, at a tile's size, or left on the GPU for an encoder on the same device.

use super::*;

impl Gpu {
    /// `Err` with what the GPU refused since the last check.
    /// This compositor's GPU, for an output to share pictures from it (§E.27).
    pub fn hand_in(&self) -> idn_open_protocol::gpu::GpuHandIn {
        idn_open_protocol::gpu::GpuHandIn {
            device: self.device.clone(),
            queue: self.queue.clone(),
        }
    }

    /// The frame [`compose_next`](Self::compose_next) or [`take_last`](Self::take_last) just
    /// read out, where it still is on the GPU: the buffer and its Y, Cb and Cr planes. Until
    /// the next frame starts: on one memory (an integrated GPU) the buffer the shader wrote,
    /// else its copy while [`share_program`](Self::share_program) is on.
    pub fn program_on_gpu(&self) -> Option<(&wgpu::Buffer, [idn_open_protocol::gpu::PlaneAt; 3])> {
        use idn_open_protocol::gpu::PlaneAt;
        let i = self.last_out?;
        // On one memory, still mapped for `read_into`: not for the GPU (and nothing takes
        // it there: `ring_only` isn't so while it is shared).
        if self.unified && self.mapped.get().is_some() {
            return None;
        }
        let (w, h) = self.size;
        let (ys, cs) = Gpu::strides(w);
        let cw = Frame::chroma_width(w) as u32;
        let plane = |offset: usize, row: usize, width: u32| PlaneAt {
            offset: offset as u64,
            row_bytes: (row * 4) as u32,
            width,
            height: h as u32,
        };
        let buffer = if self.unified {
            &self.readback[i].buffer
        } else {
            &self.share.as_ref()?[i]
        };
        Some((
            buffer,
            [
                plane(0, ys, w as u32),
                plane(ys * h * 4, cs, cw),
                plane((ys + cs) * h * 4, cs, cw),
            ],
        ))
    }

    /// The program as composed, shrunk on the GPU to `w` × `h` and read back: the
    /// multiview's program tile, a small read instead of the whole picture's (at 2160p the
    /// multiview had the program read back every frame, 6-8 ms of it, and a frame of over
    /// 60 ms when it stopped, lab 381). `None` where it can't (the caller reads it all).
    pub fn program_small(&mut self, w: usize, h: usize) -> Option<Frame> {
        let (small, hand) = self.program_small_on_gpu(w, h)?;
        read_small(&small, &hand, w, h)
    }

    /// As [`program_small`](Self::program_small), the shrink submitted only: the small
    /// picture there and the device, for another thread to read back with [`read_small`]
    /// (waited for on the frame clock, the read lengthened a 2160p frame to about 10 ms).
    pub fn program_small_on_gpu(
        &mut self,
        w: usize,
        h: usize,
    ) -> Option<(
        idn_open_protocol::gpu::GpuPicture,
        idn_open_protocol::gpu::GpuHandIn,
    )> {
        use idn_open_protocol::gpu::{GpuPicture, PlaneShrink};
        let (buffer, planes) = match self.program_on_gpu() {
            Some((b, p)) => (b.clone(), p),
            // On a GPU of its own memory, not shared: what the compositor wrote last.
            None if !self.unified && self.last_out.is_some() => {
                let (pw, ph) = self.size;
                let (ys, cs) = Gpu::strides(pw);
                let cw = Frame::chroma_width(pw) as u32;
                let plane =
                    |offset: usize, row: usize, width: u32| idn_open_protocol::gpu::PlaneAt {
                        offset: offset as u64,
                        row_bytes: (row * 4) as u32,
                        width,
                        height: ph as u32,
                    };
                (
                    self.out.clone(),
                    [
                        plane(0, ys, pw as u32),
                        plane(ys * ph * 4, cs, cw),
                        plane((ys + cs) * ph * 4, cs, cw),
                    ],
                )
            }
            None => return None,
        };
        let hand = self.hand_in();
        let shrink = self
            .shrink
            .get_or_insert_with(|| PlaneShrink::new(&self.device));
        let cw = Frame::chroma_width(w);
        let sizes = [
            (w as u32, h as u32),
            (cw as u32, h as u32),
            (cw as u32, h as u32),
        ];
        let pic = GpuPicture {
            buffer: Arc::new(buffer),
            planes: planes.to_vec(),
        };
        let small = shrink.run(&hand, &pic, &sizes)?;
        Some((small, hand))
    }

    /// Whether the frame just read out waits for [`read_into`](Self::read_into)
    /// ([`ring_only`](Self::ring_only)).
    pub fn read_pending(&self) -> bool {
        self.mapped.get().is_some()
    }

    /// The frame just read out, still mapped, copied into `planes` (Y, Cb, Cr, rows
    /// packed: an output's shared memory), then unmapped; false if there is none or the
    /// planes aren't its size.
    pub fn read_into(&self, planes: &mut [&mut [u8]]) -> bool {
        let Some(i) = self.mapped.take() else {
            return false;
        };
        let (w, h) = self.size;
        let (ys, cs) = Gpu::strides(w);
        let cw = Frame::chroma_width(w);
        let rb = &self.readback[i];
        let ok = planes.len() == 3
            && planes[0].len() == w * h
            && planes[1].len() == cw * h
            && planes[2].len() == cw * h;
        if ok && let Ok(data) = rb.buffer.slice(..).get_mapped_range() {
            use idn_open_protocol::mem::copy_uncached;
            let (yb, cb) = (ys * 4, cs * 4);
            let starts = [0, ys * h * 4, (ys + cs) * h * 4];
            for (k, width, stride) in [(0, w, yb), (1, cw, cb), (2, cw, cb)] {
                let plane = &mut *planes[k];
                if stride == width {
                    copy_uncached(plane, &data[starts[k]..starts[k] + width * h]);
                } else {
                    for r in 0..h {
                        let at = starts[k] + r * stride;
                        copy_uncached(
                            &mut plane[r * width..(r + 1) * width],
                            &data[at..at + width],
                        );
                    }
                }
            }
        }
        rb.buffer.unmap();
        rb.ready.store(false, Ordering::Release);
        ok
    }

    /// A frame left for [`read_into`](Self::read_into) and not taken: unmapped (its turn
    /// on the GPU comes again).
    pub fn release_read(&self) {
        if let Some(i) = self.mapped.take() {
            self.readback[i].buffer.unmap();
            self.readback[i].ready.store(false, Ordering::Release);
        }
    }
}
