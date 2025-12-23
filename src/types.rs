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
