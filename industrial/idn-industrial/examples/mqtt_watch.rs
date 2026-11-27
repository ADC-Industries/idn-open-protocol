//! Prints what an IDN Site Manager | Industrial's broker publishes, as an application
//! would see it (for checking a site by hand and in the lab):
//!
//!   cargo run -p idn-industrial --example mqtt_watch -- HOST PORT USER PASSWORD [FILTER] [SECONDS]

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::Packet;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("mqtt_watch HOST PORT USER PASSWORD [FILTER (idn/#)] [SECONDS (10)]");
        std::process::exit(2);
    }
    let mut o = MqttOptions::new(
        format!("watch-{}", std::process::id()),
        &a[0],
        a[1].parse().unwrap_or(1883),
    );
    o.set_credentials(&a[2], &a[3]);
    let filter = a.get(4).cloned().unwrap_or_else(|| "idn/#".into());
    let secs: u64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(10);
    let (c, mut el) = AsyncClient::new(o, 100);
    c.subscribe(filter, QoS::AtLeastOnce).await.unwrap();
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(ev) = tokio::time::timeout_at(end, el.poll()).await {
        match ev {
            Ok(Event::Incoming(Packet::Publish(p))) => println!(
                "{}{} {}",
                String::from_utf8_lossy(&p.topic),
                if p.retain { " (retained)" } else { "" },
                String::from_utf8_lossy(&p.payload)
            ),
            Ok(Event::Incoming(Packet::ConnAck(c))) => eprintln!("connected: {:?}", c.code),
            Ok(_) => {}
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        }
    }
}
