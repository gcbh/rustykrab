use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::time::Duration;
use tokio::time::timeout;

/// Network discovery tool for identifying hosts, services, and topology on the
/// local network.
///
/// Complements `net_scan` (port probing) and `net_audit` (security checks) by
/// providing higher-level discovery primitives:
///
/// - **dns_lookup**: Forward and reverse DNS resolution for multiple record
///   types (A, AAAA, MX, TXT, SRV, NS, CNAME, PTR).
/// - **mdns_discover**: Zero-configuration service discovery via mDNS/DNS-SD.
///   Finds printers, IoT devices, media servers, and other services that
///   advertise themselves on the local network segment.
/// - **arp_table**: Reads the system ARP cache to list MAC/IP mappings of
///   recently-seen devices — fast, passive, and doesn't generate traffic.
/// - **traceroute**: Maps the network path (hops) to a target host.  Restricted
///   to private/link-local targets.
/// - **interfaces**: Lists the host's own network interfaces with IP addresses,
///   subnet masks, MAC addresses, and link state.
///
/// All probing actions that reach out to a target (traceroute) are restricted to
/// RFC-1918 / link-local addresses.  Passive reads (arp_table, interfaces) and
/// DNS lookups (which query name servers, not targets) have no such restriction.
pub struct NetDiscoveryTool;

impl NetDiscoveryTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetDiscoveryTool {
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
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local
        }
    }
}

/// Validate that a host string is a local-network IP address.
fn require_local(host: &str) -> std::result::Result<IpAddr, ToolError> {
    let ip: IpAddr = host
        .parse()
        .map_err(|e| ToolError::invalid_input(format!("invalid IP address: {e}")))?;
    if !is_local_network(&ip) {
        return Err(ToolError::invalid_input(format!(
            "{ip} is not in a private/local range — only local-network targets are allowed"
        )));
    }
    Ok(ip)
}

/// Helper: resolve the system PATH for spawned subprocesses.
fn system_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string())
}

// ---------------------------------------------------------------------------
// Action implementations
// ---------------------------------------------------------------------------

/// Forward or reverse DNS lookup using `dig`.
async fn dns_lookup(name: &str, record_type: &str, timeout_ms: u64) -> Value {
    let path = system_path();

    // For PTR (reverse) lookups, use `dig -x <ip>`.
    let args: Vec<String> = if record_type.eq_ignore_ascii_case("PTR") {
        vec![
            "-x".to_string(),
            name.to_string(),
            "+short".to_string(),
            "+time=3".to_string(),
        ]
    } else {
        vec![
            name.to_string(),
            record_type.to_uppercase(),
            "+short".to_string(),
            "+time=3".to_string(),
        ]
    };

    let result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("dig")
            .args(&args)
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

            if !output.status.success() && stdout.is_empty() {
                return json!({
                    "success": false,
                    "error": if stderr.is_empty() { "dig returned no results".to_string() } else { stderr },
                });
            }

            let records: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
            json!({
                "success": true,
                "name": name,
                "record_type": record_type.to_uppercase(),
                "records": records,
            })
        }
        Ok(Err(e)) => json!({
            "success": false,
            "error": format!("dig not available: {e}"),
        }),
        Err(_) => json!({
            "success": false,
            "error": "DNS lookup timed out",
        }),
    }
}

/// Discover services on the local network via mDNS/DNS-SD using avahi-browse.
async fn mdns_discover(service_type: &str, timeout_ms: u64) -> Value {
    let path = system_path();

    // avahi-browse -t -r -p <service_type>
    //   -t: terminate after dumping cached entries
    //   -r: resolve addresses
    //   -p: parseable output
    let result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("avahi-browse")
            .args(["-t", "-r", "-p", service_type])
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut services = Vec::new();

            // Parseable output lines starting with '=' are resolved entries:
            // =;iface;protocol;name;type;domain;hostname;address;port;txt
            for line in stdout.lines() {
                if !line.starts_with('=') {
                    continue;
                }
                let fields: Vec<&str> = line.splitn(10, ';').collect();
                if fields.len() >= 9 {
                    services.push(json!({
                        "interface": fields[1],
                        "name": fields[3],
                        "type": fields[4],
                        "domain": fields[5],
                        "hostname": fields[6],
                        "address": fields[7],
                        "port": fields[8],
                        "txt": fields.get(9).unwrap_or(&""),
                    }));
                }
            }

            json!({
                "success": true,
                "service_type": service_type,
                "services": services,
                "count": services.len(),
            })
        }
        Ok(Err(e)) => {
            // Fall back to dns-sd / mdns-scan if avahi-browse is not available.
            json!({
                "success": false,
                "error": format!("avahi-browse not available: {e}. Install avahi-utils for mDNS discovery."),
            })
        }
        Err(_) => json!({
            "success": false,
            "error": "mDNS discovery timed out",
        }),
    }
}

/// Read the system ARP table for MAC/IP mappings.
async fn arp_table(timeout_ms: u64) -> Value {
    let path = system_path();

    // Prefer `ip neigh show` for modern Linux systems.
    let result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("ip")
            .args(["neigh", "show"])
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut entries = Vec::new();

            // Format: "192.168.1.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff REACHABLE"
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 4 {
                    continue;
                }
                let ip = parts[0];
                let device = if parts.len() > 2 && parts[1] == "dev" {
                    parts[2]
                } else {
                    ""
                };
                let mac = parts
                    .iter()
                    .position(|&p| p == "lladdr")
                    .and_then(|i| parts.get(i + 1))
                    .copied()
                    .unwrap_or("");
                let state = parts.last().copied().unwrap_or("");

                entries.push(json!({
                    "ip": ip,
                    "mac": mac,
                    "device": device,
                    "state": state,
                }));
            }

            json!({
                "success": true,
                "entries": entries,
                "count": entries.len(),
            })
        }
        Ok(Ok(_)) | Ok(Err(_)) => {
            // Fallback: read /proc/net/arp directly.
            match tokio::fs::read_to_string("/proc/net/arp").await {
                Ok(contents) => {
                    let mut entries = Vec::new();
                    for line in contents.lines().skip(1) {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 6 {
                            entries.push(json!({
                                "ip": parts[0],
                                "mac": parts[3],
                                "device": parts[5],
                                "state": if parts[3] == "00:00:00:00:00:00" { "INCOMPLETE" } else { "REACHABLE" },
                            }));
                        }
                    }
                    json!({
                        "success": true,
                        "entries": entries,
                        "count": entries.len(),
                        "source": "/proc/net/arp",
                    })
                }
                Err(e) => json!({
                    "success": false,
                    "error": format!("could not read ARP table: {e}"),
                }),
            }
        }
        Err(_) => json!({
            "success": false,
            "error": "ARP table read timed out",
        }),
    }
}

/// Trace the network path to a local-network target.
async fn traceroute(ip: IpAddr, timeout_ms: u64) -> Value {
    let path = system_path();
    let ip_str = ip.to_string();

    // Use traceroute with limited hops and wait time for local networks.
    let result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("traceroute")
            .args(["-n", "-m", "15", "-w", "2", &ip_str])
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut hops = Vec::new();

            // Parse traceroute output lines like:
            // " 1  192.168.1.1  0.456 ms  0.321 ms  0.298 ms"
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("traceroute") {
                    continue;
                }
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.is_empty() {
                    continue;
                }

                let hop_num = parts[0];
                let hop_ip = if parts.len() > 1 && parts[1] != "*" {
                    parts[1]
                } else {
                    "*"
                };
                // Collect RTT values (entries ending in "ms").
                let rtts: Vec<&str> = parts
                    .iter()
                    .zip(parts.iter().skip(1))
                    .filter(|(_, next)| **next == "ms")
                    .map(|(val, _)| *val)
                    .collect();

                hops.push(json!({
                    "hop": hop_num,
                    "ip": hop_ip,
                    "rtt_ms": rtts,
                }));
            }

            json!({
                "success": true,
                "target": ip_str,
                "hops": hops,
                "hop_count": hops.len(),
            })
        }
        Ok(Err(e)) => json!({
            "success": false,
            "error": format!("traceroute not available: {e}"),
        }),
        Err(_) => json!({
            "success": false,
            "error": "traceroute timed out",
        }),
    }
}

/// List local network interfaces, IPs, and MAC addresses.
async fn interfaces(timeout_ms: u64) -> Value {
    let path = system_path();

    // Try `ip -j addr show` for JSON output.
    let result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("ip")
            .args(["-j", "addr", "show"])
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            // Try to parse JSON from `ip -j`.
            if let Ok(parsed) = serde_json::from_str::<Value>(&stdout) {
                if let Some(ifaces) = parsed.as_array() {
                    let mut interfaces = Vec::new();
                    for iface in ifaces {
                        let name = iface["ifname"].as_str().unwrap_or("");
                        let mac = iface["address"].as_str().unwrap_or("");
                        let state = iface["operstate"].as_str().unwrap_or("UNKNOWN");

                        let mut addrs = Vec::new();
                        if let Some(addr_info) = iface["addr_info"].as_array() {
                            for addr in addr_info {
                                addrs.push(json!({
                                    "address": addr["local"].as_str().unwrap_or(""),
                                    "prefix_len": addr["prefixlen"],
                                    "family": addr["family"].as_str().unwrap_or(""),
                                }));
                            }
                        }

                        interfaces.push(json!({
                            "name": name,
                            "mac": mac,
                            "state": state,
                            "addresses": addrs,
                        }));
                    }
                    return json!({
                        "success": true,
                        "interfaces": interfaces,
                        "count": interfaces.len(),
                    });
                }
            }
            // JSON parsing failed — return raw output.
            json!({
                "success": true,
                "raw": stdout,
            })
        }
        _ => {
            // Fallback: plain `ip addr show`.
            let fallback = timeout(
                Duration::from_millis(timeout_ms),
                tokio::process::Command::new("ip")
                    .args(["addr", "show"])
                    .env("PATH", &path)
                    .output(),
            )
            .await;

            match fallback {
                Ok(Ok(output)) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    json!({
                        "success": true,
                        "raw": stdout,
                    })
                }
                Ok(Err(e)) => json!({
                    "success": false,
                    "error": format!("ip command not available: {e}"),
                }),
                Err(_) => json!({
                    "success": false,
                    "error": "interface listing timed out",
                }),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for NetDiscoveryTool {
    fn name(&self) -> &str {
        "net_discovery"
    }

    fn description(&self) -> &str {
        "Discover hosts, services, and network topology. DNS lookups, mDNS/DNS-SD service \
         discovery, ARP table, traceroute, and interface listing."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_read: true,
            needs_spawn: true,
            ..SandboxRequirements::default()
        }
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
                        "enum": ["dns_lookup", "mdns_discover", "arp_table", "traceroute", "interfaces"],
                        "description": "dns_lookup: forward/reverse DNS resolution. mdns_discover: find services via mDNS/DNS-SD. arp_table: read cached MAC/IP mappings. traceroute: map network hops to a local target. interfaces: list local network interfaces."
                    },
                    "name": {
                        "type": "string",
                        "description": "Domain name or IP for dns_lookup (e.g. 'example.com' or '192.168.1.1' for reverse)"
                    },
                    "record_type": {
                        "type": "string",
                        "enum": ["A", "AAAA", "MX", "TXT", "SRV", "NS", "CNAME", "PTR"],
                        "description": "DNS record type for dns_lookup (default: A). Use PTR for reverse lookups."
                    },
                    "service_type": {
                        "type": "string",
                        "description": "mDNS service type for mdns_discover (e.g. '_http._tcp', '_ssh._tcp', '_ipp._tcp', '_smb._tcp'). Default: '_services._dns-sd._udp' to list all."
                    },
                    "host": {
                        "type": "string",
                        "description": "Target IP address for traceroute (must be private/link-local)"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Timeout in milliseconds (default: 10000, max: 30000)"
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

        let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000).min(30_000);

        match action {
            "dns_lookup" => {
                let name = args["name"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "name is required for dns_lookup (domain or IP for reverse)",
                    ))
                })?;
                let record_type = args["record_type"].as_str().unwrap_or("A");

                let valid_types = ["A", "AAAA", "MX", "TXT", "SRV", "NS", "CNAME", "PTR"];
                if !valid_types
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(record_type))
                {
                    return Err(rustykrab_core::Error::ToolExecution(
                        ToolError::invalid_input(format!(
                            "unsupported record type: {record_type}. Supported: {valid_types:?}"
                        )),
                    ));
                }

                let result = dns_lookup(name, record_type, timeout_ms).await;
                Ok(json!({
                    "action": "dns_lookup",
                    "result": result,
                }))
            }

            "mdns_discover" => {
                let service_type = args["service_type"]
                    .as_str()
                    .unwrap_or("_services._dns-sd._udp");

                let result = mdns_discover(service_type, timeout_ms).await;
                Ok(json!({
                    "action": "mdns_discover",
                    "result": result,
                }))
            }

            "arp_table" => {
                let result = arp_table(timeout_ms).await;
                Ok(json!({
                    "action": "arp_table",
                    "result": result,
                }))
            }

            "traceroute" => {
                let host_str = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "host is required for traceroute",
                    ))
                })?;

                let ip = require_local(host_str).map_err(rustykrab_core::Error::ToolExecution)?;

                let result = traceroute(ip, timeout_ms).await;
                Ok(json!({
                    "action": "traceroute",
                    "result": result,
                }))
            }

            "interfaces" => {
                let result = interfaces(timeout_ms).await;
                Ok(json!({
                    "action": "interfaces",
                    "result": result,
                }))
            }

            _ => Err(rustykrab_core::Error::ToolExecution(
                ToolError::invalid_input(format!("unknown net_discovery action: {action}")),
            )),
        }
    }
}
