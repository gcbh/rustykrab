//! Ref-based action system modeled after OpenClaw's `act` command.
//!
//! Actions use element refs from snapshots instead of raw CSS selectors.
//! Supported actions: click, type, press, hover, select, fill, scroll,
//! wait, evaluate.

use chromiumoxide::Page;
use rustykrab_core::{Error, Result, ToolError, ToolErrorKind};
use serde_json::{json, Value};

use super::snapshot::{take_snapshot, ElementRef, SnapshotOptions, SnapshotStore};

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
///   match (ambiguous), return a `stale_ref` payload carrying a fresh snapshot
///   so the model can re-pick a ref in the same turn.
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
                "ref not found in the latest snapshot for this tab",
            )
            .await);
        }
    };

    let url_before = current_url(page).await;

    match dispatch_act(page, store, store_key, action, &element_ref.selector, args).await {
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

/// Run a single ref-based action against an explicit CSS selector.
async fn dispatch_act(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    action: &str,
    selector: &str,
    args: &Value,
) -> Result<Value> {
    match action {
        "click" => act_click(page, selector).await,
        "type" | "fill" => {
            let text = args["text"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'type' action requires 'text' parameter",
                ))
            })?;
            let clear = args["clear"].as_bool().unwrap_or(true); // fill clears by default
            act_type(page, selector, text, clear).await
        }
        "press" => {
            let key = args["key"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'press' action requires 'key' parameter",
                ))
            })?;
            act_press(page, selector, key).await
        }
        "hover" => act_hover(page, selector).await,
        "select" => {
            let value = args["value"].as_str().ok_or_else(|| {
                Error::ToolExecution(ToolError::invalid_input(
                    "'select' action requires 'value' parameter",
                ))
            })?;
            act_select(page, selector, value).await
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
            act_drag(page, selector, &target.selector).await
        }
        "wait" => {
            let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);
            act_wait_for_element(page, selector, timeout_ms).await
        }
        _ => Err(Error::ToolExecution(ToolError::invalid_input(format!(
            "unknown act action '{action}'. Available: click, type, fill, press, hover, select, drag, wait"
        )))),
    }
}

/// True when the error is a stale-ref failure (the element wasn't found before
/// the action ran), as opposed to a genuine failure mid-action.
fn is_stale_element(e: &Error) -> bool {
    matches!(e, Error::ToolExecution(te) if te.kind == ToolErrorKind::NotFound)
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
        return Ok(escalation_payload(
            action,
            ref_id,
            "the tab navigated to a different URL since the snapshot",
            snapshot,
            url_after.as_deref(),
        ));
    }

    // Guard 2 — unique identity. Heal only when exactly one element still
    // matches the stale ref's role+name; none or several means escalate.
    let matches = store
        .find_by_identity(store_key, &stale.role, &stale.name)
        .await;
    match matches.as_slice() {
        [only] => match dispatch_act(page, store, store_key, action, &only.selector, args).await {
            Ok(mut v) => {
                if let Value::Object(ref mut o) = v {
                    o.insert("recovered".into(), Value::Bool(true));
                }
                Ok(v)
            }
            Err(_) => Ok(escalation_payload(
                action,
                ref_id,
                "the re-resolved element could still not be actioned",
                snapshot,
                url_after.as_deref(),
            )),
        },
        [] => Ok(escalation_payload(
            action,
            ref_id,
            "the element is no longer present after the page changed",
            snapshot,
            url_after.as_deref(),
        )),
        _ => Ok(escalation_payload(
            action,
            ref_id,
            "several elements now match the same role/name — ambiguous, cannot auto-recover",
            snapshot,
            url_after.as_deref(),
        )),
    }
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
    escalation_payload(action, ref_id, reason, snapshot, url.as_deref())
}

/// Build the `stale_ref` payload returned to the model. Returned as `Ok` (not
/// `Err`) on purpose: the runner blindly retries failed tool calls with the
/// same arguments, which is pointless for a stale ref. As an `Ok` payload the
/// model gets the fresh snapshot in hand and can re-pick a ref in one turn.
fn escalation_payload(
    action: &str,
    ref_id: &str,
    reason: &str,
    snapshot: Option<Value>,
    url: Option<&str>,
) -> Value {
    json!({
        "status": "stale_ref",
        "action_performed": false,
        "action": action,
        "ref": ref_id,
        "reason": reason,
        "url": url,
        "message": format!(
            "Could not '{action}' ref {ref_id}: {reason}. The refs from the previous \
             snapshot are no longer valid. A fresh snapshot is included under \"snapshot\" \
             — pick a new ref from it and call act again. Do not reuse the old ref."
        ),
        "snapshot": snapshot,
    })
}

/// Click an element by CSS selector.
async fn act_click(page: &Page, selector: &str) -> Result<Value> {
    let elem = page.find_element(selector).await.map_err(|e| {
        Error::ToolExecution(ToolError::not_found(format!(
            "element not found '{selector}': {e}"
        )))
    })?;
    elem.click()
        .await
        .map_err(|e| Error::ToolExecution(format!("click failed on '{selector}': {e}").into()))?;
    Ok(json!({ "status": "clicked", "selector": selector }))
}

/// Type text into an element, optionally clearing first.
async fn act_type(page: &Page, selector: &str, text: &str, clear: bool) -> Result<Value> {
    let elem = page.find_element(selector).await.map_err(|e| {
        Error::ToolExecution(ToolError::not_found(format!(
            "element not found '{selector}': {e}"
        )))
    })?;

    // Focus the element
    elem.click()
        .await
        .map_err(|e| Error::ToolExecution(format!("failed to focus '{selector}': {e}").into()))?;

    if clear {
        // Clear existing value via JS
        let sel_lit = js_string_literal(selector);
        let clear_js = format!(
            "var el = document.querySelector({sel_lit}); if(el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{bubbles: true}})); }}"
        );
        let _ = page.evaluate(clear_js).await;
    }

    elem.type_str(text)
        .await
        .map_err(|e| Error::ToolExecution(format!("typing failed on '{selector}': {e}").into()))?;

    Ok(json!({
        "status": "typed",
        "selector": selector,
        "length": text.len(),
        "cleared": clear
    }))
}

/// Press a key on an element (e.g., "Enter", "Tab", "Escape").
async fn act_press(page: &Page, selector: &str, key: &str) -> Result<Value> {
    let sel_lit = js_string_literal(selector);
    let key_lit = js_string_literal(key);
    let js = format!(
        r#"(function() {{
            var el = document.querySelector({sel_lit});
            if (!el) return 'element_not_found';
            el.focus();
            var event = new KeyboardEvent('keydown', {{
                key: {key_lit},
                code: {key_lit},
                bubbles: true,
                cancelable: true
            }});
            el.dispatchEvent(event);
            var up = new KeyboardEvent('keyup', {{
                key: {key_lit},
                code: {key_lit},
                bubbles: true,
                cancelable: true
            }});
            el.dispatchEvent(up);
            return 'pressed';
        }})()"#
    );

    let result = page
        .evaluate(js)
        .await
        .map_err(|e| Error::ToolExecution(format!("press failed: {e}").into()))?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    if status == "element_not_found" {
        return Err(Error::ToolExecution(ToolError::not_found(format!(
            "element not found: '{selector}'"
        ))));
    }

    Ok(json!({ "status": "pressed", "key": key, "selector": selector }))
}

/// Hover over an element.
async fn act_hover(page: &Page, selector: &str) -> Result<Value> {
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
async fn act_select(page: &Page, selector: &str, value: &str) -> Result<Value> {
    let sel_lit = js_string_literal(selector);
    let val_lit = js_string_literal(value);
    let js = format!(
        r#"(function() {{
            var el = document.querySelector({sel_lit});
            if (!el) return 'element_not_found';
            el.value = {val_lit};
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            return 'selected';
        }})()"#
    );

    let result = page
        .evaluate(js)
        .await
        .map_err(|e| Error::ToolExecution(format!("select failed: {e}").into()))?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    if status == "element_not_found" {
        return Err(Error::ToolExecution(ToolError::not_found(format!(
            "element not found: '{selector}'"
        ))));
    }

    Ok(json!({ "status": "selected", "selector": selector, "value": value }))
}

/// Drag one element to another.
async fn act_drag(page: &Page, source_selector: &str, target_selector: &str) -> Result<Value> {
    let src_lit = js_string_literal(source_selector);
    let tgt_lit = js_string_literal(target_selector);
    let js = format!(
        r#"(function() {{
            var src = document.querySelector({src_lit});
            var tgt = document.querySelector({tgt_lit});
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

    let result = page
        .evaluate(js)
        .await
        .map_err(|e| Error::ToolExecution(format!("drag failed: {e}").into()))?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
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
async fn act_wait_for_element(page: &Page, selector: &str, timeout_ms: u64) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match page.find_element(selector).await {
            Ok(_) => {
                return Ok(json!({
                    "status": "found",
                    "selector": selector
                }));
            }
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    return Ok(json!({
                        "status": "timeout",
                        "selector": selector,
                        "timeout_ms": timeout_ms
                    }));
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
}
