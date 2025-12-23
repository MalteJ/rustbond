//! MetalBond client API.

use std::sync::Arc;

use crate::peer::Peer;
use crate::route::Route;
use crate::types::{Action, ConnectionState, Destination, NextHop, Vni};
use crate::wire::Message;
use crate::{Error, Result, RouteHandler};

/// A MetalBond client for connecting to a MetalBond server.
///
/// The client manages the connection lifecycle, including automatic reconnection,
/// and provides methods for subscribing to VNIs and announcing routes.
pub struct MetalBondClient<H: RouteHandler> {
    peer: Peer,
    _handler: std::marker::PhantomData<H>,
    _task: tokio::task::JoinHandle<()>,
}

impl<H: RouteHandler> MetalBondClient<H> {
    /// Connects to a MetalBond server.
    ///
    /// The connection is established asynchronously. Use [`Self::wait_established`] to wait
    /// for the connection to be fully established.
    ///
    /// # Arguments
    ///
    /// * `addr` - The server address (e.g., `"[::1]:4711"` or `"server.example.com:4711"`)
    /// * `handler` - A handler that will be called when routes are added or removed
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = MetalBondClient::connect("[::1]:4711", handler);
    /// client.wait_established().await?;
    /// ```
    pub fn connect(addr: impl Into<String>, handler: H) -> Self {
        let handler = Arc::new(handler);
        let (peer, task) = Peer::new(addr.into(), handler);

        Self {
            peer,
            _handler: std::marker::PhantomData,
            _task: task,
        }
    }

    /// Waits for the connection to be established.
    ///
    /// Returns an error if the connection fails or times out.
    pub async fn wait_established(&self) -> Result<()> {
        loop {
            match self.peer.get_state().await {
                ConnectionState::Established => return Ok(()),
                ConnectionState::Closed => return Err(Error::Closed),
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Waits for the connection to be established with a timeout.
    pub async fn wait_established_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<()> {
        tokio::time::timeout(timeout, self.wait_established())
            .await
            .map_err(|_| Error::Timeout)?
    }

    /// Returns the current connection state.
    pub async fn state(&self) -> ConnectionState {
        self.peer.get_state().await
    }

    /// Returns the remote server address.
    pub fn remote_addr(&self) -> &str {
        self.peer.remote_addr()
    }

    /// Returns true if the connection is established.
    pub async fn is_established(&self) -> bool {
        self.peer.get_state().await == ConnectionState::Established
    }

    /// Subscribes to a VNI to receive route updates.
    ///
    /// After subscribing, the handler will be called with route updates for this VNI.
    pub async fn subscribe(&self, vni: Vni) -> Result<()> {
        if self.peer.get_state().await != ConnectionState::Established {
            return Err(Error::NotEstablished);
        }
        self.peer.subscribe(vni).await
    }

    /// Unsubscribes from a VNI.
    ///
    /// After unsubscribing, no more route updates for this VNI will be received.
    pub async fn unsubscribe(&self, vni: Vni) -> Result<()> {
        self.peer.unsubscribe(vni).await
    }

    /// Announces a route to the MetalBond server.
    ///
    /// The route will be distributed to all peers subscribed to the VNI.
    pub async fn announce(&self, vni: Vni, destination: Destination, next_hop: NextHop) -> Result<()> {
        if self.peer.get_state().await != ConnectionState::Established {
            return Err(Error::NotEstablished);
        }

        // Check if already announced
        let state = self.peer.state();
        if state.announced_routes.next_hop_exists(vni, &destination, &next_hop) {
            return Err(Error::RouteAlreadyAnnounced);
        }

        // Add to announced routes
        state.announced_routes.add_next_hop(vni, destination.clone(), next_hop.clone());

        // Send update
        let msg = Message::Update {
            action: Action::Add,
            vni,
            destination,
            next_hop,
        };
        self.peer.send_update(msg).await
    }

    /// Withdraws a previously announced route.
    pub async fn withdraw(&self, vni: Vni, destination: Destination, next_hop: NextHop) -> Result<()> {
        let state = self.peer.state();

        // Check if the route was announced
        if !state.announced_routes.next_hop_exists(vni, &destination, &next_hop) {
            return Err(Error::RouteNotFound);
        }

        // Remove from announced routes
        state.announced_routes.remove_next_hop(vni, &destination, &next_hop);

        // Send update
        let msg = Message::Update {
            action: Action::Remove,
            vni,
            destination,
            next_hop,
        };
        self.peer.send_update(msg).await
    }

    /// Returns all routes received for a specific VNI.
    pub fn get_received_routes(&self, vni: Vni) -> Vec<Route> {
        self.peer.state().received_routes.get_routes_by_vni(vni)
    }

    /// Returns all received routes.
    pub fn get_all_received_routes(&self) -> Vec<(Vni, Route)> {
        self.peer.state().received_routes.get_all_routes()
    }

    /// Returns all announced routes.
    pub fn get_announced_routes(&self) -> Vec<(Vni, Route)> {
        self.peer.state().announced_routes.get_all_routes()
    }

    /// Shuts down the client and closes the connection.
    pub async fn shutdown(self) -> Result<()> {
        self.peer.shutdown().await
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
