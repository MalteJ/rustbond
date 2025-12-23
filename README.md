# RustBond

[![Crates.io](https://img.shields.io/crates/v/rustbond.svg)](https://crates.io/crates/rustbond)
[![Documentation](https://docs.rs/rustbond/badge.svg)](https://docs.rs/rustbond)
[![License](https://img.shields.io/crates/l/rustbond.svg)](LICENSE)

A Rust implementation of the [MetalBond](https://github.com/ironcore-dev/metalbond) route distribution protocol.

MetalBond distributes virtual network routes across hypervisors. Connections between clients and servers use TCP (typically over IPv6 for an IPv6-only fabric), while the overlay supports both IPv4 and IPv6 routes (IPv4/6-in-IPv6 tunneling).

## Features

- Async client/server with Tokio
- Automatic reconnection
- VNI-based subscriptions
- Multi-server HA with ECMP support
- Standard, NAT, and LoadBalancer route types
- Interoperable with Go MetalBond
- Optional netlink integration for kernel route installation

## Protocol Overview

MetalBond uses a simple message-based protocol over TCP:

1. **Handshake**: Client and server exchange `HELLO` messages to negotiate keepalive intervals
2. **Keepalive**: Both sides send periodic `KEEPALIVE` messages to detect connection loss
3. **Subscribe**: Clients subscribe to VNIs to receive routes for specific virtual networks
4. **Update**: Route announcements and withdrawals are distributed via `UPDATE` messages

When a client disconnects, the server automatically withdraws all routes announced by that client.

## Quick Start

### Server

```rust
use rustbond::{MetalBondServer, ServerConfig};

let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await?;
// ... server.shutdown().await?;
```

### Client

```rust
use rustbond::{MetalBondClient, RouteHandler, Route, Vni};

struct MyHandler;
impl RouteHandler for MyHandler {
    fn add_route(&self, vni: Vni, route: Route) { /* handle add */ }
    fn remove_route(&self, vni: Vni, route: Route) { /* handle remove */ }
}

let client = MetalBondClient::connect(&["[::1]:4711"], MyHandler);
client.wait_established().await?;
client.subscribe(Vni(100)).await?;
```

### Multi-Server (HA)

For high availability, connect to multiple servers simultaneously:

```rust
let client = MetalBondClient::connect(&["[::1]:4711", "[::1]:4712"], MyHandler);
client.wait_any_established().await?;
// Routes are deduplicated across servers; ECMP supported
```

## Examples

```bash
# Run server (default: [::1]:4711)
cargo run --example server

# Run server on custom address
cargo run --example server -- -l [::]:4711

# Connect client to VNI 100
cargo run --example client -- -s [::1]:4711 -v 100

# Announce a route (VNI#prefix@nexthop)
cargo run --example client -- -s [::1]:4711 -v 100 -a "100#10.0.1.0/24@2001:db8::1"

# Multi-server HA (just add more -s flags)
cargo run --example client -- -s [::1]:4711 -s [::1]:4712 -v 100
```

## Route Types

Announcement format: `VNI#prefix@nexthop[#type[#fromPort#toPort]]`

```bash
# Standard route (default) on VNI 100
"100#10.0.1.0/24@2001:db8::1"

# NAT route with port range on VNI 200
"200#10.0.2.0/24@2001:db8::2#nat#1024#2048"

# Load balancer target on VNI 100
"100#10.0.3.0/24@2001:db8::3#lb"
```

## Kernel Route Installation (Netlink)

Enable the `netlink` feature to install routes directly into the Linux kernel routing tables:

```bash
# Install routes from VNI 100 to kernel routing table 100
cargo run --example client --features netlink -- \
  -s [::1]:4711 -v 100 \
  --install-routes 100#100 \
  --tun ip6tnl0
```

Options:
- `--install-routes VNI#TABLE` - Map VNI to kernel routing table (can be repeated)
- `--tun DEVICE` - Tunnel device name for encapsulated traffic (default: ip6tnl0)

How it works:
- Routes received for a VNI are installed into the corresponding kernel routing table
- Each route points to the tunnel device with the next-hop as the gateway
- Routes are marked with protocol 254 (`RTPROT_METALBOND`) for identification
- On startup, stale routes from previous runs (same protocol marker) are automatically cleaned up
- When a route is withdrawn by the server, it's removed from the kernel table

**Note:** Requires root/`CAP_NET_ADMIN` to modify kernel routing tables.

## Error Handling

The library uses a single `Error` type for all operations. Common errors include:

- `Error::NotEstablished` - Operation attempted before connection is ready
- `Error::Timeout` / `Error::ConnectionTimeout` - Connection or operation timed out
- `Error::Closed` - Connection was closed
- `Error::RouteAlreadyAnnounced` - Attempted to announce a duplicate route
- `Error::RouteNotFound` - Attempted to withdraw a non-existent route
- `Error::Io(...)` - Underlying I/O error
- `Error::Protocol(...)` - Protocol violation or malformed message

For multi-server setups, operations like `subscribe()` and `announce()` succeed if at least one server accepts them, logging warnings for servers that fail.

## Testing

```bash
cargo test           # All tests
cargo test --lib     # Unit tests only
cargo test --test integration  # Integration tests only
```

## License

[Licensed under Apache-2.0](LICENSE)
