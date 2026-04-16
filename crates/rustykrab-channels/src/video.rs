//! Video communication channel powered by Hyperframes.
//!
//! Treats video as a first-class mode of communication: the agent can
//! respond to users by composing HTML-based video compositions and rendering
//! them to MP4 files. Uses the hyperframes CLI (`npx hyperframes`) as the
//! primary rendering engine, with optional MCP server support for advanced
//! operations.
//!
//! Hyperframes compositions are HTML documents with semantic data attributes:
//! - `data-start` -- playhead position in seconds
//! - `data-duration` -- element visibility window
//! - `data-track` -- layer ordering (compositing depth)
//! - `data-volume` -- audio level normalization
//!
//! The rendering pipeline: HTML composition -> Puppeteer (frame capture) -> FFmpeg -> MP4

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
        Self { projects_dir: dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("rustykrab").join("video"), npx_path: "npx".into(), default_quality: "standard".into(), default_format: "mp4".into(), env: Vec::new(), mcp_enabled: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoProject { pub id: String, pub name: String, pub dir: PathBuf, pub width: u32, pub height: u32, pub duration: f64, pub fps: u32 }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderResult { pub path: PathBuf, pub duration: f64, pub size: u64, pub format: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionElement { #[serde(rename="type")] pub element_type: String, pub id: String, pub start: f64, pub duration: f64, pub track: u32, #[serde(default)] pub properties: Value }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorResult { pub node_ok: bool, pub ffmpeg_ok: bool, pub chrome_ok: bool, pub raw_output: String }

pub struct VideoChannel {
    config: VideoConfig,
    mcp: Arc<Mutex<Option<McpClient>>>,
    available_tools: Mutex<Vec<McpToolDef>>,
    env_checked: Mutex<bool>,
}

impl VideoChannel {
    pub fn new(config: VideoConfig) -> Self { Self { config, mcp: Arc::new(Mutex::new(None)), available_tools: Mutex::new(Vec::new()), env_checked: Mutex::new(false) } }
    pub fn with_defaults() -> Self { Self::new(VideoConfig::default()) }
    pub fn projects_dir(&self) -> &std::path::Path { &self.config.projects_dir }
    pub fn config(&self) -> &VideoConfig { &self.config }
    pub fn name(&self) -> &str { "video" }

    pub async fn check_environment(&self) -> Result<DoctorResult, String> {
        let output = self.run_npx(&["hyperframes", "doctor"], None).await?;
        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{raw}\n{stderr}");
        let result = DoctorResult { node_ok: combined.contains("node") && !combined.contains("node: FAIL"), ffmpeg_ok: combined.contains("ffmpeg") && !combined.contains("ffmpeg: FAIL"), chrome_ok: combined.contains("chrome") || combined.contains("Chrome"), raw_output: combined };
        *self.env_checked.lock().await = true;
        if result.node_ok && result.ffmpeg_ok { tracing::info!("hyperframes environment check passed"); } else { tracing::warn!(node=result.node_ok, ffmpeg=result.ffmpeg_ok, chrome=result.chrome_ok, "hyperframes env check: deps missing"); }
        Ok(result)
    }

    pub async fn create_project(&self, name: &str, width: u32, height: u32, duration: f64, fps: u32, template: Option<&str>) -> Result<VideoProject, String> {
        std::fs::create_dir_all(&self.config.projects_dir).map_err(|e| format!("failed to create projects dir: {e}"))?;
        let project_id = Uuid::new_v4().to_string();
        let project_dir = self.config.projects_dir.join(&project_id);
        let mut args = vec!["hyperframes", "init", name];
        let tmpl = template.unwrap_or("blank"); args.push("--template"); args.push(tmpl);
        let init = self.run_npx(&args, Some(&self.config.projects_dir)).await;
        match init {
            Ok(o) if o.status.success() => { let cd = self.config.projects_dir.join(name); if cd.exists() && cd != project_dir { std::fs::rename(&cd, &project_dir).map_err(|e| format!("rename: {e}"))?; } tracing::info!(project_id=%project_id, template=tmpl, "project created via CLI"); }
            _ => { tracing::debug!("CLI unavailable, creating locally"); std::fs::create_dir_all(&project_dir).map_err(|e| format!("mkdir: {e}"))?; self.write_composition_html(&project_dir, name, width, height, duration, fps, &[])?; }
        }
        let project = VideoProject { id: project_id, name: name.into(), dir: project_dir.clone(), width, height, duration, fps };
        self.write_project_meta(&project)?;
        Ok(project)
    }

    pub async fn add_element(&self, project: &VideoProject, element: &CompositionElement) -> Result<Value, String> {
        let hp = project.dir.join("index.html");
        let eh = self.element_to_html(element)?;
        if hp.exists() {
            let mut c = std::fs::read_to_string(&hp).map_err(|e| format!("read: {e}"))?;
            if let Some(pos) = c.rfind("</div>") { c.insert_str(pos, &format!("    {eh}\n  ")); std::fs::write(&hp, c).map_err(|e| format!("write: {e}"))?; } else { return Err("missing </div>".into()); }
        } else { return Err(format!("no composition at {}", hp.display())); }
        let lint = self.lint(project).await;
        Ok(json!({"status":"added","element_id":element.id,"element_type":element.element_type,"timeline":format!("{}s-{}s track {}",element.start,element.start+element.duration,element.track),"lint":lint.unwrap_or_else(|e| json!({"error":e}))}))
    }

    pub async fn set_composition(&self, project: &VideoProject, html: &str) -> Result<Value, String> {
        let hp = project.dir.join("index.html");
        std::fs::write(&hp, html).map_err(|e| format!("write: {e}"))?;
        let lint = self.lint(project).await;
        Ok(json!({"status":"composition_set","path":hp.to_string_lossy(),"size_bytes":html.len(),"lint":lint.unwrap_or_else(|e| json!({"error":e}))}))
    }

    pub async fn lint(&self, project: &VideoProject) -> Result<Value, String> {
        let o = self.run_npx(&["hyperframes","lint"], Some(&project.dir)).await?;
        Ok(json!({"success":o.status.success(),"output":String::from_utf8_lossy(&o.stdout).trim(),"errors":String::from_utf8_lossy(&o.stderr).trim()}))
    }

    pub async fn render(&self, project: &VideoProject, output_name: Option<&str>, quality: Option<&str>, format: Option<&str>) -> Result<RenderResult, String> {
        let fmt = format.unwrap_or(&self.config.default_format);
        let dn = format!("output.{fmt}"); let ofn = output_name.unwrap_or(&dn);
        let op = project.dir.join(ofn); let qual = quality.unwrap_or(&self.config.default_quality);
        let fs = project.fps.to_string(); let os = op.to_string_lossy().to_string();
        let args = vec!["hyperframes","render","--output",&os,"--fps",&fs,"--quality",qual,"--format",fmt];
        if self.config.mcp_enabled { if let Ok(r) = self.try_mcp_render(project, &op).await { return Ok(r); } }
        let ro = self.run_npx(&args, Some(&project.dir)).await?;
        if !ro.status.success() { return Err(format!("render failed ({}): {} {}", ro.status.code().unwrap_or(-1), String::from_utf8_lossy(&ro.stderr).trim(), String::from_utf8_lossy(&ro.stdout).trim())); }
        let ap = if op.exists() { op } else { [project.dir.join("out").join(ofn), project.dir.join("dist").join(ofn), project.dir.join(ofn)].into_iter().find(|p| p.exists()).ok_or("output not found")? };
        let sz = std::fs::metadata(&ap).map(|m| m.len()).unwrap_or(0);
        tracing::info!(path=%ap.display(), size_bytes=sz, "video rendered");
        Ok(RenderResult { path: ap, duration: project.duration, size: sz, format: fmt.into() })
    }

    pub async fn project_info(&self, project: &VideoProject) -> Result<Value, String> {
        let hp = project.dir.join("index.html"); let he = hp.exists();
        let hs = if he { std::fs::metadata(&hp).map(|m|m.len()).unwrap_or(0) } else { 0 };
        let mp = project.dir.join("output.mp4"); let wp = project.dir.join("output.webm");
        let comps = self.run_npx(&["hyperframes","compositions"], Some(&project.dir)).await.ok().filter(|o|o.status.success()).map(|o|String::from_utf8_lossy(&o.stdout).trim().to_string());
        Ok(json!({"id":project.id,"name":project.name,"dir":project.dir.to_string_lossy(),"width":project.width,"height":project.height,"duration":project.duration,"fps":project.fps,"composition":{"exists":he,"size_bytes":hs,"path":hp.to_string_lossy()},"outputs":{"mp4":{"exists":mp.exists(),"path":mp.to_string_lossy()},"webm":{"exists":wp.exists(),"path":wp.to_string_lossy()}},"compositions_cli":comps}))
    }

    pub fn list_projects(&self) -> Result<Vec<VideoProject>, String> {
        let mut ps = Vec::new();
        if !self.config.projects_dir.exists() { return Ok(ps); }
        for e in std::fs::read_dir(&self.config.projects_dir).map_err(|e|format!("readdir: {e}"))?.flatten() {
            let p = e.path();
            if p.is_dir() { let mp = p.join("project.json"); if mp.exists() { if let Ok(c) = std::fs::read_to_string(&mp) { if let Ok(proj) = serde_json::from_str::<VideoProject>(&c) { ps.push(proj); } } } }
        }
        Ok(ps)
    }

    pub async fn connect_mcp(&self) -> Result<Vec<McpToolDef>, String> {
        let mut g = self.mcp.lock().await;
        if let Some(ref c) = *g { if c.is_alive().await { return Ok(self.available_tools.lock().await.clone()); } }
        let er: Vec<(&str,&str)> = self.config.env.iter().map(|(k,v)|(k.as_str(),v.as_str())).collect();
        let c = McpClient::spawn(&self.config.npx_path, &["@hyperframes/engine","mcp"], &er).await?;
        let tools = c.list_tools().await?; tracing::info!(n=tools.len(), "MCP tools discovered");
        *self.available_tools.lock().await = tools.clone(); *g = Some(c); Ok(tools)
    }

    pub async fn call_mcp_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let g = self.mcp.lock().await;
        let c = g.as_ref().ok_or("MCP not connected")?;
        let r = c.call_tool(name, arguments).await?;
        if r.is_error { return Err(format!("MCP `{name}` error: {}", r.content.iter().filter_map(|c|c.text.as_deref()).collect::<Vec<_>>().join("\n"))); }
        let ts: Vec<&str> = r.content.iter().filter_map(|c|c.text.as_deref()).collect();
        if ts.len()==1 { serde_json::from_str::<Value>(ts[0]).or_else(|_| Ok(json!({"text":ts[0]}))) } else { Ok(json!({"content":r.content})) }
    }

    pub async fn available_tools(&self) -> Vec<McpToolDef> { self.available_tools.lock().await.clone() }
    pub async fn shutdown(&self) { if let Some(c) = self.mcp.lock().await.take() { c.shutdown().await; } tracing::info!("video channel shut down"); }

    async fn run_npx(&self, args: &[&str], cwd: Option<&Path>) -> Result<std::process::Output, String> {
        let mut cmd = tokio::process::Command::new(&self.config.npx_path);
        cmd.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        if let Some(d) = cwd { cmd.current_dir(d); }
        for (k,v) in &self.config.env { cmd.env(k,v); }
        tokio::time::timeout(std::time::Duration::from_secs(300), cmd.output()).await.map_err(|_|"timeout (300s)".into())?.map_err(|e|format!("npx: {e}"))
    }

    async fn try_mcp_render(&self, project: &VideoProject, output_path: &Path) -> Result<RenderResult, String> {
        self.connect_mcp().await?;
        let r = self.call_mcp_tool("render", json!({"projectDir":project.dir.to_string_lossy(),"output":output_path.to_string_lossy(),"width":project.width,"height":project.height,"fps":project.fps,"duration":project.duration})).await?;
        let ap = r.get("path").and_then(|p|p.as_str()).map(PathBuf::from).unwrap_or_else(||output_path.to_path_buf());
        Ok(RenderResult { path: ap, duration: project.duration, size: std::fs::metadata(&ap).map(|m|m.len()).unwrap_or(0), format: "mp4".into() })
    }

    fn write_project_meta(&self, project: &VideoProject) -> Result<(), String> {
        std::fs::write(project.dir.join("project.json"), serde_json::to_string_pretty(project).map_err(|e|format!("ser: {e}"))?).map_err(|e|format!("write meta: {e}"))
    }

    #[allow(clippy::too_many_arguments)]
    fn write_composition_html(&self, dir: &Path, name: &str, w: u32, h: u32, _d: f64, _f: u32, elems: &[CompositionElement]) -> Result<(), String> {
        let mut eh = String::new();
        for e in elems { eh.push_str(&format!("    {}\n", self.element_to_html(e)?)); }
        std::fs::write(dir.join("index.html"), format!(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>{name}</title><style>*{{margin:0;padding:0;box-sizing:border-box}}#stage{{position:relative;overflow:hidden;background:#000}}</style></head><body><div id="stage" data-composition-id="{name}" data-width="{w}" data-height="{h}">{eh}</div></body></html>"#)).map_err(|e|format!("write html: {e}"))
    }

    fn element_to_html(&self, elem: &CompositionElement) -> Result<String, String> {
        let p = &elem.properties;
        let ba = format!(r#"id="{}" data-start="{}" data-duration="{}" data-track="{}""#, elem.id, elem.start, elem.duration, elem.track);
        match elem.element_type.as_str() {
            "text" => { let t=p.get("text").and_then(|v|v.as_str()).unwrap_or("Hello"); let fs=p.get("fontSize").and_then(|v|v.as_str()).unwrap_or("48px"); let c=p.get("color").and_then(|v|v.as_str()).unwrap_or("#fff"); let x=p.get("x").and_then(|v|v.as_str()).unwrap_or("50%"); let y=p.get("y").and_then(|v|v.as_str()).unwrap_or("50%"); Ok(format!(r#"<div {ba} style="position:absolute;left:{x};top:{y};font-size:{fs};color:{c};transform:translate(-50%,-50%)">{t}</div>"#)) }
            "image" => { let s=p.get("src").and_then(|v|v.as_str()).ok_or("image needs src")?; let st=p.get("style").and_then(|v|v.as_str()).unwrap_or("width:100%;height:100%;object-fit:cover"); Ok(format!(r#"<img {ba} src="{s}" style="{st}" />"#)) }
            "video" => { let s=p.get("src").and_then(|v|v.as_str()).ok_or("video needs src")?; let vol=p.get("volume").and_then(|v|v.as_f64()).unwrap_or(0.0); let va=if vol>0.0{format!(r#" data-volume="{vol}""#)}else{String::new()}; Ok(format!(r#"<video {ba}{va} src="{s}" muted playsinline style="width:100%;height:100%;object-fit:cover" />"#)) }
            "audio" => { let s=p.get("src").and_then(|v|v.as_str()).ok_or("audio needs src")?; let vol=p.get("volume").and_then(|v|v.as_f64()).unwrap_or(1.0); Ok(format!(r#"<audio {ba} data-volume="{vol}" src="{s}" />"#)) }
            "shape" => { let bg=p.get("backgroundColor").and_then(|v|v.as_str()).unwrap_or("#333"); let w=p.get("width").and_then(|v|v.as_str()).unwrap_or("100%"); let h=p.get("height").and_then(|v|v.as_str()).unwrap_or("100%"); let x=p.get("x").and_then(|v|v.as_str()).unwrap_or("0"); let y=p.get("y").and_then(|v|v.as_str()).unwrap_or("0"); Ok(format!(r#"<div {ba} style="position:absolute;left:{x};top:{y};width:{w};height:{h};background:{bg}"></div>"#)) }
            "html" => Ok(p.get("html").and_then(|v|v.as_str()).unwrap_or("<div></div>").to_string()),
            o => Err(format!("unknown element type `{o}`"))
        }
    }
}
