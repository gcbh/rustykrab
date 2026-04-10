//! Ref-based action system modeled after OpenClaw's `act` command.
//!
//! Actions use element refs from snapshots instead of raw CSS selectors.
//! Supported actions: click, type, press, hover, select, fill, scroll,
//! wait, evaluate.

use chromiumoxide::Page;
use rustykrab_core::{Error, Result};
use serde_json::{json, Value};

use super::snapshot::SnapshotStore;

/// Execute a ref-based action on the page.
///
/// The `ref_id` comes from a previous snapshot. The action is performed on
/// the element identified by that ref's stored CSS selector.
pub async fn execute_act(
    page: &Page,
    store: &SnapshotStore,
    store_key: &str,
    action: &str,
    ref_id: &str,
    args: &Value,
) -> Result<Value> {
    let element_ref = store.get_ref(store_key, ref_id).await.ok_or_else(|| {
        Error::ToolExecution(
            format!(
                "ref '{ref_id}' not found. Take a new snapshot first to get current element refs."
            )
            .into(),
        )
    })?;

    let selector = &element_ref.selector;

    match action {
        "click" => act_click(page, selector).await,
        "type" | "fill" => {
            let text = args["text"]
                .as_str()
                .ok_or_else(|| Error::ToolExecution("'type' action requires 'text' parameter".into()))?;
            let clear = args["clear"].as_bool().unwrap_or(true); // fill clears by default
            act_type(page, selector, text, clear).await
        }
        "press" => {
            let key = args["key"]
                .as_str()
                .ok_or_else(|| Error::ToolExecution("'press' action requires 'key' parameter".into()))?;
            act_press(page, selector, key).await
        }
        "hover" => act_hover(page, selector).await,
        "select" => {
            let value = args["value"]
                .as_str()
                .ok_or_else(|| Error::ToolExecution("'select' action requires 'value' parameter".into()))?;
            act_select(page, selector, value).await
        }
        "drag" => {
            let target_ref = args["targetRef"]
                .as_str()
                .ok_or_else(|| Error::ToolExecution("'drag' requires 'targetRef' parameter".into()))?;
            let target = store.get_ref(store_key, target_ref).await.ok_or_else(|| {
                Error::ToolExecution(format!("target ref '{target_ref}' not found").into())
            })?;
            act_drag(page, selector, &target.selector).await
        }
        "wait" => {
            let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);
            act_wait_for_element(page, selector, timeout_ms).await
        }
        _ => Err(Error::ToolExecution(
            format!(
                "unknown act action '{action}'. Available: click, type, fill, press, hover, select, drag, wait"
            )
            .into(),
        )),
    }
}

/// Click an element by CSS selector.
async fn act_click(page: &Page, selector: &str) -> Result<Value> {
    let elem = page.find_element(selector).await.map_err(|e| {
        Error::ToolExecution(format!("element not found '{selector}': {e}").into())
    })?;
    elem.click().await.map_err(|e| {
        Error::ToolExecution(format!("click failed on '{selector}': {e}").into())
    })?;
    Ok(json!({ "status": "clicked", "selector": selector }))
}

/// Type text into an element, optionally clearing first.
async fn act_type(page: &Page, selector: &str, text: &str, clear: bool) -> Result<Value> {
    let elem = page.find_element(selector).await.map_err(|e| {
        Error::ToolExecution(format!("element not found '{selector}': {e}").into())
    })?;

    // Focus the element
    elem.click().await.map_err(|e| {
        Error::ToolExecution(format!("failed to focus '{selector}': {e}").into())
    })?;

    if clear {
        // Clear existing value via JS
        let clear_js = format!(
            "var el = document.querySelector('{}'); if(el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{bubbles: true}})); }}",
            selector.replace('\'', "\\'").replace('"', "\\\""),
        );
        let _ = page.evaluate(clear_js).await;
    }

    elem.type_str(text).await.map_err(|e| {
        Error::ToolExecution(format!("typing failed on '{selector}': {e}").into())
    })?;

    Ok(json!({
        "status": "typed",
        "selector": selector,
        "length": text.len(),
        "cleared": clear
    }))
}

/// Press a key on an element (e.g., "Enter", "Tab", "Escape").
async fn act_press(page: &Page, selector: &str, key: &str) -> Result<Value> {
    let js = format!(
        r#"(function() {{
            var el = document.querySelector('{}');
            if (!el) return 'element_not_found';
            el.focus();
            var event = new KeyboardEvent('keydown', {{
                key: '{}',
                code: '{}',
                bubbles: true,
                cancelable: true
            }});
            el.dispatchEvent(event);
            var up = new KeyboardEvent('keyup', {{
                key: '{}',
                code: '{}',
                bubbles: true,
                cancelable: true
            }});
            el.dispatchEvent(up);
            return 'pressed';
        }})()"#,
        selector.replace('\'', "\\'").replace('"', "\\\""),
        key, key, key, key,
    );

    let result = page.evaluate(js).await.map_err(|e| {
        Error::ToolExecution(format!("press failed: {e}").into())
    })?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    if status == "element_not_found" {
        return Err(Error::ToolExecution(
            format!("element not found: '{selector}'").into(),
        ));
    }

    Ok(json!({ "status": "pressed", "key": key, "selector": selector }))
}

/// Hover over an element.
async fn act_hover(page: &Page, selector: &str) -> Result<Value> {
    let js = format!(
        r#"(function() {{
            var el = document.querySelector('{}');
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
        }})()"#,
        selector.replace('\'', "\\'").replace('"', "\\\""),
    );

    let result = page.evaluate(js).await.map_err(|e| {
        Error::ToolExecution(format!("hover failed: {e}").into())
    })?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    if status == "element_not_found" {
        return Err(Error::ToolExecution(
            format!("element not found: '{selector}'").into(),
        ));
    }

    Ok(json!({ "status": "hovered", "selector": selector }))
}

/// Select an option in a dropdown.
async fn act_select(page: &Page, selector: &str, value: &str) -> Result<Value> {
    let js = format!(
        r#"(function() {{
            var el = document.querySelector('{}');
            if (!el) return 'element_not_found';
            el.value = '{}';
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            return 'selected';
        }})()"#,
        selector.replace('\'', "\\'").replace('"', "\\\""),
        value.replace('\'', "\\'").replace('"', "\\\""),
    );

    let result = page.evaluate(js).await.map_err(|e| {
        Error::ToolExecution(format!("select failed: {e}").into())
    })?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    if status == "element_not_found" {
        return Err(Error::ToolExecution(
            format!("element not found: '{selector}'").into(),
        ));
    }

    Ok(json!({ "status": "selected", "selector": selector, "value": value }))
}

/// Drag one element to another.
async fn act_drag(page: &Page, source_selector: &str, target_selector: &str) -> Result<Value> {
    let js = format!(
        r#"(function() {{
            var src = document.querySelector('{}');
            var tgt = document.querySelector('{}');
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
        }})()"#,
        source_selector.replace('\'', "\\'").replace('"', "\\\""),
        target_selector.replace('\'', "\\'").replace('"', "\\\""),
    );

    let result = page.evaluate(js).await.map_err(|e| {
        Error::ToolExecution(format!("drag failed: {e}").into())
    })?;

    let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());
    match status.as_str() {
        "source_not_found" => Err(Error::ToolExecution(
            format!("source element not found: '{source_selector}'").into(),
        )),
        "target_not_found" => Err(Error::ToolExecution(
            format!("target element not found: '{target_selector}'").into(),
        )),
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
