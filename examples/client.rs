//! MetalBond client - connect to one or more servers for route distribution.
//!
//! Usage:
//!   cargo run --example client -- -s <server> [-s <server>...] -v <vni> [-a <announce>...]
//!
//! Examples:
//!   # Single server
//!   cargo run --example client -- -s [::1]:4711 -v 100
//!
//!   # Multiple servers (HA mode with route deduplication)
//!   cargo run --example client -- -s [::1]:4711 -s [::1]:4712 -v 100
//!
//!   # Announce routes
//!   cargo run --example client -- -s [::1]:4711 -v 100 -a "10.0.1.0/24@2001:db8::1"
//!
//! Announce format: prefix@nexthop[:type[:fromPort:toPort]]
//!   - type: std (default), nat, or lb
//!   - fromPort/toPort: required for nat type
//!
//! Examples of announce strings:
//!   10.0.1.0/24@2001:db8::1           # standard route
//!   10.0.2.0/24@2001:db8::2:nat:1024:2048  # NAT with port range
//!   10.0.3.0/24@2001:db8::3:lb        # load balancer target

use std::net::Ipv6Addr;
use std::time::Duration;

use clap::Parser;
use rustbond::{Destination, MetalBondClient, MultiServerClient, MultiServerState, NextHop, Route, RouteHandler, Vni};

/// MetalBond client for route distribution
#[derive(Parser, Debug)]
#[command(name = "rustbond-client")]
#[command(about = "Connect to MetalBond servers and manage routes")]
#[command(after_help = "Announce format: prefix@nexthop[:type[:fromPort:toPort]]\n  \
    type: std (default), nat, or lb\n\n\
    Examples:\n  \
    10.0.1.0/24@2001:db8::1              # standard route\n  \
    10.0.2.0/24@2001:db8::2:nat:1024:2048    # NAT with port range\n  \
    10.0.3.0/24@2001:db8::3:lb           # load balancer target")]
struct Args {
    /// Server address(es) to connect to (can be specified multiple times for HA)
    #[arg(short, long = "server", required = true)]
    servers: Vec<String>,

    /// Virtual Network Identifier to subscribe to
    #[arg(short, long)]
    vni: u32,

    /// Route(s) to announce (format: prefix@nexthop[:type[:fromPort:toPort]])
    #[arg(short, long = "announce")]
    announcements: Vec<String>,
}

/// A route handler that logs all route changes.
struct PrintHandler;

impl RouteHandler for PrintHandler {
    fn add_route(&self, vni: Vni, route: Route) {
        println!(
            "[+] VNI {}: {} via {} (type: {})",
            vni, route.destination, route.next_hop, route.next_hop.hop_type
        );
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        println!(
            "[-] VNI {}: {} via {} (type: {})",
            vni, route.destination, route.next_hop, route.next_hop.hop_type
        );
    }
}

/// Parse an announcement string into destination and next hop.
///
/// Format: prefix@nexthop[:type[:fromPort:toPort]]
fn parse_announcement(s: &str) -> Result<(Destination, NextHop), Box<dyn std::error::Error>> {
    let (prefix_str, rest) = s
        .split_once('@')
        .ok_or("expected format: prefix@nexthop[:type[:fromPort:toPort]]")?;

    // Parse prefix
    let prefix: ipnet::IpNet = prefix_str.parse()?;
    let destination = Destination::new(prefix);

    // Split the rest by ':'
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.is_empty() {
        return Err("missing nexthop address".into());
    }

    // Parse next hop address
    let addr: Ipv6Addr = parts[0].parse()?;

    // Parse route type (optional)
    let route_type = parts.get(1).map(|s| s.to_lowercase());

    let next_hop = match route_type.as_deref() {
        Some("nat") => {
            if parts.len() < 4 {
                return Err("nat routes require: prefix@nexthop:nat:fromPort:toPort".into());
            }
            let from: u16 = parts[2].parse()?;
            let to: u16 = parts[3].parse()?;
            NextHop::nat(addr, from, to)
        }
        Some("lb") => NextHop::load_balancer(addr),
        Some("std") | None => NextHop::standard(addr),
        Some(other) => {
            return Err(format!("unknown route type: {} (use std, nat, or lb)", other).into())
        }
    };

    Ok((destination, next_hop))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rustbond=debug".parse()?),
        )
        .init();

    let args = Args::parse();

    if args.servers.len() == 1 {
        run_single_server(args).await
    } else {
        run_multi_server(args).await
    }
}

async fn run_single_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to MetalBond server at {}", args.servers[0]);
    let client = MetalBondClient::connect(&args.servers[0], PrintHandler);

    println!("Waiting for connection...");
    client.wait_established_timeout(Duration::from_secs(10)).await?;
    println!("Connected!");

    println!("Subscribing to VNI {}...", args.vni);
    client.subscribe(Vni(args.vni)).await?;
    println!("Subscribed!");

    for announcement in &args.announcements {
        match parse_announcement(announcement) {
            Ok((destination, next_hop)) => {
                println!("Announcing: {} via {} (type: {})", destination, next_hop, next_hop.hop_type);
                client.announce(Vni(args.vni), destination, next_hop).await?;
            }
            Err(e) => eprintln!("Error parsing announcement '{}': {}", announcement, e),
        }
    }

    println!("\nCurrent routes for VNI {}:", args.vni);
    for route in client.get_received_routes(Vni(args.vni)) {
        println!("  {} via {} (type: {})", route.destination, route.next_hop, route.next_hop.hop_type);
    }

    println!("\nListening for route updates... (Ctrl+C to exit)\n");
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    client.shutdown().await?;
    Ok(())
}

async fn run_multi_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to {} MetalBond servers:", args.servers.len());
    for server in &args.servers {
        println!("  - {}", server);
    }

    let server_refs: Vec<&str> = args.servers.iter().map(|s| s.as_str()).collect();
    let client = MultiServerClient::connect(&server_refs, PrintHandler);

    println!("\nWaiting for connections...");
    client.wait_any_established_timeout(Duration::from_secs(10)).await?;

    match client.state().await {
        MultiServerState::FullyConnected => println!("All {} servers connected!", args.servers.len()),
        MultiServerState::PartiallyConnected { connected, total } => {
            println!("Connected to {}/{} servers (partial connectivity)", connected, total);
        }
        _ => {}
    }

    println!("\nSubscribing to VNI {}...", args.vni);
    client.subscribe(Vni(args.vni)).await?;
    println!("Subscribed on all connected servers!");

    for announcement in &args.announcements {
        match parse_announcement(announcement) {
            Ok((destination, next_hop)) => {
                println!("Announcing: {} via {} (type: {})", destination, next_hop, next_hop.hop_type);
                client.announce(Vni(args.vni), destination, next_hop).await?;
            }
            Err(e) => eprintln!("Error parsing announcement '{}': {}", announcement, e),
        }
    }

    println!("\nCurrent routes for VNI {}:", args.vni);
    for route in client.get_received_routes(Vni(args.vni)) {
        println!("  {} via {} (type: {})", route.destination, route.next_hop, route.next_hop.hop_type);
    }

    println!("\nListening for route updates... (Ctrl+C to exit)");
    println!("(Routes are deduplicated - same route from multiple servers shown once)\n");
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    client.shutdown().await?;
    Ok(())
}
