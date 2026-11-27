//! Screens on Wayland desktops (roadmap R.19): no program may read the screen there by
//! itself, so the desktop is asked through its screen-share permission
//! (xdg-desktop-portal's ScreenCast, the dialog GNOME, KDE, COSMIC and wlroots desktops
//! show), and the pictures come as a PipeWire stream. The permission is asked once: the
//! portal's restore token is kept (in the library's state directory, per source name)
//! and given back at the next start, so a bridge started at log-in comes back by itself.
//!
//! libpipewire is loaded at run time (every Wayland desktop has it), so nothing extra is
//! needed to build. Pictures come as DMA-BUF where the capture's GPU imports them (kept on
//! the GPU, `gpu`), else in shared memory (made RGBA on the CPU, `rgba`).
//!
//! `portal` asks; `pipewire` and `pod` speak to PipeWire; `stream` reads the screen.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

mod direct;
#[cfg(feature = "gpu")]
pub(crate) mod gpu;
mod pipewire;
mod pod;
mod portal;
mod rgba;
mod stream;

use pipewire::*;
pub use portal::{
    Grant, PortalInput, ask, ask_with_control, is_wayland, remote_desktop_available, save_token,
    saved_token,
};
use rgba::*;
pub use stream::{ScreenStream, ShareDevice, Shot};
pub use direct::{DirectInfo, DirectStream, direct_available};

/// SPA's packed RGB video formats (`spa_video_format`).
pub mod spa_format {
    pub use super::pod::{BGRA, BGRX, RGBA, RGBX};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmabuf_formats_round_trip() {
        // Offered: the modifiers to choose from, left to the consumer to fixate.
        let mods = [0x0300_0000_0000_e08e_u64, 0];
        let offer = pod::enum_format_dmabuf(pod::BGRX, &mods, false);
        assert_eq!(offer.len() % 8, 0);
        let c = pod::parse_format_full(&offer).unwrap();
        assert_eq!((c.fmt, c.w, c.h), (pod::BGRX, 1920, 1080));
        assert!(c.dont_fixate);
        assert_eq!(c.modifier, Some(mods[0]));
        assert_eq!(c.modifiers, mods.to_vec());
        // Fixed: one modifier, a Long.
        let fixed = pod::enum_format_dmabuf(pod::RGBA, &mods[1..], true);
        let c = pod::parse_format_full(&fixed).unwrap();
        assert_eq!(c.fmt, pod::RGBA);
        assert!(!c.dont_fixate);
        assert_eq!(c.modifier, Some(0));
        assert!(c.modifiers.is_empty());
        // Shared memory: no modifier.
        assert_eq!(pod::parse_format_full(&pod::enum_format()).unwrap().modifier, None);
        assert_eq!(pod::buffers_dmabuf().len() % 8, 0);
    }

    /// The consumer never picks a DMA-BUF layout from a list: the producer allocates with
    /// one of them (the driver choosing) and announces it. Picking the first tiled an
    /// NVIDIA desktop on COSMIC (it allocated with 16-GOB blocks; ours read 1-GOB ones).
    #[test]
    fn dmabuf_layout_only_once_the_producer_fixed_it() {
        use stream::{Dmabuf, dmabuf_decision};
        let (one_gob, sixteen) = (0x0300_0000_0060_6010_u64, 0x0300_0000_0060_6014_u64);
        let ours = [0, one_gob, sixteen];
        // COSMIC's offer, to be fixated by it: wait.
        let offer = pod::enum_format_dmabuf(pod::RGBA, &[one_gob, sixteen], false);
        let c = pod::parse_format_full(&offer).unwrap();
        assert_eq!(dmabuf_decision(&c, &ours), Dmabuf::Wait);
        // Its fixed one (the driver's choice): that layout, not the list's first.
        let fixed = pod::enum_format_dmabuf(pod::RGBA, &[sixteen], true);
        let c = pod::parse_format_full(&fixed).unwrap();
        assert_eq!(dmabuf_decision(&c, &ours), Dmabuf::Use(sixteen));
        // Fixed to one this device can't be told: shared memory.
        assert_eq!(dmabuf_decision(&c, &[0, one_gob]), Dmabuf::Memory(sixteen));
        // A choice not marked to be fixated is still a choice unless it has one value.
        let mut list = pod::parse_format_full(&offer).unwrap();
        list.dont_fixate = false;
        assert_eq!(dmabuf_decision(&list, &ours), Dmabuf::Wait);
        list.modifier = Some(sixteen);
        list.modifiers = vec![sixteen];
        assert_eq!(dmabuf_decision(&list, &ours), Dmabuf::Use(sixteen));
    }

    #[test]
    fn pods_and_pixels() {
        let f = pod::enum_format();
        assert_eq!(f.len() % 8, 0);
        // A fixed format as PipeWire sends it: object, format Id, size Rectangle.
        let fixed = {
            let mut body = Vec::new();
            for x in [
                pod::OBJECT_FORMAT,
                pod::PARAM_FORMAT,
                pod::KEY_FORMAT,
                0,
                4,
                pod::TYPE_ID,
                pod::BGRX,
                0,
                pod::KEY_SIZE,
                0,
                8,
                pod::TYPE_RECTANGLE,
                1280,
                720,
            ] {
                body.extend_from_slice(&x.to_ne_bytes());
            }
            let mut v = Vec::new();
            v.extend_from_slice(&(body.len() as u32).to_ne_bytes());
            v.extend_from_slice(&pod::TYPE_OBJECT.to_ne_bytes());
            v.extend_from_slice(&body);
            v
        };
        assert_eq!(pod::parse_format(&fixed), Some((pod::BGRX, 1280, 720)));
        assert_eq!(
            to_rgba(pod::BGRX, &[10, 20, 30, 0, 1, 2, 3, 0], 2, 1, 8, None).to_vec(),
            vec![30, 20, 10, 255, 3, 2, 1, 255]
        );
        // Every layout, a stride wider than the row, and a reused buffer.
        let src = [1, 2, 3, 4, 9, 9, 9, 9];
        for (fmt, want) in [
            (pod::RGBX, [1, 2, 3, 255]),
            (pod::RGBA, [1, 2, 3, 255]),
            (pod::BGRX, [3, 2, 1, 255]),
            (pod::BGRA, [3, 2, 1, 255]),
            (pod::XRGB, [2, 3, 4, 255]),
            (pod::XBGR, [4, 3, 2, 255]),
        ] {
            let a = to_rgba(fmt, &src, 1, 1, 8, None);
            assert_eq!(*a, want, "format {fmt}");
            let b = to_rgba(fmt, &[5, 6, 7, 8, 0, 0, 0, 0], 1, 1, 8, Some(a));
            assert_eq!(b[3], 255);
        }
    }

    /// to_rgba at 2160p (cargo test --release -p idn-ffmpeg --lib to_rgba_speed -- --ignored --nocapture).
    #[test]
    #[ignore = "timing"]
    fn to_rgba_speed() {
        let (w, h) = (3840usize, 2160usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7) as u8).collect();
        let t = Instant::now();
        for _ in 0..10 {
            std::hint::black_box(to_rgba(pod::BGRX, &src, w, h, w * 4, None));
        }
        let fresh = t.elapsed().as_secs_f64() * 100.0;
        let mut last = None;
        let t = Instant::now();
        for _ in 0..10 {
            last = Some(to_rgba(pod::BGRX, &src, w, h, w * 4, last.take()));
        }
        println!(
            "to_rgba 2160p BGRx: {fresh:.2} ms into a new buffer, {:.2} ms reusing the last",
            t.elapsed().as_secs_f64() * 100.0
        );
    }
}
