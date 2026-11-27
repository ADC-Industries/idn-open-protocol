//! The service's unit tests: the starters, hidden passwords, formats.

use super::*;

#[test]
fn the_starters_are_titles_timers_and_a_show() {
    let all: Value = serde_json::from_str(STARTERS).unwrap();
    let list = all.as_array().unwrap();
    assert_eq!(
        starters_list(STARTERS).as_array().unwrap().len(),
        list.len()
    );
    for s in list {
        assert!(s["about"].as_str().is_some_and(|a| !a.is_empty()));
        for i in s["inputs"].as_array().unwrap() {
            crate::title::Title::from_json(&i["title"])
                .unwrap_or_else(|e| panic!("{}: {e}", i["label"]));
        }
        let show = crate::show::Show::from_json(&s["show"]).unwrap();
        assert!(!show.to_json()["cues"].as_array().unwrap().is_empty());
    }
}

#[test]
fn passwords_are_hidden() {
    assert_eq!(
        super::hide_password("rtsp://admin:secret@10.0.0.9:554/s1"),
        "rtsp://admin:***@10.0.0.9:554/s1"
    );
    assert_eq!(
        super::hide_password("srt://1.2.3.4:9000?mode=caller&passphrase=abc123&latency=200"),
        "srt://1.2.3.4:9000?mode=caller&passphrase=***&latency=200"
    );
    assert_eq!(
        super::hide_password("rtmp://0.0.0.0:1935/live/key"),
        "rtmp://0.0.0.0:1935/live/key"
    );
}

#[test]
fn formats() {
    assert_eq!(parse_format("1080p30"), Some((1920, 1080, (30, 1))));
    assert_eq!(parse_format("2160p30"), Some((3840, 2160, (30, 1))));
    assert_eq!(
        parse_format("1080p59.94"),
        Some((1920, 1080, (60000, 1001)))
    );
    assert_eq!(parse_format("1280x720@60"), Some((1280, 720, (60, 1))));
    assert_eq!(parse_format("1080p0"), None);
    assert_eq!(parse_colour("#000000"), Some(Ycc::BLACK));
}
