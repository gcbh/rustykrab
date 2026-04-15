use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
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
    8123, 5683, 9123, 55443, 4455,
    8000, // Home Assistant, CoAP, Govee, Kasa, Lutron, IoT HTTP
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
        4455 => "lutron-caseta",
        5353 => "mdns",
        5432 => "postgresql",
        5683 => "coap",
        5900 => "vnc",
        6379 => "redis",
        8000 => "http-alt",
        8080 => "http-alt",
        8123 => "home-assistant",
        8443 => "https-alt",
        8883 => "mqtt-tls",
        9100 => "printer",
        9123 => "govee",
        27017 => "mongodb",
        55443 => "kasa",
        _ => "unknown",
    }
}

/// Network discovery and port-scanning tool.
///
/// Provides four actions:
/// - **ping_sweep**: Probes a subnet to find live hosts (TCP connect on port 80/443).
/// - **port_scan**: Scans a list of ports on a single host.
/// - **scan_subnet**: Combines sweep + port scan for every live host found.
/// - **fingerprint_http**: Connects to an HTTP port and analyzes the response
///   to identify device type, manufacturer, model, and firmware.
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
fn expand_subnet(cidr: &str) -> std::result::Result<Vec<Ipv4Addr>, ToolError> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(ToolError::invalid_input(
            "expected CIDR notation, e.g. 192.168.1.0/24",
        ));
    }
    let base: Ipv4Addr = parts[0]
        .parse()
        .map_err(|e| ToolError::invalid_input(format!("invalid IP: {e}")))?;
    let prefix_len: u32 = parts[1]
        .parse()
        .map_err(|e| ToolError::invalid_input(format!("invalid prefix length: {e}")))?;
    if prefix_len > 32 {
        return Err(ToolError::invalid_input("prefix length must be <= 32"));
    }
    // Limit to /20 (4094 hosts) to avoid accidental DoS.
    if prefix_len < 20 {
        return Err(ToolError::invalid_input(
            "prefix length must be >= 20 (max 4094 hosts) to prevent accidental DoS",
        ));
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
async fn ping_sweep(cidr: &str, timeout_ms: u64) -> std::result::Result<Vec<Value>, ToolError> {
    let hosts = expand_subnet(cidr)?;
    let probe_ports: &[u16] = &[80, 443, 22, 445];

    // Verify all hosts are in local network ranges.
    for h in &hosts {
        if !is_local_network(&IpAddr::V4(*h)) {
            return Err(ToolError::invalid_input(format!(
                "host {h} is not in a private/local range — only local-network scanning is allowed"
            )));
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

// ---------------------------------------------------------------------------
// HTTP fingerprinting
// ---------------------------------------------------------------------------

/// Known device signature patterns matched against HTTP response body.
struct DeviceSignature {
    pattern: &'static str,
    device_type: &'static str,
    manufacturer: &'static str,
}

const DEVICE_SIGNATURES: &[DeviceSignature] = &[
    DeviceSignature {
        pattern: "Shelly",
        device_type: "smart_relay",
        manufacturer: "Allterco Robotics (Shelly)",
    },
    DeviceSignature {
        pattern: "ESPHome",
        device_type: "esp_device",
        manufacturer: "ESPHome",
    },
    DeviceSignature {
        pattern: "Tasmota",
        device_type: "smart_switch",
        manufacturer: "Tasmota",
    },
    DeviceSignature {
        pattern: "Home Assistant",
        device_type: "home_automation_hub",
        manufacturer: "Home Assistant",
    },
    DeviceSignature {
        pattern: "hue personal wireless",
        device_type: "smart_lighting_bridge",
        manufacturer: "Signify (Philips Hue)",
    },
    DeviceSignature {
        pattern: "UniFi",
        device_type: "network_equipment",
        manufacturer: "Ubiquiti",
    },
    DeviceSignature {
        pattern: "Synology",
        device_type: "nas",
        manufacturer: "Synology",
    },
    DeviceSignature {
        pattern: "QNAP",
        device_type: "nas",
        manufacturer: "QNAP",
    },
    DeviceSignature {
        pattern: "Pi-hole",
        device_type: "dns_filter",
        manufacturer: "Pi-hole",
    },
    DeviceSignature {
        pattern: "OctoPrint",
        device_type: "3d_printer_controller",
        manufacturer: "OctoPrint",
    },
    DeviceSignature {
        pattern: "Sonos",
        device_type: "smart_speaker",
        manufacturer: "Sonos",
    },
];

/// Extract the content of an HTML `<title>` tag.
fn extract_html_title(body: &str) -> Option<String> {
    let lower = body.to_lowercase();
    let start = lower.find("<title>")?;
    let after = start + "<title>".len();
    let end = lower[after..].find("</title>")?;
    let title = body[after..after + end].trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Extract a simple XML field value (same as in net_discovery).
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

/// Fingerprint a device by connecting to its HTTP port and analyzing the
/// response headers and body.
///
/// Checks:
/// - **Server header** — often reveals firmware name and version.
/// - **Response body patterns** — known strings for common IoT devices.
/// - **HTML title** — many devices include the model name.
/// - **/description.xml** — UPnP device description with rich metadata.
async fn fingerprint_http(ip: IpAddr, port: u16, timeout_ms: u64) -> Value {
    let scheme = if port == 443 || port == 8443 {
        "https"
    } else {
        "http"
    };
    let base_url = format!("{scheme}://{ip}:{port}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Fetch the root page.
    let root_resp = client.get(&base_url).send().await;

    let mut server_header = String::new();
    let mut title = None;
    let mut detected_device = None;
    let mut detected_manufacturer = None;
    let mut body_snippet = String::new();
    let mut status_code = 0u16;

    if let Ok(resp) = root_resp {
        status_code = resp.status().as_u16();

        // Extract Server header.
        if let Some(srv) = resp.headers().get("server") {
            server_header = srv.to_str().unwrap_or("").to_string();
        }

        // Read body (capped at 64KB to avoid memory issues on constrained hosts).
        if let Ok(body) = resp.text().await {
            let body = if body.len() > 65_536 {
                body[..65_536].to_string()
            } else {
                body
            };

            title = extract_html_title(&body);

            // Check body against known device signatures.
            for sig in DEVICE_SIGNATURES {
                if body.contains(sig.pattern)
                    || server_header.contains(sig.pattern)
                    || title.as_deref().is_some_and(|t| t.contains(sig.pattern))
                {
                    detected_device = Some(sig.device_type);
                    detected_manufacturer = Some(sig.manufacturer);
                    break;
                }
            }

            // Keep a small snippet for context.
            body_snippet = body.chars().take(200).collect();
        }
    }

    // Try /description.xml (UPnP device description).
    let mut upnp_info = json!(null);
    let desc_url = format!("{base_url}/description.xml");
    if let Ok(resp) = client.get(&desc_url).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.text().await {
                let friendly_name = extract_xml_field(&body, "friendlyName");
                let device_type = extract_xml_field(&body, "deviceType");
                let manufacturer = extract_xml_field(&body, "manufacturer");
                let model_name = extract_xml_field(&body, "modelName");
                let model_number = extract_xml_field(&body, "modelNumber");
                let serial = extract_xml_field(&body, "serialNumber");
                let firmware = extract_xml_field(&body, "firmwareVersion")
                    .or_else(|| extract_xml_field(&body, "softwareVersion"));

                if friendly_name.is_some() || manufacturer.is_some() {
                    upnp_info = json!({
                        "friendly_name": friendly_name,
                        "device_type": device_type,
                        "manufacturer": manufacturer,
                        "model_name": model_name,
                        "model_number": model_number,
                        "serial_number": serial,
                        "firmware_version": firmware,
                    });

                    // Use UPnP info as primary identification if available.
                    if detected_manufacturer.is_none() {
                        if let Some(ref mfg) = manufacturer {
                            detected_manufacturer =
                                // Leak into 'static isn't ideal, but we'd need
                                // owned strings to avoid it.  Instead just leave
                                // it in the JSON output — the caller reads the
                                // upnp_description object.
                                None;
                            let _ = mfg; // suppress unused warning
                        }
                    }
                }
            }
        }
    }

    json!({
        "success": status_code > 0,
        "ip": ip.to_string(),
        "port": port,
        "status_code": status_code,
        "server_header": server_header,
        "title": title,
        "detected_device_type": detected_device,
        "detected_manufacturer": detected_manufacturer,
        "upnp_description": upnp_info,
        "body_snippet": body_snippet,
    })
}

#[async_trait]
impl Tool for NetScanTool {
    fn name(&self) -> &str {
        "net_scan"
    }

    fn description(&self) -> &str {
        "Discover devices and open ports on the local network. Port scanning, HTTP fingerprinting \
         for device identification. Restricted to private/link-local IP ranges."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_net: true,
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
                        "enum": ["ping_sweep", "port_scan", "scan_subnet", "fingerprint_http"],
                        "description": "ping_sweep: find live hosts in a subnet. port_scan: scan ports on a single host. scan_subnet: sweep + port scan all live hosts. fingerprint_http: identify a device by its HTTP response (Server header, body patterns, UPnP description.xml)."
                    },
                    "subnet": {
                        "type": "string",
                        "description": "CIDR subnet to scan, e.g. 192.168.1.0/24 (required for ping_sweep and scan_subnet)"
                    },
                    "host": {
                        "type": "string",
                        "description": "IP address to scan (required for port_scan and fingerprint_http)"
                    },
                    "port": {
                        "type": "integer",
                        "description": "HTTP port for fingerprint_http (default: 80)"
                    },
                    "ports": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "List of ports to scan (default: common well-known ports including IoT)"
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
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "subnet is required for ping_sweep",
                    ))
                })?;

                let live = ping_sweep(subnet, timeout_ms)
                    .await
                    .map_err(rustykrab_core::Error::ToolExecution)?;

                Ok(json!({
                    "action": "ping_sweep",
                    "subnet": subnet,
                    "live_hosts": live,
                    "count": live.len(),
                }))
            }
            "port_scan" => {
                let host_str = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "host is required for port_scan",
                    ))
                })?;
                let ip: IpAddr = host_str.parse().map_err(|e| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(format!(
                        "invalid IP address: {e}"
                    )))
                })?;
                if !is_local_network(&ip) {
                    return Err(rustykrab_core::Error::ToolExecution(
                        ToolError::invalid_input(format!(
                            "{ip} is not in a private/local range — only local-network scanning is allowed"
                        )),
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
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "subnet is required for scan_subnet",
                    ))
                })?;

                let live = ping_sweep(subnet, timeout_ms)
                    .await
                    .map_err(rustykrab_core::Error::ToolExecution)?;

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
            "fingerprint_http" => {
                let host_str = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "host is required for fingerprint_http",
                    ))
                })?;
                let ip: IpAddr = host_str.parse().map_err(|e| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(format!(
                        "invalid IP address: {e}"
                    )))
                })?;
                if !is_local_network(&ip) {
                    return Err(rustykrab_core::Error::ToolExecution(
                        ToolError::invalid_input(format!(
                            "{ip} is not in a private/local range — only local-network scanning is allowed"
                        )),
                    ));
                }

                let port = args["port"].as_u64().unwrap_or(80) as u16;
                let fp_timeout = timeout_ms.max(3_000); // fingerprinting needs a bit more time
                let result = fingerprint_http(ip, port, fp_timeout).await;

                Ok(json!({
                    "action": "fingerprint_http",
                    "result": result,
                }))
            }

            _ => Err(rustykrab_core::Error::ToolExecution(
                ToolError::invalid_input(format!("unknown net_scan action: {action}")),
            )),
        }
    }
}
