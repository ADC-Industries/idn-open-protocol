//! The driver as a cwASIO host on Linux sees it: listed in a list folder (a temporary one,
//! not /etc/cwASIO) and taken off again, and its object made by `instantiateDriver` and
//! called through the interface's vtable.
#![cfg(target_os = "linux")]

use idn_asio::linux::{instantiateDriver, own_path, register_in, releaseDriver, unregister_in};
use idn_asio_abi::{
    ASE_NotPresent, ASE_OK, ASE_SUCCESS, DriverId, drivers_in, kcwASIOsetInstanceName, text,
};
use std::ffi::{c_char, c_long, c_void};

#[test]
fn the_driver_lists_itself_and_comes_off_again() {
    // (A static program's loader can't say which file it is: nothing to list then.)
    let Some(me) = own_path() else {
        // (glibc always says: there it is a failure.)
        if cfg!(target_env = "gnu") {
            panic!("dladdr found no file");
        }
        return;
    };
    let dir = idn_common::testing::TempDir::new("idn-asio-cwasio");

    assert_eq!(register_in(&dir, "IDN"), 0);
    let all = drivers_in(&dir);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].name, "IDN");
    assert_eq!(all[0].description, "IDN");
    assert_eq!(
        all[0].id,
        DriverId::Library {
            name: "IDN".into(),
            path: me
        }
    );
    // Again: fine (the same library). A bad name: refused.
    assert_eq!(register_in(&dir, "IDN"), 0);
    assert_eq!(register_in(&dir, "a/b"), libc::EINVAL);
    // A name another library holds: neither taken over nor taken off.
    std::fs::create_dir_all(dir.join("Other")).unwrap();
    std::fs::write(dir.join("Other/driver"), "/usr/lib/cwASIO/libother.so\n").unwrap();
    assert_eq!(register_in(&dir, "Other"), libc::EEXIST);
    assert_eq!(unregister_in(&dir, "Other"), libc::EACCES);

    assert_eq!(unregister_in(&dir, "IDN"), 0);
    assert!(!dir.join("IDN").exists());
    assert_eq!(unregister_in(&dir, "IDN"), libc::ENODEV);
}

#[test]
fn a_cwasio_host_makes_and_calls_the_driver_object() {
    let d = instantiateDriver();
    assert!(!d.is_null());
    // SAFETY: the object just made (one reference ours), called through its own vtable as
    // a host does; not initialised (that joins the network).
    unsafe {
        let v = &*(*d).vtbl;
        let mut name = [0 as c_char; 32];
        (v.get_driver_name)(d, name.as_mut_ptr());
        assert_eq!(text(&name), "IDN");
        assert_eq!((v.get_driver_version)(d), 1);
        // The default interface (cwASIO asks with no id): the same object, one more
        // reference.
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!((v.query_interface)(d, std::ptr::null(), &mut out), 0);
        assert_eq!(out, d.cast::<c_void>());
        assert_eq!((v.release)(d), 1);
        // What a host asks before its buffers.
        let (mut ins, mut outs): (c_long, c_long) = (-1, -1);
        assert_eq!((v.get_channels)(d, &mut ins, &mut outs), ASE_OK);
        assert!((0..=64).contains(&ins) && (0..=64).contains(&outs));
        let (mut min, mut max, mut pref, mut gran): (c_long, c_long, c_long, c_long) = (0, 0, 0, 0);
        assert_eq!(
            (v.get_buffer_size)(d, &mut min, &mut max, &mut pref, &mut gran),
            ASE_OK
        );
        assert!(min <= pref && pref <= max);
        assert_eq!(gran, -1);
        assert_eq!((v.can_sample_rate)(d, 48_000.0), ASE_OK);
        // The last reference: the object goes; then the library's count (nothing to do).
        assert_eq!((v.release)(d), 0);
        releaseDriver(d);
    }
}

/// cwASIO's instances: the host names the instance before `init`, and the driver says
/// that name back; a null or empty name only asks; too long is refused.
#[test]
fn the_host_names_the_instance_before_init() {
    let d = instantiateDriver();
    assert!(!d.is_null());
    // SAFETY: as above; strings NUL-terminated.
    unsafe {
        let v = &*(*d).vtbl;
        let name = |d| {
            let mut b = [0 as c_char; 32];
            (v.get_driver_name)(d, b.as_mut_ptr());
            text(&b)
        };
        assert_eq!(name(d), "IDN");
        let set =
            |s: &std::ffi::CStr| (v.future)(d, kcwASIOsetInstanceName, s.as_ptr() as *mut c_void);
        assert_eq!(
            (v.future)(d, kcwASIOsetInstanceName, std::ptr::null_mut()),
            ASE_SUCCESS
        );
        assert_eq!(set(c""), ASE_SUCCESS);
        assert_eq!(set(c"IDN 2"), ASE_SUCCESS);
        assert_eq!(name(d), "IDN 2");
        // Read back by setting it again.
        assert_eq!(set(c"IDN 2"), ASE_SUCCESS);
        assert_eq!(set(c"a name longer than thirty-one bytes"), ASE_NotPresent);
        assert_eq!(name(d), "IDN 2");
        // Its channels are the new instance's (the driver made again).
        let (mut ins, mut outs): (c_long, c_long) = (-1, -1);
        assert_eq!((v.get_channels)(d, &mut ins, &mut outs), ASE_OK);
        assert!(ins >= 0 && outs >= 0);
        assert_eq!((v.release)(d), 0);
    }
}
