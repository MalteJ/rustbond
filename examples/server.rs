//! Example MetalBond server.
//!
//! Run a MetalBond server that accepts client connections and distributes routes.
//!
//! Usage:
//!   cargo run --example server -- [--listener <address>]
//!
//! Examples:
//!   cargo run --example server                       # Listen on [::1]:4711
//!   cargo run --example server -- -l [::]:4711       # Listen on all IPv6
//!   cargo run --example server -- -l 0.0.0.0:4711    # Listen on all IPv4

use std::time::Duration;

use clap::Parser;
use rustbond::{MetalBondServer, ServerConfig};

/// MetalBond server for route distribution
#[derive(Parser, Debug)]
#[command(name = "rustbond-server")]
#[command(about = "Run a MetalBond server that distributes routes to clients")]
struct Args {
    /// Address to listen on
    #[arg(short, long, default_value = "[::1]:4711")]
    listener: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (compact single-line format)
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rustbond=info".parse()?),
        )
        .init();

    let args = Args::parse();

    println!("RustBond Server");
    println!("===============");
    println!();

    // Create server config
    let config = ServerConfig {
        keepalive_interval: 5,
    };

    // Start server
    println!("Starting server on {}...", args.listener);
    let server = MetalBondServer::start(&args.listener, config).await?;
    println!("Server running!");
    println!();

    // Print status periodically.
    // The server state is shared via Arc, so we can access it from a background task.
    let state = server.state().clone();
    let status_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            let (peer_count, route_count, sub_count) = state.stats().await;
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
