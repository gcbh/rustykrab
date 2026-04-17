
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

fn handle_keychain_subcommand(data_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");

    match sub {
        "status" => {
            let available = rustykrab_store::keychain::keychain_available();
            println!(
                "OS credential store: {}",
                if available {
                    if cfg!(target_os = "macos") {
                        "available (macOS Data Protection Keychain)"
                    } else {
                        "available (Secret Service)"
                    }
                } else {
                    "not available"
                }
            );

            if !available {
                println!(
                    "\nNo OS credential store detected. Use environment variables \
                     or the encrypted store instead."
                );
                return Ok(());
            }

            // Master key (separate service).
            let master_status = match rustykrab_store::keychain::get_credential(
                "com.rustykrab.master-key",
                "rustykrab-encryption-key",
            ) {
                Ok(Some(_)) => "present",
                Ok(None) => "not set",
                Err(_) => "error",
            };

            let svc = rustykrab_store::registry::keychain_service();
            println!(
                "\n{:<25} {:<25} {:<10} STATUS",
                "CREDENTIAL", "ACCOUNT", "REQUIRED"
            );
            println!("{}", "-".repeat(75));
            println!(
                "{:<25} {:<25} {:<10} {}",
                "Master key", "rustykrab-encryption-key", "-", master_status
            );

            // All registry entries.
            for spec in rustykrab_store::registry::REGISTRY {
                let status =
                    match rustykrab_store::keychain::get_credential(svc, spec.keychain_account) {
                        Ok(Some(_)) => "present",
                        Ok(None) => "not set",
                        Err(_) => "error",
                    };
                let req = if spec.required { "yes" } else { "no" };
                println!(
                    "{:<25} {:<25} {:<10} {}",
                    spec.description, spec.keychain_account, req, status
                );
            }
            println!();
        }

        "set" => {
            // Build the list of accepted names from the registry.
            let known_names: Vec<&str> = rustykrab_store::registry::REGISTRY
                .iter()
                .map(|s| s.keychain_account)
                .collect();
            let names_list = known_names.join(", ");

            let name = args.get(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: rustykrab-cli keychain set <name> <value>\n  \
                     names: {names_list}, or <service>:<account>"
                )
            })?;
            let value = args.get(2).ok_or_else(|| {
                anyhow::anyhow!("usage: rustykrab-cli keychain set <name> <value>")
            })?;

            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!("OS credential store is not available on this platform");
            }

            let svc = rustykrab_store::registry::keychain_service();

            // Look up the name in the registry first, then fall back to
            // arbitrary service:account pairs.
            let (service, account, store_name) =
                if let Some(spec) = rustykrab_store::registry::lookup_by_account(name) {
                    (svc, spec.keychain_account, Some(spec.store_name))
                } else if let Some((s, a)) = name.split_once(':') {
                    (s, a, None)
                } else {
                    anyhow::bail!(
                        "unknown credential name '{name}'. \
                         Use: {names_list}, or <service>:<account>"
                    );
                };

            rustykrab_store::keychain::set_credential(service, account, value)
                .map_err(|e| anyhow::anyhow!("failed to store: {e}"))?;
            println!("Stored in credential store: {service}/{account}");

            // Also persist to the encrypted store if the DB exists.
            if let Some(sn) = store_name {
                let db_path = data_dir.join("db");
                if db_path.exists() {
                    if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                        if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                            let _ = store.secrets().set(sn, value);
                            println!("Also stored in encrypted store as '{sn}'");
                        }
                    }
                }
            }
        }

        "migrate" => {
            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!("OS credential store is not available on this platform");
            }

            println!("Migrating credentials to OS credential store...\n");

            // Re-resolve master key — this ensures it is stored in the credential store.
            match rustykrab_store::keychain::resolve_master_key() {
                Ok(_) => println!("  master key: OK"),
                Err(e) => println!("  master key: FAILED ({e})"),
            }

            // Migrate all registry secrets from the encrypted store.
            let db_path = data_dir.join("db");
            if db_path.exists() {
                if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                    if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                        let svc = rustykrab_store::registry::keychain_service();
                        for spec in rustykrab_store::registry::REGISTRY {
                            if let Ok(val) = store.secrets().get(spec.store_name) {
                                match rustykrab_store::keychain::set_credential(
                                    svc,
                                    spec.keychain_account,
                                    &val,
                                ) {
                                    Ok(()) => {
                                        println!("  {}: migrated", spec.description)
                                    }
                                    Err(e) => {
                                        println!("  {}: FAILED ({e})", spec.description)
                                    }
                                }
                            } else {
                                println!(
                                    "  {}: not in store (set via 'keychain set {}')",
                                    spec.description, spec.keychain_account
                                );
                            }
                        }
                    }
                }
            } else {
                println!(
                    "  No database found at {} — skipping store migration",
                    db_path.display()
                );
            }

            println!("\nMigration complete. Restart rustykrab-cli to verify.");
        }

        _ => {
            eprintln!("Unknown keychain subcommand: {sub}");
            eprintln!("Usage:");
            eprintln!("  rustykrab-cli keychain status              Show credential store status");
            eprintln!("  rustykrab-cli keychain set <name> <value>  Store a credential");
            eprintln!(
                "  rustykrab-cli keychain migrate             Migrate store to OS credential store"
            );
            eprintln!();
            eprint!("Credential names: ");
            let names: Vec<&str> = rustykrab_store::registry::REGISTRY
                .iter()
                .map(|s| s.keychain_account)
                .collect();
            eprintln!("{}, or <service>:<account>", names.join(", "));
            std::process::exit(1);
        }
    }

    Ok(())
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
