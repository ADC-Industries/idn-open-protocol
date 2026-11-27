//! Renames (§3.8, D6 §3.1, §6): the device and its channels, each name checked against the rules and the others.

use super::*;

/// The name a device rename (0x1001) asks for, if `req` is one with a name: the caller
/// checks it isn't another device's on the network first (bug run K3-2).
pub fn rename_device_to(req: &[u8]) -> Option<String> {
    let h = request_header(req)?;
    (h.opcode == op::RENAME_DEVICE && req.len() > 10)
        .then(|| string_at(req, 10))
        .flatten()
}

/// Whether `req` is a device rename back to the default name (0x1001 without a name).
pub fn is_rename_reset(req: &[u8]) -> bool {
    request_header(req).is_some_and(|h| h.opcode == op::RENAME_DEVICE) && req.len() == 10
}

/// The refusal of `req` (0x0030), when it is a request.
pub fn refusal(req: &[u8]) -> Option<Vec<u8>> {
    request_header(req).map(|h| reply(h, REFUSED, &[]))
}

/// 0x1001 (D6 §3.1): a name after the header, or none (back to the default).
pub(crate) fn rename_device(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let name = if req.len() == 10 {
        None
    } else {
        string_at(req, 10)
    };
    let refuse = Answer {
        reply: Some(reply(h, REFUSED, &[])),
        changes: Vec::new(),
    };
    match &name {
        Some(n) if !device::device_name_ok(n) => return refuse,
        None if req.len() != 10 => return refuse,
        _ => {}
    }
    // No name: back to the one it started with (bug run I1-6: acknowledged, kept).
    dev.identity.name = name
        .clone()
        .unwrap_or_else(|| dev.identity.default_name.clone());
    Answer {
        reply: Some(reply(h, OK, &[])),
        changes: vec![Change::DeviceName(name)],
    }
}

/// The items of a rename request (D6 §6 table): count at +11, items of `size` bytes from
/// +12, the channel at `ch_at` and the name's pointer at `ptr_at` within an item.
pub(crate) fn items(
    req: &[u8],
    size: usize,
    ch_at: usize,
    ptr_at: usize,
) -> Option<Vec<(usize, Option<String>)>> {
    let n = usize::from(*req.get(11)?);
    if n == 0 {
        return None;
    }
    (0..n)
        .map(|i| {
            let at = 12 + size * i;
            let ch = usize::from(u16_at(req, at + ch_at)?);
            let p = usize::from(u16_at(req, at + ptr_at)?);
            // No name: back to the default (D6 §6, "reset a channel name": the same body
            // without the name, so pointer 0, the request's end or an empty string; bug run
            // N5-2: M1-10 refused the end). A pointer past the request refuses it (M1-10).
            let name = if p == 0 || p == req.len() {
                None
            } else if p > req.len() {
                return None;
            } else {
                Some(string_at(req, p)?).filter(|n| !n.is_empty())
            };
            Some((ch, name))
        })
        .collect()
}

/// 0x2013 (§3.8): items {0, channel, pointer}.
pub(crate) fn tx_rename(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let ok = items(req, 6, 2, 4).filter(|v| {
        v.iter().all(|(ch, name)| {
            (1..=dev.tx.len()).contains(ch) && name.as_deref().is_none_or(device::channel_name_ok)
        })
    });
    let Some(v) = ok else {
        return Answer {
            reply: Some(reply(h, REFUSED, &[])),
            changes: Vec::new(),
        };
    };
    // Every output's name stays its own, against the others' names and default names, in
    // any case (bug run K1-1: two `_netaudio-chan` instances of one name, a receiver on
    // the wrong channel).
    let mut names: Vec<String> = dev.tx.iter().map(|c| c.name.clone()).collect();
    for (ch, name) in &v {
        names[ch - 1] = name.clone().unwrap_or_else(|| format!("TX {ch}"));
    }
    // (The channels renamed against all: a clash saved before doesn't block renames.)
    let clash = v.iter().any(|(ch, _)| {
        let (i, a) = (ch - 1, &names[ch - 1]);
        names.iter().enumerate().any(|(j, b)| {
            i != j
                && (a.eq_ignore_ascii_case(b)
                    || a.eq_ignore_ascii_case(&format!("TX {}", j + 1))
                    // and its factory name, announced too ("05": bug run M1-1)
                    || a.eq_ignore_ascii_case(&Device::factory_channel_name(j + 1)))
        })
    });
    if clash {
        return Answer {
            reply: Some(reply(h, REFUSED, &[])),
            changes: Vec::new(),
        };
    }
    let mut changes = Vec::new();
    for (ch, _) in v {
        dev.tx[ch - 1].name = names[ch - 1].clone();
        changes.push(Change::TxName(ch));
    }
    Answer {
        reply: Some(reply(h, OK, &[0, 0])),
        changes,
    }
}

/// 0x3001 (§3.8): items {channel, pointer}.
pub(crate) fn rx_rename(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let ok = items(req, 4, 0, 2).filter(|v| {
        v.iter().all(|(ch, name)| {
            (1..=dev.rx.len()).contains(ch) && name.as_deref().is_none_or(device::channel_name_ok)
        })
    });
    let Some(v) = ok else {
        return Answer {
            reply: Some(reply(h, REFUSED, &[])),
            changes: Vec::new(),
        };
    };
    let mut changes = Vec::new();
    for (ch, name) in v {
        dev.rx[ch - 1].name = name.unwrap_or_else(|| format!("RX {ch}"));
        changes.push(Change::RxName(ch));
    }
    Answer {
        reply: Some(reply(h, OK, &[])),
        changes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reset as D6 §6 sends it (the body without the name: the pointer at the request's
    /// end) resets; pointer 0 too; past the end refuses (bug run N5-2, M1-10).
    #[test]
    fn a_reset_points_at_the_end() {
        let mut req = vec![0u8; 18];
        req[11] = 1;
        req[14..16].copy_from_slice(&2u16.to_be_bytes());
        for (p, want) in [
            (18u16, Some(vec![(2, None)])),
            (0, Some(vec![(2, None)])),
            (19, None),
        ] {
            req[16..18].copy_from_slice(&p.to_be_bytes());
            assert_eq!(items(&req, 6, 2, 4), want, "pointer {p}");
        }
    }
}
