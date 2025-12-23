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

/// Maximum total message size (header + payload).
pub const MAX_MESSAGE_SIZE: usize = HEADER_SIZE + MAX_PAYLOAD_SIZE;

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
    fn test_keepalive_roundtrip() {
        let msg = Message::Keepalive;

        let encoded = msg.encode().unwrap();
        let (decoded, len) = Message::decode(&encoded).unwrap();

        assert_eq!(len, encoded.len());
        assert!(matches!(decoded, Message::Keepalive));
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
}
