//! MetalBond peer connection handling.
//!
//! Manages the TCP connection to a MetalBond server, including:
//! - Connection establishment and reconnection
//! - State machine (CONNECTING -> HELLO_SENT -> HELLO_RECEIVED -> ESTABLISHED)
//! - Keepalive handling
//! - Message sending and receiving

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, timeout, Instant};

use crate::route::RouteTable;
use crate::types::{Action, ConnectionState, Vni};
use crate::wire::Message;
use crate::{Error, Result, Route, RouteHandler};

/// Default keepalive interval in seconds.
const DEFAULT_KEEPALIVE_INTERVAL: u32 = 5;

/// Retry interval range (seconds).
const RETRY_INTERVAL_MIN: u64 = 5;
const RETRY_INTERVAL_MAX: u64 = 10;

/// Commands that can be sent to the peer task.
#[derive(Debug)]
pub enum PeerCommand {
    /// Subscribe to a VNI.
    Subscribe(Vni),
    /// Unsubscribe from a VNI.
    Unsubscribe(Vni),
    /// Send a route update.
    SendUpdate(Message),
    /// Shutdown the peer.
    Shutdown,
}

/// Internal state shared between the peer task and the client.
pub struct PeerState {
    /// Current connection state.
    pub state: RwLock<ConnectionState>,
    /// Subscribed VNIs.
    pub subscriptions: RwLock<HashSet<Vni>>,
    /// Routes received from the server.
    pub received_routes: RouteTable,
    /// Routes announced by this client.
    pub announced_routes: RouteTable,
}

impl PeerState {
    fn new() -> Self {
        Self {
            state: RwLock::new(ConnectionState::Connecting),
            subscriptions: RwLock::new(HashSet::new()),
            received_routes: RouteTable::new(),
            announced_routes: RouteTable::new(),
        }
    }
}

/// A MetalBond peer connection.
pub struct Peer {
    /// Remote server address.
    remote_addr: String,
    /// Shared state.
    state: Arc<PeerState>,
    /// Command sender to the peer task.
    cmd_tx: mpsc::Sender<PeerCommand>,
}

impl Peer {
    /// Creates a new peer and starts the connection task.
    pub fn new<H: RouteHandler>(
        remote_addr: String,
        handler: Arc<H>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let state = Arc::new(PeerState::new());
        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        let task_state = state.clone();
        let task_addr = remote_addr.clone();
        let handle = tokio::spawn(async move {
            peer_task(task_addr, task_state, cmd_rx, handler).await;
        });

        let peer = Self {
            remote_addr,
            state,
            cmd_tx,
        };

        (peer, handle)
    }

    /// Returns the current connection state.
    pub async fn get_state(&self) -> ConnectionState {
        *self.state.state.read().await
    }

    /// Returns the remote address.
    pub fn remote_addr(&self) -> &str {
        &self.remote_addr
    }

    /// Subscribes to a VNI.
    pub async fn subscribe(&self, vni: Vni) -> Result<()> {
        self.cmd_tx
            .send(PeerCommand::Subscribe(vni))
            .await
            .map_err(|_| Error::Closed)
    }

    /// Unsubscribes from a VNI.
    pub async fn unsubscribe(&self, vni: Vni) -> Result<()> {
        self.cmd_tx
            .send(PeerCommand::Unsubscribe(vni))
            .await
            .map_err(|_| Error::Closed)
    }

    /// Sends a route update.
    pub async fn send_update(&self, msg: Message) -> Result<()> {
        self.cmd_tx
            .send(PeerCommand::SendUpdate(msg))
            .await
            .map_err(|_| Error::Closed)
    }

    /// Shuts down the peer connection.
    pub async fn shutdown(&self) -> Result<()> {
        self.cmd_tx
            .send(PeerCommand::Shutdown)
            .await
            .map_err(|_| Error::Closed)
    }

    /// Returns a reference to the shared state.
    pub fn state(&self) -> &Arc<PeerState> {
        &self.state
    }
}

/// The main peer task that handles the connection.
async fn peer_task<H: RouteHandler>(
    remote_addr: String,
    state: Arc<PeerState>,
    mut cmd_rx: mpsc::Receiver<PeerCommand>,
    handler: Arc<H>,
) {
    loop {
        // Try to connect
        *state.state.write().await = ConnectionState::Connecting;
        tracing::info!(addr = %remote_addr, "connecting to MetalBond server");

        let stream = match TcpStream::connect(&remote_addr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(addr = %remote_addr, error = %e, "failed to connect, retrying");
                *state.state.write().await = ConnectionState::Retry;

                // Wait for retry interval or shutdown command
                let retry_delay = Duration::from_secs(
                    RETRY_INTERVAL_MIN
                        + rand::random::<u64>() % (RETRY_INTERVAL_MAX - RETRY_INTERVAL_MIN + 1),
                );

                tokio::select! {
                    _ = tokio::time::sleep(retry_delay) => continue,
                    Some(cmd) = cmd_rx.recv() => {
                        if matches!(cmd, PeerCommand::Shutdown) {
                            tracing::info!("peer shutdown requested");
                            *state.state.write().await = ConnectionState::Closed;
                            return;
                        }
                    }
                }
                continue;
            }
        };

        // Run the connection
        if let Err(e) = run_connection(stream, &state, &mut cmd_rx, &handler).await {
            tracing::warn!(addr = %remote_addr, error = %e, "connection error");
        }

        // Check if we should shutdown or retry
        let current_state = *state.state.read().await;
        if current_state == ConnectionState::Closed {
            return;
        }

        // Clear received routes on disconnect
        for (vni, route) in state.received_routes.get_all_routes() {
            handler.remove_route(vni, route);
        }
        state.received_routes.clear();

        // Retry
        *state.state.write().await = ConnectionState::Retry;
        let retry_delay = Duration::from_secs(
            RETRY_INTERVAL_MIN
                + rand::random::<u64>() % (RETRY_INTERVAL_MAX - RETRY_INTERVAL_MIN + 1),
        );

        tokio::select! {
            _ = tokio::time::sleep(retry_delay) => {},
            Some(cmd) = cmd_rx.recv() => {
                if matches!(cmd, PeerCommand::Shutdown) {
                    tracing::info!("peer shutdown requested");
                    *state.state.write().await = ConnectionState::Closed;
                    return;
                }
            }
        }
    }
}

/// Runs an established TCP connection.
async fn run_connection<H: RouteHandler>(
    mut stream: TcpStream,
    state: &Arc<PeerState>,
    cmd_rx: &mut mpsc::Receiver<PeerCommand>,
    handler: &Arc<H>,
) -> Result<()> {
    let keepalive_interval = DEFAULT_KEEPALIVE_INTERVAL;

    // Send HELLO
    let hello = Message::Hello {
        keepalive_interval,
        is_server: false,
    };
    send_message(&mut stream, &hello).await?;
    *state.state.write().await = ConnectionState::HelloSent;
    tracing::debug!("sent HELLO");

    // Wait for HELLO response
    let server_keepalive = match receive_hello(&mut stream).await? {
        Some(interval) => interval,
        None => return Err(Error::Protocol("expected HELLO response".into())),
    };
    *state.state.write().await = ConnectionState::HelloReceived;
    tracing::debug!(server_keepalive, "received HELLO");

    // Use the minimum keepalive interval
    let negotiated_keepalive = keepalive_interval.min(server_keepalive);
    let keepalive_duration = Duration::from_secs(negotiated_keepalive as u64);
    let keepalive_timeout = keepalive_duration * 5 / 2;

    // Send initial KEEPALIVE
    send_message(&mut stream, &Message::Keepalive).await?;
    tracing::trace!("sent KEEPALIVE");

    // Wait for KEEPALIVE response to establish connection
    let msg = receive_message_with_timeout(&mut stream, keepalive_timeout).await?;
    if !matches!(msg, Message::Keepalive) {
        return Err(Error::Protocol("expected KEEPALIVE response".into()));
    }
    *state.state.write().await = ConnectionState::Established;
    tracing::info!("connection established");

    // Re-subscribe to VNIs and re-announce routes
    {
        let subs = state.subscriptions.read().await;
        for &vni in subs.iter() {
            let msg = Message::Subscribe { vni };
            send_message(&mut stream, &msg).await?;
            tracing::debug!(%vni, "re-subscribed to VNI");
        }
    }
    {
        for (vni, route) in state.announced_routes.get_all_routes() {
            let msg = Message::Update {
                action: Action::Add,
                vni,
                destination: route.destination,
                next_hop: route.next_hop,
            };
            send_message(&mut stream, &msg).await?;
        }
    }

    // Main connection loop
    let mut keepalive_timer = interval(keepalive_duration);
    let mut last_received = Instant::now();
    let mut read_buf = vec![0u8; 65536];
    let mut msg_buf = Vec::new();

    loop {
        tokio::select! {
            // Keepalive timer
            _ = keepalive_timer.tick() => {
                // Check for timeout
                if last_received.elapsed() > keepalive_timeout {
                    tracing::warn!("keepalive timeout");
                    return Err(Error::Timeout);
                }

                // Send keepalive
                send_message(&mut stream, &Message::Keepalive).await?;
                tracing::trace!("sent KEEPALIVE");
            }

            // Incoming data
            result = stream.read(&mut read_buf) => {
                let n = result?;
                if n == 0 {
                    tracing::info!("connection closed by peer");
                    return Ok(());
                }

                msg_buf.extend_from_slice(&read_buf[..n]);
                last_received = Instant::now();

                // Process all complete messages in the buffer
                while !msg_buf.is_empty() {
                    match Message::decode(&msg_buf) {
                        Ok((msg, consumed)) => {
                            msg_buf.drain(..consumed);
                            handle_message(msg, state, handler).await?;
                        }
                        Err(Error::Protocol(ref e)) if e.contains("too short") || e.contains("truncated") => {
                            // Need more data
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }

            // Commands from client
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    PeerCommand::Subscribe(vni) => {
                        let msg = Message::Subscribe { vni };
                        send_message(&mut stream, &msg).await?;
                        state.subscriptions.write().await.insert(vni);
                        tracing::debug!(%vni, "subscribed to VNI");
                    }
                    PeerCommand::Unsubscribe(vni) => {
                        let msg = Message::Unsubscribe { vni };
                        send_message(&mut stream, &msg).await?;
                        state.subscriptions.write().await.remove(&vni);

                        // Remove received routes for this VNI
                        for route in state.received_routes.remove_vni(vni) {
                            handler.remove_route(vni, route);
                        }
                        tracing::debug!(%vni, "unsubscribed from VNI");
                    }
                    PeerCommand::SendUpdate(msg) => {
                        send_message(&mut stream, &msg).await?;
                    }
                    PeerCommand::Shutdown => {
                        tracing::info!("peer shutdown requested");
                        *state.state.write().await = ConnectionState::Closed;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Sends a message over the TCP stream.
async fn send_message(stream: &mut TcpStream, msg: &Message) -> Result<()> {
    let data = msg.encode()?;
    stream.write_all(&data).await?;
    Ok(())
}

/// Receives and decodes a HELLO message.
async fn receive_hello(stream: &mut TcpStream) -> Result<Option<u32>> {
    let mut buf = vec![0u8; 256];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }

    let (msg, _) = Message::decode(&buf[..n])?;
    match msg {
        Message::Hello {
            keepalive_interval, ..
        } => Ok(Some(keepalive_interval)),
        _ => Err(Error::Protocol("expected HELLO message".into())),
    }
}

/// Receives a message with a timeout.
async fn receive_message_with_timeout(
    stream: &mut TcpStream,
    duration: Duration,
) -> Result<Message> {
    let mut buf = vec![0u8; 256];
    let n = timeout(duration, stream.read(&mut buf))
        .await
        .map_err(|_| Error::Timeout)??;

    if n == 0 {
        return Err(Error::Closed);
    }

    let (msg, _) = Message::decode(&buf[..n])?;
    Ok(msg)
}

/// Handles a received message.
async fn handle_message<H: RouteHandler>(
    msg: Message,
    state: &Arc<PeerState>,
    handler: &Arc<H>,
) -> Result<()> {
    match msg {
        Message::Hello { .. } => {
            tracing::warn!("unexpected HELLO message");
        }
        Message::Keepalive => {
            tracing::trace!("received KEEPALIVE");
            // Client doesn't need to respond to keepalives from server
        }
        Message::Subscribe { vni } => {
            tracing::warn!(%vni, "unexpected SUBSCRIBE message (we are client)");
        }
        Message::Unsubscribe { vni } => {
            tracing::warn!(%vni, "unexpected UNSUBSCRIBE message (we are client)");
        }
        Message::Update {
            action,
            vni,
            destination,
            next_hop,
        } => {
            let route = Route::new(destination.clone(), next_hop.clone());
            match action {
                Action::Add => {
                    if state.received_routes.add_next_hop(vni, destination, next_hop) {
                        tracing::debug!(%vni, %route, "received route ADD");
                        handler.add_route(vni, route);
                    }
                }
                Action::Remove => {
                    let (removed, _) = state
                        .received_routes
                        .remove_next_hop(vni, &destination, &next_hop);
                    if removed {
                        tracing::debug!(%vni, %route, "received route REMOVE");
                        handler.remove_route(vni, route);
                    }
                }
            }
        }
    }
    Ok(())
}
