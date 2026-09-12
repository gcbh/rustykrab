//! Durable channel admission, separate from a compactable conversation.
//! Retention is not evidence of processing or exactly-once external effects.

use crate::{with_conn, ChannelAddress};
use rusqlite::{params, OptionalExtension};
use rustykrab_core::{
    types::{Message, Role},
    Error,
};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone)]
pub struct InboundStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl InboundStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Commit the original input before handing it to a running task. An exact
    /// UUID replay is a no-op; a conflicting reuse is rejected. This does not
    /// deduplicate upstream events that have been assigned different UUIDs.
    pub async fn accept(&self, address: &ChannelAddress, message: &Message) -> Result<bool, Error> {
        if message.role != Role::User {
            return Err(Error::Storage("only user input can be admitted".into()));
        }
        let channel = address.channel();
        let key = address.external_key();
        let id = message.id.to_string();
        let data = serde_json::to_string(message)?;
        with_conn(&self.conn,move |conn| {
            let old:Option<(String,String,String)>=conn.query_row(
                "SELECT channel,external_key,data FROM channel_inbound WHERE message_id=?1",[&id],
                |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(sql_error)?;
            if let Some(old)=old {
                if old!=(channel.into(),key,data) { return Err(Error::Storage("inbound UUID reused with different address or data".into())); }
                return Ok(false);
            }
            conn.execute("INSERT INTO channel_inbound(message_id,channel,external_key,data,status) VALUES(?1,?2,?3,?4,'accepted')",params![id,channel,key,data]).map_err(sql_error)?;
            Ok(true)
        }).await
    }

    pub async fn assign(&self, id: Uuid, conversation: Uuid) -> Result<(), Error> {
        with_conn(&self.conn,move |conn| {
            let changed=conn.execute("UPDATE channel_inbound SET conversation_id=?2 WHERE message_id=?1 AND (conversation_id IS NULL OR conversation_id=?2)",params![id.to_string(),conversation.to_string()]).map_err(sql_error)?;
            if changed!=1 { return Err(Error::Storage("inbound record missing or assigned to another conversation".into())); }
            Ok(())
        }).await
    }

    /// Invoke only after a successful conversation save. Messages omitted by
    /// compaction remain in the journal; their status stays conservatively
    /// uncheckpointed, not silently marked answered.
    pub async fn retained(&self, conversation: Uuid, ids: &[Uuid]) -> Result<(), Error> {
        let ids = ids.to_vec();
        with_conn(&self.conn,move |conn| {
            let tx=conn.unchecked_transaction().map_err(sql_error)?;
            for id in ids { tx.execute("UPDATE channel_inbound SET status='retained',conversation_id=?2 WHERE message_id=?1 AND status='accepted' AND (conversation_id IS NULL OR conversation_id=?2)",params![id.to_string(),conversation.to_string()]).map_err(sql_error)?; }
            tx.commit().map_err(sql_error)
        }).await
    }

    pub async fn pending_count(&self, address: &ChannelAddress) -> Result<usize, Error> {
        let channel = address.channel();
        let key = address.external_key();
        with_conn(&self.conn,move |conn|conn.query_row("SELECT count(*) FROM channel_inbound WHERE channel=?1 AND external_key=?2 AND status='accepted'",params![channel,key],|r|r.get(0)).map_err(sql_error)).await
    }

    /// Reset cancels only the records admitted before its receive-time fence.
    /// Passing exact IDs avoids cancelling a later post-reset admission.
    pub async fn cancel(&self, ids: &[Uuid]) -> Result<(), Error> {
        let ids = ids.to_vec();
        with_conn(&self.conn,move |conn| {
            let tx=conn.unchecked_transaction().map_err(sql_error)?;
            for id in ids { tx.execute("UPDATE channel_inbound SET status='cancelled' WHERE message_id=?1 AND status='accepted'",[id.to_string()]).map_err(sql_error)?; }
            tx.commit().map_err(sql_error)
        }).await
    }
}

fn sql_error(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::types::MessageContent;
    #[tokio::test]
    async fn journal_survives_reopen_rejects_conflicts_and_cascades_bound_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let address = ChannelAddress::Telegram {
            chat_id: 9,
            thread_id: 3,
        };
        let msg = Message::stamped(
            Role::User,
            MessageContent::Text("Use the corrected dates; do not book".into()),
        );
        let store = crate::Store::open(dir.path(), vec![7; 32]).unwrap();
        assert!(store.inbound().accept(&address, &msg).await.unwrap());
        assert!(!store.inbound().accept(&address, &msg).await.unwrap());
        let mut conflicting = msg.clone();
        conflicting.content = MessageContent::Text("different".into());
        assert!(store
            .inbound()
            .accept(&address, &conflicting)
            .await
            .is_err());
        let conv = store.conversations().create().await.unwrap();
        store.inbound().assign(msg.id, conv.id).await.unwrap();
        drop(store);
        let reopened = crate::Store::open(dir.path(), vec![7; 32]).unwrap();
        assert_eq!(reopened.inbound().pending_count(&address).await.unwrap(), 1);
        let raw = rusqlite::Connection::open(dir.path().join("store.db")).unwrap();
        let data: String = raw
            .query_row(
                "SELECT data FROM channel_inbound WHERE message_id=?1",
                [msg.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Message>(&data).unwrap().id, msg.id);
        reopened
            .inbound()
            .retained(conv.id, &[msg.id])
            .await
            .unwrap();
        assert_eq!(reopened.inbound().pending_count(&address).await.unwrap(), 0);
        reopened.conversations().delete(conv.id).await.unwrap();
        assert_eq!(
            raw.query_row("SELECT count(*) FROM channel_inbound", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
