//! Core types for MetalBond protocol.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;

use crate::proto;

/// Virtual Network Identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Vni(pub u32);

impl From<u32> for Vni {
    fn from(v: u32) -> Self {
        Vni(v)
    }
}

impl From<Vni> for u32 {
    fn from(v: Vni) -> Self {
        v.0
    }
}

impl std::fmt::Display for Vni {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// IP version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpVersion {
    V4,
    V6,
}

impl From<proto::IpVersion> for IpVersion {
    fn from(v: proto::IpVersion) -> Self {
        match v {
            proto::IpVersion::IPv4 => IpVersion::V4,
            proto::IpVersion::IPv6 => IpVersion::V6,
        }
    }
}

impl From<IpVersion> for proto::IpVersion {
    fn from(v: IpVersion) -> Self {
        match v {
            IpVersion::V4 => proto::IpVersion::IPv4,
            IpVersion::V6 => proto::IpVersion::IPv6,
        }
    }
}

/// Update action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add,
    Remove,
}

impl From<proto::Action> for Action {
    fn from(v: proto::Action) -> Self {
        match v {
            proto::Action::Add => Action::Add,
            proto::Action::Remove => Action::Remove,
        }
    }
}

impl From<Action> for proto::Action {
    fn from(v: Action) -> Self {
        match v {
            Action::Add => proto::Action::Add,
            Action::Remove => proto::Action::Remove,
        }
    }
}

/// Next hop type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NextHopType {
    /// Direct encapsulation to the target address.
    #[default]
    Standard,
    /// NAT at the endpoint.
    Nat,
    /// Load balancer backend.
    LoadBalancerTarget,
}

impl From<proto::NextHopType> for NextHopType {
    fn from(v: proto::NextHopType) -> Self {
        match v {
            proto::NextHopType::Standard => NextHopType::Standard,
            proto::NextHopType::Nat => NextHopType::Nat,
            proto::NextHopType::LoadbalancerTarget => NextHopType::LoadBalancerTarget,
        }
    }
}

impl From<NextHopType> for proto::NextHopType {
    fn from(v: NextHopType) -> Self {
        match v {
            NextHopType::Standard => proto::NextHopType::Standard,
            NextHopType::Nat => proto::NextHopType::Nat,
            NextHopType::LoadBalancerTarget => proto::NextHopType::LoadbalancerTarget,
        }
    }
}

/// Route destination (IP prefix).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Destination {
    /// The IP prefix.
    pub prefix: IpNet,
}

impl Destination {
    /// Creates a new destination from an IP network prefix.
    pub fn new(prefix: IpNet) -> Self {
        Self { prefix }
    }

    /// Creates a destination from an IPv4 address and prefix length.
    pub fn from_ipv4(addr: Ipv4Addr, prefix_len: u8) -> Self {
        Self {
            prefix: IpNet::new(IpAddr::V4(addr), prefix_len).expect("valid prefix"),
        }
    }

    /// Creates a destination from an IPv6 address and prefix length.
    pub fn from_ipv6(addr: Ipv6Addr, prefix_len: u8) -> Self {
        Self {
            prefix: IpNet::new(IpAddr::V6(addr), prefix_len).expect("valid prefix"),
        }
    }

    /// Returns the IP version of this destination.
    pub fn ip_version(&self) -> IpVersion {
        match self.prefix {
            IpNet::V4(_) => IpVersion::V4,
            IpNet::V6(_) => IpVersion::V6,
        }
    }
}

impl std::fmt::Display for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.prefix)
    }
}

impl TryFrom<proto::Destination> for Destination {
    type Error = crate::Error;

    fn try_from(d: proto::Destination) -> Result<Self, Self::Error> {
        let addr = match proto::IpVersion::try_from(d.ip_version) {
            Ok(proto::IpVersion::IPv4) => {
                if d.prefix.len() != 4 {
                    return Err(crate::Error::Protocol("invalid IPv4 address length".into()));
                }
                let bytes: [u8; 4] = d.prefix.try_into().unwrap();
                IpAddr::V4(Ipv4Addr::from(bytes))
            }
            Ok(proto::IpVersion::IPv6) => {
                if d.prefix.len() != 16 {
                    return Err(crate::Error::Protocol("invalid IPv6 address length".into()));
                }
                let bytes: [u8; 16] = d.prefix.try_into().unwrap();
                IpAddr::V6(Ipv6Addr::from(bytes))
            }
            Err(_) => return Err(crate::Error::Protocol("invalid IP version".into())),
        };

        let prefix_len = d.prefix_length as u8;
        let prefix = IpNet::new(addr, prefix_len)
            .map_err(|_| crate::Error::Protocol("invalid prefix length".into()))?;

        Ok(Self { prefix })
    }
}

impl From<&Destination> for proto::Destination {
    fn from(d: &Destination) -> Self {
        let (ip_version, prefix) = match d.prefix {
            IpNet::V4(net) => (proto::IpVersion::IPv4, net.addr().octets().to_vec()),
            IpNet::V6(net) => (proto::IpVersion::IPv6, net.addr().octets().to_vec()),
        };

        proto::Destination {
            ip_version: ip_version as i32,
            prefix,
            prefix_length: d.prefix.prefix_len() as u32,
        }
    }
}

/// Next hop for a route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NextHop {
    /// The target address (IPv6 address of the remote hypervisor).
    pub target_address: Ipv6Addr,
    /// Optional target VNI.
    pub target_vni: Option<u32>,
    /// The type of next hop.
    pub hop_type: NextHopType,
    /// NAT port range start (only for NAT type).
    pub nat_port_range_from: u16,
    /// NAT port range end (only for NAT type).
    pub nat_port_range_to: u16,
}

impl NextHop {
    /// Creates a new standard next hop.
    pub fn standard(target_address: Ipv6Addr) -> Self {
        Self {
            target_address,
            target_vni: None,
            hop_type: NextHopType::Standard,
            nat_port_range_from: 0,
            nat_port_range_to: 0,
        }
    }

    /// Creates a new standard next hop with a target VNI.
    pub fn standard_with_vni(target_address: Ipv6Addr, target_vni: u32) -> Self {
        Self {
            target_address,
            target_vni: Some(target_vni),
            hop_type: NextHopType::Standard,
            nat_port_range_from: 0,
            nat_port_range_to: 0,
        }
    }

    /// Creates a new NAT next hop.
    pub fn nat(target_address: Ipv6Addr, port_from: u16, port_to: u16) -> Self {
        Self {
            target_address,
            target_vni: None,
            hop_type: NextHopType::Nat,
            nat_port_range_from: port_from,
            nat_port_range_to: port_to,
        }
    }

    /// Creates a new load balancer target next hop.
    pub fn load_balancer(target_address: Ipv6Addr) -> Self {
        Self {
            target_address,
            target_vni: None,
            hop_type: NextHopType::LoadBalancerTarget,
            nat_port_range_from: 0,
            nat_port_range_to: 0,
        }
    }
}

impl std::fmt::Display for NextHop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.target_address)?;

        if let Some(vni) = self.target_vni {
            write!(f, " (VNI: {})", vni)?;
        }

        match self.hop_type {
            NextHopType::Standard => {}
            NextHopType::Nat => {
                write!(
                    f,
                    " [NAT ports {}-{}]",
                    self.nat_port_range_from, self.nat_port_range_to
                )?;
            }
            NextHopType::LoadBalancerTarget => {
                write!(f, " [LB]")?;
            }
        }

        Ok(())
    }
}

impl std::fmt::Display for NextHopType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NextHopType::Standard => write!(f, "STANDARD"),
            NextHopType::Nat => write!(f, "NAT"),
            NextHopType::LoadBalancerTarget => write!(f, "LOADBALANCER_TARGET"),
        }
    }
}

impl TryFrom<proto::NextHop> for NextHop {
    type Error = crate::Error;

    fn try_from(nh: proto::NextHop) -> Result<Self, Self::Error> {
        if nh.target_address.len() != 16 {
            return Err(crate::Error::Protocol("invalid target address length".into()));
        }
        let bytes: [u8; 16] = nh.target_address.try_into().unwrap();
        let target_address = Ipv6Addr::from(bytes);

        let hop_type = proto::NextHopType::try_from(nh.r#type)
            .map(NextHopType::from)
            .map_err(|_| crate::Error::Protocol("invalid next hop type".into()))?;

        Ok(Self {
            target_address,
            target_vni: if nh.target_vni != 0 {
                Some(nh.target_vni)
            } else {
                None
            },
            hop_type,
            nat_port_range_from: nh.nat_port_range_from as u16,
            nat_port_range_to: nh.nat_port_range_to as u16,
        })
    }
}

impl From<&NextHop> for proto::NextHop {
    fn from(nh: &NextHop) -> Self {
        proto::NextHop {
            target_address: nh.target_address.octets().to_vec(),
            target_vni: nh.target_vni.unwrap_or(0),
            r#type: proto::NextHopType::from(nh.hop_type) as i32,
            nat_port_range_from: nh.nat_port_range_from as u32,
            nat_port_range_to: nh.nat_port_range_to as u32,
        }
    }
}

/// Connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connecting to the server.
    Connecting,
    /// HELLO message sent, waiting for response.
    HelloSent,
    /// HELLO message received from server.
    HelloReceived,
    /// Connection established and ready for operation.
    Established,
    /// Connection lost, retrying.
    Retry,
    /// Connection closed.
    Closed,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Connecting => write!(f, "CONNECTING"),
            ConnectionState::HelloSent => write!(f, "HELLO_SENT"),
            ConnectionState::HelloReceived => write!(f, "HELLO_RECEIVED"),
            ConnectionState::Established => write!(f, "ESTABLISHED"),
            ConnectionState::Retry => write!(f, "RETRY"),
            ConnectionState::Closed => write!(f, "CLOSED"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // Vni tests
    #[test]
    fn test_vni_from_u32() {
        let vni: Vni = 100u32.into();
        assert_eq!(vni.0, 100);
    }

    #[test]
    fn test_vni_into_u32() {
        let vni = Vni(200);
        let val: u32 = vni.into();
        assert_eq!(val, 200);
    }

    #[test]
    fn test_vni_display() {
        let vni = Vni(12345);
        assert_eq!(format!("{}", vni), "12345");
    }

    #[test]
    fn test_vni_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Vni(100));
        set.insert(Vni(100));
        set.insert(Vni(200));
        assert_eq!(set.len(), 2);
    }

    // IpVersion tests
    #[test]
    fn test_ip_version_from_proto() {
        assert_eq!(IpVersion::from(proto::IpVersion::IPv4), IpVersion::V4);
        assert_eq!(IpVersion::from(proto::IpVersion::IPv6), IpVersion::V6);
    }

    #[test]
    fn test_ip_version_to_proto() {
        assert_eq!(proto::IpVersion::from(IpVersion::V4), proto::IpVersion::IPv4);
        assert_eq!(proto::IpVersion::from(IpVersion::V6), proto::IpVersion::IPv6);
    }

    // Action tests
    #[test]
    fn test_action_from_proto() {
        assert_eq!(Action::from(proto::Action::Add), Action::Add);
        assert_eq!(Action::from(proto::Action::Remove), Action::Remove);
    }

    #[test]
    fn test_action_to_proto() {
        assert_eq!(proto::Action::from(Action::Add), proto::Action::Add);
        assert_eq!(proto::Action::from(Action::Remove), proto::Action::Remove);
    }

    // NextHopType tests
    #[test]
    fn test_next_hop_type_default() {
        assert_eq!(NextHopType::default(), NextHopType::Standard);
    }

    #[test]
    fn test_next_hop_type_display() {
        assert_eq!(format!("{}", NextHopType::Standard), "STANDARD");
        assert_eq!(format!("{}", NextHopType::Nat), "NAT");
        assert_eq!(
            format!("{}", NextHopType::LoadBalancerTarget),
            "LOADBALANCER_TARGET"
        );
    }

    #[test]
    fn test_next_hop_type_from_proto() {
        assert_eq!(
            NextHopType::from(proto::NextHopType::Standard),
            NextHopType::Standard
        );
        assert_eq!(NextHopType::from(proto::NextHopType::Nat), NextHopType::Nat);
        assert_eq!(
            NextHopType::from(proto::NextHopType::LoadbalancerTarget),
            NextHopType::LoadBalancerTarget
        );
    }

    #[test]
    fn test_next_hop_type_to_proto() {
        assert_eq!(
            proto::NextHopType::from(NextHopType::Standard),
            proto::NextHopType::Standard
        );
        assert_eq!(
            proto::NextHopType::from(NextHopType::Nat),
            proto::NextHopType::Nat
        );
        assert_eq!(
            proto::NextHopType::from(NextHopType::LoadBalancerTarget),
            proto::NextHopType::LoadbalancerTarget
        );
    }

    // Destination tests
    #[test]
    fn test_destination_from_ipv4() {
        let dest = Destination::from_ipv4(Ipv4Addr::new(10, 0, 1, 0), 24);
        assert_eq!(dest.prefix.to_string(), "10.0.1.0/24");
        assert_eq!(dest.ip_version(), IpVersion::V4);
    }

    #[test]
    fn test_destination_from_ipv6() {
        let dest = Destination::from_ipv6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 64);
        assert_eq!(dest.prefix.to_string(), "2001:db8::/64");
        assert_eq!(dest.ip_version(), IpVersion::V6);
    }

    #[test]
    fn test_destination_display() {
        let dest = Destination::from_ipv4(Ipv4Addr::new(192, 168, 1, 0), 24);
        assert_eq!(format!("{}", dest), "192.168.1.0/24");
    }

    #[test]
    fn test_destination_new() {
        let prefix: ipnet::IpNet = "172.16.0.0/16".parse().unwrap();
        let dest = Destination::new(prefix);
        assert_eq!(dest.prefix.to_string(), "172.16.0.0/16");
    }

    #[test]
    fn test_destination_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Destination::from_ipv4(Ipv4Addr::new(10, 0, 0, 0), 8));
        set.insert(Destination::from_ipv4(Ipv4Addr::new(10, 0, 0, 0), 8));
        set.insert(Destination::from_ipv4(Ipv4Addr::new(10, 0, 0, 0), 16));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_destination_to_proto_ipv4() {
        let dest = Destination::from_ipv4(Ipv4Addr::new(10, 0, 1, 0), 24);
        let proto_dest = proto::Destination::from(&dest);
        assert_eq!(proto_dest.ip_version, proto::IpVersion::IPv4 as i32);
        assert_eq!(proto_dest.prefix, vec![10, 0, 1, 0]);
        assert_eq!(proto_dest.prefix_length, 24);
    }

    #[test]
    fn test_destination_to_proto_ipv6() {
        let dest = Destination::from_ipv6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 128);
        let proto_dest = proto::Destination::from(&dest);
        assert_eq!(proto_dest.ip_version, proto::IpVersion::IPv6 as i32);
        assert_eq!(proto_dest.prefix.len(), 16);
        assert_eq!(proto_dest.prefix_length, 128);
    }

    #[test]
    fn test_destination_from_proto_ipv4() {
        let proto_dest = proto::Destination {
            ip_version: proto::IpVersion::IPv4 as i32,
            prefix: vec![192, 168, 0, 0],
            prefix_length: 16,
        };
        let dest = Destination::try_from(proto_dest).unwrap();
        assert_eq!(dest.prefix.to_string(), "192.168.0.0/16");
    }

    #[test]
    fn test_destination_from_proto_ipv6() {
        let proto_dest = proto::Destination {
            ip_version: proto::IpVersion::IPv6 as i32,
            prefix: vec![0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            prefix_length: 32,
        };
        let dest = Destination::try_from(proto_dest).unwrap();
        assert_eq!(dest.prefix.to_string(), "2001:db8::/32");
    }

    #[test]
    fn test_destination_from_proto_invalid_ipv4_length() {
        let proto_dest = proto::Destination {
            ip_version: proto::IpVersion::IPv4 as i32,
            prefix: vec![10, 0, 1], // Only 3 bytes
            prefix_length: 24,
        };
        assert!(Destination::try_from(proto_dest).is_err());
    }

    #[test]
    fn test_destination_from_proto_invalid_ipv6_length() {
        let proto_dest = proto::Destination {
            ip_version: proto::IpVersion::IPv6 as i32,
            prefix: vec![0x20, 0x01, 0x0d, 0xb8], // Only 4 bytes
            prefix_length: 32,
        };
        assert!(Destination::try_from(proto_dest).is_err());
    }

    // NextHop tests
    #[test]
    fn test_next_hop_standard() {
        let addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let hop = NextHop::standard(addr);
        assert_eq!(hop.target_address, addr);
        assert_eq!(hop.target_vni, None);
        assert_eq!(hop.hop_type, NextHopType::Standard);
        assert_eq!(hop.nat_port_range_from, 0);
        assert_eq!(hop.nat_port_range_to, 0);
    }

    #[test]
    fn test_next_hop_standard_with_vni() {
        let addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);
        let hop = NextHop::standard_with_vni(addr, 200);
        assert_eq!(hop.target_address, addr);
        assert_eq!(hop.target_vni, Some(200));
        assert_eq!(hop.hop_type, NextHopType::Standard);
    }

    #[test]
    fn test_next_hop_nat() {
        let addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 3);
        let hop = NextHop::nat(addr, 30000, 40000);
        assert_eq!(hop.target_address, addr);
        assert_eq!(hop.hop_type, NextHopType::Nat);
        assert_eq!(hop.nat_port_range_from, 30000);
        assert_eq!(hop.nat_port_range_to, 40000);
    }

    #[test]
    fn test_next_hop_load_balancer() {
        let addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 4);
        let hop = NextHop::load_balancer(addr);
        assert_eq!(hop.target_address, addr);
        assert_eq!(hop.hop_type, NextHopType::LoadBalancerTarget);
    }

    #[test]
    fn test_next_hop_display_standard() {
        let hop = NextHop::standard(Ipv6Addr::LOCALHOST);
        assert_eq!(format!("{}", hop), "::1");
    }

    #[test]
    fn test_next_hop_display_with_vni() {
        let hop = NextHop::standard_with_vni(Ipv6Addr::LOCALHOST, 100);
        assert_eq!(format!("{}", hop), "::1 (VNI: 100)");
    }

    #[test]
    fn test_next_hop_display_nat() {
        let hop = NextHop::nat(Ipv6Addr::LOCALHOST, 1000, 2000);
        assert_eq!(format!("{}", hop), "::1 [NAT ports 1000-2000]");
    }

    #[test]
    fn test_next_hop_display_lb() {
        let hop = NextHop::load_balancer(Ipv6Addr::LOCALHOST);
        assert_eq!(format!("{}", hop), "::1 [LB]");
    }

    #[test]
    fn test_next_hop_to_proto() {
        let hop = NextHop::nat(
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 5),
            10000,
            20000,
        );
        let proto_hop = proto::NextHop::from(&hop);
        assert_eq!(proto_hop.target_address.len(), 16);
        assert_eq!(proto_hop.r#type, proto::NextHopType::Nat as i32);
        assert_eq!(proto_hop.nat_port_range_from, 10000);
        assert_eq!(proto_hop.nat_port_range_to, 20000);
    }

    #[test]
    fn test_next_hop_from_proto() {
        let proto_hop = proto::NextHop {
            target_address: vec![0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6],
            target_vni: 300,
            r#type: proto::NextHopType::Standard as i32,
            nat_port_range_from: 0,
            nat_port_range_to: 0,
        };
        let hop = NextHop::try_from(proto_hop).unwrap();
        assert_eq!(
            hop.target_address,
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 6)
        );
        assert_eq!(hop.target_vni, Some(300));
        assert_eq!(hop.hop_type, NextHopType::Standard);
    }

    #[test]
    fn test_next_hop_from_proto_no_vni() {
        let proto_hop = proto::NextHop {
            target_address: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            target_vni: 0,
            r#type: proto::NextHopType::Standard as i32,
            nat_port_range_from: 0,
            nat_port_range_to: 0,
        };
        let hop = NextHop::try_from(proto_hop).unwrap();
        assert_eq!(hop.target_vni, None);
    }

    #[test]
    fn test_next_hop_from_proto_invalid_address() {
        let proto_hop = proto::NextHop {
            target_address: vec![1, 2, 3], // Invalid length
            target_vni: 0,
            r#type: proto::NextHopType::Standard as i32,
            nat_port_range_from: 0,
            nat_port_range_to: 0,
        };
        assert!(NextHop::try_from(proto_hop).is_err());
    }

    #[test]
    fn test_next_hop_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let hop1 = NextHop::standard(Ipv6Addr::LOCALHOST);
        let hop2 = NextHop::standard(Ipv6Addr::LOCALHOST);
        let hop3 = NextHop::nat(Ipv6Addr::LOCALHOST, 1000, 2000);
        set.insert(hop1);
        set.insert(hop2);
        set.insert(hop3);
        assert_eq!(set.len(), 2);
    }

    // ConnectionState tests
    #[test]
    fn test_connection_state_display() {
        assert_eq!(format!("{}", ConnectionState::Connecting), "CONNECTING");
        assert_eq!(format!("{}", ConnectionState::HelloSent), "HELLO_SENT");
        assert_eq!(
            format!("{}", ConnectionState::HelloReceived),
            "HELLO_RECEIVED"
        );
        assert_eq!(format!("{}", ConnectionState::Established), "ESTABLISHED");
        assert_eq!(format!("{}", ConnectionState::Retry), "RETRY");
        assert_eq!(format!("{}", ConnectionState::Closed), "CLOSED");
    }

    #[test]
    fn test_connection_state_equality() {
        assert_eq!(ConnectionState::Established, ConnectionState::Established);
        assert_ne!(ConnectionState::Connecting, ConnectionState::Established);
    }
}
