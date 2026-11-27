//! A driver loaded (Windows: COM; Linux: its library and `instantiateDriver`), and the
//! calls made to it: the same on both, through the driver's vtable.

use super::*;
use std::io;

#[cfg(target_os = "linux")]
use crate::linux as platform;
#[cfg(windows)]
use crate::windows as platform;

/// A driver loaded on this thread (Windows: COM, used and released on it).
pub struct Loaded {
    drv: *mut Driver,
    /// What the platform keeps while the driver is loaded (Windows: COM started here for
    /// it, ended after the driver's release). Dropped after `drv` is released.
    _hold: platform::Hold,
}

impl Loaded {
    /// Loads driver `id` (not yet initialised: [`Loaded::init`]).
    pub fn load(id: &DriverId) -> io::Result<Loaded> {
        let (drv, hold) = platform::load(id)?;
        Ok(Loaded { drv, _hold: hold })
    }

    fn v(&self) -> &DriverVtbl {
        // SAFETY: a live driver object's vtable.
        unsafe { &*(*self.drv).vtbl }
    }

    /// The raw object (for the calls not wrapped here).
    pub fn raw(&self) -> *mut Driver {
        self.drv
    }

    /// `init`: false with the driver's message as the error.
    pub fn init(&self) -> io::Result<()> {
        // SAFETY: the driver's own method on itself; no window handle is passed.
        if unsafe { (self.v().init)(self.drv, std::ptr::null_mut()) } == ASIO_FALSE {
            return Err(io::Error::other(self.error_message()));
        }
        Ok(())
    }

    pub fn name(&self) -> String {
        let mut b = [0 as c_char; 32];
        // SAFETY: a 32-byte buffer, as the method fills.
        unsafe { (self.v().get_driver_name)(self.drv, b.as_mut_ptr()) };
        text(&b)
    }

    pub fn error_message(&self) -> String {
        let mut b = [0 as c_char; 124];
        // SAFETY: a 124-byte buffer, as the method fills.
        unsafe { (self.v().get_error_message)(self.drv, b.as_mut_ptr()) };
        text(&b)
    }

    /// (inputs, outputs).
    pub fn channels(&self) -> io::Result<(usize, usize)> {
        let (mut i, mut o) = (0, 0);
        check(unsafe { (self.v().get_channels)(self.drv, &mut i, &mut o) })?;
        Ok((i.max(0) as usize, o.max(0) as usize))
    }

    /// (min, max, preferred, granularity).
    pub fn buffer_size(&self) -> io::Result<(i64, i64, i64, i64)> {
        // (Typed as the interface's `long`: 32 bits on Windows, 64 on Linux.)
        let mut a: c_long = 0;
        let mut b: c_long = 0;
        let mut c: c_long = 0;
        let mut d: c_long = 0;
        check(unsafe { (self.v().get_buffer_size)(self.drv, &mut a, &mut b, &mut c, &mut d) })?;
        Ok((a as i64, b as i64, c as i64, d as i64))
    }

    /// (input, output) in samples.
    pub fn latencies(&self) -> io::Result<(i64, i64)> {
        let mut i: c_long = 0;
        let mut o: c_long = 0;
        check(unsafe { (self.v().get_latencies)(self.drv, &mut i, &mut o) })?;
        Ok((i as i64, o as i64))
    }

    pub fn can_rate(&self, rate: f64) -> bool {
        // SAFETY: the driver's own method.
        unsafe { (self.v().can_sample_rate)(self.drv, rate) == ASE_OK }
    }

    pub fn rate(&self) -> io::Result<f64> {
        let mut r = 0.0;
        check(unsafe { (self.v().get_sample_rate)(self.drv, &mut r) })?;
        Ok(r)
    }

    pub fn set_rate(&self, rate: f64) -> io::Result<()> {
        check(unsafe { (self.v().set_sample_rate)(self.drv, rate) })
    }

    pub fn channel_info(&self, input: bool, channel: usize) -> io::Result<ChannelInfo> {
        let mut info = ChannelInfo {
            channel: channel as c_long,
            is_input: if input { ASIO_TRUE } else { ASIO_FALSE },
            is_active: ASIO_FALSE,
            channel_group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        check(unsafe { (self.v().get_channel_info)(self.drv, &mut info) })?;
        Ok(info)
    }

    /// # Safety
    /// `infos` stay valid, and `callbacks` lives until [`Loaded::dispose_buffers`].
    pub unsafe fn create_buffers(
        &self,
        infos: &mut [BufferInfo],
        size: i64,
        callbacks: *const Callbacks,
    ) -> io::Result<()> {
        check(unsafe {
            (self.v().create_buffers)(
                self.drv,
                infos.as_mut_ptr(),
                infos.len() as c_long,
                size as c_long,
                callbacks,
            )
        })
    }

    pub fn dispose_buffers(&self) -> io::Result<()> {
        check(unsafe { (self.v().dispose_buffers)(self.drv) })
    }

    pub fn start(&self) -> io::Result<()> {
        check(unsafe { (self.v().start)(self.drv) })
    }

    pub fn stop(&self) -> io::Result<()> {
        check(unsafe { (self.v().stop)(self.drv) })
    }

    /// `outputReady`: whether the driver takes it (asked once; then after each switch).
    pub fn output_ready(&self) -> bool {
        // SAFETY: the driver's own method.
        unsafe { (self.v().output_ready)(self.drv) == ASE_OK }
    }

    pub fn control_panel(&self) -> io::Result<()> {
        check(unsafe { (self.v().control_panel)(self.drv) })
    }
}

/// The calls above: the driver's own methods on itself, with outputs it fills.
fn check(e: AsioError) -> io::Result<()> {
    match e {
        ASE_OK | ASE_SUCCESS => Ok(()),
        ASE_NotPresent => Err(io::Error::new(io::ErrorKind::NotFound, "not present")),
        ASE_InvalidParameter => Err(idn_common::err::invalid_input("invalid parameter")),
        ASE_InvalidMode => Err(io::Error::other("invalid mode")),
        ASE_HWMalfunction => Err(io::Error::other("the hardware is malfunctioning")),
        ASE_NoClock => Err(io::Error::other("no clock (that rate isn't available)")),
        ASE_SPNotAdvancing => Err(io::Error::other("the driver isn't running")),
        ASE_NoMemory => Err(io::Error::new(io::ErrorKind::OutOfMemory, "no memory")),
        e => Err(io::Error::other(format!("ASIO error {e}"))),
    }
}

impl Drop for Loaded {
    /// The driver's reference released; then `_hold` goes (Windows: COM ended as it was
    /// started, after the release).
    fn drop(&mut self) {
        // SAFETY: the object loaded here, released once.
        unsafe { ((*(*self.drv).vtbl).release)(self.drv) };
    }
}
