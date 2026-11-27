//! Copies out of memory the CPU doesn't cache: GPU surfaces and buffers mapped for
//! reading (decoders' downloads, the switcher's read-back).

/// Copies `src` to `dst` (as long as the shorter): from memory the CPU doesn't cache (a
/// GPU surface mapped for reading) with streaming loads, several times faster than a
/// plain copy (a 4K picture's download from the GPU took 18 ms that way, at 0.7 GB/s).
pub fn copy_uncached(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.1") {
        // SAFETY: SSE4.1 is there; the ranges are in bounds.
        unsafe { stream_copy(&mut dst[..n], &src[..n]) };
        return;
    }
    dst[..n].copy_from_slice(&src[..n]);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn stream_copy(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::{__m128i, _mm_storeu_si128, _mm_stream_load_si128};
    let n = src.len();
    // Up to the first 16-byte boundary of the source by hand, then 64 bytes a step (four
    // loads in flight), the rest by hand.
    let head = src.as_ptr().align_offset(16).min(n);
    dst[..head].copy_from_slice(&src[..head]);
    let mut i = head;
    // SAFETY: SSE4.1 is this function's feature; from `head` on the source is 16-aligned, as the
    // streaming load needs, and `i` stays 64 (then 16) bytes short of `n`; `dst` is as long as
    // `src` (the caller cuts both to one length).
    unsafe {
        while i + 64 <= n {
            let s = src.as_ptr().add(i) as *mut __m128i;
            let d = dst.as_mut_ptr().add(i) as *mut __m128i;
            let (a, b, c, e) = (
                _mm_stream_load_si128(s),
                _mm_stream_load_si128(s.add(1)),
                _mm_stream_load_si128(s.add(2)),
                _mm_stream_load_si128(s.add(3)),
            );
            _mm_storeu_si128(d, a);
            _mm_storeu_si128(d.add(1), b);
            _mm_storeu_si128(d.add(2), c);
            _mm_storeu_si128(d.add(3), e);
            i += 64;
        }
        while i + 16 <= n {
            let v = _mm_stream_load_si128(src.as_ptr().add(i) as *mut __m128i);
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, v);
            i += 16;
        }
    }
    dst[i..].copy_from_slice(&src[i..]);
}
