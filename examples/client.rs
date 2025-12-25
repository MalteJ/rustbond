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
//!   # Announce routes (VNI#prefix@nexthop)
//!   cargo run --example client -- -s [::1]:4711 -v 100 -a "100#10.0.1.0/24@2001:db8::1"
//!
//!   # Install routes to kernel (requires netlink feature and root)
//!   cargo run --example client --features netlink -- -s [::1]:4711 -v 100 \
//!     --install-routes 100#100 --tun ip6tnl0
//!
//! Announce format: VNI#prefix@nexthop[#type[#fromPort#toPort]]
//!   - VNI: Virtual Network Identifier (e.g., 100)
//!   - type: std (default), nat, or lb
//!   - fromPort/toPort: required for nat type
//!
//! Examples of announce strings:
//!   100#10.0.1.0/24@2001:db8::1             # standard route on VNI 100
//!   200#10.0.2.0/24@2001:db8::2#nat#1024#2048  # NAT with port range on VNI 200
//!   100#10.0.3.0/24@2001:db8::3#lb          # load balancer target on VNI 100

use std::net::Ipv6Addr;
use std::time::Duration;

use clap::Parser;
use rustbond::{ClientState, Destination, MetalBondClient, NextHop, Route, RouteHandler, Vni};

#[cfg(feature = "netlink")]
use rustbond::netlink::{NetlinkClient, NetlinkConfig};
#[cfg(feature = "netlink")]
use std::collections::HashMap;
#[cfg(feature = "netlink")]
use std::sync::Arc;

/// MetalBond client for route distribution
#[derive(Parser, Debug)]
#[command(name = "rustbond-client")]
#[command(about = "Connect to MetalBond servers and manage routes")]
#[command(after_help = "Announce format: VNI#prefix@nexthop[#type[#fromPort#toPort]]\n  \
    type: std (default), nat, or lb\n\n\
    Examples:\n  \
    100#10.0.1.0/24@2001:db8::1                # standard route on VNI 100\n  \
    200#10.0.2.0/24@2001:db8::2#nat#1024#2048  # NAT with port range on VNI 200\n  \
    100#10.0.3.0/24@2001:db8::3#lb             # load balancer target on VNI 100\n\n\
    Netlink (requires --features netlink):\n  \
    --install-routes 100#100    # Install VNI 100 routes to table 100\n  \
    --tun ip6tnl0               # Use ip6tnl0 as tunnel device")]
struct Args {
    /// Server address(es) to connect to (can be specified multiple times for HA)
    #[arg(short, long = "server", required = true)]
    servers: Vec<String>,

    /// Virtual Network Identifier to subscribe to
    #[arg(short, long)]
    vni: u32,

    /// Route(s) to announce (format: VNI#prefix@nexthop[#type[#fromPort#toPort]])
    #[arg(short, long = "announce")]
    announcements: Vec<String>,

    /// Install routes to kernel routing table (format: VNI#TABLE, e.g., 100#100)
    /// Requires the netlink feature and root privileges.
    #[arg(long = "install-routes")]
    install_routes: Vec<String>,

    /// Tunnel device name for route installation (e.g., ip6tnl0)
    #[arg(long, default_value = "ip6tnl0")]
    tun: String,
}

/// A route handler that logs all route changes.
///
/// The RouteHandler trait is how you receive route updates from the server.
/// In multi-server mode, routes are deduplicated - you only get one add_route
/// call even if multiple servers announce the same route.
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

/// A route handler that installs routes to the kernel via netlink.
///
/// This demonstrates integrating MetalBond with the Linux routing stack.
/// Routes are installed as they arrive and removed when withdrawn.
#[cfg(feature = "netlink")]
struct NetlinkHandler {
    netlink: Arc<NetlinkClient>,
    runtime: tokio::runtime::Handle,
}

#[cfg(feature = "netlink")]
impl RouteHandler for NetlinkHandler {
    fn add_route(&self, vni: Vni, route: Route) {
        println!(
            "[+] VNI {}: {} via {} (type: {}) -> installing to kernel",
            vni, route.destination, route.next_hop, route.next_hop.hop_type
        );

        let netlink = self.netlink.clone();
        let dest = route.destination.clone();
        let next_hop = route.next_hop.clone();

        self.runtime.spawn(async move {
            if let Err(e) = netlink.add_route(vni, &dest, &next_hop).await {
                eprintln!("Failed to install route: {}", e);
            }
        });
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        println!(
            "[-] VNI {}: {} via {} (type: {}) -> removing from kernel",
            vni, route.destination, route.next_hop, route.next_hop.hop_type
        );

        let netlink = self.netlink.clone();
        let dest = route.destination.clone();
        let next_hop = route.next_hop.clone();

        self.runtime.spawn(async move {
            if let Err(e) = netlink.remove_route(vni, &dest, &next_hop).await {
                eprintln!("Failed to remove route: {}", e);
            }
        });
    }
}

/// Parse an announcement string into VNI, destination and next hop.
///
/// Format: VNI#prefix@nexthop[#type[#fromPort#toPort]]
fn parse_announcement(s: &str) -> Result<(Vni, Destination, NextHop), Box<dyn std::error::Error>> {
    // Split by # to get parts: [VNI, prefix@nexthop, type?, fromPort?, toPort?]
    let parts: Vec<&str> = s.split('#').collect();
    if parts.len() < 2 {
        return Err("expected format: VNI#prefix@nexthop[#type[#fromPort#toPort]]".into());
    }

    // Parse VNI
    let vni: u32 = parts[0]
        .parse()
        .map_err(|_| format!("invalid VNI '{}': must be a number", parts[0]))?;

    // Parse prefix and nexthop
    let (prefix_str, addr_str) = parts[1]
        .split_once('@')
        .ok_or("expected format: VNI#prefix@nexthop[#type[#fromPort#toPort]]")?;

    let prefix: ipnet::IpNet = prefix_str.parse()?;
    let destination = Destination::new(prefix);
    let addr: Ipv6Addr = addr_str.parse()?;

    // Parse optional type and ports
    let next_hop = match parts.get(2).map(|s| s.to_lowercase()).as_deref() {
        Some("nat") => {
            if parts.len() != 5 {
                return Err("nat routes require: VNI#prefix@nexthop#nat#fromPort#toPort".into());
            }
            let from: u16 = parts[3].parse()?;
            let to: u16 = parts[4].parse()?;
            NextHop::nat(addr, from, to)
        }
        Some("lb") => NextHop::load_balancer(addr),
        Some("std") => NextHop::standard(addr),
        None => NextHop::standard(addr),
        Some(t) => return Err(format!("unknown route type '{}': expected std, nat, or lb", t).into()),
    };

    Ok((Vni(vni), destination, next_hop))
}

/// Parse VNI#TABLE mapping string.
#[cfg(feature = "netlink")]
fn parse_vni_table_map(mappings: &[String]) -> Result<HashMap<Vni, u32>, Box<dyn std::error::Error>> {
    let mut map = HashMap::new();

    for mapping in mappings {
        let parts: Vec<&str> = mapping.split('#').collect();
        if parts.len() != 2 {
            return Err(format!("Invalid VNI#TABLE format: '{}' (expected e.g., '100#100')", mapping).into());
        }

        let vni: u32 = parts[0].parse()
            .map_err(|_| format!("Invalid VNI in '{}': must be a number", mapping))?;
        let table: u32 = parts[1].parse()
            .map_err(|_| format!("Invalid table ID in '{}': must be a number", mapping))?;

        map.insert(Vni(vni), table);
    }

    Ok(map)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Check if netlink is requested but feature not enabled
    #[cfg(not(feature = "netlink"))]
    if !args.install_routes.is_empty() {
        eprintln!("Error: --install-routes requires the 'netlink' feature.");
        eprintln!("Rebuild with: cargo run --example client --features netlink -- ...");
        std::process::exit(1);
    }

    // Setup netlink if requested
    #[cfg(feature = "netlink")]
    let netlink_client = if !args.install_routes.is_empty() {
        let vni_table_map = parse_vni_table_map(&args.install_routes)?;

        println!("Initializing netlink with VNI->table mappings:");
        for (vni, table) in &vni_table_map {
            println!("  VNI {} -> table {}", vni, table);
        }

        let config = NetlinkConfig {
            vni_table_map,
            link_name: args.tun.clone(),
        };

        let client = NetlinkClient::new(config).await?;

        // Clean up stale routes from previous runs
        println!("Cleaning up stale routes...");
        client.cleanup_stale_routes().await?;

        Some(Arc::new(client))
    } else {
        None
    };

    #[cfg(feature = "netlink")]
    if let Some(netlink) = netlink_client {
        let handler = NetlinkHandler {
            netlink,
            runtime: tokio::runtime::Handle::current(),
        };
        return run_client(args, handler).await;
    }

    run_client(args, PrintHandler).await
}

/// Main client loop - connects to servers, subscribes, and listens for routes.
///
/// When multiple servers are provided, the client connects to all of them
/// simultaneously for high availability. Routes from all servers are combined,
/// with automatic deduplication.
async fn run_client<H: RouteHandler>(args: Args, handler: H) -> Result<(), Box<dyn std::error::Error>> {
    let server_refs: Vec<&str> = args.servers.iter().map(|s| s.as_str()).collect();

    if args.servers.len() == 1 {
        println!("Connecting to MetalBond server at {}", args.servers[0]);
    } else {
        println!("Connecting to {} MetalBond servers:", args.servers.len());
        for server in &args.servers {
            println!("  - {}", server);
        }
    }

    let client = MetalBondClient::connect(&server_refs, handler);

    println!("\nWaiting for connection...");
    client.wait_established_timeout(Duration::from_secs(10)).await?;

    match client.state() {
        ClientState::FullyConnected => {
            if args.servers.len() == 1 {
                println!("Connected!");
            } else {
                println!("All {} servers connected!", args.servers.len());
            }
        }
        ClientState::PartiallyConnected { connected, total } => {
            println!("Connected to {}/{} servers (partial connectivity)", connected, total);
        }
        _ => {}
    }

    println!("\nSubscribing to VNI {}...", args.vni);
    client.subscribe(Vni(args.vni)).await?;
    println!("Subscribed!");

    for announcement in &args.announcements {
        match parse_announcement(announcement) {
            Ok((vni, destination, next_hop)) => {
                println!("Announcing on VNI {}: {} via {} (type: {})", vni, destination, next_hop, next_hop.hop_type);
                client.subscribe(vni).await.ok(); // Subscribe to VNI if not already (ignore errors)
                client.announce(vni, destination, next_hop).await?;
            }
            Err(e) => eprintln!("Error parsing announcement '{}': {}", announcement, e),
        }
    }

    println!("\nCurrent routes for VNI {}:", args.vni);
    for route in client.get_received_routes(Vni(args.vni)) {
        println!("  {} via {} (type: {})", route.destination, route.next_hop, route.next_hop.hop_type);
    }

    if args.servers.len() > 1 {
        println!("\nListening for route updates... (Ctrl+C to exit)");
        println!("(Routes are deduplicated - same route from multiple servers shown once)\n");
    } else {
        println!("\nListening for route updates... (Ctrl+C to exit)\n");
    }

    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    client.shutdown().await?;
    Ok(())
}
