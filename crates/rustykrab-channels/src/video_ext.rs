
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

    pub async fn call_mcp_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let mcp_guard = self.mcp.lock().await;
        let client = mcp_guard
            .as_ref()
            .ok_or("MCP not connected \u2014 call connect_mcp first, or set mcp_enabled=true")?;

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

    pub async fn available_tools(&self) -> Vec<McpToolDef> {
        self.available_tools.lock().await.clone()
    }

    pub async fn shutdown(&self) {
        let mut mcp_guard = self.mcp.lock().await;
        if let Some(client) = mcp_guard.take() {
            client.shutdown().await;
        }
        tracing::info!("video channel shut down");
    }

    // --- Private helpers ---

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

    fn write_project_meta(&self, project: &VideoProject) -> Result<(), String> {
        let meta = serde_json::to_string_pretty(project)
            .map_err(|e| format!("failed to serialize project meta: {e}"))?;
        let meta_path = project.dir.join("project.json");
        std::fs::write(meta_path, meta)
            .map_err(|e| format!("failed to write project metadata: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
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
                "unknown element type `{other}` \u2014 use text, image, video, audio, shape, or html"
            )),
        }
    }
