//! IDN Camera (spec/extensions/idn-display.md §D.5): a switcher output (or any source) as a
//! webcam on Windows 11, through Media Foundation's virtual cameras. `idn-display
//! --virtual-camera` creates the camera while it runs; Windows' Frame Server then loads the
//! media source (`idn-camera-source`, `idn_camera.dll`), which shows what `idn-display`
//! writes to the shared memory ([`shm`]). What both sides share is here.

pub mod shm;

/// IDN Camera's media source's COM class (`idn_camera.dll`).
pub const CLSID_TEXT: &str = "{6EB55C36-2402-46E0-89C1-D28956A21F23}";
/// The same, as a number (`0x6eb55c36_2402_46e0_89c1_d28956a21f23`).
pub const CLSID_U128: u128 = 0x6eb55c36_2402_46e0_89c1_d28956a21f23;
