//! A small MQTT 5 client for checking IDN's brokers by hand and in the lab:
//!
//!   mqtt sub HOST:PORT FILTER [SECONDS (10)] [USER PASSWORD]   prints what comes
//!   mqtt pub HOST:PORT TOPIC MESSAGE [--retain] [USER PASSWORD]
//!
//! (IDN Media's site broker lets programs on its own machine in without a login.)

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::Packet;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "mqtt sub HOST:PORT FILTER [SECONDS] [USER PASSWORD]\nmqtt pub HOST:PORT TOPIC MESSAGE [--retain] [USER PASSWORD]"
    );
    std::process::exit(2);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        usage();
    }
    let retain = a.iter().any(|x| x == "--retain");
    a.retain(|x| x != "--retain");
    let (host, port) = a[1].rsplit_once(':').unwrap_or((&a[1], "1883"));
    let mut o = MqttOptions::new(
        format!("idn-mqtt-tool-{}", std::process::id()),
        host,
        port.parse().unwrap_or(1883),
    );
    o.set_keep_alive(Duration::from_secs(10));
    let login = |i: usize| {
        a.get(i)
            .zip(a.get(i + 1))
            .map(|(u, p)| (u.clone(), p.clone()))
    };
    match a[0].as_str() {
        "sub" => {
            let secs: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            if let Some((u, p)) = login(4) {
                o.set_credentials(u, p);
            }
            let (c, mut el) = AsyncClient::new(o, 100);
            c.subscribe(a[2].clone(), QoS::AtLeastOnce).await.unwrap();
            let end = tokio::time::Instant::now() + Duration::from_secs(secs);
            while let Ok(ev) = tokio::time::timeout_at(end, el.poll()).await {
                match ev {
                    Ok(Event::Incoming(Packet::Publish(p))) => println!(
                        "{}{} {}",
                        String::from_utf8_lossy(&p.topic),
                        if p.retain { " (retained)" } else { "" },
                        String::from_utf8_lossy(&p.payload)
                    ),
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        "pub" if a.len() >= 4 => {
            if let Some((u, p)) = login(4) {
                o.set_credentials(u, p);
            }
            let (c, mut el) = AsyncClient::new(o, 10);
            c.publish(
                a[2].clone(),
                QoS::AtLeastOnce,
                retain,
                a[3].clone().into_bytes(),
            )
            .await
            .unwrap();
            // Until the broker has it.
            let end = tokio::time::Instant::now() + Duration::from_secs(5);
            while let Ok(ev) = tokio::time::timeout_at(end, el.poll()).await {
                match ev {
                    Ok(Event::Incoming(Packet::PubAck(_))) => return,
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            eprintln!("no answer from the broker");
            std::process::exit(1);
        }
        _ => usage(),
    }
}
