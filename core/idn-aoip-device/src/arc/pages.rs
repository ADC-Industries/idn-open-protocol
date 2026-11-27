//! The channel pages and names a controller reads (§3.5–§3.7, D6 §7.2–§7.4).

use super::*;

/// The common channel descriptor (§3.5), at a 4-byte boundary (netaudio reads it as an
/// availability block too, D6 §7.4).
pub(crate) fn descriptor(b: &mut Body, dev: &Device) -> u16 {
    b.align4();
    let at = b.at();
    let f = dev.format;
    b.u32(f.rate);
    b.u8(1);
    b.u8(1);
    b.u16(f.bits);
    b.u16(0x0400);
    b.u16(f.bits);
    b.u16(f.bits);
    b.u16(f.pcm_type);
    at
}

/// A page from channel `start` of `total` channels, `cap` slots of `size` bytes: the
/// slots filled by `record` (given the body and the channel number), `None` ending the
/// page early; data kept under the soft limit.
pub(crate) fn page(
    total: usize,
    start: u16,
    cap: usize,
    size: usize,
    record: impl FnMut(&mut Body, usize, u16),
) -> (u16, Vec<u8>) {
    page_within(total, start, cap, size, SOFT_LIMIT, record)
}

/// [`page`] with its own soft limit on the data.
pub(crate) fn page_within(
    total: usize,
    start: u16,
    cap: usize,
    size: usize,
    soft: usize,
    mut record: impl FnMut(&mut Body, usize, u16),
) -> (u16, Vec<u8>) {
    let first = usize::from(start);
    let mut b = Body::new();
    // The last page as long as what is left (bug run M1-3: capacity 16 with 8 records,
    // which netaudio 0.3.14 refuses).
    let cap = cap.min(total + 1 - first.clamp(1, total.max(1)));
    b.u8(cap as u8);
    b.u8(0);
    let table = b.at();
    b.zeros(cap * size);
    let mut n = 0;
    let mut ch = first;
    while ch <= total && n < cap {
        if n > 0 && b.0.len() >= soft {
            break;
        }
        record(&mut b, n, table + (n * size) as u16);
        n += 1;
        ch += 1;
    }
    b.0[1] = n as u8;
    let more = ch <= total;
    (if more { MORE } else { OK }, b.0)
}

/// 0x3000 (§3.7): 20-byte records; the status a u32 at +12 (netaudio reads its low half
/// at +14).
pub(crate) fn rx_page(dev: &Device, start: u16) -> (u16, Vec<u8>) {
    if usize::from(start) > dev.rx.len() {
        return (OK, vec![0, 0]);
    }
    let mut desc = None;
    // Every page full, whatever its names' length (bug run N5-9: the 800-byte limit cut a
    // page of long names to 12 records, and netaudio 0.3.14 stops at the first short page;
    // 16 records of 31-character names are about 2 KB, as K1-9's 0x2010).
    page_within(
        dev.rx.len(),
        start,
        RX_PAGE,
        20,
        usize::MAX,
        |b, i, slot| {
            let d = *desc.get_or_insert_with(|| descriptor(b, dev));
            let n = usize::from(start) + i;
            let c = &dev.rx[n - 1];
            let (ch, devname) = match &c.subscription {
                Some(s) => (b.string(&s.channel), b.string(&s.device)),
                None => (0, 0),
            };
            let name = b.string(&c.name);
            b.put_u16(slot, n as u16);
            b.put_u16(slot + 2, 6);
            b.put_u16(slot + 4, d);
            b.put_u16(slot + 6, ch);
            b.put_u16(slot + 8, devname);
            b.put_u16(slot + 10, name);
            b.put_u32(slot + 12, c.status);
        },
    )
}

/// 0x2000 (§3.6): 8-byte records, each with the factory name.
pub(crate) fn tx_page(dev: &Device, start: u16) -> (u16, Vec<u8>) {
    if usize::from(start) > dev.tx.len() {
        return (OK, vec![0, 0]);
    }
    let mut desc = None;
    page(dev.tx.len(), start, TX_PAGE, 8, |b, i, slot| {
        let d = *desc.get_or_insert_with(|| descriptor(b, dev));
        let n = usize::from(start) + i;
        let name = b.string(&Device::factory_channel_name(n));
        b.put_u16(slot, n as u16);
        b.put_u16(slot + 2, 7);
        b.put_u16(slot + 4, d);
        b.put_u16(slot + 6, name);
    })
}

/// 0x2010 (§3.6): 6-byte records {n, n, friendly name}, from `start`.
pub(crate) fn tx_names(dev: &Device, start: u16) -> (u16, Vec<u8>) {
    if usize::from(start) > dev.tx.len() {
        return (OK, vec![0, 0]);
    }
    // A controller asks once for 1..count and takes 32 names at most (D6 §7.3); with more
    // outputs, the 32 given are the renamed ones first (any channel numbers, in order:
    // layout B), the rest by number (lab Alpha84 533-A: outputs past 32 read back bare).
    if start == 1 && dev.tx.len() > TX_PAGE {
        let renamed = |n: usize| dev.tx[n - 1].name != format!("TX {n}");
        let mut pick: Vec<usize> = (1..=dev.tx.len())
            .filter(|&n| renamed(n))
            .take(TX_PAGE)
            .collect();
        for n in 1..=dev.tx.len() {
            if pick.len() >= TX_PAGE {
                break;
            }
            if !renamed(n) {
                pick.push(n);
            }
        }
        pick.sort_unstable();
        let mut b = Body::new();
        b.u8(pick.len() as u8);
        b.u8(pick.len() as u8);
        let table = b.at();
        b.zeros(pick.len() * 6);
        for (i, &n) in pick.iter().enumerate() {
            let name = b.string(&dev.tx[n - 1].name);
            let slot = table + (i * 6) as u16;
            b.put_u16(slot, n as u16);
            b.put_u16(slot + 2, n as u16);
            b.put_u16(slot + 4, name);
        }
        return (OK, b.0);
    }
    // A whole page of names (32 × 31 characters is about 1.2 KB): a controller asks for
    // them once, not page by page (bug run K1-9: long names cut the list at about 19).
    page_within(dev.tx.len(), start, TX_PAGE, 6, usize::MAX, |b, i, slot| {
        let n = usize::from(start) + i;
        let name = b.string(&dev.tx[n - 1].name);
        b.put_u16(slot, n as u16);
        b.put_u16(slot + 2, n as u16);
        b.put_u16(slot + 4, name);
    })
}
