# RustBond

A Rust implementation of the [MetalBond](https://github.com/ironcore-dev/metalbond) route distribution protocol.

MetalBond is a route distribution protocol for managing virtual network routes across hypervisors. It operates over TCP/IPv6 and distributes routing information for IP-in-IPv6 overlay networks.

## Features

- Async TCP client and server using Tokio
- Automatic reconnection on connection loss (client)
- VNI-based subscription model for selective route updates
- Route announcement and withdrawal
- Keepalive handling with configurable intervals
- Support for STANDARD, NAT, and LOADBALANCER_TARGET route types
- Full interoperability with Go MetalBond implementation

## Quick Start

### Running the Server

```bash
cargo run --example server
```

This starts a MetalBond server on `[::]:4711`.

### Running a Client

```bash
# Connect to server, subscribe to VNI 100, and announce a route
cargo run --example client -- [::1]:4711 100 10.0.1.0/24 2001:db8::1
```

### Running Tests

```bash
# Run all tests (unit + integration)
cargo test

# Run only unit tests
cargo test --lib

# Run only integration tests
cargo test --test integration

# Run a specific test
cargo test test_route_distribution_between_clients
```

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rustbond = "0.1"
```

## Usage

### Server

```rust
use rustbond::{MetalBondServer, ServerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = MetalBondServer::start("[::]:4711", ServerConfig::default()).await?;
    println!("Server running on [::]:4711");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    server.shutdown().await?;

    Ok(())
}
```

### Client

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

## Interoperability with Go MetalBond

RustBond is fully interoperable with the Go MetalBond implementation.

### Rust Server with Go Client

```bash
# Terminal 1: Start Rust server
cargo run --example server

# Terminal 2: Connect Go client
cd /path/to/metalbond
./metalbond client --server [::1]:4711 --subscribe 100 --announce "100#10.0.1.0/24#2001:db8::1"
```

### Go Server with Rust Client

```bash
# Terminal 1: Start Go server
cd /path/to/metalbond
./metalbond server --listen [::]:4711

# Terminal 2: Connect Rust client
cargo run --example client -- [::1]:4711 100 10.0.1.0/24 2001:db8::1
```

### Route Types

The client example supports all route types:

```bash
# Standard route
cargo run --example client -- [::1]:4711 100 10.0.1.0/24 2001:db8::1

# NAT route with port range
cargo run --example client -- [::1]:4711 100 10.0.1.0/24 2001:db8::1 nat 30000 40000

# Load balancer target
cargo run --example client -- [::1]:4711 100 10.0.1.0/24 2001:db8::1 lb
```

## License

Apache-2.0
