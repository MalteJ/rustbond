//! Route and route table types.

use std::collections::{HashMap, HashSet};

use parking_lot::RwLock;

use crate::types::{Destination, NextHop, Vni};

/// A route consisting of a destination and next hop.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Route {
    /// The destination prefix.
    pub destination: Destination,
    /// The next hop for this route.
    pub next_hop: NextHop,
}

impl Route {
    /// Creates a new route.
    pub fn new(destination: Destination, next_hop: NextHop) -> Self {
        Self {
            destination,
            next_hop,
        }
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} via {}", self.destination, self.next_hop)
    }
}

/// A thread-safe route table.
///
/// Structure: `VNI -> Destination -> Set<NextHop>`
#[derive(Debug, Default)]
pub struct RouteTable {
    inner: RwLock<RouteTableInner>,
}

#[derive(Debug, Default)]
struct RouteTableInner {
    /// VNI -> Destination -> Set<NextHop>
    routes: HashMap<Vni, HashMap<Destination, HashSet<NextHop>>>,
}

impl RouteTable {
    /// Creates a new empty route table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a next hop for a destination in a VNI.
    ///
    /// Returns true if the next hop was newly added, false if it already existed.
    pub fn add_next_hop(&self, vni: Vni, destination: Destination, next_hop: NextHop) -> bool {
        let mut inner = self.inner.write();
        let vni_routes = inner.routes.entry(vni).or_default();
        let next_hops = vni_routes.entry(destination).or_default();
        next_hops.insert(next_hop)
    }

    /// Removes a next hop for a destination in a VNI.
    ///
    /// Returns true if the next hop was removed, false if it didn't exist.
    /// Also returns the number of remaining next hops for this destination.
    pub fn remove_next_hop(
        &self,
        vni: Vni,
        destination: &Destination,
        next_hop: &NextHop,
    ) -> (bool, usize) {
        let mut inner = self.inner.write();

        if let Some(vni_routes) = inner.routes.get_mut(&vni) {
            if let Some(next_hops) = vni_routes.get_mut(destination) {
                let removed = next_hops.remove(next_hop);
                let remaining = next_hops.len();

                // Clean up empty entries
                if next_hops.is_empty() {
                    vni_routes.remove(destination);
                }
                if vni_routes.is_empty() {
                    inner.routes.remove(&vni);
                }

                return (removed, remaining);
            }
        }

        (false, 0)
    }

    /// Checks if a next hop exists for a destination in a VNI.
    pub fn next_hop_exists(&self, vni: Vni, destination: &Destination, next_hop: &NextHop) -> bool {
        let inner = self.inner.read();
        inner
            .routes
            .get(&vni)
            .and_then(|vni_routes| vni_routes.get(destination))
            .map(|next_hops| next_hops.contains(next_hop))
            .unwrap_or(false)
    }

    /// Returns all VNIs in the route table.
    pub fn get_vnis(&self) -> Vec<Vni> {
        let inner = self.inner.read();
        inner.routes.keys().copied().collect()
    }

    /// Returns all destinations and their next hops for a VNI.
    pub fn get_destinations_by_vni(&self, vni: Vni) -> HashMap<Destination, Vec<NextHop>> {
        let inner = self.inner.read();
        inner
            .routes
            .get(&vni)
            .map(|vni_routes| {
                vni_routes
                    .iter()
                    .map(|(dest, hops)| (dest.clone(), hops.iter().cloned().collect()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns all routes for a VNI.
    pub fn get_routes_by_vni(&self, vni: Vni) -> Vec<Route> {
        let inner = self.inner.read();
        let mut routes = Vec::new();

        if let Some(vni_routes) = inner.routes.get(&vni) {
            for (dest, hops) in vni_routes {
                for hop in hops {
                    routes.push(Route::new(dest.clone(), hop.clone()));
                }
            }
        }

        routes
    }

    /// Returns all routes in the table.
    pub fn get_all_routes(&self) -> Vec<(Vni, Route)> {
        let inner = self.inner.read();
        let mut routes = Vec::new();

        for (&vni, vni_routes) in &inner.routes {
            for (dest, hops) in vni_routes {
                for hop in hops {
                    routes.push((vni, Route::new(dest.clone(), hop.clone())));
                }
            }
        }

        routes
    }

    /// Removes all routes for a VNI.
    pub fn remove_vni(&self, vni: Vni) -> Vec<Route> {
        let mut inner = self.inner.write();
        let mut removed = Vec::new();

        if let Some(vni_routes) = inner.routes.remove(&vni) {
            for (dest, hops) in vni_routes {
                for hop in hops {
                    removed.push(Route::new(dest.clone(), hop));
                }
            }
        }

        removed
    }

    /// Clears all routes from the table.
    pub fn clear(&self) {
        let mut inner = self.inner.write();
        inner.routes.clear();
    }

    /// Returns the total number of routes in the table.
    pub fn len(&self) -> usize {
        let inner = self.inner.read();
        inner
            .routes
            .values()
            .map(|vni_routes| {
                vni_routes
                    .values()
                    .map(|next_hops| next_hops.len())
                    .sum::<usize>()
            })
            .sum()
    }

    /// Returns true if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn make_dest(prefix: &str) -> Destination {
        Destination::new(prefix.parse().unwrap())
    }

    fn make_hop(addr: &str) -> NextHop {
        NextHop::standard(addr.parse::<Ipv6Addr>().unwrap())
    }

    // Route tests
    #[test]
    fn test_route_new() {
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");
        let route = Route::new(dest.clone(), hop.clone());

        assert_eq!(route.destination, dest);
        assert_eq!(route.next_hop, hop);
    }

    #[test]
    fn test_route_display() {
        let route = Route::new(make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        let display = format!("{}", route);
        assert!(display.contains("10.0.1.0/24"));
        assert!(display.contains("2001:db8::1"));
    }

    #[test]
    fn test_route_equality() {
        let route1 = Route::new(make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        let route2 = Route::new(make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        let route3 = Route::new(make_dest("10.0.2.0/24"), make_hop("2001:db8::1"));

        assert_eq!(route1, route2);
        assert_ne!(route1, route3);
    }

    #[test]
    fn test_route_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();

        let route1 = Route::new(make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        let route2 = Route::new(make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        let route3 = Route::new(make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));

        set.insert(route1);
        set.insert(route2); // Duplicate, should not increase size
        set.insert(route3);

        assert_eq!(set.len(), 2);
    }

    // RouteTable tests
    #[test]
    fn test_route_table_new() {
        let table = RouteTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_route_table_default() {
        let table = RouteTable::default();
        assert!(table.is_empty());
    }

    #[test]
    fn test_add_and_check() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        assert!(!table.next_hop_exists(vni, &dest, &hop));
        assert!(table.add_next_hop(vni, dest.clone(), hop.clone()));
        assert!(table.next_hop_exists(vni, &dest, &hop));
        assert!(!table.add_next_hop(vni, dest, hop)); // Duplicate returns false
    }

    #[test]
    fn test_remove() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop1 = make_hop("2001:db8::1");
        let hop2 = make_hop("2001:db8::2");

        table.add_next_hop(vni, dest.clone(), hop1.clone());
        table.add_next_hop(vni, dest.clone(), hop2.clone());

        let (removed, remaining) = table.remove_next_hop(vni, &dest, &hop1);
        assert!(removed);
        assert_eq!(remaining, 1);

        let (removed, remaining) = table.remove_next_hop(vni, &dest, &hop2);
        assert!(removed);
        assert_eq!(remaining, 0);

        assert!(table.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        let (removed, remaining) = table.remove_next_hop(vni, &dest, &hop);
        assert!(!removed);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn test_remove_wrong_vni() {
        let table = RouteTable::new();
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        table.add_next_hop(Vni(100), dest.clone(), hop.clone());

        let (removed, _) = table.remove_next_hop(Vni(200), &dest, &hop);
        assert!(!removed);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn test_get_routes() {
        let table = RouteTable::new();
        let vni = Vni(100);

        table.add_next_hop(vni, make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        table.add_next_hop(vni, make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));

        let routes = table.get_routes_by_vni(vni);
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn test_get_routes_empty_vni() {
        let table = RouteTable::new();
        let routes = table.get_routes_by_vni(Vni(100));
        assert!(routes.is_empty());
    }

    #[test]
    fn test_get_destinations_by_vni() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest1 = make_dest("10.0.1.0/24");
        let dest2 = make_dest("10.0.2.0/24");

        table.add_next_hop(vni, dest1.clone(), make_hop("2001:db8::1"));
        table.add_next_hop(vni, dest1.clone(), make_hop("2001:db8::2")); // Second hop for same dest
        table.add_next_hop(vni, dest2.clone(), make_hop("2001:db8::3"));

        let dests = table.get_destinations_by_vni(vni);
        assert_eq!(dests.len(), 2);
        assert_eq!(dests.get(&dest1).unwrap().len(), 2);
        assert_eq!(dests.get(&dest2).unwrap().len(), 1);
    }

    #[test]
    fn test_get_vnis() {
        let table = RouteTable::new();

        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        table.add_next_hop(Vni(200), make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));
        table.add_next_hop(Vni(100), make_dest("10.0.3.0/24"), make_hop("2001:db8::3"));

        let vnis = table.get_vnis();
        assert_eq!(vnis.len(), 2);
        assert!(vnis.contains(&Vni(100)));
        assert!(vnis.contains(&Vni(200)));
    }

    #[test]
    fn test_get_all_routes() {
        let table = RouteTable::new();

        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        table.add_next_hop(Vni(200), make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));

        let all_routes = table.get_all_routes();
        assert_eq!(all_routes.len(), 2);
    }

    #[test]
    fn test_remove_vni() {
        let table = RouteTable::new();

        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        table.add_next_hop(Vni(100), make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));
        table.add_next_hop(Vni(200), make_dest("10.0.3.0/24"), make_hop("2001:db8::3"));

        let removed = table.remove_vni(Vni(100));
        assert_eq!(removed.len(), 2);
        assert_eq!(table.len(), 1);
        assert!(table.get_vnis().contains(&Vni(200)));
        assert!(!table.get_vnis().contains(&Vni(100)));
    }

    #[test]
    fn test_remove_vni_nonexistent() {
        let table = RouteTable::new();
        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));

        let removed = table.remove_vni(Vni(200));
        assert!(removed.is_empty());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn test_clear() {
        let table = RouteTable::new();

        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        table.add_next_hop(Vni(200), make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));

        assert_eq!(table.len(), 2);
        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_len() {
        let table = RouteTable::new();
        assert_eq!(table.len(), 0);

        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        assert_eq!(table.len(), 1);

        // Same destination, different hop
        table.add_next_hop(Vni(100), make_dest("10.0.1.0/24"), make_hop("2001:db8::2"));
        assert_eq!(table.len(), 2);

        // Different destination
        table.add_next_hop(Vni(100), make_dest("10.0.2.0/24"), make_hop("2001:db8::3"));
        assert_eq!(table.len(), 3);

        // Different VNI
        table.add_next_hop(Vni(200), make_dest("10.0.1.0/24"), make_hop("2001:db8::4"));
        assert_eq!(table.len(), 4);
    }

    #[test]
    fn test_multiple_next_hops_per_destination() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");

        table.add_next_hop(vni, dest.clone(), make_hop("2001:db8::1"));
        table.add_next_hop(vni, dest.clone(), make_hop("2001:db8::2"));
        table.add_next_hop(vni, dest.clone(), make_hop("2001:db8::3"));

        let dests = table.get_destinations_by_vni(vni);
        assert_eq!(dests.get(&dest).unwrap().len(), 3);

        let routes = table.get_routes_by_vni(vni);
        assert_eq!(routes.len(), 3);
    }

    #[test]
    fn test_thread_safety() {
        use std::sync::Arc;
        use std::thread;

        let table = Arc::new(RouteTable::new());
        let mut handles = vec![];

        // Spawn multiple threads adding routes
        for i in 0..10 {
            let table_clone = Arc::clone(&table);
            let handle = thread::spawn(move || {
                let dest = make_dest(&format!("10.0.{}.0/24", i));
                let hop = make_hop(&format!("2001:db8::{:x}", i));
                table_clone.add_next_hop(Vni(100), dest, hop);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(table.len(), 10);
    }

    #[test]
    fn test_concurrent_read_write() {
        use std::sync::Arc;
        use std::thread;

        let table = Arc::new(RouteTable::new());

        // Add some initial routes
        for i in 0..5 {
            let dest = make_dest(&format!("10.0.{}.0/24", i));
            let hop = make_hop(&format!("2001:db8::{:x}", i));
            table.add_next_hop(Vni(100), dest, hop);
        }

        let mut handles = vec![];

        // Readers
        for _ in 0..5 {
            let table_clone = Arc::clone(&table);
            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    let _ = table_clone.get_routes_by_vni(Vni(100));
                    let _ = table_clone.len();
                }
            });
            handles.push(handle);
        }

        // Writers
        for i in 5..10 {
            let table_clone = Arc::clone(&table);
            let handle = thread::spawn(move || {
                let dest = make_dest(&format!("10.0.{}.0/24", i));
                let hop = make_hop(&format!("2001:db8::{:x}", i));
                table_clone.add_next_hop(Vni(100), dest, hop);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(table.len(), 10);
    }

    #[test]
    fn test_cleanup_on_removal() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        table.add_next_hop(vni, dest.clone(), hop.clone());
        assert_eq!(table.get_vnis().len(), 1);

        table.remove_next_hop(vni, &dest, &hop);

        // After removing the only route, VNI should be cleaned up
        assert!(table.get_vnis().is_empty());
        assert!(table.is_empty());
    }

    #[test]
    fn test_next_hop_exists_different_vni() {
        let table = RouteTable::new();
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        table.add_next_hop(Vni(100), dest.clone(), hop.clone());

        assert!(table.next_hop_exists(Vni(100), &dest, &hop));
        assert!(!table.next_hop_exists(Vni(200), &dest, &hop));
    }

    #[test]
    fn test_next_hop_exists_different_dest() {
        let table = RouteTable::new();
        let dest1 = make_dest("10.0.1.0/24");
        let dest2 = make_dest("10.0.2.0/24");
        let hop = make_hop("2001:db8::1");

        table.add_next_hop(Vni(100), dest1.clone(), hop.clone());

        assert!(table.next_hop_exists(Vni(100), &dest1, &hop));
        assert!(!table.next_hop_exists(Vni(100), &dest2, &hop));
    }

    #[test]
    fn test_next_hop_exists_different_hop() {
        let table = RouteTable::new();
        let dest = make_dest("10.0.1.0/24");
        let hop1 = make_hop("2001:db8::1");
        let hop2 = make_hop("2001:db8::2");

        table.add_next_hop(Vni(100), dest.clone(), hop1.clone());

        assert!(table.next_hop_exists(Vni(100), &dest, &hop1));
        assert!(!table.next_hop_exists(Vni(100), &dest, &hop2));
    }
}
