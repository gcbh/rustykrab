use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Security auditing tool for the local network.
///
/// Supported actions:
/// - **banner_grab**: Connects to a port and reads the service banner
///   to identify software versions.
/// - **ssl_check**: Tests TLS configuration on a host+port, reporting
///   certificate info and protocol version.
/// - **ssh_auth_methods**: Queries an SSH server for supported
///   authentication methods.
/// - **service_info**: Performs banner grab on all open ports of a host
///   to produce a comprehensive service inventory.
///
/// All targets must be in private/link-local IP ranges.
pub struct NetAuditTool;

impl NetAuditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetAuditTool {
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
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Validate that `host` is a local-network IP.
fn require_local(host: &str) -> std::result::Result<IpAddr, String> {
    let ip: IpAddr = host
        .parse()
        .map_err(|e| format!("invalid IP address: {e}"))?;
    if !is_local_network(&ip) {
        return Err(format!(
            "{ip} is not in a private/local range — only local-network auditing is allowed"
        ));
    }
    Ok(ip)
}

/// Connect to a TCP port and read the initial service banner.
async fn banner_grab(ip: IpAddr, port: u16, timeout_ms: u64) -> Option<String> {
    let addr = SocketAddr::new(ip, port);
    let connect = timeout(Duration::from_millis(timeout_ms), TcpStream::connect(addr));
    let mut stream = match connect.await {
        Ok(Ok(s)) => s,
        _ => return None,
    };

    // Some services (HTTP) need a small nudge to send a banner.
    if port == 80 || port == 8080 || port == 8443 {
        let _ = stream
            .write_all(b"HEAD / HTTP/1.0\r\nHost: check\r\n\r\n")
            .await;
    }

    let mut buf = vec![0u8; 2048];
    let read = timeout(Duration::from_millis(timeout_ms), stream.read(&mut buf));
    match read.await {
        Ok(Ok(n)) if n > 0 => {
            let banner = String::from_utf8_lossy(&buf[..n]).to_string();
            Some(banner.trim().to_string())
        }
        _ => None,
    }
}

/// Query SSH authentication methods by connecting and performing a partial
/// handshake (reading the server ident string).
async fn ssh_auth_probe(ip: IpAddr, port: u16, timeout_ms: u64) -> Value {
    let addr = SocketAddr::new(ip, port);
    let connect = timeout(Duration::from_millis(timeout_ms), TcpStream::connect(addr));
    let mut stream = match connect.await {
        Ok(Ok(s)) => s,
        _ => {
            return json!({
                "reachable": false,
                "error": "connection failed or timed out",
            })
        }
    };

    // Read server identification string (e.g. "SSH-2.0-OpenSSH_9.6").
    let mut buf = vec![0u8; 512];
    let read = timeout(Duration::from_millis(timeout_ms), stream.read(&mut buf));
    let server_ident = match read.await {
        Ok(Ok(n)) if n > 0 => String::from_utf8_lossy(&buf[..n]).trim().to_string(),
        _ => String::new(),
    };

    // Try ssh-keyscan to discover host key types.
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());

    let keyscan = timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("ssh-keyscan")
            .args(["-p", &port.to_string(), &ip.to_string()])
            .env("PATH", &path)
            .output(),
    )
    .await;

    let mut key_types = Vec::new();
    if let Ok(Ok(output)) = keyscan {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // Lines look like: "192.168.1.1 ssh-ed25519 AAAA..."
            if let Some(key_type) = line.split_whitespace().nth(1) {
                if key_type.starts_with("ssh-") || key_type.starts_with("ecdsa-") {
                    key_types.push(key_type.to_string());
                }
            }
        }
    }

    json!({
        "reachable": true,
        "server_ident": server_ident,
        "host_key_types": key_types,
    })
}

/// Check TLS/SSL configuration on a host+port.
async fn ssl_check(ip: IpAddr, port: u16, timeout_ms: u64) -> Value {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());

    // Try openssl s_client for certificate details.
    let result = timeout(
        Duration::from_millis(timeout_ms.max(5_000)),
        tokio::process::Command::new("openssl")
            .args([
                "s_client",
                "-connect",
                &format!("{ip}:{port}"),
                "-servername",
                &ip.to_string(),
                "-brief",
            ])
            .stdin(std::process::Stdio::null())
            .env("PATH", &path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Parse out useful fields.
            let combined = format!("{stdout}\n{stderr}");
            let mut protocol = String::new();
            let mut cipher = String::new();
            let mut subject = String::new();
            let mut issuer = String::new();
            let mut not_after = String::new();

            for line in combined.lines() {
                let line = line.trim();
                if line.starts_with("Protocol version:") || line.starts_with("Protocol  :") {
                    protocol = line
                        .split_once(':')
                        .map(|(_, v)| v)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                } else if line.starts_with("Ciphersuite:") || line.starts_with("Cipher    :") {
                    cipher = line
                        .split_once(':')
                        .map(|(_, v)| v)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                } else if line.starts_with("subject=") {
                    subject = line.trim_start_matches("subject=").trim().to_string();
                } else if line.starts_with("issuer=") {
                    issuer = line.trim_start_matches("issuer=").trim().to_string();
                } else if line.starts_with("notAfter=") || line.contains("Not After") {
                    not_after = line
                        .split_once('=')
                        .map(|(_, v)| v)
                        .or_else(|| line.split_once(':').map(|(_, v)| v))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                }
            }

            json!({
                "tls_available": true,
                "protocol": protocol,
                "cipher": cipher,
                "subject": subject,
                "issuer": issuer,
                "not_after": not_after,
            })
        }
        Ok(Err(e)) => json!({
            "tls_available": false,
            "error": format!("openssl not available: {e}"),
        }),
        Err(_) => json!({
            "tls_available": false,
            "error": "TLS check timed out",
        }),
    }
}

#[async_trait]
impl Tool for NetAuditTool {
    fn name(&self) -> &str {
        "net_audit"
    }

    fn description(&self) -> &str {
        "Security auditing for local network services. Grab banners, check TLS/SSL, and probe SSH configurations."
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
                        "enum": ["banner_grab", "ssl_check", "ssh_auth_methods", "service_info"],
                        "description": "banner_grab: read a service banner. ssl_check: test TLS config. ssh_auth_methods: probe SSH auth. service_info: banner grab on multiple ports."
                    },
                    "host": {
                        "type": "string",
                        "description": "Target IP address (must be private/link-local)"
                    },
                    "port": {
                        "type": "integer",
                        "description": "Port to probe (default varies by action)"
                    },
                    "ports": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "List of ports for service_info (default: common ports)"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Per-probe timeout in milliseconds (default: 3000, max: 15000)"
                    }
                },
                "required": ["action", "host"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing action".into()))?;

        let host_str = args["host"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing host".into()))?;

        let ip =
            require_local(host_str).map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

        let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(3_000).min(15_000);

        match action {
            "banner_grab" => {
                let port = args["port"].as_u64().unwrap_or(80) as u16;
                let banner = banner_grab(ip, port, timeout_ms).await;

                Ok(json!({
                    "action": "banner_grab",
                    "host": host_str,
                    "port": port,
                    "banner": banner,
                }))
            }

            "ssl_check" => {
                let port = args["port"].as_u64().unwrap_or(443) as u16;
                let result = ssl_check(ip, port, timeout_ms).await;

                Ok(json!({
                    "action": "ssl_check",
                    "host": host_str,
                    "port": port,
                    "result": result,
                }))
            }

            "ssh_auth_methods" => {
                let port = args["port"].as_u64().unwrap_or(22) as u16;
                let result = ssh_auth_probe(ip, port, timeout_ms).await;

                Ok(json!({
                    "action": "ssh_auth_methods",
                    "host": host_str,
                    "port": port,
                    "result": result,
                }))
            }

            "service_info" => {
                let default_ports: Vec<u16> = vec![
                    21, 22, 23, 25, 53, 80, 110, 143, 443, 445, 993, 995, 3306, 3389, 5432, 5900,
                    8080, 8443,
                ];
                let ports: Vec<u16> = args["ports"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u16))
                            .collect()
                    })
                    .unwrap_or(default_ports);

                let mut services = Vec::new();
                for &port in &ports {
                    if let Some(banner) = banner_grab(ip, port, timeout_ms).await {
                        services.push(json!({
                            "port": port,
                            "banner": banner,
                        }));
                    }
                }

                Ok(json!({
                    "action": "service_info",
                    "host": host_str,
                    "services": services,
                    "ports_scanned": ports.len(),
                }))
            }

            _ => Err(rustykrab_core::Error::ToolExecution(
                format!("unknown net_audit action: {action}").into(),
            )),
        }
    }
}
