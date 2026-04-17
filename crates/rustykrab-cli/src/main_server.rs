
    // --- Spawn Telegram agent loop (after state is fully built) ---
    if let (Some(rx), Some(tg)) = (telegram_rx, telegram_arc) {
        // Store handle so panics are not silently swallowed (fixes ASYNC-H4).
        infra_handles.push(tokio::spawn(telegram_agent_loop(rx, tg, state.clone())));
        tracing::info!("Telegram agent loop started");
    }

    // --- Task queue (bounded concurrency for background work) ---
    let max_concurrent: usize = std::env::var("RUSTYKRAB_MAX_CONCURRENT_TASKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let (task_queue, queue_handle) = task_queue::TaskQueue::spawn(
        64, // buffer capacity
        max_concurrent,
        state.clone(),
        store_handle.clone(),
    );
    infra_handles.push(queue_handle);
    tracing::info!(max_concurrent, "task queue started");

    // --- Job executor (scheduled task runner) ---
    {
        let executor_store = store_handle.clone();
        let executor_queue = task_queue.clone();
        infra_handles.push(tokio::spawn(async move {
            job_executor_loop(executor_store, executor_queue).await;
        }));
        tracing::info!("job executor started (30s poll interval)");
    }

    // Save a reference to the video channel for shutdown.
    let video_shutdown_handle = state.video.clone();

    // --- Gateway with security middleware ---
    let app = rustykrab_gateway::router(state);

    // Bind to loopback only — never 0.0.0.0.
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!(%addr, "RustyKrab gateway listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    server.await?;

    // Abort infrastructure tasks and log any panics.
    for handle in &infra_handles {
        handle.abort();
    }
    for handle in infra_handles {
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!("infrastructure task panicked during shutdown: {e}");
            }
        }
    }

    // Finalize memory session: Working → Episodic + lifecycle sweep.
    idle_sweep_handle.abort();
    tracing::info!("finalizing memory session...");
    if let Err(e) = memory_system.finalize_session(agent_id, session_id).await {
        tracing::warn!(error = %e, "failed to finalize memory session");
    }
    if let Err(e) = memory_system.lifecycle_sweep(agent_id).await {
        tracing::warn!(error = %e, "shutdown lifecycle sweep failed");
    }

    // Shut down video channel (MCP server).
    if let Some(ref vc) = video_shutdown_handle {
        tracing::info!("shutting down video channel...");
        vc.shutdown().await;
    }

    // Flush database before exit
    tracing::info!("flushing database...");
    store_handle
        .flush()
        .map_err(|e| anyhow::anyhow!("flush failed: {e}"))?;
    tracing::info!("shutdown complete");

    Ok(())
}

const HEARTBEAT_TIMEOUT_SECS: u64 = 1800; // 30 minutes

const TYPING_INTERVAL_SECS: u64 = 4;

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct ChatState {
    conv_id: Uuid,
    busy: bool,
}
