//! Pictures into the compositor's regions: each written once while it is shown (held), lifted for an HDR program, the program's own picture put up.

use super::*;

impl Gpu {
    /// A picture's planes' offsets within its region, and the bytes it takes.
    pub(super) fn layout_of(pic: &Frame) -> ([usize; 4], usize) {
        // (A picture as coefficients has no planes yet: its sizes from its dimensions.)
        let (w, h) = (pic.width, pic.height);
        // (Nor one kept on the GPU.)
        let coefs = pic.deferred();
        let len = |v: &Vec<u8>, n: usize| if coefs { n } else { v.len() };
        let (ly, lc) = (
            round4(len(&pic.y, w * h)),
            round4(len(&pic.cb, Frame::chroma_width(w) * h)),
        );
        let la = pic.a.as_ref().map_or(0, |a| round4(len(a, w * h)));
        ([0, ly, ly + lc, ly + 2 * lc], ly + 2 * lc + la)
    }

    /// Writes the scene's pictures for upload (each once, however many layers show it;
    /// those already up stay); each picture's planes' offsets, in the scene's order.
    pub(super) fn upload(&mut self, pics: &[&Arc<Frame>]) -> Result<Vec<[u32; 4]>, String> {
        // Coefficients past the GPU's buffers: refused before anything here changes (bug
        // run M7-1: refused at the submission, a staging buffer was left unmapped and the
        // next frame failed the GPU for good). (All of this frame's counted: at most more
        // than will go up.)
        let words: usize = pics
            .iter()
            .filter_map(|p| p.coefs.as_ref())
            .map(|c| idn_open_protocol::shq::gpu_idct::words_len(c))
            .sum();
        let limits = self.device.limits();
        let most = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        if (words as u64) * 4 > most {
            return Err(format!(
                "{}{} MB of coefficients in one frame",
                crate::TOO_LARGE,
                (words * 4) >> 20
            ));
        }
        // A region big enough for the largest picture; growing it starts afresh.
        let need = pics.iter().map(|p| Gpu::layout_of(p).1).max().unwrap_or(4);
        // An HDR pass that doesn't fit while the scene's pictures are smaller now (the 4K
        // camera gone): after 300 frames (5–10 s) the region shrinks to them, so the 16-bit
        // pictures may fit again (else the HDR program stayed on the CPU for good).
        let unfit = self.hdr.as_ref().is_some_and(|h| !h.fits);
        self.small_for = if unfit && need < self.region {
            self.small_for + 1
        } else {
            0
        };
        if self.small_for >= 300 {
            self.small_for = 0;
            self.region = need;
            (self.pics, self.staging) = Gpu::pictures(&self.device, need * LAYERS, self.unified);
            self.held = [vec![None; LAYERS], vec![None; LAYERS]];
        } else if need > self.region {
            self.region = need;
            (self.pics, self.staging) = Gpu::pictures(&self.device, need * LAYERS, self.unified);
            self.held = [vec![None; LAYERS], vec![None; LAYERS]];
        }
        // HDR: the 16-bit pictures beside them (made again empty: every picture lifted
        // again), and this frame's lifts.
        if let Some(h) = self.hdr.as_mut() {
            if h.ensure_pics(&self.device, (self.region * LAYERS) as u64, self.unified) {
                self.held = [vec![None; LAYERS], vec![None; LAYERS]];
            }
            h.jobs.clear();
        }
        let hdr = self.hdr.as_ref().filter(|h| h.fits).map(|h| h.transfer);
        let raw = self.raw_lift.take();
        let mut lifts: Vec<crate::gpu_hdr::LiftJob> = Vec::new();
        let mut direct: Vec<(crate::gpu_hdr::LiftJob, Arc<crate::frame::Hdr16>)> = Vec::new();
        // HDR: each new picture lifted into the HDR space, or its own 16-bit planes put
        // there as they are (an HDR source in the program's transfer, without alpha).
        let mut lift = |lifts: &mut Vec<crate::gpu_hdr::LiftJob>,
                        base: usize,
                        offs: [usize; 4],
                        p: &Arc<Frame>| {
            let Some(t) = hdr else { return };
            let job = crate::gpu_hdr::LiftJob {
                base: base as u32,
                offs: offs.map(|o| (base + o) as u32),
                w: p.width as u32,
                h: p.height as u32,
                alpha: p.a.is_some(),
                raw: raw.as_ref().is_some_and(|r| Arc::ptr_eq(r, p)),
            };
            match crate::frame::hdr16_of(p) {
                // Its 8-bit samples already in the program's space: copied up.
                // (With alpha too: its 16-bit planes go in without it, the raw copy has it.)
                Some(h) if h.transfer == t && (h.y.is_empty() || p.a.is_some()) => {
                    lifts.push(crate::gpu_hdr::LiftJob { raw: true, ..job })
                }
                Some(h) if h.transfer == t && p.a.is_none() && !job.raw => direct.push((job, h)),
                _ => lifts.push(job),
            }
        };
        self.copies.clear();
        self.coefs.clear();
        self.recon_jobs.clear();
        self.gpu_jobs.clear();
        self.clears.clear();
        let set = if self.unified { self.next } else { 0 };
        let held = &mut self.held[set];
        let held_raw = &mut self.held_raw[set];
        // HDR: a region whose picture is wanted the other way (the pattern lifted as a
        // picture, or a picture copied up raw as the pattern) isn't held.
        let as_raw =
            |p: &Arc<Frame>| hdr.is_some() && raw.as_ref().is_some_and(|r| Arc::ptr_eq(r, p));
        for (h, was) in held.iter_mut().zip(held_raw.iter()) {
            if h.as_ref().is_some_and(|h| as_raw(h) != *was) {
                *h = None;
            }
        }
        // Regions holding a picture this frame needs are kept; the others are free.
        let mut keep = [false; LAYERS];
        for p in pics {
            if let Some(r) = held
                .iter()
                .position(|h| h.as_ref().is_some_and(|h| Arc::ptr_eq(h, p)))
            {
                keep[r] = true;
            }
        }
        let mut out = Vec::with_capacity(pics.len());
        for p in pics {
            let r = match held
                .iter()
                .position(|h| h.as_ref().is_some_and(|h| Arc::ptr_eq(h, p)))
            {
                Some(r) => r,
                None => {
                    let r = (0..LAYERS)
                        .find(|&r| !keep[r])
                        .expect("a region for each layer");
                    keep[r] = true;
                    let (offs, size) = Gpu::layout_of(p);
                    let base = r * self.region;
                    // Kept on the GPU (§E.27): copied into its region there, Y as it is,
                    // Cb and Cr each row (4:2:2) or each row twice (4:2:0). Rows not whole
                    // words can't be copied so: read back instead (below).
                    let (w, cw) = (p.width, Frame::chroma_width(p.width));
                    let samples;
                    let p = match &p.gpu {
                        Some(g) if w.is_multiple_of(4) && cw.is_multiple_of(4) => {
                            use idn_open_protocol::gpu::{PlaneAt, PlaneJob};
                            let at = &g.picture.planes;
                            let job = |k: usize, width: usize| PlaneJob {
                                from: at[k],
                                to: PlaneAt {
                                    offset: (base + offs[k]) as u64,
                                    row_bytes: width as u32,
                                    width: width as u32,
                                    height: p.height as u32,
                                },
                                double_rows: (at[k].height as usize) < p.height,
                            };
                            self.gpu_jobs.push((
                                g.picture.buffer.clone(),
                                [job(0, w), job(1, cw), job(2, cw)],
                            ));
                            lift(&mut lifts, base, offs, p);
                            held[r] = Some((*p).clone());
                            out.push(offs.map(|o| (base + o) as u32));
                            continue;
                        }
                        Some(_) => {
                            samples = Arc::new(p.pixels());
                            // (Its HDR marker goes with it: else lifted as SDR.)
                            if let Some(h) = crate::frame::hdr16_of(p) {
                                crate::frame::attach_hdr16(&samples, h);
                            }
                            &samples
                        }
                        None => p,
                    };
                    if let Some(c) = &p.coefs {
                        // Reconstructed into its region on the GPU (cleared first).
                        // (Packed at the frame's submission, straight into staging.)
                        let dst = offs.map(|o| (base + o) as u32);
                        // 4:2:0 (NDI's pictures above 1080p): its chroma rows each
                        // serve two of the region's 4:2:2 rows.
                        let doubled = (c.fourcc == idn_open_protocol::shq::Fourcc::Shq0)
                            .then_some(p.height as u32);
                        self.coefs.add(c.clone(), dst, doubled);
                        self.clears.push((base as u64, round4(size) as u64));
                        lift(&mut lifts, base, offs, p);
                        held[r] = Some((*p).clone());
                        out.push(offs.map(|o| (base + o) as u32));
                        continue;
                    }
                    let st = &self.staging[self.next];
                    wait_mapped(&self.device, st)?;
                    {
                        let mut view = st
                            .buffer
                            .slice(base as u64..(base + size) as u64)
                            .get_mapped_range_mut()
                            .map_err(|e| format!("GPU: {e}"))?;
                        for (k, data) in [Some(&p.y), Some(&p.cb), Some(&p.cr), p.a.as_ref()]
                            .into_iter()
                            .enumerate()
                        {
                            if let Some(data) = data {
                                view.slice(offs[k]..offs[k] + data.len())
                                    .copy_from_slice(data);
                            }
                        }
                    }
                    self.copies.push((base as u64, size as u64));
                    lift(&mut lifts, base, offs, p);
                    held[r] = Some((*p).clone());
                    r
                }
            };
            out.push(Gpu::layout_of(p).0.map(|o| (r * self.region + o) as u32));
        }
        drop(lift);
        for (h, was) in held.iter().zip(held_raw.iter_mut()) {
            *was = h.as_ref().is_some_and(as_raw);
        }
        if let Some(h) = self.hdr.as_mut() {
            h.jobs = lifts;
            h.direct = direct;
        }
        Ok(out)
    }

    /// `f` (a program passed through as it came: planes only, no alpha) uploaded, for its
    /// output to share GPU to GPU (§E.27): the buffer and its Y, Cb and Cr planes, each row
    /// as the picture has it. `None` where its rows aren't whole words (copies to the GPU
    /// go in words), or it has alpha or coefficients.
    pub fn upload_program(
        &mut self,
        f: &Frame,
    ) -> Option<(&wgpu::Buffer, [idn_open_protocol::gpu::PlaneAt; 3])> {
        use idn_open_protocol::gpu::PlaneAt;
        let (w, h) = (f.width, f.height);
        let cw = Frame::chroma_width(w);
        if f.a.is_some() || f.deferred() || !w.is_multiple_of(4) || !cw.is_multiple_of(4) {
            return None;
        }
        if f.y.len() != w * h || f.cb.len() != cw * h || f.cr.len() != cw * h {
            return None;
        }
        let size = ((w + 2 * cw) * h) as u64;
        if self.passed.as_ref().is_none_or(|b| b.size() != size) {
            self.passed = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("program passed through"),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let b = self.passed.as_ref().expect("made above");
        let (c0, c1) = ((w * h) as u64, ((w + cw) * h) as u64);
        // (Written before the output's copy out of it, which is the next submission.)
        self.queue.write_buffer(b, 0, &f.y);
        self.queue.write_buffer(b, c0, &f.cb);
        self.queue.write_buffer(b, c1, &f.cr);
        let plane = |offset: u64, width: usize| PlaneAt {
            offset,
            row_bytes: width as u32,
            width: width as u32,
            height: h as u32,
        };
        Some((b, [plane(0, w), plane(c0, cw), plane(c1, cw)]))
    }

    /// Whether a picture fits the GPU's regions as it is (else give it fitted to the
    /// program: a picture over 2160p with alpha, or 8K, was refused).
    pub fn fits(pic: &Frame) -> bool {
        Gpu::layout_of(pic).1 <= REGION_BYTES
    }
}
