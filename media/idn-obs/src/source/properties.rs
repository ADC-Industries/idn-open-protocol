//! The source's settings page: its defaults and its properties.

use super::*;

pub(super) unsafe extern "C" fn get_defaults(settings: *mut ffi::obs_data_t) {
    // SAFETY: settings OBS gives for the call; NUL-ended names and values.
    unsafe {
        ffi::obs_data_set_default_bool(settings, S_LOW.as_ptr(), false);
        ffi::obs_data_set_default_string(settings, S_TIMING.as_ptr(), c"auto".as_ptr());
        ffi::obs_data_set_default_string(settings, S_CODEC.as_ptr(), c"allow".as_ptr());
        ffi::obs_data_set_default_string(settings, S_PARALLEL.as_ptr(), c"machine".as_ptr());
        ffi::obs_data_set_default_string(settings, S_DECODER.as_ptr(), c"machine".as_ptr());
        ffi::obs_data_set_default_string(settings, S_SHQ.as_ptr(), c"auto".as_ptr());
        ffi::obs_data_set_default_int(settings, crate::S_THREADS.as_ptr(), 0);
        ffi::obs_data_set_default_bool(settings, S_USE_NODE.as_ptr(), true);
        ffi::obs_data_set_default_string(settings, S_HDR.as_ptr(), c"plugin".as_ptr());
        // A Windows desktop in HDR mode shows SDR content at its "SDR content brightness",
        // 80 nits on the laptops seen so far; a screen capture of it is PQ-coded at that.
        ffi::obs_data_set_default_int(settings, S_SDR_WHITE.as_ptr(), 80);
    }
}

pub(super) unsafe extern "C" fn get_properties(_: *mut c_void) -> *mut ffi::obs_properties_t {
    // SAFETY: a properties page made here and handed to OBS; the lists are its own;
    // NUL-ended strings that outlive each call.
    unsafe {
        let props = ffi::obs_properties_create();
        let list = ffi::obs_properties_add_list(
            props,
            S_SOURCE.as_ptr(),
            c"Source (found on the network)".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        ffi::obs_property_list_add_string(list, c"(none)".as_ptr(), c"".as_ptr());
        // A one-second look at the network when the properties open.
        let opts = idn_open_protocol::discovery::BrowseOptions::default();
        if let Ok(found) = idn_open_protocol::discovery::browse(Duration::from_secs(1), &opts) {
            for s in found {
                if let Ok(name) = CString::new(s.name.clone()) {
                    ffi::obs_property_list_add_string(list, name.as_ptr(), name.as_ptr());
                }
            }
        }
        ffi::obs_properties_add_text(
            props,
            S_ADDRESS.as_ptr(),
            c"Or an address (host:port, or \"HOST (name)\")".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        ffi::obs_properties_add_text(
            props,
            S_RECEIVER.as_ptr(),
            c"Receiver name (shown to the sender; default: OBS + this source's name)".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        ffi::obs_properties_add_bool(
            props,
            S_LOW.as_ptr(),
            c"Low bandwidth (preview quality)".as_ptr(),
        );
        let timing = ffi::obs_properties_add_list(
            props,
            S_TIMING.as_ptr(),
            c"Timing".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        for (label, value) in [
            (c"Automatic (buffered)", c"auto"),
            (
                c"Buffered: smooth timing, sound in sync (adds delay)",
                c"buffered",
            ),
            (
                c"Unbuffered: each frame at once, lowest delay",
                c"unbuffered",
            ),
        ] {
            ffi::obs_property_list_add_string(timing, label.as_ptr(), value.as_ptr());
        }
        let codec = ffi::obs_properties_add_list(
            props,
            S_CODEC.as_ptr(),
            c"Compressed video from IDN senders (H.264/HEVC)".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        ffi::obs_property_list_add_string(
            codec,
            c"Never (SpeedHQ only)".as_ptr(),
            c"never".as_ptr(),
        );
        ffi::obs_property_list_add_string(
            codec,
            c"When the sender chooses it (default: 4K pictures, or when it insists)".as_ptr(),
            c"allow".as_ptr(),
        );
        ffi::obs_property_list_add_string(
            codec,
            c"Prefer it (Wi-Fi, remote rooms)".as_ptr(),
            c"prefer".as_ptr(),
        );
        let dec = ffi::obs_properties_add_list(
            props,
            S_DECODER.as_ptr(),
            c"Decoding of H.264/HEVC".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        for (label, value) in [
            (
                c"The machine's setting in the IDN Node (default; automatic without one)",
                c"machine",
            ),
            (c"Automatic (CPU, the GPU if the CPU falls behind)", c"auto"),
            (c"GPU (VA-API, NVIDIA NVDEC or Windows D3D11VA)", c"gpu"),
            (c"CPU (software)", c"software"),
        ] {
            ffi::obs_property_list_add_string(dec, label.as_ptr(), value.as_ptr());
        }
        let shq = ffi::obs_properties_add_list(
            props,
            S_SHQ.as_ptr(),
            c"Decoding of SpeedHQ (IDN, NDI-Compatible full bandwidth)".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        for (label, value) in [
            (
                c"Automatic (default: tries the GPU and the CPU, keeps the cheaper)",
                c"auto",
            ),
            (
                c"GPU (hybrid: the bitstream on the CPU, the transform on the GPU)",
                c"gpu",
            ),
            (c"CPU", c"cpu"),
        ] {
            ffi::obs_property_list_add_string(shq, label.as_ptr(), value.as_ptr());
        }
        let par = ffi::obs_properties_add_list(
            props,
            S_PARALLEL.as_ptr(),
            c"Decode two pictures at once (4K at 50/60 fps)".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        for (label, value) in [
            (
                c"The machine's setting in the IDN Node (default; on without one)",
                c"machine",
            ),
            (c"On", c"on"),
            (c"Off", c"off"),
        ] {
            ffi::obs_property_list_add_string(par, label.as_ptr(), value.as_ptr());
        }
        crate::add_threads_property(props);
        let hdr = ffi::obs_properties_add_list(
            props,
            S_HDR.as_ptr(),
            c"HDR sources (PQ/HLG, e.g. a Windows HDR desktop)".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        ffi::obs_property_list_add_string(
            hdr,
            c"Convert to SDR here (default; white at the level below)".as_ptr(),
            c"plugin".as_ptr(),
        );
        ffi::obs_property_list_add_string(
            hdr,
            c"Hand to OBS as HDR (OBS tone-maps with its SDR White Level)".as_ptr(),
            c"obs".as_ptr(),
        );
        ffi::obs_properties_add_int(
            props,
            S_SDR_WHITE.as_ptr(),
            c"SDR white of HDR sources (nits; Windows HDR desktops: often 80, the \"SDR content brightness\" setting)".as_ptr(),
            50,
            1000,
            1,
        );
        ffi::obs_properties_add_bool(
            props,
            S_USE_NODE.as_ptr(),
            c"Use the IDN Node (this machine's site identity; the fields below apply when off or with no Node running)".as_ptr(),
        );
        ffi::obs_properties_add_text(
            props,
            S_PASSPHRASE.as_ptr(),
            c"Site passphrase (encryption, IDN sources)".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_PASSWORD,
        );
        ffi::obs_properties_add_text(
            props,
            S_SERVER.as_ptr(),
            c"Discovery Server (host[:port])".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        ffi::obs_properties_add_text(
            props,
            S_TRUST_DIR.as_ptr(),
            c"Site trust directory (enrols this OBS with the authority)".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        props
    }
}
