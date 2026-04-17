use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::time::Duration;

/// Device control tool for smart-home hubs on the local network.
///
/// Provides a protocol-agnostic interface to interact with devices through
/// their hub APIs.  Currently supports Home Assistant as the primary hub,
/// with actions for listing devices, reading entity state, and calling
/// services.
///
/// All hub URLs must point to local-network addresses (RFC-1918 / link-local).
/// Authentication is via long-lived access tokens passed as parameters or
/// environment variables.
pub struct NetDeviceControlTool;

impl NetDeviceControlTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetDeviceControlTool {
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

/// Validate that a Home Assistant URL is safe (local network only).
fn validate_ha_url(url: &str) -> std::result::Result<(), ToolError> {
    if url.is_empty() {
        return Err(ToolError::invalid_input(
            "Home Assistant URL is required (e.g. 'http://192.168.1.100:8123')",
        ));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ToolError::invalid_input(
            "Home Assistant URL must start with http:// or https://",
        ));
    }
    let host_part = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if let Ok(ip) = host_part.parse::<IpAddr>() {
        if !is_local_network(&ip) {
            return Err(ToolError::invalid_input(format!(
                "{ip} is not a local network address — only local HA instances are supported"
            )));
        }
    }
    Ok(())
}

/// Resolve HA URL from args or environment.
fn resolve_ha_url(args: &Value) -> std::result::Result<String, ToolError> {
    let url = args["ha_url"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| std::env::var("HOME_ASSISTANT_URL").ok())
        .unwrap_or_default();
    validate_ha_url(&url)?;
    Ok(url)
}

/// Resolve HA token from args or environment.
fn resolve_ha_token(args: &Value) -> std::result::Result<String, ToolError> {
    args["ha_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| std::env::var("HOME_ASSISTANT_TOKEN").ok())
        .ok_or_else(|| {
            ToolError::invalid_input("ha_token is required (or set HOME_ASSISTANT_TOKEN env var)")
        })
}

/// Build a reqwest client with the HA bearer token.
fn ha_client(token: &str, timeout_ms: u64) -> reqwest::Client {
    use reqwest::header;
    let mut headers = header::HeaderMap::new();
    if let Ok(val) = header::HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(header::AUTHORIZATION, val);
    }
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .default_headers(headers)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ---------------------------------------------------------------------------
// Action implementations
// ---------------------------------------------------------------------------

/// List all devices registered in Home Assistant.
async fn ha_list_devices(ha_url: &str, token: &str, timeout_ms: u64) -> Value {
    let client = ha_client(token, timeout_ms);
    let url = format!("{ha_url}/api/config/device_registry/list");

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(Value::Array(devices)) => {
                let parsed: Vec<Value> = devices
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d["id"].as_str().unwrap_or(""),
                            "name": d["name"].as_str()
                                .or_else(|| d["name_by_user"].as_str())
                                .unwrap_or(""),
                            "manufacturer": d["manufacturer"].as_str().unwrap_or(""),
                            "model": d["model"].as_str().unwrap_or(""),
                            "area_id": d["area_id"].as_str().unwrap_or(""),
                            "via_device_id": d["via_device_id"].as_str().unwrap_or(""),
                            "disabled_by": d["disabled_by"],
                            "entry_type": d["entry_type"],
                            "hw_version": d["hw_version"].as_str().unwrap_or(""),
                            "sw_version": d["sw_version"].as_str().unwrap_or(""),
                            "connections": d["connections"],
                            "identifiers": d["identifiers"],
                        })
                    })
                    .collect();
                json!({
                    "success": true,
                    "devices": parsed,
                    "count": parsed.len(),
                })
            }
            Ok(other) => json!({
                "success": false,
                "error": format!("unexpected response type: {}", other.to_string().chars().take(200).collect::<String>()),
            }),
            Err(e) => json!({
                "success": false,
                "error": format!("failed to parse HA response: {e}"),
            }),
        },
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            json!({
                "success": false,
                "error": format!("HA API returned {status}: {}", body.chars().take(300).collect::<String>()),
            })
        }
        Err(e) => json!({
            "success": false,
            "error": format!("failed to connect to Home Assistant: {e}"),
        }),
    }
}

/// Get the state of a single Home Assistant entity.
async fn ha_entity_state(ha_url: &str, token: &str, entity_id: &str, timeout_ms: u64) -> Value {
    let client = ha_client(token, timeout_ms);
    let url = format!("{ha_url}/api/states/{entity_id}");

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(state) => {
                json!({
                    "success": true,
                    "entity_id": entity_id,
                    "state": state["state"].as_str().unwrap_or(""),
                    "attributes": state["attributes"],
                    "last_changed": state["last_changed"].as_str().unwrap_or(""),
                    "last_updated": state["last_updated"].as_str().unwrap_or(""),
                })
            }
            Err(e) => json!({
                "success": false,
                "error": format!("failed to parse entity state: {e}"),
            }),
        },
        Ok(resp) if resp.status().as_u16() == 404 => json!({
            "success": false,
            "error": format!("entity '{entity_id}' not found"),
        }),
        Ok(resp) => json!({
            "success": false,
            "error": format!("HA API returned {}", resp.status().as_u16()),
        }),
        Err(e) => json!({
            "success": false,
            "error": format!("failed to connect to Home Assistant: {e}"),
        }),
    }
}

/// Call a Home Assistant service.
async fn ha_call_service(
    ha_url: &str,
    token: &str,
    domain: &str,
    service: &str,
    data: &Value,
    timeout_ms: u64,
) -> Value {
    let client = ha_client(token, timeout_ms);
    let url = format!("{ha_url}/api/services/{domain}/{service}");

    match client.post(&url).json(data).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.json::<Value>().await.unwrap_or(Value::Null);
            json!({
                "success": true,
                "domain": domain,
                "service": service,
                "result": body,
            })
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            json!({
                "success": false,
                "error": format!("HA service call returned {status}: {}", body.chars().take(300).collect::<String>()),
            })
        }
        Err(e) => json!({
            "success": false,
            "error": format!("failed to call HA service: {e}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for NetDeviceControlTool {
    fn name(&self) -> &str {
        "net_device_control"
    }

    fn description(&self) -> &str {
        "Control smart-home devices through hub APIs. List Home Assistant devices, read entity \
         states, and call services (e.g. turn on lights, set thermostat). Restricted to \
         local-network hubs."
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
                        "enum": ["ha_devices", "ha_entity_state", "ha_call_service"],
                        "description": "ha_devices: list all devices registered in Home Assistant. ha_entity_state: get the current state of an entity. ha_call_service: call a Home Assistant service (e.g. light.turn_on)."
                    },
                    "ha_url": {
                        "type": "string",
                        "description": "Home Assistant URL (default: env HOME_ASSISTANT_URL, e.g. 'http://192.168.1.100:8123')"
                    },
                    "ha_token": {
                        "type": "string",
                        "description": "Home Assistant Long-Lived Access Token (default: env HOME_ASSISTANT_TOKEN)"
                    },
                    "entity_id": {
                        "type": "string",
                        "description": "Entity ID for ha_entity_state (e.g. 'light.living_room', 'sensor.temperature')"
                    },
                    "domain": {
                        "type": "string",
                        "description": "Service domain for ha_call_service (e.g. 'light', 'switch', 'climate')"
                    },
                    "service": {
                        "type": "string",
                        "description": "Service name for ha_call_service (e.g. 'turn_on', 'turn_off', 'set_temperature')"
                    },
                    "service_data": {
                        "type": "object",
                        "description": "Data payload for ha_call_service (e.g. {\"entity_id\": \"light.living_room\", \"brightness\": 128})"
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
            "ha_devices" => {
                let ha_url = resolve_ha_url(&args).map_err(rustykrab_core::Error::ToolExecution)?;
                let token =
                    resolve_ha_token(&args).map_err(rustykrab_core::Error::ToolExecution)?;

                let result = ha_list_devices(&ha_url, &token, timeout_ms).await;
                Ok(json!({
                    "action": "ha_devices",
                    "result": result,
                }))
            }

            "ha_entity_state" => {
                let ha_url = resolve_ha_url(&args).map_err(rustykrab_core::Error::ToolExecution)?;
                let token =
                    resolve_ha_token(&args).map_err(rustykrab_core::Error::ToolExecution)?;

                let entity_id = args["entity_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "entity_id is required for ha_entity_state",
                    ))
                })?;

                let result = ha_entity_state(&ha_url, &token, entity_id, timeout_ms).await;
                Ok(json!({
                    "action": "ha_entity_state",
                    "result": result,
                }))
            }

            "ha_call_service" => {
                let ha_url = resolve_ha_url(&args).map_err(rustykrab_core::Error::ToolExecution)?;
                let token =
                    resolve_ha_token(&args).map_err(rustykrab_core::Error::ToolExecution)?;

                let domain = args["domain"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "domain is required for ha_call_service (e.g. 'light', 'switch')",
                    ))
                })?;
                let service = args["service"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(ToolError::invalid_input(
                        "service is required for ha_call_service (e.g. 'turn_on', 'turn_off')",
                    ))
                })?;
                let data = args
                    .get("service_data")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                let result =
                    ha_call_service(&ha_url, &token, domain, service, &data, timeout_ms).await;
                Ok(json!({
                    "action": "ha_call_service",
                    "result": result,
                }))
            }

            _ => Err(rustykrab_core::Error::ToolExecution(
                ToolError::invalid_input(format!("unknown net_device_control action: {action}")),
            )),
        }
    }
}
