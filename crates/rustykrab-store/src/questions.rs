//! Questions, standing judgment and timed notices: the durable half of
//! `docs/plans/control-layer-and-worker-fleet.md`, sections 6.6, 7 and 13.
//!
//! - `questions`: every question a worker asked, as the control layer's
//!   router classified it, with where it went and how it was answered. The
//!   item it belongs to parks while it is open; answering it is what resumes
//!   the item. Rows are never deleted: a settled question is the record
//!   dreaming reads for avoidable escalations (section 10).
//! - `judgment_policies`: standing judgment the user granted in ordinary
//!   language, with the checklist it compiled to. Revoked rows stay, stamped.
//! - `work_outbox.not_before`: when a notice may go out. A notice written
//!   while an earlier one for the same parent is still waiting for its time
//!   replaces it in place, so child transitions inside a window reach the
//!   user as one message (6.6).
//!
//! What it does not decide: the router, the compiler and the notice text
//! belong to `rustykrab-control`. This module writes what it is told and
//! refuses only what would corrupt the record: answering a question that is
//! already settled.

use std::collections::BTreeSet;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rustykrab_core::questions::{
    DelegatedDecision, JudgmentCheck, QuestionClass, QuestionKind, QuestionStatus,
};
use rustykrab_core::work::WorkItemId;
use rustykrab_core::Error;

use crate::work_items::{OutboxDraft, WorkStoreError};
use crate::{with_conn, Store};

// ── schema ─────────────────────────────────────────────────────────────

/// The Phase 4 tables and the outbox's `not_before`, idempotently. Called
/// from `Store::run_migrations` after the work-item tables exist.
pub(crate) fn migrate(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "
        -- A question a worker asked, as the router classified it (plan
        -- sections 7 and 13). `item` and `root` are unenforced like every
        -- work-item reference: the record outlives aging.
        CREATE TABLE IF NOT EXISTS questions (
            id             TEXT PRIMARY KEY,
            item           TEXT NOT NULL,
            root           TEXT NOT NULL,
            kind           TEXT NOT NULL,
            class          TEXT NOT NULL,
            text           TEXT NOT NULL,
            options        TEXT NOT NULL DEFAULT '[]',
            default_answer TEXT,
            asked_class    TEXT,
            rule           TEXT NOT NULL,
            asked_by       TEXT NOT NULL,
            status         TEXT NOT NULL,
            delivered_via  TEXT,
            answer         TEXT,
            answered_by    TEXT,
            answered_at    TEXT,
            research_item  TEXT,
            decision       TEXT,
            created_at     TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_questions_item
            ON questions (item, created_at);
        CREATE INDEX IF NOT EXISTS idx_questions_waiting
            ON questions (status, created_at)
            WHERE status IN ('open', 'recorded', 'researching');

        -- Standing judgment (plan section 7): the user's words, what they
        -- compiled to, and when they were granted and revoked.
        CREATE TABLE IF NOT EXISTS judgment_policies (
            id           TEXT PRIMARY KEY,
            scope        TEXT NOT NULL,
            text         TEXT NOT NULL,
            checks       TEXT NOT NULL DEFAULT '[]',
            policy       TEXT NOT NULL DEFAULT '{}',
            unrecognised TEXT NOT NULL DEFAULT '[]',
            granted_by   TEXT,
            granted_at   TEXT NOT NULL,
            revoked_at   TEXT
        );
        ",
    )
    .map_err(|e| Error::Storage(e.to_string()))?;

    let mut stmt = conn
        .prepare("PRAGMA table_info(work_outbox)")
        .map_err(|e| Error::Storage(e.to_string()))?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| Error::Storage(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    // Notices written before timed delivery existed were due at once, which
    // NULL still means.
    if !existing.iter().any(|c| c == "not_before") {
        conn.execute("ALTER TABLE work_outbox ADD COLUMN not_before TEXT", [])
            .map_err(|e| Error::Storage(e.to_string()))?;
    }
    Ok(())
}

// ── encodings ──────────────────────────────────────────────────────────

/// The work-item tables' timestamp form: RFC 3339, UTC, nanoseconds, fixed
/// width, so text comparison in SQL orders them.
pub(crate) fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn json<T: Serialize>(value: &T) -> Result<String, WorkStoreError> {
    Ok(serde_json::to_string(value)?)
}

// ── questions ──────────────────────────────────────────────────────────

/// One `questions` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRow {
    pub id: String,
    /// The item that asked, or the root a plan's approval holds.
    pub item: WorkItemId,
    /// The top of the item's tree, whose message carries the question.
    pub root: WorkItemId,
    pub kind: QuestionKind,
    /// What the router decided.
    pub class: QuestionClass,
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// The recorded default of a defaultable question.
    #[serde(default)]
    pub default_answer: Option<String>,
    /// What the asking model claimed, kept for audit: the router decides.
    #[serde(default)]
    pub asked_class: Option<String>,
    /// The router rule that classified it.
    pub rule: String,
    /// `worker:<name>`, `controller`, `planner`.
    pub asked_by: String,
    pub status: QuestionStatus,
    /// The channel whose outbox notice carries the question. Delivery is\n    /// confirmed separately by the outbox row, or by the user answering it.
    #[serde(default)]
    pub delivered_via: Option<String>,
    #[serde(default)]
    pub answer: Option<String>,
    /// `user:<principal>`, `default`, `policy:<id>`, `research:<item>`.
    #[serde(default)]
    pub answered_by: Option<String>,
    #[serde(default)]
    pub answered_at: Option<DateTime<Utc>>,
    /// The `research` item filed for a researchable question.
    #[serde(default)]
    pub research_item: Option<WorkItemId>,
    /// The record of a delegated decision.
    #[serde(default)]
    pub decision: Option<DelegatedDecision>,
    pub created_at: DateTime<Utc>,
}

const QUESTION_COLUMNS: &str = "id, item, root, kind, class, text, options, default_answer, \
     asked_class, rule, asked_by, status, delivered_via, answer, answered_by, answered_at, \
     research_item, decision, created_at";

/// Map a `SELECT {QUESTION_COLUMNS}` row. Conservative: an unreadable kind
/// reads as `decision`, an unreadable class as `blocking_now` and an
/// unreadable status as `open`, so a row this build cannot interpret keeps
/// asking the user rather than letting its item run on no answer.
fn question_from_row(row: &Row) -> rusqlite::Result<QuestionRow> {
    let kind: String = row.get("kind")?;
    let class: String = row.get("class")?;
    let status: String = row.get("status")?;
    let options: String = row.get("options")?;
    let decision: Option<String> = row.get("decision")?;
    let answered_at: Option<String> = row.get("answered_at")?;
    let created_at: String = row.get("created_at")?;
    Ok(QuestionRow {
        id: row.get("id")?,
        item: row.get("item")?,
        root: row.get("root")?,
        kind: QuestionKind::parse(&kind).unwrap_or_default(),
        class: QuestionClass::parse(&class).unwrap_or(QuestionClass::BlockingNow),
        text: row.get("text")?,
        options: serde_json::from_str(&options).unwrap_or_default(),
        default_answer: row.get("default_answer")?,
        asked_class: row.get("asked_class")?,
        rule: row.get("rule")?,
        asked_by: row.get("asked_by")?,
        status: QuestionStatus::parse(&status),
        delivered_via: row.get("delivered_via")?,
        answer: row.get("answer")?,
        answered_by: row.get("answered_by")?,
        answered_at: answered_at.and_then(|raw| parse_ts(&raw)),
        research_item: row.get("research_item")?,
        decision: decision.and_then(|raw| serde_json::from_str(&raw).ok()),
        created_at: parse_ts(&created_at).unwrap_or_default(),
    })
}

/// One write to `questions`, applied inside a `work_apply` batch
/// ([`crate::WorkOp::Question`]) so a question lands with the transition
/// that parks or resumes its item, or on its own
/// ([`Store::question_write`]).
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionWrite {
    Insert(Box<QuestionRow>),
    /// Settle a waiting question: answered by the user, defaulted,
    /// delegated, or obsolete. Refused with `AlreadyExists` when it is
    /// already settled: an answer is given once.
    Settle {
        id: String,
        status: QuestionStatus,
        answer: Option<String>,
        by: Option<String>,
        decision: Option<DelegatedDecision>,
        at: DateTime<Utc>,
    },
    /// A research item is now finding the answer.
    Research {
        id: String,
        item: WorkItemId,
    },
    /// The notification channel for the question being asked.
    Delivered {
        id: String,
        via: String,
    },
}

/// A query over `questions`, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionFilter {
    pub item: Option<WorkItemId>,
    pub root: Option<WorkItemId>,
    pub status: Option<QuestionStatus>,
    /// Only questions still waiting (open, recorded, researching).
    pub waiting: bool,
}

pub(crate) fn insert_question(conn: &Connection, q: &QuestionRow) -> Result<(), WorkStoreError> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM questions WHERE id = ?1",
            params![q.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Err(WorkStoreError::AlreadyExists(format!("question {}", q.id)));
    }
    conn.execute(
        &format!(
            "INSERT INTO questions ({QUESTION_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)"
        ),
        params![
            q.id,
            q.item,
            q.root,
            q.kind.as_str(),
            q.class.as_str(),
            q.text,
            json(&q.options)?,
            q.default_answer,
            q.asked_class,
            q.rule,
            q.asked_by,
            q.status.as_str(),
            q.delivered_via,
            q.answer,
            q.answered_by,
            q.answered_at.as_ref().map(ts),
            q.research_item,
            q.decision.as_ref().map(json).transpose()?,
            ts(&q.created_at),
        ],
    )?;
    Ok(())
}

fn get_question(conn: &Connection, id: &str) -> Result<Option<QuestionRow>, WorkStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {QUESTION_COLUMNS} FROM questions WHERE id = ?1"),
            params![id],
            question_from_row,
        )
        .optional()?)
}

/// Apply one [`QuestionWrite`].
pub(crate) fn apply_question(
    conn: &Connection,
    write: &QuestionWrite,
) -> Result<(), WorkStoreError> {
    match write {
        QuestionWrite::Insert(q) => insert_question(conn, q),
        QuestionWrite::Settle {
            id,
            status,
            answer,
            by,
            decision,
            at,
        } => {
            let current = get_question(conn, id)?
                .ok_or_else(|| WorkStoreError::NotFound(format!("question {id}")))?;
            if !current.status.is_waiting() {
                return Err(WorkStoreError::AlreadyExists(format!(
                    "question {id} is already {}",
                    current.status
                )));
            }
            conn.execute(
                "UPDATE questions SET status = ?2, answer = ?3, answered_by = ?4, \
                 decision = ?5, answered_at = ?6 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    answer,
                    by,
                    decision.as_ref().map(json).transpose()?,
                    ts(at),
                ],
            )?;
            Ok(())
        }
        QuestionWrite::Research { id, item } => {
            let changed = conn.execute(
                "UPDATE questions SET status = 'researching', research_item = ?2 WHERE id = ?1",
                params![id, item],
            )?;
            if changed == 0 {
                return Err(WorkStoreError::NotFound(format!("question {id}")));
            }
            Ok(())
        }
        QuestionWrite::Delivered { id, via } => {
            conn.execute(
                "UPDATE questions SET delivered_via = ?2 WHERE id = ?1",
                params![id, via],
            )?;
            Ok(())
        }
    }
}

fn list_questions(
    conn: &Connection,
    filter: &QuestionFilter,
) -> Result<Vec<QuestionRow>, WorkStoreError> {
    let mut clauses: Vec<&str> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    if let Some(item) = &filter.item {
        clauses.push("item = ?");
        values.push(item.clone());
    }
    if let Some(root) = &filter.root {
        clauses.push("root = ?");
        values.push(root.clone());
    }
    if let Some(status) = &filter.status {
        clauses.push("status = ?");
        values.push(status.as_str().to_string());
    }
    if filter.waiting {
        clauses.push("status IN ('open', 'recorded', 'researching')");
    }
    let mut sql = format!("SELECT {QUESTION_COLUMNS} FROM questions");
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, rowid DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), question_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ── standing judgment ──────────────────────────────────────────────────

/// One `judgment_policies` row: a grant in the user's words and what it
/// compiled to. The projects crate's `JudgmentPolicy` carries the words as
/// the planning plan models them (statement, delegated scopes, reserved
/// decisions); `checks` is what the controller evaluates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgmentRow {
    pub id: String,
    /// What the grant covers: `plans`, `questions`, `resources`, or a
    /// caller's own scope such as a kind of work.
    pub scope: String,
    /// The user's words, verbatim.
    pub text: String,
    pub checks: Vec<JudgmentCheck>,
    pub policy: rustykrab_projects::JudgmentPolicy,
    /// Sentences that compiled to no check, reported back to the user
    /// rather than guessed at.
    #[serde(default)]
    pub unrecognised: Vec<String>,
    #[serde(default)]
    pub granted_by: Option<String>,
    pub granted_at: DateTime<Utc>,
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
}

const JUDGMENT_COLUMNS: &str =
    "id, scope, text, checks, policy, unrecognised, granted_by, granted_at, revoked_at";

/// Map a `judgment_policies` row. A checklist that no longer parses
/// compiles to nothing: an unreadable grant delegates no authority.
fn judgment_from_row(row: &Row) -> rusqlite::Result<JudgmentRow> {
    let checks: String = row.get("checks")?;
    let policy: String = row.get("policy")?;
    let unrecognised: String = row.get("unrecognised")?;
    let granted_at: String = row.get("granted_at")?;
    let revoked_at: Option<String> = row.get("revoked_at")?;
    Ok(JudgmentRow {
        id: row.get("id")?,
        scope: row.get("scope")?,
        text: row.get("text")?,
        checks: serde_json::from_str(&checks).unwrap_or_default(),
        policy: serde_json::from_str(&policy).unwrap_or_default(),
        unrecognised: serde_json::from_str(&unrecognised).unwrap_or_default(),
        granted_by: row.get("granted_by")?,
        granted_at: parse_ts(&granted_at).unwrap_or_default(),
        // A revocation stamp that does not parse still means revoked.
        revoked_at: revoked_at.map(|raw| parse_ts(&raw).unwrap_or_default()),
    })
}

// ── timed notices ──────────────────────────────────────────────────────

/// A notice with a send time (6.6). `replace` names a pending row the
/// caller read and merged into `draft.body`: it is updated in place while
/// it is still waiting for its time, else a new row is written, so nothing
/// the earlier notice said is lost to a race with delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct NoticeDraft {
    pub draft: OutboxDraft,
    /// `None`: due at once.
    pub not_before: Option<DateTime<Utc>>,
    pub replace: Option<String>,
}

/// One `work_outbox` row still waiting for its send time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingNotice {
    pub id: String,
    pub parent: WorkItemId,
    pub channel: String,
    pub body: String,
    pub not_before: DateTime<Utc>,
}

/// Write one [`NoticeDraft`]. Returns the row id it landed in.
pub(crate) fn apply_notice(
    conn: &Connection,
    notice: &NoticeDraft,
    now: DateTime<Utc>,
) -> Result<String, WorkStoreError> {
    let not_before = notice.not_before.as_ref().map(ts);
    if let Some(id) = &notice.replace {
        let updated = conn.execute(
            "UPDATE work_outbox SET body = ?2, origin = COALESCE(?3, origin), not_before = ?4 \
             WHERE id = ?1 AND delivered_at IS NULL AND retired_at IS NULL AND not_before IS NOT NULL \
               AND not_before > ?5",
            params![
                id,
                notice.draft.body,
                notice.draft.origin,
                not_before.clone().unwrap_or_else(|| ts(&now)),
                ts(&now),
            ],
        )?;
        if updated == 1 {
            return Ok(id.clone());
        }
    }
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO work_outbox (id, parent, origin, channel, body, created_at, delivered_at, \
         not_before) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
        params![
            id,
            notice.draft.parent,
            notice.draft.origin,
            notice.draft.channel,
            notice.draft.body,
            ts(&now),
            not_before,
        ],
    )?;
    Ok(id)
}

fn waiting_notice(
    conn: &Connection,
    parent: &str,
    channel: &str,
    now: DateTime<Utc>,
) -> Result<Option<WaitingNotice>, WorkStoreError> {
    Ok(conn
        .query_row(
            "SELECT id, parent, channel, body, not_before FROM work_outbox \
             WHERE parent = ?1 AND channel = ?2 AND delivered_at IS NULL AND retired_at IS NULL \
               AND not_before IS NOT NULL AND not_before > ?3 \
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
            params![parent, channel, ts(&now)],
            |row| {
                let not_before: String = row.get(4)?;
                Ok(WaitingNotice {
                    id: row.get(0)?,
                    parent: row.get(1)?,
                    channel: row.get(2)?,
                    body: row.get(3)?,
                    not_before: parse_ts(&not_before).unwrap_or_default(),
                })
            },
        )
        .optional()?)
}

// ── the API ────────────────────────────────────────────────────────────

impl Store {
    async fn question_call<T, F>(&self, f: F) -> Result<T, WorkStoreError>
    where
        F: FnOnce(&Connection) -> Result<T, WorkStoreError> + Send + 'static,
        T: Send + 'static,
    {
        with_conn(&self.conn, move |conn| Ok(f(conn))).await?
    }

    pub async fn question_get(&self, id: &str) -> Result<Option<QuestionRow>, WorkStoreError> {
        let id = id.to_string();
        self.question_call(move |conn| get_question(conn, &id))
            .await
    }

    /// Questions matching `filter`, newest first.
    pub async fn questions_list(
        &self,
        filter: &QuestionFilter,
    ) -> Result<Vec<QuestionRow>, WorkStoreError> {
        let filter = filter.clone();
        self.question_call(move |conn| list_questions(conn, &filter))
            .await
    }

    /// Of `ids`, the questions settled with an answer: what fires an
    /// `on_answer` trigger (section 4).
    pub async fn questions_answered_among(
        &self,
        ids: Vec<String>,
    ) -> Result<BTreeSet<String>, WorkStoreError> {
        self.question_call(move |conn| {
            let mut out = BTreeSet::new();
            for id in ids {
                if get_question(conn, &id)?.is_some_and(|q| q.status.has_answer()) {
                    out.insert(id);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Apply one write in its own transaction.
    pub async fn question_write(&self, write: QuestionWrite) -> Result<(), WorkStoreError> {
        self.question_call(move |conn| {
            let tx = conn.unchecked_transaction()?;
            apply_question(&tx, &write)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Record a grant.
    pub async fn judgment_grant(&self, row: JudgmentRow) -> Result<(), WorkStoreError> {
        self.question_call(move |conn| {
            conn.execute(
                &format!(
                    "INSERT INTO judgment_policies ({JUDGMENT_COLUMNS}) VALUES \
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    row.id,
                    row.scope,
                    row.text,
                    json(&row.checks)?,
                    json(&row.policy)?,
                    json(&row.unrecognised)?,
                    row.granted_by,
                    ts(&row.granted_at),
                    row.revoked_at.as_ref().map(ts),
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Grants, oldest first; revoked ones only when asked.
    pub async fn judgment_list(
        &self,
        include_revoked: bool,
    ) -> Result<Vec<JudgmentRow>, WorkStoreError> {
        self.question_call(move |conn| {
            let sql = if include_revoked {
                format!(
                    "SELECT {JUDGMENT_COLUMNS} FROM judgment_policies ORDER BY granted_at, rowid"
                )
            } else {
                format!(
                    "SELECT {JUDGMENT_COLUMNS} FROM judgment_policies WHERE revoked_at IS NULL \
                     ORDER BY granted_at, rowid"
                )
            };
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], judgment_from_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Revoke a grant. `false` if it was already revoked; `NotFound` if no
    /// grant has the id.
    pub async fn judgment_revoke(
        &self,
        id: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, WorkStoreError> {
        let id = id.to_string();
        self.question_call(move |conn| {
            let changed = conn.execute(
                "UPDATE judgment_policies SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                params![id, ts(&at)],
            )?;
            if changed == 1 {
                return Ok(true);
            }
            let exists = conn
                .query_row(
                    "SELECT 1 FROM judgment_policies WHERE id = ?1",
                    params![id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                Ok(false)
            } else {
                Err(WorkStoreError::NotFound(format!("judgment policy {id}")))
            }
        })
        .await
    }

    /// The newest notice for `parent` on `channel` still waiting for its
    /// send time, which a new notice for the parent merges into (6.6).
    pub async fn work_outbox_waiting(
        &self,
        parent: &str,
        channel: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<WaitingNotice>, WorkStoreError> {
        let (parent, channel) = (parent.to_string(), channel.to_string());
        self.question_call(move |conn| waiting_notice(conn, &parent, &channel, now))
            .await
    }

    /// When the newest notice for `parent` was written, delivered or not:
    /// what a digest's window counts from.
    pub async fn work_outbox_latest(
        &self,
        parent: &str,
    ) -> Result<Option<DateTime<Utc>>, WorkStoreError> {
        let parent = parent.to_string();
        self.question_call(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM work_outbox WHERE parent = ?1",
                    params![parent],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            Ok(raw.and_then(|r| parse_ts(&r)))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkOp;

    fn store() -> Store {
        Store::open_in_memory()
    }

    fn question(id: &str, item: &str) -> QuestionRow {
        QuestionRow {
            id: id.into(),
            item: item.into(),
            root: item.into(),
            kind: QuestionKind::Decision,
            class: QuestionClass::BlockingNow,
            text: "Which florist?".into(),
            options: vec!["Petals".into(), "Stems".into()],
            default_answer: None,
            asked_class: Some("blocking_now".into()),
            rule: "blocking_now".into(),
            asked_by: "worker:pinch".into(),
            status: QuestionStatus::Open,
            delivered_via: None,
            answer: None,
            answered_by: None,
            answered_at: None,
            research_item: None,
            decision: None,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_question_round_trips_and_is_answered_once() {
        let s = store();
        s.question_write(QuestionWrite::Insert(Box::new(question("q1", "i1"))))
            .await
            .unwrap();
        let got = s.question_get("q1").await.unwrap().unwrap();
        assert_eq!(got.options, vec!["Petals", "Stems"]);
        assert_eq!(got.status, QuestionStatus::Open);
        let settle = QuestionWrite::Settle {
            id: "q1".into(),
            status: QuestionStatus::Answered,
            answer: Some("Petals".into()),
            by: Some("user:master".into()),
            decision: None,
            at: Utc::now(),
        };
        s.question_write(settle.clone()).await.unwrap();
        let err = s.question_write(settle).await.unwrap_err();
        assert!(matches!(err, WorkStoreError::AlreadyExists(_)), "{err}");
        let answered = s
            .questions_answered_among(vec!["q1".into(), "nope".into()])
            .await
            .unwrap();
        assert_eq!(answered.into_iter().collect::<Vec<_>>(), vec!["q1"]);
    }

    #[tokio::test]
    async fn filters_select_by_item_and_waiting() {
        let s = store();
        for (id, item) in [("a", "i1"), ("b", "i1"), ("c", "i2")] {
            s.question_write(QuestionWrite::Insert(Box::new(question(id, item))))
                .await
                .unwrap();
        }
        s.question_write(QuestionWrite::Settle {
            id: "b".into(),
            status: QuestionStatus::Obsolete,
            answer: None,
            by: None,
            decision: None,
            at: Utc::now(),
        })
        .await
        .unwrap();
        let of_i1 = s
            .questions_list(&QuestionFilter {
                item: Some("i1".into()),
                ..QuestionFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(of_i1.len(), 2);
        let waiting = s
            .questions_list(&QuestionFilter {
                waiting: true,
                ..QuestionFilter::default()
            })
            .await
            .unwrap();
        let ids: Vec<&str> = waiting.iter().map(|q| q.id.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"c") && !ids.contains(&"b"));
    }

    #[tokio::test]
    async fn a_question_lands_with_its_work_batch() {
        let s = store();
        s.work_apply(vec![WorkOp::Question(QuestionWrite::Insert(Box::new(
            question("q9", "i9"),
        )))])
        .await
        .unwrap();
        assert!(s.question_get("q9").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn grants_are_listed_and_revoked_once() {
        let s = store();
        let row = JudgmentRow {
            id: "j1".into(),
            scope: "plans".into(),
            text: "Ask me before paying for anything.".into(),
            checks: vec![JudgmentCheck::ConsentFor {
                resource: "payment".into(),
            }],
            policy: rustykrab_projects::JudgmentPolicy {
                statement: "Ask me before paying for anything.".into(),
                ..Default::default()
            },
            unrecognised: vec![],
            granted_by: Some("user:master".into()),
            granted_at: Utc::now(),
            revoked_at: None,
        };
        s.judgment_grant(row.clone()).await.unwrap();
        assert_eq!(s.judgment_list(false).await.unwrap(), vec![row]);
        assert!(s.judgment_revoke("j1", Utc::now()).await.unwrap());
        assert!(!s.judgment_revoke("j1", Utc::now()).await.unwrap());
        assert!(s.judgment_list(false).await.unwrap().is_empty());
        assert_eq!(s.judgment_list(true).await.unwrap().len(), 1);
        assert!(s.judgment_revoke("nope", Utc::now()).await.is_err());
    }

    #[tokio::test]
    async fn a_timed_notice_waits_and_a_later_one_merges_into_it() {
        let s = store();
        let now = Utc::now();
        let later = now + chrono::TimeDelta::seconds(30);
        let draft = |body: &str| OutboxDraft {
            parent: "p".into(),
            origin: None,
            channel: "telegram".into(),
            body: body.into(),
        };
        let first = s
            .work_apply(vec![WorkOp::Notice(NoticeDraft {
                draft: draft("first"),
                not_before: Some(later),
                replace: None,
            })])
            .await
            .unwrap();
        assert!(
            s.work_outbox_pending().await.unwrap().is_empty(),
            "a notice waits for its time"
        );
        let waiting = s
            .work_outbox_waiting("p", "telegram", now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.id, first.outbox_ids[0]);
        // An urgent notice for the same parent takes the waiting row and
        // makes it due at once.
        s.work_apply(vec![WorkOp::Notice(NoticeDraft {
            draft: draft("first\nsecond"),
            not_before: None,
            replace: Some(waiting.id.clone()),
        })])
        .await
        .unwrap();
        let pending = s.work_outbox_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "first\nsecond");
        // A row that is already due is never rewritten: a new one is.
        s.work_apply(vec![WorkOp::Notice(NoticeDraft {
            draft: draft("third"),
            not_before: None,
            replace: Some(waiting.id),
        })])
        .await
        .unwrap();
        assert_eq!(s.work_outbox_pending().await.unwrap().len(), 2);
        assert!(s.work_outbox_latest("p").await.unwrap().is_some());
    }
}
