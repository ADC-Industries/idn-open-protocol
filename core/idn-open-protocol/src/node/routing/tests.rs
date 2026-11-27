//! Tests of the route table and route changes.

use super::*;

#[test]
fn table_applies_saves_and_loads() {
    let mut t = RouteTable::default();
    let set = |ch, tx: &str, tx_ch| RouteChange {
        receiver: "Stage".into(),
        ch,
        to: Some((tx.into(), tx_ch)),
    };
    assert!(t.apply(&set(1, "FOH (Desk)", 13)));
    assert!(t.apply(&set(0, "FOH (Desk)", 12)));
    assert!(!t.apply(&set(0, "FOH (Desk)", 12)), "same route: no change");
    assert_eq!(
        t.routes("Stage")
            .iter()
            .map(|r| (r.ch, r.tx_ch))
            .collect::<Vec<_>>(),
        [(0, 12), (1, 13)]
    );
    let dir = idn_common::testing::TempDir::new("routes");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("routes.xml");
    t.save(&p).unwrap();
    assert_eq!(RouteTable::load(&p), t);
    // A latency (§A.12): kept with the routes, and for a receiver without any.
    assert!(t.set_latency("Stage", Some(3)));
    assert!(!t.set_latency("Stage", Some(3)), "same latency: no change");
    assert!(t.set_latency("Lobby", Some(20)));
    t.save(&p).unwrap();
    let back = RouteTable::load(&p);
    assert_eq!(back, t);
    assert_eq!(
        (back.latency("Stage"), back.latency("Lobby")),
        (Some(3), Some(20))
    );
    assert_eq!(back.receivers(), ["Lobby", "Stage"]);
    assert!(t.set_latency("Lobby", None));
    assert!(t.set_latency("Stage", None));
    assert_eq!(parse_latency(Some("0")), None);
    assert_eq!(parse_latency(Some("2")), Some(2));
    let clear = RouteChange {
        receiver: "Stage".into(),
        ch: 0,
        to: None,
    };
    assert!(t.apply(&clear));
    assert!(!t.apply(&clear));
    assert!(t.apply(&RouteChange {
        ch: 1,
        ..clear.clone()
    }));
    assert!(
        t.receivers().is_empty(),
        "a receiver without routes is forgotten"
    );
}

#[test]
fn changes_parse_and_refuse_bad_ones() {
    let c = RouteChange {
        receiver: "Stage".into(),
        ch: 3,
        to: Some(("A (B)".into(), 7)),
    };
    assert_eq!(RouteChange::parse(&c.to_element()).unwrap(), c);
    let clear = RouteChange {
        to: None,
        ..c.clone()
    };
    assert_eq!(RouteChange::parse(&clear.to_element()).unwrap(), clear);
    let bad = |xml: &str| RouteChange::parse(&crate::xml::parse(xml).unwrap()).is_err();
    assert!(bad(r#"<route ch="0" tx="A" tx_ch="0"/>"#));
    assert!(bad(r#"<route receiver="R" ch="256" tx="A" tx_ch="0"/>"#));
    assert!(bad(r#"<route receiver="R" ch="0" tx="A"/>"#));
    assert_eq!(
        labels_attr(&["Kick".into(), "Vox, lead".into()]),
        "Kick,Vox  lead"
    );
    assert_eq!(parse_labels(Some("a, b")), ["a", "b"]);
}
