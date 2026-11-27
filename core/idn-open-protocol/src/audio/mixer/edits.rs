//! Edits of a lane's ring: channels placed, fades in and out, cuts and pads at quiet places.

use super::*;

/// Places a routed block's channels on the device's (see [`Lane::push`]).
pub(crate) fn route_channels(
    src: &[Vec<f32>],
    frames: usize,
    n: usize,
    map: &[(usize, u16)],
) -> Vec<Vec<f32>> {
    // Only the channels the routes feed (the resampler takes no others): the rest empty,
    // not a zeroed block each (16 stereo sources on a 32-channel desk allocated and zeroed
    // 32 channels a block each).
    let mut out = vec![Vec::new(); n];
    for &(i, d) in map {
        if let (Some(s), Some(o)) = (src.get(i), out.get_mut(d as usize)) {
            if o.is_empty() {
                o.resize(frames, 0.0);
            }
            for (o, v) in o.iter_mut().zip(s) {
                *o += *v;
            }
        }
    }
    out
}

/// Fades the interleaved ring's front in over `fade` frames. What it doesn't hold yet of
/// the fade is left in `left` for the frames that come next ([`fade_in_rest`]): faded
/// only over what it held, a start at a desk's low latency rose to half and jumped to
/// full level (lab 372).
pub(crate) fn fade_in_front(
    ring: &mut VecDeque<f32>,
    rw: usize,
    fade: usize,
    left: &mut Option<(usize, usize)>,
) {
    let fade = fade.max(1);
    let n = (ring.len() / rw).min(fade);
    for (i, s) in ring.iter_mut().take(n * rw).enumerate() {
        *s *= (i / rw) as f32 / fade as f32;
    }
    *left = (n < fade).then_some((n, fade));
}

/// The rest of a fade-in begun by [`fade_in_front`], on `fresh` frames about to join the
/// ring.
pub(crate) fn fade_in_rest(fresh: &mut [f32], rw: usize, left: &mut Option<(usize, usize)>) {
    let Some((done, fade)) = *left else {
        return;
    };
    let n = (fresh.len() / rw).min(fade - done);
    for (i, s) in fresh.iter_mut().take(n * rw).enumerate() {
        *s *= (done + i / rw) as f32 / fade as f32;
    }
    *left = (done + n < fade).then_some((done + n, fade));
}

/// Removes `cut` samples from the front of the interleaved ring, crossfading the sound
/// just before the cut into the sound just after it over `fade` frames so the join
/// doesn't click. Returns the frames removed.
pub(crate) fn cut_front(ring: &mut VecDeque<f32>, cut: usize, dev_ch: usize, fade: usize) -> usize {
    let cut = cut / dev_ch * dev_ch;
    if cut == 0 {
        return 0;
    }
    let fade_s = (fade * dev_ch).min(cut).min(ring.len() - cut);
    let old: Vec<f32> = ring.iter().take(fade_s).copied().collect();
    ring.drain(..cut);
    let fade_frames = (fade_s / dev_ch).max(1) as f32;
    for (i, s) in ring.iter_mut().take(fade_s).enumerate() {
        let t = (i / dev_ch) as f32 / fade_frames;
        *s = old[i] * (1.0 - t) + *s * t;
    }
    cut / dev_ch
}

/// Fades channels `chans` of the interleaved ring out over its last `frames` frames (or
/// all it holds): a route taken away ends without a step.
pub(crate) fn fade_tail(ring: &mut VecDeque<f32>, dev_ch: usize, chans: &[usize], frames: usize) {
    if chans.is_empty() || dev_ch == 0 {
        return;
    }
    let have = ring.len() / dev_ch;
    let n = frames.min(have).max(1);
    for j in 0..n.min(have) {
        let g = 1.0 - (j + 1) as f32 / n as f32;
        let at = (have - n + j) * dev_ch;
        for &c in chans.iter().filter(|&&c| c < dev_ch) {
            ring[at + c] *= g;
        }
    }
}

/// Under this on every channel (-54 dBFS) a stretch of sound is quiet: a cut or added
/// silence there isn't heard.
pub(crate) const QUIET: f32 = 0.002;

/// The first frame of the first run of `need` quiet frames in the interleaved ring.
pub(crate) fn quiet_run(
    ring: &VecDeque<f32>,
    dev_ch: usize,
    need: usize,
    from: usize,
) -> Option<usize> {
    let mut run = 0;
    for f in from..ring.len() / dev_ch {
        if (0..dev_ch).all(|c| ring[f * dev_ch + c].abs() < QUIET) {
            run += 1;
            if run >= need {
                return Some(f + 1 - need);
            }
        } else {
            run = 0;
        }
    }
    None
}

/// Where to look for a quiet stretch of `need` frames: the whole ring, or, when the ring
/// was looked through after the last push and had none, only what could end in the
/// `recent` frames pushed since (the whole ring was scanned under
/// its lock on every push, for up to 5 s after a start).
pub(crate) fn quiet_from(
    ring: &VecDeque<f32>,
    dev_ch: usize,
    need: usize,
    recent: Option<usize>,
) -> usize {
    recent.map_or(0, |r| (ring.len() / dev_ch).saturating_sub(r + need))
}

/// Removes `n` frames from a quiet stretch of the ring (with `margin` quiet frames kept
/// either side), if it has one.
pub(crate) fn cut_quiet(
    ring: &mut VecDeque<f32>,
    n: usize,
    dev_ch: usize,
    margin: usize,
    recent: Option<usize>,
) -> bool {
    let need = n + 2 * margin;
    let Some(f) = quiet_run(ring, dev_ch, need, quiet_from(ring, dev_ch, need, recent)) else {
        return false;
    };
    let at = (f + margin) * dev_ch;
    ring.drain(at..at + n * dev_ch);
    true
}

/// Puts `n` frames of silence into a quiet stretch of the ring, if it has one.
pub(crate) fn pad_quiet(
    ring: &mut VecDeque<f32>,
    n: usize,
    dev_ch: usize,
    margin: usize,
    recent: Option<usize>,
) -> bool {
    let need = 2 * margin;
    let Some(f) = quiet_run(ring, dev_ch, need, quiet_from(ring, dev_ch, need, recent)) else {
        return false;
    };
    let tail = ring.split_off((f + margin) * dev_ch);
    ring.extend(std::iter::repeat_n(0.0, n * dev_ch));
    ring.extend(tail);
    true
}
