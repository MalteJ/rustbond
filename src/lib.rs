//! RustBond - A Rust implementation of the MetalBond protocol.
//!
//! MetalBond is a route distribution protocol for managing virtual network routes
//! across hypervisors. It operates over TCP/IPv6 and distributes routing information
//! for IP-in-IPv6 overlay networks.
//!
//! ## Features
//!
//! - Async TCP client and server using Tokio
//! - Automatic reconnection on connection loss (client)
//! - VNI-based subscription model
//! - Route announcement and withdrawal
//! - Keepalive handling
//! - Support for STANDARD, NAT, and LOADBALANCER_TARGET route types
//!
//! ## Server Usage
//!
//! ```ignore
//! use rustbond::{MetalBondServer, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await.unwrap();
//!     println!("Server running, press Ctrl+C to stop");
//!     tokio::signal::ctrl_c().await.unwrap();
//!     server.shutdown().await.unwrap();
//! }
//! ```
//!
//! ## Client Usage
//!
//! ```ignore
//! use rustbond::{MetalBondClient, RouteHandler, Route, Vni, Destination, NextHop};
//!
//! struct MyHandler;
//!
//! impl RouteHandler for MyHandler {
//!     fn add_route(&self, vni: Vni, route: Route) {
//!         println!("Add route: {} in VNI {}", route, vni);
//!     }
//!
//!     fn remove_route(&self, vni: Vni, route: Route) {
//!         println!("Remove route: {} in VNI {}", route, vni);
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let client = MetalBondClient::connect("[::1]:4711", MyHandler);
//!     client.wait_established().await.unwrap();
//!     client.subscribe(Vni(100)).await.unwrap();
//!
//!     let dest = Destination::from_ipv4([10, 0, 1, 0].into(), 24);
//!     let hop = NextHop::standard("2001:db8::1".parse().unwrap());
//!     client.announce(Vni(100), dest, hop).await.unwrap();
//! }
//! ```

mod error;
mod proto;
mod route;
mod types;
mod wire;

mod client;
mod peer;
mod server;

pub use client::{LoggingHandler, MetalBondClient, NoOpHandler};
pub use error::Error;
pub use route::{Route, RouteTable};
pub use server::{MetalBondServer, ServerConfig, ServerState};
pub use types::{
    Action, ConnectionState, Destination, IpVersion, NextHop, NextHopType, Vni,
};

/// Result type for MetalBond operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Handler trait for receiving route updates.
pub trait RouteHandler: Send + Sync + 'static {
    /// Called when a route is added.
    fn add_route(&self, vni: Vni, route: Route);

    /// Called when a route is removed.
    fn remove_route(&self, vni: Vni, route: Route);
}
