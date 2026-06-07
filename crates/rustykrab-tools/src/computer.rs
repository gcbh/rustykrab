//! A tool that lets the agent drive a desktop: take screenshots and
//! synthesize mouse/keyboard input.
//!
//! Actions follow Anthropic's computer-use vocabulary (`screenshot`,
//! `left_click`, `type`, `key`, `scroll`, …) so the same backend can later be
//! wired to the native `computer_*` tool type. Screenshots are returned via
//! the reserved `_images` key, which the agent runner extracts into the
//! tool result's image blocks for vision-capable models.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::computer_backend::{ComputerBackend, MouseButton, ScrollDirection};

/// Tool exposing screen capture and input synthesis to the agent.
pub struct ComputerTool {
    backend: Arc<dyn ComputerBackend>,
}

impl ComputerTool {
    pub fn new(backend: Arc<dyn ComputerBackend>) -> Self {
        Self { backend }
    }

    /// Construct a `ToolExecution` error with a message.
    fn err(msg: impl Into<String>) -> rustykrab_core::Error {
        rustykrab_core::Error::ToolExecution(msg.into().into())
    }

    /// Parse coordinates from either a `coordinate: [x, y]` array (Anthropic
    /// style) or separate `x` / `y` integer fields. Returns `None` when
    /// neither form is present or well-formed.
    fn parse_coords(args: &Value, key: &str) -> Option<(i32, i32)> {
        if let Some(arr) = args.get(key).and_then(|v| v.as_array()) {
            if arr.len() == 2 {
                if let (Some(x), Some(y)) = (arr[0].as_i64(), arr[1].as_i64()) {
                    return Some((x as i32, y as i32));
                }
            }
        }
        match (
            args.get("x").and_then(|v| v.as_i64()),
            args.get("y").and_then(|v| v.as_i64()),
        ) {
            (Some(x), Some(y)) => Some((x as i32, y as i32)),
            _ => None,
        }
    }

    fn require_coords(args: &Value, key: &str, action: &str) -> Result<(i32, i32)> {
        Self::parse_coords(args, key)
            .ok_or_else(|| Self::err(format!("action '{action}' requires a coordinate [x, y]")))
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "computer"
    }

    fn description(&self) -> &str {
        "Control the desktop: take a screenshot, move/click the mouse, type \
         text, press keys, or scroll. Coordinates are absolute display pixels \
         (origin top-left). Start with a `screenshot` to see the screen."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        // Driving the display is a system-level side effect.
        SandboxRequirements {
            needs_spawn: true,
            ..SandboxRequirements::default()
        }
    }

    fn schema(&self) -> ToolSchema {
        let (w, h) = self.backend.display_size();
        ToolSchema {
            name: self.name().to_string(),
            description: format!("{} The display is {w}x{h} pixels.", self.description()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "screenshot",
                            "cursor_position",
                            "mouse_move",
                            "left_click",
                            "right_click",
                            "middle_click",
                            "double_click",
                            "left_click_drag",
                            "type",
                            "key",
                            "scroll"
                        ],
                        "description": "The action to perform."
                    },
                    "coordinate": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Target [x, y] in display pixels. Required for mouse_move and left_click_drag (destination); optional for clicks and scroll."
                    },
                    "start_coordinate": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Drag start [x, y] for left_click_drag. Defaults to the current cursor position."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type (action 'type') or key combination such as 'ctrl+s' (action 'key')."
                    },
                    "scroll_direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"],
                        "description": "Scroll direction (action 'scroll')."
                    },
                    "scroll_amount": {
                        "type": "integer",
                        "description": "Number of wheel steps to scroll (action 'scroll'). Defaults to 3."
                    },
                    "grid": {
                        "type": "boolean",
                        "description": "For 'screenshot': overlay a labeled coordinate grid to read off precise [x, y] targets. Useful when clicks miss."
                    },
                    "grid_spacing": {
                        "type": "integer",
                        "description": "Grid line spacing in pixels when 'grid' is true. Defaults to 100."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Self::err("missing action"))?;

        match action {
            "screenshot" => {
                let img = self.backend.screenshot().await?;
                let grid = args["grid"].as_bool().unwrap_or(false);
                // Optionally overlay a labeled coordinate grid to help the
                // model read off pixel coordinates. A failed overlay (e.g. an
                // unexpected image format) must not fail the screenshot, so we
                // fall back to the raw capture.
                let mut png = img.png;
                if grid {
                    let spacing = args["grid_spacing"].as_u64().unwrap_or(100) as u32;
                    match crate::grid::annotate_with_grid(&png, spacing) {
                        Ok(annotated) => png = annotated,
                        Err(e) => {
                            tracing::warn!(error = %e, "grid overlay failed; returning raw screenshot")
                        }
                    }
                }
                Ok(json!({
                    "action": "screenshot",
                    "width": img.width,
                    "height": img.height,
                    "grid": grid,
                    "_images": [{
                        "media_type": "image/png",
                        "data": STANDARD.encode(&png),
                    }],
                }))
            }
            "cursor_position" => {
                let (x, y) = self.backend.cursor_position().await?;
                Ok(json!({ "action": "cursor_position", "x": x, "y": y }))
            }
            "mouse_move" => {
                let (x, y) = Self::require_coords(&args, "coordinate", action)?;
                self.backend.mouse_move(x, y).await?;
                Ok(json!({ "action": "mouse_move", "x": x, "y": y }))
            }
            "left_click" | "right_click" | "middle_click" => {
                let button = match action {
                    "left_click" => MouseButton::Left,
                    "right_click" => MouseButton::Right,
                    _ => MouseButton::Middle,
                };
                let at = Self::parse_coords(&args, "coordinate");
                self.backend.click(button, at).await?;
                Ok(json!({ "action": action, "coordinate": at.map(|(x, y)| [x, y]) }))
            }
            "double_click" => {
                let at = Self::parse_coords(&args, "coordinate");
                self.backend.double_click(at).await?;
                Ok(json!({ "action": "double_click", "coordinate": at.map(|(x, y)| [x, y]) }))
            }
            "left_click_drag" => {
                let to = Self::require_coords(&args, "coordinate", action)?;
                let from = match Self::parse_coords(&args, "start_coordinate") {
                    Some(p) => p,
                    None => self.backend.cursor_position().await?,
                };
                self.backend.drag(from, to).await?;
                Ok(json!({
                    "action": "left_click_drag",
                    "from": [from.0, from.1],
                    "to": [to.0, to.1],
                }))
            }
            "type" => {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| Self::err("action 'type' requires 'text'"))?;
                self.backend.type_text(text).await?;
                Ok(json!({ "action": "type", "typed": text.len() }))
            }
            "key" => {
                let combo = args["text"]
                    .as_str()
                    .ok_or_else(|| Self::err("action 'key' requires 'text' (the key combo)"))?;
                self.backend.key(combo).await?;
                Ok(json!({ "action": "key", "combo": combo }))
            }
            "scroll" => {
                let dir_str = args["scroll_direction"]
                    .as_str()
                    .ok_or_else(|| Self::err("action 'scroll' requires 'scroll_direction'"))?;
                let direction = ScrollDirection::parse(dir_str)
                    .ok_or_else(|| Self::err(format!("invalid scroll_direction: {dir_str}")))?;
                let amount = args["scroll_amount"].as_i64().unwrap_or(3) as i32;
                let at = Self::parse_coords(&args, "coordinate");
                self.backend.scroll(direction, amount, at).await?;
                Ok(json!({
                    "action": "scroll",
                    "scroll_direction": dir_str,
                    "scroll_amount": amount,
                }))
            }
            other => Err(Self::err(format!("unknown action: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer_backend::CapturedImage;
    use std::sync::Mutex;

    /// Records every backend call so tests can assert on dispatch + parsing.
    #[derive(Default)]
    struct MockBackend {
        calls: Mutex<Vec<String>>,
        cursor: (i32, i32),
    }

    #[async_trait]
    impl ComputerBackend for MockBackend {
        async fn screenshot(&self) -> Result<CapturedImage> {
            self.calls.lock().unwrap().push("screenshot".into());
            // A real (small) PNG so the grid-overlay path can decode it.
            let img = image::RgbaImage::from_pixel(200, 120, image::Rgba([30, 30, 30, 255]));
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
            Ok(CapturedImage {
                png: buf.into_inner(),
                width: 200,
                height: 120,
            })
        }
        async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("mouse_move({x},{y})"));
            Ok(())
        }
        async fn click(&self, button: MouseButton, at: Option<(i32, i32)>) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("click({button:?},{at:?})"));
            Ok(())
        }
        async fn double_click(&self, at: Option<(i32, i32)>) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("double_click({at:?})"));
            Ok(())
        }
        async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("drag({from:?},{to:?})"));
            Ok(())
        }
        async fn type_text(&self, text: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("type({text})"));
            Ok(())
        }
        async fn key(&self, combo: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("key({combo})"));
            Ok(())
        }
        async fn scroll(
            &self,
            direction: ScrollDirection,
            amount: i32,
            at: Option<(i32, i32)>,
        ) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("scroll({direction:?},{amount},{at:?})"));
            Ok(())
        }
        async fn cursor_position(&self) -> Result<(i32, i32)> {
            self.calls.lock().unwrap().push("cursor_position".into());
            Ok(self.cursor)
        }
        fn display_size(&self) -> (u32, u32) {
            (1920, 1080)
        }
    }

    fn tool() -> (ComputerTool, Arc<MockBackend>) {
        let backend = Arc::new(MockBackend::default());
        (ComputerTool::new(backend.clone()), backend)
    }

    fn calls(b: &MockBackend) -> Vec<String> {
        b.calls.lock().unwrap().clone()
    }

    fn decode_image_data(out: &Value) -> Vec<u8> {
        let imgs = out["_images"].as_array().expect("_images array");
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0]["media_type"], "image/png");
        STANDARD
            .decode(imgs[0]["data"].as_str().expect("base64 data"))
            .expect("valid base64")
    }

    #[tokio::test]
    async fn screenshot_returns_image_via_images_key() {
        let (t, _b) = tool();
        let out = t.execute(json!({ "action": "screenshot" })).await.unwrap();
        assert_eq!(out["width"], 200);
        assert_eq!(out["height"], 120);
        assert_eq!(out["grid"], false);
        let png = decode_image_data(&out);
        let dims = image::load_from_memory(&png)
            .unwrap()
            .to_rgba8()
            .dimensions();
        assert_eq!(dims, (200, 120));
    }

    #[tokio::test]
    async fn screenshot_with_grid_overlays_and_differs() {
        let (t, _b) = tool();
        let plain = decode_image_data(&t.execute(json!({ "action": "screenshot" })).await.unwrap());
        let out = t
            .execute(json!({ "action": "screenshot", "grid": true, "grid_spacing": 50 }))
            .await
            .unwrap();
        assert_eq!(out["grid"], true);
        let gridded = decode_image_data(&out);
        // Same dimensions, but pixels changed by the overlay.
        let g = image::load_from_memory(&gridded).unwrap().to_rgba8();
        assert_eq!(g.dimensions(), (200, 120));
        assert_ne!(plain, gridded, "grid overlay should change the image");
    }

    #[tokio::test]
    async fn left_click_with_coordinate_array() {
        let (t, b) = tool();
        t.execute(json!({ "action": "left_click", "coordinate": [10, 20] }))
            .await
            .unwrap();
        assert_eq!(calls(&b), vec!["click(Left,Some((10, 20)))"]);
    }

    #[tokio::test]
    async fn click_accepts_separate_x_y_fields() {
        let (t, b) = tool();
        t.execute(json!({ "action": "right_click", "x": 5, "y": 6 }))
            .await
            .unwrap();
        assert_eq!(calls(&b), vec!["click(Right,Some((5, 6)))"]);
    }

    #[tokio::test]
    async fn mouse_move_requires_coordinate() {
        let (t, _b) = tool();
        let err = t
            .execute(json!({ "action": "mouse_move" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("coordinate"));
    }

    #[tokio::test]
    async fn drag_defaults_start_to_cursor_position() {
        let (t, b) = tool();
        t.execute(json!({ "action": "left_click_drag", "coordinate": [100, 200] }))
            .await
            .unwrap();
        // Falls back to cursor_position() (mock returns (0,0)) for the start.
        assert_eq!(
            calls(&b),
            vec![
                "cursor_position".to_string(),
                "drag((0, 0),(100, 200))".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn type_and_key_dispatch() {
        let (t, b) = tool();
        t.execute(json!({ "action": "type", "text": "hello" }))
            .await
            .unwrap();
        t.execute(json!({ "action": "key", "text": "ctrl+s" }))
            .await
            .unwrap();
        assert_eq!(calls(&b), vec!["type(hello)", "key(ctrl+s)"]);
    }

    #[tokio::test]
    async fn scroll_parses_direction_and_default_amount() {
        let (t, b) = tool();
        t.execute(json!({ "action": "scroll", "scroll_direction": "Down" }))
            .await
            .unwrap();
        assert_eq!(calls(&b), vec!["scroll(Down,3,None)"]);
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let (t, _b) = tool();
        let err = t
            .execute(json!({ "action": "teleport" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown action"));
    }

    #[test]
    fn schema_includes_display_dimensions() {
        let (t, _b) = tool();
        assert!(t.schema().description.contains("1920x1080"));
    }
}
