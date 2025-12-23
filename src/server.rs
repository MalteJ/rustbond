//! MetalBond server implementation.
//!
//! The server accepts connections from clients, manages subscriptions,
//! and distributes routes between subscribers.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio::time::Instant;

use crate::route::RouteTable;
use crate::types::{Action, ConnectionState, Destination, NextHop, Vni};
use crate::wire::Message;
use crate::{Error, Result, Route};

/// Default keepalive interval in seconds.
const DEFAULT_KEEPALIVE_INTERVAL: u32 = 5;

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
    /// Shutdown signal sender.
    shutdown_tx: mpsc::Sender<()>,
    /// Server task handle.
    _task: tokio::task::JoinHandle<()>,
}

/// Shared server state.
pub struct ServerState {
    /// Server configuration.
    pub config: ServerConfig,
    /// All routes received from clients.
    /// Maps VNI -> Destination -> NextHop -> PeerAddr
    pub routes: RouteTable,
    /// Route ownership: tracks which peer announced each route.
    pub route_owners: RwLock<HashMap<(Vni, Destination, NextHop), SocketAddr>>,
    /// Subscriptions: VNI -> Set of peer addresses.
    pub subscriptions: RwLock<HashMap<Vni, HashSet<SocketAddr>>>,
    /// Connected peers.
    pub peers: RwLock<HashMap<SocketAddr, PeerHandle>>,
}

/// Handle to communicate with a connected peer.
#[derive(Debug)]
pub struct PeerHandle {
    /// Sender to send messages to this peer.
    pub tx: mpsc::Sender<Message>,
    /// Peer's connection state.
    pub state: ConnectionState,
}

impl MetalBondServer {
    /// Creates and starts a new MetalBond server.
    ///
    /// # Arguments
    ///
    /// * `listen_addr` - Address to listen on (e.g., "[::]:4711")
    /// * `config` - Server configuration
    ///
    /// # Example
    ///
    /// ```ignore
    /// let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await?;
    /// ```
    pub async fn start(
        listen_addr: impl AsRef<str>,
        config: ServerConfig,
    ) -> Result<Self> {
        let listener = TcpListener::bind(listen_addr.as_ref()).await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(addr = %local_addr, "MetalBond server listening");

        let state = Arc::new(ServerState {
            config,
            routes: RouteTable::new(),
            route_owners: RwLock::new(HashMap::new()),
            subscriptions: RwLock::new(HashMap::new()),
            peers: RwLock::new(HashMap::new()),
        });

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        let task_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                tracing::info!(peer = %addr, "new connection");
                                let peer_state = task_state.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_peer(stream, addr, peer_state).await {
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
            shutdown_tx,
            _task: task,
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
    pub fn route_count(&self) -> usize {
        self.state.routes.len()
    }

    /// Shuts down the server.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(()).await;
        Ok(())
    }
}

/// Handle a single peer connection.
async fn handle_peer(
    mut stream: TcpStream,
    addr: SocketAddr,
    state: Arc<ServerState>,
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

    // Create channel for sending messages to this peer
    let (tx, mut rx) = mpsc::channel::<Message>(256);

    // Register peer
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

            // Outgoing messages to peer
            Some(msg) = rx.recv() => {
                if let Err(e) = send_message(&mut stream, &msg).await {
                    break Err(e);
                }
            }
        }
    };

    // Cleanup: remove peer and withdraw its routes
    cleanup_peer(addr, &state).await;

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
    // Add to subscriptions
    {
        let mut subs = state.subscriptions.write().await;
        subs.entry(vni).or_default().insert(addr);
    }
    tracing::info!(peer = %addr, %vni, "subscribed");

    // Send existing routes for this VNI
    let routes = state.routes.get_destinations_by_vni(vni);
    for (dest, next_hops) in routes {
        for next_hop in next_hops {
            let msg = Message::Update {
                action: Action::Add,
                vni,
                destination: dest.clone(),
                next_hop,
            };
            send_message(stream, &msg).await?;
        }
    }

    Ok(())
}

/// Handle UNSUBSCRIBE message.
async fn handle_unsubscribe(
    addr: SocketAddr,
    vni: Vni,
    state: &Arc<ServerState>,
) -> Result<()> {
    let mut subs = state.subscriptions.write().await;
    if let Some(peers) = subs.get_mut(&vni) {
        peers.remove(&addr);
        if peers.is_empty() {
            subs.remove(&vni);
        }
    }
    tracing::info!(peer = %addr, %vni, "unsubscribed");
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
    let route = Route::new(destination.clone(), next_hop.clone());

    match action {
        Action::Add => {
            tracing::info!(
                peer = %from_addr,
                %vni,
                %route,
                hop_type = %next_hop.hop_type,
                "received route ADD"
            );

            // Add to route table
            state.routes.add_next_hop(vni, destination.clone(), next_hop.clone());

            // Track ownership
            {
                let mut owners = state.route_owners.write().await;
                owners.insert((vni, destination.clone(), next_hop.clone()), from_addr);
            }

            // Distribute to subscribers
            distribute_route(from_addr, Action::Add, vni, destination, next_hop, state).await?;
        }
        Action::Remove => {
            tracing::info!(
                peer = %from_addr,
                %vni,
                %route,
                hop_type = %next_hop.hop_type,
                "received route REMOVE"
            );

            // Remove from route table
            state.routes.remove_next_hop(vni, &destination, &next_hop);

            // Remove ownership
            {
                let mut owners = state.route_owners.write().await;
                owners.remove(&(vni, destination.clone(), next_hop.clone()));
            }

            // Distribute withdrawal to subscribers
            distribute_route(from_addr, Action::Remove, vni, destination, next_hop, state).await?;
        }
    }

    Ok(())
}

/// Distribute a route update to all subscribers of the VNI.
async fn distribute_route(
    from_addr: SocketAddr,
    action: Action,
    vni: Vni,
    destination: Destination,
    next_hop: NextHop,
    state: &Arc<ServerState>,
) -> Result<()> {
    let msg = Message::Update {
        action,
        vni,
        destination,
        next_hop: next_hop.clone(),
    };

    let subs = state.subscriptions.read().await;
    let peers = state.peers.read().await;

    if let Some(subscribers) = subs.get(&vni) {
        for &peer_addr in subscribers {
            // Don't send routes back to the peer that announced them
            if peer_addr == from_addr {
                continue;
            }

            if let Some(peer) = peers.get(&peer_addr) {
                if let Err(e) = peer.tx.try_send(msg.clone()) {
                    tracing::warn!(peer = %peer_addr, error = %e, "failed to send update");
                }
            }
        }
    }

    Ok(())
}

/// Clean up when a peer disconnects.
async fn cleanup_peer(addr: SocketAddr, state: &Arc<ServerState>) {
    tracing::info!(peer = %addr, "cleaning up peer");

    // Remove from peers
    {
        let mut peers = state.peers.write().await;
        peers.remove(&addr);
    }

    // Remove from all subscriptions
    {
        let mut subs = state.subscriptions.write().await;
        for (_, peers) in subs.iter_mut() {
            peers.remove(&addr);
        }
        subs.retain(|_, peers| !peers.is_empty());
    }

    // Withdraw all routes announced by this peer
    let routes_to_remove: Vec<_> = {
        let owners = state.route_owners.read().await;
        owners
            .iter()
            .filter(|(_, &owner)| owner == addr)
            .map(|((vni, dest, hop), _)| (*vni, dest.clone(), hop.clone()))
            .collect()
    };

    for (vni, destination, next_hop) in routes_to_remove {
        tracing::info!(
            peer = %addr,
            %vni,
            dest = %destination,
            hop = %next_hop,
            "withdrawing route"
        );

        // Remove from route table
        state.routes.remove_next_hop(vni, &destination, &next_hop);

        // Remove ownership
        {
            let mut owners = state.route_owners.write().await;
            owners.remove(&(vni, destination.clone(), next_hop.clone()));
        }

        // Notify subscribers
        if let Err(e) = distribute_route(addr, Action::Remove, vni, destination, next_hop, state).await {
            tracing::warn!(error = %e, "failed to distribute route withdrawal");
        }
    }
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
