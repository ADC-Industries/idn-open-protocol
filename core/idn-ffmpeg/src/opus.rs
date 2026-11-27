//! Opus (RFC 6716) through FFmpeg: libopus to encode, FFmpeg's own decoder. For sound
//! where the bandwidth is short: WebRTC viewers (E.19.4) and a site link that is tight
//! (X.12.8): stereo 48 kHz in 20 ms packets, about 128 kbit/s instead of 2.3 Mbit/s of
//! 24-bit samples.

use crate::ffi::{Packet, check, oom};
use ffmpeg_sys_next as ff;
use std::io;
use std::os::raw::c_int;
use std::ptr;

/// Opus through FFmpeg's libopus: 48 kHz stereo, 20 ms frames, 128 kbit/s.
pub(crate) struct OpusEncoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: Packet,
    fifo: Vec<f32>,
    pts: i64,
}

// SAFETY: the context, frame and packet are its own, used through &mut self only.
unsafe impl Send for OpusEncoder {}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: allocated in `open_kbps` (the frees take null), freed once.
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

impl OpusEncoder {
    pub(crate) fn open() -> io::Result<Self> {
        Self::open_kbps(128)
    }

    /// The same at `kbps` kbit/s (listening on phones: 64, so a crowd fits the Wi-Fi).
    pub(crate) fn open_kbps(kbps: u32) -> io::Result<Self> {
        // SAFETY: a NUL-terminated name; the result is checked for null.
        let codec = unsafe { ff::avcodec_find_encoder_by_name(c"libopus".as_ptr()) };
        if codec.is_null() {
            return Err(io::Error::other(
                "this FFmpeg has no Opus encoder (libopus)",
            ));
        }
        // Owned at once, so a failure below frees both (built at
        // the end, they leaked on every refused option).
        // SAFETY: `codec` was found above (not null).
        let ctx = oom(unsafe { ff::avcodec_alloc_context3(codec) })?;
        let mut enc = OpusEncoder {
            ctx,
            frame: ptr::null_mut(),
            pkt: Packet::checked()?,
            fifo: Vec::new(),
            pts: 0,
        };
        // SAFETY: a plain allocation, checked by `oom`.
        enc.frame = oom(unsafe { ff::av_frame_alloc() })?;
        let frame = enc.frame;
        // SAFETY: `ctx` just allocated and not open; `frame` ours, its buffers allocated for the
        // format and 960 samples set.
        unsafe {
            let c = &mut *ctx;
            c.sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLT;
            c.sample_rate = 48_000;
            ff::av_channel_layout_default(&mut c.ch_layout, 2);
            c.bit_rate = i64::from(kbps.clamp(16, 256)) * 1000;
            c.time_base = ff::AVRational {
                num: 1,
                den: 48_000,
            };
            // Constrained VBR: near the rate asked (plain VBR ran a third over it), so a
            // crowd of listeners fits the Wi-Fi it was planned for.
            ff::av_opt_set(c.priv_data, c"vbr".as_ptr(), c"constrained".as_ptr(), 0);
            check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                "opening Opus",
            )?;
            let f = &mut *frame;
            f.format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int;
            f.nb_samples = 960;
            f.sample_rate = 48_000;
            ff::av_channel_layout_default(&mut f.ch_layout, 2);
            check(ff::av_frame_get_buffer(frame, 0), "audio frame")?;
        }
        Ok(enc)
    }

    /// Stereo planar samples in; Opus packets out (one per 20 ms).
    pub(crate) fn push(&mut self, ch: &[Vec<f32>]) -> io::Result<Vec<Vec<u8>>> {
        let [l, r] = ch else { return Ok(Vec::new()) };
        for (a, b) in l.iter().zip(r) {
            self.fifo.push(*a);
            self.fifo.push(*b);
        }
        let mut out = Vec::new();
        while self.fifo.len() >= 960 * 2 {
            // SAFETY: our frame, made writable, holds 960 interleaved stereo floats; the fifo holds
            // at least that.
            unsafe {
                check(ff::av_frame_make_writable(self.frame), "audio frame")?;
                ptr::copy_nonoverlapping(
                    self.fifo.as_ptr(),
                    (*self.frame).data[0] as *mut f32,
                    960 * 2,
                );
                (*self.frame).pts = self.pts;
            }
            self.fifo.drain(..960 * 2);
            self.pts += 960;
            check(
                // SAFETY: our open encoder and our frame.
                unsafe { ff::avcodec_send_frame(self.ctx, self.frame) },
                "Opus",
            )?;
            loop {
                // SAFETY: our open encoder and our packet.
                let r = unsafe { ff::avcodec_receive_packet(self.ctx, self.pkt.0) };
                if r == crate::ffi::EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "Opus")?;
                // SAFETY: the packet just received, copied out before it is unreferenced.
                unsafe {
                    let data = crate::ffi::packet_bytes(self.pkt.0);
                    ff::av_packet_unref(self.pkt.0);
                    if !data.is_empty() {
                        out.push(data);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// An Opus stream's identification header (RFC 7845 §5.1) for a stereo 48 kHz stream as
/// [`OpusEncoder`] makes it: what a decoder needs to start.
pub(crate) fn stereo_head() -> Vec<u8> {
    let mut h = b"OpusHead".to_vec();
    h.push(1); // version
    h.push(2); // channels
    h.extend_from_slice(&312u16.to_le_bytes()); // pre-skip (libopus's at 48 kHz)
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes()); // output gain
    h.push(0); // channel mapping family
    h
}

/// FFmpeg's Opus decoder for a stream from [`OpusEncoder`]: packets in, planar float
/// samples (stereo, 48 kHz) out.
pub(crate) fn decoder() -> io::Result<crate::ffi::AudioDec> {
    crate::ffi::AudioDec::open(ff::AVCodecID::AV_CODEC_ID_OPUS, &stereo_head())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone through the encoder and the decoder comes out about as loud and at its
    /// pitch, in 20 ms pieces: at the viewer's 128 kbit/s and listening's 64 (and 32).
    #[test]
    fn a_tone_crosses() {
        for kbps in [128, 64, 32] {
            let mut enc = OpusEncoder::open_kbps(kbps).unwrap();
            let mut dec = decoder().unwrap();
            let tone: Vec<f32> = (0..48_000)
                .map(|i| 0.25 * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin())
                .collect();
            let packets = enc.push(&[tone.clone(), tone]).unwrap();
            assert!(packets.len() >= 48, "{} packets", packets.len());
            let bytes: usize = packets.iter().map(Vec::len).sum();
            let rate = bytes as f64 * 8.0 / (packets.len() as f64 * 0.02) / 1000.0;
            assert!(rate < kbps as f64 * 1.2, "{kbps}: {rate:.0} kbit/s");
            let mut out: Vec<f32> = Vec::new();
            for p in &packets {
                let ch = dec.decode(p).unwrap();
                assert_eq!(ch.len(), 2);
                out.extend_from_slice(&ch[0]);
            }
            let tail = &out[out.len() / 2..];
            let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
            assert!((rms - 0.25 / 2f32.sqrt()).abs() < 0.03, "{kbps}: rms {rms}");
            // Its pitch: rising zero crossings a second.
            let ups = tail
                .windows(2)
                .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
                .count();
            let hz = ups as f64 / (tail.len() as f64 / 48_000.0);
            assert!((hz - 440.0).abs() < 5.0, "{kbps}: {hz:.0} Hz");
        }
    }
}
