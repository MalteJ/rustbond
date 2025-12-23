//! Route and route table types.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

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
/// Structure: VNI -> Destination -> Set<NextHop>
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
        let mut inner = self.inner.write().unwrap();
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
        let mut inner = self.inner.write().unwrap();

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
        let inner = self.inner.read().unwrap();
        inner
            .routes
            .get(&vni)
            .and_then(|vni_routes| vni_routes.get(destination))
            .map(|next_hops| next_hops.contains(next_hop))
            .unwrap_or(false)
    }

    /// Returns all VNIs in the route table.
    pub fn get_vnis(&self) -> Vec<Vni> {
        let inner = self.inner.read().unwrap();
        inner.routes.keys().copied().collect()
    }

    /// Returns all destinations and their next hops for a VNI.
    pub fn get_destinations_by_vni(&self, vni: Vni) -> HashMap<Destination, Vec<NextHop>> {
        let inner = self.inner.read().unwrap();
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
        let inner = self.inner.read().unwrap();
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
        let inner = self.inner.read().unwrap();
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
        let mut inner = self.inner.write().unwrap();
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
        let mut inner = self.inner.write().unwrap();
        inner.routes.clear();
    }

    /// Returns the total number of routes in the table.
    pub fn len(&self) -> usize {
        let inner = self.inner.read().unwrap();
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

    #[test]
    fn test_add_and_check() {
        let table = RouteTable::new();
        let vni = Vni(100);
        let dest = make_dest("10.0.1.0/24");
        let hop = make_hop("2001:db8::1");

        assert!(!table.next_hop_exists(vni, &dest, &hop));
        assert!(table.add_next_hop(vni, dest.clone(), hop.clone()));
        assert!(table.next_hop_exists(vni, &dest, &hop));
        assert!(!table.add_next_hop(vni, dest, hop));
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
    fn test_get_routes() {
        let table = RouteTable::new();
        let vni = Vni(100);

        table.add_next_hop(vni, make_dest("10.0.1.0/24"), make_hop("2001:db8::1"));
        table.add_next_hop(vni, make_dest("10.0.2.0/24"), make_hop("2001:db8::2"));

        let routes = table.get_routes_by_vni(vni);
        assert_eq!(routes.len(), 2);
    }
}
