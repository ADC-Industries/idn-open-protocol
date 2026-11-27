//! The threaded decode paths must give exactly the serial result. Kept in its own test
//! binary because `force_workers` is process-wide.

use idn_open_protocol::shq::{Limits, VideoParams, decode_with_ac_mask, force_workers};
use serde_json::Value;
use std::path::PathBuf;

#[test]
fn parallel_equals_serial_on_all_vectors() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/shq");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("vectors.json")).unwrap()).unwrap();
    for v in manifest["vectors"].as_array().unwrap() {
        let eh = &v["ext_header"];
        let params = VideoParams {
            fourcc: eh["fourcc"]
                .as_str()
                .unwrap()
                .as_bytes()
                .try_into()
                .unwrap(),
            width: eh["width"].as_u64().unwrap() as u32,
            height: eh["height"].as_u64().unwrap() as u32,
            field_id: 0,
        };
        let payload = std::fs::read(dir.join(v["files"]["payload"].as_str().unwrap())).unwrap();
        force_workers(1);
        let (serial, smask) = decode_with_ac_mask(&params, &payload, &Limits::default()).unwrap();
        for n in [2, 3, 4, 16] {
            force_workers(n);
            let (par, pmask) = decode_with_ac_mask(&params, &payload, &Limits::default()).unwrap();
            assert_eq!(par.planes, serial.planes, "{} with {n} workers", v["id"]);
            assert_eq!(pmask, smask, "{} mask with {n} workers", v["id"]);
        }
    }
    force_workers(0);
}
