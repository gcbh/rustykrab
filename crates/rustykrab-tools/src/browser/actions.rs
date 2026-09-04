//! CDP-native, ref-based action system informed by browser-use's action model.
//!
//! Actions use element refs from snapshots instead of raw CSS selectors.
//! Supported actions include click, type/fill, press, hover, select/options,
//! upload, drag, and wait.

use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetContentQuadsParams, SetFileInputFilesParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EventJavascriptDialogOpening, FrameId, HandleJavaScriptDialogParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, ExecutionContextId, RemoteObjectId,
};
use chromiumoxide::layout::{ElementQuad, Point};
use chromiumoxide::Page;
use rustykrab_core::{Error, Result, ToolError, ToolErrorKind};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;

use super::snapshot::{
    take_snapshot, ElementRef, SnapshotOptions, SnapshotStore, IFRAME_SEP, SHADOW_SEP,
};
use super::{config::DialogPolicy, config::SsrfPolicy, policy};

/// Allow renderer event handlers queued by an acknowledged input command to
/// run before policy checks and the post-action snapshot. Browser-use applies a
/// 100ms default gap between actions for the same reason. Without this barrier,
/// a fast CDP response can race the click/input handler and return stale state.
const POST_ACTION_SETTLE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
pub(crate) struct ActionPolicies<'a> {
    pub dialog: DialogPolicy,
    pub navigation: &'a SsrfPolicy,
}

/// Encode a string as a safe JavaScript string literal (including quotes).
/// Uses serde_json serialization which properly escapes backslashes, quotes,
/// newlines, line/paragraph separators, and all other special characters.
fn js_string_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Execute a ref-based action on the page.
///
/// The `ref_id` comes from a previous snapshot. The action is performed on the
/// element identified by that ref's stored CSS selector.
///
/// Refs go stale whenever the page re-renders or navigates. Rather than relying
/// on the model to notice the failure and decide to re-snapshot (a decision a
/// weaker model makes inconsistently), `act` recovers deterministically:
///
/// - **Heal** — if the action fails because the element is gone, re-snapshot
///   and re-resolve the *same logical element* by role+name. When exactly one
///   element matches and the page hasn't navigated, retry the action once.
/// - **Escalate** — if the page navigated, the element is gone, or several now
///   match (ambiguous), return a `new_snapshot` payload carrying a fresh
///   snapshot so the model can re-pick a ref in the same turn.
///
/// Healing is only attempted for pre-action "element not found" failures, where
/// nothing happened yet — so a retry can't double-fire a click or a submit.
pub async fn execute_act(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    action: &str,
    ref_id: &str,
    args: &Value,
    policies: ActionPolicies<'_>,
) -> Result<Value> {
    let dialog_watchdog = DialogWatchdog::start(page, policies.dialog).await;
    let budget = action_budget(action, args);
    let result = tokio::time::timeout(
        budget,
        execute_act_inner(page, store, store_key, action, ref_id, args),
    )
    .await;

    let value = match result {
        Ok(Ok(value)) => normalize_outcome(value),
        Ok(Err(e)) if is_invalid_input(&e) || is_permission_denied(&e) => return Err(e),
        Ok(Err(e)) => unknown_outcome(action, ref_id, "action_error", e.to_string(), true),
        Err(_) => {
            store.clear(store_key).await;
            unknown_outcome(
                action,
                ref_id,
                "action_deadline",
                format!(
                    "the complete browser action exceeded its {}ms deadline",
                    budget.as_millis()
                ),
                true,
            )
        }
    };

    if value["outcome"] == "applied" && !matches!(action, "options" | "wait") {
        tokio::time::sleep(POST_ACTION_SETTLE).await;
    }

    // Enforce policy before taking the post-action snapshot. Otherwise a form
    // submit or click redirected to an internal service could return that
    // service's DOM before the caller had a chance to inspect the final URL.
    let navigation_guard = policy::enforce_page(page, policies.navigation).await;
    let mut value = if navigation_guard["status"] == "blocked" {
        store.clear(store_key).await;
        let mut value = value;
        if let Value::Object(ref mut object) = value {
            object.insert("page_state".into(), Value::Null);
            object.insert("snapshot".into(), Value::Null);
            object.insert(
                "page_state_status".into(),
                Value::String("policy_blocked".into()),
            );
        }
        value
    } else {
        attach_post_action_state(page, store, store_key, value).await?
    };
    if let Value::Object(ref mut object) = value {
        object.insert("navigation_guard".into(), navigation_guard);
    }
    Ok(match dialog_watchdog {
        Some(watchdog) => watchdog.finish(value).await,
        None => value,
    })
}

/// Click viewport coordinates from a native-resolution screenshot using the
/// same side-effect and post-state contract as ref-based clicks.
pub async fn execute_coordinate_click(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    x: f64,
    y: f64,
    dialog_policy: DialogPolicy,
    navigation_policy: &SsrfPolicy,
) -> Result<Value> {
    let dialog_watchdog = DialogWatchdog::start(page, dialog_policy).await;
    let point = Point::new(x, y);
    let mut value = match tokio::time::timeout(CLICK_MOUSE_BUDGET, page.move_mouse(point)).await {
        Ok(Ok(_)) => {
            let pressed = DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MousePressed)
                .x(x)
                .y(y)
                .button(MouseButton::Left)
                .click_count(1)
                .build()
                .expect("complete coordinate mouse-press parameters");
            match tokio::time::timeout(CLICK_PRESS_BUDGET, page.execute(pressed)).await {
                Ok(Ok(_)) => {
                    let released = DispatchMouseEventParams::builder()
                        .r#type(DispatchMouseEventType::MouseReleased)
                        .x(x)
                        .y(y)
                        .button(MouseButton::Left)
                        .click_count(1)
                        .build()
                        .expect("complete coordinate mouse-release parameters");
                    match tokio::time::timeout(CLICK_RELEASE_BUDGET, page.execute(released)).await {
                        Ok(Ok(_)) => json!({
                            "status": "clicked",
                            "outcome": "applied",
                            "action": "click_coordinates",
                            "method": "cdp_mouse",
                            "x": x,
                            "y": y,
                            "retry_safe": false,
                            "browser_degraded": false,
                        }),
                        Ok(Err(error)) => unknown_outcome(
                            "click_coordinates",
                            &format!("{x},{y}"),
                            "mouse_released",
                            error.to_string(),
                            true,
                        ),
                        Err(_) => unknown_outcome(
                            "click_coordinates",
                            &format!("{x},{y}"),
                            "mouse_released",
                            "mouse-release response timed out".to_string(),
                            true,
                        ),
                    }
                }
                Ok(Err(error)) => unknown_outcome(
                    "click_coordinates",
                    &format!("{x},{y}"),
                    "mouse_pressed",
                    error.to_string(),
                    true,
                ),
                Err(_) => unknown_outcome(
                    "click_coordinates",
                    &format!("{x},{y}"),
                    "mouse_pressed",
                    "mouse-press response timed out".to_string(),
                    true,
                ),
            }
        }
        Ok(Err(error)) => json!({
            "status": "failed",
            "outcome": "not_applied",
            "action": "click_coordinates",
            "stage": "mouse_move",
            "reason": error.to_string(),
            "x": x,
            "y": y,
            "retry_safe": true,
            "browser_degraded": false,
        }),
        Err(_) => json!({
            "status": "failed",
            "outcome": "not_applied",
            "action": "click_coordinates",
            "stage": "mouse_move",
            "reason": "mouse-move response timed out",
            "x": x,
            "y": y,
            "retry_safe": true,
            "browser_degraded": true,
        }),
    };

    if value["outcome"] == "applied" {
        tokio::time::sleep(POST_ACTION_SETTLE).await;
    }
    let navigation_guard = policy::enforce_page(page, navigation_policy).await;
    value = if navigation_guard["status"] == "blocked" {
        store.clear(store_key).await;
        if let Value::Object(ref mut object) = value {
            object.insert("page_state".into(), Value::Null);
            object.insert("snapshot".into(), Value::Null);
            object.insert(
                "page_state_status".into(),
                Value::String("policy_blocked".into()),
            );
        }
        value
    } else {
        attach_post_action_state(page, store, store_key, value).await?
    };
    if let Value::Object(ref mut object) = value {
        object.insert("navigation_guard".into(), navigation_guard);
    }
    Ok(match dialog_watchdog {
        Some(watchdog) => watchdog.finish(value).await,
        None => value,
    })
}

/// Send physical CDP keyboard input to the currently focused element.
///
/// browser-use exposes this independently of indexed/ref-based element
/// actions because it is also needed for native shortcuts and controls that
/// retain focus after a click. Keep it on the same outcome, dialog, policy,
/// and post-state contract as the other side-effecting actions.
pub async fn execute_send_keys(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    keys: &str,
    dialog_policy: DialogPolicy,
    navigation_policy: &SsrfPolicy,
) -> Result<Value> {
    let dialog_watchdog = DialogWatchdog::start(page, dialog_policy).await;
    let result = tokio::time::timeout(Duration::from_secs(12), send_keys_inner(page, keys)).await;
    let mut value = match result {
        Ok(Ok(())) => json!({
            "status": "sent",
            "outcome": "applied",
            "action": "send_keys",
            "keys": keys,
            "method": "cdp_keyboard",
            "retry_safe": false,
            "browser_degraded": false,
        }),
        Ok(Err(error)) if is_invalid_input(&error) => return Err(error),
        Ok(Err(error)) => unknown_outcome(
            "send_keys",
            "active_element",
            "key_dispatch",
            error.to_string(),
            true,
        ),
        Err(_) => unknown_outcome(
            "send_keys",
            "active_element",
            "action_deadline",
            "the complete send-keys action exceeded its 12000ms deadline".into(),
            true,
        ),
    };

    if value["outcome"] == "applied" {
        tokio::time::sleep(POST_ACTION_SETTLE).await;
    }
    let navigation_guard = policy::enforce_page(page, navigation_policy).await;
    value = if navigation_guard["status"] == "blocked" {
        store.clear(store_key).await;
        if let Value::Object(ref mut object) = value {
            object.insert("page_state".into(), Value::Null);
            object.insert("snapshot".into(), Value::Null);
            object.insert(
                "page_state_status".into(),
                Value::String("policy_blocked".into()),
            );
        }
        value
    } else {
        attach_post_action_state(page, store, store_key, value).await?
    };
    if let Value::Object(ref mut object) = value {
        object.insert("navigation_guard".into(), navigation_guard);
    }
    Ok(match dialog_watchdog {
        Some(watchdog) => watchdog.finish(value).await,
        None => value,
    })
}

fn normalize_key_alias(key: &str) -> String {
    match key.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Control".into(),
        "alt" | "option" => "Alt".into(),
        "meta" | "cmd" | "command" => "Meta".into(),
        "shift" => "Shift".into(),
        "enter" | "return" => "Enter".into(),
        "tab" => "Tab".into(),
        "delete" => "Delete".into(),
        "backspace" => "Backspace".into(),
        "escape" | "esc" => "Escape".into(),
        "space" => " ".into(),
        "up" => "ArrowUp".into(),
        "down" => "ArrowDown".into(),
        "left" => "ArrowLeft".into(),
        "right" => "ArrowRight".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        _ => key.to_string(),
    }
}

fn modifier_mask(key: &str) -> Option<i64> {
    match key {
        "Alt" => Some(1),
        "Control" => Some(2),
        "Meta" => Some(4),
        "Shift" => Some(8),
        _ => None,
    }
}

async fn dispatch_key_event(
    page: &Page,
    event_type: DispatchKeyEventType,
    key: &str,
    modifiers: i64,
) -> Result<()> {
    let definition = chromiumoxide::keys::get_key_definition(key).ok_or_else(|| {
        Error::ToolExecution(ToolError::invalid_input(format!(
            "unsupported physical key '{key}'"
        )))
    })?;
    let mut params = DispatchKeyEventParams::builder()
        .r#type(event_type)
        .key(definition.key)
        .code(definition.code)
        .windows_virtual_key_code(definition.key_code)
        .native_virtual_key_code(definition.key_code);
    if modifiers != 0 {
        params = params.modifiers(modifiers);
    }
    page.execute(params.build().expect("complete key-event parameters"))
        .await
        .map_err(|error| {
            Error::ToolExecution(format!("physical key dispatch failed: {error}").into())
        })?;
    Ok(())
}

async fn press_physical_key(page: &Page, key: &str) -> Result<()> {
    let definition = chromiumoxide::keys::get_key_definition(key).ok_or_else(|| {
        Error::ToolExecution(ToolError::invalid_input(format!(
            "unsupported physical key '{key}'"
        )))
    })?;
    let text = definition
        .text
        .map(str::to_string)
        .or_else(|| (definition.key.chars().count() == 1).then(|| definition.key.to_string()));
    let event_type = if text.is_some() {
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    let mut down = DispatchKeyEventParams::builder()
        .r#type(event_type)
        .key(definition.key)
        .code(definition.code)
        .windows_virtual_key_code(definition.key_code)
        .native_virtual_key_code(definition.key_code);
    if let Some(text) = text {
        down = down.text(text);
    }
    page.execute(down.build().expect("complete key-down parameters"))
        .await
        .map_err(|error| {
            Error::ToolExecution(format!("physical key-down failed: {error}").into())
        })?;
    dispatch_key_event(page, DispatchKeyEventType::KeyUp, key, 0).await
}

async fn send_keys_inner(page: &Page, keys: &str) -> Result<()> {
    if keys.is_empty() {
        return Err(Error::ToolExecution(ToolError::invalid_input(
            "'send_keys' requires non-empty 'keys'",
        )));
    }

    // Treat plus as a shortcut delimiter only when every prefix component is
    // a modifier. This preserves literal text such as `a+b` and supports the
    // browser-use spelling `Control++` for Control + plus.
    let shortcut = if let Some(prefix) = keys.strip_suffix("++") {
        let modifiers: Vec<String> = prefix.split('+').map(normalize_key_alias).collect();
        if !modifiers.is_empty() && modifiers.iter().all(|key| modifier_mask(key).is_some()) {
            Some((modifiers, "+".to_string()))
        } else {
            None
        }
    } else if let Some((prefix, main)) = keys.rsplit_once('+') {
        let modifiers: Vec<String> = prefix.split('+').map(normalize_key_alias).collect();
        if !main.trim().is_empty()
            && !modifiers.is_empty()
            && modifiers.iter().all(|key| modifier_mask(key).is_some())
        {
            Some((modifiers, normalize_key_alias(main)))
        } else {
            None
        }
    } else {
        None
    };

    if let Some((modifiers, main)) = shortcut {
        let mask = modifiers
            .iter()
            .filter_map(|key| modifier_mask(key))
            .fold(0, |combined, value| combined | value);
        for modifier in &modifiers {
            dispatch_key_event(page, DispatchKeyEventType::RawKeyDown, modifier, 0).await?;
        }
        dispatch_key_event(page, DispatchKeyEventType::RawKeyDown, &main, mask).await?;
        dispatch_key_event(page, DispatchKeyEventType::KeyUp, &main, mask).await?;
        for modifier in modifiers.iter().rev() {
            dispatch_key_event(page, DispatchKeyEventType::KeyUp, modifier, 0).await?;
        }
        return Ok(());
    }

    let normalized = normalize_key_alias(keys);
    let is_special = matches!(
        normalized.as_str(),
        "Enter"
            | "Tab"
            | "Delete"
            | "Backspace"
            | "Escape"
            | "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
            | "PageUp"
            | "PageDown"
            | "Home"
            | "End"
            | "Control"
            | "Alt"
            | "Meta"
            | "Shift"
            | "F1"
            | "F2"
            | "F3"
            | "F4"
            | "F5"
            | "F6"
            | "F7"
            | "F8"
            | "F9"
            | "F10"
            | "F11"
            | "F12"
    );
    if is_special {
        press_physical_key(page, &normalized).await?;
    } else {
        for part in normalized.split_inclusive(['\n', '\r']) {
            let text = part.trim_end_matches(['\n', '\r']);
            if !text.is_empty() {
                for character in text.chars() {
                    let key = character.to_string();
                    if chromiumoxide::keys::get_key_definition(&key).is_some() {
                        press_physical_key(page, &key).await?;
                    } else {
                        // CDP's text insertion path covers Unicode characters
                        // not represented in chromiumoxide's US keyboard map.
                        page.execute(InsertTextParams::new(key))
                            .await
                            .map_err(|error| {
                                Error::ToolExecution(
                                    format!("Unicode text dispatch failed: {error}").into(),
                                )
                            })?;
                    }
                }
            }
            if part.ends_with(['\n', '\r']) {
                press_physical_key(page, "Enter").await?;
            }
        }
    }
    Ok(())
}

/// Watch native JavaScript dialogs while an action is in flight. A modal
/// `alert`, `confirm`, or `prompt` freezes the renderer until CDP handles it;
/// without a concurrent listener, the action and the post-action snapshot can
/// both time out even though the click itself succeeded.
struct DialogWatchdog {
    observations: Arc<tokio::sync::Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl DialogWatchdog {
    async fn start(page: &Page, policy: DialogPolicy) -> Option<Self> {
        let mut events = tokio::time::timeout(
            Duration::from_millis(500),
            page.event_listener::<EventJavascriptDialogOpening>(),
        )
        .await
        .ok()?
        .ok()?;
        let page = page.clone();
        let observations = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let task_observations = Arc::clone(&observations);
        let task = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let dialog_type = event.r#type.as_ref().to_string();
                let message = event.message.clone();
                let accept = match policy {
                    DialogPolicy::Accept => true,
                    DialogPolicy::Dismiss => false,
                    DialogPolicy::Auto => {
                        matches!(dialog_type.as_str(), "alert" | "confirm" | "beforeunload")
                    }
                };
                let handled = matches!(
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        page.execute(HandleJavaScriptDialogParams::new(accept)),
                    )
                    .await,
                    Ok(Ok(_))
                );
                tracing::info!(
                    dialog_type,
                    handled,
                    "handled JavaScript dialog opened by browser action"
                );
                task_observations.lock().await.push(json!({
                    "type": dialog_type,
                    "message": message,
                    "accepted": handled && accept,
                    "dismissed": handled && !accept,
                    "handled": handled,
                }));
            }
        });
        Some(Self { observations, task })
    }

    async fn finish(self, mut value: Value) -> Value {
        // Give a dialog event queued with the action response one scheduler
        // turn to reach the listener before it is detached.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.task.abort();
        let observations = self.observations.lock().await.clone();
        if observations.is_empty() {
            return value;
        }

        if let Value::Object(ref mut object) = value {
            // A handled dialog opened during this click is independent
            // evidence that the click handler ran. A missing mouse-release
            // response is no longer ambiguous in that case.
            let dialog_proves_click = matches!(
                object.get("action").and_then(Value::as_str),
                Some("click" | "click_coordinates")
            ) && object.get("outcome").and_then(Value::as_str)
                == Some("unknown")
                && observations
                    .iter()
                    .any(|dialog| dialog["handled"].as_bool() == Some(true));
            if dialog_proves_click {
                object.insert("status".into(), Value::String("clicked".into()));
                object.insert("outcome".into(), Value::String("applied".into()));
                object.insert("browser_degraded".into(), Value::Bool(false));
                object.insert("confirmed_by".into(), Value::String("dialog_opened".into()));
                object.insert(
                    "message".into(),
                    Value::String(
                        "The click opened and accepted a JavaScript dialog; the dialog event confirms the action was applied."
                            .into(),
                    ),
                );
            }
            object.insert("dialogs".into(), Value::Array(observations));
        }
        value
    }
}

/// Every successful call has an explicit outcome contract. Older action
/// implementations returned only verbs such as `typed` or `pressed`, which
/// made a credential caller treat any `Ok(Value)` as proof that the side
/// effect happened. Preserve their useful details while making the outcome
/// and retry semantics machine-readable.
fn normalize_outcome(mut value: Value) -> Value {
    if let Value::Object(ref mut object) = value {
        if !object.contains_key("outcome") {
            let status = object.get("status").and_then(Value::as_str);
            let outcome = match status {
                Some("new_snapshot" | "timeout") => "not_applied",
                Some("unknown") => "unknown",
                _ => "applied",
            };
            object.insert("outcome".into(), Value::String(outcome.into()));
        }
        object
            .entry("retry_safe")
            .or_insert_with(|| Value::Bool(false));
        object
            .entry("browser_degraded")
            .or_insert_with(|| Value::Bool(false));
    }
    value
}

/// The actual action flow, wrapped as one future by [`execute_act`] so every
/// sub-operation, stale-ref repair, and retry shares one absolute budget.
async fn execute_act_inner(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    action: &str,
    ref_id: &str,
    args: &Value,
) -> Result<Value> {
    let element_ref = match store.get_ref(store_key, ref_id).await {
        Some(r) => r,
        None => {
            // No stored identity to re-resolve (ref never captured for this
            // tab, LRU-evicted, or the tab navigated). Hand back a fresh
            // snapshot rather than a bare error so the model can re-pick.
            return Ok(escalate(
                page,
                store,
                store_key,
                action,
                ref_id,
                "that ref isn't present in the current snapshot for this tab",
            )
            .await);
        }
    };

    let url_before = current_url(page).await;

    match dispatch_act(page, store, store_key, action, ref_id, &element_ref, args).await {
        Ok(v) => Ok(v),
        // A pre-action "element not found" (typed NotFound) means a stale ref —
        // recover. Genuine failures after the element resolved (click/type
        // errored) propagate unchanged: retrying those risks a double side
        // effect.
        Err(e) if is_stale_element(&e) => {
            heal_or_escalate(
                page,
                store,
                store_key,
                &element_ref,
                action,
                ref_id,
                args,
                url_before.as_deref(),
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// Run a single ref-based action against an element captured in a specific
/// document context. Child-frame identity is intentionally carried separately
/// from the CSS selector: selectors are document-local, and concatenating an
/// iframe selector cannot cross the same-origin boundary.
async fn dispatch_act(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    action: &str,
    ref_id: &str,
    element_ref: &ElementRef,
    args: &Value,
) -> Result<Value> {
    if element_ref.target_id.is_some() {
        let context = store.oopif_context(store_key).await.ok_or_else(|| {
            Error::ToolExecution(
                "site-isolated iframe action is unavailable because its CDP context expired; take a new snapshot"
                    .into(),
            )
        })?;
        let target = if action == "drag" {
            let target_ref = args["targetRef"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input("drag requires targetRef"))
            })?;
            Some(store.get_ref(store_key, target_ref).await.ok_or_else(|| {
                Error::ToolExecution(ToolError::not_found(format!(
                    "target ref '{target_ref}' not found"
                )))
            })?)
        } else {
            None
        };
        return super::oopif::execute_action(
            &context.websocket_url,
            action,
            element_ref,
            target.as_ref(),
            args,
            &context.policy,
        )
        .await;
    }

    match action {
        "click" => act_click(page, ref_id, element_ref).await,
        "type" | "fill" => {
            let text = args["text"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'type' action requires 'text' parameter",
                ))
            })?;
            let clear = args["clear"].as_bool().unwrap_or(true); // fill clears by default
            act_type(page, element_ref, text, clear).await
        }
        "press" => {
            let key = args["key"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'press' action requires 'key' parameter",
                ))
            })?;
            act_press(page, element_ref, key).await
        }
        "hover" => act_hover(page, element_ref).await,
        "select" => {
            let value = args["value"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'select' action requires 'value' parameter",
                ))
            })?;
            act_select(page, element_ref, value).await
        }
        "drag" => {
            let target_ref = args["targetRef"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'drag' requires 'targetRef' parameter",
                ))
            })?;
            let target = store.get_ref(store_key, target_ref).await.ok_or_else(|| {
                Error::ToolExecution(ToolError::not_found(format!(
                    "target ref '{target_ref}' not found"
                )))
            })?;
            act_drag(page, element_ref, &target).await
        }
        "upload" => {
            let paths = args["paths"]
                .as_array()
                .ok_or_else(|| {
                    Error::ToolExecution(ToolError::invalid_input(
                        "'upload' requires a non-empty 'paths' array",
                    ))
                })?
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if paths.is_empty() {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "'upload' requires a non-empty 'paths' array",
                )));
            }
            act_upload(page, element_ref, &paths).await
        }
        "options" => act_dropdown_options(page, element_ref).await,
        "wait" => {
            let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000).min(30_000);
            act_wait_for_element(page, element_ref, timeout_ms).await
        }
        _ => Err(Error::ToolExecution(ToolError::invalid_input(format!(
            "unknown act action '{action}'. Available: click, type, fill, press, hover, select, drag, upload, options, wait"
        )))),
    }
}

/// True when the error is a stale-ref failure (the element wasn't found before
/// the action ran), as opposed to a genuine failure mid-action.
fn is_stale_element(e: &Error) -> bool {
    matches!(e, Error::ToolExecution(te) if te.kind == ToolErrorKind::NotFound)
}

fn is_invalid_input(e: &Error) -> bool {
    matches!(e, Error::ToolExecution(te) if te.kind == ToolErrorKind::InvalidInput)
}

fn is_permission_denied(e: &Error) -> bool {
    matches!(e, Error::ToolExecution(te) if te.kind == ToolErrorKind::PermissionDenied)
}

async fn current_url(page: &Page) -> Option<String> {
    page.url().await.ok().flatten()
}

/// A stale-ref action failed. Re-snapshot, then either silently re-resolve the
/// same logical element (unique role+name match, same page) and retry once, or
/// escalate a fresh snapshot back to the model.
#[allow(clippy::too_many_arguments)]
async fn heal_or_escalate(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    stale: &ElementRef,
    action: &str,
    ref_id: &str,
    args: &Value,
    url_before: Option<&str>,
) -> Result<Value> {
    // Re-snapshot first: this refreshes the store (so find_by_identity sees the
    // current DOM) and gives us a payload to embed if we escalate.
    let snapshot = take_snapshot(page, &SnapshotOptions::default(), store, store_key)
        .await
        .ok();
    let url_after = current_url(page).await;

    // Guard 1 — navigation. A changed URL means the page is semantically
    // different; never silently re-target, hand control back to the model.
    if url_before != url_after.as_deref() {
        return Ok(new_snapshot_payload(
            action,
            ref_id,
            "the tab navigated to a different URL",
            snapshot,
            url_after.as_deref(),
        ));
    }

    // Guard 2 — unique identity. Heal only when exactly one element still
    // matches the stale ref's role+name; none or several means escalate.
    let matches = store
        .find_by_identity(
            store_key,
            &stale.role,
            &stale.name,
            stale.frame_url.as_deref(),
        )
        .await;
    match matches.as_slice() {
        [only] => {
            match dispatch_act(page, store, store_key, action, &only.ref_id, only, args).await {
                Ok(mut v) => {
                    if let Value::Object(ref mut o) = v {
                        o.insert("recovered".into(), Value::Bool(true));
                    }
                    Ok(v)
                }
                Err(_) => Ok(new_snapshot_payload(
                    action,
                    ref_id,
                    "the matching element could not be actioned",
                    snapshot,
                    url_after.as_deref(),
                )),
            }
        }
        [] => Ok(new_snapshot_payload(
            action,
            ref_id,
            "the element is no longer present on the page",
            snapshot,
            url_after.as_deref(),
        )),
        _ => Ok(new_snapshot_payload(
            action,
            ref_id,
            "several elements now match the same role/name (ambiguous)",
            snapshot,
            url_after.as_deref(),
        )),
    }
}

/// Most interactions should either complete or produce actionable state well
/// before the runner's 60-second browser-tool ceiling. `wait` is the exception:
/// its caller-supplied wait is honored, with a small allowance for state
/// capture, but capped so one model call cannot monopolize the browser.
fn action_budget(action: &str, args: &Value) -> Duration {
    if action == "wait" {
        let requested = args["timeout_ms"].as_u64().unwrap_or(10_000);
        return Duration::from_millis(requested.min(30_000).saturating_add(2_000));
    }
    Duration::from_secs(15)
}

/// Attach the current page state to an action result. This mirrors the useful
/// browser-use invariant that an action and the state it produced travel back
/// together. A failed state capture never changes the action outcome.
async fn attach_post_action_state(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    mut value: Value,
) -> Result<Value> {
    if value["status"] == "new_snapshot" {
        return Ok(value);
    }

    let state = tokio::time::timeout(
        Duration::from_secs(5),
        take_snapshot(page, &SnapshotOptions::default(), store, store_key),
    )
    .await;

    if let Value::Object(ref mut object) = value {
        match state {
            Ok(Ok(snapshot)) => {
                object.insert("page_state".into(), snapshot);
                object.insert("page_state_status".into(), Value::String("captured".into()));
            }
            Ok(Err(error)) => {
                object.insert("page_state".into(), Value::Null);
                object.insert("page_state_status".into(), Value::String("failed".into()));
                object.insert("page_state_reason".into(), Value::String(error.to_string()));
            }
            Err(_) => {
                object.insert("page_state".into(), Value::Null);
                object.insert(
                    "page_state_status".into(),
                    Value::String("timed_out".into()),
                );
                object.insert(
                    "page_state_reason".into(),
                    Value::String(
                        "post-action snapshot exceeded its 5s observation budget; the action outcome is unchanged"
                            .into(),
                    ),
                );
            }
        }
    }
    Ok(value)
}

fn unknown_outcome(
    action: &str,
    ref_id: &str,
    stage: &str,
    reason: String,
    browser_degraded: bool,
) -> Value {
    tracing::warn!(
        action,
        ref_id,
        stage,
        browser_degraded,
        reason = %reason,
        "browser action outcome is unknown"
    );
    json!({
        "status": "unknown",
        "outcome": "unknown",
        "action": action,
        "ref": ref_id,
        "stage": stage,
        "reason": reason,
        "retry_safe": false,
        "browser_degraded": browser_degraded,
        "message": "The browser may have applied this action before the response was lost. Do not repeat it blindly; use page_state or a new snapshot to determine what happened."
    })
}

/// Re-snapshot and build a stale-ref escalation payload without attempting a
/// heal (used when there's no stored identity to re-resolve).
async fn escalate(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    action: &str,
    ref_id: &str,
    reason: &str,
) -> Value {
    let snapshot = take_snapshot(page, &SnapshotOptions::default(), store, store_key)
        .await
        .ok();
    let url = current_url(page).await;
    new_snapshot_payload(action, ref_id, reason, snapshot, url.as_deref())
}

/// Build the `new_snapshot` payload returned to the model. Returned as `Ok`
/// (not `Err`) on purpose: the runner blindly retries failed tool calls with
/// the same arguments, which is pointless when the page has moved on. As an
/// `Ok` payload the model gets the fresh snapshot in hand and can re-pick a
/// ref in one turn — and the framing is an observation ("here's the current
/// state"), not a failure report, so weaker models are less likely to read it
/// as "retry the same call."
fn new_snapshot_payload(
    action: &str,
    ref_id: &str,
    reason: &str,
    snapshot: Option<Value>,
    url: Option<&str>,
) -> Value {
    json!({
        "status": "new_snapshot",
        "snapshot": snapshot,
        "url": url,
        "action": action,
        "previous_ref": ref_id,
        "reason": reason,
        "message": format!(
            "Fresh snapshot taken — {reason}. The current page state is in \
             \"snapshot\". Pick a ref from this new snapshot and call act with \
             that ref to continue. The previous ref {ref_id} refers to an \
             element that is not in this snapshot."
        ),
    })
}

/// How long a single CDP element operation may take.
///
/// A page whose DOM handle has gone stale -- which happens after it
/// navigates -- answers `DOM.querySelector` neither quickly nor at all.
/// Unbounded, the call falls through to the configurable CDP request timeout,
/// the runner retries the identical call, and one unreachable
/// input consumed three 60s tool budgets inside a single 600s trial
/// before it ran out. The page itself was healthy throughout: the
/// accessibility snapshot kept returning the full form while every
/// `find_element` against it timed out.
///
/// Ten seconds is far longer than a live document needs and far shorter
/// than a wedged one costs.
const ELEMENT_OP_BUDGET: Duration = Duration::from_secs(3);
const CLICK_GEOMETRY_BUDGET: Duration = Duration::from_secs(3);
const CLICK_MOUSE_BUDGET: Duration = Duration::from_secs(2);
const CLICK_PRESS_BUDGET: Duration = Duration::from_secs(3);
const CLICK_RELEASE_BUDGET: Duration = Duration::from_secs(5);
const CLICK_JS_FALLBACK_BUDGET: Duration = Duration::from_secs(3);

/// Resolve selectors through open shadow roots. Legacy `|||` chains are kept
/// for refs produced by older snapshots; new iframe refs carry a CDP frame id
/// and begin resolving from that frame's own `document`.
const ELEMENT_RESOLVER: &str = r#"function(selector) {
    var parts = String(selector).split(/( >>> | \|\|\| )/);
    var root = document;
    var element = root.querySelector(parts[0]);
    for (var i = 1; element && i < parts.length; i += 2) {
        var boundary = parts[i];
        root = boundary === ' >>> ' ? element.shadowRoot : element.contentDocument;
        if (!root || !root.querySelector) return null;
        element = root.querySelector(parts[i + 1]);
    }
    return element || null;
}"#;

#[derive(Debug, Clone)]
struct ResolvedElement {
    object_id: RemoteObjectId,
    backend_node_id: BackendNodeId,
}

fn requires_document_resolver(element_ref: &ElementRef) -> bool {
    element_ref.frame_id.is_some()
        || element_ref.selector.contains(SHADOW_SEP)
        || element_ref.selector.contains(IFRAME_SEP)
}

async fn execution_context_for_ref(
    page: &Page,
    element_ref: &ElementRef,
) -> Result<Option<ExecutionContextId>> {
    let Some(frame_id) = element_ref.frame_id.as_deref() else {
        return Ok(None);
    };
    match tokio::time::timeout(
        ELEMENT_OP_BUDGET,
        page.frame_execution_context(FrameId::new(frame_id)),
    )
    .await
    {
        Ok(Ok(Some(context_id))) => Ok(Some(context_id)),
        Ok(Ok(None)) => Err(Error::ToolExecution(ToolError::not_found(format!(
            "frame execution context is no longer available for '{}'",
            element_ref.frame_url.as_deref().unwrap_or(frame_id)
        )))),
        Ok(Err(error)) => Err(Error::ToolExecution(ToolError::not_found(format!(
            "failed to resolve frame '{}': {error}",
            element_ref.frame_url.as_deref().unwrap_or(frame_id)
        )))),
        Err(_) => Err(Error::ToolExecution(ToolError::not_found(format!(
            "frame lookup timed out for '{}'",
            element_ref.frame_url.as_deref().unwrap_or(frame_id)
        )))),
    }
}

fn resolver_expression(selector: &str) -> String {
    format!("({ELEMENT_RESOLVER})({})", js_string_literal(selector))
}

fn element_expression(selector: &str, body: &str) -> String {
    format!(
        "(function() {{ var el = ({ELEMENT_RESOLVER})({}); if (!el) return 'element_not_found'; {body} }})()",
        js_string_literal(selector)
    )
}

async fn evaluate_in_element_context<T: serde::de::DeserializeOwned>(
    page: &Page,
    element_ref: &ElementRef,
    expression: String,
) -> Result<T> {
    let context_id = execution_context_for_ref(page, element_ref).await?;
    let mut params = EvaluateParams::builder()
        .expression(expression)
        .return_by_value(true);
    if let Some(context_id) = context_id {
        params = params.context_id(context_id);
    }
    let params = params
        .build()
        .map_err(|e| Error::ToolExecution(format!("invalid browser evaluation: {e}").into()))?;
    let result = tokio::time::timeout(ELEMENT_OP_BUDGET, page.evaluate_expression(params))
        .await
        .map_err(|_| Error::ToolExecution("browser evaluation timed out".into()))?
        .map_err(|e| Error::ToolExecution(format!("browser evaluation failed: {e}").into()))?;
    result
        .into_value()
        .map_err(|e| Error::ToolExecution(format!("invalid browser evaluation result: {e}").into()))
}

async fn resolve_element(page: &Page, element_ref: &ElementRef) -> Result<ResolvedElement> {
    let context_id = execution_context_for_ref(page, element_ref).await?;
    let mut params = EvaluateParams::builder()
        .expression(resolver_expression(&element_ref.selector))
        .return_by_value(false);
    if let Some(context_id) = context_id {
        params = params.context_id(context_id);
    }
    let params = params
        .build()
        .map_err(|e| Error::ToolExecution(format!("invalid element lookup: {e}").into()))?;
    let evaluated = tokio::time::timeout(ELEMENT_OP_BUDGET, page.evaluate_expression(params))
        .await
        .map_err(|_| {
            Error::ToolExecution(ToolError::not_found(format!(
                "element lookup timed out for '{}'",
                element_ref.selector
            )))
        })?
        .map_err(|e| {
            Error::ToolExecution(ToolError::not_found(format!(
                "element lookup failed for '{}': {e}",
                element_ref.selector
            )))
        })?;
    let object_id = evaluated.object().object_id.clone().ok_or_else(|| {
        Error::ToolExecution(ToolError::not_found(format!(
            "element not found: '{}'",
            element_ref.selector
        )))
    })?;
    let described = tokio::time::timeout(
        ELEMENT_OP_BUDGET,
        page.execute(
            DescribeNodeParams::builder()
                .object_id(object_id.clone())
                .depth(0)
                .build(),
        ),
    )
    .await
    .map_err(|_| Error::ToolExecution("element description timed out".into()))?
    .map_err(|e| {
        Error::ToolExecution(ToolError::not_found(format!(
            "element detached before action: {e}"
        )))
    })?;
    Ok(ResolvedElement {
        object_id,
        backend_node_id: described.node.backend_node_id,
    })
}

async fn call_on_element(
    page: &Page,
    resolved: &ResolvedElement,
    function: &str,
) -> std::result::Result<Value, String> {
    let params = CallFunctionOnParams::builder()
        .function_declaration(function)
        .object_id(resolved.object_id.clone())
        .await_promise(true)
        .return_by_value(true)
        .build()
        .map_err(|e| format!("invalid element function: {e}"))?;
    let response = page.execute(params).await.map_err(|e| e.to_string())?;
    if let Some(ref exception) = response.exception_details {
        return Err(format!("JavaScript exception: {exception:?}"));
    }
    Ok(response.result.result.value.unwrap_or(Value::Null))
}

async fn resolved_clickable_point(page: &Page, resolved: &ResolvedElement) -> Result<Point> {
    let quads = page
        .execute(
            GetContentQuadsParams::builder()
                .backend_node_id(resolved.backend_node_id)
                .build(),
        )
        .await
        .map_err(|e| {
            Error::ToolExecution(format!("could not read element geometry: {e}").into())
        })?;
    quads
        .quads
        .iter()
        .filter(|quad| quad.inner().len() == 8)
        .map(ElementQuad::from_quad)
        .find(|quad| quad.quad_area() > 1.0)
        .map(|quad| quad.quad_center())
        .ok_or_else(|| Error::ToolExecution("element has no clickable geometry".into()))
}

/// `find_element` with a bound, and an error the model can act on.
///
/// "Request timed out" told the agent nothing, so it reissued the same
/// call until the trial ended. A fresh snapshot re-reads the document and
/// yields refs that work, so say that.
pub(super) async fn find_element_bounded(
    page: &Page,
    selector: &str,
) -> Result<chromiumoxide::Element> {
    match tokio::time::timeout(ELEMENT_OP_BUDGET, page.find_element(selector)).await {
        Ok(Ok(elem)) => Ok(elem),
        Ok(Err(e)) => Err(Error::ToolExecution(ToolError::not_found(format!(
            "element not found '{selector}': {e}"
        )))),
        Err(_) => Err(Error::ToolExecution(
            format!(
                "element lookup for '{selector}' did not respond within {}s. \
                 The selected target session may be stale or its renderer may \
                 be unresponsive. A fresh snapshot or browser recovery is required.",
                ELEMENT_OP_BUDGET.as_secs()
            )
            .into(),
        )),
    }
}

/// Click an element using explicit, individually-bounded CDP stages.
///
/// Chromiumoxide's `Element::click()` hides scroll, geometry, move, press and
/// release behind one future. A missing response in any stage used to consume
/// one or two 30-second client timeouts before the runner killed the tool. The
/// explicit sequence keeps failures attributable and, critically, distinguishes
/// failures before a side effect from ambiguous failures after mouse-down.
async fn act_click(page: &Page, ref_id: &str, element_ref: &ElementRef) -> Result<Value> {
    if requires_document_resolver(element_ref) {
        return act_click_in_document_context(page, ref_id, element_ref).await;
    }
    let selector = &element_ref.selector;
    let elem = find_element_bounded(page, selector).await?;
    let backend_node_id = elem.backend_node_id.inner();

    tracing::debug!(
        action = "click",
        ref_id,
        selector,
        backend_node_id = *backend_node_id,
        "browser action resolved element"
    );

    match tokio::time::timeout(CLICK_GEOMETRY_BUDGET, elem.scroll_into_view()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Ok(js_click_fallback(
                &elem,
                ref_id,
                selector,
                "scroll_into_view",
                e.to_string(),
            )
            .await)
        }
        Err(_) => {
            return Ok(js_click_fallback(
                &elem,
                ref_id,
                selector,
                "scroll_into_view",
                "stage timed out".to_string(),
            )
            .await)
        }
    }

    let point = match tokio::time::timeout(CLICK_GEOMETRY_BUDGET, elem.clickable_point()).await {
        Ok(Ok(point)) => point,
        Ok(Err(e)) => {
            return Ok(
                js_click_fallback(&elem, ref_id, selector, "clickable_point", e.to_string()).await,
            )
        }
        Err(_) => {
            return Ok(js_click_fallback(
                &elem,
                ref_id,
                selector,
                "clickable_point",
                "stage timed out".to_string(),
            )
            .await)
        }
    };

    match tokio::time::timeout(CLICK_MOUSE_BUDGET, page.move_mouse(point)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Ok(
                js_click_fallback(&elem, ref_id, selector, "mouse_move", e.to_string()).await,
            )
        }
        Err(_) => {
            return Ok(js_click_fallback(
                &elem,
                ref_id,
                selector,
                "mouse_move",
                "stage timed out".to_string(),
            )
            .await)
        }
    }

    let pressed = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(point.x)
        .y(point.y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .expect("complete mouse-press parameters");
    match tokio::time::timeout(CLICK_PRESS_BUDGET, page.execute(pressed)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Ok(unknown_outcome(
                "click",
                ref_id,
                "mouse_pressed",
                e.to_string(),
                true,
            ))
        }
        Err(_) => {
            return Ok(unknown_outcome(
                "click",
                ref_id,
                "mouse_pressed",
                "mouse-press response timed out".to_string(),
                true,
            ))
        }
    }

    let released = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(point.x)
        .y(point.y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .expect("complete mouse-release parameters");
    match tokio::time::timeout(CLICK_RELEASE_BUDGET, page.execute(released)).await {
        Ok(Ok(_)) => Ok(json!({
            "status": "clicked",
            "outcome": "applied",
            "method": "cdp_mouse",
            "ref": ref_id,
            "selector": selector,
            "retry_safe": false
        })),
        Ok(Err(e)) => Ok(unknown_outcome(
            "click",
            ref_id,
            "mouse_released",
            e.to_string(),
            true,
        )),
        Err(_) => Ok(unknown_outcome(
            "click",
            ref_id,
            "mouse_released",
            "mouse-release response timed out".to_string(),
            true,
        )),
    }
}

async fn act_click_in_document_context(
    page: &Page,
    ref_id: &str,
    element_ref: &ElementRef,
) -> Result<Value> {
    let selector = &element_ref.selector;
    let resolved = resolve_element(page, element_ref).await?;
    tracing::debug!(
        action = "click",
        ref_id,
        selector,
        frame_id = ?element_ref.frame_id,
        backend_node_id = *resolved.backend_node_id.inner(),
        "browser action resolved frame/shadow element"
    );

    let scroll = tokio::time::timeout(
        CLICK_GEOMETRY_BUDGET,
        call_on_element(
            page,
            &resolved,
            "function() { this.scrollIntoView({block:'center', inline:'center', behavior:'instant'}); return true; }",
        ),
    )
    .await;
    if !matches!(scroll, Ok(Ok(_))) {
        let reason = match scroll {
            Ok(Err(reason)) => reason,
            Err(_) => "stage timed out".to_string(),
            Ok(Ok(_)) => unreachable!(),
        };
        return Ok(js_click_fallback_resolved(
            page,
            &resolved,
            ref_id,
            element_ref,
            "scroll_into_view",
            reason,
        )
        .await);
    }

    let point = match tokio::time::timeout(
        CLICK_GEOMETRY_BUDGET,
        resolved_clickable_point(page, &resolved),
    )
    .await
    {
        Ok(Ok(point)) => point,
        Ok(Err(error)) => {
            return Ok(js_click_fallback_resolved(
                page,
                &resolved,
                ref_id,
                element_ref,
                "clickable_point",
                error.to_string(),
            )
            .await)
        }
        Err(_) => {
            return Ok(js_click_fallback_resolved(
                page,
                &resolved,
                ref_id,
                element_ref,
                "clickable_point",
                "stage timed out".to_string(),
            )
            .await)
        }
    };

    match tokio::time::timeout(CLICK_MOUSE_BUDGET, page.move_mouse(point)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return Ok(js_click_fallback_resolved(
                page,
                &resolved,
                ref_id,
                element_ref,
                "mouse_move",
                error.to_string(),
            )
            .await)
        }
        Err(_) => {
            return Ok(js_click_fallback_resolved(
                page,
                &resolved,
                ref_id,
                element_ref,
                "mouse_move",
                "stage timed out".to_string(),
            )
            .await)
        }
    }

    let pressed = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(point.x)
        .y(point.y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .expect("complete mouse-press parameters");
    match tokio::time::timeout(CLICK_PRESS_BUDGET, page.execute(pressed)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return Ok(unknown_outcome(
                "click",
                ref_id,
                "mouse_pressed",
                error.to_string(),
                true,
            ))
        }
        Err(_) => {
            return Ok(unknown_outcome(
                "click",
                ref_id,
                "mouse_pressed",
                "mouse-press response timed out".to_string(),
                true,
            ))
        }
    }

    let released = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(point.x)
        .y(point.y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .expect("complete mouse-release parameters");
    match tokio::time::timeout(CLICK_RELEASE_BUDGET, page.execute(released)).await {
        Ok(Ok(_)) => Ok(json!({
            "status": "clicked",
            "outcome": "applied",
            "method": "cdp_mouse",
            "ref": ref_id,
            "selector": selector,
            "frame_id": element_ref.frame_id,
            "retry_safe": false
        })),
        Ok(Err(error)) => Ok(unknown_outcome(
            "click",
            ref_id,
            "mouse_released",
            error.to_string(),
            true,
        )),
        Err(_) => Ok(unknown_outcome(
            "click",
            ref_id,
            "mouse_released",
            "mouse-release response timed out".to_string(),
            true,
        )),
    }
}

async fn js_click_fallback_resolved(
    page: &Page,
    resolved: &ResolvedElement,
    ref_id: &str,
    element_ref: &ElementRef,
    failed_stage: &str,
    failed_reason: String,
) -> Value {
    match tokio::time::timeout(
        CLICK_JS_FALLBACK_BUDGET,
        call_on_element(page, resolved, "function() { this.click(); return true; }"),
    )
    .await
    {
        Ok(Ok(_)) => json!({
            "status": "clicked",
            "outcome": "applied",
            "method": "javascript",
            "fallback_from": failed_stage,
            "ref": ref_id,
            "selector": element_ref.selector,
            "frame_id": element_ref.frame_id,
            "retry_safe": false
        }),
        Ok(Err(error)) => unknown_outcome(
            "click",
            ref_id,
            "javascript_click",
            format!("{failed_stage} failed ({failed_reason}); JavaScript fallback failed: {error}"),
            true,
        ),
        Err(_) => unknown_outcome(
            "click",
            ref_id,
            "javascript_click",
            format!("{failed_stage} failed ({failed_reason}); JavaScript fallback timed out"),
            true,
        ),
    }
}

/// Browser-use's most valuable click fallback is `HTMLElement.click()` when
/// layout/occlusion geometry cannot be obtained. It is still a CDP request, so
/// a lost response is an *unknown* side-effect outcome rather than a safe error.
async fn js_click_fallback(
    elem: &chromiumoxide::Element,
    ref_id: &str,
    selector: &str,
    failed_stage: &str,
    failed_reason: String,
) -> Value {
    tracing::debug!(
        action = "click",
        ref_id,
        selector,
        failed_stage,
        reason = %failed_reason,
        "falling back to HTMLElement.click"
    );
    match tokio::time::timeout(
        CLICK_JS_FALLBACK_BUDGET,
        elem.call_js_fn("function() { this.click(); return true; }", false),
    )
    .await
    {
        Ok(Ok(_)) => json!({
            "status": "clicked",
            "outcome": "applied",
            "method": "javascript",
            "fallback_from": failed_stage,
            "ref": ref_id,
            "selector": selector,
            "retry_safe": false
        }),
        Ok(Err(e)) => unknown_outcome(
            "click",
            ref_id,
            "javascript_click",
            format!("{failed_stage} failed ({failed_reason}); JavaScript fallback failed: {e}"),
            true,
        ),
        Err(_) => unknown_outcome(
            "click",
            ref_id,
            "javascript_click",
            format!("{failed_stage} failed ({failed_reason}); JavaScript fallback timed out"),
            true,
        ),
    }
}

/// Type text into an element, optionally clearing first.
async fn act_type(page: &Page, element_ref: &ElementRef, text: &str, clear: bool) -> Result<Value> {
    if requires_document_resolver(element_ref) {
        let resolved = resolve_element(page, element_ref).await?;
        tokio::time::timeout(
            CLICK_GEOMETRY_BUDGET,
            call_on_element(page, &resolved, "function() { this.focus(); return true; }"),
        )
        .await
        .map_err(|_| Error::ToolExecution("focus timed out".into()))?
        .map_err(|error| Error::ToolExecution(format!("focus failed: {error}").into()))?;
        if clear {
            tokio::time::timeout(
                CLICK_GEOMETRY_BUDGET,
                call_on_element(
                    page,
                    &resolved,
                    "function() { var p = Object.getPrototypeOf(this); var d = Object.getOwnPropertyDescriptor(p, 'value'); if (d && d.set) d.set.call(this, ''); else this.value = ''; this.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'deleteContentBackward'})); this.dispatchEvent(new Event('change', {bubbles:true})); return true; }",
                ),
            )
            .await
            .map_err(|_| Error::ToolExecution("clear timed out".into()))?
            .map_err(|error| Error::ToolExecution(format!("clear failed: {error}").into()))?;
        }
        tokio::time::timeout(
            Duration::from_secs(8),
            page.execute(InsertTextParams::new(text)),
        )
        .await
        .map_err(|_| Error::ToolExecution("typing timed out".into()))?
        .map_err(|error| Error::ToolExecution(format!("typing failed: {error}").into()))?;
        return Ok(json!({
            "status": "typed",
            "selector": element_ref.selector,
            "frame_id": element_ref.frame_id,
            "length": text.len(),
            "cleared": clear
        }));
    }
    let selector = &element_ref.selector;
    let elem = find_element_bounded(page, selector).await?;

    // Focus without clicking. A focus click can activate a surrounding label,
    // submit control, or navigation and makes typing's outcome ambiguous before
    // a character has been entered.
    tokio::time::timeout(CLICK_GEOMETRY_BUDGET, elem.focus())
        .await
        .map_err(|_| Error::ToolExecution(format!("focus timed out on '{selector}'").into()))?
        .map_err(|e| Error::ToolExecution(format!("failed to focus '{selector}': {e}").into()))?;

    if clear {
        // Clear existing value via JS
        let sel_lit = js_string_literal(selector);
        let clear_js = format!(
            "var el = document.querySelector({sel_lit}); if(el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{bubbles: true}})); }}"
        );
        tokio::time::timeout(CLICK_GEOMETRY_BUDGET, page.evaluate(clear_js))
            .await
            .map_err(|_| Error::ToolExecution(format!("clear timed out on '{selector}'").into()))?
            .map_err(|e| {
                Error::ToolExecution(format!("clear failed on '{selector}': {e}").into())
            })?;
    }

    tokio::time::timeout(Duration::from_secs(8), elem.type_str(text))
        .await
        .map_err(|_| Error::ToolExecution(format!("typing timed out on '{selector}'").into()))?
        .map_err(|e| Error::ToolExecution(format!("typing failed on '{selector}': {e}").into()))?;

    Ok(json!({
        "status": "typed",
        "selector": selector,
        "length": text.len(),
        "cleared": clear
    }))
}

/// Press a key on an element (e.g., "Enter", "Tab", "Escape").
async fn act_press(page: &Page, element_ref: &ElementRef, key: &str) -> Result<Value> {
    let selector = &element_ref.selector;
    if requires_document_resolver(element_ref) {
        let resolved = resolve_element(page, element_ref).await?;
        tokio::time::timeout(
            CLICK_GEOMETRY_BUDGET,
            call_on_element(page, &resolved, "function() { this.focus(); return true; }"),
        )
        .await
        .map_err(|_| Error::ToolExecution("focus timed out".into()))?
        .map_err(|error| Error::ToolExecution(format!("focus failed: {error}").into()))?;
    } else {
        let elem = find_element_bounded(page, selector).await?;
        tokio::time::timeout(CLICK_GEOMETRY_BUDGET, elem.focus())
            .await
            .map_err(|_| Error::ToolExecution(format!("focus timed out on '{selector}'").into()))?
            .map_err(|error| {
                Error::ToolExecution(format!("failed to focus '{selector}': {error}").into())
            })?;
    }
    send_keys_inner(page, key).await?;

    Ok(json!({
        "status": "pressed",
        "key": key,
        "selector": selector,
        "method": "cdp_keyboard",
    }))
}

/// Hover over an element.
async fn act_hover(page: &Page, element_ref: &ElementRef) -> Result<Value> {
    let selector = &element_ref.selector;
    if requires_document_resolver(element_ref) {
        let resolved = resolve_element(page, element_ref).await?;
        call_on_element(
            page,
            &resolved,
            "function() { this.scrollIntoView({block:'center', inline:'center', behavior:'instant'}); return true; }",
        )
        .await
        .map_err(|error| Error::ToolExecution(format!("hover scroll failed: {error}").into()))?;
        let point = resolved_clickable_point(page, &resolved).await?;
        page.move_mouse(point)
            .await
            .map_err(|error| Error::ToolExecution(format!("hover failed: {error}").into()))?;
        return Ok(json!({
            "status": "hovered",
            "selector": selector,
            "frame_id": element_ref.frame_id
        }));
    }
    let sel_lit = js_string_literal(selector);
    let js = format!(
        r#"(function() {{
            var el = document.querySelector({sel_lit});
            if (!el) return 'element_not_found';
            var rect = el.getBoundingClientRect();
            var event = new MouseEvent('mouseover', {{
                clientX: rect.x + rect.width / 2,
                clientY: rect.y + rect.height / 2,
                bubbles: true
            }});
            el.dispatchEvent(event);
            var enter = new MouseEvent('mouseenter', {{
                clientX: rect.x + rect.width / 2,
                clientY: rect.y + rect.height / 2,
                bubbles: true
            }});
            el.dispatchEvent(enter);
            return 'hovered';
        }})()"#
    );

    let result = page
        .evaluate(js)
        .await
        .map_err(|e| Error::ToolExecution(format!("hover failed: {e}").into()))?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    if status == "element_not_found" {
        return Err(Error::ToolExecution(ToolError::not_found(format!(
            "element not found: '{selector}'"
        ))));
    }

    Ok(json!({ "status": "hovered", "selector": selector }))
}

/// Select an option in a dropdown.
async fn act_select(page: &Page, element_ref: &ElementRef, value: &str) -> Result<Value> {
    let selector = &element_ref.selector;
    let sel_lit = js_string_literal(selector);
    let val_lit = js_string_literal(value);
    let js = if requires_document_resolver(element_ref) {
        element_expression(
            selector,
            &format!(
                r#"
            el.value = {val_lit};
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            return 'selected';"#
            ),
        )
    } else {
        format!(
            r#"(function() {{
                var el = document.querySelector({sel_lit});
                if (!el) return 'element_not_found';
                el.value = {val_lit};
                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                return 'selected';
            }})()"#
        )
    };

    let status: String = if requires_document_resolver(element_ref) {
        evaluate_in_element_context(page, element_ref, js).await?
    } else {
        page.evaluate(js)
            .await
            .map_err(|e| Error::ToolExecution(format!("select failed: {e}").into()))?
            .into_value()
            .unwrap_or_else(|_| "unknown".into())
    };
    if status == "element_not_found" {
        return Err(Error::ToolExecution(ToolError::not_found(format!(
            "element not found: '{selector}'"
        ))));
    }

    Ok(json!({ "status": "selected", "selector": selector, "value": value }))
}

/// Upload local, pre-validated files through CDP's native file-input command.
/// This works for hidden inputs and does not need to synthesize a chooser UI.
async fn act_upload(page: &Page, element_ref: &ElementRef, paths: &[String]) -> Result<Value> {
    let resolved = resolve_element(page, element_ref).await?;
    let is_file_input = tokio::time::timeout(
        ELEMENT_OP_BUDGET,
        call_on_element(
            page,
            &resolved,
            "function() { return this.tagName === 'INPUT' && String(this.type).toLowerCase() === 'file'; }",
        ),
    )
    .await
    .map_err(|_| Error::ToolExecution("file-input verification timed out".into()))?
    .map_err(|error| {
        Error::ToolExecution(format!("file-input verification failed: {error}").into())
    })?
    .as_bool()
    .unwrap_or(false);
    if !is_file_input {
        return Err(Error::ToolExecution(ToolError::invalid_input(
            "upload requires a live <input type='file'> ref",
        )));
    }
    let params = SetFileInputFilesParams::builder()
        .files(paths.iter().cloned())
        .backend_node_id(resolved.backend_node_id)
        .build()
        .map_err(|error| Error::ToolExecution(format!("invalid upload request: {error}").into()))?;
    tokio::time::timeout(Duration::from_secs(10), page.execute(params))
        .await
        .map_err(|_| Error::ToolExecution("file upload timed out".into()))?
        .map_err(|error| Error::ToolExecution(format!("file upload failed: {error}").into()))?;
    Ok(json!({
        "status": "uploaded",
        "selector": element_ref.selector,
        "file_count": paths.len(),
        "files": paths,
    }))
}

/// Return the actual options available in a native `<select>` element.
async fn act_dropdown_options(page: &Page, element_ref: &ElementRef) -> Result<Value> {
    let result: Value = evaluate_in_element_context(
        page,
        element_ref,
        element_expression(
            &element_ref.selector,
            r#"
            if (el.tagName !== 'SELECT') return {status:'not_select'};
            return {
                status: 'ok',
                multiple: Boolean(el.multiple),
                disabled: Boolean(el.disabled),
                options: Array.from(el.options).map(function(option, index) {
                    return {
                        index: index,
                        text: (option.textContent || '').trim(),
                        value: option.value,
                        selected: option.selected,
                        disabled: option.disabled
                    };
                })
            };"#,
        ),
    )
    .await?;
    if result["status"] == "element_not_found" {
        return Err(Error::ToolExecution(ToolError::not_found(format!(
            "element not found: '{}'",
            element_ref.selector
        ))));
    }
    if result["status"] == "not_select" {
        return Err(Error::ToolExecution(ToolError::invalid_input(
            "dropdown options require a native <select> element",
        )));
    }
    Ok(json!({
        "status": "options",
        "selector": element_ref.selector,
        "multiple": result["multiple"],
        "disabled": result["disabled"],
        "options": result["options"],
    }))
}

/// Drag one element to another.
async fn act_drag(page: &Page, source: &ElementRef, target: &ElementRef) -> Result<Value> {
    if source.frame_id != target.frame_id {
        return Err(Error::ToolExecution(ToolError::invalid_input(
            "dragging between different frame documents is not supported",
        )));
    }
    let source_selector = &source.selector;
    let target_selector = &target.selector;
    let src_lit = js_string_literal(source_selector);
    let tgt_lit = js_string_literal(target_selector);
    let source_lookup = if requires_document_resolver(source) {
        format!("({ELEMENT_RESOLVER})({src_lit})")
    } else {
        format!("document.querySelector({src_lit})")
    };
    let target_lookup = if requires_document_resolver(target) {
        format!("({ELEMENT_RESOLVER})({tgt_lit})")
    } else {
        format!("document.querySelector({tgt_lit})")
    };
    let js = format!(
        r#"(function() {{
            var src = {source_lookup};
            var tgt = {target_lookup};
            if (!src) return 'source_not_found';
            if (!tgt) return 'target_not_found';
            var srcRect = src.getBoundingClientRect();
            var tgtRect = tgt.getBoundingClientRect();
            var sx = srcRect.x + srcRect.width / 2;
            var sy = srcRect.y + srcRect.height / 2;
            var tx = tgtRect.x + tgtRect.width / 2;
            var ty = tgtRect.y + tgtRect.height / 2;
            src.dispatchEvent(new MouseEvent('mousedown', {{ clientX: sx, clientY: sy, bubbles: true }}));
            src.dispatchEvent(new MouseEvent('mousemove', {{ clientX: tx, clientY: ty, bubbles: true }}));
            tgt.dispatchEvent(new MouseEvent('mouseup', {{ clientX: tx, clientY: ty, bubbles: true }}));
            // Also fire dragstart/drop for drag-and-drop API
            try {{
                var dt = new DataTransfer();
                src.dispatchEvent(new DragEvent('dragstart', {{ dataTransfer: dt, bubbles: true }}));
                tgt.dispatchEvent(new DragEvent('drop', {{ dataTransfer: dt, bubbles: true }}));
                src.dispatchEvent(new DragEvent('dragend', {{ bubbles: true }}));
            }} catch(e) {{}}
            return 'dragged';
        }})()"#
    );

    let status: String = if requires_document_resolver(source) {
        evaluate_in_element_context(page, source, js).await?
    } else {
        page.evaluate(js)
            .await
            .map_err(|e| Error::ToolExecution(format!("drag failed: {e}").into()))?
            .into_value()
            .unwrap_or_else(|_| "unknown".into())
    };
    match status.as_str() {
        "source_not_found" => Err(Error::ToolExecution(ToolError::not_found(format!(
            "source element not found: '{source_selector}'"
        )))),
        "target_not_found" => Err(Error::ToolExecution(ToolError::not_found(format!(
            "target element not found: '{target_selector}'"
        )))),
        _ => Ok(json!({
            "status": "dragged",
            "source": source_selector,
            "target": target_selector
        })),
    }
}

/// Wait for an element to appear.
async fn act_wait_for_element(
    page: &Page,
    element_ref: &ElementRef,
    timeout_ms: u64,
) -> Result<Value> {
    let selector = &element_ref.selector;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut last_probe_timed_out = false;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(json!({
                "status": "timeout",
                "outcome": "not_applied",
                "selector": selector,
                "timeout_ms": timeout_ms,
                "browser_degraded": last_probe_timed_out,
                "retry_safe": true,
                "reason": if last_probe_timed_out {
                    "the renderer did not answer the final element probe before the deadline"
                } else {
                    "the element did not appear before the deadline"
                }
            }));
        }
        let probe_budget = ELEMENT_OP_BUDGET.min(deadline - now);
        let found = if requires_document_resolver(element_ref) {
            tokio::time::timeout(
                probe_budget,
                evaluate_in_element_context::<bool>(
                    page,
                    element_ref,
                    format!("Boolean({})", resolver_expression(selector)),
                ),
            )
            .await
            .map(|result| result.map(|found| found.then_some(())))
        } else {
            tokio::time::timeout(probe_budget, page.find_element(selector))
                .await
                .map(|result| {
                    result.map(|_| Some(())).map_err(|error| {
                        Error::ToolExecution(ToolError::not_found(error.to_string()))
                    })
                })
        };
        match found {
            Ok(Ok(Some(_))) => {
                return Ok(json!({
                    "status": "found",
                    "outcome": "applied",
                    "selector": selector
                }));
            }
            Ok(Ok(None)) | Ok(Err(_)) => {
                last_probe_timed_out = false;
                if tokio::time::Instant::now() >= deadline {
                    return Ok(json!({
                        "status": "timeout",
                        "outcome": "not_applied",
                        "selector": selector,
                        "timeout_ms": timeout_ms,
                        "browser_degraded": false,
                        "retry_safe": true,
                        "reason": "the element did not appear before the deadline"
                    }));
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(_) => {
                last_probe_timed_out = true;
                if tokio::time::Instant::now() >= deadline {
                    return Ok(json!({
                        "status": "timeout",
                        "outcome": "not_applied",
                        "selector": selector,
                        "timeout_ms": timeout_ms,
                        "browser_degraded": true,
                        "retry_safe": true,
                        "reason": "the renderer did not answer the final element probe before the deadline"
                    }));
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_success_shapes_receive_an_explicit_outcome_contract() {
        let value = normalize_outcome(json!({ "status": "typed", "length": 4 }));
        assert_eq!(value["outcome"], "applied");
        assert_eq!(value["browser_degraded"], false);
        assert_eq!(value["retry_safe"], false);
    }

    #[test]
    fn normalization_preserves_failure_specific_retry_and_health_fields() {
        let value = normalize_outcome(json!({
            "status": "timeout",
            "outcome": "not_applied",
            "browser_degraded": true,
            "retry_safe": true,
        }));
        assert_eq!(value["outcome"], "not_applied");
        assert_eq!(value["browser_degraded"], true);
        assert_eq!(value["retry_safe"], true);
    }
}
