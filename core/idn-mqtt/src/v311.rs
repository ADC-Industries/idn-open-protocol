//! MQTT 3.1.1 clients (protocol level 4; Home Assistant's, Node-RED's and mqtt.js's
//! default), spoken by translating: their packets are read into the MQTT 5 ones the
//! broker handles, and its MQTT 5 packets written to them as 3.1.1. What 3.1.1 hasn't got
//! is left out: properties (user properties, response topics, correlation, expiry), reason
//! codes beyond its own, and a server's DISCONNECT (it closes instead).

use crate::err;
use bytes::{Bytes, BytesMut};
use rumqttc::mqttbytes as m4;
use rumqttc::mqttbytes::v4 as v4;
use rumqttc::v5::mqttbytes::v5 as v5;
use rumqttc::v5::mqttbytes::QoS;
use std::io;

fn qos_in(q: m4::QoS) -> QoS {
    match q {
        m4::QoS::AtMostOnce => QoS::AtMostOnce,
        m4::QoS::AtLeastOnce => QoS::AtLeastOnce,
        m4::QoS::ExactlyOnce => QoS::ExactlyOnce,
    }
}

fn qos_out(q: QoS) -> m4::QoS {
    match q {
        QoS::AtMostOnce => m4::QoS::AtMostOnce,
        QoS::AtLeastOnce => m4::QoS::AtLeastOnce,
        QoS::ExactlyOnce => m4::QoS::ExactlyOnce,
    }
}

/// Whether `buf` (a whole first packet or its start) is a 3.1.1 CONNECT: the protocol
/// name "MQTT" with level 4. (Level 5 and anything else go to the MQTT 5 reader.)
pub(crate) fn is_v311_connect(buf: &[u8]) -> bool {
    if buf.first().is_none_or(|b| b >> 4 != 1) {
        return false;
    }
    // The remaining length: one to four bytes.
    let mut i = 1;
    while i < buf.len() && i < 5 && buf[i] & 0x80 != 0 {
        i += 1;
    }
    let at = i + 1;
    buf.get(at..at + 7)
        .is_some_and(|h| h[..6] == [0, 4, b'M', b'Q', b'T', b'T'] && h[6] == 4)
}

/// One 3.1.1 packet from `buf`, as the MQTT 5 packet it means. `Ok(None)`: more bytes
/// needed.
pub(crate) fn read(buf: &mut BytesMut, max: usize) -> io::Result<Option<v5::Packet>> {
    let p = match v4::Packet::read(buf, max) {
        Ok(p) => p,
        Err(m4::Error::InsufficientBytes(_)) => return Ok(None),
        Err(e) => return Err(err(e)),
    };
    Ok(Some(match p {
        v4::Packet::Connect(c) => {
            let will = c.last_will.map(|w| v5::LastWill {
                topic: Bytes::from(w.topic.into_bytes()),
                message: w.message,
                qos: qos_in(w.qos),
                retain: w.retain,
                properties: None,
            });
            let login = c.login.map(|l| v5::Login {
                username: l.username,
                password: l.password,
            });
            // A clean session ends with its connection (3.1.1), else it is kept: as
            // MQTT 5's session expiry, the longest.
            let properties = (!c.clean_session).then(|| {
                let mut p = v5::ConnectProperties::new();
                p.session_expiry_interval = Some(u32::MAX);
                p
            });
            v5::Packet::Connect(
                v5::Connect {
                    keep_alive: c.keep_alive,
                    client_id: c.client_id,
                    clean_start: c.clean_session,
                    properties,
                },
                will,
                login,
            )
        }
        v4::Packet::Publish(p) => v5::Packet::Publish(v5::Publish {
            dup: p.dup,
            qos: qos_in(p.qos),
            retain: p.retain,
            topic: Bytes::from(p.topic.into_bytes()),
            pkid: p.pkid,
            payload: p.payload,
            properties: None,
        }),
        v4::Packet::PubAck(a) => v5::Packet::PubAck(v5::PubAck {
            pkid: a.pkid,
            reason: v5::PubAckReason::Success,
            properties: None,
        }),
        v4::Packet::Subscribe(s) => v5::Packet::Subscribe(v5::Subscribe {
            pkid: s.pkid,
            filters: s
                .filters
                .into_iter()
                .map(|f| v5::Filter {
                    path: f.path,
                    qos: qos_in(f.qos),
                    nolocal: false,
                    preserve_retain: false,
                    retain_forward_rule: v5::RetainForwardRule::OnEverySubscribe,
                })
                .collect(),
            properties: None,
        }),
        v4::Packet::Unsubscribe(u) => v5::Packet::Unsubscribe(v5::Unsubscribe {
            pkid: u.pkid,
            filters: u.topics,
            properties: None,
        }),
        v4::Packet::PingReq => v5::Packet::PingReq(v5::PingReq),
        v4::Packet::Disconnect => v5::Packet::Disconnect(v5::Disconnect {
            reason_code: v5::DisconnectReasonCode::NormalDisconnection,
            properties: None,
        }),
        // QoS 2's packets and anything a client doesn't send: as MQTT 5 would read them
        // (the broker refuses them the same way).
        v4::Packet::PubRec(r) => v5::Packet::PubRec(v5::PubRec {
            pkid: r.pkid,
            reason: v5::PubRecReason::Success,
            properties: None,
        }),
        v4::Packet::PubRel(r) => v5::Packet::PubRel(v5::PubRel {
            pkid: r.pkid,
            reason: v5::PubRelReason::Success,
            properties: None,
        }),
        v4::Packet::PubComp(c) => v5::Packet::PubComp(v5::PubComp {
            pkid: c.pkid,
            reason: v5::PubCompReason::Success,
            properties: None,
        }),
        other => return Err(err(format!("a client doesn't send {other:?}"))),
    }))
}

/// The broker's MQTT 5 packet `p` written for a 3.1.1 client into `b`. False: nothing to
/// write (a server's DISCONNECT, which 3.1.1 hasn't got: the connection just closes).
pub(crate) fn write(p: &v5::Packet, b: &mut BytesMut, max: usize) -> io::Result<bool> {
    let p4 = match p {
        v5::Packet::ConnAck(a) => v4::Packet::ConnAck(v4::ConnAck {
            session_present: a.session_present,
            code: match a.code {
                v5::ConnectReturnCode::Success => v4::ConnectReturnCode::Success,
                v5::ConnectReturnCode::BadUserNamePassword => {
                    v4::ConnectReturnCode::BadUserNamePassword
                }
                v5::ConnectReturnCode::NotAuthorized => v4::ConnectReturnCode::NotAuthorized,
                v5::ConnectReturnCode::ClientIdentifierNotValid => {
                    v4::ConnectReturnCode::BadClientId
                }
                v5::ConnectReturnCode::UnsupportedProtocolVersion => {
                    v4::ConnectReturnCode::RefusedProtocolVersion
                }
                _ => v4::ConnectReturnCode::ServiceUnavailable,
            },
        }),
        v5::Packet::Publish(p) => v4::Packet::Publish(v4::Publish {
            dup: p.dup,
            qos: qos_out(p.qos),
            retain: p.retain,
            topic: String::from_utf8_lossy(&p.topic).into_owned(),
            pkid: p.pkid,
            payload: p.payload.clone(),
        }),
        v5::Packet::PubAck(a) => v4::Packet::PubAck(v4::PubAck { pkid: a.pkid }),
        v5::Packet::SubAck(s) => v4::Packet::SubAck(v4::SubAck {
            pkid: s.pkid,
            return_codes: s
                .return_codes
                .iter()
                .map(|c| match c {
                    v5::SubscribeReasonCode::Success(q) => {
                        v4::SubscribeReasonCode::Success(qos_out(*q))
                    }
                    _ => v4::SubscribeReasonCode::Failure,
                })
                .collect(),
        }),
        v5::Packet::UnsubAck(u) => v4::Packet::UnsubAck(v4::UnsubAck { pkid: u.pkid }),
        v5::Packet::PingResp(_) => v4::Packet::PingResp,
        v5::Packet::Disconnect(_) => return Ok(false),
        other => return Err(err(format!("not for a 3.1.1 client: {other:?}"))),
    };
    p4.write(b, max).map_err(err)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v311_connect_reads_as_mqtt_5_and_answers_go_back_as_v311() {
        let mut b = BytesMut::new();
        let c = v4::Connect {
            protocol: m4::Protocol::V4,
            keep_alive: 30,
            client_id: "ha".into(),
            clean_session: true,
            last_will: None,
            login: Some(v4::Login {
                username: "u".into(),
                password: "p".into(),
            }),
        };
        v4::Packet::Connect(c).write(&mut b, 1 << 20).unwrap();
        assert!(is_v311_connect(&b));
        let p = read(&mut b, 1 << 20).unwrap().unwrap();
        let v5::Packet::Connect(c, _, Some(l)) = p else {
            panic!("{p:?}")
        };
        assert_eq!((c.client_id.as_str(), c.keep_alive), ("ha", 30));
        assert_eq!((l.username.as_str(), l.password.as_str()), ("u", "p"));
        let mut out = BytesMut::new();
        let ack = v5::Packet::ConnAck(v5::ConnAck {
            session_present: false,
            code: v5::ConnectReturnCode::Success,
            properties: None,
        });
        assert!(write(&ack, &mut out, 1 << 20).unwrap());
        assert_eq!(&out[..], &[0x20, 2, 0, 0]);
    }
}
