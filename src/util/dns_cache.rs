//! Simple async DNS cache to reduce repeated lookups for popular domains.

use crate::util::{AnyTlsError, Result};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::lookup_host;
use tokio::sync::RwLock;
use tracing::{debug, info, trace};
use trust_dns_resolver::TokioAsyncResolver;
use trust_dns_resolver::config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts};

/// TTL for cached DNS entries.
const DEFAULT_TTL: Duration = Duration::from_secs(60);
/// Timeout for DNS lookup operations.
const DNS_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound memory retained for client-controlled hostnames.
const MAX_CACHE_ENTRIES: usize = 4096;

static DNS_CACHE: Lazy<DnsCache> = Lazy::new(DnsCache::new);
static DNS_RESOLVER: Lazy<RwLock<Option<Arc<TokioAsyncResolver>>>> =
    Lazy::new(|| RwLock::new(None));

struct CacheEntry {
    addresses: Vec<IpAddr>,
    expires_at: Instant,
    next_index: usize,
}

pub struct DnsCache {
    inner: RwLock<HashMap<String, CacheEntry>>,
}

impl DnsCache {
    fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    async fn get(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        let mut cache = self.inner.write().await;
        let entry = cache.get_mut(host)?;
        if Instant::now() >= entry.expires_at || entry.addresses.is_empty() {
            cache.remove(host);
            return None;
        }
        // Select and advance under one lock, so concurrent requests rotate too.
        let index = entry.next_index % entry.addresses.len();
        entry.next_index = (index + 1) % entry.addresses.len();
        let addresses = entry.addresses[index..]
            .iter()
            .chain(&entry.addresses[..index])
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        trace!("[DNS] Cache hit for {}", host);
        Some(addresses)
    }

    async fn insert(&self, host: String, addresses: Vec<IpAddr>) {
        let mut cache = self.inner.write().await;
        if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(&host) {
            let now = Instant::now();
            cache.retain(|_, entry| entry.expires_at > now);
            if cache.len() >= MAX_CACHE_ENTRIES {
                let oldest = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.expires_at)
                    .map(|(host, _)| host.clone());
                if let Some(oldest) = oldest {
                    cache.remove(&oldest);
                }
            }
        }
        cache.insert(
            host,
            CacheEntry {
                addresses,
                expires_at: Instant::now() + DEFAULT_TTL,
                // The lookup that fills this entry uses the first address.
                next_index: 1,
            },
        );
    }

    async fn clear(&self) {
        let mut cache = self.inner.write().await;
        cache.clear();
    }
}

/// Resolve one address, rotating cached IPs and attaching this request's port.
pub async fn resolve_host_with_cache(host: &str, port: u16) -> Result<SocketAddr> {
    resolve_host_addresses_with_cache(host, port)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AnyTlsError::Protocol(format!("No address found for {}", host)))
}

/// Resolve all candidate addresses with caching and timeout.
///
/// Only IPs are cached: ports belong to individual proxy requests. Cached results
/// rotate their starting address but retain every candidate for dial fallback.
pub async fn resolve_host_addresses_with_cache(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    if let Some(addresses) = DNS_CACHE.get(host, port).await {
        return Ok(addresses);
    }

    let resolver_opt = DNS_RESOLVER.read().await.clone();
    let lookup = async {
        if let Some(resolver) = resolver_opt {
            resolver
                .lookup_ip(host)
                .await
                .map(|lookup| lookup.iter().collect::<Vec<_>>())
                .map_err(|err| Error::other(err.to_string()))
        } else {
            lookup_host((host, 0))
                .await
                .map(|lookup| lookup.map(|address| address.ip()).collect::<Vec<_>>())
        }
    };
    let mut addresses = tokio::time::timeout(DNS_TIMEOUT, lookup)
        .await
        .map_err(|_| {
            AnyTlsError::Protocol(format!(
                "DNS resolution timeout ({}s) for {}",
                DNS_TIMEOUT.as_secs(),
                host
            ))
        })?
        .map_err(|err| {
            AnyTlsError::Io(Error::other(format!(
                "DNS resolution failed for {}: {}",
                host, err
            )))
        })?;

    if addresses.is_empty() {
        return Err(AnyTlsError::Protocol(format!(
            "No address found for {}",
            host
        )));
    }

    // Stable order on the first lookup; subsequent hits rotate the candidates.
    addresses.sort_unstable();
    addresses.dedup();
    debug!(
        "[DNS] Resolved {} -> {} entries (ttl={}s)",
        host,
        addresses.len(),
        DEFAULT_TTL.as_secs()
    );
    DNS_CACHE.insert(host.to_string(), addresses.clone()).await;
    Ok(addresses
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

pub async fn set_custom_dns_servers(servers: &[String]) -> Result<()> {
    let mut parsed_servers = Vec::new();
    for raw in servers {
        let socket = parse_dns_server(raw)
            .map_err(|err| AnyTlsError::Config(format!("Invalid DNS server '{}': {}", raw, err)))?;
        parsed_servers.push(socket);
    }

    let mut resolver_guard = DNS_RESOLVER.write().await;

    if parsed_servers.is_empty() {
        *resolver_guard = None;
        DNS_CACHE.clear().await;
        info!("[DNS] Using system DNS resolver");
        return Ok(());
    }

    let mut resolver_config = ResolverConfig::new();
    for server in &parsed_servers {
        resolver_config.add_name_server(NameServerConfig::new(*server, Protocol::Udp));
        resolver_config.add_name_server(NameServerConfig::new(*server, Protocol::Tcp));
    }

    let resolver = TokioAsyncResolver::tokio(resolver_config, ResolverOpts::default());
    *resolver_guard = Some(Arc::new(resolver));
    DNS_CACHE.clear().await;

    info!(
        "[DNS] Custom DNS servers configured: {}",
        parsed_servers
            .iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    Ok(())
}

fn parse_dns_server(entry: &str) -> std::io::Result<SocketAddr> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "DNS server address is empty",
        ));
    }

    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(addr);
    }

    // Allow IPv6 without port (e.g., "2001:4860:4860::8888")
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 53));
    }

    // Allow bracketed IPv6 without port (e.g., "[2001:4860:4860::8888]")
    if trimmed.starts_with('[')
        && trimmed.ends_with(']')
        && let Ok(ip) = trimmed[1..trimmed.len() - 1].parse::<IpAddr>()
    {
        return Ok(SocketAddr::new(ip, 53));
    }

    Err(Error::new(
        ErrorKind::InvalidInput,
        format!("invalid DNS server '{}'", entry),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cached_dns_keeps_request_ports_and_all_candidates() {
        let cache = DnsCache::new();
        let ips = vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()];
        cache.insert("example.test".into(), ips.clone()).await;
        let https = cache.get("example.test", 443).await.unwrap();
        let http = cache.get("example.test", 80).await.unwrap();
        assert_eq!(
            https,
            vec![SocketAddr::new(ips[1], 443), SocketAddr::new(ips[0], 443)]
        );
        assert_eq!(
            http,
            vec![SocketAddr::new(ips[0], 80), SocketAddr::new(ips[1], 80)]
        );
    }

    #[tokio::test]
    async fn expired_entries_are_removed_and_cache_is_bounded() {
        let cache = DnsCache::new();
        let ip = "127.0.0.1".parse().unwrap();
        cache.insert("expired.test".into(), vec![ip]).await;
        cache
            .inner
            .write()
            .await
            .get_mut("expired.test")
            .unwrap()
            .expires_at = Instant::now();
        assert!(cache.get("expired.test", 80).await.is_none());
        assert!(cache.inner.read().await.is_empty());
        for index in 0..=MAX_CACHE_ENTRIES {
            cache.insert(format!("{}.test", index), vec![ip]).await;
        }
        assert_eq!(cache.inner.read().await.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get("0.test", 80).await.is_none());
    }

    #[tokio::test]
    async fn ip_literals_keep_the_requested_port() {
        assert_eq!(
            resolve_host_with_cache("::1", 5353).await.unwrap(),
            "[::1]:5353".parse().unwrap()
        );
        assert_eq!(
            resolve_host_addresses_with_cache("127.0.0.1", 443)
                .await
                .unwrap(),
            vec!["127.0.0.1:443".parse().unwrap()]
        );
    }
}
