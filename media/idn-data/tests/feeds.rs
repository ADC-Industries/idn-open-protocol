//! A feed end to end (R.22): a CSV file read by IDN Data, its rows received from the
//! feed's text-only source, a change to the file seen by the receiver, kept in
//! feeds.json for the next start.

use idn_data::{Feed, Feeds};
use idn_open_protocol::api::{Publisher, PublisherConfig};
use idn_open_protocol::input::InputConfig;
use idn_open_protocol::link::LinkConfig;
use idn_open_protocol::text::TextReceiver;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn a_file_feed_reaches_a_receiver_and_follows_changes() {
    let dir = std::env::temp_dir().join(format!("idn-data-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("scores.csv");
    std::fs::write(&csv, "Team,Score\nLions,3\nBears,1\n").unwrap();
    let publisher = || {
        Publisher::new(PublisherConfig {
            mdns: Some(false),
            link: Some(LinkConfig::ephemeral()),
            ..Default::default()
        })
        .unwrap()
    };
    let feeds = Feeds::start(publisher(), Some(dir.join("state"))).unwrap();
    feeds
        .add(
            Feed::from_json(&json!({"name": "Scores", "from": csv.to_str().unwrap(), "every": 5}))
                .unwrap(),
        )
        .unwrap();
    // The feed's source, by its port (no discovery here).
    let port = |feeds: &Feeds| {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let s = feeds.state();
            if s["feeds"][0]["rows"].is_number() || Instant::now() > end {
                let name = s["feeds"][0]["source"].as_str().unwrap().to_owned();
                assert!(name.ends_with("(Data: Scores)"), "{name}");
                return s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let s = port(&feeds);
    assert_eq!(s["feeds"][0]["rows"], 2, "{s}");
    // In the file's order.
    assert_eq!(s["feeds"][0]["columns"], json!(["Team", "Score"]));
    let rx = TextReceiver::open(
        &format!("127.0.0.1:{}", feeds_port(&feeds)),
        InputConfig {
            link: Some(LinkConfig::ephemeral()),
            ..InputConfig::new("")
        },
    )
    .unwrap();
    let wait = |pred: &dyn Fn(&idn_open_protocol::text::DataMessage) -> bool| {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if let Some(m) = rx.data(Duration::from_millis(200))
                && pred(&m)
            {
                return Some(m);
            }
        }
        None
    };
    let m = wait(&|m| m.rows.len() == 2).expect("the rows (by the repeat)");
    assert_eq!(m.rows[0]["Team"], "Lions");
    // The sheet changes: read again (now), and the receiver has it.
    std::fs::write(&csv, "Team,Score\nLions,4\nBears,1\nTigers,0\n").unwrap();
    feeds.read_now("Scores").unwrap();
    let m = wait(&|m| m.rows.len() == 3).expect("the new rows");
    assert_eq!(m.rows[0]["Score"], "4");
    // The file goes: the last rows stay, with why.
    std::fs::remove_file(&csv).unwrap();
    feeds.read_now("Scores").unwrap();
    let m = wait(&|m| m.error.is_some()).expect("the error");
    assert_eq!(m.rows.len(), 3);
    // Kept for the next start.
    drop(rx);
    drop(feeds);
    let again = Feeds::start(publisher(), Some(dir.join("state"))).unwrap();
    assert_eq!(again.state()["feeds"][0]["name"], "Scores");
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
}

fn feeds_port(feeds: &Feeds) -> u16 {
    feeds.state()["feeds"][0]["port"].as_u64().unwrap() as u16
}
