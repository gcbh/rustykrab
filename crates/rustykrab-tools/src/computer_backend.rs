//! Backend abstraction for "computer use": capturing the screen and
//! synthesizing mouse/keyboard input.
//!
//! The trait lives here (dependency-light) so the [`crate::ComputerTool`] can
//! be built and unit-tested anywhere. Concrete implementations that pull in
//! platform/GUI dependencies (e.g. an `enigo` + `xcap` backend) live in the
//! crate that owns those dependencies — typically the binary — and are bridged
//! in via an adapter, mirroring the `CronBackend` / `MessageBackend` pattern.

use async_trait::async_trait;
use rustykrab_core::Result;

/// A screenshot captured from the display.
#[derive(Debug, Clone)]
pub struct CapturedImage {
    /// PNG-encoded image bytes.
    pub png: Vec<u8>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Mouse button for click/drag actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Direction for a scroll action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    /// Parse a direction from its lowercase string form.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// Abstract interface for controlling a desktop session: capturing the screen
/// and synthesizing pointer/keyboard input. All coordinates are absolute
/// display pixels with the origin at the top-left.
#[async_trait]
pub trait ComputerBackend: Send + Sync {
    /// Capture the current screen as a PNG.
    async fn screenshot(&self) -> Result<CapturedImage>;

    /// Move the mouse cursor to absolute coordinates.
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;

    /// Click a mouse button. When `at` is provided, move there first.
    async fn click(&self, button: MouseButton, at: Option<(i32, i32)>) -> Result<()>;

    /// Double-click the left button. When `at` is provided, move there first.
    async fn double_click(&self, at: Option<(i32, i32)>) -> Result<()>;

    /// Press the left button at `from`, move to `to`, then release.
    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<()>;

    /// Type a string of text at the current focus.
    async fn type_text(&self, text: &str) -> Result<()>;

    /// Press a key combination such as `"ctrl+s"`, `"Return"`, or `"alt+Tab"`.
    async fn key(&self, combo: &str) -> Result<()>;

    /// Scroll in a direction by `amount` wheel steps. When `at` is provided,
    /// move the cursor there first.
    async fn scroll(
        &self,
        direction: ScrollDirection,
        amount: i32,
        at: Option<(i32, i32)>,
    ) -> Result<()>;

    /// Return the current cursor position as `(x, y)`.
    async fn cursor_position(&self) -> Result<(i32, i32)>;

    /// Display dimensions in pixels as `(width, height)`. Reported to the
    /// model so it can reason about coordinate bounds.
    fn display_size(&self) -> (u32, u32);
}
