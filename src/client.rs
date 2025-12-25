//! MetalBond client API.
//!
//! This module provides a client that connects to one or more MetalBond servers
//! for route distribution. When connecting to multiple servers, it provides
//! high availability and ECMP (Equal-Cost Multi-Path) support.
//!
//! ## Features
//!
//! - **Single or Multi-Server**: Works with one server or many
//! - **Active-Active**: Connects to all configured servers simultaneously
//! - **ECMP**: Different next-hops from different servers are combined
//! - **Route Origin Tracking**: Routes are only removed when ALL servers withdraw them
//! - **Automatic Deduplication**: Duplicate routes from multiple servers trigger only one handler call
//!
//! ## Example
//!
//! ```no_run
//! # use rustbond::{MetalBondClient, RouteHandler, Route, Vni};
//! # struct MyHandler;
//! # impl RouteHandler for MyHandler {
//! #     fn add_route(&self, _: Vni, _: Route) {}
//! #     fn remove_route(&self, _: Vni, _: Route) {}
//! # }
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Single server
//! let client = MetalBondClient::connect(&["[::1]:4711"], MyHandler);
//! client.wait_established().await?;
//!
//! // Multiple servers (HA mode)
//! let client = MetalBondClient::connect(
//!     &["[::1]:4711", "[::1]:4712"],
//!     MyHandler,
//! );
//! client.wait_any_established().await?;
//! client.subscribe(Vni(100)).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::future::select_all;
use tokio::sync::{watch, RwLock};

use crate::peer::Peer;
use crate::route::{Route, RouteTable};
use crate::types::{Action, ConnectionState, Destination, NextHop, Vni};
use crate::wire::Message;
use crate::{Error, Result, RouteHandler};

/// Unique identifier for each server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServerId(pub usize);

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server-{}", self.0)
    }
}

/// Connection state when using multiple servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// No servers connected.
    Disconnected,
    /// At least one server connected, but not all.
    PartiallyConnected {
        /// Number of servers currently connected.
        connected: usize,
        /// Total number of configured servers.
        total: usize,
    },
    /// All servers connected.
    FullyConnected,
    /// Client has been shut down.
    Closed,
}

/// Tracks which servers announced each route for ECMP deduplication.
///
/// This is the core data structure for multi-server route handling.
/// It maps each unique route (VNI, Destination, NextHop) to the set of
/// servers that have announced it.
#[derive(Debug, Default)]
struct RouteOriginTracker {
    inner: parking_lot::RwLock<HashMap<(Vni, Destination, NextHop), HashSet<ServerId>>>,
}

impl RouteOriginTracker {
    fn new() -> Self {
        Self::default()
    }

    /// Records that a server announced this route.
    /// Returns `true` if this is the FIRST server to announce this route.
    fn add_origin(
        &self,
        vni: Vni,
        dest: Destination,
        hop: NextHop,
        server: ServerId,
    ) -> bool {
        let mut inner = self.inner.write();
        let key = (vni, dest, hop);
        let servers = inner.entry(key).or_default();
        let was_empty = servers.is_empty();
        servers.insert(server);
        was_empty
    }

    /// Records that a server withdrew this route.
    /// Returns `true` if ALL servers have now withdrawn this route.
    fn remove_origin(
        &self,
        vni: Vni,
        dest: &Destination,
        hop: &NextHop,
        server: ServerId,
    ) -> bool {
        let mut inner = self.inner.write();
        let key = (vni, dest.clone(), hop.clone());

        if let Some(servers) = inner.get_mut(&key) {
            servers.remove(&server);
            if servers.is_empty() {
                inner.remove(&key);
                return true;
            }
        }
        false
    }

    /// Removes all routes from a specific server.
    /// Returns the routes where this was the last server announcing them.
    fn remove_server(&self, server: ServerId) -> Vec<(Vni, Destination, NextHop)> {
        let mut inner = self.inner.write();
        let mut to_notify = Vec::new();

        inner.retain(|(vni, dest, hop), servers| {
            servers.remove(&server);
            if servers.is_empty() {
                to_notify.push((*vni, dest.clone(), hop.clone()));
                false
            } else {
                true
            }
        });

        to_notify
    }

    fn len(&self) -> usize {
        self.inner.read().len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get_all(&self) -> Vec<((Vni, Destination, NextHop), HashSet<ServerId>)> {
        let inner = self.inner.read();
        inner
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn get_servers(
        &self,
        vni: Vni,
        dest: &Destination,
        hop: &NextHop,
    ) -> Option<HashSet<ServerId>> {
        let inner = self.inner.read();
        let key = (vni, dest.clone(), hop.clone());
        inner.get(&key).cloned()
    }
}

/// Internal state shared across all server connections.
struct ClientSharedState<H: RouteHandler> {
    origins: RouteOriginTracker,
    handler: Arc<H>,
    subscriptions: RwLock<HashSet<Vni>>,
    announced_routes: RouteTable,
}

/// Wrapper handler that intercepts route updates and deduplicates.
struct DeduplicatingHandler<H: RouteHandler> {
    server_id: ServerId,
    shared: Arc<ClientSharedState<H>>,
}

impl<H: RouteHandler> RouteHandler for DeduplicatingHandler<H> {
    fn add_route(&self, vni: Vni, route: Route) {
        if self.shared.origins.add_origin(
            vni,
            route.destination.clone(),
            route.next_hop.clone(),
            self.server_id,
        ) {
            tracing::debug!(
                server = %self.server_id,
                %vni,
                %route,
                "route added (first server)"
            );
            self.shared.handler.add_route(vni, route);
        } else {
            tracing::trace!(
                server = %self.server_id,
                %vni,
                %route,
                "route already known from other server"
            );
        }
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        if self.shared.origins.remove_origin(
            vni,
            &route.destination,
            &route.next_hop,
            self.server_id,
        ) {
            tracing::debug!(
                server = %self.server_id,
                %vni,
                %route,
                "route removed (last server)"
            );
            self.shared.handler.remove_route(vni, route);
        } else {
            tracing::trace!(
                server = %self.server_id,
                %vni,
                %route,
                "route still available from other servers"
            );
        }
    }
}

/// A MetalBond client for connecting to one or more MetalBond servers.
///
/// The client manages connections to servers, handles automatic reconnection,
/// and provides methods for subscribing to VNIs and announcing routes.
///
/// When connecting to multiple servers:
/// - Routes from all servers are combined (ECMP support)
/// - Duplicate routes are deduplicated (handler called once)
/// - Routes are only removed when ALL servers withdraw them
///
/// # Example
///
/// ```no_run
/// # use rustbond::{MetalBondClient, NoOpHandler, Vni};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Single server
/// let client = MetalBondClient::connect(&["[::1]:4711"], NoOpHandler);
/// client.wait_established().await?;
/// client.subscribe(Vni(100)).await?;
///
/// // Multiple servers for HA
/// let client = MetalBondClient::connect(
///     &["[::1]:4711", "[::1]:4712", "[::1]:4713"],
///     NoOpHandler,
/// );
/// client.wait_any_established().await?;
/// # Ok(())
/// # }
/// ```
pub struct MetalBondClient<H: RouteHandler> {
    peers: HashMap<ServerId, Peer>,
    shared: Arc<ClientSharedState<H>>,
    server_addrs: HashMap<ServerId, String>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl<H: RouteHandler> MetalBondClient<H> {
    /// Creates a new client connecting to the specified servers.
    ///
    /// The client will immediately start connecting to all servers in the background.
    /// Use [`Self::wait_established`] or [`Self::wait_any_established`] to wait for connections.
    ///
    /// # Arguments
    ///
    /// * `servers` - List of server addresses (e.g., `&["[::1]:4711"]` or `&["[::1]:4711", "[::1]:4712"]`)
    /// * `handler` - Route handler that will receive deduplicated route updates
    ///
    /// # Panics
    ///
    /// Panics if `servers` is empty.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use rustbond::{MetalBondClient, NoOpHandler};
    /// // Single server
    /// let client = MetalBondClient::connect(&["[::1]:4711"], NoOpHandler);
    ///
    /// // Multiple servers
    /// let client = MetalBondClient::connect(
    ///     &["[::1]:4711", "[::1]:4712"],
    ///     NoOpHandler,
    /// );
    /// ```
    pub fn connect(servers: &[&str], handler: H) -> Self {
        assert!(!servers.is_empty(), "at least one server address required");

        let handler = Arc::new(handler);
        let shared = Arc::new(ClientSharedState {
            origins: RouteOriginTracker::new(),
            handler,
            subscriptions: RwLock::new(HashSet::new()),
            announced_routes: RouteTable::new(),
        });

        let mut peers = HashMap::new();
        let mut server_addrs = HashMap::new();
        let mut tasks = Vec::new();

        for (idx, addr) in servers.iter().enumerate() {
            let server_id = ServerId(idx);
            let wrapper = DeduplicatingHandler {
                server_id,
                shared: shared.clone(),
            };

            let (peer, task) = Peer::new(
                addr.to_string(),
                Arc::new(wrapper),
                shared.announced_routes.clone(),
            );
            peers.insert(server_id, peer);
            server_addrs.insert(server_id, addr.to_string());
            tasks.push(task);

            tracing::info!(
                server_id = %server_id,
                addr = %addr,
                "connecting to server"
            );
        }

        Self {
            peers,
            shared,
            server_addrs,
            _tasks: tasks,
        }
    }

    /// Returns the current connection state.
    pub fn state(&self) -> ClientState {
        let mut connected = 0;
        let mut any_closed = false;

        for peer in self.peers.values() {
            match peer.get_state() {
                ConnectionState::Established => connected += 1,
                ConnectionState::Closed => any_closed = true,
                _ => {}
            }
        }

        if any_closed && connected == 0 {
            ClientState::Closed
        } else if connected == 0 {
            ClientState::Disconnected
        } else if connected == self.peers.len() {
            ClientState::FullyConnected
        } else {
            ClientState::PartiallyConnected {
                connected,
                total: self.peers.len(),
            }
        }
    }

    /// Waits until at least one server is established.
    ///
    /// For single-server setups, this is equivalent to waiting for the connection.
    /// For multi-server setups, returns as soon as any server connects.
    pub async fn wait_any_established(&self) -> Result<()> {
        // Collect state receivers from all peers
        let mut receivers: Vec<watch::Receiver<ConnectionState>> = self
            .peers
            .values()
            .map(|p| p.state_receiver())
            .collect();

        loop {
            // Check current states
            let mut any_established = false;
            let mut all_closed = true;

            for rx in &receivers {
                match *rx.borrow() {
                    ConnectionState::Established => {
                        any_established = true;
                        all_closed = false;
                    }
                    ConnectionState::Closed => {}
                    _ => {
                        all_closed = false;
                    }
                }
            }

            if any_established {
                return Ok(());
            }

            if all_closed {
                return Err(Error::Closed);
            }

            // Wait for any state to change
            // We need to handle the borrow carefully - select_all consumes the futures
            let futures: Vec<_> = receivers
                .iter_mut()
                .map(|rx| Box::pin(rx.changed()))
                .collect();

            let (result, _, remaining) = select_all(futures).await;
            drop(remaining); // Drop remaining futures to release borrows

            // If all senders are dropped, the connection is closed
            if result.is_err() {
                // Check if all are closed now
                if receivers.iter().all(|rx| *rx.borrow() == ConnectionState::Closed) {
                    return Err(Error::Closed);
                }
            }
        }
    }

    /// Waits until all servers are established.
    pub async fn wait_all_established(&self) -> Result<()> {
        // Collect state receivers from all peers
        let mut receivers: Vec<watch::Receiver<ConnectionState>> = self
            .peers
            .values()
            .map(|p| p.state_receiver())
            .collect();

        loop {
            // Check current states
            let mut all_established = true;
            let mut any_closed = false;

            for rx in &receivers {
                match *rx.borrow() {
                    ConnectionState::Established => {}
                    ConnectionState::Closed => any_closed = true,
                    _ => all_established = false,
                }
            }

            if all_established && !any_closed {
                return Ok(());
            }

            if any_closed {
                return Err(Error::Closed);
            }

            // Wait for any state to change
            let futures: Vec<_> = receivers
                .iter_mut()
                .map(|rx| Box::pin(rx.changed()))
                .collect();

            let (result, _, remaining) = select_all(futures).await;
            drop(remaining); // Drop remaining futures to release borrows

            // If sender dropped unexpectedly, check state
            if result.is_err() {
                if receivers.iter().any(|rx| *rx.borrow() == ConnectionState::Closed) {
                    return Err(Error::Closed);
                }
            }
        }
    }

    /// Waits for connection to be established (alias for [`Self::wait_any_established`]).
    pub async fn wait_established(&self) -> Result<()> {
        self.wait_any_established().await
    }

    /// Waits for connection with a timeout.
    pub async fn wait_established_timeout(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.wait_established())
            .await
            .map_err(|_| Error::Timeout)?
    }

    /// Waits until at least one server is established, with timeout.
    pub async fn wait_any_established_timeout(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.wait_any_established())
            .await
            .map_err(|_| Error::ConnectionTimeout)?
    }

    /// Waits until all servers are established, with timeout.
    pub async fn wait_all_established_timeout(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.wait_all_established())
            .await
            .map_err(|_| Error::ConnectionTimeout)?
    }

    /// Returns true if at least one server is connected.
    pub fn is_established(&self) -> bool {
        self.peers
            .values()
            .any(|p| p.state_receiver().borrow().clone() == ConnectionState::Established)
    }

    /// Subscribes to a VNI on all connected servers.
    pub async fn subscribe(&self, vni: Vni) -> Result<()> {
        self.shared.subscriptions.write().await.insert(vni);

        let mut sent_any = false;
        for (server_id, peer) in &self.peers {
            if peer.get_state() == ConnectionState::Established {
                if let Err(e) = peer.subscribe(vni).await {
                    tracing::warn!(
                        server = %server_id,
                        %vni,
                        error = %e,
                        "failed to subscribe on server"
                    );
                } else {
                    sent_any = true;
                }
            }
        }

        if !sent_any {
            return Err(Error::NotEstablished);
        }

        tracing::info!(%vni, "subscribed");
        Ok(())
    }

    /// Unsubscribes from a VNI on all servers.
    pub async fn unsubscribe(&self, vni: Vni) -> Result<()> {
        self.shared.subscriptions.write().await.remove(&vni);

        for (server_id, peer) in &self.peers {
            if let Err(e) = peer.unsubscribe(vni).await {
                tracing::warn!(
                    server = %server_id,
                    %vni,
                    error = %e,
                    "failed to unsubscribe on server"
                );
            }
        }

        tracing::info!(%vni, "unsubscribed");
        Ok(())
    }

    /// Announces a route to all connected servers.
    pub async fn announce(
        &self,
        vni: Vni,
        destination: Destination,
        next_hop: NextHop,
    ) -> Result<()> {
        if self
            .shared
            .announced_routes
            .next_hop_exists(vni, &destination, &next_hop)
        {
            return Err(Error::RouteAlreadyAnnounced);
        }

        self.shared
            .announced_routes
            .add_next_hop(vni, destination.clone(), next_hop.clone());

        let msg = Message::Update {
            action: Action::Add,
            vni,
            destination: destination.clone(),
            next_hop: next_hop.clone(),
        };

        let mut sent_any = false;
        for (server_id, peer) in &self.peers {
            if peer.get_state() == ConnectionState::Established {
                if let Err(e) = peer.send_update(msg.clone()).await {
                    tracing::warn!(
                        server = %server_id,
                        %vni,
                        error = %e,
                        "failed to announce route on server"
                    );
                } else {
                    sent_any = true;
                }
            }
        }

        if !sent_any {
            return Err(Error::NotEstablished);
        }

        let route = Route::new(destination, next_hop);
        tracing::info!(%vni, %route, "announced route");
        Ok(())
    }

    /// Withdraws a route from all servers.
    pub async fn withdraw(
        &self,
        vni: Vni,
        destination: Destination,
        next_hop: NextHop,
    ) -> Result<()> {
        if !self
            .shared
            .announced_routes
            .next_hop_exists(vni, &destination, &next_hop)
        {
            return Err(Error::RouteNotFound);
        }

        self.shared
            .announced_routes
            .remove_next_hop(vni, &destination, &next_hop);

        let msg = Message::Update {
            action: Action::Remove,
            vni,
            destination: destination.clone(),
            next_hop: next_hop.clone(),
        };

        for (server_id, peer) in &self.peers {
            if let Err(e) = peer.send_update(msg.clone()).await {
                tracing::warn!(
                    server = %server_id,
                    %vni,
                    error = %e,
                    "failed to withdraw route on server"
                );
            }
        }

        let route = Route::new(destination, next_hop);
        tracing::info!(%vni, %route, "withdrew route");
        Ok(())
    }

    /// Returns all received routes for a VNI (deduplicated across servers).
    pub fn get_received_routes(&self, vni: Vni) -> Vec<Route> {
        self.shared
            .origins
            .get_all()
            .into_iter()
            .filter(|((v, _, _), _)| *v == vni)
            .map(|((_, dest, hop), _)| Route::new(dest, hop))
            .collect()
    }

    /// Returns all announced routes.
    pub fn get_announced_routes(&self) -> Vec<(Vni, Route)> {
        self.shared.announced_routes.get_all_routes()
    }

    /// Returns the number of unique routes tracked across all servers.
    pub fn route_count(&self) -> usize {
        self.shared.origins.len()
    }

    /// Returns true if no routes are being tracked.
    pub fn is_route_table_empty(&self) -> bool {
        self.shared.origins.is_empty()
    }

    /// Returns per-server connection states.
    pub fn server_states(&self) -> HashMap<ServerId, ConnectionState> {
        self.peers
            .iter()
            .map(|(&id, peer)| (id, peer.get_state()))
            .collect()
    }

    /// Returns the number of configured servers.
    pub fn server_count(&self) -> usize {
        self.peers.len()
    }

    /// Returns the number of currently connected servers.
    pub fn connected_count(&self) -> usize {
        self.peers
            .values()
            .filter(|p| p.get_state() == ConnectionState::Established)
            .count()
    }

    /// Returns the address for a server ID.
    pub fn server_addr(&self, id: ServerId) -> Option<&str> {
        self.server_addrs.get(&id).map(|s| s.as_str())
    }

    /// Returns which servers announced a specific route.
    pub fn get_route_servers(
        &self,
        vni: Vni,
        dest: &Destination,
        hop: &NextHop,
    ) -> Option<HashSet<ServerId>> {
        self.shared.origins.get_servers(vni, dest, hop)
    }

    /// Removes all routes from a specific server.
    ///
    /// Call this when you detect a server has permanently disconnected
    /// to clean up routes that were only announced by that server.
    pub fn remove_server_routes(&self, server: ServerId) -> Vec<Route> {
        let removed = self.shared.origins.remove_server(server);

        for (vni, dest, hop) in &removed {
            let route = Route::new(dest.clone(), hop.clone());
            self.shared.handler.remove_route(*vni, route);
        }

        removed
            .into_iter()
            .map(|(_, dest, hop)| Route::new(dest, hop))
            .collect()
    }

    /// Shuts down all connections.
    pub async fn shutdown(self) -> Result<()> {
        for peer in self.peers.values() {
            let _ = peer.shutdown().await;
        }
        Ok(())
    }
}

/// A no-op route handler for testing.
pub struct NoOpHandler;

impl RouteHandler for NoOpHandler {
    fn add_route(&self, _vni: Vni, _route: Route) {}
    fn remove_route(&self, _vni: Vni, _route: Route) {}
}

/// A logging route handler for debugging.
pub struct LoggingHandler;

impl RouteHandler for LoggingHandler {
    fn add_route(&self, vni: Vni, route: Route) {
        tracing::info!(%vni, %route, "route added");
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        tracing::info!(%vni, %route, "route removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn make_dest(prefix: &str) -> Destination {
        Destination::new(prefix.parse().unwrap())
    }

    fn make_hop(addr: &str) -> NextHop {
        NextHop::standard(addr.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn test_server_id_display() {
        assert_eq!(format!("{}", ServerId(0)), "server-0");
        assert_eq!(format!("{}", ServerId(5)), "server-5");
    }

    #[test]
    fn test_server_id_equality() {
        assert_eq!(ServerId(1), ServerId(1));
        assert_ne!(ServerId(1), ServerId(2));
    }

    #[test]
    fn test_server_id_hash() {
        let mut set = HashSet::new();
        set.insert(ServerId(1));
        set.insert(ServerId(1));
        set.insert(ServerId(2));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_first_add_returns_true() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        assert!(tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(0)));
    }

    #[test]
    fn test_second_add_returns_false() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        assert!(tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(0)));
        assert!(!tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(1)));
    }

    #[test]
    fn test_different_hop_is_new_route() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop1 = make_hop("2001:db8::1");
        let hop2 = make_hop("2001:db8::2");

        assert!(tracker.add_origin(vni, dest.clone(), hop1.clone(), ServerId(0)));
        assert!(tracker.add_origin(vni, dest.clone(), hop2.clone(), ServerId(0)));
    }

    #[test]
    fn test_remove_not_last_returns_false() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(0));
        tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(1));

        assert!(!tracker.remove_origin(vni, &dest, &hop, ServerId(0)));
    }

    #[test]
    fn test_remove_last_returns_true() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(0));
        tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(1));

        tracker.remove_origin(vni, &dest, &hop, ServerId(0));
        assert!(tracker.remove_origin(vni, &dest, &hop, ServerId(1)));
    }

    #[test]
    fn test_remove_nonexistent_returns_false() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        assert!(!tracker.remove_origin(vni, &dest, &hop, ServerId(0)));
    }

    #[test]
    fn test_remove_server_batch() {
        let tracker = RouteOriginTracker::new();

        let dest1 = make_dest("10.0.1.0/24");
        let dest2 = make_dest("10.0.2.0/24");
        let hop1 = make_hop("2001:db8::1");
        let hop2 = make_hop("2001:db8::2");

        tracker.add_origin(Vni(100), dest1.clone(), hop1.clone(), ServerId(0));
        tracker.add_origin(Vni(100), dest1.clone(), hop1.clone(), ServerId(1));
        tracker.add_origin(Vni(100), dest2.clone(), hop2.clone(), ServerId(0));

        assert_eq!(tracker.len(), 2);

        let removed = tracker.remove_server(ServerId(0));

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], (Vni(100), dest2, hop2));
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_get_servers() {
        let tracker = RouteOriginTracker::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        assert!(tracker.get_servers(vni, &dest, &hop).is_none());

        tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(0));
        tracker.add_origin(vni, dest.clone(), hop.clone(), ServerId(2));

        let servers = tracker.get_servers(vni, &dest, &hop).unwrap();
        assert_eq!(servers.len(), 2);
        assert!(servers.contains(&ServerId(0)));
        assert!(servers.contains(&ServerId(2)));
    }

    #[test]
    fn test_len_and_is_empty() {
        let tracker = RouteOriginTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);

        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");
        tracker.add_origin(Vni(100), dest.clone(), hop.clone(), ServerId(0));

        assert!(!tracker.is_empty());
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_thread_safety() {
        use std::thread;

        let tracker = Arc::new(RouteOriginTracker::new());
        let mut handles = vec![];

        for i in 0..10 {
            let tracker = tracker.clone();
            let handle = thread::spawn(move || {
                let dest = make_dest(&format!("10.0.{}.0/24", i));
                let hop = make_hop(&format!("2001:db8::{:x}", i));
                tracker.add_origin(Vni(100), dest, hop, ServerId(i));
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(tracker.len(), 10);
    }
}
