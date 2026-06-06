//! Concrete computer-use backend built on `enigo` (input synthesis) and
//! `xcap` (screen capture).
//!
//! Compiled only with the `computer-use` cargo feature, which pulls in the
//! X11/Wayland system dependencies. `enigo`'s `Enigo` handle is not `Send`,
//! so all platform calls run on a single dedicated OS thread that owns the
//! handle; the async [`ComputerBackend`] methods talk to it over a channel,
//! which keeps the backend `Send + Sync` as the trait requires.

use std::sync::mpsc::Sender;

use async_trait::async_trait;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use tokio::sync::oneshot;
use xcap::Monitor;

use rustykrab_core::Result;
use rustykrab_tools::{CapturedImage, ComputerBackend, MouseButton, ScrollDirection};

fn err(msg: impl Into<String>) -> rustykrab_core::Error {
    rustykrab_core::Error::ToolExecution(msg.into().into())
}

/// Commands sent to the worker thread. Each carries a oneshot channel for the
/// reply so the async caller can await the (blocking) platform operation.
enum Command {
    Screenshot(oneshot::Sender<Result<CapturedImage>>),
    MouseMove {
        x: i32,
        y: i32,
        reply: oneshot::Sender<Result<()>>,
    },
    Click {
        button: MouseButton,
        at: Option<(i32, i32)>,
        reply: oneshot::Sender<Result<()>>,
    },
    DoubleClick {
        at: Option<(i32, i32)>,
        reply: oneshot::Sender<Result<()>>,
    },
    Drag {
        from: (i32, i32),
        to: (i32, i32),
        reply: oneshot::Sender<Result<()>>,
    },
    Type {
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Key {
        combo: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Scroll {
        direction: ScrollDirection,
        amount: i32,
        at: Option<(i32, i32)>,
        reply: oneshot::Sender<Result<()>>,
    },
    CursorPosition(oneshot::Sender<Result<(i32, i32)>>),
}

/// `enigo` + `xcap` implementation of [`ComputerBackend`].
pub struct EnigoXcapBackend {
    tx: Sender<Command>,
    display: (u32, u32),
}

impl EnigoXcapBackend {
    /// Spawn the worker thread and probe the primary display. Returns an
    /// error if `enigo` can't initialize (e.g. no display available).
    pub fn new() -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<Command>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(u32, u32)>>();

        std::thread::Builder::new()
            .name("computer-use".to_string())
            .spawn(move || {
                let mut enigo = match Enigo::new(&Settings::default()) {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = init_tx.send(Err(err(format!("enigo init failed: {e}"))));
                        return;
                    }
                };
                let display = match enigo.main_display() {
                    Ok((w, h)) => (w.max(0) as u32, h.max(0) as u32),
                    Err(e) => {
                        let _ = init_tx.send(Err(err(format!("display query failed: {e}"))));
                        return;
                    }
                };
                if init_tx.send(Ok(display)).is_err() {
                    return; // creator gave up
                }
                for cmd in rx.iter() {
                    handle(&mut enigo, cmd);
                }
            })
            .map_err(|e| err(format!("failed to spawn computer-use thread: {e}")))?;

        let display = init_rx
            .recv()
            .map_err(|_| err("computer-use thread exited during init"))??;
        Ok(Self { tx, display })
    }

    async fn dispatch<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(make(rtx))
            .map_err(|_| err("computer-use thread is not running"))?;
        rrx.await
            .map_err(|_| err("computer-use thread dropped the reply"))?
    }
}

#[async_trait]
impl ComputerBackend for EnigoXcapBackend {
    async fn screenshot(&self) -> Result<CapturedImage> {
        self.dispatch(Command::Screenshot).await
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.dispatch(|reply| Command::MouseMove { x, y, reply }).await
    }

    async fn click(&self, button: MouseButton, at: Option<(i32, i32)>) -> Result<()> {
        self.dispatch(|reply| Command::Click { button, at, reply })
            .await
    }

    async fn double_click(&self, at: Option<(i32, i32)>) -> Result<()> {
        self.dispatch(|reply| Command::DoubleClick { at, reply }).await
    }

    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.dispatch(|reply| Command::Drag { from, to, reply }).await
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        let text = text.to_string();
        self.dispatch(|reply| Command::Type { text, reply }).await
    }

    async fn key(&self, combo: &str) -> Result<()> {
        let combo = combo.to_string();
        self.dispatch(|reply| Command::Key { combo, reply }).await
    }

    async fn scroll(
        &self,
        direction: ScrollDirection,
        amount: i32,
        at: Option<(i32, i32)>,
    ) -> Result<()> {
        self.dispatch(|reply| Command::Scroll {
            direction,
            amount,
            at,
            reply,
        })
        .await
    }

    async fn cursor_position(&self) -> Result<(i32, i32)> {
        self.dispatch(Command::CursorPosition).await
    }

    fn display_size(&self) -> (u32, u32) {
        self.display
    }
}

/// Map an `enigo` input error into our error type.
fn emap(e: enigo::InputError) -> rustykrab_core::Error {
    err(format!("input error: {e}"))
}

fn map_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

/// Resolve a single token (e.g. `"ctrl"`, `"Return"`, `"a"`) to an `enigo`
/// key. Single characters map to `Key::Unicode`.
fn key_from_name(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "cmd" | "command" | "super" | "meta" | "win" | "windows" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // multi-char, unrecognized name
            }
            Key::Unicode(c)
        }
    })
}

/// Press a key combination like `"ctrl+shift+s"`: all but the last token are
/// held as modifiers around a click of the final key.
fn press_combo(enigo: &mut Enigo, combo: &str) -> Result<()> {
    let tokens: Vec<&str> = combo
        .split('+')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err(err("empty key combo"));
    }
    let keys: Vec<Key> = tokens
        .iter()
        .map(|t| key_from_name(t).ok_or_else(|| err(format!("unknown key: {t}"))))
        .collect::<Result<_>>()?;

    let (main_key, mods) = keys.split_last().expect("non-empty");
    for m in mods {
        enigo.key(*m, Direction::Press).map_err(emap)?;
    }
    let click = enigo.key(*main_key, Direction::Click).map_err(emap);
    // Always release modifiers, even if the main click failed.
    for m in mods.iter().rev() {
        let _ = enigo.key(*m, Direction::Release);
    }
    click
}

fn capture_primary() -> Result<CapturedImage> {
    let monitors = Monitor::all().map_err(|e| err(format!("listing monitors: {e}")))?;
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| err("no monitors found"))?;
    let img = monitor
        .capture_image()
        .map_err(|e| err(format!("capturing screen: {e}")))?;
    let (width, height) = (img.width(), img.height());
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, xcap::image::ImageFormat::Png)
        .map_err(|e| err(format!("encoding PNG: {e}")))?;
    Ok(CapturedImage {
        png: buf.into_inner(),
        width,
        height,
    })
}

/// Execute one command on the worker thread, sending the result back.
fn handle(enigo: &mut Enigo, cmd: Command) {
    match cmd {
        Command::Screenshot(reply) => {
            let _ = reply.send(capture_primary());
        }
        Command::MouseMove { x, y, reply } => {
            let _ = reply.send(enigo.move_mouse(x, y, Coordinate::Abs).map_err(emap));
        }
        Command::Click { button, at, reply } => {
            let r = (|| {
                if let Some((x, y)) = at {
                    enigo.move_mouse(x, y, Coordinate::Abs).map_err(emap)?;
                }
                enigo.button(map_button(button), Direction::Click).map_err(emap)
            })();
            let _ = reply.send(r);
        }
        Command::DoubleClick { at, reply } => {
            let r = (|| {
                if let Some((x, y)) = at {
                    enigo.move_mouse(x, y, Coordinate::Abs).map_err(emap)?;
                }
                enigo.button(Button::Left, Direction::Click).map_err(emap)?;
                enigo.button(Button::Left, Direction::Click).map_err(emap)
            })();
            let _ = reply.send(r);
        }
        Command::Drag { from, to, reply } => {
            let r = (|| {
                enigo.move_mouse(from.0, from.1, Coordinate::Abs).map_err(emap)?;
                enigo.button(Button::Left, Direction::Press).map_err(emap)?;
                enigo.move_mouse(to.0, to.1, Coordinate::Abs).map_err(emap)?;
                enigo.button(Button::Left, Direction::Release).map_err(emap)
            })();
            let _ = reply.send(r);
        }
        Command::Type { text, reply } => {
            let _ = reply.send(enigo.text(&text).map_err(emap));
        }
        Command::Key { combo, reply } => {
            let _ = reply.send(press_combo(enigo, &combo));
        }
        Command::Scroll {
            direction,
            amount,
            at,
            reply,
        } => {
            let r = (|| {
                if let Some((x, y)) = at {
                    enigo.move_mouse(x, y, Coordinate::Abs).map_err(emap)?;
                }
                // enigo: positive length scrolls down / right.
                let (length, axis) = match direction {
                    ScrollDirection::Down => (amount, Axis::Vertical),
                    ScrollDirection::Up => (-amount, Axis::Vertical),
                    ScrollDirection::Right => (amount, Axis::Horizontal),
                    ScrollDirection::Left => (-amount, Axis::Horizontal),
                };
                enigo.scroll(length, axis).map_err(emap)
            })();
            let _ = reply.send(r);
        }
        Command::CursorPosition(reply) => {
            let _ = reply.send(enigo.location().map_err(emap));
        }
    }
}
