//! Video communication channel powered by Hyperframes via MCP.
//!
//! Treats video as a first-class mode of communication: the agent can
//! respond to users by composing HTML-based video compositions and rendering
//! them to MP4 files. Internally manages the hyperframes MCP server process
//! and provides the full pipeline: compose → preview → render.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing;
use uuid::Uuid;

use crate::mcp::{McpClient, McpToolDef};

/// Configuration for the video channel.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    /// Working directory for video projects.
    pub projects_dir: PathBuf,
    /// Path to the `npx` binary (defaults to "npx").
    pub npx_path: String,
    /// Additional environment variables for the MCP server process.
    pub env: Vec<(String, String)>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rustykrab")
            .join("video");

        Self {
            projects_dir: data_dir,
            npx_path: "npx".to_string(),
            env: Vec::new(),
        }
    }
}

/// A video composition project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoProject {
    /// Unique project ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Directory on disk where composition files live.
    pub dir: PathBuf,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Duration in seconds.
    pub duration: f64,
    /// Frames per second.
    pub fps: u32,
}

/// Metadata for a rendered video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderResult {
    /// Path to the rendered MP4 file.
    pub path: PathBuf,
    /// Duration of the rendered video in seconds.
    pub duration: f64,
    /// File size in bytes.
    pub size: u64,
    /// Format (always "mp4" for now).
    pub format: String,
}

/// An element in a video composition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionElement {
    /// Element type: "text", "image", "video", "audio", "shape".
    #[serde(rename = "type")]
    pub element_type: String,
    /// Unique element ID within the composition.
    pub id: String,
    /// Start time in seconds on the timeline.
    pub start: f64,
    /// Duration in seconds.
    pub duration: f64,
    /// Track/layer index (0 = bottom).
    pub track: u32,
    /// Element-specific properties (src, text, color, font, etc.).
    #[serde(default)]
    pub properties: Value,
}

/// The video communication channel.
///
/// Manages the hyperframes MCP server lifecycle and provides methods for
/// composing and rendering video content. Like Telegram sends text or
/// Signal sends encrypted messages, VideoChannel communicates via video.
pub struct VideoChannel {
    config: VideoConfig,
    /// MCP client connection to the hyperframes server.
    mcp: Arc<Mutex<Option<McpClient>>>,
    /// Available MCP tools (cached after first connection).
    available_tools: Mutex<Vec<McpToolDef>>,
}

impl VideoChannel {
    /// Create a new video channel with the given configuration.
    pub fn new(config: VideoConfig) -> Self {
        Self {
            config,
            mcp: Arc::new(Mutex::new(None)),
            available_tools: Mutex::new(Vec::new()),
        }
    }

    /// Create a new video channel with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(VideoConfig::default())
    }

    pub fn name(&self) -> &str {
        "video"
    }

    /// Ensure the MCP server is running and connected.
    /// Lazily starts the server on first use.
    pub async fn ensure_connected(&self) -> Result<(), String> {
        let mut mcp_guard = self.mcp.lock().await;

        // Check if already connected and alive.
        if let Some(ref client) = *mcp_guard {
            if client.is_alive().await {
                return Ok(());
            }
            tracing::warn!("hyperframes MCP server died, restarting");
        }

        // Ensure projects directory exists.
        std::fs::create_dir_all(&self.config.projects_dir)
            .map_err(|e| format!("failed to create video projects dir: {e}"))?;

        // Build env vars for the child process.
        let env_refs: Vec<(&str, &str)> = self
            .config
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Spawn the hyperframes MCP server via npx.
        let client = McpClient::spawn(
            &self.config.npx_path,
            &["@hyperframes/engine", "mcp"],
            &env_refs,
        )
        .await?;

        // Discover available tools.
        let tools = client.list_tools().await?;
        tracing::info!(
            tool_count = tools.len(),
            "hyperframes MCP tools discovered"
        );
        for tool in &tools {
            tracing::debug!(name = %tool.name, "  MCP tool: {}", tool.description.as_deref().unwrap_or(""));
        }

        let mut cached = self.available_tools.lock().await;
        *cached = tools;

        *mcp_guard = Some(client);
        Ok(())
    }

    /// Initialize a new video project (composition).
    pub async fn create_project(
        &self,
        name: &str,
        width: u32,
        height: u32,
        duration: f64,
        fps: u32,
    ) -> Result<VideoProject, String> {
        self.ensure_connected().await?;

        let project_id = Uuid::new_v4().to_string();
        let project_dir = self.config.projects_dir.join(&project_id);
        std::fs::create_dir_all(&project_dir)
            .map_err(|e| format!("failed to create project dir: {e}"))?;

        // Try to use the MCP init tool if available.
        let init_result = self
            .call_mcp_tool(
                "init",
                json!({
                    "name": name,
                    "width": width,
                    "height": height,
                    "duration": duration,
                    "fps": fps,
                    "outputDir": project_dir.to_string_lossy()
                }),
            )
            .await;

        match init_result {
            Ok(_) => {
                tracing::info!(project_id = %project_id, "video project created via MCP");
            }
            Err(e) => {
                tracing::debug!(
                    "MCP init not available ({e}), creating project locally"
                );
                // Fallback: create the HTML composition file directly.
                self.write_composition_html(
                    &project_dir, name, width, height, duration, fps, &[],
                )?;
            }
        }

        Ok(VideoProject {
            id: project_id,
            name: name.to_string(),
            dir: project_dir,
            width,
            height,
            duration,
            fps,
        })
    }

    /// Add an element to an existing composition.
    pub async fn add_element(
        &self,
        project: &VideoProject,
        element: &CompositionElement,
    ) -> Result<Value, String> {
        self.ensure_connected().await?;

        // Try the MCP tool first.
        let mcp_result = self
            .call_mcp_tool(
                "add_element",
                json!({
                    "projectDir": project.dir.to_string_lossy(),
                    "element": element
                }),
            )
            .await;

        match mcp_result {
            Ok(result) => Ok(result),
            Err(_) => {
                // Fallback: directly edit the HTML composition file.
                let html_path = project.dir.join("index.html");
                let element_html = self.element_to_html(element)?;

                if html_path.exists() {
                    let mut content = std::fs::read_to_string(&html_path)
                        .map_err(|e| format!("failed to read composition: {e}"))?;

                    // Insert before closing stage div.
                    if let Some(pos) = content.rfind("</div>") {
                        content.insert_str(pos, &format!("  {element_html}\n  "));
                        std::fs::write(&html_path, content)
                            .map_err(|e| format!("failed to write composition: {e}"))?;
                    }
                }

                Ok(json!({
                    "status": "added",
                    "element_id": element.id,
                    "method": "direct"
                }))
            }
        }
    }

    /// Update the full HTML composition for a project.
    pub async fn set_composition(
        &self,
        project: &VideoProject,
        html: &str,
    ) -> Result<Value, String> {
        self.ensure_connected().await?;

        // Try MCP tool first.
        let mcp_result = self
            .call_mcp_tool(
                "set_composition",
                json!({
                    "projectDir": project.dir.to_string_lossy(),
                    "html": html
                }),
            )
            .await;

        match mcp_result {
            Ok(result) => Ok(result),
            Err(_) => {
                // Fallback: write HTML directly.
                let html_path = project.dir.join("index.html");
                std::fs::write(&html_path, html)
                    .map_err(|e| format!("failed to write composition: {e}"))?;

                Ok(json!({
                    "status": "composition_set",
                    "path": html_path.to_string_lossy(),
                    "method": "direct"
                }))
            }
        }
    }

    /// Render the composition to an MP4 video.
    pub async fn render(
        &self,
        project: &VideoProject,
        output_name: Option<&str>,
    ) -> Result<RenderResult, String> {
        self.ensure_connected().await?;

        let output_filename = output_name.unwrap_or("output.mp4");
        let output_path = project.dir.join(output_filename);

        // Try the MCP render tool.
        let render_result = self
            .call_mcp_tool(
                "render",
                json!({
                    "projectDir": project.dir.to_string_lossy(),
                    "output": output_path.to_string_lossy(),
                    "width": project.width,
                    "height": project.height,
                    "fps": project.fps,
                    "duration": project.duration
                }),
            )
            .await;

        match render_result {
            Ok(result) => {
                // Parse the result for the output path.
                let actual_path = result
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(PathBuf::from)
                    .unwrap_or(output_path.clone());

                let size = std::fs::metadata(&actual_path)
                    .map(|m| m.len())
                    .unwrap_or(0);

                Ok(RenderResult {
                    path: actual_path,
                    duration: project.duration,
                    size,
                    format: "mp4".to_string(),
                })
            }
            Err(e) => Err(format!("render failed: {e}")),
        }
    }

    /// Get project status and composition info.
    pub async fn project_info(&self, project: &VideoProject) -> Result<Value, String> {
        let html_path = project.dir.join("index.html");

        let html_exists = html_path.exists();
        let html_size = if html_exists {
            std::fs::metadata(&html_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        // Check for rendered output.
        let output_path = project.dir.join("output.mp4");
        let rendered = output_path.exists();
        let render_size = if rendered {
            std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        Ok(json!({
            "id": project.id,
            "name": project.name,
            "dir": project.dir.to_string_lossy(),
            "width": project.width,
            "height": project.height,
            "duration": project.duration,
            "fps": project.fps,
            "composition": {
                "exists": html_exists,
                "size_bytes": html_size,
                "path": html_path.to_string_lossy()
            },
            "rendered": {
                "exists": rendered,
                "size_bytes": render_size,
                "path": output_path.to_string_lossy()
            }
        }))
    }

    /// List all video projects in the projects directory.
    pub fn list_projects(&self) -> Result<Vec<VideoProject>, String> {
        let mut projects = Vec::new();

        if !self.config.projects_dir.exists() {
            return Ok(projects);
        }

        let entries = std::fs::read_dir(&self.config.projects_dir)
            .map_err(|e| format!("failed to read projects dir: {e}"))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let meta_path = path.join("project.json");
                if meta_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&meta_path) {
                        if let Ok(project) = serde_json::from_str::<VideoProject>(&content) {
                            projects.push(project);
                        }
                    }
                }
            }
        }

        Ok(projects)
    }

    /// Get available MCP tools from the hyperframes server.
    pub async fn available_tools(&self) -> Vec<McpToolDef> {
        self.available_tools.lock().await.clone()
    }

    /// Call an arbitrary MCP tool on the hyperframes server.
    pub async fn call_mcp_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let mcp_guard = self.mcp.lock().await;
        let client = mcp_guard
            .as_ref()
            .ok_or("MCP client not connected")?;

        let result = client.call_tool(name, arguments).await?;

        if result.is_error {
            let error_text: String = result
                .content
                .iter()
                .filter_map(|c| c.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!("MCP tool `{name}` error: {error_text}"));
        }

        // Extract text content from the result.
        let texts: Vec<&str> = result
            .content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect();

        if texts.len() == 1 {
            // Try to parse as JSON, otherwise wrap as text.
            match serde_json::from_str::<Value>(texts[0]) {
                Ok(v) => Ok(v),
                Err(_) => Ok(json!({ "text": texts[0] })),
            }
        } else {
            Ok(json!({ "content": result.content }))
        }
    }

    /// Gracefully shut down the hyperframes MCP server.
    pub async fn shutdown(&self) {
        let mut mcp_guard = self.mcp.lock().await;
        if let Some(client) = mcp_guard.take() {
            client.shutdown().await;
        }
        tracing::info!("video channel shut down");
    }

    // --- Private helpers ---

    /// Generate an HTML composition file for a project.
    fn write_composition_html(
        &self,
        project_dir: &Path,
        name: &str,
        width: u32,
        height: u32,
        _duration: f64,
        _fps: u32,
        elements: &[CompositionElement],
    ) -> Result<(), String> {
        let mut elements_html = String::new();
        for elem in elements {
            let html = self.element_to_html(elem)?;
            elements_html.push_str(&format!("    {html}\n"));
        }

        let html = format!(
            r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>{name}</title>
  <style>
    * {{ margin: 0; padding: 0; box-sizing: border-box; }}
    #stage {{ position: relative; overflow: hidden; background: #000; }}
  </style>
</head>
<body>
  <div id="stage"
       data-composition-id="{name}"
       data-width="{width}"
       data-height="{height}">
{elements_html}  </div>
</body>
</html>"#
        );

        let html_path = project_dir.join("index.html");
        std::fs::write(&html_path, &html)
            .map_err(|e| format!("failed to write composition HTML: {e}"))?;

        // Write project metadata.
        let meta = json!({
            "id": project_dir.file_name().unwrap().to_string_lossy(),
            "name": name,
            "dir": project_dir.to_string_lossy(),
            "width": width,
            "height": height
        });
        let meta_path = project_dir.join("project.json");
        std::fs::write(meta_path, serde_json::to_string_pretty(&meta).unwrap())
            .map_err(|e| format!("failed to write project metadata: {e}"))?;

        Ok(())
    }

    /// Convert a composition element to its HTML representation using
    /// hyperframes data attributes.
    fn element_to_html(&self, elem: &CompositionElement) -> Result<String, String> {
        let props = &elem.properties;
        let base_attrs = format!(
            r#"id="{}" data-start="{}" data-duration="{}" data-track="{}""#,
            elem.id, elem.start, elem.duration, elem.track
        );

        match elem.element_type.as_str() {
            "text" => {
                let text = props
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Hello");
                let font_size = props
                    .get("fontSize")
                    .and_then(|v| v.as_str())
                    .unwrap_or("48px");
                let color = props
                    .get("color")
                    .and_then(|v| v.as_str())
                    .unwrap_or("#ffffff");
                let x = props.get("x").and_then(|v| v.as_str()).unwrap_or("50%");
                let y = props.get("y").and_then(|v| v.as_str()).unwrap_or("50%");

                Ok(format!(
                    r#"<div {base_attrs} style="position:absolute;left:{x};top:{y};font-size:{font_size};color:{color};transform:translate(-50%,-50%)">{text}</div>"#
                ))
            }
            "image" => {
                let src = props
                    .get("src")
                    .and_then(|v| v.as_str())
                    .ok_or("image element requires `src` property")?;
                let style = props
                    .get("style")
                    .and_then(|v| v.as_str())
                    .unwrap_or("width:100%;height:100%;object-fit:cover");

                Ok(format!(
                    r#"<img {base_attrs} src="{src}" style="{style}" />"#
                ))
            }
            "video" => {
                let src = props
                    .get("src")
                    .and_then(|v| v.as_str())
                    .ok_or("video element requires `src` property")?;
                let volume = props.get("volume").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let volume_attr = if volume > 0.0 {
                    format!(r#" data-volume="{volume}""#)
                } else {
                    String::new()
                };

                Ok(format!(
                    r#"<video {base_attrs}{volume_attr} src="{src}" muted playsinline style="width:100%;height:100%;object-fit:cover" />"#
                ))
            }
            "audio" => {
                let src = props
                    .get("src")
                    .and_then(|v| v.as_str())
                    .ok_or("audio element requires `src` property")?;
                let volume = props.get("volume").and_then(|v| v.as_f64()).unwrap_or(1.0);

                Ok(format!(
                    r#"<audio {base_attrs} data-volume="{volume}" src="{src}" />"#
                ))
            }
            "shape" => {
                let bg = props
                    .get("backgroundColor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("#333333");
                let width = props
                    .get("width")
                    .and_then(|v| v.as_str())
                    .unwrap_or("100%");
                let height = props
                    .get("height")
                    .and_then(|v| v.as_str())
                    .unwrap_or("100%");
                let x = props.get("x").and_then(|v| v.as_str()).unwrap_or("0");
                let y = props.get("y").and_then(|v| v.as_str()).unwrap_or("0");

                Ok(format!(
                    r#"<div {base_attrs} style="position:absolute;left:{x};top:{y};width:{width};height:{height};background:{bg}"></div>"#
                ))
            }
            "html" => {
                // Raw HTML passthrough — for advanced compositions.
                let raw = props
                    .get("html")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<div></div>");
                Ok(raw.to_string())
            }
            other => Err(format!(
                "unknown element type `{other}` — use text, image, video, audio, shape, or html"
            )),
        }
    }
}
