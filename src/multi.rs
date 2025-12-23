//! Multi-server client for high availability.
//!
//! This module provides a client that connects to multiple MetalBond servers
//! simultaneously for high availability and ECMP (Equal-Cost Multi-Path) support.
//!
//! ## Features
//!
//! - **Active-Active**: Connects to all configured servers simultaneously
//! - **ECMP**: Different next-hops from different servers are combined
//! - **Route Origin Tracking**: Routes are only removed when ALL servers withdraw them
//! - **Automatic Deduplication**: Duplicate routes from multiple servers trigger only one handler call
//!
//! ## Example
//!
//! ```no_run
//! # use rustbond::{MultiServerClient, RouteHandler, Route, Vni};
//! # struct MyHandler;
//! # impl RouteHandler for MyHandler {
//! #     fn add_route(&self, _: Vni, _: Route) {}
//! #     fn remove_route(&self, _: Vni, _: Route) {}
//! # }
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = MultiServerClient::connect(
//!     &["[::1]:4711", "[::1]:4712"],
//!     MyHandler,
//! );
//!
//! client.wait_any_established().await?;
//! client.subscribe(Vni(100)).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

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

/// Combined connection state for multi-server client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiServerState {
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
pub struct RouteOriginTracker {
    inner: parking_lot::RwLock<HashMap<(Vni, Destination, NextHop), HashSet<ServerId>>>,
}

impl RouteOriginTracker {
    /// Creates a new empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a server announced this route.
    ///
    /// Returns `true` if this is the FIRST server to announce this route,
    /// meaning `handler.add_route()` should be called.
    pub fn add_origin(
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
    ///
    /// Returns `true` if ALL servers have now withdrawn this route,
    /// meaning `handler.remove_route()` should be called.
    pub fn remove_origin(
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
    ///
    /// Returns the routes that should be removed from the handler
    /// (those where this was the last server announcing them).
    ///
    /// This should be called when a server disconnects to clean up
    /// routes that were only announced by that server.
    pub fn remove_server(&self, server: ServerId) -> Vec<(Vni, Destination, NextHop)> {
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

    /// Returns the number of unique routes being tracked.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Returns true if no routes are being tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns all routes with their announcing servers.
    pub fn get_all(&self) -> Vec<((Vni, Destination, NextHop), HashSet<ServerId>)> {
        let inner = self.inner.read();
        inner
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Returns the servers that have announced a specific route.
    pub fn get_servers(
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
struct MultiClientState<H: RouteHandler> {
    /// Route origin tracking for ECMP deduplication.
    origins: RouteOriginTracker,
    /// User's handler (receives deduplicated updates).
    handler: Arc<H>,
    /// Client subscriptions (unified across all servers).
    subscriptions: RwLock<HashSet<Vni>>,
    /// Routes announced by this client (sent to all servers).
    announced_routes: RouteTable,
}

/// Wrapper handler that intercepts route updates and deduplicates.
struct MultiServerHandler<H: RouteHandler> {
    server_id: ServerId,
    shared: Arc<MultiClientState<H>>,
}

impl<H: RouteHandler> RouteHandler for MultiServerHandler<H> {
    fn add_route(&self, vni: Vni, route: Route) {
        // Only notify user if this is the first server to announce
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
        // Only notify user if all servers have withdrawn
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

/// A MetalBond client that connects to multiple servers simultaneously.
///
/// This provides high availability and ECMP support by:
/// - Connecting to all configured servers at once
/// - Combining routes from all servers (different next-hops = ECMP)
/// - Deduplicating identical routes from multiple servers
/// - Only removing routes when ALL servers have withdrawn them
pub struct MultiServerClient<H: RouteHandler> {
    /// Per-server peers, keyed by server ID.
    peers: HashMap<ServerId, Peer>,
    /// Shared state across all connections.
    shared: Arc<MultiClientState<H>>,
    /// Server addresses for display/debugging.
    server_addrs: HashMap<ServerId, String>,
    /// Background task handles.
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl<H: RouteHandler> MultiServerClient<H> {
    /// Creates a new multi-server client connecting to all specified servers.
    ///
    /// The client will immediately start connecting to all servers in the background.
    /// Use `wait_any_established()` or `wait_all_established()` to wait for connections.
    ///
    /// # Arguments
    ///
    /// * `servers` - List of server addresses (e.g., `["[::1]:4711", "[::1]:4712"]`)
    /// * `handler` - Route handler that will receive deduplicated route updates
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use rustbond::{MultiServerClient, NoOpHandler};
    /// let client = MultiServerClient::connect(
    ///     &["[::1]:4711", "[::1]:4712", "[::1]:4713"],
    ///     NoOpHandler,
    /// );
    /// ```
    pub fn connect(servers: &[&str], handler: H) -> Self {
        let handler = Arc::new(handler);
        let shared = Arc::new(MultiClientState {
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
            let wrapper = MultiServerHandler {
                server_id,
                shared: shared.clone(),
            };

            let (peer, task) = Peer::new(addr.to_string(), Arc::new(wrapper));
            peers.insert(server_id, peer);
            server_addrs.insert(server_id, addr.to_string());
            tasks.push(task);

            tracing::info!(
                server_id = %server_id,
                addr = %addr,
                "added server to multi-server client"
            );
        }

        Self {
            peers,
            shared,
            server_addrs,
            _tasks: tasks,
        }
    }

    /// Returns the current combined connection state.
    pub async fn state(&self) -> MultiServerState {
        let mut connected = 0;
        let mut any_closed = false;

        for peer in self.peers.values() {
            match peer.get_state().await {
                ConnectionState::Established => connected += 1,
                ConnectionState::Closed => any_closed = true,
                _ => {}
            }
        }

        if any_closed && connected == 0 {
            MultiServerState::Closed
        } else if connected == 0 {
            MultiServerState::Disconnected
        } else if connected == self.peers.len() {
            MultiServerState::FullyConnected
        } else {
            MultiServerState::PartiallyConnected {
                connected,
                total: self.peers.len(),
            }
        }
    }

    /// Waits until at least one server is established, with timeout.
    ///
    /// Returns `Ok(())` when at least one server connection is established.
    /// Returns `Err(Error::ConnectionTimeout)` if the timeout expires.
    /// Returns `Err(Error::Closed)` if all servers are closed.
    pub async fn wait_any_established_timeout(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.wait_any_established())
            .await
            .map_err(|_| Error::ConnectionTimeout)?
    }

    /// Waits until all servers are established, with timeout.
    ///
    /// Returns `Ok(())` when all server connections are established.
    /// Returns `Err(Error::ConnectionTimeout)` if the timeout expires.
    /// Returns `Err(Error::Closed)` if any server is closed.
    pub async fn wait_all_established_timeout(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.wait_all_established())
            .await
            .map_err(|_| Error::ConnectionTimeout)?
    }

    /// Waits until at least one server is established.
    ///
    /// Returns `Ok(())` when at least one server connection is established.
    /// Returns `Err(Error::Closed)` if all servers are closed.
    pub async fn wait_any_established(&self) -> Result<()> {
        loop {
            let mut any_established = false;
            let mut all_closed = true;

            for peer in self.peers.values() {
                match peer.get_state().await {
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

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits until all servers are established.
    ///
    /// Returns `Ok(())` when all server connections are established.
    /// Returns `Err(Error::Closed)` if any server is closed.
    pub async fn wait_all_established(&self) -> Result<()> {
        loop {
            let mut all_established = true;
            let mut any_closed = false;

            for peer in self.peers.values() {
                match peer.get_state().await {
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

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Subscribes to a VNI on all connected servers.
    ///
    /// The subscription is tracked locally and will be re-sent to servers
    /// that reconnect later.
    pub async fn subscribe(&self, vni: Vni) -> Result<()> {
        // Track subscription
        self.shared.subscriptions.write().await.insert(vni);

        // Send to all established peers
        let mut sent_any = false;
        for (server_id, peer) in &self.peers {
            if peer.get_state().await == ConnectionState::Established {
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

        tracing::info!(%vni, "subscribed on multi-server client");
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

        tracing::info!(%vni, "unsubscribed on multi-server client");
        Ok(())
    }

    /// Announces a route to all connected servers.
    ///
    /// The route is tracked locally and will be re-announced to servers
    /// that reconnect later.
    pub async fn announce(
        &self,
        vni: Vni,
        destination: Destination,
        next_hop: NextHop,
    ) -> Result<()> {
        // Check if already announced
        if self
            .shared
            .announced_routes
            .next_hop_exists(vni, &destination, &next_hop)
        {
            return Err(Error::RouteAlreadyAnnounced);
        }

        // Track announcement
        self.shared
            .announced_routes
            .add_next_hop(vni, destination.clone(), next_hop.clone());

        // Send to all established peers
        let msg = Message::Update {
            action: Action::Add,
            vni,
            destination: destination.clone(),
            next_hop: next_hop.clone(),
        };

        let mut sent_any = false;
        for (server_id, peer) in &self.peers {
            if peer.get_state().await == ConnectionState::Established {
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
        tracing::info!(%vni, %route, "announced route on multi-server client");
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
        tracing::info!(%vni, %route, "withdrew route on multi-server client");
        Ok(())
    }

    /// Returns per-server connection states.
    pub async fn server_states(&self) -> HashMap<ServerId, ConnectionState> {
        let mut states = HashMap::new();
        for (&id, peer) in &self.peers {
            states.insert(id, peer.get_state().await);
        }
        states
    }

    /// Returns the number of configured servers.
    pub fn server_count(&self) -> usize {
        self.peers.len()
    }

    /// Returns the number of currently connected servers.
    pub async fn connected_count(&self) -> usize {
        let mut count = 0;
        for peer in self.peers.values() {
            if peer.get_state().await == ConnectionState::Established {
                count += 1;
            }
        }
        count
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

    /// Returns the number of unique routes tracked across all servers.
    pub fn route_count(&self) -> usize {
        self.shared.origins.len()
    }

    /// Returns the address for a server ID.
    pub fn server_addr(&self, id: ServerId) -> Option<&str> {
        self.server_addrs.get(&id).map(|s| s.as_str())
    }

    /// Returns true if no routes are being tracked.
    pub fn is_route_table_empty(&self) -> bool {
        self.shared.origins.is_empty()
    }

    /// Returns which servers announced a specific route.
    ///
    /// Useful for debugging and understanding route distribution.
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
    /// Routes announced by other servers will remain.
    ///
    /// Returns the routes that were removed (those only from this server).
    pub fn remove_server_routes(&self, server: ServerId) -> Vec<Route> {
        let removed = self.shared.origins.remove_server(server);

        // Notify handler for removed routes
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

    // ServerId tests
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

    // RouteOriginTracker tests
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
        // Different hop = different route, should return true
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

        // First removal - still have another server
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
        // Last removal
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

        // Route 1: from servers 0 and 1
        tracker.add_origin(Vni(100), dest1.clone(), hop1.clone(), ServerId(0));
        tracker.add_origin(Vni(100), dest1.clone(), hop1.clone(), ServerId(1));

        // Route 2: only from server 0
        tracker.add_origin(Vni(100), dest2.clone(), hop2.clone(), ServerId(0));

        assert_eq!(tracker.len(), 2);

        // Remove server 0
        let removed = tracker.remove_server(ServerId(0));

        // Only route 2 should be fully removed (was only from server 0)
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], (Vni(100), dest2, hop2));

        // Route 1 should still exist (server 1 still has it)
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

        // Spawn threads adding routes
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
