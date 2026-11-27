//! Subscriptions set and cleared by a controller (§3.9, D6 §6.1).

use super::*;

/// 0x3010 (§3.9, D6 §6.1.1): records {0, RX channel (u8), channel pointer, device
/// pointer}; both pointers set subscribes, either 0 unsubscribes. All or nothing.
pub(crate) fn subscribe(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let refuse = Answer {
        reply: Some(reply(h, REFUSED, &[])),
        changes: Vec::new(),
    };
    let n = usize::from(*req.get(11).unwrap_or(&0));
    if n == 0 {
        return refuse;
    }
    let mut set = Vec::with_capacity(n);
    for i in 0..n {
        let at = 12 + 6 * i;
        let (Some(ch), Some(cp), Some(dp)) =
            (u16_at(req, at), u16_at(req, at + 2), u16_at(req, at + 4))
        else {
            return refuse;
        };
        let ch = usize::from(ch);
        if !(1..=dev.rx.len()).contains(&ch) {
            return refuse;
        }
        let sub = if cp == 0 || dp == 0 {
            None
        } else {
            match (
                string_at(req, usize::from(cp)),
                string_at(req, usize::from(dp)),
            ) {
                (Some(c), Some(d))
                    if device::channel_name_ok(&c) && (d == "." || device::device_name_ok(&d)) =>
                {
                    // "." is this device itself (D6 §6.1).
                    let d = if d == "." {
                        dev.identity.name.clone()
                    } else {
                        d
                    };
                    Some(Subscription {
                        channel: c,
                        device: d,
                    })
                }
                _ => return refuse,
            }
        };
        set.push((ch, sub));
    }
    let mut changes = Vec::new();
    for (ch, sub) in set {
        let c = &mut dev.rx[ch - 1];
        c.status = if sub.is_some() {
            status::UNRESOLVED
        } else {
            status::NONE
        };
        c.subscription = sub;
        changes.push(Change::Subscription(ch));
    }
    Answer {
        reply: Some(reply(h, OK, &[])),
        changes,
    }
}

/// 0x3014 (D6 §6.1.2): a u32 count at +8, then u32 channels; a channel the device
/// doesn't have is refused.
pub(crate) fn unsubscribe(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let refuse = Answer {
        reply: Some(reply(h, REFUSED, &[])),
        changes: Vec::new(),
    };
    let n = u32_at(req, 8).unwrap_or(0) as usize;
    if n == 0 || req.len() != 12 + 4 * n {
        return refuse;
    }
    let chans: Vec<usize> = (0..n)
        .map(|i| u32_at(req, 12 + 4 * i).unwrap_or(0) as usize)
        .collect();
    if chans.iter().any(|ch| !(1..=dev.rx.len()).contains(ch)) {
        return refuse;
    }
    let mut changes = Vec::new();
    for ch in chans {
        let c = &mut dev.rx[ch - 1];
        c.subscription = None;
        c.status = status::NONE;
        changes.push(Change::Subscription(ch));
    }
    Answer {
        reply: Some(reply(h, OK, &[])),
        changes,
    }
}
