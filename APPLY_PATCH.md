# Remaining manual changes for main.rs

The MCP API cannot push files >15KB. main.rs (55KB) needs 2 small additions:

## 1. Add module declaration (line ~35, after `use uuid::Uuid;`)

```rust
mod media_delivery;
```

## 2. Add media attachment call (after the `send_text` reply in `telegram_agent_loop`, around line 810)

After this line:
```rust
            if let Err(e) = tg.send_text(chat_id, &reply, thread_id).await {
                tracing::error!(chat_id, thread_id, "failed to send Telegram reply: {e}");
            }
```

Add:
```rust
            media_delivery::send_media_attachments(&tg, chat_id, thread_id, &reply).await;
```

The `media_delivery.rs` file is already pushed to this branch.
