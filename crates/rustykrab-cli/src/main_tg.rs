
async fn telegram_agent_loop(
    mut rx: mpsc::Receiver<ChannelMessage>,
    tg: Arc<TelegramChannel>,
    state: AppState,
) {
    let chat_states: Arc<tokio::sync::Mutex<HashMap<(i64, i64), ChatState>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while let Some(channel_msg) = rx.recv().await {
        let chat_id = channel_msg.chat_id;
        let thread_id = channel_msg.thread_id;
        let key = (chat_id, thread_id);
        let tg = tg.clone();
        let state = state.clone();
        let chat_states = chat_states.clone();

        tokio::spawn(async move {
            // Check if this chat/thread already has an agent run in progress.
            {
                let states = chat_states.lock().await;
                if let Some(cs) = states.get(&key) {
                    if cs.busy {
                        let _ = tg
                            .send_text(
                                chat_id,
                                "I'm still working on your previous message. Please wait.",
                                thread_id,
                            )
                            .await;
                        return;
                    }
                }
            }

            // Handle conversation reset via structured flag (not sentinel string).
            if channel_msg.reset {
                {
                    let mut states = chat_states.lock().await;
                    states.remove(&key);
                }
                // Also clear the persisted mapping.
                if let Err(e) = state.store.chat_map().remove(chat_id, thread_id) {
                    tracing::warn!(chat_id, thread_id, "failed to remove chat map entry: {e}");
                }
                return;
            }

            let user_text = match &channel_msg.message.content {
                MessageContent::Text(t) => t.clone(),
                _ => return,
            };

            // Get or create conversation. Check in-memory first, then DB,
            // then create a brand new one.
            let conv_id = {
                let mut states = chat_states.lock().await;
                match states.get(&key) {
                    Some(cs) => cs.conv_id,
                    None => {
                        // Try the database (survives restarts).
                        let db_id = state
                            .store
                            .chat_map()
                            .lookup(chat_id, thread_id)
                            .ok()
                            .flatten();

                        match db_id {
                            Some(id) => {
                                states.insert(
                                    key,
                                    ChatState {
                                        conv_id: id,
                                        busy: false,
                                    },
                                );
                                tracing::info!(
                                    chat_id, thread_id, conv_id = %id,
                                    "restored conversation from database"
                                );
                                id
                            }
                            None => match state.store.conversations().create() {
                                Ok(mut conv) => {
                                    conv.channel_source = Some("telegram".to_string());
                                    conv.channel_id = Some(chat_id.to_string());
                                    if thread_id != 0 {
                                        conv.channel_thread_id = Some(thread_id.to_string());
                                    }
                                    if let Err(e) = state.store.conversations().save(&conv) {
                                        tracing::warn!(
                                            chat_id,
                                            "failed to persist channel metadata: {e}"
                                        );
                                    }
                                    let id = conv.id;
                                    states.insert(
                                        key,
                                        ChatState {
                                            conv_id: id,
                                            busy: false,
                                        },
                                    );
                                    // Persist the mapping so it survives restarts.
                                    if let Err(e) =
                                        state.store.chat_map().upsert(chat_id, thread_id, id)
                                    {
                                        tracing::warn!(
                                            chat_id,
                                            thread_id,
                                            "failed to persist chat map: {e}"
                                        );
                                    }
                                    tracing::info!(
                                        chat_id, thread_id, conv_id = %id,
                                        "created new conversation for Telegram chat/thread"
                                    );
                                    id
                                }
                                Err(e) => {
                                    tracing::error!(
                                        chat_id,
                                        thread_id,
                                        "failed to create conversation: {e}"
                                    );
                                    let _ = tg
                                        .send_text(
                                            chat_id,
                                            "Internal error — please try again.",
                                            thread_id,
                                        )
                                        .await;
                                    return;
                                }
                            },
                        }
                    }
                }
            };

            // Mark chat/thread as busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&key) {
                    cs.busy = true;
                }
            }

            let reply = process_telegram_message(
                &state,
                &tg,
                chat_id,
                thread_id,
                conv_id,
                channel_msg.message,
                &user_text,
            )
            .await;

            // Mark chat/thread as no longer busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&key) {
                    cs.busy = false;
                }
            }

            // Send response back to Telegram (in the correct thread).
            if let Err(e) = tg.send_text(chat_id, &reply, thread_id).await {
                tracing::error!(chat_id, thread_id, "failed to send Telegram reply: {e}");
            }
            media_delivery::send_media_attachments(&tg, chat_id, thread_id, &reply).await;
        });
    }

    tracing::warn!("Telegram agent loop exited — inbound channel closed");
}

async fn process_telegram_message(
    state: &AppState,
    tg: &Arc<TelegramChannel>,
    chat_id: i64,
    thread_id: i64,
    conv_id: Uuid,
    message: rustykrab_core::types::Message,
    user_text: &str,
) -> String {
    // Load the conversation.
    let mut conv = match state.store.conversations().get(conv_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(chat_id, thread_id, %conv_id, "failed to load conversation: {e}");
            return "Internal error — please try again.".to_string();
        }
    };

    // Ensure channel metadata is present (backfills conversations created
    // before this field was populated).
    if conv.channel_source.is_none() {
        conv.channel_source = Some("telegram".to_string());
        conv.channel_id = Some(chat_id.to_string());
        if thread_id != 0 {
            conv.channel_thread_id = Some(thread_id.to_string());
        }
    }
    if conv.channel_thread_id.is_none() && thread_id != 0 {
        conv.channel_thread_id = Some(thread_id.to_string());
    }

    // Append user message.
    conv.messages.push(message);
    conv.updated_at = Utc::now();

    // Send initial typing indicator (scoped to forum thread if applicable).
    let _ = tg.send_typing(chat_id, thread_id).await;

    // Spawn a background task to keep re-sending typing indicators
    // while the agent is working.
    let typing_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let typing_flag = typing_active.clone();
    let tg_typing = tg.clone();
    let typing_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(TYPING_INTERVAL_SECS)).await;
            if !typing_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let _ = tg_typing.send_typing(chat_id, thread_id).await;
        }
    });

    // Run agent with heartbeat-based timeout.
    let last_heartbeat = Arc::new(AtomicU64::new(epoch_millis()));
    let hb = last_heartbeat.clone();

    let on_event = move |_event: AgentEvent| {
        hb.store(epoch_millis(), Ordering::Relaxed);
    };

    let agent_fut = rustykrab_gateway::run_agent_streaming(state, &mut conv, user_text, &on_event);

    let timeout_millis = HEARTBEAT_TIMEOUT_SECS * 1000;
    let heartbeat_monitor = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let last = last_heartbeat.load(Ordering::Relaxed);
            if epoch_millis() - last > timeout_millis {
                break;
            }
        }
    };

    let reply = tokio::select! {
        result = agent_fut => {
            match result {
                Ok(assistant_msg) => {
                    match &assistant_msg.content {
                        MessageContent::Text(t) => t.clone(),
                        _ => "I processed your message but have no text response.".to_string(),
                    }
                }
                Err(_status) => {
                    tracing::error!(chat_id, %conv_id, "agent returned error");
                    "Sorry, I encountered an error processing your message.".to_string()
                }
            }
        }
        _ = heartbeat_monitor => {
            tracing::error!(
                chat_id, %conv_id,
                "agent stalled — no activity for {HEARTBEAT_TIMEOUT_SECS}s"
            );
            "Sorry, the agent appears to have stalled. Please try again.".to_string()
        }
    };

    // Stop typing indicator.
    typing_active.store(false, std::sync::atomic::Ordering::Relaxed);
    typing_task.abort();

    // Persist conversation.
    if let Err(e) = state.store.conversations().save(&conv) {
        tracing::error!(chat_id, %conv_id, "failed to persist conversation: {e}");
    }

    reply
}
