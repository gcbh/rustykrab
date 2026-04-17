
    // --- Video channel (optional, enabled via RUSTYKRAB_VIDEO=true) ---
    let video_enabled = std::env::var("RUSTYKRAB_VIDEO")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let video_channel: Option<Arc<VideoChannel>> = if video_enabled {
        let video_dir = data_dir.join("video");
        std::fs::create_dir_all(&video_dir)?;

        let mut video_config = VideoConfig {
            projects_dir: video_dir,
            ..Default::default()
        };

        // Allow custom npx path via env.
        if let Ok(npx) = std::env::var("RUSTYKRAB_NPX_PATH") {
            video_config.npx_path = npx;
        }

        let channel = Arc::new(VideoChannel::new(video_config));
        tracing::info!("video communication channel enabled");
        Some(channel)
    } else {
        tracing::info!("video channel disabled (set RUSTYKRAB_VIDEO=true to enable)");
        None
    };

    // --- Tools ---
    let mut tools = rustykrab_tools::builtin_tools(store.secrets());
    tools.extend(rustykrab_tools::memory_tools(memory_backend));
    tools.extend(rustykrab_tools::skill_tools(skills_dir.clone()));

    // --- Cron tool (task scheduling) ---
    let cron_backend: Arc<dyn CronBackend> = Arc::new(CronAdapter {
        store: store.clone(),
    });
    tools.push(Arc::new(rustykrab_tools::CronTool::new(cron_backend)));
    tracing::info!("cron tool registered");

    // --- Video tool (if video channel enabled) ---
    if let Some(ref vc) = video_channel {
        let video_backend: Arc<dyn rustykrab_tools::VideoBackend> =
            Arc::new(rustykrab_tools::VideoChannelAdapter::new(vc.clone()));
        tools.extend(rustykrab_tools::video_tools(video_backend));
        tracing::info!("video tool registered");
    }

    // --- Log provider status ---
    tracing::info!(provider = provider.name(), "model provider configured");

    // --- Harness router (auto-selects profile per message) ---
    // Reuses the main provider for classification to avoid model swapping.
    // The classification prompt is ~50 tokens — negligible overhead on any model.
    let classifier: Arc<dyn ModelProvider> = provider.clone();

    let router = Arc::new(HarnessRouter::new(classifier).with_base(profile));

    // --- Orchestration config (used by RLM module) ---
    let orchestration_config = load_orchestration_config(&data_dir)?;

    // --- Load SKILL.md skills ---
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;
    let mut skill_registry = SkillRegistry::new();
    match rustykrab_skills::load_skills_from_dir(&skills_dir) {
        Ok(loaded) => {
            let count = loaded.len();
            for s in loaded {
                skill_registry.register_md(Arc::new(s));
            }
            if count > 0 {
                tracing::info!(count, "SKILL.md skills loaded");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to scan skills directory"),
    }

    // --- Build gateway state ---
    // Clone store handle so we can flush it after the server shuts down.
    let store_handle = store.clone();
    let mut state = rustykrab_gateway::AppState::new(store, tools, provider, auth_token)
        .with_harness_router(router)
        .with_orchestration_config(orchestration_config)
        .with_skill_registry(Arc::new(skill_registry));

    // --- Attach video channel to state ---
    if let Some(vc) = video_channel {
        state = state.with_video(vc);
    }

    // Track infrastructure task JoinHandles so panics are surfaced
    // instead of silently swallowed (fixes ASYNC-H4).
    let mut infra_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // --- Telegram channel (optional) ---
    // We need to take the inbound_rx before wrapping in Arc, so build in stages.
    let mut telegram_rx: Option<mpsc::Receiver<ChannelMessage>> = None;
    let mut telegram_arc: Option<Arc<TelegramChannel>> = None;

    if let Ok(bot_token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        let allowed_chats: HashSet<i64> = std::env::var("TELEGRAM_ALLOWED_CHATS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();

        let webhook_secret = std::env::var("TELEGRAM_WEBHOOK_SECRET").ok();

        let mut tg = TelegramChannel::new(bot_token, allowed_chats.clone());
        if let Some(secret) = webhook_secret {
            tg = tg.with_webhook_secret(secret);
        }

        // Take rx before wrapping in Arc (requires &mut self).
        telegram_rx = tg.take_inbound_rx();

        let tg = Arc::new(tg);
        telegram_arc = Some(tg.clone());
        state = state.with_telegram(tg.clone());

        if let Ok(webhook_url) = std::env::var("TELEGRAM_WEBHOOK_URL") {
            tg.set_webhook(&webhook_url).await?;
            tracing::info!("Telegram: webhook mode");
        } else {
            let tg_poll = tg.clone();
            // Store handle so panics are not silently swallowed (fixes ASYNC-H4).
            infra_handles.push(tokio::spawn(async move {
                if let Err(e) = tg_poll.start_polling().await {
                    tracing::error!("Telegram polling error: {e}");
                }
            }));
            tracing::info!("Telegram: long-polling mode");
        }

        if allowed_chats.is_empty() {
            tracing::warn!(
                "TELEGRAM_ALLOWED_CHATS not set — bot will deny all chats. \
                 Set it to a comma-separated list of chat IDs."
            );
        } else {
            tracing::info!(chats = ?allowed_chats, "Telegram allowed chats configured");
        }
    }

    // --- Signal channel (optional) ---
    if let Ok(account_number) = std::env::var("SIGNAL_ACCOUNT") {
        let base_url =
            std::env::var("SIGNAL_CLI_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());

        let allowed_numbers: HashSet<String> = std::env::var("SIGNAL_ALLOWED_NUMBERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let webhook_secret = std::env::var("SIGNAL_WEBHOOK_SECRET").ok();

        let mut sig = rustykrab_channels::SignalChannel::new(
            base_url.clone(),
            account_number.clone(),
            allowed_numbers.clone(),
        );
        if let Some(secret) = webhook_secret {
            sig = sig.with_webhook_secret(secret);
        }

        let sig = Arc::new(sig);
        state = state.with_signal(sig.clone());

        // Health check — verify signal-cli-rest-api is running.
        match sig.health_check().await {
            Ok(()) => tracing::info!("signal-cli-rest-api connected"),
            Err(e) => tracing::error!("signal-cli-rest-api not reachable: {e}"),
        }

        // Webhook or polling mode.
        if let Ok(webhook_url) = std::env::var("SIGNAL_WEBHOOK_URL") {
            if let Err(e) = sig.register_webhook(&webhook_url).await {
                tracing::error!("failed to register Signal webhook: {e}");
            } else {
                tracing::info!("Signal: webhook mode");
            }
        } else {
            let sig_poll = sig.clone();
            // Store handle so panics are not silently swallowed (fixes ASYNC-H4).
            infra_handles.push(tokio::spawn(async move {
                if let Err(e) = sig_poll.start_polling().await {
                    tracing::error!("Signal polling error: {e}");
                }
            }));
            tracing::info!("Signal: polling mode");
        }

        if allowed_numbers.is_empty() {
            tracing::warn!(
                "SIGNAL_ALLOWED_NUMBERS not set — bot will deny all messages. \
                 Set it to a comma-separated list of E.164 phone numbers."
            );
        } else {
            tracing::info!(
                numbers = ?allowed_numbers,
                "Signal allowed numbers configured"
            );
        }
    }
