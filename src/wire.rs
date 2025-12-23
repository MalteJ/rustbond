//! Wire format handling for MetalBond protocol.
//!
//! MetalBond messages use a 4-byte header followed by a protobuf payload:
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |    Version    |       Message Length          |  Message Type |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                                                               |
//! |                   Protobuf Payload                            |
//! |                   (variable length, max 1188 bytes)           |
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use prost::Message as _;

use crate::proto;
use crate::types::{Action, Destination, NextHop, Vni};
use crate::Error;

/// Protocol version (must be 1).
pub const PROTOCOL_VERSION: u8 = 1;

/// Header size in bytes.
pub const HEADER_SIZE: usize = 4;

/// Maximum payload size (to avoid IPv6 fragmentation).
pub const MAX_PAYLOAD_SIZE: usize = 1188;

/// Message type identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Hello = 1,
    Keepalive = 2,
    Subscribe = 3,
    Unsubscribe = 4,
    Update = 5,
}

impl TryFrom<u8> for MessageType {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(MessageType::Hello),
            2 => Ok(MessageType::Keepalive),
            3 => Ok(MessageType::Subscribe),
            4 => Ok(MessageType::Unsubscribe),
            5 => Ok(MessageType::Update),
            _ => Err(Error::Protocol(format!("unknown message type: {}", value))),
        }
    }
}

/// A MetalBond message.
#[derive(Debug, Clone)]
pub enum Message {
    /// Hello message for connection handshake.
    Hello {
        keepalive_interval: u32,
        is_server: bool,
    },
    /// Keepalive message for liveness detection.
    Keepalive,
    /// Subscribe to a VNI.
    Subscribe { vni: Vni },
    /// Unsubscribe from a VNI.
    Unsubscribe { vni: Vni },
    /// Route update (add or remove).
    Update {
        action: Action,
        vni: Vni,
        destination: Destination,
        next_hop: NextHop,
    },
}

impl Message {
    /// Returns the message type.
    pub fn message_type(&self) -> MessageType {
        match self {
            Message::Hello { .. } => MessageType::Hello,
            Message::Keepalive => MessageType::Keepalive,
            Message::Subscribe { .. } => MessageType::Subscribe,
            Message::Unsubscribe { .. } => MessageType::Unsubscribe,
            Message::Update { .. } => MessageType::Update,
        }
    }

    /// Encodes the message to wire format.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let payload = self.encode_payload()?;

        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(Error::Protocol(format!(
                "message too long: {} bytes > maximum of {} bytes",
                payload.len(),
                MAX_PAYLOAD_SIZE
            )));
        }

        let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());

        // Header: version (1 byte), length (2 bytes big-endian), type (1 byte)
        buf.push(PROTOCOL_VERSION);
        buf.push((payload.len() >> 8) as u8);
        buf.push((payload.len() & 0xFF) as u8);
        buf.push(self.message_type() as u8);

        // Payload
        buf.extend_from_slice(&payload);

        Ok(buf)
    }

    /// Encodes just the protobuf payload.
    fn encode_payload(&self) -> Result<Vec<u8>, Error> {
        match self {
            Message::Hello {
                keepalive_interval,
                is_server,
            } => {
                let msg = proto::Hello {
                    keepalive_interval: *keepalive_interval,
                    is_server: *is_server,
                };
                Ok(msg.encode_to_vec())
            }
            Message::Keepalive => {
                // Keepalive has no payload
                Ok(Vec::new())
            }
            Message::Subscribe { vni } => {
                let msg = proto::Subscription { vni: vni.0 };
                Ok(msg.encode_to_vec())
            }
            Message::Unsubscribe { vni } => {
                let msg = proto::Subscription { vni: vni.0 };
                Ok(msg.encode_to_vec())
            }
            Message::Update {
                action,
                vni,
                destination,
                next_hop,
            } => {
                let msg = proto::Update {
                    action: proto::Action::from(*action) as i32,
                    vni: vni.0,
                    destination: Some(proto::Destination::from(destination)),
                    next_hop: Some(proto::NextHop::from(next_hop)),
                };
                Ok(msg.encode_to_vec())
            }
        }
    }

    /// Decodes a message from wire format.
    ///
    /// Returns the message and the number of bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), Error> {
        if buf.len() < HEADER_SIZE {
            return Err(Error::Protocol("message too short".into()));
        }

        let version = buf[0];
        if version != PROTOCOL_VERSION {
            return Err(Error::Protocol(format!(
                "unsupported protocol version: {}",
                version
            )));
        }

        let payload_len = ((buf[1] as usize) << 8) | (buf[2] as usize);
        let msg_type = MessageType::try_from(buf[3])?;

        let total_len = HEADER_SIZE + payload_len;
        if buf.len() < total_len {
            return Err(Error::Protocol("message truncated".into()));
        }

        let payload = &buf[HEADER_SIZE..total_len];
        let message = Self::decode_payload(msg_type, payload)?;

        Ok((message, total_len))
    }

    /// Decodes just the protobuf payload.
    fn decode_payload(msg_type: MessageType, payload: &[u8]) -> Result<Self, Error> {
        match msg_type {
            MessageType::Hello => {
                let msg = proto::Hello::decode(payload)?;
                Ok(Message::Hello {
                    keepalive_interval: msg.keepalive_interval,
                    is_server: msg.is_server,
                })
            }
            MessageType::Keepalive => Ok(Message::Keepalive),
            MessageType::Subscribe => {
                let msg = proto::Subscription::decode(payload)?;
                Ok(Message::Subscribe {
                    vni: Vni(msg.vni),
                })
            }
            MessageType::Unsubscribe => {
                let msg = proto::Subscription::decode(payload)?;
                Ok(Message::Unsubscribe {
                    vni: Vni(msg.vni),
                })
            }
            MessageType::Update => {
                let msg = proto::Update::decode(payload)?;
                let action = proto::Action::try_from(msg.action)
                    .map(Action::from)
                    .map_err(|_| Error::Protocol("invalid action".into()))?;

                let destination = msg
                    .destination
                    .ok_or_else(|| Error::Protocol("missing destination".into()))?;
                let destination = Destination::try_from(destination)?;

                let next_hop = msg
                    .next_hop
                    .ok_or_else(|| Error::Protocol("missing next_hop".into()))?;
                let next_hop = NextHop::try_from(next_hop)?;

                Ok(Message::Update {
                    action,
                    vni: Vni(msg.vni),
                    destination,
                    next_hop,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn test_hello_roundtrip() {
        let msg = Message::Hello {
            keepalive_interval: 5,
            is_server: false,
        };

        let encoded = msg.encode().unwrap();
        let (decoded, len) = Message::decode(&encoded).unwrap();

        assert_eq!(len, encoded.len());
        match decoded {
            Message::Hello {
                keepalive_interval,
                is_server,
            } => {
                assert_eq!(keepalive_interval, 5);
                assert!(!is_server);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_hello_server() {
        let msg = Message::Hello {
            keepalive_interval: 10,
            is_server: true,
        };

        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Hello {
                keepalive_interval,
                is_server,
            } => {
                assert_eq!(keepalive_interval, 10);
                assert!(is_server);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_keepalive_roundtrip() {
        let msg = Message::Keepalive;

        let encoded = msg.encode().unwrap();
        let (decoded, len) = Message::decode(&encoded).unwrap();

        assert_eq!(len, encoded.len());
        assert!(matches!(decoded, Message::Keepalive));
    }

    #[test]
    fn test_keepalive_header() {
        let msg = Message::Keepalive;
        let encoded = msg.encode().unwrap();

        // Keepalive should have 4-byte header with 0 payload
        assert_eq!(encoded.len(), 4);
        assert_eq!(encoded[0], PROTOCOL_VERSION);
        assert_eq!(encoded[1], 0); // Length high byte
        assert_eq!(encoded[2], 0); // Length low byte
        assert_eq!(encoded[3], MessageType::Keepalive as u8);
    }

    #[test]
    fn test_subscribe_roundtrip() {
        let msg = Message::Subscribe { vni: Vni(100) };

        let encoded = msg.encode().unwrap();
        let (decoded, len) = Message::decode(&encoded).unwrap();

        assert_eq!(len, encoded.len());
        match decoded {
            Message::Subscribe { vni } => assert_eq!(vni.0, 100),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_unsubscribe_roundtrip() {
        let msg = Message::Unsubscribe { vni: Vni(200) };

        let encoded = msg.encode().unwrap();
        let (decoded, len) = Message::decode(&encoded).unwrap();

        assert_eq!(len, encoded.len());
        match decoded {
            Message::Unsubscribe { vni } => assert_eq!(vni.0, 200),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_update_roundtrip() {
        let msg = Message::Update {
            action: Action::Add,
            vni: Vni(100),
            destination: Destination::from_ipv4([10, 0, 1, 0].into(), 24),
            next_hop: NextHop::standard(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0xa)),
        };

        let encoded = msg.encode().unwrap();
        let (decoded, len) = Message::decode(&encoded).unwrap();

        assert_eq!(len, encoded.len());
        match decoded {
            Message::Update {
                action,
                vni,
                destination,
                next_hop,
            } => {
                assert_eq!(action, Action::Add);
                assert_eq!(vni.0, 100);
                assert_eq!(destination.prefix.to_string(), "10.0.1.0/24");
                assert_eq!(
                    next_hop.target_address,
                    Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0xa)
                );
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_update_remove_action() {
        let msg = Message::Update {
            action: Action::Remove,
            vni: Vni(300),
            destination: Destination::from_ipv4([192, 168, 0, 0].into(), 16),
            next_hop: NextHop::standard(Ipv6Addr::LOCALHOST),
        };

        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Update { action, vni, .. } => {
                assert_eq!(action, Action::Remove);
                assert_eq!(vni.0, 300);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_update_with_nat_hop() {
        let msg = Message::Update {
            action: Action::Add,
            vni: Vni(100),
            destination: Destination::from_ipv4([10, 0, 0, 0].into(), 8),
            next_hop: NextHop::nat(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 30000, 40000),
        };

        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Update { next_hop, .. } => {
                assert_eq!(next_hop.hop_type, crate::types::NextHopType::Nat);
                assert_eq!(next_hop.nat_port_range_from, 30000);
                assert_eq!(next_hop.nat_port_range_to, 40000);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_update_with_lb_hop() {
        let msg = Message::Update {
            action: Action::Add,
            vni: Vni(100),
            destination: Destination::from_ipv4([172, 16, 0, 0].into(), 12),
            next_hop: NextHop::load_balancer(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2)),
        };

        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Update { next_hop, .. } => {
                assert_eq!(
                    next_hop.hop_type,
                    crate::types::NextHopType::LoadBalancerTarget
                );
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_update_ipv6_destination() {
        let msg = Message::Update {
            action: Action::Add,
            vni: Vni(100),
            destination: Destination::from_ipv6(
                Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0, 0, 0, 0, 0),
                48,
            ),
            next_hop: NextHop::standard(Ipv6Addr::LOCALHOST),
        };

        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Update { destination, .. } => {
                assert_eq!(destination.prefix.to_string(), "2001:db8:1234::/48");
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_update_with_target_vni() {
        let msg = Message::Update {
            action: Action::Add,
            vni: Vni(100),
            destination: Destination::from_ipv4([10, 0, 0, 0].into(), 8),
            next_hop: NextHop::standard_with_vni(
                Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 3),
                500,
            ),
        };

        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Update { next_hop, .. } => {
                assert_eq!(next_hop.target_vni, Some(500));
            }
            _ => panic!("wrong message type"),
        }
    }

    // Error cases
    #[test]
    fn test_decode_too_short() {
        let buf = vec![1, 0]; // Only 2 bytes
        let result = Message::decode(&buf);
        assert!(result.is_err());
        match result {
            Err(Error::Protocol(msg)) => assert!(msg.contains("too short")),
            _ => panic!("expected Protocol error"),
        }
    }

    #[test]
    fn test_decode_invalid_version() {
        let buf = vec![2, 0, 0, 1]; // Version 2 (invalid)
        let result = Message::decode(&buf);
        assert!(result.is_err());
        match result {
            Err(Error::Protocol(msg)) => assert!(msg.contains("version")),
            _ => panic!("expected Protocol error"),
        }
    }

    #[test]
    fn test_decode_unknown_message_type() {
        let buf = vec![1, 0, 0, 99]; // Message type 99 (invalid)
        let result = Message::decode(&buf);
        assert!(result.is_err());
        match result {
            Err(Error::Protocol(msg)) => assert!(msg.contains("message type")),
            _ => panic!("expected Protocol error"),
        }
    }

    #[test]
    fn test_decode_truncated_payload() {
        // Header says 10 bytes payload, but only 2 bytes provided
        let buf = vec![1, 0, 10, 1, 0, 0];
        let result = Message::decode(&buf);
        assert!(result.is_err());
        match result {
            Err(Error::Protocol(msg)) => assert!(msg.contains("truncated")),
            _ => panic!("expected Protocol error"),
        }
    }

    #[test]
    fn test_message_type_try_from() {
        assert_eq!(MessageType::try_from(1).unwrap(), MessageType::Hello);
        assert_eq!(MessageType::try_from(2).unwrap(), MessageType::Keepalive);
        assert_eq!(MessageType::try_from(3).unwrap(), MessageType::Subscribe);
        assert_eq!(MessageType::try_from(4).unwrap(), MessageType::Unsubscribe);
        assert_eq!(MessageType::try_from(5).unwrap(), MessageType::Update);
        assert!(MessageType::try_from(0).is_err());
        assert!(MessageType::try_from(6).is_err());
        assert!(MessageType::try_from(255).is_err());
    }

    #[test]
    fn test_message_type_method() {
        assert_eq!(
            Message::Hello {
                keepalive_interval: 5,
                is_server: false
            }
            .message_type(),
            MessageType::Hello
        );
        assert_eq!(Message::Keepalive.message_type(), MessageType::Keepalive);
        assert_eq!(
            Message::Subscribe { vni: Vni(1) }.message_type(),
            MessageType::Subscribe
        );
        assert_eq!(
            Message::Unsubscribe { vni: Vni(1) }.message_type(),
            MessageType::Unsubscribe
        );

        let update = Message::Update {
            action: Action::Add,
            vni: Vni(1),
            destination: Destination::from_ipv4([10, 0, 0, 0].into(), 8),
            next_hop: NextHop::standard(Ipv6Addr::LOCALHOST),
        };
        assert_eq!(update.message_type(), MessageType::Update);
    }

    #[test]
    fn test_header_encoding() {
        let msg = Message::Subscribe { vni: Vni(100) };
        let encoded = msg.encode().unwrap();

        // Check header
        assert_eq!(encoded[0], PROTOCOL_VERSION);
        assert_eq!(encoded[3], MessageType::Subscribe as u8);

        // Payload length should be in bytes 1-2 (big endian)
        let payload_len = ((encoded[1] as usize) << 8) | (encoded[2] as usize);
        assert_eq!(payload_len, encoded.len() - HEADER_SIZE);
    }

    #[test]
    fn test_decode_multiple_messages_in_buffer() {
        // Encode two messages
        let msg1 = Message::Keepalive;
        let msg2 = Message::Subscribe { vni: Vni(100) };

        let mut buf = msg1.encode().unwrap();
        buf.extend(msg2.encode().unwrap());

        // Decode first message
        let (decoded1, consumed1) = Message::decode(&buf).unwrap();
        assert!(matches!(decoded1, Message::Keepalive));

        // Decode second message from remaining buffer
        let (decoded2, consumed2) = Message::decode(&buf[consumed1..]).unwrap();
        match decoded2 {
            Message::Subscribe { vni } => assert_eq!(vni.0, 100),
            _ => panic!("wrong message type"),
        }

        assert_eq!(consumed1 + consumed2, buf.len());
    }

    #[test]
    fn test_large_vni_values() {
        // Test with max VNI value
        let msg = Message::Subscribe {
            vni: Vni(u32::MAX),
        };
        let encoded = msg.encode().unwrap();
        let (decoded, _) = Message::decode(&encoded).unwrap();

        match decoded {
            Message::Subscribe { vni } => assert_eq!(vni.0, u32::MAX),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_constants() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(HEADER_SIZE, 4);
        assert_eq!(MAX_PAYLOAD_SIZE, 1188);
    }
}
