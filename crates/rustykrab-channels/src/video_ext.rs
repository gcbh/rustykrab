
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
        std::fs::write(project.dir.join("project.json"), serde_json::to_string_pretty(project).map_err(|e|format!("ser: {e}"))?).map_err(|e|format!("write: {e}"))
    }

    #[allow(clippy::too_many_arguments)]
    fn write_composition_html(&self, dir: &Path, name: &str, w: u32, h: u32, _d: f64, _f: u32, elems: &[CompositionElement]) -> Result<(), String> {
        let mut eh = String::new();
        for e in elems { eh.push_str(&format!("    {}\n", self.element_to_html(e)?)); }
        std::fs::write(dir.join("index.html"), format!(r#"<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{name}</title><style>*{{margin:0;padding:0;box-sizing:border-box}}#stage{{position:relative;overflow:hidden;background:#000}}</style></head><body><div id=\"stage\" data-composition-id=\"{name}\" data-width=\"{w}\" data-height=\"{h}\">{eh}</div></body></html>"#)).map_err(|e|format!("write html: {e}"))
    }

    fn element_to_html(&self, elem: &CompositionElement) -> Result<String, String> {
        let p = &elem.properties;
        let ba = format!(r#"id=\"{}\" data-start=\"{}\" data-duration=\"{}\" data-track=\"{}\""#, elem.id, elem.start, elem.duration, elem.track);
        match elem.element_type.as_str() {
            "text" => { let t=p.get("text").and_then(|v|v.as_str()).unwrap_or("Hello"); let fs=p.get("fontSize").and_then(|v|v.as_str()).unwrap_or("48px"); let c=p.get("color").and_then(|v|v.as_str()).unwrap_or("#fff"); let x=p.get("x").and_then(|v|v.as_str()).unwrap_or("50%"); let y=p.get("y").and_then(|v|v.as_str()).unwrap_or("50%"); Ok(format!(r#"<div {ba} style=\"position:absolute;left:{x};top:{y};font-size:{fs};color:{c};transform:translate(-50%,-50%)\">{t}</div>"#)) }
            "image" => { let s=p.get("src").and_then(|v|v.as_str()).ok_or("image needs src")?; Ok(format!(r#"<img {ba} src=\"{s}\" style=\"width:100%;height:100%;object-fit:cover\" />"#)) }
            "video" => { let s=p.get("src").and_then(|v|v.as_str()).ok_or("video needs src")?; let vol=p.get("volume").and_then(|v|v.as_f64()).unwrap_or(0.0); let va=if vol>0.0{format!(r#" data-volume=\"{vol}\""#)}else{String::new()}; Ok(format!(r#"<video {ba}{va} src=\"{s}\" muted playsinline style=\"width:100%;height:100%;object-fit:cover\" />"#)) }
            "audio" => { let s=p.get("src").and_then(|v|v.as_str()).ok_or("audio needs src")?; let vol=p.get("volume").and_then(|v|v.as_f64()).unwrap_or(1.0); Ok(format!(r#"<audio {ba} data-volume=\"{vol}\" src=\"{s}\" />"#)) }
            "shape" => { let bg=p.get("backgroundColor").and_then(|v|v.as_str()).unwrap_or("#333"); let w=p.get("width").and_then(|v|v.as_str()).unwrap_or("100%"); let h=p.get("height").and_then(|v|v.as_str()).unwrap_or("100%"); let x=p.get("x").and_then(|v|v.as_str()).unwrap_or("0"); let y=p.get("y").and_then(|v|v.as_str()).unwrap_or("0"); Ok(format!(r#"<div {ba} style=\"position:absolute;left:{x};top:{y};width:{w};height:{h};background:{bg}\"></div>"#)) }
            "html" => Ok(p.get("html").and_then(|v|v.as_str()).unwrap_or("<div></div>").to_string()),
            o => Err(format!("unknown element type `{o}`"))
        }
    }
