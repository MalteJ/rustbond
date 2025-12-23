# RustBond

A Rust implementation of the [MetalBond](https://github.com/ironcore-dev/metalbond) route distribution protocol.

MetalBond distributes virtual network routes across hypervisors over TCP/IPv6.

## Features

- Async client/server with Tokio
- Automatic reconnection
- VNI-based subscriptions
- Multi-server HA with ECMP support
- Standard, NAT, and LoadBalancer route types
- Interoperable with Go MetalBond

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

let client = MetalBondClient::connect("[::1]:4711", MyHandler);
client.wait_established().await?;
client.subscribe(Vni(100)).await?;
```

### Multi-Server (HA)

For high availability, connect to multiple servers simultaneously:

```rust
let client = MultiServerClient::connect(&["[::1]:4711", "[::1]:4712"], MyHandler);
client.wait_any_established().await?;
// Routes are deduplicated across servers; ECMP supported
```

## Examples

```bash
# Run server
cargo run --example server

# Connect client to VNI 100
cargo run --example client -- [::1]:4711 100

# Announce a route
cargo run --example client -- [::1]:4711 100 "10.0.1.0/24#2001:db8::1"

# Multi-server HA
cargo run --example multi_client -- [::1]:4711 [::1]:4712 100
```

## Route Types

Announcement format: `prefix#nexthop[#type][#fromPort#toPort]`

```bash
# Standard route (default)
"10.0.1.0/24#2001:db8::1"

# NAT route with port range
"10.0.2.0/24#2001:db8::2#NAT#1024#2048"

# Load balancer target
"10.0.3.0/24#2001:db8::3#LB"
```

## Testing

```bash
cargo test           # All tests
cargo test --lib     # Unit tests only
cargo test --test integration  # Integration tests only
```

## License

[Licensed under Apache-2.0](LICENSE)
