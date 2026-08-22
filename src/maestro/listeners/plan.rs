use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use crate::config::{ProxyConfig, ServerConfig, SynLimitMode};
use crate::transport::ListenOptions;

use super::tcp_mss_runtime_profile;

/// Immutable socket and connection policy for one listener endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListenerBindSpec {
    pub(super) addr: SocketAddr,
    pub(super) options: ListenOptions,
    pub(super) proxy_protocol: bool,
    pub(super) tls_response_fragment_size: Option<u16>,
}

fn listener_port_or_legacy(
    listener: &crate::config::ListenerConfig,
    server: &ServerConfig,
) -> u16 {
    listener.port.unwrap_or(server.port)
}

/// Derives inbound listener intent without consulting transient outbound probes.
pub(crate) fn listener_bind_plan(
    config: &ProxyConfig,
) -> Result<BTreeMap<SocketAddr, ListenerBindSpec>, String> {
    let mut plan = BTreeMap::new();
    let bulk_client_mss = config
        .server
        .client_mss_bulk_value()
        .map_err(|error| format!("invalid server.client_mss_bulk: {error}"))?;

    for listener in &config.server.listeners {
        let addr = SocketAddr::new(
            listener.ip,
            listener_port_or_legacy(listener, &config.server),
        );
        if addr.is_ipv4() && !config.network.ipv4 {
            continue;
        }
        if addr.is_ipv6() && config.network.ipv6 == Some(false) {
            continue;
        }
        let configured_client_mss = listener
            .effective_client_mss(&config.server)
            .map_err(|error| format!("invalid client MSS for listener {addr}: {error}"))?;
        #[cfg(target_os = "linux")]
        let (client_mss, tls_response_fragment_size) =
            tcp_mss_runtime_profile(configured_client_mss, bulk_client_mss);
        #[cfg(not(target_os = "linux"))]
        let (client_mss, tls_response_fragment_size) = (configured_client_mss, None);
        let spec = ListenerBindSpec {
            addr,
            options: ListenOptions {
                reuse_port: listener.reuse_allow,
                ipv6_only: listener.ip.is_ipv6(),
                backlog: config.server.listen_backlog,
                client_mss,
                ..Default::default()
            },
            proxy_protocol: listener
                .proxy_protocol
                .unwrap_or(config.server.proxy_protocol),
            tls_response_fragment_size,
        };
        if plan.insert(addr, spec).is_some() {
            return Err(format!("duplicate effective listener endpoint: {addr}"));
        }
    }

    Ok(plan)
}

fn any_synlimit_enabled(config: &ProxyConfig) -> bool {
    config
        .server
        .listeners
        .iter()
        .any(|listener| listener.synlimit != SynLimitMode::Off)
}

/// Returns whether an endpoint-only change can use coordinated process rebind.
pub(crate) fn listener_rebind_supported(old: &ProxyConfig, desired: &ProxyConfig) -> bool {
    if any_synlimit_enabled(old) || any_synlimit_enabled(desired) {
        return false;
    }
    let Ok(old_plan) = listener_bind_plan(old) else {
        return false;
    };
    let Ok(desired_plan) = listener_bind_plan(desired) else {
        return false;
    };
    let retained: BTreeSet<_> = old_plan
        .keys()
        .filter(|addr| desired_plan.contains_key(addr))
        .copied()
        .collect();
    retained
        .iter()
        .all(|addr| old_plan.get(addr) == desired_plan.get(addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ListenerConfig;

    fn listener(ip: &str, port: u16) -> ListenerConfig {
        ListenerConfig {
            ip: ip.parse().unwrap(),
            port: Some(port),
            client_mss: None,
            synlimit: SynLimitMode::Off,
            synlimit_seconds: 60,
            synlimit_hitcount: 48,
            synlimit_burst: 24,
            synlimit_ios_seconds: 1,
            synlimit_ios_hitcount: 12,
            synlimit_ios_burst: 24,
            synlimit_hashlimit_expire_ms: 60_000,
            synlimit_hashlimit_size: 32_768,
            announce: None,
            announce_ip: None,
            proxy_protocol: None,
            reuse_allow: false,
        }
    }

    #[test]
    fn plan_depends_on_inbound_family_policy_only() {
        let mut config = ProxyConfig::default();
        config.server.listeners = vec![listener("0.0.0.0", 443), listener("::", 443)];
        config.network.ipv4 = true;
        config.network.ipv6 = None;

        let plan = listener_bind_plan(&config).unwrap();

        assert_eq!(plan.len(), 2);
        config.network.ipv6 = Some(false);
        let plan = listener_bind_plan(&config).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan.keys().all(SocketAddr::is_ipv4));
    }

    #[test]
    fn duplicate_effective_endpoint_is_rejected() {
        let mut config = ProxyConfig::default();
        config.server.listeners = vec![listener("127.0.0.1", 443), listener("127.0.0.1", 443)];

        assert!(listener_bind_plan(&config).is_err());
    }

    #[test]
    fn retained_policy_change_is_not_rebindable() {
        let mut old = ProxyConfig::default();
        old.server.listeners = vec![listener("127.0.0.1", 443)];
        let mut desired = old.clone();
        desired.server.listeners[0].proxy_protocol = Some(true);

        assert!(!listener_rebind_supported(&old, &desired));
    }

    #[test]
    fn endpoint_move_without_synlimit_is_rebindable() {
        let mut old = ProxyConfig::default();
        old.server.listeners = vec![listener("127.0.0.1", 443)];
        let mut desired = old.clone();
        desired.server.listeners[0].port = Some(444);

        assert!(listener_rebind_supported(&old, &desired));
    }
}
