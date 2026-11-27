use super::*;

#[test]
fn messages_keep_their_properties_in_storage() {
    let mut m = Msg::new("idn/pd/m/batch.ticket", Bytes::from_static(b"{}"), 1, false);
    m.props.response_topic = Some("app/answers".into());
    m.props.correlation = Some(vec![1, 2, 3]);
    m.props.user = vec![("id".into(), "ab".into())];
    assert_eq!(Msg::decode(&m.encode()), Some(m));
}

#[test]
fn only_the_site_manager_writes_what_machines_send() {
    assert!(reserved("idn/pd/v/silo/3"));
    assert!(reserved("idn/pd/status"));
    assert!(reserved("idn/site/events"));
    assert!(!reserved("idn/pd/cmd/plant.scan"));
    assert!(!reserved("idn/pd/res/abc"));
    assert!(!reserved("app/anything"));
    // IDN Media's: only the Site Manager's own subtree.
    assert!(Role::Media.reserves("idn/site/devices"));
    // IDN Voice's commands are open to clients with a login (cues, an operations system).
    assert!(!Role::Media.reserves("idn/site/voice/cmd/message"));
    assert!(!Role::Site.reserves("idn/site/voice/cmd/message"));
    assert!(Role::Site.reserves("idn/site/voice/presence"));
    assert!(Role::Media.reserves("idn/site"));
    assert!(!Role::Media.reserves("idn/sanctuary/switcher/program"));
    assert!(!Role::Media.reserves("idn/pd/status"));
    assert!(!Role::Local.reserves("idn/site/devices"));
}
