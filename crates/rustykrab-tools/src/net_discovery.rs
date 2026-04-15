use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
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
///   advertise themselves on the local network segment.  Supports IoT service
///   type presets for common smart-home protocols.
/// - **arp_table**: Reads the system ARP cache to list MAC/IP mappings of
///   recently-seen devices — fast, passive, and doesn't generate traffic.
/// - **arp_scan**: Actively scans a subnet to discover all IP-connected devices
///   (including those with static IPs) via ARP requests.
/// - **dhcp_leases**: Queries a router's DHCP lease table over SSH to discover
///   all DHCP-assigned devices with hostnames, IPs, and MAC addresses.
/// - **ssdp_discover**: Discovers UPnP devices on the local network via SSDP
///   M-SEARCH multicast (Hue bridges, smart TVs, NAS, media servers).
/// - **oui_lookup**: Looks up the manufacturer/vendor for a MAC address using
///   the IEEE OUI database.
/// - **traceroute**: Maps the network path (hops) to a target host.  Restricted
///   to private/link-local targets.
/// - **interfaces**: Lists the host's own network interfaces with IP addresses,
///   subnet masks, MAC addresses, and link state.
///
/// All probing actions that reach out to a target (traceroute, arp_scan) are
/// restricted to RFC-1918 / link-local addresses.  Passive reads (arp_table,
/// interfaces) and DNS lookups (which query name servers, not targets) have no
/// such restriction.
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

/// Resolve an mDNS preset name to a list of well-known service types.
fn mdns_preset_service_types(preset: &str) -> std::result::Result<Vec<&'static str>, String> {
    match preset {
        "homekit" => Ok(vec!["_hap._tcp"]),
        "chromecast" => Ok(vec!["_googlecast._tcp"]),
        "sonos" => Ok(vec!["_sonos._tcp"]),
        "airplay" => Ok(vec!["_airplay._tcp", "_raop._tcp"]),
        "printers" => Ok(vec!["_ipp._tcp", "_printer._tcp", "_pdl-datastream._tcp"]),
        "iot" => Ok(vec![
            "_hap._tcp",          // HomeKit
            "_googlecast._tcp",   // Chromecast / Google Home / Nest Hub
            "_sonos._tcp",        // Sonos
            "_airplay._tcp",      // AirPlay 2
            "_raop._tcp",         // AirPlay (Remote Audio Output)
            "_http._tcp",         // Generic HTTP (many IoT web UIs)
            "_mqtt._tcp",         // MQTT brokers
            "_esphomelib._tcp",   // ESPHome devices
            "_miio._udp",         // Xiaomi IoT
            "_printer._tcp",      // Network printers
            "_ipp._tcp",          // IPP printers
            "_smb._tcp",          // SMB file shares
            "_ssh._tcp",          // SSH servers
        ]),
        other => Err(format!(
            "unknown mDNS preset '{other}'. Available: homekit, chromecast, sonos, airplay, printers, iot"
        )),
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
// Active ARP scan
// ---------------------------------------------------------------------------

/// Detect the default network interface by parsing `ip route show default`.
///
/// Returns the device name from the first default route (e.g. "enp0s3",
/// "wlan0", "eth0").  Returns `None` if no default route exists or the
/// `ip` command is unavailable.
async fn detect_default_interface() -> Option<String> {
    let path = system_path();
    let output = tokio::process::Command::new("ip")
        .args(["route", "show", "default"])
        .env("PATH", &path)
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Typical line: "default via 192.168.1.1 dev enp0s3 proto dhcp metric 100"
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(idx) = parts.iter().position(|&p| p == "dev") {
            if let Some(iface) = parts.get(idx + 1) {
                return Some((*iface).to_string());
            }
        }
    }
    None
}

/// Validate that an interface name is safe to pass as a CLI argument.
///
/// Only allows alphanumeric characters, hyphens, underscores, and dots —
/// typical Linux interface naming (e.g. "eth0", "enp0s3", "wlan0", "br-lan").
fn validate_interface_name(name: &str) -> std::result::Result<(), ToolError> {
    if name.is_empty() {
        return Err(ToolError::invalid_input("interface name must not be empty"));
    }
    if name.starts_with('-') {
        return Err(ToolError::invalid_input(
            "interface name must not start with '-'",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ToolError::invalid_input(
            "interface name contains disallowed characters",
        ));
    }
    Ok(())
}

/// Actively scan a subnet for all devices using ARP requests.
///
/// When `interface` is `None`, auto-detects the default interface via
/// `ip route`.  Tries `arp-scan` first (most reliable, returns vendor
/// info).  Falls back to `nmap -sn` (ping scan + ARP on local segment).
async fn arp_scan(subnet: &str, interface: Option<&str>, timeout_ms: u64) -> Value {
    let path = system_path();

    // Resolve the interface to use.
    let iface = match interface {
        Some(i) => i.to_string(),
        None => detect_default_interface()
            .await
            .unwrap_or_else(|| "eth0".to_string()),
    };
    let iface_arg = format!("--interface={iface}");

    // Try arp-scan first.
    let arp_scan_result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("arp-scan")
            .args([&iface_arg, "--retry=1", "--timeout=500", subnet])
            .env("PATH", &path)
            .output(),
    )
    .await;

    if let Ok(Ok(output)) = arp_scan_result {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut entries = Vec::new();

            for line in stdout.lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    // arp-scan output: IP\tMAC\tVendor
                    let ip_str = parts[0].trim();
                    // Only include lines that start with a valid IP.
                    if ip_str.parse::<IpAddr>().is_ok() {
                        entries.push(json!({
                            "ip": ip_str,
                            "mac": parts[1].trim(),
                            "vendor": parts[2].trim(),
                        }));
                    }
                }
            }

            return json!({
                "success": true,
                "subnet": subnet,
                "interface": iface,
                "source": "arp-scan",
                "entries": entries,
                "count": entries.len(),
            });
        }
    }

    // Fallback: nmap -sn (ping/ARP scan), with -e <interface>.
    let nmap_result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("nmap")
            .args(["-sn", "-n", "-e", &iface, subnet])
            .env("PATH", &path)
            .output(),
    )
    .await;

    match nmap_result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut entries = Vec::new();
            let mut current_ip = String::new();
            let mut current_mac = String::new();
            let mut current_vendor = String::new();

            // nmap output blocks look like:
            //   Nmap scan report for 192.168.1.1
            //   Host is up (0.0012s latency).
            //   MAC Address: AA:BB:CC:DD:EE:FF (Vendor Name)
            for line in stdout.lines() {
                let line = line.trim();
                if line.starts_with("Nmap scan report for ") {
                    // Flush previous entry.
                    if !current_ip.is_empty() {
                        entries.push(json!({
                            "ip": current_ip,
                            "mac": current_mac,
                            "vendor": current_vendor,
                        }));
                    }
                    current_ip = line.trim_start_matches("Nmap scan report for ").to_string();
                    current_mac = String::new();
                    current_vendor = String::new();
                } else if line.starts_with("MAC Address: ") {
                    let rest = line.trim_start_matches("MAC Address: ");
                    if let Some((mac, vendor)) = rest.split_once(' ') {
                        current_mac = mac.to_string();
                        current_vendor = vendor
                            .trim_start_matches('(')
                            .trim_end_matches(')')
                            .to_string();
                    } else {
                        current_mac = rest.to_string();
                    }
                }
            }
            // Flush last entry.
            if !current_ip.is_empty() {
                entries.push(json!({
                    "ip": current_ip,
                    "mac": current_mac,
                    "vendor": current_vendor,
                }));
            }

            json!({
                "success": true,
                "subnet": subnet,
                "interface": iface,
                "source": "nmap",
                "entries": entries,
                "count": entries.len(),
            })
        }
        Ok(Err(e)) => json!({
            "success": false,
            "error": format!(
                "Neither arp-scan nor nmap available: {e}. \
                 Install arp-scan or nmap for active ARP scanning."
            ),
        }),
        Err(_) => json!({
            "success": false,
            "error": "ARP scan timed out",
        }),
    }
}

// ---------------------------------------------------------------------------
// OUI vendor lookup
// ---------------------------------------------------------------------------

/// Normalize a MAC address to an uppercase OUI prefix (first 3 octets).
///
/// Accepts "AA:BB:CC:DD:EE:FF", "AA-BB-CC-DD-EE-FF", or "AABBCCDDEEFF".
fn normalize_oui_prefix(mac: &str) -> Option<String> {
    let cleaned: String = mac
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase();
    if cleaned.len() < 6 {
        return None;
    }
    // Return "AA:BB:CC" format.
    Some(format!(
        "{}:{}:{}",
        &cleaned[0..2],
        &cleaned[2..4],
        &cleaned[4..6]
    ))
}

/// Built-in lookup table for common IoT / smart-home OUI prefixes.
///
/// This covers the most frequently seen vendors in home-network device
/// discovery.  For comprehensive lookups the tool also checks system-installed
/// IEEE databases.
fn builtin_oui_lookup(prefix: &str) -> Option<&'static str> {
    // prefix is "AA:BB:CC" uppercase
    match prefix {
        // Amazon
        "F0:F0:A4" | "A4:08:EA" | "74:C2:46" | "FC:65:DE" | "4C:EF:C0" | "0C:47:C9" => {
            Some("Amazon Technologies (Echo, Ring, Fire TV)")
        }
        // Google / Nest
        "F4:F5:D8" | "54:60:09" | "A4:77:33" | "30:FD:38" | "48:D6:D5" | "6C:AD:F8" => {
            Some("Google LLC (Nest, Chromecast, Google Home)")
        }
        // Apple
        "3C:22:FB" | "A8:5C:2C" | "F0:B3:EC" | "AC:BC:32" | "D0:03:4B" | "70:56:81" => {
            Some("Apple Inc. (HomePod, Apple TV, HomeKit)")
        }
        // Philips Hue / Signify
        "00:17:88" | "EC:B5:FA" => Some("Signify / Philips Lighting (Hue bridge, bulbs)"),
        // Espressif (ESP32/ESP8266)
        "24:0A:C4" | "30:AE:A4" | "A4:CF:12" | "AC:67:B2" | "CC:50:E3" | "24:6F:28"
        | "84:CC:A8" | "3C:61:05" | "EC:FA:BC" | "10:52:1C" => {
            Some("Espressif Systems (ESP32/ESP8266 — Tasmota, ESPHome, DIY IoT)")
        }
        // TP-Link / Kasa
        "50:C7:BF" | "60:A4:B7" | "1C:61:B4" | "98:DA:C4" | "B0:95:75" | "E8:48:B8" => {
            Some("TP-Link Technologies (Kasa smart plugs, routers)")
        }
        // Belkin / WeMo
        "94:10:3E" | "C4:41:1E" | "EC:1A:59" | "08:86:3B" => {
            Some("Belkin International (WeMo smart plugs)")
        }
        // Sonos
        "00:0E:58" | "34:7E:5C" | "48:A6:B8" | "5C:AA:FD" | "78:28:CA" | "B8:E9:37" => {
            Some("Sonos Inc. (speakers, amps)")
        }
        // Samsung / SmartThings
        "8C:79:F5" | "D0:66:7B" | "FC:A6:67" | "C4:73:1E" => {
            Some("Samsung Electronics (SmartThings, TVs)")
        }
        // Tuya
        "D8:1F:12" | "10:D5:61" | "A0:92:08" => Some("Tuya Smart (generic smart-home devices)"),
        // Shelly (Allterco Robotics)
        "E8:DB:84" | "EC:62:60" | "30:C6:F7" | "C8:F0:9E" => {
            Some("Allterco Robotics (Shelly relays, plugs)")
        }
        // IKEA
        "00:0B:57" | "D0:73:D5" | "CC:86:EC" | "94:3A:F0" => {
            Some("IKEA (TRADFRI / Dirigera smart home)")
        }
        // Roku
        "D8:31:34" | "C8:3A:6B" | "B0:A7:37" | "AC:3A:7A" => {
            Some("Roku Inc. (streaming players, TVs)")
        }
        // Raspberry Pi
        "B8:27:EB" | "DC:A6:32" | "E4:5F:01" | "D8:3A:DD" | "28:CD:C1" => {
            Some("Raspberry Pi Foundation")
        }
        // Synology
        "00:11:32" => Some("Synology Inc. (NAS)"),
        // QNAP
        "00:08:9B" | "24:5E:BE" => Some("QNAP Systems (NAS)"),
        // Ring (Amazon)
        "B0:09:DA" | "18:B4:30" => Some("Ring LLC (doorbells, cameras)"),
        // Wyze
        "2C:AA:8E" => Some("Wyze Labs (cameras, sensors)"),
        // Ecobee
        "44:61:32" => Some("ecobee Inc. (thermostats)"),
        _ => None,
    }
}

/// Look up the vendor/manufacturer for a MAC address.
///
/// Checks system-installed IEEE OUI databases at standard paths, then falls
/// back to a built-in table of common IoT/smart-home vendors.
async fn oui_lookup(mac: &str) -> Value {
    let prefix = match normalize_oui_prefix(mac) {
        Some(p) => p,
        None => {
            return json!({
                "success": false,
                "error": "invalid MAC address — expected at least 6 hex digits (e.g. 'AA:BB:CC:DD:EE:FF')",
            });
        }
    };

    // Prefix without colons for matching some file formats.
    let prefix_nocolon = prefix.replace(':', "");

    // Try system OUI databases.
    let oui_paths = [
        "/usr/share/ieee-data/oui.csv",
        "/usr/share/misc/oui.txt",
        "/usr/share/nmap/nmap-mac-prefixes",
        "/var/lib/ieee-data/oui.csv",
    ];

    for path in &oui_paths {
        if let Ok(contents) = tokio::fs::read_to_string(path).await {
            // Search for the OUI prefix in the file.
            for line in contents.lines() {
                let upper = line.to_uppercase();
                if upper.contains(&prefix) || upper.contains(&prefix_nocolon) {
                    // Extract vendor name — varies by format.
                    let vendor = if path.ends_with(".csv") {
                        // CSV format: MA-L,AABBCC,Vendor Name,...
                        line.split(',')
                            .nth(2)
                            .unwrap_or("")
                            .trim()
                            .trim_matches('"')
                    } else if path.ends_with("nmap-mac-prefixes") {
                        // nmap format: AABBCC Vendor Name
                        line.split_once(' ')
                            .or_else(|| line.split_once('\t'))
                            .map(|(_, v)| v.trim())
                            .unwrap_or("")
                    } else {
                        // oui.txt format: AA-BB-CC (hex)\tVendor Name
                        line.split_once('\t')
                            .or_else(|| line.split_once("  "))
                            .map(|(_, v)| v.trim())
                            .unwrap_or("")
                    };

                    if !vendor.is_empty() {
                        return json!({
                            "success": true,
                            "mac": mac,
                            "oui_prefix": prefix,
                            "vendor": vendor,
                            "source": path,
                        });
                    }
                }
            }
        }
    }

    // Fall back to built-in table.
    if let Some(vendor) = builtin_oui_lookup(&prefix) {
        return json!({
            "success": true,
            "mac": mac,
            "oui_prefix": prefix,
            "vendor": vendor,
            "source": "builtin",
        });
    }

    json!({
        "success": false,
        "mac": mac,
        "oui_prefix": prefix,
        "error": "OUI prefix not found in available databases",
    })
}

// ---------------------------------------------------------------------------
// SSDP / UPnP discovery
// ---------------------------------------------------------------------------

const SSDP_MULTICAST_ADDR: &str = "239.255.255.250:1900";
const SSDP_M_SEARCH: &str = "M-SEARCH * HTTP/1.1\r\n\
                               HOST: 239.255.255.250:1900\r\n\
                               MAN: \"ssdp:discover\"\r\n\
                               MX: 3\r\n\
                               ST: ssdp:all\r\n\r\n";

/// Parse an SSDP response into key-value headers.
fn parse_ssdp_headers(response: &str) -> Value {
    let mut headers = serde_json::Map::new();
    for line in response.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_uppercase();
            let value = value.trim().to_string();
            if !key.is_empty() && !value.is_empty() {
                headers.insert(key, Value::String(value));
            }
        }
    }
    Value::Object(headers)
}

/// Extract a friendly name from UPnP device description XML.
fn extract_xml_field(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let Some(start) = xml.find(&open) {
        let start = start + open.len();
        if let Some(end) = xml[start..].find(&close) {
            let value = xml[start..start + end].trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Discover UPnP devices on the local network via SSDP M-SEARCH.
///
/// Sends a multicast M-SEARCH request and collects responses.  When
/// `fetch_descriptions` is true, fetches the device description XML from each
/// device's LOCATION URL to extract friendly names, device types, and
/// manufacturer info.
async fn ssdp_discover(timeout_ms: u64, fetch_descriptions: bool) -> Value {
    let sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            return json!({
                "success": false,
                "error": format!("failed to bind UDP socket: {e}"),
            });
        }
    };

    // Enable broadcast / multicast.
    if let Err(e) = sock.set_broadcast(true) {
        return json!({
            "success": false,
            "error": format!("failed to set broadcast: {e}"),
        });
    }

    // Send M-SEARCH.
    let dest: std::net::SocketAddr = match SSDP_MULTICAST_ADDR.parse() {
        Ok(a) => a,
        Err(e) => {
            return json!({
                "success": false,
                "error": format!("invalid multicast address: {e}"),
            });
        }
    };

    if let Err(e) = sock.send_to(SSDP_M_SEARCH.as_bytes(), dest).await {
        return json!({
            "success": false,
            "error": format!("failed to send M-SEARCH: {e}"),
        });
    }

    // Collect responses until timeout.
    let mut devices = Vec::new();
    let mut seen_usns = std::collections::HashSet::new();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.min(15_000));
    let mut buf = [0u8; 4096];

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, addr))) => {
                let resp = String::from_utf8_lossy(&buf[..n]).to_string();
                let headers = parse_ssdp_headers(&resp);

                // De-duplicate by USN (Unique Service Name).
                let usn = headers["USN"].as_str().unwrap_or("").to_string();
                if !usn.is_empty() && !seen_usns.insert(usn.clone()) {
                    continue;
                }

                let location = headers["LOCATION"].as_str().unwrap_or("").to_string();
                let st = headers["ST"].as_str().unwrap_or("").to_string();
                let server = headers["SERVER"].as_str().unwrap_or("").to_string();

                devices.push(json!({
                    "ip": addr.ip().to_string(),
                    "port": addr.port(),
                    "usn": usn,
                    "st": st,
                    "location_url": location,
                    "server": server,
                }));
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }

    // Optionally fetch device description XML from LOCATION URLs.
    if fetch_descriptions {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        for device in &mut devices {
            let location = device["location_url"].as_str().unwrap_or("").to_string();
            if location.is_empty() || !location.starts_with("http") {
                continue;
            }
            if let Ok(resp) = client.get(&location).send().await {
                if let Ok(body) = resp.text().await {
                    // Extract key fields from XML.
                    let friendly_name = extract_xml_field(&body, "friendlyName");
                    let device_type = extract_xml_field(&body, "deviceType");
                    let manufacturer = extract_xml_field(&body, "manufacturer");
                    let model_name = extract_xml_field(&body, "modelName");
                    let model_number = extract_xml_field(&body, "modelNumber");

                    if let Some(obj) = device.as_object_mut() {
                        if let Some(v) = friendly_name {
                            obj.insert("friendly_name".to_string(), Value::String(v));
                        }
                        if let Some(v) = device_type {
                            obj.insert("device_type".to_string(), Value::String(v));
                        }
                        if let Some(v) = manufacturer {
                            obj.insert("manufacturer".to_string(), Value::String(v));
                        }
                        if let Some(v) = model_name {
                            obj.insert("model_name".to_string(), Value::String(v));
                        }
                        if let Some(v) = model_number {
                            obj.insert("model_number".to_string(), Value::String(v));
                        }
                    }
                }
            }
        }
    }

    json!({
        "success": true,
        "devices": devices,
        "count": devices.len(),
    })
}

// ---------------------------------------------------------------------------
// DHCP lease table
// ---------------------------------------------------------------------------

/// Validate that an SSH username is safe.
///
/// Rejects values that start with `-` (which could be interpreted as SSH
/// options, enabling option injection) and values containing shell
/// metacharacters.  Only allows alphanumeric characters, hyphens (not as
/// the first character), underscores, and dots.
fn validate_ssh_user(user: &str) -> std::result::Result<(), ToolError> {
    if user.is_empty() {
        return Err(ToolError::invalid_input("SSH user must not be empty"));
    }
    if user.starts_with('-') {
        return Err(ToolError::invalid_input(
            "SSH user must not start with '-' (option injection)",
        ));
    }
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ToolError::invalid_input(
            "SSH user contains disallowed characters — \
             only alphanumeric, '-', '_', '.' are permitted",
        ));
    }
    Ok(())
}

/// Validate a CIDR subnet string for use with scanning tools.
///
/// Ensures the string is well-formed (`<ipv4>/<prefix>`), the base IP is
/// in a local network range, and the prefix length is between 20 and 32
/// to prevent accidental DoS via overly broad scans.
fn validate_scan_cidr(cidr: &str) -> std::result::Result<(), ToolError> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(ToolError::invalid_input(
            "expected CIDR notation, e.g. '192.168.1.0/24'",
        ));
    }

    let ip: IpAddr = parts[0]
        .parse()
        .map_err(|e| ToolError::invalid_input(format!("invalid IP in CIDR: {e}")))?;

    if !is_local_network(&ip) {
        return Err(ToolError::invalid_input(format!(
            "{ip} is not in a private/local range — only local-network scanning is allowed"
        )));
    }

    let prefix_len: u32 = parts[1]
        .parse()
        .map_err(|e| ToolError::invalid_input(format!("invalid prefix length: {e}")))?;

    if prefix_len > 32 {
        return Err(ToolError::invalid_input("prefix length must be <= 32"));
    }
    if prefix_len < 20 {
        return Err(ToolError::invalid_input(
            "prefix length must be >= 20 (max 4094 hosts) to prevent accidental DoS",
        ));
    }

    Ok(())
}

/// Validate that a lease file path is safe to pass to a remote shell.
///
/// Rejects paths containing shell metacharacters, backticks, command
/// substitution, pipes, redirects, semicolons, etc.  Only allows
/// absolute paths with alphanumeric segments, hyphens, underscores,
/// dots, and forward slashes.
fn validate_lease_path(path: &str) -> std::result::Result<(), ToolError> {
    if path.is_empty() {
        return Err(ToolError::invalid_input("lease_file must not be empty"));
    }
    if !path.starts_with('/') {
        return Err(ToolError::invalid_input(
            "lease_file must be an absolute path (start with '/')",
        ));
    }
    if path.contains("..") {
        return Err(ToolError::invalid_input(
            "lease_file must not contain '..' path traversal",
        ));
    }
    // Allow only safe characters: alphanumeric, '/', '-', '_', '.'
    if !path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        return Err(ToolError::invalid_input(
            "lease_file contains disallowed characters — \
             only alphanumeric, '/', '-', '_', '.' are permitted",
        ));
    }
    Ok(())
}

/// Query a router's DHCP lease table over SSH.
///
/// Connects to the router at `host` as `user` (default "root") and reads the
/// dnsmasq lease file.  The standard location is `/tmp/dnsmasq.leases` on
/// OpenWrt/DD-WRT routers, but `lease_file` can be overridden.
///
/// Lease line format: `epoch mac ip hostname client-id`
async fn dhcp_leases(host: &str, user: &str, lease_file: &str, timeout_ms: u64) -> Value {
    let path = system_path();

    // Pass the file path as a separate argument to `cat` instead of
    // interpolating it into a shell string, preventing command injection.
    // SSH with "--" separates ssh options from the remote command; each
    // subsequent argument becomes a separate word in the remote argv.
    let result = timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-l",
                user,
                host,
                "--",
                "cat",
                lease_file,
            ])
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

            if !output.status.success() {
                return json!({
                    "success": false,
                    "error": if stderr.is_empty() {
                        format!("SSH command failed with exit code {}", output.status.code().unwrap_or(-1))
                    } else {
                        stderr
                    },
                });
            }

            let mut leases = Vec::new();
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 4 {
                    continue;
                }
                // dnsmasq format: epoch mac ip hostname [client-id]
                let expires = parts[0].parse::<u64>().unwrap_or(0);
                let mac = parts[1];
                let ip = parts[2];
                let hostname = parts[3];
                let client_id = parts.get(4).copied().unwrap_or("");

                leases.push(json!({
                    "ip": ip,
                    "mac": mac,
                    "hostname": if hostname == "*" { "" } else { hostname },
                    "lease_expires": expires,
                    "client_id": client_id,
                }));
            }

            json!({
                "success": true,
                "router": host,
                "leases": leases,
                "count": leases.len(),
            })
        }
        Ok(Err(e)) => json!({
            "success": false,
            "error": format!("SSH not available: {e}"),
        }),
        Err(_) => json!({
            "success": false,
            "error": "DHCP lease query timed out",
        }),
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
         discovery, DHCP lease table, active ARP scan, SSDP/UPnP, OUI vendor lookup, \
         ARP cache, traceroute, and interface listing."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_read: true,
            needs_net: true,
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
                        "enum": [
                            "dns_lookup", "mdns_discover", "arp_table", "arp_scan",
                            "dhcp_leases", "ssdp_discover", "oui_lookup",
                            "traceroute", "interfaces"
                        ],
                        "description": "dns_lookup: forward/reverse DNS resolution. mdns_discover: find services via mDNS/DNS-SD (supports IoT presets). arp_table: read cached MAC/IP mappings. arp_scan: actively scan a subnet for all devices via ARP. dhcp_leases: query router DHCP lease table over SSH. ssdp_discover: find UPnP devices via SSDP multicast. oui_lookup: look up MAC address vendor/manufacturer. traceroute: map network hops to a local target. interfaces: list local network interfaces."
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
                        "description": "mDNS service type for mdns_discover (e.g. '_http._tcp', '_ssh._tcp', '_hap._tcp'). Default: '_services._dns-sd._udp' to list all."
                    },
                    "preset": {
                        "type": "string",
                        "enum": ["homekit", "chromecast", "sonos", "airplay", "iot", "printers"],
                        "description": "mDNS preset for mdns_discover — browses well-known IoT service types. 'iot' scans all smart-home types. Overrides service_type."
                    },
                    "host": {
                        "type": "string",
                        "description": "Target IP for traceroute (must be private/link-local). Router IP for dhcp_leases."
                    },
                    "user": {
                        "type": "string",
                        "description": "SSH user for dhcp_leases (default: 'root')"
                    },
                    "lease_file": {
                        "type": "string",
                        "description": "Path to lease file on router for dhcp_leases (default: '/tmp/dnsmasq.leases')"
                    },
                    "subnet": {
                        "type": "string",
                        "description": "CIDR subnet for arp_scan (e.g. '192.168.1.0/24')"
                    },
                    "interface": {
                        "type": "string",
                        "description": "Network interface for arp_scan (e.g. 'eth0', 'enp0s3', 'wlan0'). Auto-detected from the default route if omitted."
                    },
                    "mac": {
                        "type": "string",
                        "description": "MAC address for oui_lookup (e.g. 'AA:BB:CC:DD:EE:FF')"
                    },
                    "fetch_descriptions": {
                        "type": "boolean",
                        "description": "For ssdp_discover: fetch UPnP device description XML from LOCATION URLs (default: false, adds latency)"
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
                // If a preset is given, browse multiple service types at once.
                if let Some(preset) = args["preset"].as_str() {
                    let types = mdns_preset_service_types(preset).map_err(|e| {
                        rustykrab_core::Error::ToolExecution(ToolError::invalid_input(e))
                    })?;

                    let mut all_services = Vec::new();
                    // Split timeout evenly across types, minimum 3s each.
                    let per_type_ms = (timeout_ms / types.len() as u64).max(3_000);
                    for svc_type in &types {
                        let result = mdns_discover(svc_type, per_type_ms).await;
                        if let Some(services) = result["services"].as_array() {
                            all_services.extend(services.clone());
                        }
                    }

                    return Ok(json!({
                        "action": "mdns_discover",
                        "preset": preset,
                        "service_types_queried": types,
                        "result": {
                            "success": true,
                            "services": all_services,
                            "count": all_services.len(),
                        },
                    }));
                }

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

            "arp_scan" => {
                let subnet = args["subnet"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "subnet (CIDR) is required for arp_scan, e.g. '192.168.1.0/24'",
                    ))
                })?;

                // Full CIDR validation: well-formed, local network, sane prefix.
                validate_scan_cidr(subnet).map_err(rustykrab_core::Error::ToolExecution)?;

                let interface = args["interface"].as_str();
                if let Some(iface) = interface {
                    validate_interface_name(iface).map_err(rustykrab_core::Error::ToolExecution)?;
                }

                let result = arp_scan(subnet, interface, timeout_ms).await;
                Ok(json!({
                    "action": "arp_scan",
                    "result": result,
                }))
            }

            "oui_lookup" => {
                let mac_str = args["mac"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "mac is required for oui_lookup (e.g. 'AA:BB:CC:DD:EE:FF')",
                    ))
                })?;

                let result = oui_lookup(mac_str).await;
                Ok(json!({
                    "action": "oui_lookup",
                    "result": result,
                }))
            }

            "ssdp_discover" => {
                let fetch_desc = args["fetch_descriptions"].as_bool().unwrap_or(false);
                let result = ssdp_discover(timeout_ms, fetch_desc).await;
                Ok(json!({
                    "action": "ssdp_discover",
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

            "dhcp_leases" => {
                let host_str = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "host (router IP) is required for dhcp_leases",
                    ))
                })?;
                // Validate router IP is on local network.
                require_local(host_str).map_err(rustykrab_core::Error::ToolExecution)?;

                let user = args["user"].as_str().unwrap_or("root");
                validate_ssh_user(user).map_err(rustykrab_core::Error::ToolExecution)?;

                let lease_file = args["lease_file"].as_str().unwrap_or("/tmp/dnsmasq.leases");
                validate_lease_path(lease_file).map_err(rustykrab_core::Error::ToolExecution)?;

                let result = dhcp_leases(host_str, user, lease_file, timeout_ms).await;
                Ok(json!({
                    "action": "dhcp_leases",
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
