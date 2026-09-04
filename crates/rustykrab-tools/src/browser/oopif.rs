//! Narrow raw-CDP bridge for out-of-process iframe (OOPIF) targets.
//!
//! `chromiumoxide` is still the primary browser driver. Its target poller,
//! including current upstream, ignores target type `iframe`, so commands cannot
//! be routed to the execution context that owns a site-isolated cross-origin
//! frame. This module opens a short-lived second DevTools connection, attaches
//! only to iframe targets belonging to the selected page, and closes it after
//! one bounded snapshot or action.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use rustykrab_core::{Error, Result, ToolError};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use super::config::SsrfPolicy;
use super::policy;
use super::snapshot::{ElementRef, RawElement, SHADOW_SEP};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const DETACH_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_OOPIF_TARGETS: usize = 100;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct RawCdp {
    socket: Socket,
    next_id: u64,
}

impl RawCdp {
    async fn connect(websocket_url: &str) -> Result<Self> {
        let (socket, _) = tokio::time::timeout(COMMAND_TIMEOUT, connect_async(websocket_url))
            .await
            .map_err(|_| Error::ToolExecution("OOPIF CDP connection timed out".into()))?
            .map_err(|error| {
                Error::ToolExecution(format!("OOPIF CDP connection failed: {error}").into())
            })?;
        Ok(Self { socket, next_id: 1 })
    }

    async fn command(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id.to_string());
        }
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            self.socket.send(Message::Text(request.to_string())),
        )
        .await
        .map_err(|_| Error::ToolExecution(format!("OOPIF CDP send timed out for {method}").into()))?
        .map_err(|error| {
            Error::ToolExecution(format!("OOPIF CDP send failed for {method}: {error}").into())
        })?;

        let deadline = tokio::time::Instant::now() + COMMAND_TIMEOUT;
        loop {
            let message = tokio::time::timeout_at(deadline, self.socket.next())
                .await
                .map_err(|_| {
                    Error::ToolExecution(format!("OOPIF CDP command {method} timed out").into())
                })?
                .ok_or_else(|| {
                    Error::ToolExecution(
                        format!("OOPIF CDP connection closed during {method}").into(),
                    )
                })?
                .map_err(|error| {
                    Error::ToolExecution(
                        format!("OOPIF CDP receive failed during {method}: {error}").into(),
                    )
                })?;
            let text = match message {
                Message::Text(text) => text,
                Message::Binary(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
                Message::Ping(payload) => {
                    tokio::time::timeout_at(deadline, self.socket.send(Message::Pong(payload)))
                        .await
                        .map_err(|_| Error::ToolExecution("OOPIF CDP pong timed out".into()))?
                        .map_err(|error| {
                            Error::ToolExecution(format!("OOPIF CDP pong failed: {error}").into())
                        })?;
                    continue;
                }
                Message::Close(_) => {
                    return Err(Error::ToolExecution(
                        format!("OOPIF CDP connection closed during {method}").into(),
                    ));
                }
                _ => continue,
            };
            let response: Value = match serde_json::from_str(&text) {
                Ok(response) => response,
                Err(_) => continue,
            };
            if response["id"].as_u64() != Some(id) {
                continue;
            }
            if !response["error"].is_null() {
                return Err(Error::ToolExecution(
                    format!("OOPIF CDP {method} failed: {}", response["error"]).into(),
                ));
            }
            return Ok(response["result"].clone());
        }
    }

    async fn attach(&mut self, target_id: &str) -> Result<String> {
        let result = self
            .command(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
                None,
            )
            .await?;
        result["sessionId"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| Error::ToolExecution("OOPIF attach returned no sessionId".into()))
    }

    async fn detach(&mut self, session_id: &str) {
        let _ = tokio::time::timeout(
            DETACH_TIMEOUT,
            self.command(
                "Target.detachFromTarget",
                json!({"sessionId": session_id}),
                None,
            ),
        )
        .await;
    }
}

#[derive(Debug)]
pub(crate) struct CapturedFrame {
    pub target_id: String,
    pub frame_id: String,
    pub frame_url: String,
    pub elements: Vec<RawElement>,
}

#[derive(Debug, Default)]
pub(crate) struct Capture {
    pub frames_seen: usize,
    pub frames_included: usize,
    pub frames_skipped: Vec<String>,
    pub frames: Vec<CapturedFrame>,
}

fn associated_iframe_targets(targets: &[Value], page_frame_ids: &HashSet<String>) -> Vec<Value> {
    let mut known = page_frame_ids.clone();
    let mut selected = Vec::new();
    loop {
        let mut changed = false;
        for target in targets {
            if target["type"] != "iframe" {
                continue;
            }
            let Some(target_id) = target["targetId"].as_str() else {
                continue;
            };
            if selected
                .iter()
                .any(|existing: &Value| existing["targetId"] == target["targetId"])
            {
                continue;
            }
            let parent_known = target["parentFrameId"]
                .as_str()
                .is_some_and(|parent| known.contains(parent));
            if known.contains(target_id) || parent_known {
                known.insert(target_id.to_string());
                selected.push(target.clone());
                changed = true;
            }
        }
        if !changed || selected.len() >= MAX_OOPIF_TARGETS {
            break;
        }
    }
    selected.sort_by(|a, b| a["targetId"].as_str().cmp(&b["targetId"].as_str()));
    selected.truncate(MAX_OOPIF_TARGETS);
    selected
}

async fn frame_identity(
    client: &mut RawCdp,
    session_id: &str,
    fallback_target: &Value,
) -> Result<(String, String)> {
    let tree = client
        .command("Page.getFrameTree", json!({}), Some(session_id))
        .await?;
    let frame = &tree["frameTree"]["frame"];
    let frame_id = frame["id"]
        .as_str()
        .or_else(|| fallback_target["targetId"].as_str())
        .unwrap_or_default()
        .to_string();
    let url = frame["url"]
        .as_str()
        .filter(|url| !url.is_empty())
        .or_else(|| fallback_target["url"].as_str())
        .unwrap_or_default()
        .to_string();
    Ok((frame_id, url))
}

pub(crate) async fn capture(
    websocket_url: &str,
    page_frame_ids: &HashSet<String>,
    expression: &str,
    navigation_policy: &SsrfPolicy,
) -> Capture {
    let mut capture = Capture::default();
    let mut client = match RawCdp::connect(websocket_url).await {
        Ok(client) => client,
        Err(error) => {
            capture
                .frames_skipped
                .push(format!("OOPIF bridge unavailable: {error}"));
            return capture;
        }
    };
    let targets = match client.command("Target.getTargets", json!({}), None).await {
        Ok(result) => result["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        Err(error) => {
            capture
                .frames_skipped
                .push(format!("OOPIF discovery failed: {error}"));
            return capture;
        }
    };
    let targets = associated_iframe_targets(&targets, page_frame_ids);
    capture.frames_seen = targets.len();

    for target in targets {
        let target_id = target["targetId"].as_str().unwrap_or_default().to_string();
        let session_id = match client.attach(&target_id).await {
            Ok(session_id) => session_id,
            Err(error) => {
                capture
                    .frames_skipped
                    .push(format!("{target_id}: attach failed: {error}"));
                continue;
            }
        };
        let _ = client
            .command(
                "Runtime.runIfWaitingForDebugger",
                json!({}),
                Some(&session_id),
            )
            .await;
        let _ = client
            .command("Runtime.enable", json!({}), Some(&session_id))
            .await;
        let _ = client
            .command("Page.enable", json!({}), Some(&session_id))
            .await;

        let (frame_id, frame_url) = match frame_identity(&mut client, &session_id, &target).await {
            Ok(identity) => identity,
            Err(error) => {
                capture
                    .frames_skipped
                    .push(format!("{target_id}: frame identity failed: {error}"));
                client.detach(&session_id).await;
                continue;
            }
        };
        if let Err(reason) = policy::validate_observed(&frame_url, navigation_policy).await {
            let _ = client
                .command(
                    "Page.navigate",
                    json!({"url": "about:blank"}),
                    Some(&session_id),
                )
                .await;
            capture.frames_skipped.push(format!(
                "{frame_id}: navigation policy blocked {frame_url}: {reason}"
            ));
            client.detach(&session_id).await;
            continue;
        }

        match client
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
                Some(&session_id),
            )
            .await
        {
            Ok(result) => {
                let raw = result["result"]["value"].as_str().unwrap_or("[]");
                match serde_json::from_str::<Vec<RawElement>>(raw) {
                    Ok(elements) => {
                        capture.frames_included += 1;
                        capture.frames.push(CapturedFrame {
                            target_id,
                            frame_id,
                            frame_url,
                            elements,
                        });
                    }
                    Err(error) => capture
                        .frames_skipped
                        .push(format!("{frame_id}: invalid snapshot: {error}")),
                }
            }
            Err(error) => capture
                .frames_skipped
                .push(format!("{frame_id}: snapshot failed: {error}")),
        }
        client.detach(&session_id).await;
    }
    capture
}

fn element_expression(selector: &str, body: &str) -> String {
    let selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    let separator = serde_json::to_string(SHADOW_SEP).expect("static separator serializes");
    format!(
        r#"(function() {{
            var parts = {selector}.split({separator});
            var root = document;
            var el = null;
            for (var i = 0; i < parts.length; i++) {{
                el = root.querySelector(parts[i]);
                if (!el) return {{ok:false, error:'element_not_found'}};
                if (i < parts.length - 1) {{
                    root = el.shadowRoot;
                    if (!root) return {{ok:false, error:'shadow_root_not_found'}};
                }}
            }}
            {body}
        }})()"#
    )
}

async fn evaluate(client: &mut RawCdp, session_id: &str, expression: String) -> Result<Value> {
    let result = client
        .command(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            Some(session_id),
        )
        .await?;
    if !result["exceptionDetails"].is_null() {
        return Err(Error::ToolExecution(
            format!("OOPIF JavaScript failed: {}", result["exceptionDetails"]).into(),
        ));
    }
    Ok(result["result"]["value"].clone())
}

async fn element_geometry(client: &mut RawCdp, session_id: &str, selector: &str) -> Result<Value> {
    let value = evaluate(
        client,
        session_id,
        element_expression(
            selector,
            "var r=el.getBoundingClientRect(); return {ok:true, tag:el.tagName.toLowerCase(), type:(el.type||''), x:r.x+r.width/2, y:r.y+r.height/2, width:r.width, height:r.height};",
        ),
    )
    .await?;
    if value["ok"] != true {
        return Err(Error::ToolExecution(ToolError::not_found(format!(
            "OOPIF element not found: '{selector}'"
        ))));
    }
    Ok(value)
}

async fn mouse(
    client: &mut RawCdp,
    session_id: &str,
    event_type: &str,
    x: f64,
    y: f64,
    button: Option<&str>,
) -> Result<()> {
    let mut params = json!({"type": event_type, "x": x, "y": y});
    if let Some(button) = button {
        params["button"] = Value::String(button.to_string());
        params["clickCount"] = json!(1);
    }
    client
        .command("Input.dispatchMouseEvent", params, Some(session_id))
        .await?;
    Ok(())
}

async fn press_key(client: &mut RawCdp, session_id: &str, key: &str) -> Result<()> {
    let normalized = match key.trim().to_ascii_lowercase().as_str() {
        "return" => "Enter",
        "esc" => "Escape",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "home" => "Home",
        "end" => "End",
        "space" => " ",
        _ => key,
    };
    if normalized.chars().count() == 1 && !normalized.is_ascii() {
        client
            .command(
                "Input.insertText",
                json!({"text": normalized}),
                Some(session_id),
            )
            .await?;
        return Ok(());
    }
    let (code, virtual_key) = match normalized {
        "Enter" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" => ("Delete", 46),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", 39),
        "PageUp" => ("PageUp", 33),
        "PageDown" => ("PageDown", 34),
        "End" => ("End", 35),
        "Home" => ("Home", 36),
        " " => ("Space", 32),
        _ if normalized.chars().count() == 1 => ("", normalized.as_bytes()[0] as i64),
        _ => {
            return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                "unsupported OOPIF key '{key}'"
            ))));
        }
    };
    let mut down = json!({
        "type": if normalized.chars().count() == 1 { "keyDown" } else { "rawKeyDown" },
        "key": normalized,
        "windowsVirtualKeyCode": virtual_key,
    });
    if !code.is_empty() {
        down["code"] = Value::String(code.to_string());
    }
    if normalized.chars().count() == 1 {
        down["text"] = Value::String(normalized.to_string());
    }
    client
        .command("Input.dispatchKeyEvent", down, Some(session_id))
        .await?;
    client
        .command(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": normalized,
                "code": code,
                "windowsVirtualKeyCode": virtual_key,
            }),
            Some(session_id),
        )
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn perform_action(
    client: &mut RawCdp,
    session_id: &str,
    action: &str,
    element: &ElementRef,
    target: Option<&ElementRef>,
    args: &Value,
) -> Result<Value> {
    match action {
        "click" => {
            let geometry = element_geometry(client, session_id, &element.selector).await?;
            if geometry["tag"] == "input" && geometry["type"] == "file" {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "file inputs require actAction='upload'",
                )));
            }
            if geometry["tag"] == "select" {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "select elements require actAction='options' or 'select'",
                )));
            }
            let x = geometry["x"].as_f64().unwrap_or_default();
            let y = geometry["y"].as_f64().unwrap_or_default();
            mouse(client, session_id, "mouseMoved", x, y, None).await?;
            mouse(client, session_id, "mousePressed", x, y, Some("left")).await?;
            mouse(client, session_id, "mouseReleased", x, y, Some("left")).await?;
            Ok(json!({"status":"clicked", "method":"oopif_cdp_mouse"}))
        }
        "hover" => {
            let geometry = element_geometry(client, session_id, &element.selector).await?;
            mouse(
                client,
                session_id,
                "mouseMoved",
                geometry["x"].as_f64().unwrap_or_default(),
                geometry["y"].as_f64().unwrap_or_default(),
                None,
            )
            .await?;
            Ok(json!({"status":"hovered", "method":"oopif_cdp_mouse"}))
        }
        "type" | "fill" => {
            let text = args["text"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input("type/fill requires 'text'"))
            })?;
            let clear = args["clear"].as_bool().unwrap_or(true);
            let body = if clear {
                "el.focus(); var p=Object.getPrototypeOf(el); var d=Object.getOwnPropertyDescriptor(p,'value'); if(d&&d.set)d.set.call(el,'');else el.value=''; el.dispatchEvent(new InputEvent('input',{bubbles:true,inputType:'deleteContentBackward'})); return {ok:true};"
            } else {
                "el.focus(); return {ok:true};"
            };
            let focused = evaluate(
                client,
                session_id,
                element_expression(&element.selector, body),
            )
            .await?;
            if focused["ok"] != true {
                return Err(Error::ToolExecution(ToolError::not_found(
                    "OOPIF input element is stale",
                )));
            }
            client
                .command("Input.insertText", json!({"text": text}), Some(session_id))
                .await?;
            Ok(
                json!({"status":"typed", "length":text.len(), "cleared":clear, "method":"oopif_cdp_text"}),
            )
        }
        "press" => {
            let key = args["key"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input("press requires 'key'"))
            })?;
            let focused = evaluate(
                client,
                session_id,
                element_expression(&element.selector, "el.focus(); return {ok:true};"),
            )
            .await?;
            if focused["ok"] != true {
                return Err(Error::ToolExecution(ToolError::not_found(
                    "OOPIF element is stale",
                )));
            }
            press_key(client, session_id, key).await?;
            Ok(json!({"status":"pressed", "key":key, "method":"oopif_cdp_keyboard"}))
        }
        "select" => {
            let value = serde_json::to_string(args["value"].as_str().unwrap_or_default())
                .expect("string serializes");
            let result = evaluate(
                client,
                session_id,
                element_expression(
                    &element.selector,
                    &format!("if(el.tagName!=='SELECT')return {{ok:false,error:'not_select'}}; el.value={value}; el.dispatchEvent(new Event('input',{{bubbles:true}})); el.dispatchEvent(new Event('change',{{bubbles:true}})); return {{ok:true,value:el.value}};"),
                ),
            )
            .await?;
            if result["ok"] != true {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "OOPIF ref is not a live select element",
                )));
            }
            Ok(json!({"status":"selected", "value":result["value"], "method":"oopif_dom"}))
        }
        "options" => {
            let result = evaluate(
                client,
                session_id,
                element_expression(
                    &element.selector,
                    "if(el.tagName!=='SELECT')return {ok:false,error:'not_select'}; return {ok:true,options:Array.from(el.options).map(function(o){return {value:o.value,label:o.text,labelled:o.label,selected:o.selected,disabled:o.disabled};})};",
                ),
            )
            .await?;
            if result["ok"] != true {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "OOPIF ref is not a live select element",
                )));
            }
            Ok(json!({"status":"inspected", "options":result["options"], "method":"oopif_dom"}))
        }
        "upload" => {
            let paths = args["paths"].as_array().cloned().unwrap_or_default();
            if paths.is_empty() {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "upload requires non-empty 'paths'",
                )));
            }
            let geometry = element_geometry(client, session_id, &element.selector).await?;
            if geometry["tag"] != "input" || geometry["type"] != "file" {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "OOPIF ref is not a live file input",
                )));
            }
            let expression = element_expression(&element.selector, "return el;");
            let object = client
                .command(
                    "Runtime.evaluate",
                    json!({"expression":expression,"returnByValue":false}),
                    Some(session_id),
                )
                .await?;
            let object_id = object["result"]["objectId"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::not_found("OOPIF file input is stale"))
            })?;
            client
                .command(
                    "DOM.setFileInputFiles",
                    // `objectId` is scoped to this attached OOPIF session. A
                    // frontend `nodeId` obtained via DOM.requestNode is not
                    // stable here unless that session owns an enabled DOM
                    // document, and Chrome can reject it as an unknown node.
                    json!({"files": paths, "objectId": object_id}),
                    Some(session_id),
                )
                .await?;
            Ok(
                json!({"status":"uploaded", "file_count":paths.len(), "method":"oopif_cdp_file_input"}),
            )
        }
        "wait" => {
            let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000).min(30_000);
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let found = evaluate(
                    client,
                    session_id,
                    element_expression(&element.selector, "return {ok:true};"),
                )
                .await
                .is_ok_and(|value| value["ok"] == true);
                if found {
                    return Ok(json!({"status":"found", "method":"oopif_dom"}));
                }
                if Instant::now() >= deadline {
                    return Ok(
                        json!({"status":"timeout", "outcome":"not_applied", "retry_safe":true}),
                    );
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        "drag" => {
            let target = target.ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input("drag requires targetRef"))
            })?;
            if target.target_id.as_deref() != element.target_id.as_deref() {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "cross-target iframe drag is not supported; source and target must share one OOPIF",
                )));
            }
            let source = element_geometry(client, session_id, &element.selector).await?;
            let destination = element_geometry(client, session_id, &target.selector).await?;
            let sx = source["x"].as_f64().unwrap_or_default();
            let sy = source["y"].as_f64().unwrap_or_default();
            let dx = destination["x"].as_f64().unwrap_or_default();
            let dy = destination["y"].as_f64().unwrap_or_default();
            mouse(client, session_id, "mouseMoved", sx, sy, None).await?;
            mouse(client, session_id, "mousePressed", sx, sy, Some("left")).await?;
            for step in 1..=5 {
                let ratio = f64::from(step) / 5.0;
                mouse(
                    client,
                    session_id,
                    "mouseMoved",
                    sx + (dx - sx) * ratio,
                    sy + (dy - sy) * ratio,
                    Some("left"),
                )
                .await?;
            }
            mouse(client, session_id, "mouseReleased", dx, dy, Some("left")).await?;
            Ok(json!({"status":"dragged", "method":"oopif_cdp_mouse"}))
        }
        other => Err(Error::ToolExecution(ToolError::invalid_input(format!(
            "unsupported OOPIF action '{other}'"
        )))),
    }
}

pub(crate) async fn execute_action(
    websocket_url: &str,
    action: &str,
    element: &ElementRef,
    target: Option<&ElementRef>,
    args: &Value,
    navigation_policy: &SsrfPolicy,
) -> Result<Value> {
    let target_id = element.target_id.as_deref().ok_or_else(|| {
        Error::ToolExecution("OOPIF action received a ref with no target ID".into())
    })?;
    let mut client = RawCdp::connect(websocket_url).await?;
    let targets = client.command("Target.getTargets", json!({}), None).await?;
    let target_info = targets["targetInfos"]
        .as_array()
        .and_then(|targets| {
            targets
                .iter()
                .find(|target| target["targetId"] == target_id)
        })
        .cloned()
        .ok_or_else(|| Error::ToolExecution(ToolError::not_found("OOPIF target is stale")))?;
    let session_id = client.attach(target_id).await?;
    let _ = client
        .command(
            "Runtime.runIfWaitingForDebugger",
            json!({}),
            Some(&session_id),
        )
        .await;
    let (_, before_url) = frame_identity(&mut client, &session_id, &target_info).await?;
    if let Err(reason) = policy::validate_observed(&before_url, navigation_policy).await {
        let _ = client
            .command(
                "Page.navigate",
                json!({"url":"about:blank"}),
                Some(&session_id),
            )
            .await;
        client.detach(&session_id).await;
        return Err(Error::ToolExecution(
            format!("OOPIF navigation policy blocked '{before_url}': {reason}").into(),
        ));
    }

    let mut result =
        perform_action(&mut client, &session_id, action, element, target, args).await?;
    let guard = match frame_identity(&mut client, &session_id, &target_info).await {
        Ok((_, after_url)) => {
            match policy::validate_observed(&after_url, navigation_policy).await {
                Ok(()) => json!({"status":"allowed", "url":after_url}),
                Err(reason) => {
                    let _ = client
                        .command(
                            "Page.navigate",
                            json!({"url":"about:blank"}),
                            Some(&session_id),
                        )
                        .await;
                    result = json!({
                        "status":"blocked",
                        "outcome":"unknown",
                        "retry_safe":false,
                        "reason":"the iframe navigated to a blocked URL while the action was in flight",
                    });
                    json!({"status":"blocked", "url":after_url, "reason":reason})
                }
            }
        }
        Err(error) => {
            // The action may have destroyed or replaced the target. Do not
            // misreport the old pre-action URL as proof of the new boundary.
            if let Value::Object(ref mut object) = result {
                object.insert("outcome".into(), Value::String("unknown".into()));
                object.insert("retry_safe".into(), Value::Bool(false));
            }
            json!({
                "status":"unverified",
                "reason":format!("could not verify iframe URL after action: {error}"),
            })
        }
    };
    client.detach(&session_id).await;
    if let Value::Object(ref mut object) = result {
        object.insert(
            "iframe_target_id".into(),
            Value::String(target_id.to_string()),
        );
        object.insert("iframe_navigation_guard".into(), guard);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associates_only_iframes_belonging_to_the_selected_page() {
        let targets = vec![
            json!({"type":"iframe","targetId":"child","parentFrameId":"root"}),
            json!({"type":"iframe","targetId":"grandchild","parentFrameId":"child"}),
            json!({"type":"iframe","targetId":"other","parentFrameId":"another-tab"}),
        ];
        let selected = associated_iframe_targets(&targets, &HashSet::from(["root".into()]));
        let ids: Vec<_> = selected
            .iter()
            .filter_map(|target| target["targetId"].as_str())
            .collect();
        assert_eq!(ids, vec!["child", "grandchild"]);
    }

    #[test]
    fn element_expression_escapes_agent_controlled_selectors() {
        let expression = element_expression("input[data-x='\";throw 1;//']", "return {ok:true};");
        assert!(expression.contains("\\\";throw 1;//"));
    }
}
