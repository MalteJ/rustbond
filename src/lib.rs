//! Rust implementation of the MetalBond route distribution protocol.
//!
//! MetalBond distributes virtual network routes across hypervisors over TCP/IPv6.
//!
//! # Quick Start
//!
//! ## Server
//!
//! ```no_run
//! # use rustbond::{MetalBondServer, ServerConfig};
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await?;
//! // ... server.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Client
//!
//! ```no_run
//! use rustbond::{MetalBondClient, RouteHandler, Route, Vni};
//!
//! struct MyHandler;
//! impl RouteHandler for MyHandler {
//!     fn add_route(&self, vni: Vni, route: Route) { /* handle add */ }
//!     fn remove_route(&self, vni: Vni, route: Route) { /* handle remove */ }
//! }
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = MetalBondClient::connect(&["[::1]:4711"], MyHandler);
//! client.wait_established().await?;
//! client.subscribe(Vni(100)).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Multi-Server (HA)
//!
//! For high availability, connect to multiple servers simultaneously:
//!
//! ```no_run
//! # use rustbond::{MetalBondClient, RouteHandler, Route, Vni};
//! # struct MyHandler;
//! # impl RouteHandler for MyHandler {
//! #     fn add_route(&self, _: Vni, _: Route) {}
//! #     fn remove_route(&self, _: Vni, _: Route) {}
//! # }
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = MetalBondClient::connect(&["[::1]:4711", "[::1]:4712"], MyHandler);
//! client.wait_any_established().await?;
//! // Routes are deduplicated across servers; ECMP supported
//! # Ok(())
//! # }
//! ```
//!
//! # Features
//!
//! - Async client/server with Tokio
//! - Automatic reconnection
//! - VNI-based subscriptions
//! - Multi-server HA with ECMP support
//! - Standard, NAT, and LoadBalancer route types
//!
//! ## Netlink Feature
//!
//! Enable the `netlink` feature to install routes directly into the Linux kernel:
//!
//! ```toml
//! [dependencies]
//! rustbond = { version = "0.1", features = ["netlink"] }
//! ```

mod error;
mod proto;
mod route;
mod types;
mod wire;

mod client;
mod peer;
mod server;

#[cfg(feature = "netlink")]
pub mod netlink;

pub use client::{ClientState, LoggingHandler, MetalBondClient, NoOpHandler, ServerId};
pub use error::Error;
pub use route::{Route, RouteTable};
pub use server::{MetalBondServer, ServerConfig, ServerState};
pub use types::{
    Action, ConnectionState, Destination, IpVersion, NextHop, NextHopType, Vni,
};

use std::sync::Arc;

/// Result type for MetalBond operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Handler trait for receiving route updates.
pub trait RouteHandler: Send + Sync + 'static {
    /// Called when a route is added.
    fn add_route(&self, vni: Vni, route: Route);

    /// Called when a route is removed.
    fn remove_route(&self, vni: Vni, route: Route);
}

impl<T: RouteHandler> RouteHandler for Arc<T> {
    fn add_route(&self, vni: Vni, route: Route) {
        (**self).add_route(vni, route)
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        (**self).remove_route(vni, route)
    }
}
