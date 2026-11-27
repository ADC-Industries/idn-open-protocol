//! PTPv1 network messages
pub(crate) use sync_or_delay_req::*;
pub(crate) use delay_resp::*;
pub(crate) use follow_up::*;
pub use header::*;
use crate::PtpInstanceState;

use super::{common::{PortIdentity, WireTimestampV1, GrandmasterPropertiesV1, v2_variance_to_v1, V1_STRATUM_ORDINARY, V2_COMPAT_PRIORITY1_PREFERRED}, datasets::{InternalDefaultDS, InternalParentDS}, messages::EnumConversionError, WireFormatError};
/* use self::{
    management::ManagementMessage,
}; */
/* use super::{
    WireFormatError,
};
use crate::config::ClockIdentity; */
mod sync_or_delay_req;
mod control_field;
use control_field::ControlField;
mod delay_resp;
mod follow_up;
mod header;

/// Maximum length of a packet
///
/// This can be used to preallocate buffers that can always fit packets send by
/// `statime`.
pub const MAX_DATA_LEN: usize = 255;


/// Checks whether message is PTPv1
pub fn is_compatible(buffer: &[u8]) -> bool {
    // this ensures that versionPTP in the header is 1
    (buffer.len() >= 2) && (buffer[0] == 0) && (buffer[1] == 1)
}


/// Type of message, used to differentiate low-delay and other messages.
/// `Event` is low-delay.
/// 
/// To avoid confusion with PTPv2 MessageType which is functionally equivalent
/// to ControlField in PTPv1, PTPv1's messageType is called here PortType
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PortType {
    Event = 0x1,
    General = 0x2,
}
impl TryFrom<u8> for PortType {
    type Error = EnumConversionError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        use PortType::*;
        match value {
            0x1 => Ok(Event),
            0x2 => Ok(General),
            _ => Err(EnumConversionError),
        }
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) header: Header,
    pub(crate) body: MessageBody,
}

impl Message {
    pub(crate) fn is_event(&self) -> bool {
        use MessageBody::*;
        match self.body {
            Sync(_) | DelayReq(_) => true,
            FollowUp(_) | DelayResp(_) /* | Management(_) */ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessageBody {
    Sync(SyncMessage),
    DelayReq(DelayReqMessage),
    FollowUp(FollowUpMessage),
    DelayResp(DelayRespMessage),
    //Management(ManagementMessage), // TODO
}
impl MessageBody {
    fn wire_size(&self) -> usize {
        match &self {
            MessageBody::Sync(m) => m.content_size(),
            MessageBody::DelayReq(m) => m.content_size(),
            MessageBody::FollowUp(m) => m.content_size(),
            MessageBody::DelayResp(m) => m.content_size(),
            /* MessageBody::Management(m) => m.content_size(), */
        }
    }
    fn content_type(&self) -> ControlField {
        match self {
            MessageBody::Sync(_) => ControlField::Sync,
            MessageBody::DelayReq(_) => ControlField::DelayReq,
            MessageBody::FollowUp(_) => ControlField::FollowUp,
            MessageBody::DelayResp(_) => ControlField::DelayResp,
            /* MessageBody::Management(_) => MessageType::Management, */
        }
    }
    pub(crate) fn serialize(&self, buffer: &mut [u8]) -> Result<usize, super::WireFormatError> {
        match &self {
            MessageBody::Sync(m) => m.serialize_content(buffer)?,
            MessageBody::DelayReq(m) => m.serialize_content(buffer)?,
            MessageBody::FollowUp(m) => m.serialize_content(buffer)?,
            MessageBody::DelayResp(m) => m.serialize_content(buffer)?,
            /* MessageBody::Management(m) => m.serialize_content(buffer)?, */
        }
        Ok(self.wire_size())
    }
    pub(crate) fn deserialize(
        message_type: ControlField,
        header: &Header,
        buffer: &[u8],
    ) -> Result<Self, super::WireFormatError> {
        let body = match message_type {
            ControlField::Sync => MessageBody::Sync(SyncMessage::deserialize_content(*header, buffer)?),
            ControlField::DelayReq => {
                MessageBody::DelayReq(DelayReqMessage::deserialize_content(*header, buffer)?)
            }
            ControlField::FollowUp => {
                MessageBody::FollowUp(FollowUpMessage::deserialize_content(buffer)?)
            }
            ControlField::DelayResp => {
                MessageBody::DelayResp(DelayRespMessage::deserialize_content(buffer)?)
            }
            /* ControlField::Management => {
                MessageBody::Management(ManagementMessage::deserialize_content(buffer)?)
            } */
        };
        Ok(body)
    }
}

fn base_header(
    default_ds: &InternalDefaultDS,
    port_identity: PortIdentity,
    sequence_id: u16,
) -> Header {
    Header {
        subdomain: subdomain_of(default_ds.domain_number),
        source_uuid: port_identity.clock_identity.0[0..6].try_into().unwrap(),
        source_port_id: port_identity.port_number,
        sequence_id,
        ..Default::default()
    }
}

/// IDN change 12: the v1 subdomain of a domain number: 0 `_DFLT`, 1-3 `_ALT1`-`_ALT3`
/// (IEEE 1588-2002's names; others `_DFLT`).
pub(crate) fn subdomain_of(domain: u8) -> [u8; 16] {
    let name: &[u8] = match domain {
        1 => b"_ALT1",
        2 => b"_ALT2",
        3 => b"_ALT3",
        _ => b"_DFLT",
    };
    let mut s = [0u8; 16];
    s[..name.len()].copy_from_slice(name);
    s
}

/// IDN change 13: a v1 UUID (a MAC address) from a clock identity: an EUI-64 made from a
/// MAC (ff-fe in its middle) gives that MAC back; others their first six bytes.
pub(crate) fn uuid_of(id: &crate::datastructures::common::ClockIdentity) -> [u8; 6] {
    let b = id.0;
    if b[3] == 0xff && b[4] == 0xfe {
        [b[0], b[1], b[2], b[5], b[6], b[7]]
    } else {
        [b[0], b[1], b[2], b[3], b[4], b[5]]
    }
}

impl Message {
    pub(crate) fn header(&self) -> &Header {
        &self.header
    }
    /// The byte size on the wire of this message
    pub(crate) fn wire_size(&self) -> usize {
        self.header.wire_size() + self.body.wire_size()
    }
    /// Serializes the object into the PTP wire format.
    ///
    /// Returns the used buffer size that contains the message or an error.
    pub(crate) fn serialize(&self, buffer: &mut [u8]) -> Result<usize, super::WireFormatError> {
        let (header, rest) = buffer.split_at_mut(40);
        let (body, _tlv) = rest.split_at_mut(self.body.wire_size());
        self.header
            .serialize_header(
                self.body.content_type(),
                self.body.wire_size(),
                header,
            )
            .unwrap();
        self.body.serialize(body).unwrap();
        Ok(self.wire_size())
    }
    /// Deserializes a message from the PTP wire format.
    ///
    /// Returns the message or an error.
    pub(crate) fn deserialize(buffer: &[u8]) -> Result<Self, super::WireFormatError> {
        let header_data = Header::deserialize_header(buffer)?;
        /* if header_data.message_length < 34 {
            return Err(WireFormatError::Invalid);
        } */
        // Ensure we have the entire message and ignore potential padding
        // Skip the header bytes and only keep the content
        let content_buffer = buffer
            .get(40..buffer.len())
            .ok_or(WireFormatError::BufferTooShort)?;
        let body = MessageBody::deserialize(
            header_data.control,
            &header_data.header,
            content_buffer,
        )?;
        Ok(Message {
            header: header_data.header,
            body,
        })
    }

    /// IDN change 5: the v1 leader's messages (IEEE 1588-2002): a two-step Sync (its
    /// origin timestamp rough, the PTP_ASSIST flag saying a Follow_Up brings the precise
    /// one), the Follow_Up, and the Delay_Resp to a follower's Delay_Req.
    pub(crate) fn sync(
        global: &PtpInstanceState,
        port_identity: PortIdentity,
        sequence_id: u16,
        sync_interval: i8,
        now: crate::time::Time,
    ) -> Self {
        let gm = grandmaster_props(global, sequence_id);
        let leap = global.time_properties_ds.leap_indicator;
        let header = Header {
            ptp_assist: true,
            ptp_boundary_clock: global.default_ds.number_ports > 1,
            // IDN change 13: a leap second announced by the grandmaster passed on.
            leap59: leap == crate::config::LeapIndicator::Leap59,
            leap61: leap == crate::config::LeapIndicator::Leap61,
            ..base_header(&global.default_ds, port_identity, sequence_id)
        };
        let own_variance = v2_variance_to_v1(global.default_ds.clock_quality.offset_scaled_log_variance);
        let parent = global.parent_ds.parent_port_identity;
        Message {
            header,
            body: MessageBody::Sync(SyncMessage {
                header,
                // The rough time it goes (IDN change 13; the Follow_Up has the precise
                // one), not 1970.
                origin_timestamp: now.into(),
                epoch_number: 0,
                current_utc_offset: global.time_properties_ds.current_utc_offset.unwrap_or(0),
                grandmaster: gm,
                sync_interval,
                // This clock's own (IDN change 13), not the grandmaster's.
                local_clock_variance: own_variance,
                local_steps_removed: global.current_ds.steps_removed,
                local_clock_stratum: V1_STRATUM_ORDINARY,
                local_clock_identifier: *b"DFLT",
                parent_communication_technology: 1,
                parent_uuid: uuid_of(&parent.clock_identity),
                parent_port_field: parent.port_number,
                estimated_master_variance: 0,
                estimated_master_drift: 0,
                utc_reasonable: global.time_properties_ds.current_utc_offset.is_some(),
            }),
        }
    }

    /// The Follow_Up to Sync `sequence_id`, with the time it went.
    pub(crate) fn follow_up(domain: u8, port_identity: PortIdentity, sequence_id: u16, sent: crate::time::Time) -> Self {
        let header = Header {
            subdomain: subdomain_of(domain),
            source_uuid: port_identity.clock_identity.0[0..6].try_into().unwrap(),
            source_port_id: port_identity.port_number,
            sequence_id,
            ..Default::default()
        };
        Message {
            header,
            body: MessageBody::FollowUp(FollowUpMessage {
                associated_sequence_id: sequence_id,
                precise_origin_timestamp: sent.into(),
            }),
        }
    }

    /// The Delay_Resp to the Delay_Req `request` (its header), received at `received`.
    pub(crate) fn delay_resp(domain: u8, port_identity: PortIdentity, request: &Header, received: crate::time::Time) -> Self {
        let header = Header {
            subdomain: subdomain_of(domain),
            source_uuid: port_identity.clock_identity.0[0..6].try_into().unwrap(),
            source_port_id: port_identity.port_number,
            sequence_id: request.sequence_id,
            ..Default::default()
        };
        Message {
            header,
            body: MessageBody::DelayResp(DelayRespMessage {
                receive_timestamp: received.into(),
                requesting_source_communication_technology: request.source_communication_technology,
                requesting_source_uuid: request.source_uuid,
                requesting_source_port_id: request.source_port_id,
                requesting_source_sequence_id: request.sequence_id,
            }),
        }
    }

    pub(crate) fn delay_req(
        global: &PtpInstanceState,
        port_identity: PortIdentity,
        sequence_id: u16,
    ) -> Self {
        let header = Header {
            ..base_header(&global.default_ds, port_identity, sequence_id)
        };

        Message {
            header,
            body: MessageBody::DelayReq(DelayReqMessage {
                header,
                origin_timestamp: WireTimestampV1::default(),
                epoch_number: 0, // FIXME
                current_utc_offset: global.time_properties_ds.current_utc_offset.unwrap_or(0),
                // FIXME: what if grandmaster is acquired via PTPv2?
                grandmaster: global.parent_ds.grandmaster_v1.expect("parent_ds.grandmaster_v1 not filled but trying to send DelayReq"),
                sync_interval: 0x7f,
                // TODO really? looks like Dante does it this way but is it correct?
                // IDN change 4: shifted, not cast (u16::MAX went out as -1, a very good variance).
                local_clock_variance: v2_variance_to_v1(global.default_ds.clock_quality.offset_scaled_log_variance),
                local_steps_removed: global.current_ds.steps_removed,
                local_clock_stratum: 255, // TODO but enough for slave-only
                local_clock_identifier: *b"DFLT", // FIXME
                parent_communication_technology: 1,
                parent_uuid: global.parent_ds.parent_port_identity.clock_identity.0[0..6].try_into().unwrap(),
                parent_port_field: global.parent_ds.parent_port_identity.port_number,
                estimated_master_variance: 0,
                estimated_master_drift: 0,
                utc_reasonable: global.time_properties_ds.current_utc_offset.is_some(),
            }),
        }
    }
}

/// The grandmaster as a v1 leader passes it on (IDN change 5): a v1 grandmaster's
/// properties as received; this clock as an ordinary one (stratum 4, `DFLT`, its
/// variance); a v2 grandmaster it follows ranked as v1 would rank it (IDN change 9:
/// preferred and stratum from its priority1 and class, its variance kept above this
/// clock's own offer), with its MAC address as the UUID.
fn grandmaster_props(global: &PtpInstanceState, own_sequence_id: u16) -> GrandmasterPropertiesV1 {
    let p = &global.parent_ds;
    let own = p.grandmaster_identity == global.default_ds.clock_identity;
    if let Some(g) = p.grandmaster_v1 {
        if !own {
            return g;
        }
    }
    if own {
        // IDN change 15: preferred when it is set to lead (priority1 below 128), so v1
        // equipment, which compares the preferred flag first, follows it (IDN bug run J4-1:
        // never preferred, a leader set by hand and a Dante leader both led).
        return GrandmasterPropertiesV1 {
            communication_technology: 1,
            clock_uuid: uuid_of(&p.grandmaster_identity),
            port_id: 1,
            sequence_id: own_sequence_id,
            clock_stratum: V1_STRATUM_ORDINARY,
            clock_identifier: *b"DFLT",
            clock_variance: v2_variance_to_v1(p.grandmaster_clock_quality.offset_scaled_log_variance),
            preferred: global.default_ds.priority_1 < 128,
            is_boundary_clock: false,
        };
    }
    let (not_preferred, stratum, variance) =
        crate::datastructures::common::v1_rank_of_v2(p.grandmaster_priority_1, p.grandmaster_clock_quality);
    GrandmasterPropertiesV1 {
        communication_technology: 1,
        clock_uuid: uuid_of(&p.grandmaster_identity),
        port_id: 1,
        sequence_id: 0,
        clock_stratum: stratum,
        clock_identifier: *b"DFLT",
        clock_variance: variance,
        preferred: !not_preferred,
        is_boundary_clock: false,
    }
}
