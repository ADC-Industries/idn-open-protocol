#[test]
fn a_block_eased_keeps_its_ends_and_shape() {
    let c: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.05).sin()).collect();
    for m in [990, 1000, 1010] {
        let s = super::stretch(&c, m);
        assert_eq!(s.len(), m);
        assert_eq!((s[0], s[m - 1]), (c[0], c[999]));
        // A slow sine stays within a sample step of itself (0.05 here).
        let worst = (0..m)
            .map(|j| (s[j] - c[(j * 999 / (m - 1)).min(999)]).abs())
            .fold(0f32, f32::max);
        assert!(worst < 0.06, "{m}: {worst}");
    }
}
