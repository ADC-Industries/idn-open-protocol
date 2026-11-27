//! SpeedHQ conformance against the spec's test vectors (spec §3.13, rule of §3.9.3), copied
//! into tests/vectors/shq (verify with `sha256sum -c SHA256SUMS` there).

use idn_open_protocol::shq::{Limits, VideoParams, decode_with_ac_mask};
use serde_json::Value;
use std::path::PathBuf;

fn vec_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/shq")
}

#[test]
fn all_vectors_conform() {
    let dir = vec_dir();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("vectors.json")).unwrap()).unwrap();
    let vectors = manifest["vectors"].as_array().unwrap();
    assert_eq!(vectors.len(), 8);
    let mut failures = Vec::new();
    for v in vectors {
        let id = v["id"].as_str().unwrap();
        let eh = &v["ext_header"];
        let fourcc: [u8; 4] = eh["fourcc"]
            .as_str()
            .unwrap()
            .as_bytes()
            .try_into()
            .unwrap();
        let params = VideoParams {
            fourcc,
            width: eh["width"].as_u64().unwrap() as u32,
            height: eh["height"].as_u64().unwrap() as u32,
            field_id: eh["field_id"].as_u64().unwrap() as u32,
        };
        let payload = std::fs::read(dir.join(v["files"]["payload"].as_str().unwrap())).unwrap();
        let want = std::fs::read(dir.join(v["files"]["planes"].as_str().unwrap())).unwrap();
        let want_mask = std::fs::read(dir.join(v["files"]["acmask"].as_str().unwrap())).unwrap();

        let (pic, masks) = match decode_with_ac_mask(&params, &payload, &Limits::default()) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{id}: decode error {e}"));
                continue;
            }
        };
        assert_eq!(
            pic.quality as u64,
            v["payload"]["quality"].as_u64().unwrap(),
            "{id}"
        );
        let got: Vec<u8> = pic
            .planes
            .iter()
            .flat_map(|p| p.data.iter().copied())
            .collect();
        let got_mask: Vec<u8> = masks.iter().flat_map(|p| p.data.iter().copied()).collect();
        if got.len() != want.len() {
            failures.push(format!("{id}: size {} != {}", got.len(), want.len()));
            continue;
        }
        let mask_diff = got_mask
            .iter()
            .zip(&want_mask)
            .filter(|(a, b)| a != b)
            .count();
        let (mut exact_bad, mut ac_off1, mut ac_bad) = (0, 0, 0);
        for i in 0..want.len() {
            let d = (got[i] as i32 - want[i] as i32).abs();
            match (want_mask[i], d) {
                (_, 0) => {}
                (0, _) => exact_bad += 1,
                (_, 1) => ac_off1 += 1,
                _ => ac_bad += 1,
            }
        }
        println!("{id}: ±1 in AC blocks: {ac_off1}; mask mismatches: {mask_diff}");
        if exact_bad + ac_bad + mask_diff > 0 {
            failures.push(format!(
                "{id}: {exact_bad} wrong exact samples, {ac_bad} AC samples off by >1, \
                 {mask_diff} mask mismatches"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Corrupted payloads must be decoded or rejected, never panic (§3.12.1).
#[test]
fn mutated_vectors_never_panic() {
    let dir = vec_dir();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("vectors.json")).unwrap()).unwrap();
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut ok, mut err) = (0, 0);
    for v in manifest["vectors"].as_array().unwrap() {
        let eh = &v["ext_header"];
        let payload = std::fs::read(dir.join(v["files"]["payload"].as_str().unwrap())).unwrap();
        for _ in 0..3000 {
            let mut p = payload.clone();
            match rnd() % 4 {
                0 => p.truncate((rnd() as usize) % (p.len() + 1)),
                1 => {
                    for _ in 0..1 + rnd() % 4 {
                        let i = (rnd() as usize) % p.len();
                        p[i] ^= 1 << (rnd() % 8);
                    }
                }
                2 => {
                    let i = (rnd() as usize) % p.len();
                    p[i] = rnd() as u8;
                }
                _ => p.extend((0..rnd() % 64).map(|_| rnd() as u8)),
            }
            let params = VideoParams {
                fourcc: eh["fourcc"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
                    .try_into()
                    .unwrap(),
                width: eh["width"].as_u64().unwrap() as u32
                    + if rnd() % 8 == 0 {
                        (rnd() % 40) as u32
                    } else {
                        0
                    },
                height: eh["height"].as_u64().unwrap() as u32,
                field_id: (rnd() % 8 == 0) as u32 * (rnd() % 5) as u32,
            };
            match decode_with_ac_mask(&params, &p, &Limits::default()) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
    }
    println!("mutations: {ok} decoded, {err} rejected");
}
