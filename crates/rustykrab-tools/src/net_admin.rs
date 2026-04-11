use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use tokio::time::timeout;

/// Remote administration tool for machines the user owns.
///
/// Supported actions:
/// - **ssh_exec**: Run a command on a remote host via SSH (requires `ssh` binary).
/// - **wake_on_lan**: Send a WoL magic packet to wake a machine by MAC address.
/// - **ssh_copy_id**: Copy the local public key to a remote host for key-based auth.
///
/// All targets must be in private/link-local IP ranges.
pub struct NetAdminTool;

impl NetAdminTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetAdminTool {
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
            "{ip} is not in a private/local range — only local-network targets are allowed"
        ));
    }
    Ok(ip)
}

/// Build a Wake-on-LAN magic packet for the given MAC address.
fn build_wol_packet(mac: &str) -> std::result::Result<Vec<u8>, String> {
    let mac_bytes: Vec<u8> = mac
        .split([':', '-'])
        .map(|octet| u8::from_str_radix(octet, 16).map_err(|e| format!("bad MAC octet: {e}")))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if mac_bytes.len() != 6 {
        return Err("MAC address must have exactly 6 octets".into());
    }
    // Magic packet: 6 x 0xFF followed by the MAC repeated 16 times.
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac_bytes);
    }
    Ok(packet)
}

/// Execute an SSH command on a remote host.
async fn ssh_exec(
    host: &str,
    user: &str,
    command: &str,
    port: u16,
    timeout_secs: u64,
) -> Result<Value> {
    let path = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string());

    let future = tokio::process::Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            &format!("ConnectTimeout={}", timeout_secs.min(30)),
            "-o",
            "BatchMode=yes",
            "-p",
            &port.to_string(),
            &format!("{user}@{host}"),
            command,
        ])
        .env("PATH", &path)
        .output();

    let output = timeout(Duration::from_secs(timeout_secs), future)
        .await
        .map_err(|_| {
            rustykrab_core::Error::ToolExecution(
                format!("SSH command timed out after {timeout_secs}s").into(),
            )
        })?
        .map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("failed to execute ssh: {e}").into())
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(json!({
        "action": "ssh_exec",
        "host": host,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

#[async_trait]
impl Tool for NetAdminTool {
    fn name(&self) -> &str {
        "net_admin"
    }

    fn description(&self) -> &str {
        "Remote administration for machines you own on the local network. Supports SSH command execution and Wake-on-LAN."
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
                        "enum": ["ssh_exec", "wake_on_lan", "ssh_copy_id"],
                        "description": "ssh_exec: run a command via SSH. wake_on_lan: send a WoL magic packet. ssh_copy_id: install your public key on a remote host."
                    },
                    "host": {
                        "type": "string",
                        "description": "Target IP address (must be a private/link-local address)"
                    },
                    "user": {
                        "type": "string",
                        "description": "SSH username (default: current user)"
                    },
                    "command": {
                        "type": "string",
                        "description": "Command to execute on the remote host (required for ssh_exec)"
                    },
                    "port": {
                        "type": "integer",
                        "description": "SSH port (default: 22)"
                    },
                    "mac_address": {
                        "type": "string",
                        "description": "MAC address for Wake-on-LAN (e.g. AA:BB:CC:DD:EE:FF)"
                    },
                    "broadcast_ip": {
                        "type": "string",
                        "description": "Broadcast IP for WoL (default: 255.255.255.255)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30, max: 120)"
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

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(120);

        match action {
            "ssh_exec" => {
                let host = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("host is required for ssh_exec".into())
                })?;
                require_local(host).map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

                let user = args["user"].as_str().unwrap_or("root");
                let command = args["command"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("command is required for ssh_exec".into())
                })?;
                let port = args["port"].as_u64().unwrap_or(22) as u16;

                ssh_exec(host, user, command, port, timeout_secs).await
            }

            "ssh_copy_id" => {
                let host = args["host"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("host is required for ssh_copy_id".into())
                })?;
                require_local(host).map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

                let user = args["user"].as_str().unwrap_or("root");
                let port = args["port"].as_u64().unwrap_or(22) as u16;

                let path = std::env::var("PATH")
                    .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());

                let output = timeout(
                    Duration::from_secs(timeout_secs),
                    tokio::process::Command::new("ssh-copy-id")
                        .args(["-p", &port.to_string(), &format!("{user}@{host}")])
                        .env("PATH", &path)
                        .output(),
                )
                .await
                .map_err(|_| rustykrab_core::Error::ToolExecution("ssh-copy-id timed out".into()))?
                .map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to run ssh-copy-id: {e}").into(),
                    )
                })?;

                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                Ok(json!({
                    "action": "ssh_copy_id",
                    "host": host,
                    "success": output.status.success(),
                    "stdout": stdout,
                    "stderr": stderr,
                }))
            }

            "wake_on_lan" => {
                let mac = args["mac_address"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "mac_address is required for wake_on_lan".into(),
                    )
                })?;
                let broadcast_str = args["broadcast_ip"].as_str().unwrap_or("255.255.255.255");
                let broadcast_ip: Ipv4Addr = broadcast_str.parse().map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("invalid broadcast IP: {e}").into(),
                    )
                })?;

                let packet = build_wol_packet(mac)
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

                let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(
                            format!("failed to bind UDP socket: {e}").into(),
                        )
                    })?;
                socket.set_broadcast(true).map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to enable broadcast: {e}").into(),
                    )
                })?;

                let dest = std::net::SocketAddr::new(
                    IpAddr::V4(broadcast_ip),
                    9, // WoL standard port
                );
                socket.send_to(&packet, dest).await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to send WoL packet: {e}").into(),
                    )
                })?;

                Ok(json!({
                    "action": "wake_on_lan",
                    "mac_address": mac,
                    "broadcast_ip": broadcast_str,
                    "sent": true,
                }))
            }

            _ => Err(rustykrab_core::Error::ToolExecution(
                format!("unknown net_admin action: {action}").into(),
            )),
        }
    }
}
