use std::sync::Arc;
use rustykrab_channels::TelegramChannel;
use rustykrab_core::Result;

pub async fn send_media_attachments(tg: &Arc<TelegramChannel>, chat_id: i64, thread_id: i64, reply: &str) {
    let exts = [".mp4", ".webm", ".wav", ".mp3", ".png", ".jpg"];
    for word in reply.split_whitespace() {
        let cleaned = word.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == '(' || c == ')' || c == '[' || c == ']');
        if !exts.iter().any(|ext| cleaned.ends_with(ext)) { continue; }
        let path = std::path::Path::new(cleaned);
        if !path.is_absolute() || !path.exists() { continue; }
        let is_video = cleaned.ends_with(".mp4") || cleaned.ends_with(".webm");
        let result = if is_video { tg.send_video(chat_id, path, None, thread_id).await } else { tg.send_document(chat_id, path, None, thread_id).await };
        match result {
            Ok(()) => tracing::info!(chat_id, path = %path.display(), "sent media attachment"),
            Err(e) => tracing::error!(chat_id, path = %path.display(), "media attachment failed: {e}"),
        }
    }
}
