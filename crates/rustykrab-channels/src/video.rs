//! Video communication channel powered by Hyperframes.
//!
//! Treats video as a first-class mode of communication: the agent can
//! respond to users by composing HTML-based video compositions and rendering
//! them to MP4 files. Uses the hyperframes CLI (`npx hyperframes`) as the
//! primary rendering engine, with optional MCP server support for advanced
//! operations.
//!
//! Hyperframes compositions are HTML documents with semantic data attributes:
//! - `data-start` — playhead position in seconds
//! - `data-duration` — element visibility window
//! - `data-track` — layer ordering (compositing depth)
//! - `data-volume` — audio level normalization
//!
//! The rendering pipeline: HTML composition → Puppeteer (frame capture) → FFmpeg → MP4

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
    /// Render quality: "draft", "standard", or "high".
    pub default_quality: String,
    /// Default output format: "mp4" or "webm".
    pub default_format: String,
    /// Additional environment variables for child processes.
    pub env: Vec<(String, String)>,
    /// Whether to attempt MCP connection for advanced operations.
    pub mcp_enabled: bool,
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
            default_quality: "standard".to_string(),
            default_format: "mp4".to_string(),
            env: Vec::new(),
            mcp_enabled: false,
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
    /// Format ("mp4" or "webm").
    pub format: String,
}

/// An element in a video composition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionElement {
    /// Element type: "text", "image", "video", "audio", "shape", "html".
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

/// Result of running `hyperframes doctor` — environment readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorResult {
    pub node_ok: bool,
    pub ffmpeg_ok: bool,
    pub chrome_ok: bool,
    pub raw_output: String,
}

/// The video communication channel.
///
/// Manages the hyperframes CLI and optional MCP server to provide video
/// as a mode of communication. Like Telegram sends text or Signal sends
/// encrypted messages, VideoChannel communicates via video.
pub struct VideoChannel {
    config: VideoConfig,
    /// Optional MCP client for advanced operations.
    mcp: Arc<Mutex<Option<McpClient>>>,
    /// Cached MCP tool definitions.
    available_tools: Mutex<Vec<McpToolDef>>,
    /// Whether the environment has been verified.
    env_checked: Mutex<bool>,
}

impl VideoChannel {
    /// Create a new video channel with the given configuration.
    pub fn new(config: VideoConfig) -> Self {
        Self {
            config,
            mcp: Arc::new(Mutex::new(None)),
            available_tools: Mutex::new(Vec::new()),
            env_checked: Mutex::new(false),
        }
    }

    /// Create a new video channel with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(VideoConfig::default())
    }

    pub fn name(&self) -> &str {
        "video"
    }

    /// Run `hyperframes doctor` to verify the environment (Node >= 22, FFmpeg, Chrome).
    pub async fn check_environment(&self) -> Result<DoctorResult, String> {
        let output = self
            .run_npx(&["hyperframes", "doctor"], None)
            .await?;

        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{raw}\n{stderr}");

        let result = DoctorResult {
            node_ok: combined.contains("node") && !combined.contains("node: FAIL"),
            ffmpeg_ok: combined.contains("ffmpeg") && !combined.contains("ffmpeg: FAIL"),
            chrome_ok: combined.contains("chrome") || combined.contains("Chrome"),
            raw_output: combined,
        };

        let mut checked = self.env_checked.lock().await;
        *checked = true;

        if result.node_ok && result.ffmpeg_ok {
            tracing::info!("hyperframes environment check passed");
        } else {
            tracing::warn!(
                node = result.node_ok,
                ffmpeg = result.ffmpeg_ok,
                chrome = result.chrome_ok,
                "hyperframes environment check — some dependencies missing"
            );
        }

        Ok(result)
    }

    /// Initialize a new video project using `npx hyperframes init`.
    pub async fn create_project(
        &self,
        name: &str,
        width: u32,
        height: u32,
        duration: f64,
        fps: u32,
        template: Option<&str>,
    ) -> Result<VideoProject, String> {
        // Ensure projects directory exists.
        std::fs::create_dir_all(&self.config.projects_dir)
            .map_err(|e| format!("failed to create video projects dir: {e}"))?;

        let project_id = Uuid::new_v4().to_string();
        let project_dir = self.config.projects_dir.join(&project_id);

        // Try `npx hyperframes init` first.
        let mut args = vec!["hyperframes", "init", name];
        let template_val = template.unwrap_or("blank");
        args.push("--template");
        args.push(template_val);

        let init_result = self
            .run_npx(&args, Some(&self.config.projects_dir))
            .await;

        match init_result {
            Ok(output) if output.status.success() => {
                // hyperframes init creates a directory named after the project.
                // Rename it to our UUID-based directory.
                let created_dir = self.config.projects_dir.join(name);
                if created_dir.exists() && created_dir != project_dir {
                    std::fs::rename(&created_dir, &project_dir).map_err(|e| {
                        format!("failed to rename project dir: {e}")
                    })?;
                }
                tracing::info!(
                    project_id = %project_id,
                    template = template_val,
                    "video project created via hyperframes CLI"
                );
            }
            _ => {
                // Fallback: create the composition HTML directly.
                tracing::debug!("hyperframes CLI not available, creating project locally");
                std::fs::create_dir_all(&project_dir)
                    .map_err(|e| format!("failed to create project dir: {e}"))?;
                self.write_composition_html(
                    &project_dir, name, width, height, duration, fps, &[],
                )?;
            }
        }

        let project = VideoProject {
            id: project_id,
            name: name.to_string(),
            dir: project_dir.clone(),
            width,
            height,
            duration,
            fps,
        };

        // Persist project metadata.
        self.write_project_meta(&project)?;

        Ok(project)
    }

    /// Add an element to an existing composition by editing the HTML.
    pub async fn add_element(
        &self,
        project: &VideoProject,
        element: &CompositionElement,
    ) -> Result<Value, String> {
        let html_path = project.dir.join("index.html");
        let element_html = self.element_to_html(element)?;

        if html_path.exists() {
            let mut content = std::fs::read_to_string(&html_path)
                .map_err(|e| format!("failed to read composition: {e}"))?;

            // Insert before the closing </div> of the stage.
            if let Some(pos) = content.rfind("</div>") {
                content.insert_str(pos, &format!("    {element_html}\n  "));
                std::fs::write(&html_path, content)
                    .map_err(|e| format!("failed to write composition: {e}"))?;
            } else {
                return Err("composition HTML missing closing </div> for stage".to_string());
            }
        } else {
            return Err(format!(
                "composition file not found: {}. Use create_project first.",
                html_path.display()
            ));
        }

        // Run lint to validate the composition.
        let lint_result = self.lint(project).await;

        Ok(json!({
            "status": "added",
            "element_id": element.id,
            "element_type": element.element_type,
            "timeline": format!("{}s – {}s on track {}", element.start, element.start + element.duration, element.track),
            "lint": lint_result.unwrap_or_else(|e| json!({"error": e}))
        }))
    }

    /// Set the full HTML composition for a project.
    pub async fn set_composition(
        &self,
        project: &VideoProject,
        html: &str,
    ) -> Result<Value, String> {
        let html_path = project.dir.join("index.html");
        std::fs::write(&html_path, html)
            .map_err(|e| format!("failed to write composition: {e}"))?;

        // Run lint to validate.
        let lint_result = self.lint(project).await;

        Ok(json!({
            "status": "composition_set",
            "path": html_path.to_string_lossy(),
            "size_bytes": html.len(),
            "lint": lint_result.unwrap_or_else(|e| json!({"error": e}))
        }))
    }

    /// Lint the composition using `npx hyperframes lint`.
    pub async fn lint(&self, project: &VideoProject) -> Result<Value, String> {
        let output = self
            .run_npx(&["hyperframes", "lint"], Some(&project.dir))
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(json!({
            "success": output.status.success(),
            "output": stdout.trim(),
            "errors": stderr.trim(),
        }))
    }

    /// Render the composition to video using `npx hyperframes render`.
    pub async fn render(
        &self,
        project: &VideoProject,
        output_name: Option<&str>,
        quality: Option<&str>,
        format: Option<&str>,
    ) -> Result<RenderResult, String> {
        let fmt = format.unwrap_or(&self.config.default_format);
        let default_name = format!("output.{fmt}");
        let output_filename = output_name.unwrap_or(&default_name);
        let output_path = project.dir.join(output_filename);
        let qual = quality.unwrap_or(&self.config.default_quality);

        let fps_str = project.fps.to_string();
        let output_str = output_path.to_string_lossy().to_string();

        let args = vec![
            "hyperframes",
            "render",
            "--output",
            &output_str,
            "--fps",
            &fps_str,
            "--quality",
            qual,
            "--format",
            fmt,
        ];

        // If MCP is available, try it first (may support more render options).
        if self.config.mcp_enabled {
            if let Ok(result) = self.try_mcp_render(project, &output_path).await {
                return Ok(result);
            }
        }

        let render_output = self
            .run_npx(&args, Some(&project.dir))
            .await?;

        if !render_output.status.success() {
            let stderr = String::from_utf8_lossy(&render_output.stderr);
            let stdout = String::from_utf8_lossy(&render_output.stdout);
            return Err(format!(
                "render failed (exit {}): {}\n{}",
                render_output.status.code().unwrap_or(-1),
                stderr.trim(),
                stdout.trim()
            ));
        }

        // Find the output file. hyperframes may place it in a different location.
        let actual_path = if output_path.exists() {
            output_path
        } else {
            // Search common output locations.
            let alt_paths = [
                project.dir.join("out").join(output_filename),
                project.dir.join("dist").join(output_filename),
                project.dir.join(output_filename),
            ];
            alt_paths
                .into_iter()
                .find(|p| p.exists())
                .ok_or("render completed but output file not found")?
        };

        let size = std::fs::metadata(&actual_path)
            .map(|m| m.len())
            .unwrap_or(0);

        tracing::info!(
            path = %actual_path.display(),
            size_bytes = size,
            "video rendered successfully"
        );

        Ok(RenderResult {
            path: actual_path,
            duration: project.duration,
            size,
            format: fmt.to_string(),
        })
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

        // Check for rendered outputs.
        let mp4_path = project.dir.join("output.mp4");
        let webm_path = project.dir.join("output.webm");
        let rendered_mp4 = mp4_path.exists();
        let rendered_webm = webm_path.exists();

        // Try to get composition list from CLI.
        let compositions = self
            .run_npx(&["hyperframes", "compositions"], Some(&project.dir))
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

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
            "outputs": {
                "mp4": { "exists": rendered_mp4, "path": mp4_path.to_string_lossy() },
                "webm": { "exists": rendered_webm, "path": webm_path.to_string_lossy() }
            },
            "compositions_cli": compositions
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

    /// Connect to the hyperframes MCP server (optional, for advanced operations).
    pub async fn connect_mcp(&self) -> Result<Vec<McpToolDef>, String> {
        let mut mcp_guard = self.mcp.lock().await;

        if let Some(ref client) = *mcp_guard {
            if client.is_alive().await {
                return Ok(self.available_tools.lock().await.clone());
            }
        }

        let env_refs: Vec<(&str, &str)> = self
            .config
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let client = McpClient::spawn(
            &self.config.npx_path,
            &["@hyperframes/engine", "mcp"],
            &env_refs,
        )
        .await?;

        let tools = client.list_tools().await?;
        tracing::info!(tool_count = tools.len(), "hyperframes MCP tools discovered");

        let mut cached = self.available_tools.lock().await;
        *cached = tools.clone();
        *mcp_guard = Some(client);

        Ok(tools)
    }

    /// Call an MCP tool (requires prior `connect_mcp`).
    pub async fn call_mcp_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let mcp_guard = self.mcp.lock().await;
        let client = mcp_guard
            .as_ref()
            .ok_or("MCP not connected — call connect_mcp first, or set mcp_enabled=true")?;

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

        let texts: Vec<&str> = result
            .content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect();

        if texts.len() == 1 {
            match serde_json::from_str::<Value>(texts[0]) {
                Ok(v) => Ok(v),
                Err(_) => Ok(json!({ "text": texts[0] })),
            }
        } else {
            Ok(json!({ "content": result.content }))
        }
    }

    /// Get cached MCP tool definitions.
    pub async fn available_tools(&self) -> Vec<McpToolDef> {
        self.available_tools.lock().await.clone()
    }

    /// Gracefully shut down the MCP server (if running).
    pub async fn shutdown(&self) {
        let mut mcp_guard = self.mcp.lock().await;
        if let Some(client) = mcp_guard.take() {
            client.shutdown().await;
        }
        tracing::info!("video channel shut down");
    }

    // --- Private helpers ---

    /// Run an npx command and capture output.
    async fn run_npx(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<std::process::Output, String> {
        let mut cmd = tokio::process::Command::new(&self.config.npx_path);
        cmd.args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        for (k, v) in &self.config.env {
            cmd.env(k, v);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(300), // 5 minute timeout for renders
            cmd.output(),
        )
        .await
        .map_err(|_| "hyperframes command timed out (300s)".to_string())?
        .map_err(|e| format!("failed to run npx: {e}"))?;

        Ok(output)
    }

    /// Try rendering via MCP (optional path).
    async fn try_mcp_render(
        &self,
        project: &VideoProject,
        output_path: &Path,
    ) -> Result<RenderResult, String> {
        self.connect_mcp().await?;

        let result = self
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
            .await?;

        let actual_path = result
            .get("path")
            .and_then(|p| p.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| output_path.to_path_buf());

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

    /// Write project metadata to disk.
    fn write_project_meta(&self, project: &VideoProject) -> Result<(), String> {
        let meta = serde_json::to_string_pretty(project)
            .map_err(|e| format!("failed to serialize project meta: {e}"))?;
        let meta_path = project.dir.join("project.json");
        std::fs::write(meta_path, meta)
            .map_err(|e| format!("failed to write project metadata: {e}"))?;
        Ok(())
    }

    /// Generate an HTML composition file using hyperframes data attributes.
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
                let w = props
                    .get("width")
                    .and_then(|v| v.as_str())
                    .unwrap_or("100%");
                let h = props
                    .get("height")
                    .and_then(|v| v.as_str())
                    .unwrap_or("100%");
                let x = props.get("x").and_then(|v| v.as_str()).unwrap_or("0");
                let y = props.get("y").and_then(|v| v.as_str()).unwrap_or("0");

                Ok(format!(
                    r#"<div {base_attrs} style="position:absolute;left:{x};top:{y};width:{w};height:{h};background:{bg}"></div>"#
                ))
            }
            "html" => {
                // Raw HTML passthrough for advanced compositions.
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
