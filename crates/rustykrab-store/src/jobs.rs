use std::sync::Arc;

use chrono::{DateTime, Utc};
use croner::Cron;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use uuid::Uuid;

use rustykrab_core::Error;

/// A persisted scheduled job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: String,
    pub schedule: String,
    pub task: String,
    pub channel: Option<String>,
    pub chat_id: Option<String>,
    pub one_shot: bool,
    pub enabled: bool,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Handle for scheduled-job CRUD operations, backed by SQLite.
#[derive(Clone)]
pub struct JobStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl JobStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Insert a new scheduled job and return it.
    ///
    /// `schedule` is either a cron expression (e.g. `"0 9 * * *"`) for
    /// recurring jobs, or an ISO 8601 timestamp (e.g. `"2025-03-15T14:30:00Z"`)
    /// for one-shot jobs.
    pub fn create_job(
        &self,
        schedule: &str,
        task: &str,
        channel: Option<&str>,
        chat_id: Option<&str>,
    ) -> Result<ScheduledJob, Error> {
        let now = Utc::now();
        let (one_shot, next_run_at) = parse_schedule(schedule, now)?;

        let id = Uuid::new_v4().to_string();
        let job = ScheduledJob {
            id: id.clone(),
            schedule: schedule.to_string(),
            task: task.to_string(),
            channel: channel.map(|s| s.to_string()),
            chat_id: chat_id.map(|s| s.to_string()),
            one_shot,
            enabled: true,
            next_run_at,
            last_run_at: None,
            created_at: now,
        };

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO scheduled_jobs (id, schedule, task, channel, chat_id, one_shot, enabled, next_run_at, last_run_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                job.id,
                job.schedule,
                job.task,
                job.channel,
                job.chat_id,
                job.one_shot as i32,
                job.enabled as i32,
                job.next_run_at.to_rfc3339(),
                job.last_run_at.map(|t| t.to_rfc3339()),
                job.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(job)
    }

    /// List all scheduled jobs.
    pub fn list_jobs(&self) -> Result<Vec<ScheduledJob>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, schedule, task, channel, chat_id, one_shot, enabled, next_run_at, last_run_at, created_at
                 FROM scheduled_jobs ORDER BY next_run_at",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let jobs = stmt
            .query_map([], |row| {
                Ok(ScheduledJob {
                    id: row.get(0)?,
                    schedule: row.get(1)?,
                    task: row.get(2)?,
                    channel: row.get(3)?,
                    chat_id: row.get(4)?,
                    one_shot: row.get::<_, i32>(5)? != 0,
                    enabled: row.get::<_, i32>(6)? != 0,
                    next_run_at: parse_stored_timestamp(row.get::<_, String>(7)?),
                    last_run_at: row.get::<_, Option<String>>(8)?.map(parse_stored_timestamp),
                    created_at: parse_stored_timestamp(row.get::<_, String>(9)?),
                })
            })
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(jobs)
    }

    /// Delete a scheduled job by ID.
    pub fn delete_job(&self, job_id: &str) -> Result<bool, Error> {
        let conn = self.conn.lock().unwrap();
        let rows = conn
            .execute("DELETE FROM scheduled_jobs WHERE id = ?1", params![job_id])
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Return all enabled jobs whose `next_run_at` is at or before `now`.
    pub fn get_due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<ScheduledJob>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, schedule, task, channel, chat_id, one_shot, enabled, next_run_at, last_run_at, created_at
                 FROM scheduled_jobs
                 WHERE enabled = 1 AND next_run_at <= ?1",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let jobs = stmt
            .query_map(params![now.to_rfc3339()], |row| {
                Ok(ScheduledJob {
                    id: row.get(0)?,
                    schedule: row.get(1)?,
                    task: row.get(2)?,
                    channel: row.get(3)?,
                    chat_id: row.get(4)?,
                    one_shot: row.get::<_, i32>(5)? != 0,
                    enabled: row.get::<_, i32>(6)? != 0,
                    next_run_at: parse_stored_timestamp(row.get::<_, String>(7)?),
                    last_run_at: row.get::<_, Option<String>>(8)?.map(parse_stored_timestamp),
                    created_at: parse_stored_timestamp(row.get::<_, String>(9)?),
                })
            })
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(jobs)
    }

    /// Mark a job as executed: update `last_run_at`, advance `next_run_at`
    /// for recurring jobs, or disable one-shot jobs.
    pub fn mark_executed(&self, job_id: &str) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now();

        // Read the job to determine schedule type.
        let (schedule, one_shot): (String, bool) = conn
            .query_row(
                "SELECT schedule, one_shot FROM scheduled_jobs WHERE id = ?1",
                params![job_id],
                |row| Ok((row.get(0)?, row.get::<_, i32>(1)? != 0)),
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        if one_shot {
            // Disable one-shot jobs after execution.
            conn.execute(
                "UPDATE scheduled_jobs SET enabled = 0, last_run_at = ?1 WHERE id = ?2",
                params![now.to_rfc3339(), job_id],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        } else {
            // Advance next_run_at for recurring jobs.
            let next = compute_next_cron_run(&schedule, now)
                .unwrap_or_else(|_| now + chrono::Duration::hours(1));
            conn.execute(
                "UPDATE scheduled_jobs SET last_run_at = ?1, next_run_at = ?2 WHERE id = ?3",
                params![now.to_rfc3339(), next.to_rfc3339(), job_id],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        }

        Ok(())
    }
}

/// Parse a schedule string, returning `(is_one_shot, next_run_at)`.
fn parse_schedule(schedule: &str, now: DateTime<Utc>) -> Result<(bool, DateTime<Utc>), Error> {
    // Try ISO 8601 timestamp first (one-shot).
    if let Ok(ts) = DateTime::parse_from_rfc3339(schedule) {
        let ts = ts.with_timezone(&Utc);
        if ts <= now {
            return Err(Error::Config(
                "one-shot schedule must be in the future".to_string(),
            ));
        }
        return Ok((true, ts));
    }

    // Try cron expression (recurring).
    let next = compute_next_cron_run(schedule, now)?;
    Ok((false, next))
}

/// Compute the next occurrence of a cron expression after `after`.
fn compute_next_cron_run(expression: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>, Error> {
    let cron: Cron = expression
        .parse()
        .map_err(|e| Error::Config(format!("invalid cron expression: {e}")))?;

    cron.find_next_occurrence(&after, false)
        .map_err(|e| Error::Config(format!("could not compute next cron occurrence: {e}")))
}

/// Parse an RFC 3339 timestamp stored in SQLite.
fn parse_stored_timestamp(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
