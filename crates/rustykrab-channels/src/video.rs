
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing;
use uuid::Uuid;

use crate::mcp::{McpClient, McpToolDef};

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub projects_dir: PathBuf,
    pub npx_path: String,
    pub default_quality: String,
    pub default_format: String,
    pub env: Vec<(String, String)>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoProject {
    pub id: String,
    pub name: String,
    pub dir: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub fps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderResult {
    pub path: PathBuf,
    pub duration: f64,
    pub size: u64,
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionElement {
    #[serde(rename = "type")]
    pub element_type: String,
    pub id: String,
    pub start: f64,
    pub duration: f64,
    pub track: u32,
    #[serde(default)]
    pub properties: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorResult {
    pub node_ok: bool,
    pub ffmpeg_ok: bool,
    pub chrome_ok: bool,
    pub raw_output: String,
}

pub struct VideoChannel {
    config: VideoConfig,
    mcp: Arc<Mutex<Option<McpClient>>>,
    available_tools: Mutex<Vec<McpToolDef>>,
    env_checked: Mutex<bool>,
}

impl VideoChannel {
    pub fn new(config: VideoConfig) -> Self {
        Self {
            config,
            mcp: Arc::new(Mutex::new(None)),
            available_tools: Mutex::new(Vec::new()),
            env_checked: Mutex::new(false),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(VideoConfig::default())
    }

    pub fn projects_dir(&self) -> &std::path::Path { &self.config.projects_dir }
    pub fn config(&self) -> &VideoConfig { &self.config }
    pub fn name(&self) -> &str {
        "video"
    }

    pub async fn check_environment(&self) -> Result<DoctorResult, String> {
        let output = self.run_npx(&["hyperframes", "doctor"], None).await?;

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

        let init_result = self.run_npx(&args, Some(&self.config.projects_dir)).await;

        match init_result {
            Ok(output) if output.status.success() => {
                // hyperframes init creates a directory named after the project.
                // Rename it to our UUID-based directory.
                let created_dir = self.config.projects_dir.join(name);
                if created_dir.exists() && created_dir != project_dir {
                    std::fs::rename(&created_dir, &project_dir)
                        .map_err(|e| format!("failed to rename project dir: {e}"))?;
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
                self.write_composition_html(&project_dir, name, width, height, duration, fps, &[])?;
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

        let render_output = self.run_npx(&args, Some(&project.dir)).await?;

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

    include!("video_ext.rs");
    include!("video_ext.rs");
}
