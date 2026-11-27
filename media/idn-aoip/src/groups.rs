//! A multicast flow's group checked and held before it sends (§2.6): another group when
//! someone holds it.

use idn_common::sync::LockOk as _;
use idn_aoip_device::server::Server;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// Flow `n`'s group checked on the interface `ip` (moved while another device holds it),
/// held, the flow marked held (announced from then on) and started (`start_tx`).
pub fn claim(
    server: &Server,
    ip: Ipv4Addr,
    n: u16,
    held: &mut BTreeMap<u16, Ipv4Addr>,
    start_tx: &dyn Fn(usize),
) {
    let i = usize::from(n) - 1;
    let Some(mut group) = server.device.lock_ok()
        .tx_flows
        .flows
        .get(i)
        .and_then(|f| f.as_ref())
        .map(|f| f.to.0)
    else {
        return;
    };
    for k in 0..5u32 {
        if !idn_aoip_device::mdns::group_taken(ip, group) {
            break;
        }
        let id = server.device.lock_ok().identity.device_id;
        let seed = u32::from_be_bytes([id[4], id[5], id[6], id[7]])
            ^ (u32::from(n) << 8)
            ^ (k + 1).wrapping_mul(0x9E37_79B9);
        let next = idn_aoip_device::flows::pick_group(seed);
        println!("multicast flow {n}: {group} is held by another device; {next} instead");
        group = next;
        if let Some(f) = server.device.lock_ok()
            .tx_flows
            .flows
            .get_mut(i)
            .and_then(|f| f.as_mut())
        {
            f.to.0 = group;
        }
    }
    server.reserve(group);
    // Checked again with it held (§2.6 step 3; bug run I3-11): two devices that
    // picked one group at once both see the other; this one moves.
    if idn_aoip_device::mdns::group_taken(ip, group) {
        server.release(group);
        let id = server.device.lock_ok().identity.device_id;
        let next = idn_aoip_device::flows::pick_group(
            u32::from_be_bytes([id[4], id[5], id[6], id[7]]) ^ (u32::from(n) << 8) ^ 0x5bd1_e995,
        );
        println!("multicast flow {n}: {group} taken meanwhile; {next} instead");
        group = next;
        if let Some(f) = server.device.lock_ok()
            .tx_flows
            .flows
            .get_mut(i)
            .and_then(|f| f.as_mut())
        {
            f.to.0 = group;
        }
        server.reserve(group);
    }
    held.insert(n, group);
    if let Some(f) = server.device.lock_ok()
        .tx_flows
        .flows
        .get_mut(i)
        .and_then(|f| f.as_mut())
    {
        f.held = true;
    }
    start_tx(i);
    server.announce();
}
