//! A Node's local MQTT endpoint (§I.7.4): a broker on loopback (and, with a login, on the
//! plant's network) for the machine's programs and devices, bridged over the link: what
//! they publish goes up (`idn/<machine>/local/<topic>` at the Site Manager), and what the
//! application publishes to `idn/<machine>/to/<topic>` comes down and is published here
//! as `<topic>`. Up, it goes through the outbox: kept on disk while the link is down.

use crate::modules::MqttConfig;
use crate::mqtt::{Broker, Hook, Role, bridge_decode, bridge_encode};
use crate::node::IndustrialNode;
use std::io;
use std::path::Path;

/// Starts the endpoint; where it listens.
pub fn start(node: &IndustrialNode, state: &Path, cfg: &MqttConfig) -> io::Result<String> {
    let rt = crate::node::runtime();
    let _g = rt.enter();
    let (broker, mut hooks) = Broker::open_as(&state.join("mqtt.redb"), Role::Local)?;
    let loopback = cfg
        .loopback
        .clone()
        .unwrap_or_else(|| "127.0.0.1:1883".into());
    let l = rt.block_on(tokio::net::TcpListener::bind(&loopback))?;
    // Everything that can fail before anything runs (bug run B: the loopback endpoint was
    // up and acknowledging messages nobody took on when the plant's then failed).
    let plant = match &cfg.listen {
        Some(addr) => {
            let (Some(u), Some(p)) = (&cfg.username, &cfg.password) else {
                return Err(io::Error::other(
                    "[mqtt] listen needs a username and a password",
                ));
            };
            broker.set_user(u, p)?;
            Some(rt.block_on(tokio::net::TcpListener::bind(addr))?)
        }
        None => None,
    };
    let b = broker.clone();
    rt.spawn(async move {
        // One failed accept (a client reset before it was taken, too many open
        // files) isn't the end of the broker: it stopped for good, the panel still
        // saying it listened (deep bug run 2026-11-09).
        loop {
            let (s, from) = match l.accept().await {
                Ok(x) => x,
                Err(e) => {
                    idn_open_protocol::log::log(&format!("mqtt: a connection not taken: {e}"));
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
            };
            let b = b.clone();
            tokio::spawn(async move { b.serve_open(s, from.to_string()).await });
        }
    });
    // On the plant's network: with a login only.
    if let Some(l) = plant {
        let b = broker.clone();
        rt.spawn(async move {
            // One failed accept (a client reset before it was taken, too many open
            // files) isn't the end of the broker: it stopped for good, the panel still
            // saying it listened (deep bug run 2026-11-09).
            loop {
                let (s, from) = match l.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        idn_open_protocol::log::log(&format!("mqtt: a connection not taken: {e}"));
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let b = b.clone();
                tokio::spawn(async move { b.serve(s, from.to_string()).await });
            }
        });
    }
    let filters = if cfg.up.is_empty() {
        vec!["#".to_owned()]
    } else {
        cfg.up.clone()
    };
    let up = node.clone();
    rt.spawn(async move {
        while let Some(h) = hooks.recv().await {
            if let Hook::Local { msg: m, saved } = h {
                if !filters
                    .iter()
                    .any(|f| rumqttc::v5::mqttbytes::matches(&m.topic, f))
                {
                    continue; // (not going up: acked as before)
                }
                let n = up.clone();
                let payload = bridge_encode(&m);
                // The device's PUBACK once the outbox (on disk) has it; one it can't take
                // is an error to the device and said in the log (deep bug run 2026-11-09).
                let topic = m.topic.clone();
                let r = tokio::task::spawn_blocking(move || n.send("idn.mqtt", payload)).await;
                saved.done(matches!(r, Ok(Ok(_))));
                match r {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        idn_open_protocol::log::log(&format!("mqtt: {topic} not sent up: {e}"))
                    }
                    Err(e) => {
                        idn_open_protocol::log::log(&format!("mqtt: {topic} not sent up: {e}"))
                    }
                }
            }
        }
    });
    node.set_downlink(move |it| {
        it.kind == "idn.mqtt"
            && bridge_decode(&it.payload).is_some_and(|m| broker.publish(&m).is_ok())
    });
    Ok(format!(
        "{loopback}{}",
        cfg.listen
            .as_ref()
            .map_or(String::new(), |l| format!(" and {l} (login)"))
    ))
}
