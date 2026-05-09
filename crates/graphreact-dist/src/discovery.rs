//! Peer discovery — `NodeId → SocketAddr` resolution.
//!
//! The substrate is the [`Discovery`] trait; the dist crate ships
//! two built-in implementations:
//!
//! - [`StaticDiscovery`] — explicit `HashMap<NodeId, SocketAddr>`.
//!   Right for dev / small clusters / config-file-driven topology.
//! - [`DnsDiscovery`] — uses `tokio::net::lookup_host` against
//!   `node.host():default_port`. Right for production behind DNS
//!   (k8s headless services, cloud DNS, mDNS). Resolves via the
//!   system resolver — A/AAAA records suffice; SRV-record support
//!   can be implemented as a separate `impl Discovery` if needed.
//!
//! Custom resolvers (Consul, etcd, kubernetes API, etc.) are an
//! `impl Discovery for X` away — the trait is the one stable
//! integration point for higher layers (M14.4+).

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use crate::node_id::NodeId;
use crate::DistError;

/// `NodeId → SocketAddr` resolver. Implementations must be
/// `Send + Sync` so they can be stored behind `Arc<dyn Discovery>`
/// and shared across spawned tasks.
///
/// `Pin<Box<dyn Future ...>>` rather than native `async fn in trait`
/// because we want the trait to be object-safe (`Arc<dyn Discovery>`).
pub trait Discovery: Send + Sync {
    fn resolve<'a>(
        &'a self,
        node: &'a NodeId,
    ) -> Pin<Box<dyn Future<Output = Result<SocketAddr, DistError>> + Send + 'a>>;
}

/// Hashmap-backed discovery. Right for dev / small clusters where
/// the topology is known at startup.
#[derive(Debug, Clone, Default)]
pub struct StaticDiscovery {
    entries: HashMap<NodeId, SocketAddr>,
}

impl StaticDiscovery {
    pub fn new(entries: HashMap<NodeId, SocketAddr>) -> Self {
        Self { entries }
    }

    pub fn insert(&mut self, node: NodeId, addr: SocketAddr) {
        self.entries.insert(node, addr);
    }
}

impl Discovery for StaticDiscovery {
    fn resolve<'a>(
        &'a self,
        node: &'a NodeId,
    ) -> Pin<Box<dyn Future<Output = Result<SocketAddr, DistError>> + Send + 'a>> {
        Box::pin(async move {
            self.entries.get(node).copied().ok_or_else(|| {
                DistError::Discovery(format!("static discovery has no entry for `{node}`"))
            })
        })
    }
}

/// DNS-based discovery via `tokio::net::lookup_host`. Resolves
/// `node.host():default_port` through the system resolver — A/AAAA
/// records suffice. Returns the first successfully-resolved
/// SocketAddr.
#[derive(Debug, Clone, Copy)]
pub struct DnsDiscovery {
    default_port: u16,
}

impl DnsDiscovery {
    pub fn new(default_port: u16) -> Self {
        Self { default_port }
    }
}

impl Discovery for DnsDiscovery {
    fn resolve<'a>(
        &'a self,
        node: &'a NodeId,
    ) -> Pin<Box<dyn Future<Output = Result<SocketAddr, DistError>> + Send + 'a>> {
        Box::pin(async move {
            let target = format!("{}:{}", node.host(), self.default_port);
            let mut iter = tokio::net::lookup_host(&target)
                .await
                .map_err(|e| DistError::Discovery(format!("dns lookup `{target}`: {e}")))?;
            iter.next().ok_or_else(|| {
                DistError::Discovery(format!("dns lookup `{target}` returned no addresses"))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> NodeId {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn static_discovery_hit() {
        let mut d = StaticDiscovery::default();
        let addr: SocketAddr = "127.0.0.1:9700".parse().unwrap();
        d.insert(id("alice@127.0.0.1"), addr);
        let resolved = d.resolve(&id("alice@127.0.0.1")).await.unwrap();
        assert_eq!(resolved, addr);
    }

    #[tokio::test]
    async fn static_discovery_miss() {
        let d = StaticDiscovery::default();
        let err = d.resolve(&id("alice@127.0.0.1")).await.unwrap_err();
        assert!(matches!(err, DistError::Discovery(_)));
    }

    #[tokio::test]
    async fn dns_discovery_resolves_localhost() {
        let d = DnsDiscovery::new(9700);
        let resolved = d.resolve(&id("alice@localhost")).await.unwrap();
        assert!(resolved.is_ipv4() || resolved.is_ipv6());
        assert_eq!(resolved.port(), 9700);
    }

    #[tokio::test]
    async fn dns_discovery_rejects_unresolvable() {
        let d = DnsDiscovery::new(9700);
        // RFC 6761 reserves `.invalid` for guaranteed-non-resolution.
        let err = d
            .resolve(&id("alice@graphreact-test.invalid"))
            .await
            .unwrap_err();
        assert!(matches!(err, DistError::Discovery(_)));
    }

    #[tokio::test]
    async fn discovery_is_object_safe() {
        // Compile-test: must be able to store any Discovery impl
        // behind `Arc<dyn Discovery>`. Higher layers (M14.4+) need
        // this for the cluster config.
        let _: std::sync::Arc<dyn Discovery> =
            std::sync::Arc::new(StaticDiscovery::default());
        let _: std::sync::Arc<dyn Discovery> = std::sync::Arc::new(DnsDiscovery::new(9700));
    }
}
