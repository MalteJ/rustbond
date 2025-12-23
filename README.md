# RustBond

A Rust implementation of the [MetalBond](https://github.com/ironcore-dev/metalbond) route distribution protocol.

MetalBond is a route distribution protocol for managing virtual network routes across hypervisors. It operates over TCP/IPv6 and distributes routing information for IP-in-IPv6 overlay networks.

## Features

- Async TCP client using Tokio
- Automatic reconnection on connection loss
- VNI-based subscription model for selective route updates
- Route announcement and withdrawal
- Keepalive handling with configurable intervals

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rustbond = "0.1"
```

## Usage

```rust
use rustbond::{MetalBondClient, RouteHandler, Route, Vni, Destination, NextHop};

struct MyHandler;

impl RouteHandler for MyHandler {
    fn add_route(&self, vni: Vni, route: Route) {
        println!("Add route: {} in VNI {}", route, vni);
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        println!("Remove route: {} in VNI {}", route, vni);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let handler = MyHandler;
    let client = MetalBondClient::connect("[::1]:4711", handler);

    // Wait for connection
    client.wait_established().await?;

    // Subscribe to VNI 100
    client.subscribe(Vni(100)).await?;

    // Announce a route
    let dest = Destination::from_ipv4([10, 0, 1, 0].into(), 24);
    let hop = NextHop::standard("2001:db8::1".parse()?);
    client.announce(Vni(100), dest, hop).await?;

    Ok(())
}
```

## Protocol Overview

MetalBond messages use a 4-byte header followed by a protobuf payload:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    Version    |       Message Length          |  Message Type |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
|                   Protobuf Payload                            |
|                   (variable length, max 1188 bytes)           |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

### Message Types

| Type | ID | Description |
|------|----|-------------|
| HELLO | 1 | Connection handshake |
| KEEPALIVE | 2 | Liveness detection |
| SUBSCRIBE | 3 | Request routes for VNI |
| UNSUBSCRIBE | 4 | Stop receiving routes for VNI |
| UPDATE | 5 | Route announcement/withdrawal |

### Connection Lifecycle

```
  Client                              Server
    │                                    │
    │────────── TCP Connect ────────────>│
    │                                    │
    │────────── HELLO ──────────────────>│
    │<───────── HELLO ───────────────────│
    │                                    │
    │────────── KEEPALIVE ──────────────>│
    │<───────── KEEPALIVE ───────────────│
    │                                    │
    │         [ESTABLISHED]              │
    │                                    │
    │────────── SUBSCRIBE(vni=100) ─────>│
    │<───────── UPDATE (routes) ─────────│
```

## Testing with Go MetalBond

You can test this Rust client against the Go MetalBond server:

```bash
# Build and run the Go server
cd /path/to/metalbond
go build -o metalbond ./cmd
./metalbond server --listen [::]:4711

# In another terminal, run a Go client to announce routes
./metalbond client --server [::1]:4711 --subscribe 100 --announce 10.0.0.0/24

# Run the Rust client
cd /path/to/rustbond
cargo run --example client -- [::1]:4711 100
```

## License

Apache-2.0
