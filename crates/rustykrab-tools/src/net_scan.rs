use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Well-known ports to scan by default when no explicit list is provided.
const DEFAULT_PORTS: &[u16] = &[
    22, 80, 443, 445, 3389, 5900, 8080, 8443, // SSH, HTTP, HTTPS, SMB, RDP, VNC, alt-HTTP
    21, 23, 25, 53, 110, 143, 993, 995, // FTP, Telnet, SMTP, DNS, POP3, IMAP
    3306, 5432, 6379, 27017, // MySQL, PostgreSQL, Redis, MongoDB
    1883, 8883, 5353, 9100, // MQTT, mDNS, printers
];

/// Human-readable labels for common services.
fn service_label(port: u16) -> &'static str {
    match port {
        21 => "ftp",
        22 => "ssh",
        23 => "telnet",
        25 => "smtp",
        53 => "dns",
        80 => "http",
        110 => "pop3",
        143 => "imap",
        443 => "https",
        445 => "smb",
        993 => "imaps",
        995 => "pop3s",
        1883 => "mqtt",
        3306 => "mysql",
        3389 => "rdp",
        5353 => "mdns",
        5432 => "postgresql",
        5900 => "vnc",
        6379 => "redis",
        8080 => "http-alt",
        8443 => "https-alt",
        8883 => "mqtt-tls",
        9100 => "printer",
        27017 => "mongodb",
        _ => "unknown",
    }
}

/// Network discovery and port-scanning tool.
///
/// Provides three actions:
/// - **ping_sweep**: Probes a subnet to find live hosts (TCP connect on port 80/443).
/// - **port_scan**: Scans a list of ports on a single host.
/// - **scan_subnet**: Combines sweep + port scan for every live host found.
///
/// All operations are restricted to RFC-1918 / link-local addresses so the
/// tool cannot be used for external reconnaissance.
pub struct NetScanTool;

impl NetScanTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetScanTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Return `true` if the IP is in a private / link-local range.
fn is_local_network(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00  // unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local
        }
    }
}

/// Try a TCP connect to `addr` within `timeout_ms` milliseconds.
async fn tcp_probe(addr: SocketAddr, timeout_ms: u64) -> bool {
    timeout(Duration::from_millis(timeout_ms), TcpStream::connect(addr))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Expand a CIDR-style subnet (e.g. `192.168.1.0/24`) into individual IPv4
/// host addresses, excluding the network and broadcast addresses.
fn expand_subnet(cidr: &str) -> std::result::Result<Vec<Ipv4Addr>, String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err("expected CIDR notation, e.g. 192.168.1.0/24".into());
    }
    let base: Ipv4Addr = parts[0].parse().map_err(|e| format!("invalid IP: {e}"))?;
    let prefix_len: u32 = parts[1]
        .parse()
        .map_err(|e| format!("invalid prefix length: {e}"))?;
    if prefix_len > 32 {
        return Err("prefix length must be <= 32".into());
    }
    // Limit to /20 (4094 hosts) to avoid accidental DoS.
    if prefix_len < 20 {
        return Err(
            "prefix length must be >= 20 (max 4094 hosts) to prevent accidental DoS".into(),
        );
    }

    let base_u32 = u32::from(base);
    let mask = if prefix_len == 0 {
        0u32
    } else {
        !0u32 << (32 - prefix_len)
    };
    let network = base_u32 & mask;
    let host_count = 1u32 << (32 - prefix_len);

    let mut addrs = Vec::new();
    // Skip network address (i=0) and broadcast (i=host_count-1).
    for i in 1..host_count.saturating_sub(1) {
        addrs.push(Ipv4Addr::from(network + i));
    }
    Ok(addrs)
}

/// Perform a ping sweep by attempting a TCP connect on probe ports.
async fn ping_sweep(cidr: &str, timeout_ms: u64) -> std::result::Result<Vec<Value>, String> {
    let hosts = expand_subnet(cidr)?;
    let probe_ports: &[u16] = &[80, 443, 22, 445];

    // Verify all hosts are in local network ranges.
    for h in &hosts {
        if !is_local_network(&IpAddr::V4(*h)) {
            return Err(format!(
                "host {h} is not in a private/local range — only local-network scanning is allowed"
            ));
        }
    }

    let mut handles = Vec::new();
    for host in hosts {
        let handle = tokio::spawn(async move {
            for &port in probe_ports {
                let addr = SocketAddr::new(IpAddr::V4(host), port);
                if tcp_probe(addr, timeout_ms).await {
                    return Some(host);
                }
            }
            None
        });
        handles.push(handle);
    }

    let mut live = Vec::new();
    for h in handles {
        if let Ok(Some(ip)) = h.await {
            live.push(json!(ip.to_string()));
        }
    }

    Ok(live)
}

/// Scan a list of ports on a single host.
async fn port_scan(ip: IpAddr, ports: &[u16], timeout_ms: u64) -> Vec<Value> {
    let mut handles = Vec::new();
    for &port in ports {
        let addr = SocketAddr::new(ip, port);
        let handle = tokio::spawn(async move {
            if tcp_probe(addr, timeout_ms).await {
                Some(port)
            } else {
                None
            }
        });
        handles.push(handle);
    }

    let mut open = Vec::new();
    for h in handles {
        if let Ok(Some(port)) = h.await {
            open.push(json!({
                "port": port,
                "service": service_label(port),
            }));
        }
    }
    open
}

#[async_trait]
impl Tool for NetScanTool {
    fn name(&self) -> &str {
        "net_scan"
    }

    fn description(&self) -> &str {
        "Discover devices and open ports on the local network. Restricted to private/link-local IP ranges."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["ping_sweep", "port_scan", "scan_subnet"],
                        "description": "ping_sweep: find live hosts in a subnet. port_scan: scan ports on a single host. scan_subnet: sweep + port scan all live hosts."
                    },
                    "subnet": {
                        "type": "string",
                        "description": "CIDR subnet to scan, e.g. 192.168.1.0/24 (required for ping_sweep and scan_subnet)"
                    },
                    "host": {
                        "type": "string",
                        "description": "IP address to scan (required for port_scan)"
                    },
                    "ports": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "List of ports to scan (default: common well-known ports)"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Per-probe timeout in milliseconds (default: 1500, max: 10000)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing action".into()))?;

        let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(1500).min(10_000);

        let ports: Vec<u16> = args["ports"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u16))
                    .collect()
            })
            .unwrap_or_else(|| DEFAULT_PORTS.to_vec());

        match action {
            "ping_sweep" => {
                let subnet = args["subnet"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("subnet is required for ping_sweep".into())
                })?;

                let live = ping_sweep(subnet, timeout_ms)
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

                Ok(json!({
                    "action": "ping_sweep",
                    "subnet": subnet,
                    "live_hosts": live,
                    "count": live.len(),
                }))
            }
            "port_scan" => {
                let host_str = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("host is required for port_scan".into())
                })?;
                let ip: IpAddr = host_str.parse().map_err(|e| {
                    rustykrab_core::Error::ToolExecution(format!("invalid IP address: {e}").into())
                })?;
                if !is_local_network(&ip) {
                    return Err(rustykrab_core::Error::ToolExecution(
                        format!("{ip} is not in a private/local range — only local-network scanning is allowed").into(),
                    ));
                }

                let open = port_scan(ip, &ports, timeout_ms).await;
                Ok(json!({
                    "action": "port_scan",
                    "host": host_str,
                    "open_ports": open,
                    "scanned_count": ports.len(),
                }))
            }
            "scan_subnet" => {
                let subnet = args["subnet"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "subnet is required for scan_subnet".into(),
                    )
                })?;

                let live = ping_sweep(subnet, timeout_ms)
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

                let mut results = Vec::new();
                for host_val in &live {
                    let host_str = host_val.as_str().unwrap_or_default();
                    if let Ok(ip) = host_str.parse::<IpAddr>() {
                        let open = port_scan(ip, &ports, timeout_ms).await;
                        results.push(json!({
                            "host": host_str,
                            "open_ports": open,
                        }));
                    }
                }

                Ok(json!({
                    "action": "scan_subnet",
                    "subnet": subnet,
                    "hosts": results,
                    "live_count": live.len(),
                }))
            }
            _ => Err(rustykrab_core::Error::ToolExecution(
                format!("unknown net_scan action: {action}").into(),
            )),
        }
    }
}
