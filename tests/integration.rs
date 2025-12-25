//! Integration tests for rustbond client-server communication.

use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustbond::{
    ClientState, Destination, MetalBondClient, MetalBondServer, NextHop,
    NextHopType, Route, RouteHandler, ServerConfig, Vni,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Test handler that collects route updates.
struct TestHandler {
    add_tx: mpsc::UnboundedSender<(Vni, Route)>,
    remove_tx: mpsc::UnboundedSender<(Vni, Route)>,
}

impl RouteHandler for TestHandler {
    fn add_route(&self, vni: Vni, route: Route) {
        let _ = self.add_tx.send((vni, route));
    }

    fn remove_route(&self, vni: Vni, route: Route) {
        let _ = self.remove_tx.send((vni, route));
    }
}

/// Counting handler that just counts operations.
struct CountingHandler {
    add_count: AtomicUsize,
    remove_count: AtomicUsize,
}

impl CountingHandler {
    fn new() -> Self {
        Self {
            add_count: AtomicUsize::new(0),
            remove_count: AtomicUsize::new(0),
        }
    }

    #[allow(dead_code)]
    fn adds(&self) -> usize {
        self.add_count.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    fn removes(&self) -> usize {
        self.remove_count.load(Ordering::SeqCst)
    }
}

impl RouteHandler for CountingHandler {
    fn add_route(&self, _vni: Vni, _route: Route) {
        self.add_count.fetch_add(1, Ordering::SeqCst);
    }

    fn remove_route(&self, _vni: Vni, _route: Route) {
        self.remove_count.fetch_add(1, Ordering::SeqCst);
    }
}

/// Helper to create destination.
fn dest(prefix: &str) -> Destination {
    Destination::new(prefix.parse().unwrap())
}

/// Helper to create next hop.
fn hop(addr: &str) -> NextHop {
    NextHop::standard(addr.parse::<Ipv6Addr>().unwrap())
}

#[tokio::test]
async fn test_server_starts_and_stops() {
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:0", config).await.unwrap();

    assert_eq!(server.route_count().await, 0);
    assert_eq!(server.peer_count().await, 0);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_client_connects_to_server() {
    // Start server on a specific port
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14711", config).await.unwrap();

    // Create and connect client
    let handler = CountingHandler::new();
    let handler = Arc::new(handler);
    let client = MetalBondClient::connect(&["[::1]:14711"], handler.clone());

    // Wait for connection
    let result = timeout(Duration::from_secs(5), client.wait_established()).await;
    assert!(result.is_ok(), "Client should connect within timeout");

    // Verify peer count
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.peer_count().await, 1);

    // Cleanup
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_client_subscribe_and_announce() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14712", config).await.unwrap();

    // Create and connect client
    let handler = CountingHandler::new();
    let handler = Arc::new(handler);
    let client = MetalBondClient::connect(&["[::1]:14712"], handler.clone());

    // Wait for connection
    client.wait_established().await.unwrap();

    // Subscribe to VNI
    client.subscribe(Vni(100)).await.unwrap();

    // Announce a route
    let destination = dest("10.0.1.0/24");
    let next_hop = hop("2001:db8::1");
    client
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    // Give server time to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify route is in server
    assert_eq!(server.route_count().await, 1);

    // Cleanup
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_route_distribution_between_clients() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14713", config).await.unwrap();

    // Create receiver channels for client 1
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, _remove_rx1) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14713"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Create receiver channels for client 2
    let (add_tx2, mut add_rx2) = mpsc::unbounded_channel();
    let (remove_tx2, _remove_rx2) = mpsc::unbounded_channel();
    let handler2 = TestHandler {
        add_tx: add_tx2,
        remove_tx: remove_tx2,
    };
    let client2 = MetalBondClient::connect(&["[::1]:14713"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    // Give time for subscriptions to be processed
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Client 2 announces a route
    let destination = dest("10.0.2.0/24");
    let next_hop = hop("2001:db8::99");
    client2
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    // Client 1 should receive the route
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok(), "Client 1 should receive route");
    let (vni, route) = result.unwrap().unwrap();
    assert_eq!(vni.0, 100);
    assert_eq!(route.destination.prefix.to_string(), "10.0.2.0/24");

    // Client 2 should NOT receive its own route (no echo)
    let result = timeout(Duration::from_millis(500), add_rx2.recv()).await;
    assert!(result.is_err(), "Client 2 should NOT receive its own route");

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_route_withdrawal() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14714", config).await.unwrap();

    // Create client 1 (receiver)
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, mut remove_rx1) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14714"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Create client 2 (announcer)
    let handler2 = CountingHandler::new();
    let handler2 = Arc::new(handler2);
    let client2 = MetalBondClient::connect(&["[::1]:14714"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Client 2 announces a route
    let destination = dest("10.0.3.0/24");
    let next_hop = hop("2001:db8::33");
    client2
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    // Client 1 receives the add
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok());

    // Client 2 withdraws the route
    client2
        .withdraw(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    // Client 1 should receive the removal
    let result = timeout(Duration::from_secs(2), remove_rx1.recv()).await;
    assert!(result.is_ok(), "Client 1 should receive route removal");
    let (vni, route) = result.unwrap().unwrap();
    assert_eq!(vni.0, 100);
    assert_eq!(route.destination.prefix.to_string(), "10.0.3.0/24");

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_existing_routes_sent_on_subscribe() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14715", config).await.unwrap();

    // Client 1 announces a route before client 2 subscribes
    let handler1 = CountingHandler::new();
    let handler1 = Arc::new(handler1);
    let client1 = MetalBondClient::connect(&["[::1]:14715"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    let destination = dest("10.0.4.0/24");
    let next_hop = hop("2001:db8::44");
    client1
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Now client 2 connects and subscribes
    let (add_tx2, mut add_rx2) = mpsc::unbounded_channel();
    let (remove_tx2, _) = mpsc::unbounded_channel();
    let handler2 = TestHandler {
        add_tx: add_tx2,
        remove_tx: remove_tx2,
    };
    let client2 = MetalBondClient::connect(&["[::1]:14715"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    // Client 2 should receive the existing route
    let result = timeout(Duration::from_secs(2), add_rx2.recv()).await;
    assert!(result.is_ok(), "Client 2 should receive existing route");
    let (vni, route) = result.unwrap().unwrap();
    assert_eq!(vni.0, 100);
    assert_eq!(route.destination.prefix.to_string(), "10.0.4.0/24");

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

/// Test that routes are properly withdrawn when client explicitly withdraws.
/// Note: Testing automatic withdrawal on TCP disconnect would require waiting
/// for keepalive timeout (12.5+ seconds), so we test explicit withdrawal instead.
#[tokio::test]
async fn test_explicit_route_withdrawal() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14716", config).await.unwrap();

    // Client 1 subscribes to receive updates
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, mut remove_rx1) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14716"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Client 2 announces a route
    let handler2 = CountingHandler::new();
    let handler2 = Arc::new(handler2);
    let client2 = MetalBondClient::connect(&["[::1]:14716"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let destination = dest("10.0.5.0/24");
    let next_hop = hop("2001:db8::55");
    client2
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    // Wait for client 1 to receive the route
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok());

    // Client 2 explicitly withdraws the route
    client2
        .withdraw(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    // Client 1 should receive the route withdrawal
    let result = timeout(Duration::from_secs(2), remove_rx1.recv()).await;
    assert!(
        result.is_ok(),
        "Client 1 should receive route withdrawal"
    );
    let (vni, route) = result.unwrap().unwrap();
    assert_eq!(vni.0, 100);
    assert_eq!(route.destination.prefix.to_string(), "10.0.5.0/24");

    // Verify route is removed from server
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.route_count().await, 0);

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multiple_vnis() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14717", config).await.unwrap();

    // Client 1 subscribes to VNI 100
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, _) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14717"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Client 2 subscribes to VNI 200 only
    let (add_tx2, mut add_rx2) = mpsc::unbounded_channel();
    let (remove_tx2, _) = mpsc::unbounded_channel();
    let handler2 = TestHandler {
        add_tx: add_tx2,
        remove_tx: remove_tx2,
    };
    let client2 = MetalBondClient::connect(&["[::1]:14717"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(200)).await.unwrap();

    // Client 3 announces on VNI 100
    let handler3 = CountingHandler::new();
    let handler3 = Arc::new(handler3);
    let client3 = MetalBondClient::connect(&["[::1]:14717"], handler3);
    client3.wait_established().await.unwrap();
    client3.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    client3
        .announce(Vni(100), dest("10.0.6.0/24"), hop("2001:db8::66"))
        .await
        .unwrap();

    // Client 1 should receive the route (subscribed to VNI 100)
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok(), "Client 1 should receive route on VNI 100");

    // Client 2 should NOT receive the route (subscribed to VNI 200)
    let result = timeout(Duration::from_millis(500), add_rx2.recv()).await;
    assert!(
        result.is_err(),
        "Client 2 should NOT receive route on different VNI"
    );

    // Cleanup
    drop(client1);
    drop(client2);
    drop(client3);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_nat_route_type() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14718", config).await.unwrap();

    // Client 1 subscribes
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, _) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14718"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Client 2 announces NAT route
    let handler2 = CountingHandler::new();
    let handler2 = Arc::new(handler2);
    let client2 = MetalBondClient::connect(&["[::1]:14718"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let nat_hop = NextHop::nat("2001:db8::77".parse().unwrap(), 30000, 40000);
    client2
        .announce(Vni(100), dest("10.0.7.0/24"), nat_hop)
        .await
        .unwrap();

    // Client 1 should receive the NAT route
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok());
    let (_, route) = result.unwrap().unwrap();
    assert_eq!(route.next_hop.hop_type, NextHopType::Nat);
    assert_eq!(route.next_hop.nat_port_range_from, 30000);
    assert_eq!(route.next_hop.nat_port_range_to, 40000);

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_load_balancer_route_type() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14719", config).await.unwrap();

    // Client 1 subscribes
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, _) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14719"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Client 2 announces LB route
    let handler2 = CountingHandler::new();
    let handler2 = Arc::new(handler2);
    let client2 = MetalBondClient::connect(&["[::1]:14719"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let lb_hop = NextHop::load_balancer("2001:db8::88".parse().unwrap());
    client2
        .announce(Vni(100), dest("10.0.8.0/24"), lb_hop)
        .await
        .unwrap();

    // Client 1 should receive the LB route
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok());
    let (_, route) = result.unwrap().unwrap();
    assert_eq!(route.next_hop.hop_type, NextHopType::LoadBalancerTarget);

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_ipv6_destination() {
    // Start server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14720", config).await.unwrap();

    // Client 1 subscribes
    let (add_tx1, mut add_rx1) = mpsc::unbounded_channel();
    let (remove_tx1, _) = mpsc::unbounded_channel();
    let handler1 = TestHandler {
        add_tx: add_tx1,
        remove_tx: remove_tx1,
    };
    let client1 = MetalBondClient::connect(&["[::1]:14720"], handler1);
    client1.wait_established().await.unwrap();
    client1.subscribe(Vni(100)).await.unwrap();

    // Client 2 announces IPv6 route
    let handler2 = CountingHandler::new();
    let handler2 = Arc::new(handler2);
    let client2 = MetalBondClient::connect(&["[::1]:14720"], handler2);
    client2.wait_established().await.unwrap();
    client2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let ipv6_dest = dest("2001:db8:1::/48");
    client2
        .announce(Vni(100), ipv6_dest, hop("2001:db8::99"))
        .await
        .unwrap();

    // Client 1 should receive the IPv6 route
    let result = timeout(Duration::from_secs(2), add_rx1.recv()).await;
    assert!(result.is_ok());
    let (_, route) = result.unwrap().unwrap();
    assert_eq!(route.destination.prefix.to_string(), "2001:db8:1::/48");

    // Cleanup
    drop(client1);
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await.unwrap();
}

// ============================================================================
// Multi-Server Client Tests
// ============================================================================

#[tokio::test]
async fn test_multi_server_connects_all() {
    // Start two servers
    let config = ServerConfig::default();
    let server1 = MetalBondServer::start("[::1]:14801", config.clone()).await.unwrap();
    let server2 = MetalBondServer::start("[::1]:14802", config).await.unwrap();

    // Create multi-server client
    let handler = CountingHandler::new();
    let handler = Arc::new(handler);
    let client = MetalBondClient::connect(&["[::1]:14801", "[::1]:14802"], handler.clone());

    // Wait for all connections
    let result = timeout(Duration::from_secs(5), client.wait_all_established()).await;
    assert!(result.is_ok(), "Should connect to all servers within timeout");
    assert!(result.unwrap().is_ok());

    // Verify state
    let state = client.state();
    assert!(
        matches!(state, ClientState::FullyConnected),
        "State should be FullyConnected, got {:?}",
        state
    );

    // Verify peer counts
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server1.peer_count().await, 1);
    assert_eq!(server2.peer_count().await, 1);

    // Cleanup
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.shutdown().await.unwrap();
    server2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multi_server_ecmp_deduplication() {
    // Start two servers
    let config = ServerConfig::default();
    let server1 = MetalBondServer::start("[::1]:14803", config.clone()).await.unwrap();
    let server2 = MetalBondServer::start("[::1]:14804", config).await.unwrap();

    // Create multi-server client with counting handler
    let handler = Arc::new(CountingHandler::new());
    let client = MetalBondClient::connect(&["[::1]:14803", "[::1]:14804"], handler.clone());
    client.wait_all_established().await.unwrap();
    client.subscribe(Vni(100)).await.unwrap();

    // Create announcer on server 1
    let announcer1_handler = CountingHandler::new();
    let announcer1 = MetalBondClient::connect(&["[::1]:14803"], Arc::new(announcer1_handler));
    announcer1.wait_established().await.unwrap();
    announcer1.subscribe(Vni(100)).await.unwrap();

    // Create announcer on server 2
    let announcer2_handler = CountingHandler::new();
    let announcer2 = MetalBondClient::connect(&["[::1]:14804"], Arc::new(announcer2_handler));
    announcer2.wait_established().await.unwrap();
    announcer2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Announce SAME route from both announcers (same dest + same hop)
    let destination = dest("10.1.0.0/24");
    let next_hop = hop("2001:db8::100");

    announcer1
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    announcer2
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Multi-client should have only called add_route ONCE (deduplication)
    assert_eq!(
        handler.adds(),
        1,
        "Same route from two servers should only trigger one add_route"
    );

    // Cleanup
    drop(client);
    drop(announcer1);
    drop(announcer2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.shutdown().await.unwrap();
    server2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multi_server_different_hops_are_separate() {
    // Start two servers
    let config = ServerConfig::default();
    let server1 = MetalBondServer::start("[::1]:14805", config.clone()).await.unwrap();
    let server2 = MetalBondServer::start("[::1]:14806", config).await.unwrap();

    // Create multi-server client with counting handler
    let handler = Arc::new(CountingHandler::new());
    let client = MetalBondClient::connect(&["[::1]:14805", "[::1]:14806"], handler.clone());
    client.wait_all_established().await.unwrap();
    client.subscribe(Vni(100)).await.unwrap();

    // Create announcer on server 1
    let announcer1 = MetalBondClient::connect(&["[::1]:14805"], Arc::new(CountingHandler::new()));
    announcer1.wait_established().await.unwrap();
    announcer1.subscribe(Vni(100)).await.unwrap();

    // Create announcer on server 2
    let announcer2 = MetalBondClient::connect(&["[::1]:14806"], Arc::new(CountingHandler::new()));
    announcer2.wait_established().await.unwrap();
    announcer2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Announce SAME destination but DIFFERENT next-hops (ECMP scenario)
    let destination = dest("10.2.0.0/24");
    let next_hop1 = hop("2001:db8::201");
    let next_hop2 = hop("2001:db8::202");

    announcer1
        .announce(Vni(100), destination.clone(), next_hop1)
        .await
        .unwrap();

    announcer2
        .announce(Vni(100), destination.clone(), next_hop2)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Multi-client should have called add_route TWICE (different next-hops are separate routes)
    assert_eq!(
        handler.adds(),
        2,
        "Different next-hops should trigger two add_route calls (ECMP)"
    );

    // Cleanup
    drop(client);
    drop(announcer1);
    drop(announcer2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.shutdown().await.unwrap();
    server2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multi_server_partial_withdraw() {
    // Start two servers
    let config = ServerConfig::default();
    let server1 = MetalBondServer::start("[::1]:14807", config.clone()).await.unwrap();
    let server2 = MetalBondServer::start("[::1]:14808", config).await.unwrap();

    // Create multi-server client
    let handler = Arc::new(CountingHandler::new());
    let client = MetalBondClient::connect(&["[::1]:14807", "[::1]:14808"], handler.clone());
    client.wait_all_established().await.unwrap();
    client.subscribe(Vni(100)).await.unwrap();

    // Create announcers
    let announcer1 = MetalBondClient::connect(&["[::1]:14807"], Arc::new(CountingHandler::new()));
    announcer1.wait_established().await.unwrap();
    announcer1.subscribe(Vni(100)).await.unwrap();

    let announcer2 = MetalBondClient::connect(&["[::1]:14808"], Arc::new(CountingHandler::new()));
    announcer2.wait_established().await.unwrap();
    announcer2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Announce same route from both
    let destination = dest("10.3.0.0/24");
    let next_hop = hop("2001:db8::300");

    announcer1
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();
    announcer2
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(handler.adds(), 1, "Should have 1 add");
    assert_eq!(handler.removes(), 0, "Should have 0 removes initially");

    // Announcer 1 withdraws - but announcer 2 still has the route
    announcer1
        .withdraw(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Remove should NOT be called yet (route still available from server 2)
    assert_eq!(
        handler.removes(),
        0,
        "Partial withdraw should NOT trigger remove_route"
    );

    // Cleanup
    drop(client);
    drop(announcer1);
    drop(announcer2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.shutdown().await.unwrap();
    server2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multi_server_full_withdraw() {
    // Start two servers
    let config = ServerConfig::default();
    let server1 = MetalBondServer::start("[::1]:14809", config.clone()).await.unwrap();
    let server2 = MetalBondServer::start("[::1]:14810", config).await.unwrap();

    // Create multi-server client
    let handler = Arc::new(CountingHandler::new());
    let client = MetalBondClient::connect(&["[::1]:14809", "[::1]:14810"], handler.clone());
    client.wait_all_established().await.unwrap();
    client.subscribe(Vni(100)).await.unwrap();

    // Create announcers
    let announcer1 = MetalBondClient::connect(&["[::1]:14809"], Arc::new(CountingHandler::new()));
    announcer1.wait_established().await.unwrap();
    announcer1.subscribe(Vni(100)).await.unwrap();

    let announcer2 = MetalBondClient::connect(&["[::1]:14810"], Arc::new(CountingHandler::new()));
    announcer2.wait_established().await.unwrap();
    announcer2.subscribe(Vni(100)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Announce same route from both
    let destination = dest("10.4.0.0/24");
    let next_hop = hop("2001:db8::400");

    announcer1
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();
    announcer2
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Both withdraw
    announcer1
        .withdraw(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();
    announcer2
        .withdraw(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Now remove SHOULD be called
    assert_eq!(handler.adds(), 1, "Should have 1 add");
    assert_eq!(
        handler.removes(),
        1,
        "Full withdraw should trigger remove_route"
    );

    // Cleanup
    drop(client);
    drop(announcer1);
    drop(announcer2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.shutdown().await.unwrap();
    server2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multi_server_subscribe_announce_to_all() {
    // Start two servers
    let config = ServerConfig::default();
    let server1 = MetalBondServer::start("[::1]:14811", config.clone()).await.unwrap();
    let server2 = MetalBondServer::start("[::1]:14812", config).await.unwrap();

    // Create multi-server client
    let handler = Arc::new(CountingHandler::new());
    let client = MetalBondClient::connect(&["[::1]:14811", "[::1]:14812"], handler.clone());
    client.wait_all_established().await.unwrap();

    // Subscribe and announce - should go to all servers
    client.subscribe(Vni(100)).await.unwrap();

    let destination = dest("10.5.0.0/24");
    let next_hop = hop("2001:db8::500");
    client
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Both servers should have the route
    assert_eq!(server1.route_count().await, 1, "Server 1 should have the route");
    assert_eq!(server2.route_count().await, 1, "Server 2 should have the route");

    // Cleanup
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.shutdown().await.unwrap();
    server2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_routes_reannounced_on_reconnect() {
    // Start initial server
    let config = ServerConfig::default();
    let server = MetalBondServer::start("[::1]:14815", config.clone()).await.unwrap();

    // Create client that will announce a route
    let handler = Arc::new(CountingHandler::new());
    let client = MetalBondClient::connect(&["[::1]:14815"], handler.clone());
    client.wait_established().await.unwrap();
    client.subscribe(Vni(100)).await.unwrap();

    // Announce a route
    let destination = dest("10.99.0.0/24");
    let next_hop = hop("2001:db8::99");
    client
        .announce(Vni(100), destination.clone(), next_hop.clone())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify server has the route
    assert_eq!(server.route_count().await, 1, "Server should have the route");

    // Shutdown the server - client will disconnect
    server.shutdown().await.unwrap();

    // Wait for client to detect disconnect
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Start a new server on the same port
    let new_server = MetalBondServer::start("[::1]:14815", config).await.unwrap();

    // Wait for client to reconnect (retry interval is 5-10 seconds)
    let reconnected = timeout(Duration::from_secs(15), async {
        loop {
            if new_server.peer_count().await > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    assert!(reconnected.is_ok(), "Client should reconnect to new server");

    // Wait a bit for route re-announcement to complete
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify the route was re-announced to the new server
    assert_eq!(
        new_server.route_count().await,
        1,
        "Route should be re-announced after reconnection"
    );

    // Cleanup
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    new_server.shutdown().await.unwrap();
}
