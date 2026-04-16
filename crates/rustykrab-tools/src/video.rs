//! Video creation tool — the agent's interface to the video communication channel.
//!
//! Enables the agent to compose HTML-based video compositions and render
//! them to MP4 via the hyperframes engine. Video is treated as a mode of
//! communication: the agent can respond with rendered video just as it
//! responds with text through Telegram or WebChat.
//!
//! Primary workflow:
//!   1. `create_project` — scaffold a new composition (uses `npx hyperframes init`)
//!   2. `add_element` or `set_composition` — build the scene
//!   3. `lint` — validate before rendering
//!   4. `render` — produce MP4/WebM (uses `npx hyperframes render`)
//!
//! The tool also supports `doctor` (environment check), `info`, `list`,
//! and direct MCP passthrough for advanced operations.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool};
use serde_json::{json, Value};

use rustykrab_channels::video::{CompositionElement, VideoChannel, VideoProject};

/// Backend trait for video operations, allowing the tool to remain
/// decoupled from the channel lifecycle.
#[async_trait]
pub trait VideoBackend: Send + Sync {
    /// Check environment readiness (Node >= 22, FFmpeg, Chrome).
    async fn check_environment(&self) -> std::result::Result<Value, String>;

    /// Create a new video project.
    async fn create_project(
        &self,
        name: &str,
        width: u32,
        height: u32,
        duration: f64,
        fps: u32,
        template: Option<&str>,
    ) -> std::result::Result<VideoProject, String>;

    /// Add an element to a composition.
    async fn add_element(
        &self,
        project: &VideoProject,
        element: &CompositionElement,
    ) -> std::result::Result<Value, String>;

    /// Set the full HTML composition.
    async fn set_composition(
        &self,
        project: &VideoProject,
        html: &str,
    ) -> std::result::Result<Value, String>;

    /// Lint a composition.
    async fn lint(&self, project: &VideoProject) -> std::result::Result<Value, String>;

    /// Render a project to video.
    async fn render(
        &self,
        project: &VideoProject,
        output_name: Option<&str>,
        quality: Option<&str>,
        format: Option<&str>,
    ) -> std::result::Result<Value, String>;

    /// Get project info.
    async fn project_info(&self, project: &VideoProject) -> std::result::Result<Value, String>;

    /// List projects.
    async fn list_projects(&self) -> std::result::Result<Vec<VideoProject>, String>;

    /// Connect to MCP server (optional).
    async fn connect_mcp(&self) -> std::result::Result<Value, String>;

    /// Call a raw MCP tool.
    async fn call_mcp_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<Value, String>;
}

/// Adapter bridging [VideoChannel] to the [VideoBackend] trait.
pub struct VideoChannelAdapter {
    channel: Arc<VideoChannel>,
}

impl VideoChannelAdapter {
    pub fn new(channel: Arc<VideoChannel>) -> Self {
        Self { channel }
    }
}

#[async_trait]
impl VideoBackend for VideoChannelAdapter {
    async fn check_environment(&self) -> std::result::Result<Value, String> {
        let result = self.channel.check_environment().await?;
        Ok(json!({
            "node_ok": result.node_ok,
            "ffmpeg_ok": result.ffmpeg_ok,
            "chrome_ok": result.chrome_ok,
            "raw_output": result.raw_output
        }))
    }

    async fn create_project(
        &self,
        name: &str,
        width: u32,
        height: u32,
        duration: f64,
        fps: u32,
        template: Option<&str>,
    ) -> std::result::Result<VideoProject, String> {
        self.channel
            .create_project(name, width, height, duration, fps, template)
            .await
    }

    async fn add_element(
        &self,
        project: &VideoProject,
        element: &CompositionElement,
    ) -> std::result::Result<Value, String> {
        self.channel.add_element(project, element).await
    }

    async fn set_composition(
        &self,
        project: &VideoProject,
        html: &str,
    ) -> std::result::Result<Value, String> {
        self.channel.set_composition(project, html).await
    }

    async fn lint(&self, project: &VideoProject) -> std::result::Result<Value, String> {
        self.channel.lint(project).await
    }

    async fn render(
        &self,
        project: &VideoProject,
        output_name: Option<&str>,
        quality: Option<&str>,
        format: Option<&str>,
    ) -> std::result::Result<Value, String> {
        let result = self
            .channel
            .render(project, output_name, quality, format)
            .await?;
        Ok(json!({
            "status": "rendered",
            "path": result.path.to_string_lossy(),
            "duration": result.duration,
            "size_bytes": result.size,
            "format": result.format
        }))
    }

    async fn project_info(&self, project: &VideoProject) -> std::result::Result<Value, String> {
        self.channel.project_info(project).await
    }

    async fn list_projects(&self) -> std::result::Result<Vec<VideoProject>, String> {
        self.channel.list_projects()
    }

    async fn connect_mcp(&self) -> std::result::Result<Value, String> {
        let tools = self.channel.connect_mcp().await?;
        Ok(json!(tools))
    }

    async fn call_mcp_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<Value, String> {
        self.channel.call_mcp_tool(name, arguments).await
    }
}

/// Agent-facing video creation tool.
///
/// Actions:
/// - `doctor` — Check environment (Node >= 22, FFmpeg, Chrome)
/// - `create_project` — Initialize a new video composition
/// - `add_element` — Add a visual/audio element to the timeline
/// - `set_composition` — Set the full HTML composition directly
/// - `lint` — Validate the composition
/// - `render` — Render to MP4/WebM
/// - `info` — Get project status
/// - `list` — List all video projects
/// - `connect_mcp` — Connect to hyperframes MCP server (advanced)
/// - `mcp_call` — Call a raw MCP tool (advanced)
pub struct VideoTool {
    backend: Arc<dyn VideoBackend>,
    /// In-memory project cache for the current session.
    projects: tokio::sync::Mutex<std::collections::HashMap<String, VideoProject>>,
}

impl VideoTool {
    pub fn new(backend: Arc<dyn VideoBackend>) -> Self {
        Self {
            backend,
            projects: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    async fn get_project(&self, project_id: &str) -> Result<VideoProject> {
        let projects = self.projects.lock().await;
        projects.get(project_id).cloned().ok_or_else(|| {
            rustykrab_core::Error::ToolExecution(
                format!(
                    "project `{project_id}` not found. Use action `create_project` first, \
                     or `list` to see existing projects."
                )
                .into(),
            )
        })
    }
}

#[async_trait]
impl Tool for VideoTool {
    fn name(&self) -> &str {
        "video"
    }

    fn description(&self) -> &str {
        "Create and render video compositions using hyperframes. Video is a \
         communication channel: compose HTML-based scenes with text, images, \
         video clips, and audio on a timeline, then render to MP4. Use this \
         when you want to communicate via video — tutorials, presentations, \
         explanations, greetings, or any visual content. Requires Node.js >= 22 \
         and FFmpeg. Templates: blank, warm-grain, play-mode, swiss-grid, \
         vignelli, decision-tree, kinetic-type, product-promo, nyt-graph."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_write: true,
            needs_spawn: true,
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
                        "enum": [
                            "doctor",
                            "create_project",
                            "add_element",
                            "set_composition",
                            "lint",
                            "render",
                            "info",
                            "list",
                            "connect_mcp",
                            "mcp_call"
                        ],
                        "description": "The video action to perform"
                    },
                    "name": {
                        "type": "string",
                        "description": "Project name (for create_project)"
                    },
                    "project_id": {
                        "type": "string",
                        "description": "Project ID (for add_element, set_composition, lint, render, info)"
                    },
                    "width": {
                        "type": "integer",
                        "description": "Video width in pixels (default: 1920)",
                        "default": 1920
                    },
                    "height": {
                        "type": "integer",
                        "description": "Video height in pixels (default: 1080)",
                        "default": 1080
                    },
                    "duration": {
                        "type": "number",
                        "description": "Video duration in seconds (default: 10)",
                        "default": 10
                    },
                    "fps": {
                        "type": "integer",
                        "description": "Frames per second: 24, 30, or 60 (default: 30)",
                        "default": 30
                    },
                    "template": {
                        "type": "string",
                        "enum": ["blank", "warm-grain", "play-mode", "swiss-grid", "vignelli", "decision-tree", "kinetic-type", "product-promo", "nyt-graph"],
                        "description": "Project template (for create_project, default: blank)"
                    },
                    "element": {
                        "type": "object",
                        "description": "Element to add. Fields: type (text|image|video|audio|shape|html), id, start, duration, track, properties",
                        "properties": {
                            "type": {
                                "type": "string",
                                "enum": ["text", "image", "video", "audio", "shape", "html"]
                            },
                            "id": { "type": "string" },
                            "start": { "type": "number", "description": "Start time in seconds" },
                            "duration": { "type": "number", "description": "Duration in seconds" },
                            "track": { "type": "integer", "description": "Layer (0 = bottom)" },
                            "properties": {
                                "type": "object",
                                "description": "Element-specific: text (text, fontSize, color, x, y), image/video (src, style, volume), audio (src, volume), shape (backgroundColor, width, height, x, y), html (html)"
                            }
                        },
                        "required": ["type", "id", "start", "duration", "track"]
                    },
                    "html": {
                        "type": "string",
                        "description": "Full HTML composition (for set_composition). Use hyperframes data attributes: data-start, data-duration, data-track, data-volume on a #stage div"
                    },
                    "output_name": {
                        "type": "string",
                        "description": "Output filename (for render, default: output.mp4)"
                    },
                    "quality": {
                        "type": "string",
                        "enum": ["draft", "standard", "high"],
                        "description": "Render quality (default: standard)"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["mp4", "webm"],
                        "description": "Output format (default: mp4)"
                    },
                    "mcp_tool": {
                        "type": "string",
                        "description": "MCP tool name (for mcp_call)"
                    },
                    "mcp_arguments": {
                        "type": "object",
                        "description": "MCP tool arguments (for mcp_call)"
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

        match action {
            "doctor" => {
                let result = self.backend.check_environment().await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("environment check failed: {e}").into(),
                    )
                })?;

                Ok(json!({
                    "action": "doctor",
                    "success": true,
                    "environment": result,
                    "note": "Requires: Node.js >= 22, FFmpeg, and Chrome/Chromium for rendering."
                }))
            }

            "create_project" => {
                let name = args["name"].as_str().unwrap_or("untitled");
                let width = args["width"].as_u64().unwrap_or(1920) as u32;
                let height = args["height"].as_u64().unwrap_or(1080) as u32;
                let duration = args["duration"].as_f64().unwrap_or(10.0);
                let fps = args["fps"].as_u64().unwrap_or(30) as u32;
                let template = args["template"].as_str();

                let project = self
                    .backend
                    .create_project(name, width, height, duration, fps, template)
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(
                            format!("failed to create project: {e}").into(),
                        )
                    })?;

                let project_id = project.id.clone();

                let result = json!({
                    "action": "create_project",
                    "success": true,
                    "project_id": &project.id,
                    "name": &project.name,
                    "dir": project.dir.to_string_lossy(),
                    "width": project.width,
                    "height": project.height,
                    "duration": project.duration,
                    "fps": project.fps,
                    "template": template.unwrap_or("blank"),
                    "note": "Project created. Use `add_element` to add content to the timeline, \
                             or `set_composition` to set the full HTML directly. Use `lint` to \
                             validate, then `render` to produce MP4/WebM."
                });

                let mut projects = self.projects.lock().await;
                projects.insert(project_id, project);

                Ok(result)
            }

            "add_element" => {
                let project_id = args["project_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing project_id".into())
                })?;

                let project = self.get_project(project_id).await?;

                let elem_val = &args["element"];
                let element: CompositionElement = serde_json::from_value(elem_val.clone())
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(format!("invalid element: {e}").into())
                    })?;

                let result = self
                    .backend
                    .add_element(&project, &element)
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(
                            format!("failed to add element: {e}").into(),
                        )
                    })?;

                Ok(json!({
                    "action": "add_element",
                    "success": true,
                    "project_id": project_id,
                    "result": result
                }))
            }

            "set_composition" => {
                let project_id = args["project_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing project_id".into())
                })?;

                let project = self.get_project(project_id).await?;

                let html = args["html"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing html for set_composition".into())
                })?;

                let result = self
                    .backend
                    .set_composition(&project, html)
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(
                            format!("failed to set composition: {e}").into(),
                        )
                    })?;

                Ok(json!({
                    "action": "set_composition",
                    "success": true,
                    "project_id": project_id,
                    "result": result,
                    "note": "Composition set. Use `lint` to validate, then `render` to produce video."
                }))
            }

            "lint" => {
                let project_id = args["project_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing project_id".into())
                })?;

                let project = self.get_project(project_id).await?;

                let result = self.backend.lint(&project).await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(format!("lint failed: {e}").into())
                })?;

                Ok(json!({
                    "action": "lint",
                    "success": true,
                    "project_id": project_id,
                    "result": result
                }))
            }

            "render" => {
                let project_id = args["project_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing project_id".into())
                })?;

                let project = self.get_project(project_id).await?;
                let output_name = args["output_name"].as_str();
                let quality = args["quality"].as_str();
                let format = args["format"].as_str();

                let result = self
                    .backend
                    .render(&project, output_name, quality, format)
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(format!("render failed: {e}").into())
                    })?;

                Ok(json!({
                    "action": "render",
                    "success": true,
                    "project_id": project_id,
                    "render": result,
                    "note": "Video rendered successfully. The file is ready for delivery."
                }))
            }

            "info" => {
                let project_id = args["project_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing project_id".into())
                })?;

                let project = self.get_project(project_id).await?;

                let info = self.backend.project_info(&project).await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to get project info: {e}").into(),
                    )
                })?;

                Ok(json!({
                    "action": "info",
                    "success": true,
                    "project": info
                }))
            }

            "list" => {
                let projects = self.backend.list_projects().await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to list projects: {e}").into(),
                    )
                })?;

                let cached = self.projects.lock().await;
                let cached_ids: Vec<&str> = cached.keys().map(|s| s.as_str()).collect();

                Ok(json!({
                    "action": "list",
                    "success": true,
                    "projects": projects,
                    "active_session_projects": cached_ids,
                    "count": projects.len()
                }))
            }

            "connect_mcp" => {
                let tools = self.backend.connect_mcp().await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to connect MCP: {e}").into(),
                    )
                })?;

                Ok(json!({
                    "action": "connect_mcp",
                    "success": true,
                    "tools": tools,
                    "note": "MCP connected. Use `mcp_call` for advanced hyperframes operations."
                }))
            }

            "mcp_call" => {
                let tool_name = args["mcp_tool"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing mcp_tool name".into())
                })?;

                let arguments = args["mcp_arguments"]
                    .as_object()
                    .map(|o| Value::Object(o.clone()))
                    .unwrap_or(json!({}));

                let result = self
                    .backend
                    .call_mcp_tool(tool_name, arguments)
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(
                            format!("MCP tool call failed: {e}").into(),
                        )
                    })?;

                Ok(json!({
                    "action": "mcp_call",
                    "success": true,
                    "tool": tool_name,
                    "result": result
                }))
            }

            _ => Err(rustykrab_core::Error::ToolExecution(
                format!(
                    "unknown video action `{action}`. Available: doctor, create_project, \
                     add_element, set_composition, lint, render, info, list, connect_mcp, mcp_call"
                )
                .into(),
            )),
        }
    }
}
