//! Sustained throughput stress test for MetalBond server.
//!
//! This benchmark measures real sustained processing performance by running
//! continuous route announce/withdraw cycles for a fixed duration. TCP backpressure
//! naturally throttles clients when the server can't keep up.
//!
//! ## Run
//! ```sh
//! cargo run --release --example stress_test_sustained
//! cargo run --release --example stress_test_sustained -- --clients 100 --duration 120
//! cargo run --release --example stress_test_sustained -- --help
//! ```

use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use rustbond::{
    Destination, MetalBondClient, MetalBondServer, NextHop, Route, RouteHandler, ServerConfig, Vni,
};

/// Sustained throughput stress test for MetalBond server
#[derive(Parser, Debug)]
#[command(name = "stress_test_sustained")]
#[command(about = "Measure sustained MetalBond server throughput")]
struct Args {
    /// Number of concurrent clients
    #[arg(short, long, default_value_t = 50)]
    clients: usize,

    /// Test duration in seconds
    #[arg(short, long, default_value_t = 60)]
    duration: u64,

    /// Routes per announce/withdraw batch
    #[arg(short, long, default_value_t = 100)]
    batch_size: usize,

    /// Server port
    #[arg(short, long, default_value_t = 14901)]
    port: u16,

    /// VNI to use for testing
    #[arg(short, long, default_value_t = 100)]
    vni: u32,

    /// Connection timeout in seconds
    #[arg(long, default_value_t = 30)]
    connect_timeout: u64,

    /// Updates per second per client (0 = unlimited)
    #[arg(short, long, default_value_t = 0)]
    rate: u64,
}

/// Status output interval
const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// Destinations per client (rotating, 2^16 = 65536)
const DESTINATIONS_PER_CLIENT: usize = 65536;

/// Route handler that counts operations and reports to shared state
struct CountingHandler {
    state: Arc<TestState>,
}

impl CountingHandler {
    fn new(state: Arc<TestState>) -> Self {
        Self { state }
    }
}

impl RouteHandler for CountingHandler {
    fn add_route(&self, _vni: Vni, _route: Route) {
        self.state.record_received_add();
    }

    fn remove_route(&self, _vni: Vni, _route: Route) {
        self.state.record_received_remove();
    }
}

/// Generate a destination for a given client and route index (rotating)
fn make_destination(client_id: usize, route_idx: usize) -> Destination {
    // Use rotating index within 65536 range
    let idx = route_idx % DESTINATIONS_PER_CLIENT;
    let b1 = client_id as u8;
    let b2 = (idx >> 8) as u8;
    let b3 = (idx & 0xFF) as u8;
    let prefix = format!("10.{}.{}.{}/32", b1, b2, b3);
    Destination::new(prefix.parse().unwrap())
}

/// Generate a unique next-hop for a given client
fn make_next_hop(client_id: usize) -> NextHop {
    let addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, client_id as u16);
    NextHop::standard(addr)
}

/// Shared state for the sustained test
struct TestState {
    /// Signal for clients to start
    start_signal: AtomicBool,
    /// Signal to stop the test
    stop_signal: AtomicBool,
    /// Count of clients ready
    ready_count: AtomicUsize,
    /// Count of successful announces sent
    sent_announces: AtomicUsize,
    /// Count of successful withdraws sent
    sent_withdraws: AtomicUsize,
    /// Count of route adds received by handlers
    received_adds: AtomicUsize,
    /// Count of route removes received by handlers
    received_removes: AtomicUsize,
    /// Global counter of errors
    total_errors: AtomicUsize,
}

impl TestState {
    fn new() -> Self {
        Self {
            start_signal: AtomicBool::new(false),
            stop_signal: AtomicBool::new(false),
            ready_count: AtomicUsize::new(0),
            sent_announces: AtomicUsize::new(0),
            sent_withdraws: AtomicUsize::new(0),
            received_adds: AtomicUsize::new(0),
            received_removes: AtomicUsize::new(0),
            total_errors: AtomicUsize::new(0),
        }
    }

    fn mark_ready(&self) {
        self.ready_count.fetch_add(1, Ordering::SeqCst);
    }

    fn ready_count(&self) -> usize {
        self.ready_count.load(Ordering::SeqCst)
    }

    fn signal_start(&self) {
        self.start_signal.store(true, Ordering::SeqCst);
    }

    fn should_start(&self) -> bool {
        self.start_signal.load(Ordering::SeqCst)
    }

    fn signal_stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }

    fn should_stop(&self) -> bool {
        self.stop_signal.load(Ordering::SeqCst)
    }

    fn record_announce(&self) {
        self.sent_announces.fetch_add(1, Ordering::Relaxed);
    }

    fn record_withdraw(&self) {
        self.sent_withdraws.fetch_add(1, Ordering::Relaxed);
    }

    fn record_received_add(&self) {
        self.received_adds.fetch_add(1, Ordering::Relaxed);
    }

    fn record_received_remove(&self) {
        self.received_removes.fetch_add(1, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.total_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn get_sent_announces(&self) -> usize {
        self.sent_announces.load(Ordering::Relaxed)
    }

    fn get_sent_withdraws(&self) -> usize {
        self.sent_withdraws.load(Ordering::Relaxed)
    }

    fn get_received_adds(&self) -> usize {
        self.received_adds.load(Ordering::Relaxed)
    }

    fn get_received_removes(&self) -> usize {
        self.received_removes.load(Ordering::Relaxed)
    }

    fn get_errors(&self) -> usize {
        self.total_errors.load(Ordering::Relaxed)
    }
}

/// Client configuration passed to each worker
#[derive(Clone)]
struct ClientConfig {
    server_addr: String,
    vni: Vni,
    batch_size: usize,
    connect_timeout: Duration,
    /// Updates per second (0 = unlimited)
    rate_limit: u64,
}

/// Run a single client's sustained workload
async fn run_client(
    client_id: usize,
    config: ClientConfig,
    state: Arc<TestState>,
) {
    let handler = Arc::new(CountingHandler::new(state.clone()));
    let client = MetalBondClient::connect(&[config.server_addr.as_str()], handler.clone());

    // Wait for connection
    if let Err(e) = client.wait_established_timeout(config.connect_timeout).await {
        eprintln!("Client {} connect failed: {}", client_id, e);
        state.record_error();
        return;
    }

    // Subscribe to VNI
    if let Err(e) = client.subscribe(config.vni).await {
        eprintln!("Client {} subscribe failed: {}", client_id, e);
        state.record_error();
        return;
    }

    // Signal ready and wait for start
    state.mark_ready();
    while !state.should_start() {
        tokio::time::sleep(Duration::from_micros(100)).await;
    }

    // Announce/withdraw routes continuously until stop signal
    let next_hop = make_next_hop(client_id);
    let mut route_idx: usize = 0;
    let mut announced: Vec<Destination> = Vec::with_capacity(config.batch_size);

    // Rate limiting: calculate delay between operations
    let delay = if config.rate_limit > 0 {
        Some(Duration::from_secs_f64(1.0 / config.rate_limit as f64))
    } else {
        None
    };

    while !state.should_stop() {
        // Announce a batch of routes
        for _ in 0..config.batch_size {
            if state.should_stop() {
                break;
            }
            let dest = make_destination(client_id, route_idx);
            route_idx = route_idx.wrapping_add(1) % DESTINATIONS_PER_CLIENT;

            match client.announce(config.vni, dest.clone(), next_hop.clone()).await {
                Ok(_) => {
                    state.record_announce();
                    announced.push(dest);
                }
                Err(_) => {
                    state.record_error();
                }
            }

            // Apply rate limit
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
        }

        // Withdraw the announced routes
        for dest in announced.drain(..) {
            if state.should_stop() {
                break;
            }
            match client.withdraw(config.vni, dest, next_hop.clone()).await {
                Ok(_) => state.record_withdraw(),
                Err(_) => state.record_error(),
            }

            // Apply rate limit
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
        }
    }

    // Keep client alive briefly for cleanup
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Status reporter task
async fn status_reporter(state: Arc<TestState>, duration: Duration, num_clients: usize) {
    let start = Instant::now();
    let mut last_sent = 0usize;
    let mut last_recv = 0usize;
    let mut interval_start = Instant::now();

    println!();
    println!(
        "{:>6} {:>10} {:>10} {:>10} {:>10} {:>8}",
        "Time", "Sent", "Received", "Send/s", "Recv/s", "Errors"
    );
    println!("{:-<6} {:-<10} {:-<10} {:-<10} {:-<10} {:-<8}", "", "", "", "", "", "");

    while start.elapsed() < duration {
        tokio::time::sleep(STATUS_INTERVAL).await;

        let sent = state.get_sent_announces() + state.get_sent_withdraws();
        let recv = state.get_received_adds() + state.get_received_removes();
        let errors = state.get_errors();
        let elapsed_secs = start.elapsed().as_secs();

        let interval_elapsed = interval_start.elapsed().as_secs_f64();
        let (send_rate, recv_rate) = if interval_elapsed > 0.0 {
            (
                (sent - last_sent) as f64 / interval_elapsed,
                (recv - last_recv) as f64 / interval_elapsed,
            )
        } else {
            (0.0, 0.0)
        };

        // Expected received = sent * (clients - 1) for fanout
        let expected_recv = sent * (num_clients - 1);
        let recv_ratio = if expected_recv > 0 {
            recv as f64 / expected_recv as f64 * 100.0
        } else {
            100.0
        };

        println!(
            "{:>5}s {:>10} {:>10} {:>10.0} {:>10.0} {:>8} ({:.1}%)",
            elapsed_secs, sent, recv, send_rate, recv_rate, errors, recv_ratio
        );

        last_sent = sent;
        last_recv = recv;
        interval_start = Instant::now();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Disable logging for raw performance measurement
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "error");
    }
    tracing_subscriber::fmt::init();

    let test_duration = Duration::from_secs(args.duration);
    let connect_timeout = Duration::from_secs(args.connect_timeout);
    let server_addr = format!("[::1]:{}", args.port);
    let vni = Vni(args.vni);

    println!("=== MetalBond Sustained Throughput Test ===");
    println!();
    println!("Configuration:");
    println!("  Clients:             {}", args.clients);
    println!("  Duration:            {} seconds", args.duration);
    println!("  Batch size:          {}", args.batch_size);
    println!("  VNI:                 {}", args.vni);
    println!("  Port:                {}", args.port);
    println!("  Rate limit:          {}", if args.rate > 0 { format!("{} ops/sec/client", args.rate) } else { "unlimited".to_string() });
    println!("  Destinations/Client: {}", DESTINATIONS_PER_CLIENT);
    println!();

    // Start server
    print!("Starting server... ");
    let config = ServerConfig::default();
    let server = MetalBondServer::start(&server_addr, config).await?;
    println!("OK");

    // Create shared state
    let state = Arc::new(TestState::new());

    // Create client config
    let client_config = ClientConfig {
        server_addr: server_addr.clone(),
        vni,
        batch_size: args.batch_size,
        connect_timeout,
        rate_limit: args.rate,
    };

    // Spawn all clients
    print!("Spawning {} clients... ", args.clients);
    for client_id in 0..args.clients {
        let cfg = client_config.clone();
        let client_state = state.clone();
        tokio::spawn(async move {
            run_client(client_id, cfg, client_state).await;
        });
    }
    println!("OK");

    // Wait for all clients to connect
    print!("Waiting for all clients to connect... ");
    let connect_start = Instant::now();
    while state.ready_count() < args.clients {
        if connect_start.elapsed() > connect_timeout {
            println!("TIMEOUT");
            println!("Only {} of {} clients connected", state.ready_count(), args.clients);
            state.signal_stop();
            return Err("Connection timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("{} ready", state.ready_count());

    // Verify peer count
    let peer_count = server.peer_count().await;
    if peer_count != args.clients {
        println!(
            "WARNING: Expected {} peers, got {}",
            args.clients, peer_count
        );
    }

    // Start the test
    println!();
    println!("Starting sustained load test for {} seconds...", args.duration);

    let overall_start = Instant::now();
    state.signal_start();

    // Spawn status reporter
    let reporter_state = state.clone();
    let num_clients = args.clients;
    let reporter_handle = tokio::spawn(async move {
        status_reporter(reporter_state, test_duration, num_clients).await;
    });

    // Wait for test duration
    tokio::time::sleep(test_duration).await;

    // Signal stop
    state.signal_stop();
    let overall_elapsed = overall_start.elapsed();

    // Wait for reporter to finish
    let _ = reporter_handle.await;

    // Wait for in-flight distribution to complete
    println!();
    print!("Draining in-flight messages (5s)... ");
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("OK");

    // Brief pause for final stats
    print!("Collecting final stats... ");

    // Get final stats
    let sent_announces = state.get_sent_announces();
    let sent_withdraws = state.get_sent_withdraws();
    let total_sent = sent_announces + sent_withdraws;
    let received_adds = state.get_received_adds();
    let received_removes = state.get_received_removes();
    let total_received = received_adds + received_removes;
    let total_errors = state.get_errors();
    let (routes_len, owners_len) = server.route_stats().await;
    let peer_count = server.peer_count().await;
    println!("OK");

    // Expected received = sent * (clients - 1) for fanout
    let expected_received = total_sent * (args.clients - 1);
    let delivery_ratio = if expected_received > 0 {
        total_received as f64 / expected_received as f64 * 100.0
    } else {
        100.0
    };

    println!();
    println!("=== Final Results ===");
    println!();
    println!("Throughput:");
    println!("  Total Duration:    {:?}", overall_elapsed);
    println!("  Sent Announces:    {}", sent_announces);
    println!("  Sent Withdraws:    {}", sent_withdraws);
    println!("  Total Sent:        {}", total_sent);
    println!(
        "  Send Rate:         {:.0} ops/sec",
        total_sent as f64 / overall_elapsed.as_secs_f64()
    );
    println!();
    println!("Distribution Verification:");
    println!("  Expected Received: {} (sent × {} peers)", expected_received, args.clients - 1);
    println!("  Actual Received:   {}", total_received);
    println!("  Delivery Ratio:    {:.2}%", delivery_ratio);
    println!("  Received Adds:     {}", received_adds);
    println!("  Received Removes:  {}", received_removes);
    println!();
    println!("Errors:");
    println!("  Total Errors:      {}", total_errors);
    println!();
    println!("Server State:");
    println!("  Routes in table:   {}", routes_len);
    println!("  Route owners:      {}", owners_len);
    println!("  Connected peers:   {}", peer_count);

    // Delivery check
    if delivery_ratio < 99.0 {
        println!();
        println!("⚠️  WARNING: Delivery ratio below 99% - updates may be dropped!");
    } else if delivery_ratio >= 100.0 {
        println!();
        println!("✓ All updates delivered successfully");
    }

    // Hard exit - don't wait for cleanup
    println!();
    println!("Done.");
    std::process::exit(0);
}
