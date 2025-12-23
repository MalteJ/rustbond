//! Netlink integration for installing routes into the Linux kernel.
//!
//! This module provides functionality to install MetalBond routes into
//! Linux kernel routing tables using the netlink protocol.
//!
//! # Example
//!
//! ```no_run
//! # use rustbond::netlink::{NetlinkClient, NetlinkConfig};
//! # use rustbond::Vni;
//! # use std::collections::HashMap;
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut vni_table_map = HashMap::new();
//! vni_table_map.insert(Vni(100), 100); // VNI 100 -> route table 100
//!
//! let config = NetlinkConfig {
//!     vni_table_map,
//!     link_name: "ip6tnl0".to_string(),
//!     ipv4_only: false,
//! };
//!
//! let client = NetlinkClient::new(config).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use futures::TryStreamExt;
use rtnetlink::Handle;
use netlink_packet_route::route::RouteProtocol;

use crate::types::{Destination, NextHop, Vni};
use crate::{Error, Result};

/// Protocol number used to mark routes installed by MetalBond.
/// This allows identifying and cleaning up MetalBond-managed routes.
pub const METALBOND_RT_PROTO: u8 = 254;

/// Configuration for the netlink client.
#[derive(Debug, Clone)]
pub struct NetlinkConfig {
    /// Maps VNIs to Linux kernel routing table IDs.
    pub vni_table_map: HashMap<Vni, u32>,
    /// Name of the tunnel device (e.g., "ip6tnl0").
    pub link_name: String,
}

/// Client for installing routes into the Linux kernel via netlink.
pub struct NetlinkClient {
    handle: Handle,
    config: NetlinkConfig,
    link_index: u32,
}

impl NetlinkClient {
    /// Creates a new netlink client.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration including VNI-to-table mapping and tunnel device name
    ///
    /// # Errors
    ///
    /// Returns an error if the tunnel device cannot be found.
    pub async fn new(config: NetlinkConfig) -> Result<Self> {
        let (connection, handle, _) = rtnetlink::new_connection()?;
        tokio::spawn(connection);

        // Find the tunnel device
        let link_index = Self::get_link_index(&handle, &config.link_name).await?;

        tracing::info!(
            link = %config.link_name,
            link_index = link_index,
            tables = ?config.vni_table_map,
            "NetlinkClient initialized"
        );

        Ok(Self {
            handle,
            config,
            link_index,
        })
    }

    /// Gets the interface index for a link by name.
    async fn get_link_index(handle: &Handle, name: &str) -> Result<u32> {
        let mut links = handle.link().get().match_name(name.to_string()).execute();

        if let Some(link) = links.try_next().await? {
            Ok(link.header.index)
        } else {
            Err(Error::Netlink(format!(
                "Cannot find tunnel device '{}'",
                name
            )))
        }
    }

    /// Adds a route to the kernel routing table.
    ///
    /// The route is installed with the next hop's target address as the gateway.
    ///
    /// # Arguments
    ///
    /// * `vni` - The VNI determining which routing table to use
    /// * `dest` - The destination prefix
    /// * `next_hop` - The next hop containing the tunnel endpoint
    pub async fn add_route(&self, vni: Vni, dest: &Destination, next_hop: &NextHop) -> Result<()> {
        // Get the routing table for this VNI
        let table = self.config.vni_table_map.get(&vni).ok_or_else(|| {
            Error::Netlink(format!("No route table ID configured for VNI {}", vni))
        })?;

        // Build and execute the route add request
        let dest_addr = dest.prefix.addr();
        let prefix_len = dest.prefix.prefix_len();
        let gateway = next_hop.target_address;

        match dest_addr {
            IpAddr::V4(addr) => {
                self.add_route_v4(addr, prefix_len, gateway, *table).await?;
            }
            IpAddr::V6(addr) => {
                self.add_route_v6(addr, prefix_len, gateway, *table).await?;
            }
        }

        tracing::debug!(
            vni = %vni,
            dest = %dest,
            next_hop = %next_hop.target_address,
            table = table,
            "Route added to kernel"
        );

        Ok(())
    }

    async fn add_route_v4(
        &self,
        dest: Ipv4Addr,
        prefix_len: u8,
        _gateway: Ipv6Addr,
        table: u32,
    ) -> Result<()> {
        // For IPv4 destinations with IPv6 tunnel, we need encapsulation
        // The route goes via the tunnel device
        // TODO: Add proper IP6tnl encapsulation with gateway as tunnel endpoint
        self.handle
            .route()
            .add()
            .v4()
            .destination_prefix(dest, prefix_len)
            .output_interface(self.link_index)
            .table_id(table)
            .protocol(RouteProtocol::Other(METALBOND_RT_PROTO))
            .execute()
            .await
            .map_err(|e| {
                Error::Netlink(format!(
                    "Cannot add IPv4 route to {}/{} (table {}): {}",
                    dest, prefix_len, table, e
                ))
            })
    }

    async fn add_route_v6(
        &self,
        dest: Ipv6Addr,
        prefix_len: u8,
        gateway: Ipv6Addr,
        table: u32,
    ) -> Result<()> {
        self.handle
            .route()
            .add()
            .v6()
            .destination_prefix(dest, prefix_len)
            .gateway(gateway)
            .output_interface(self.link_index)
            .table_id(table)
            .protocol(RouteProtocol::Other(METALBOND_RT_PROTO))
            .execute()
            .await
            .map_err(|e| {
                Error::Netlink(format!(
                    "Cannot add IPv6 route to {}/{} (table {}): {}",
                    dest, prefix_len, table, e
                ))
            })
    }

    /// Removes a route from the kernel routing table.
    pub async fn remove_route(&self, vni: Vni, dest: &Destination, next_hop: &NextHop) -> Result<()> {
        // Get the routing table for this VNI
        let table = self.config.vni_table_map.get(&vni).ok_or_else(|| {
            Error::Netlink(format!("No route table ID configured for VNI {}", vni))
        })?;

        let dest_addr = dest.prefix.addr();
        let prefix_len = dest.prefix.prefix_len();

        // Find and delete the route
        match dest_addr {
            IpAddr::V4(addr) => {
                self.remove_route_v4(addr, prefix_len, *table).await?;
            }
            IpAddr::V6(addr) => {
                self.remove_route_v6(addr, prefix_len, *table).await?;
            }
        }

        tracing::debug!(
            vni = %vni,
            dest = %dest,
            next_hop = %next_hop.target_address,
            table = table,
            "Route removed from kernel"
        );

        Ok(())
    }

    async fn remove_route_v4(&self, dest: Ipv4Addr, prefix_len: u8, table: u32) -> Result<()> {
        // Find the route first
        let mut routes = self.handle.route().get(rtnetlink::IpVersion::V4).execute();

        while let Some(route) = routes.try_next().await? {
            // Check if this is our route
            let matches_table = route.header.table == table as u8
                || route.attributes.iter().any(|attr| {
                    use netlink_packet_route::route::RouteAttribute;
                    matches!(attr, RouteAttribute::Table(t) if *t == table)
                });

            let matches_protocol =
                route.header.protocol == RouteProtocol::Other(METALBOND_RT_PROTO);

            let matches_dest = route.attributes.iter().any(|attr| {
                use netlink_packet_route::route::{RouteAddress, RouteAttribute};
                if let RouteAttribute::Destination(RouteAddress::Inet(addr)) = attr {
                    *addr == dest
                } else {
                    false
                }
            }) && route.header.destination_prefix_length == prefix_len;

            if matches_table && matches_protocol && matches_dest {
                self.handle
                    .route()
                    .del(route)
                    .execute()
                    .await
                    .map_err(|e| {
                        Error::Netlink(format!(
                            "Cannot remove route to {}/{} (table {}): {}",
                            dest, prefix_len, table, e
                        ))
                    })?;
                return Ok(());
            }
        }

        // Route not found is not an error
        tracing::debug!(
            dest = %dest,
            prefix_len = prefix_len,
            table = table,
            "Route not found in kernel (already removed?)"
        );
        Ok(())
    }

    async fn remove_route_v6(&self, dest: Ipv6Addr, prefix_len: u8, table: u32) -> Result<()> {
        let mut routes = self.handle.route().get(rtnetlink::IpVersion::V6).execute();

        while let Some(route) = routes.try_next().await? {
            let matches_table = route.header.table == table as u8
                || route.attributes.iter().any(|attr| {
                    use netlink_packet_route::route::RouteAttribute;
                    matches!(attr, RouteAttribute::Table(t) if *t == table)
                });

            let matches_protocol =
                route.header.protocol == RouteProtocol::Other(METALBOND_RT_PROTO);

            let matches_dest = route.attributes.iter().any(|attr| {
                use netlink_packet_route::route::{RouteAddress, RouteAttribute};
                if let RouteAttribute::Destination(RouteAddress::Inet6(addr)) = attr {
                    *addr == dest
                } else {
                    false
                }
            }) && route.header.destination_prefix_length == prefix_len;

            if matches_table && matches_protocol && matches_dest {
                self.handle
                    .route()
                    .del(route)
                    .execute()
                    .await
                    .map_err(|e| {
                        Error::Netlink(format!(
                            "Cannot remove route to {}/{} (table {}): {}",
                            dest, prefix_len, table, e
                        ))
                    })?;
                return Ok(());
            }
        }

        tracing::debug!(
            dest = %dest,
            prefix_len = prefix_len,
            table = table,
            "Route not found in kernel (already removed?)"
        );
        Ok(())
    }

    /// Cleans up all routes installed by MetalBond from the configured tables.
    ///
    /// This removes routes with protocol 254 (METALBOND_RT_PROTO) from all
    /// tables in the VNI-to-table mapping.
    pub async fn cleanup_stale_routes(&self) -> Result<()> {
        for (vni, table) in &self.config.vni_table_map {
            tracing::info!(vni = %vni, table = table, "Cleaning up stale routes");

            // Clean IPv4 routes
            self.cleanup_routes_for_table(*table, rtnetlink::IpVersion::V4).await?;

            // Clean IPv6 routes
            self.cleanup_routes_for_table(*table, rtnetlink::IpVersion::V6).await?;
        }

        Ok(())
    }

    async fn cleanup_routes_for_table(
        &self,
        table: u32,
        ip_version: rtnetlink::IpVersion,
    ) -> Result<()> {
        let mut routes = self.handle.route().get(ip_version).execute();
        let mut to_delete = Vec::new();

        while let Some(route) = routes.try_next().await? {
            let matches_table = route.header.table == table as u8
                || route.attributes.iter().any(|attr| {
                    use netlink_packet_route::route::RouteAttribute;
                    matches!(attr, RouteAttribute::Table(t) if *t == table)
                });

            let matches_protocol =
                route.header.protocol == RouteProtocol::Other(METALBOND_RT_PROTO);

            if matches_table && matches_protocol {
                to_delete.push(route);
            }
        }

        for route in to_delete {
            tracing::debug!(table = table, "Removing stale route");
            if let Err(e) = self.handle.route().del(route).execute().await {
                tracing::warn!(table = table, error = %e, "Failed to remove stale route");
            }
        }

        Ok(())
    }
}
