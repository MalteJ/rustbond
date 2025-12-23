//! Example MetalBond client.
//!
//! Connect to a MetalBond server, subscribe to a VNI, and print received routes.
//!
//! Usage:
//!   cargo run --example client -- <server> <vni> [announce...]
//!
//! Announce format: prefix#nexthop[#type][#fromPort#toPort]
//!   - type: STD (default), NAT, or LB
//!   - fromPort/toPort: required for NAT type
//!
//! Examples:
//!   # Just subscribe to VNI 100
//!   cargo run --example client -- [::1]:4711 100
//!
//!   # Announce a standard route
//!   cargo run --example client -- [::1]:4711 100 "10.0.1.0/24#2001:db8::1"
//!
//!   # Announce a NAT route with port range
//!   cargo run --example client -- [::1]:4711 100 "10.0.2.0/24#2001:db8::2#NAT#1024#2048"
//!
//!   # Announce a load balancer target
//!   cargo run --example client -- [::1]:4711 100 "10.0.3.0/24#2001:db8::3#LB"

use std::net::Ipv6Addr;
use std::time::Duration;

use rustbond::{Destination, MetalBondClient, NextHop, Route, RouteHandler, Vni};

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

    if args.len() < 3 {
        eprintln!("Usage: {} <server> <vni> [announce...]", args[0]);
        eprintln!();
        eprintln!("Announce format: prefix#nexthop[#type][#fromPort#toPort]");
        eprintln!("  type: STD (default), NAT, or LB");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} [::1]:4711 100", args[0]);
        eprintln!("  {} [::1]:4711 100 \"10.0.1.0/24#2001:db8::1\"", args[0]);
        eprintln!(
            "  {} [::1]:4711 100 \"10.0.2.0/24#2001:db8::2#NAT#1024#2048\"",
            args[0]
        );
        eprintln!("  {} [::1]:4711 100 \"10.0.3.0/24#2001:db8::3#LB\"", args[0]);
        std::process::exit(1);
    }

    let server_addr = &args[1];
    let vni: u32 = args[2].parse()?;
    let announcements: Vec<&String> = args.iter().skip(3).collect();

    println!("Connecting to MetalBond server at {}", server_addr);

    // Create client
    let handler = PrintHandler;
    let client = MetalBondClient::connect(server_addr, handler);

    // Wait for connection
    println!("Waiting for connection...");
    client
        .wait_established_timeout(Duration::from_secs(10))
        .await?;
    println!("Connected!");

    // Subscribe to VNI
    println!("Subscribing to VNI {}...", vni);
    client.subscribe(Vni(vni)).await?;
    println!("Subscribed!");

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
    println!("\nListening for route updates... (Ctrl+C to exit)\n");

    // Wait forever (or until interrupted)
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    client.shutdown().await?;

    Ok(())
}
