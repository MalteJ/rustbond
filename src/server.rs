//! MetalBond server implementation.
//!
//! The server accepts connections from clients, manages subscriptions,
//! and distributes routes between subscribers.
//!
//! ## Architecture
//!
//! The server uses an actor model for routing state management:
//! - A single `RoutingActor` owns all routing state (no locks needed)
//! - Peer handlers send commands to the actor via a channel
//! - The actor processes commands sequentially and distributes updates
//!
//! ## Features
//!
//! - **VNI-based routing**: Routes are organized by Virtual Network Identifier
//! - **Automatic cleanup**: When a client disconnects, all its routes are withdrawn
//! - **Route distribution**: Updates are forwarded to all subscribers of a VNI
//! - **Keepalive negotiation**: Server and client agree on the minimum keepalive interval
//!
//! ## Protocol Flow
//!
//! 1. Client connects and sends `HELLO` with its keepalive interval
//! 2. Server responds with `HELLO` containing its interval
//! 3. Client sends `KEEPALIVE`, server responds with `KEEPALIVE`
//! 4. Connection is established; client can now subscribe and announce routes
//!
//! ## Example
//!
//! ```no_run
//! # use rustbond::{MetalBondServer, ServerConfig};
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Start server with default configuration
//! let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await?;
//!
//! // Check connected peers
//! println!("Connected peers: {}", server.peer_count().await);
//!
//! // Shutdown when done
//! server.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::time::Instant;

use crate::route::RouteTable;
use crate::types::{Action, ConnectionState, Destination, NextHop, Vni};
use crate::wire::Message;
use crate::{Error, Result, Route};

/// Default keepalive interval in seconds.
const DEFAULT_KEEPALIVE_INTERVAL: u32 = 5;

/// Maximum pending messages per peer (slow consumer protection).
/// Large enough to handle bursts, but bounded to prevent OOM.
const MAX_PENDING_MESSAGES: usize = 50_000;


/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Keepalive interval in seconds.
    pub keepalive_interval: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
        }
    }
}

/// A MetalBond server.
pub struct MetalBondServer {
    /// Server state shared between all peer handlers.
    state: Arc<ServerState>,
    /// Handle to send commands to the routing actor.
    routing: RoutingHandle,
    /// Shutdown signal sender for accept loop.
    shutdown_tx: mpsc::Sender<()>,
    /// Shutdown broadcast sender for peer handlers.
    peer_shutdown_tx: broadcast::Sender<()>,
    /// Server task handle.
    _task: tokio::task::JoinHandle<()>,
    /// Routing actor task handle.
    _routing_task: tokio::task::JoinHandle<()>,
}

/// Shared server state.
///
/// Note: Routing state is owned by the RoutingActor, not stored here.
pub struct ServerState {
    /// Server configuration.
    pub config: ServerConfig,
    /// Handle to send commands to the routing actor.
    routing: RoutingHandle,
    /// Connected peers (for keepalive tracking, not distribution).
    pub peers: RwLock<HashMap<SocketAddr, PeerHandle>>,
}

/// Handle to communicate with a connected peer.
#[derive(Debug, Clone)]
pub struct PeerHandle {
    /// Sender to send messages to this peer.
    pub tx: mpsc::Sender<Message>,
    /// Peer's connection state.
    pub state: ConnectionState,
}

// ============================================================================
// Routing Actor
// ============================================================================

/// Commands sent to the routing actor.
enum RoutingCommand {
    /// Register a new peer connection.
    PeerConnected {
        addr: SocketAddr,
        tx: mpsc::Sender<Message>,
    },
    /// Peer disconnected - clean up routes and subscriptions.
    PeerDisconnected {
        addr: SocketAddr,
    },
    /// Subscribe to a VNI (returns existing routes).
    Subscribe {
        addr: SocketAddr,
        vni: Vni,
        response_tx: oneshot::Sender<Vec<(Destination, NextHop)>>,
    },
    /// Unsubscribe from a VNI.
    Unsubscribe {
        addr: SocketAddr,
        vni: Vni,
    },
    /// Route update (add or remove).
    Update {
        from_addr: SocketAddr,
        action: Action,
        vni: Vni,
        destination: Destination,
        next_hop: NextHop,
    },
    /// Get statistics.
    GetStats {
        response_tx: oneshot::Sender<(usize, usize, usize)>,
    },
}

/// Actor that owns all routing state.
///
/// Processes commands sequentially - no locks needed.
struct RoutingActor {
    /// Route table: VNI -> Destination -> Set<NextHop>
    routes: RouteTable,
    /// Route ownership: which peer announced each route
    route_owners: HashMap<(Vni, Destination, NextHop), SocketAddr>,
    /// Subscriptions: VNI -> Set of subscribed peer addresses
    subscriptions: HashMap<Vni, HashSet<SocketAddr>>,
    /// Peer channels for distribution (bounded for slow consumer protection)
    peers: HashMap<SocketAddr, mpsc::Sender<Message>>,
    /// Command receiver
    cmd_rx: mpsc::Receiver<RoutingCommand>,
}

impl RoutingActor {
    fn new(cmd_rx: mpsc::Receiver<RoutingCommand>) -> Self {
        Self {
            routes: RouteTable::new(),
            route_owners: HashMap::new(),
            subscriptions: HashMap::new(),
            peers: HashMap::new(),
            cmd_rx,
        }
    }

    async fn run(mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                RoutingCommand::PeerConnected { addr, tx } => {
                    self.peers.insert(addr, tx);
                    tracing::debug!(peer = %addr, "peer registered with routing actor");
                }
                RoutingCommand::PeerDisconnected { addr } => {
                    self.handle_peer_disconnected(addr);
                }
                RoutingCommand::Subscribe { addr, vni, response_tx } => {
                    let routes = self.handle_subscribe(addr, vni);
                    let _ = response_tx.send(routes);
                }
                RoutingCommand::Unsubscribe { addr, vni } => {
                    self.handle_unsubscribe(addr, vni);
                }
                RoutingCommand::Update { from_addr, action, vni, destination, next_hop } => {
                    self.handle_update(from_addr, action, vni, destination, next_hop);
                }
                RoutingCommand::GetStats { response_tx } => {
                    let stats = (
                        self.routes.len(),
                        self.route_owners.len(),
                        self.peers.len(),
                    );
                    let _ = response_tx.send(stats);
                }
            }
        }
        tracing::debug!("routing actor shutting down");
    }

    fn handle_subscribe(&mut self, addr: SocketAddr, vni: Vni) -> Vec<(Destination, NextHop)> {
        self.subscriptions.entry(vni).or_default().insert(addr);
        tracing::info!(peer = %addr, %vni, "subscribed");

        // Collect existing routes for this VNI
        self.routes.get_destinations_by_vni(vni)
            .into_iter()
            .flat_map(|(dest, hops)| {
                hops.into_iter().map(move |hop| (dest.clone(), hop))
            })
            .collect()
    }

    fn handle_unsubscribe(&mut self, addr: SocketAddr, vni: Vni) {
        if let Some(peers) = self.subscriptions.get_mut(&vni) {
            peers.remove(&addr);
            if peers.is_empty() {
                self.subscriptions.remove(&vni);
            }
        }
        tracing::info!(peer = %addr, %vni, "unsubscribed");
    }

    fn handle_update(
        &mut self,
        from_addr: SocketAddr,
        action: Action,
        vni: Vni,
        destination: Destination,
        next_hop: NextHop,
    ) {
        let route = Route::new(destination.clone(), next_hop.clone());

        match action {
            Action::Add => {
                tracing::debug!(
                    peer = %from_addr,
                    %vni,
                    %route,
                    hop_type = %next_hop.hop_type,
                    "route ADD"
                );
                self.routes.add_next_hop(vni, destination.clone(), next_hop.clone());
                self.route_owners.insert((vni, destination.clone(), next_hop.clone()), from_addr);
            }
            Action::Remove => {
                tracing::debug!(
                    peer = %from_addr,
                    %vni,
                    %route,
                    hop_type = %next_hop.hop_type,
                    "route REMOVE"
                );
                self.routes.remove_next_hop(vni, &destination, &next_hop);
                self.route_owners.remove(&(vni, destination.clone(), next_hop.clone()));
            }
        }

        // Distribute to subscribers (excluding sender)
        let msg = Message::Update {
            action,
            vni,
            destination: route.destination,
            next_hop: route.next_hop,
        };

        let mut slow_consumers = Vec::new();

        if let Some(subs) = self.subscriptions.get(&vni) {
            for &addr in subs {
                if addr != from_addr {
                    if let Some(tx) = self.peers.get(&addr) {
                        // try_send is non-blocking; Full means 50k messages backed up = slow consumer
                        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg.clone()) {
                            tracing::warn!(
                                peer = %addr,
                                "slow consumer detected (50k messages backed up), disconnecting"
                            );
                            slow_consumers.push(addr);
                        }
                    }
                }
            }
        }

        // Remove slow consumers - they'll reconnect and get full sync
        for addr in slow_consumers {
            self.peers.remove(&addr);
            for subs in self.subscriptions.values_mut() {
                subs.remove(&addr);
            }
        }
    }

    fn handle_peer_disconnected(&mut self, addr: SocketAddr) {
        tracing::info!(peer = %addr, "cleaning up peer");

        // Remove peer channel
        self.peers.remove(&addr);

        // Remove from all subscriptions
        for peers in self.subscriptions.values_mut() {
            peers.remove(&addr);
        }
        self.subscriptions.retain(|_, peers| !peers.is_empty());

        // Collect and remove owned routes, along with their subscribers
        let owned_routes: Vec<_> = self.route_owners
            .iter()
            .filter(|(_, &owner)| owner == addr)
            .map(|((vni, dest, hop), _)| (*vni, dest.clone(), hop.clone()))
            .collect();

        let mut slow_consumers = Vec::new();

        for (vni, destination, next_hop) in owned_routes {
            self.routes.remove_next_hop(vni, &destination, &next_hop);
            self.route_owners.remove(&(vni, destination.clone(), next_hop.clone()));

            // Distribute remove to subscribers
            let msg = Message::Update {
                action: Action::Remove,
                vni,
                destination,
                next_hop,
            };

            if let Some(subs) = self.subscriptions.get(&vni) {
                for sub_addr in subs {
                    if let Some(tx) = self.peers.get(sub_addr) {
                        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg.clone()) {
                            if !slow_consumers.contains(sub_addr) {
                                tracing::warn!(
                                    peer = %sub_addr,
                                    "slow consumer detected (50k messages backed up), disconnecting"
                                );
                                slow_consumers.push(*sub_addr);
                            }
                        }
                    }
                }
            }
        }

        // Remove slow consumers
        for addr in slow_consumers {
            self.peers.remove(&addr);
            for subs in self.subscriptions.values_mut() {
                subs.remove(&addr);
            }
        }
    }
}

/// Handle for sending commands to the routing actor.
#[derive(Clone)]
struct RoutingHandle {
    tx: mpsc::Sender<RoutingCommand>,
}

impl MetalBondServer {
    /// Creates and starts a new MetalBond server.
    ///
    /// # Arguments
    ///
    /// * `listen_addr` - Address to listen on (e.g., `"[::]:4711"`)
    /// * `config` - Server configuration
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use rustbond::{MetalBondServer, ServerConfig};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start(
        listen_addr: impl AsRef<str>,
        config: ServerConfig,
    ) -> Result<Self> {
        let listener = TcpListener::bind(listen_addr.as_ref()).await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(addr = %local_addr, "MetalBond server listening");

        // Create routing actor with large command buffer
        let (routing_tx, routing_rx) = mpsc::channel::<RoutingCommand>(8192);
        let routing_handle = RoutingHandle { tx: routing_tx };
        let routing_actor = RoutingActor::new(routing_rx);
        let routing_task = tokio::spawn(routing_actor.run());

        let state = Arc::new(ServerState {
            config,
            routing: routing_handle.clone(),
            peers: RwLock::new(HashMap::new()),
        });

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (peer_shutdown_tx, _) = broadcast::channel::<()>(1);

        let task_state = state.clone();
        let task_peer_shutdown = peer_shutdown_tx.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                tracing::info!(peer = %addr, "new connection");
                                let peer_state = task_state.clone();
                                let mut peer_shutdown_rx = task_peer_shutdown.subscribe();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_peer(stream, addr, peer_state, &mut peer_shutdown_rx).await {
                                        tracing::warn!(peer = %addr, error = %e, "peer error");
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "accept error");
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("server shutting down");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            state,
            routing: routing_handle,
            shutdown_tx,
            peer_shutdown_tx,
            _task: task,
            _routing_task: routing_task,
        })
    }

    /// Returns a reference to the server state.
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Returns the number of connected peers.
    pub async fn peer_count(&self) -> usize {
        self.state.peers.read().await.len()
    }

    /// Returns the number of routes in the server.
    pub async fn route_count(&self) -> usize {
        let (routes, _) = self.route_stats().await;
        routes
    }

    /// Returns detailed route statistics for debugging.
    /// Returns (routes_count, owners_count).
    pub async fn route_stats(&self) -> (usize, usize) {
        let (response_tx, response_rx) = oneshot::channel();
        let _ = self.routing.tx.send(RoutingCommand::GetStats { response_tx }).await;
        match response_rx.await {
            Ok((routes, owners, _peers)) => (routes, owners),
            Err(_) => (0, 0),
        }
    }

    /// Shuts down the server and closes all peer connections.
    pub async fn shutdown(self) -> Result<()> {
        // Signal all peer handlers to shutdown
        let _ = self.peer_shutdown_tx.send(());
        // Signal the accept loop to stop
        let _ = self.shutdown_tx.send(()).await;
        Ok(())
    }
}

/// Handle a single peer connection.
async fn handle_peer(
    mut stream: TcpStream,
    addr: SocketAddr,
    state: Arc<ServerState>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<()> {
    let keepalive_interval = state.config.keepalive_interval;

    // Wait for HELLO from client
    let client_keepalive = match receive_hello(&mut stream).await? {
        Some((interval, _is_server)) => interval,
        None => return Err(Error::Protocol("expected HELLO".into())),
    };
    tracing::debug!(peer = %addr, client_keepalive, "received HELLO");

    // Send HELLO response
    let hello = Message::Hello {
        keepalive_interval,
        is_server: true,
    };
    send_message(&mut stream, &hello).await?;
    tracing::debug!(peer = %addr, "sent HELLO");

    // Use minimum keepalive interval
    let negotiated_keepalive = keepalive_interval.min(client_keepalive);
    let keepalive_duration = Duration::from_secs(negotiated_keepalive as u64);
    let keepalive_timeout = keepalive_duration * 5 / 2;

    // Wait for KEEPALIVE from client
    let msg = receive_message_with_timeout(&mut stream, keepalive_timeout).await?;
    if !matches!(msg, Message::Keepalive) {
        return Err(Error::Protocol("expected KEEPALIVE".into()));
    }
    tracing::debug!(peer = %addr, "received KEEPALIVE");

    // Send KEEPALIVE response
    send_message(&mut stream, &Message::Keepalive).await?;
    tracing::info!(peer = %addr, "connection established");

    // Create bounded channel for sending messages to this peer
    // Large capacity to handle bursts, bounded to prevent OOM
    let (tx, mut rx) = mpsc::channel::<Message>(MAX_PENDING_MESSAGES);

    // Register peer with routing actor (for distribution)
    let _ = state.routing.tx.send(RoutingCommand::PeerConnected {
        addr,
        tx: tx.clone(),
    }).await;

    // Also register in peers map (for keepalive tracking)
    {
        let mut peers = state.peers.write().await;
        peers.insert(
            addr,
            PeerHandle {
                tx: tx.clone(),
                state: ConnectionState::Established,
            },
        );
    }

    // Main connection loop
    let mut last_received = Instant::now();
    let mut read_buf = vec![0u8; 65536];
    let mut msg_buf = Vec::new();

    let result: Result<()> = 'outer: loop {
        tokio::select! {
            // Server shutdown signal
            _ = shutdown_rx.recv() => {
                tracing::info!(peer = %addr, "server shutdown, closing connection");
                break Ok(());
            }

            // Check for timeout
            _ = tokio::time::sleep(keepalive_timeout) => {
                if last_received.elapsed() > keepalive_timeout {
                    tracing::warn!(peer = %addr, "keepalive timeout");
                    break Err(Error::Timeout);
                }
            }

            // Incoming data from peer
            result = stream.read(&mut read_buf) => {
                let n = match result {
                    Ok(0) => {
                        tracing::info!(peer = %addr, "connection closed by peer");
                        break Ok(());
                    }
                    Ok(n) => n,
                    Err(e) => break Err(e.into()),
                };

                msg_buf.extend_from_slice(&read_buf[..n]);
                last_received = Instant::now();

                // Process all complete messages
                while !msg_buf.is_empty() {
                    match Message::decode(&msg_buf) {
                        Ok((msg, consumed)) => {
                            msg_buf.drain(..consumed);
                            if let Err(e) = handle_message(&mut stream, addr, msg, &state).await {
                                tracing::warn!(peer = %addr, error = %e, "message handling error");
                            }
                        }
                        Err(Error::Protocol(ref e)) if e.contains("too short") || e.contains("truncated") => {
                            break;
                        }
                        Err(e) => break 'outer Err(e),
                    }
                }
            }

            // Outgoing messages to peer - batched for efficiency
            Some(msg) = rx.recv() => {
                // Collect all available messages into a batch
                let mut batch = Vec::with_capacity(256);
                batch.push(msg);

                // Drain pending messages up to batch limit
                while batch.len() < 1000 {
                    match rx.try_recv() {
                        Ok(m) => batch.push(m),
                        Err(_) => break,
                    }
                }

                // Encode all messages into a single buffer
                let mut data = Vec::with_capacity(batch.len() * 64);
                for m in &batch {
                    if let Ok(encoded) = m.encode() {
                        data.extend_from_slice(&encoded);
                    }
                }

                // Single write for the entire batch
                if let Err(e) = stream.write_all(&data).await {
                    break Err(e.into());
                }
            }
        }
    };

    // Cleanup: remove from peers map
    {
        let mut peers = state.peers.write().await;
        peers.remove(&addr);
    }

    // Notify routing actor to cleanup routes and subscriptions
    let _ = state.routing.tx.send(RoutingCommand::PeerDisconnected { addr }).await;

    result
}

/// Handle a message from a peer.
async fn handle_message(
    stream: &mut TcpStream,
    addr: SocketAddr,
    msg: Message,
    state: &Arc<ServerState>,
) -> Result<()> {
    match msg {
        Message::Hello { .. } => {
            tracing::warn!(peer = %addr, "unexpected HELLO");
        }
        Message::Keepalive => {
            tracing::trace!(peer = %addr, "received KEEPALIVE");
            // Server responds to client keepalives
            send_message(stream, &Message::Keepalive).await?;
        }
        Message::Subscribe { vni } => {
            tracing::debug!(peer = %addr, %vni, "SUBSCRIBE");
            handle_subscribe(stream, addr, vni, state).await?;
        }
        Message::Unsubscribe { vni } => {
            tracing::debug!(peer = %addr, %vni, "UNSUBSCRIBE");
            handle_unsubscribe(addr, vni, state).await?;
        }
        Message::Update {
            action,
            vni,
            destination,
            next_hop,
        } => {
            handle_update(addr, action, vni, destination, next_hop, state).await?;
        }
    }
    Ok(())
}

/// Handle SUBSCRIBE message.
async fn handle_subscribe(
    stream: &mut TcpStream,
    addr: SocketAddr,
    vni: Vni,
    state: &Arc<ServerState>,
) -> Result<()> {
    // Send subscribe command to actor and get existing routes
    let (response_tx, response_rx) = oneshot::channel();
    let _ = state.routing.tx.send(RoutingCommand::Subscribe {
        addr,
        vni,
        response_tx,
    }).await;

    // Wait for routes from actor
    let routes = response_rx.await.unwrap_or_default();

    // Send existing routes for this VNI
    for (dest, next_hop) in routes {
        let msg = Message::Update {
            action: Action::Add,
            vni,
            destination: dest,
            next_hop,
        };
        send_message(stream, &msg).await?;
    }

    Ok(())
}

/// Handle UNSUBSCRIBE message.
async fn handle_unsubscribe(
    addr: SocketAddr,
    vni: Vni,
    state: &Arc<ServerState>,
) -> Result<()> {
    let _ = state.routing.tx.send(RoutingCommand::Unsubscribe { addr, vni }).await;
    Ok(())
}

/// Handle UPDATE message.
async fn handle_update(
    from_addr: SocketAddr,
    action: Action,
    vni: Vni,
    destination: Destination,
    next_hop: NextHop,
    state: &Arc<ServerState>,
) -> Result<()> {
    // Send update command to actor (blocking to ensure delivery)
    // Actor handles state update and distribution
    let _ = state.routing.tx.send(RoutingCommand::Update {
        from_addr,
        action,
        vni,
        destination,
        next_hop,
    }).await;
    Ok(())
}

/// Send a message over the TCP stream.
async fn send_message(stream: &mut TcpStream, msg: &Message) -> Result<()> {
    let data = msg.encode()?;
    stream.write_all(&data).await?;
    Ok(())
}

/// Receive and decode a HELLO message.
async fn receive_hello(stream: &mut TcpStream) -> Result<Option<(u32, bool)>> {
    let mut buf = vec![0u8; 256];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }

    let (msg, _) = Message::decode(&buf[..n])?;
    match msg {
        Message::Hello {
            keepalive_interval,
            is_server,
        } => Ok(Some((keepalive_interval, is_server))),
        _ => Err(Error::Protocol("expected HELLO message".into())),
    }
}

/// Receive a message with a timeout.
async fn receive_message_with_timeout(
    stream: &mut TcpStream,
    duration: Duration,
) -> Result<Message> {
    let mut buf = vec![0u8; 256];
    let n = tokio::time::timeout(duration, stream.read(&mut buf))
        .await
        .map_err(|_| Error::Timeout)??;

    if n == 0 {
        return Err(Error::Closed);
    }

    let (msg, _) = Message::decode(&buf[..n])?;
    Ok(msg)
}
