
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    tracing::info!("shutdown signal received");
}

async fn job_executor_loop(store: rustykrab_store::Store, queue: task_queue::TaskQueue) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        interval.tick().await;

        let now = Utc::now();
        let due_jobs = match store.jobs().get_due_jobs(now) {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!(error = %e, "failed to query due jobs");
                continue;
            }
        };

        for job in due_jobs {
            let request = task_queue::TaskRequest {
                prompt: job.task.clone(),
                source: task_queue::TaskSource::Cron {
                    job_id: job.id.clone(),
                    channel: job.channel.clone(),
                    chat_id: job.chat_id.clone(),
                },
                dedupe_key: Some(format!("cron:{}", job.id)),
            };

            if let Err(e) = queue.submit(request).await {
                tracing::error!(job_id = %job.id, "failed to submit job to task queue: {e}");
            }
        }
    }
}

fn load_orchestration_config(data_dir: &std::path::Path) -> anyhow::Result<OrchestrationConfig> {
    let config_path = data_dir.join("orchestration.toml");
    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)?;
        let config: OrchestrationConfig = toml::from_str(&contents)?;
        tracing::info!("loaded orchestration config from {}", config_path.display());
        return Ok(config);
    }

    let mut config = OrchestrationConfig::default();

    // Allow env var overrides for key settings.
    if let Ok(val) = std::env::var("ORCHESTRATION_MAX_RECURSION_DEPTH") {
        if let Ok(depth) = val.parse() {
            config.max_recursion_depth = depth;
        }
    }
    if let Ok(val) = std::env::var("ORCHESTRATION_CONSISTENCY_SAMPLES") {
        if let Ok(samples) = val.parse() {
            config.consistency_samples = samples;
        }
    }
    if let Ok(val) = std::env::var("ORCHESTRATION_PIPELINE_TIMEOUT_SECS") {
        if let Ok(secs) = val.parse() {
            config.pipeline_timeout_secs = secs;
        }
    }
    if let Ok(val) = std::env::var("ORCHESTRATION_MODEL_CALL_TIMEOUT_SECS") {
        if let Ok(secs) = val.parse() {
            config.model_call_timeout_secs = secs;
        }
    }

    Ok(config)
}

fn load_harness_profile(data_dir: &std::path::Path) -> anyhow::Result<HarnessProfile> {
    let profile_path = data_dir.join("harness.toml");
    if profile_path.exists() {
        let contents = std::fs::read_to_string(&profile_path)?;
        let profile: HarnessProfile = toml::from_str(&contents)?;
        return Ok(profile);
    }

    let preset = std::env::var("RUSTYKRAB_HARNESS").unwrap_or_else(|_| "default".to_string());
    let profile = match preset.to_lowercase().as_str() {
        "coding" => HarnessProfile::coding(),
        "research" => HarnessProfile::research(),
        "creative" => HarnessProfile::creative(),
        _ => HarnessProfile::default(),
    };

    Ok(profile)
}

fn handle_skill_subcommand(data_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" => {
            let skills = rustykrab_skills::load_skills_from_dir(&skills_dir)?;
            if skills.is_empty() {
                println!("No skills installed.");
                println!("  Skills directory: {}", skills_dir.display());
                println!("  Place skill directories containing SKILL.md here.");
                return Ok(());
            }
            println!("{:<24} {:<10} DESCRIPTION", "NAME", "STATUS");
            println!("{}", "-".repeat(60));
            for s in &skills {
                let status = if s.validation.is_satisfied() {
                    "ready"
                } else {
                    "unmet"
                };
                println!(
                    "{:<24} {:<10} {}",
                    s.frontmatter.name, status, s.frontmatter.description
                );
                if !s.validation.missing_env.is_empty() {
                    println!("  missing env: {}", s.validation.missing_env.join(", "));
                }
                if !s.validation.missing_bins.is_empty() {
                    println!("  missing bins: {}", s.validation.missing_bins.join(", "));
                }
            }
        }
        "install" => {
            let src = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: skill install <path>"))?;
            let src_path = std::path::Path::new(src);
            if !src_path.is_dir() {
                anyhow::bail!("source path is not a directory: {}", src_path.display());
            }
            let skill_md = src_path.join("SKILL.md");
            if !skill_md.is_file() {
                anyhow::bail!("no SKILL.md found in {}", src_path.display());
            }
            let name = src_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("cannot determine skill name from path"))?;
            let dest = skills_dir.join(name);
            copy_dir_recursive(src_path, &dest)?;
            println!("Installed skill to {}", dest.display());
        }
        _ => {
            eprintln!("Unknown skill subcommand: {sub}");
            eprintln!("Usage:");
            eprintln!("  rustykrab-cli skill list              List installed skills");
            eprintln!("  rustykrab-cli skill install <path>    Install a skill directory");
            std::process::exit(1);
        }
    }
    Ok(())
}

// --- Credential resolution helpers ---
// These use the central registry (rustykrab_store::registry) for the
// env-var → OS-keychain → SecretStore lookup chain.

fn resolve_auth_token(store: &rustykrab_store::Store) -> String {
    let spec = rustykrab_store::registry::lookup("rustykrab_auth_token")
        .expect("rustykrab_auth_token must be in the registry");

    if let Some(token) = rustykrab_store::registry::resolve(spec, &store.secrets()) {
        tracing::info!("auth token resolved via registry");
        return token;
    }

    // Not found anywhere — generate a new token and persist it.
    let token = rustykrab_gateway::generate_token();
    tracing::info!("generated new auth token — persisting for future restarts");
    println!("\n  Auth token (also saved to credential store): {token}\n");

    let svc = rustykrab_store::registry::keychain_service();
    if rustykrab_store::keychain::keychain_available() {
        let _ = rustykrab_store::keychain::set_credential(svc, spec.keychain_account, &token);
    }
    let _ = store.secrets().set(spec.store_name, &token);
    token
}

fn resolve_api_key(store: &rustykrab_store::Store) -> String {
    let spec = rustykrab_store::registry::lookup("anthropic_api_key")
        .expect("anthropic_api_key must be in the registry");

    if let Some(key) = rustykrab_store::registry::resolve(spec, &store.secrets()) {
        tracing::info!("API key resolved via registry");
        return key;
    }

    tracing::error!(
        "ANTHROPIC_API_KEY not set. Set the env var, store it via the secrets API, \
         or add it to the OS credential store (rustykrab-cli keychain set {}).",
        spec.keychain_account,
    );
    String::new()
}
