use crate::datastructures::{WireFormat, WireFormatError};

// TODO make them configurable instead of constants
pub(crate) const V2_COMPAT_PRIORITY1: u8 = 128;
pub(crate) const V2_COMPAT_PRIORITY1_PREFERRED: u8 = 64;
pub(crate) const V2_COMPAT_PRIORITY2: u8 = 128;

/// IDN change 4: a v1 clock variance (signed; lower is steadier) as v2's
/// offsetScaledLogVariance (unsigned; lower is steadier), shifted rather than cast, so the
/// order holds (a cast put every negative variance, the usual ones, after every positive
/// one).
pub(crate) fn v1_variance_to_v2(v: i16) -> u16 {
    (v as i32 + 0x8000) as u16
}

/// The reverse of [`v1_variance_to_v2`].
pub(crate) fn v2_variance_to_v1(v: u16) -> i16 {
    (v as i32 - 0x8000) as i16
}

/// IDN change 9: how a v2 grandmaster looks in v1 when a bridge relays it, as v1's order
/// compares it (not preferred first, then stratum, then variance; lower wins each):
/// preferred when its priority1 is under 128 or its class is a locked one (≤ 127),
/// stratum 2 for such a class, else 3 (better than an IDN Node's own 4), its variance
/// kept below the Node's own offer (i16::MAX) so the relayed one always outranks a Node.
pub(crate) fn v1_rank_of_v2(priority_1: u8, quality: crate::datastructures::common::ClockQuality) -> (bool, u8, i16) {
    let locked = quality.clock_class <= 127;
    let preferred = priority_1 < 128 || locked;
    let stratum = if locked { 2 } else { 3 };
    let variance = v2_variance_to_v1(quality.offset_scaled_log_variance).min(i16::MAX - 1);
    (!preferred, stratum, variance)
}

/// The stratum this clock gives itself as a v1 leader (IEEE 1588-2002's default for an
/// ordinary clock without a better reference).
pub(crate) const V1_STRATUM_ORDINARY: u8 = 4;


#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub(crate) struct GrandmasterPropertiesV1 {
    pub(crate) communication_technology: u8,
    pub(crate) clock_uuid: [u8; 6],
    pub(crate) port_id: u16,
    pub(crate) sequence_id: u16,
    pub(crate) clock_stratum: u8,
    pub(crate) clock_identifier: [u8; 4],
    pub(crate) clock_variance: i16,
    pub(crate) preferred: bool,
    pub(crate) is_boundary_clock: bool
}

impl WireFormat for GrandmasterPropertiesV1 {
    /* fn wire_size(&self) -> usize {
        28
    } */
    fn serialize(&self, buffer: &mut [u8]) -> Result<(), WireFormatError> {
        buffer[0] = 0;
        buffer[1] = self.communication_technology;
        buffer[2..8].copy_from_slice(&self.clock_uuid);
        buffer[8..10].copy_from_slice(&self.port_id.to_be_bytes());
        buffer[10..12].copy_from_slice(&self.sequence_id.to_be_bytes());
        buffer[12] = 0;
        buffer[13] = 0;
        buffer[14] = 0;
        buffer[15] = self.clock_stratum;
        buffer[16..20].copy_from_slice(&self.clock_identifier);
        buffer[20] = 0;
        buffer[21] = 0;
        buffer[22..24].copy_from_slice(&self.clock_variance.to_be_bytes());
        buffer[24] = 0;
        buffer[25] = self.preferred as u8;
        buffer[26] = 0;
        buffer[27] = self.is_boundary_clock as u8;
        Ok(())
    }
    fn deserialize(buffer: &[u8]) -> Result<Self, WireFormatError> {
        match buffer.get(0..28) {
            None => Err(WireFormatError::BufferTooShort),
            Some(buf) => Ok(Self {
                communication_technology: buf[1],
                clock_uuid: buf[2..8].try_into().unwrap(),
                port_id: u16::from_be_bytes(buf[8..10].try_into().unwrap()),
                sequence_id: u16::from_be_bytes(buf[10..12].try_into().unwrap()),
                clock_stratum: buf[15],
                clock_identifier: buf[16..20].try_into().unwrap(),
                clock_variance: i16::from_be_bytes(buf[22..24].try_into().unwrap()),
                preferred: buf[25] > 0,
                is_boundary_clock: buf[27] > 0
            })
        }
    }
}

#[cfg(test)]
mod idn_tests {
    use super::*;

    #[test]
    fn v1_variances_keep_their_order() {
        // Lower is steadier in both: a usual negative variance ranks before 0, and the
        // largest (an IDN Node offering itself) after everything.
        assert!(v1_variance_to_v2(-4000) < v1_variance_to_v2(0));
        assert!(v1_variance_to_v2(0) < v1_variance_to_v2(100));
        assert_eq!(v1_variance_to_v2(i16::MAX), u16::MAX);
        for v in [i16::MIN, -4000, 0, 100, i16::MAX] {
            assert_eq!(v2_variance_to_v1(v1_variance_to_v2(v)), v);
        }
    }
}
