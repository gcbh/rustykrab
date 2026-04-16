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

/// A recorded execution of a scheduled job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    /// `"ok"` or `"error"`.
    pub status: String,
    /// Agent response text (or error message).
    pub output: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
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
    /// Record a completed run for a job.
    pub fn record_run(
        &self,
        job_id: &str,
        status: &str,
        output: Option<&str>,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
    ) -> Result<JobRun, Error> {
        let id = Uuid::new_v4().to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO job_runs (id, job_id, status, output, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                job_id,
                status,
                output,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
            ],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(JobRun {
            id,
            job_id: job_id.to_string(),
            status: status.to_string(),
            output: output.map(|s| s.to_string()),
            started_at,
            finished_at,
        })
    }

    /// List recent runs for a job, newest first.
    ///
    /// Returns at most `limit` entries.
    pub fn list_runs(&self, job_id: &str, limit: u32) -> Result<Vec<JobRun>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, job_id, status, output, started_at, finished_at
                 FROM job_runs
                 WHERE job_id = ?1
                 ORDER BY finished_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let runs = stmt
            .query_map(params![job_id, limit], |row| {
                Ok(JobRun {
                    id: row.get(0)?,
                    job_id: row.get(1)?,
                    status: row.get(2)?,
                    output: row.get(3)?,
                    started_at: parse_stored_timestamp(row.get::<_, String>(4)?),
                    finished_at: parse_stored_timestamp(row.get::<_, String>(5)?),
                })
            })
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(runs)
    }
}

/// Parse a schedule string, returning `(is_one_shot, next_run_at)`.
fn parse_schedule(schedule: &str, now: DateTime<Utc>) -> Result<(bool, DateTime<Utc>), Error> {
    // Try ISO 8601 / RFC 3339 timestamp first (one-shot).
    if let Some(ts) = try_parse_datetime(schedule) {
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

/// Try to parse a datetime string in multiple common formats.
///
/// Accepts RFC 3339 (`2025-04-12T14:30:00Z`), with offset
/// (`2025-04-12T14:30:00+02:00`), or naive datetime assumed as UTC
/// (`2025-04-12T14:30:00`, `2025-04-12 14:30:00`).
fn try_parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    // RFC 3339 with timezone
    if let Ok(ts) = DateTime::parse_from_rfc3339(s) {
        return Some(ts.with_timezone(&Utc));
    }
    // Naive datetime (no timezone) — assume UTC
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(naive.and_utc());
    }
    // Date + time without seconds
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Some(naive.and_utc());
    }
    None
}

/// Compute the next occurrence of a cron expression after `after`.
///
/// Accepts standard 5-field cron (`minute hour dom month dow`), 6-field
/// with seconds, or 7-field with seconds and year.
fn compute_next_cron_run(expression: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>, Error> {
    let cron: Cron = expression
        .parse()
        .map_err(|e| {
            Error::Config(format!(
                "invalid cron expression '{expression}': {e}. \
                 Use standard 5-field format: minute(0-59) hour(0-23) day(1-31) month(1-12) weekday(0-6, 0=Sun). \
                 Example: '0 9 * * *' for daily at 9 AM"
            ))
        })?;

    cron.find_next_occurrence(&after, false)
        .map_err(|_| {
            Error::Config(format!(
                "cron expression '{expression}' cannot produce a future occurrence. \
                 The expression may be too restrictive or invalid. \
                 Use standard 5-field format: minute(0-59) hour(0-23) day(1-31) month(1-12) weekday(0-6, 0=Sun). \
                 Example: '0 9 * * 1-5' for weekdays at 9 AM"
            ))
        })
}

/// Parse an RFC 3339 timestamp stored in SQLite.
fn parse_stored_timestamp(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_next_cron_5_field_expression() {
        let now = Utc::now();
        let result = compute_next_cron_run("*/5 * * * *", now);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap() > now);
    }

    #[test]
    fn compute_next_cron_6_field_expression() {
        let now = Utc::now();
        let result = compute_next_cron_run("0 */5 * * * *", now);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap() > now);
    }

    #[test]
    fn compute_next_cron_too_few_fields_gives_actionable_error() {
        let now = Utc::now();
        let err = compute_next_cron_run("* *", now).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid cron expression"), "got: {msg}");
        assert!(msg.contains("Example:"), "got: {msg}");
    }

    #[test]
    fn compute_next_cron_garbage_gives_actionable_error() {
        let now = Utc::now();
        let err = compute_next_cron_run("not a cron", now).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid cron expression"), "got: {msg}");
        assert!(msg.contains("Example:"), "got: {msg}");
    }

    #[test]
    fn compute_next_cron_unreachable_gives_actionable_error() {
        let now = Utc::now();
        // February 31st never exists
        let err = compute_next_cron_run("0 0 31 2 *", now).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot produce a future occurrence"),
            "got: {msg}"
        );
        assert!(msg.contains("Example:"), "got: {msg}");
    }

    #[test]
    fn parse_schedule_one_shot_in_past_fails() {
        let now = Utc::now();
        let err = parse_schedule("2020-01-01T00:00:00Z", now).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("one-shot schedule must be in the future"),
            "got: {msg}"
        );
    }

    #[test]
    fn parse_schedule_one_shot_in_future_succeeds() {
        let now = Utc::now();
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        let (one_shot, next_run) = parse_schedule(&future, now).unwrap();
        assert!(one_shot);
        assert!(next_run > now);
    }

    #[test]
    fn parse_schedule_valid_cron() {
        let now = Utc::now();
        let (one_shot, next_run) = parse_schedule("0 9 * * *", now).unwrap();
        assert!(!one_shot);
        assert!(next_run > now);
    }
}
