//! Example MetalBond server.
//!
//! Run a MetalBond server that accepts client connections and distributes routes.
//!
//! Usage:
//!   cargo run --example server -- [listen_address]
//!
//! Examples:
//!   cargo run --example server                    # Listen on [::]:4711
//!   cargo run --example server -- [::]:4711       # Listen on [::]:4711
//!   cargo run --example server -- 0.0.0.0:4711    # Listen on IPv4

use std::time::Duration;

use rustbond::{MetalBondServer, ServerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rustbond=info".parse()?),
        )
        .init();

    // Parse arguments
    let args: Vec<String> = std::env::args().collect();
    let listen_addr = args.get(1).map(|s| s.as_str()).unwrap_or("[::]:4711");

    println!("RustBond Server");
    println!("===============");
    println!();

    // Create server config
    let config = ServerConfig {
        keepalive_interval: 5,
    };

    // Start server
    println!("Starting server on {}...", listen_addr);
    let server = MetalBondServer::start(listen_addr, config).await?;
    println!("Server running!");
    println!();

    // Print status periodically
    let state = server.state().clone();
    let status_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            let peer_count = state.peers.read().await.len();
            let route_count = state.routes.len();
            let sub_count: usize = state
                .subscriptions
                .read()
                .await
                .values()
                .map(|s| s.len())
                .sum();
            println!(
                "[status] peers: {}, routes: {}, subscriptions: {}",
                peer_count, route_count, sub_count
            );
        }
    });

    println!("Press Ctrl+C to stop");
    println!();

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;

    println!();
    println!("Shutting down...");

    status_task.abort();
    server.shutdown().await?;

    println!("Server stopped.");

    Ok(())
}
