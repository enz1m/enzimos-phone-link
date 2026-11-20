use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
use std::net::SocketAddr;

/// The mDNS service type for enzremote signaling.
pub const SERVICE_TYPE: &str = "_enzremote._tcp.local.";

/// Interfaces that never reach a phone: container bridges, VM networks,
/// VPN tunnels. Advertising them makes clients try unreachable addresses.
pub fn is_virtual_interface(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "lo",
        "docker",
        "br-",
        "veth",
        "virbr",
        "vnet",
        "lxc",
        "lxd",
        "podman",
        "cni",
        "flannel",
        "cali",
        "vmnet",
        "vboxnet",
        "zt",
        "tun",
        "tap",
        "wg",
        "wt",
        "tailscale",
        "kube",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Up, non-virtual, non-link-local IPv4 addresses, most useful first
/// (private RFC 1918 ranges before others).
pub fn lan_addresses() -> Vec<String> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut addrs: Vec<std::net::Ipv4Addr> = interfaces
        .into_iter()
        .filter(|i| !i.is_loopback() && !is_virtual_interface(&i.name))
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_unspecified() => Some(v4),
            _ => None,
        })
        .collect();
    addrs.sort_by_key(|a| (!a.is_private(), a.octets()));
    addrs.dedup();
    addrs.into_iter().map(|a| a.to_string()).collect()
}

/// Identity published in the mDNS TXT record.
#[derive(Debug, Clone, Default)]
pub struct AdvertisedIdentity {
    pub host_id: String,
    pub name: String,
    pub cert_fingerprint: Option<String>,
}

/// Register the signaling server for LAN discovery via mDNS/DNS-SD.
///
/// Returns the `ServiceDaemon` which must be kept alive for the
/// advertisement to persist.
pub fn advertise(
    bind_addr: SocketAddr,
    hostname: &str,
    identity: &AdvertisedIdentity,
) -> Result<ServiceDaemon, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("could not start mDNS daemon: {e}"))?;
    let _ = daemon.disable_interface(mdns_sd::IfKind::Predicate(mdns_sd::IfPredicate::new(
        |intf| is_virtual_interface(&intf.name),
    )));
    let _ = daemon.disable_interface(mdns_sd::IfKind::IPv6);

    let mut properties = HashMap::new();
    properties.insert("v".into(), "1".into());
    properties.insert(
        "protocol".into(),
        if identity.cert_fingerprint.is_some() {
            "wss"
        } else {
            "ws"
        }
        .into(),
    );
    if !identity.host_id.is_empty() {
        properties.insert("id".into(), identity.host_id.clone());
    }
    if !identity.name.is_empty() {
        properties.insert("name".into(), identity.name.clone());
    }
    if let Some(fp) = &identity.cert_fingerprint {
        properties.insert("fp".into(), fp.clone());
        properties.insert("tls".into(), "1".into());
    }

    // When bound to 0.0.0.0 or ::, pass an empty address so mdns-sd
    // auto-discovers the host's real interface addresses.
    let ip = bind_addr.ip();
    let addr_str = if ip.is_unspecified() {
        String::new()
    } else {
        ip.to_string()
    };

    // Instance names must be unique on the LAN; the short ID suffix keeps two
    // computers with the same hostname apart. Clients show TXT `name`.
    let instance = if identity.host_id.len() >= 6 {
        format!("{hostname}-{}", &identity.host_id[..6])
    } else {
        hostname.to_string()
    };
    let mdns_host = format!(
        "{}.local.",
        hostname.trim_end_matches(".local.").trim_end_matches('.')
    );
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &mdns_host,
        addr_str,
        bind_addr.port(),
        properties,
    )
    .map_err(|e| format!("could not create mDNS service info: {e}"))?
    .enable_addr_auto();

    daemon
        .register(service)
        .map_err(|e| format!("could not register mDNS service: {e}"))?;

    eprintln!(
        "mDNS: advertising {SERVICE_TYPE} on {}:{} as {instance}",
        bind_addr.ip(),
        bind_addr.port()
    );

    Ok(daemon)
}

/// Discover enzremote services on the local network.
///
/// Returns after the specified timeout.
pub fn browse(timeout: std::time::Duration) -> Result<Vec<DiscoveredService>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("could not start mDNS daemon: {e}"))?;

    let browser = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| format!("could not browse {SERVICE_TYPE}: {e}"))?;

    let mut services = Vec::new();
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match browser.recv_timeout(remaining) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let addresses: Vec<String> =
                    info.get_addresses().iter().map(|a| a.to_string()).collect();
                services.push(DiscoveredService {
                    name: info.get_fullname().to_string(),
                    hostname: info.get_hostname().to_string(),
                    port: info.get_port(),
                    addresses,
                    protocol_version: info
                        .get_properties()
                        .get("v")
                        .map(|v| v.val_str().to_string())
                        .unwrap_or_default(),
                });
            }
            Ok(_) => continue,
            Err(_) => break, // Timeout or channel closed.
        }
    }

    let _ = daemon.shutdown();
    Ok(services)
}

/// A discovered enzremote service.
#[derive(Debug, Clone)]
pub struct DiscoveredService {
    pub name: String,
    pub hostname: String,
    pub port: u16,
    pub addresses: Vec<String>,
    pub protocol_version: String,
}
