use super::*;

#[test]
fn messages_round_trip() {
    let m = video_message(
        Codec::Hevc,
        true,
        (1920, 1080),
        (30000, 1001),
        1234,
        b"sps",
        b"frame",
    );
    let v = parse_video(&m).unwrap();
    assert_eq!(
        (v.codec, v.keyframe, v.fps, v.pts, v.extra, v.data),
        (
            Codec::Hevc,
            true,
            (30000, 1001),
            1234,
            &b"sps"[..],
            &b"frame"[..]
        )
    );
    let a = parse_audio(&audio_message(
        48000,
        &[vec![0.5, -1.0, 0.0], vec![0.25, 1.0, -0.125]],
    ))
    .unwrap();
    assert_eq!(a.sample_rate, 48000);
    for (got, want) in a
        .channels
        .concat()
        .iter()
        .zip([0.5, -1.0, 0.0, 0.25, 1.0, -0.125])
    {
        assert!((got - want).abs() < 1e-6, "{got} vs {want}");
    }
    assert_eq!(
        split_name("CAMPUS-B (Camera 1 (wide))"),
        Some(("CAMPUS-B", "Camera 1 (wide)"))
    );
    assert!(resolve("127.0.0.1").unwrap().port() == DEFAULT_PORT);
}
