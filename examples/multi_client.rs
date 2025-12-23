//! Example multi-server MetalBond client for HA deployments.
//!
//! Connect to multiple MetalBond servers simultaneously for high availability.
//! Routes are deduplicated across servers (same route from multiple servers only reported once).
//! Different next-hops for the same destination are combined (ECMP support).
//!
//! Usage:
//!   cargo run --example multi_client -- <server1> <server2> [...] <vni> [announce...]
//!
//! Announce format: prefix#nexthop[#type][#fromPort#toPort]
//!   - type: STD (default), NAT, or LB
//!   - fromPort/toPort: required for NAT type
//!
//! Examples:
//!   # Connect to two servers and subscribe to VNI 100
//!   cargo run --example multi_client -- [::1]:4711 [::1]:4712 100
//!
//!   # Connect to three servers and announce a route
//!   cargo run --example multi_client -- [::1]:4711 [::1]:4712 [::1]:4713 100 "10.0.1.0/24#2001:db8::1"

use std::net::Ipv6Addr;
use std::time::Duration;

use rustbond::{Destination, MultiServerClient, MultiServerState, NextHop, Route, RouteHandler, Vni};

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
/// Format: prefix#nexthop[#type][#fromPort#toPort]
fn parse_announcement(s: &str) -> Result<(Destination, NextHop), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = s.split('#').collect();

    if parts.len() < 2 {
        return Err("expected format: prefix#nexthop[#type][#fromPort#toPort]".into());
    }

    // Parse prefix
    let prefix: ipnet::IpNet = parts[0].parse()?;
    let destination = Destination::new(prefix);

    // Parse next hop address
    let addr: Ipv6Addr = parts[1].parse()?;

    // Parse route type (optional)
    let route_type = parts.get(2).map(|s| s.to_uppercase());

    let next_hop = match route_type.as_deref() {
        Some("NAT") => {
            if parts.len() < 5 {
                return Err("NAT routes require: prefix#nexthop#NAT#fromPort#toPort".into());
            }
            let from: u16 = parts[3].parse()?;
            let to: u16 = parts[4].parse()?;
            NextHop::nat(addr, from, to)
        }
        Some("LB") => NextHop::load_balancer(addr),
        Some("STD") | None => NextHop::standard(addr),
        Some(other) => return Err(format!("unknown route type: {} (use STD, NAT, or LB)", other).into()),
    };

    Ok((destination, next_hop))
}

/// Parse command line args into (servers, vni, announcements)
fn parse_args(args: &[String]) -> Result<(Vec<&str>, u32, Vec<&str>), String> {
    if args.len() < 3 {
        return Err("need at least 2 arguments".into());
    }

    // Find the first numeric argument - that's the VNI
    // Everything before it is a server address
    let mut vni_index = None;
    for (i, arg) in args.iter().skip(1).enumerate() {
        if arg.parse::<u32>().is_ok() && !arg.contains(':') && !arg.contains('.') {
            vni_index = Some(i + 1);
            break;
        }
    }

    let vni_index = vni_index.ok_or("could not find VNI argument")?;

    if vni_index < 2 {
        return Err("need at least one server address before VNI".into());
    }

    let servers: Vec<&str> = args[1..vni_index].iter().map(|s| s.as_str()).collect();
    let vni: u32 = args[vni_index].parse().map_err(|_| "invalid VNI")?;
    let announcements: Vec<&str> = args[vni_index + 1..].iter().map(|s| s.as_str()).collect();

    Ok((servers, vni, announcements))
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

    // Parse arguments
    let args: Vec<String> = std::env::args().collect();

    let (servers, vni, announcements) = match parse_args(&args) {
        Ok(result) => result,
        Err(_) => {
            eprintln!("Usage: {} <server1> <server2> [...] <vni> [announce...]", args[0]);
            eprintln!();
            eprintln!("Multi-server client for HA deployments.");
            eprintln!("Routes are deduplicated across servers (ECMP supported).");
            eprintln!();
            eprintln!("Announce format: prefix#nexthop[#type][#fromPort#toPort]");
            eprintln!("  type: STD (default), NAT, or LB");
            eprintln!();
            eprintln!("Examples:");
            eprintln!("  {} [::1]:4711 [::1]:4712 100", args[0]);
            eprintln!("  {} [::1]:4711 [::1]:4712 100 \"10.0.1.0/24#2001:db8::1\"", args[0]);
            std::process::exit(1);
        }
    };

    println!("Connecting to {} MetalBond servers:", servers.len());
    for server in &servers {
        println!("  - {}", server);
    }

    // Create multi-server client
    let handler = PrintHandler;
    let client = MultiServerClient::connect(&servers, handler);

    // Wait for at least one connection
    println!("\nWaiting for connections...");
    client.wait_any_established_timeout(Duration::from_secs(10)).await?;

    // Report connection status
    let state = client.state().await;
    match state {
        MultiServerState::FullyConnected => {
            println!("All {} servers connected!", servers.len());
        }
        MultiServerState::PartiallyConnected { connected, total } => {
            println!("Connected to {}/{} servers (partial connectivity)", connected, total);
        }
        _ => {}
    }

    // Subscribe to VNI
    println!("\nSubscribing to VNI {}...", vni);
    client.subscribe(Vni(vni)).await?;
    println!("Subscribed on all connected servers!");

    // Announce routes
    for announcement in &announcements {
        match parse_announcement(announcement) {
            Ok((destination, next_hop)) => {
                println!(
                    "Announcing: {} via {} (type: {})",
                    destination, next_hop, next_hop.hop_type
                );
                client
                    .announce(Vni(vni), destination, next_hop)
                    .await?;
            }
            Err(e) => {
                eprintln!("Error parsing announcement '{}': {}", announcement, e);
            }
        }
    }

    // Print current routes
    println!("\nCurrent routes for VNI {}:", vni);
    for route in client.get_received_routes(Vni(vni)) {
        println!(
            "  {} via {} (type: {})",
            route.destination, route.next_hop, route.next_hop.hop_type
        );
    }

    // Keep running and print updates
    println!("\nListening for route updates... (Ctrl+C to exit)");
    println!("(Routes are deduplicated - same route from multiple servers shown once)\n");

    // Wait for interrupt
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    client.shutdown().await?;

    Ok(())
}
