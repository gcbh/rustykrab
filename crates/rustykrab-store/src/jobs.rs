use std::sync::Arc;

use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use croner::Cron;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use uuid::Uuid;

use rustykrab_core::timezone::{self, Tz};
use rustykrab_core::Error;

use crate::with_conn;

/// Maximum retained `job_runs` rows per job. Older runs are pruned on
/// insert so the history table can't grow without bound.
const MAX_RUNS_PER_JOB: u32 = 100;

/// A persisted scheduled job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: String,
    pub schedule: String,
    pub task: String,
    pub channel: Option<String>,
    pub chat_id: Option<String>,
    /// Channel-specific thread identifier. Telegram: forum topic thread_id
    /// (numeric string, e.g. "42"). Slack: thread_ts (e.g.
    /// "1700000000.000100"). `None` means post at the channel's top level.
    pub thread_id: Option<String>,
    pub one_shot: bool,
    pub enabled: bool,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// The conversation this job resumes on each run. `None` until the first
    /// run creates and persists one; subsequent runs append to the same
    /// conversation so the agent sees prior context.
    pub conversation_id: Option<String>,
    /// RustyKrab version that created this job.
    ///
    /// The `task` string is written once at creation and never updated, so
    /// its quality is fixed by whatever build produced it. Recording that
    /// build makes "which jobs were created before fix X?" a query rather
    /// than a guess. `None` for jobs created before this column existed.
    pub created_version: Option<String>,
    /// IANA zone the `schedule` field is written in, e.g.
    /// `America/Los_Angeles`.
    ///
    /// `next_run_at` and every other timestamp on this row stay UTC. This
    /// is only the lens: `"30 7 * * *"` means half past seven *here*, and
    /// the zone database supplies the offset for each individual fire, so
    /// the job holds its wall-clock time across the DST boundary instead of
    /// sliding an hour every spring.
    ///
    /// Jobs written before this column existed read back as `UTC`, which is
    /// exactly the lens they were created under — the migration does not
    /// reinterpret them, because silently moving a live job's fire time is
    /// worse than leaving it where the operator last saw it.
    pub timezone: String,
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
    /// RustyKrab version that executed this run, so a given output can be
    /// attributed to the build that produced it. `None` for runs recorded
    /// before this column existed.
    pub rustykrab_version: Option<String>,
}

/// Handle for scheduled-job CRUD operations, backed by SQLite.
///
/// All methods run their rusqlite work on tokio's blocking pool via
/// `spawn_blocking` so async workers never park on disk I/O.
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
    ///
    /// `timezone` is the IANA zone the schedule is written in — the caller's
    /// wall clock, not the server's. It is stored alongside the schedule so
    /// every later advance of `next_run_at` re-derives the offset from the
    /// zone database rather than freezing whatever offset was in force at
    /// creation. A timestamp carrying an explicit offset ignores it.
    ///
    /// Recurring jobs are deduplicated on `(task, channel, chat_id,
    /// thread_id)`: creating a second enabled job that delivers the same
    /// work to the same place returns [`Error::AlreadyExists`] naming the
    /// job already doing it. An agent that has lost the memory of scheduling
    /// something — through compaction, a new session, or a summary that
    /// misdescribed it — otherwise silently doubles the user's delivery rate,
    /// and nothing downstream can tell the two apart afterwards.
    ///
    /// `allow_duplicate` is the escape hatch for the case that is genuinely
    /// two jobs: the same briefing at 8am and at 5:30pm cannot be one cron
    /// expression when the minute fields differ.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_job(
        &self,
        schedule: &str,
        task: &str,
        channel: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
        timezone: &str,
        allow_duplicate: bool,
    ) -> Result<ScheduledJob, Error> {
        let now = Utc::now();
        let tz = timezone::parse(timezone)?;
        let (one_shot, next_run_at) = parse_schedule(schedule, now, tz)?;

        let id = Uuid::new_v4().to_string();
        let job = ScheduledJob {
            id: id.clone(),
            schedule: schedule.to_string(),
            task: task.to_string(),
            channel: channel.map(|s| s.to_string()),
            chat_id: chat_id.map(|s| s.to_string()),
            thread_id: thread_id.map(|s| s.to_string()),
            one_shot,
            enabled: true,
            next_run_at,
            last_run_at: None,
            created_at: now,
            conversation_id: None,
            created_version: Some(rustykrab_core::VERSION.to_string()),
            timezone: tz.name().to_string(),
        };

        let row = job.clone();
        with_conn(&self.conn, move |conn| {
            // Checked under the same lock as the insert, so two concurrent
            // creates cannot both pass the check and both write.
            if !allow_duplicate && !row.one_shot {
                if let Some((existing_id, existing_schedule)) = find_duplicate(conn, &row)? {
                    return Err(Error::AlreadyExists(format!(
                        "a job already delivers this task here: id {existing_id}, \
                         schedule '{existing_schedule}'. Delete it first if you meant \
                         to replace it, or pass allow_duplicate to run both."
                    )));
                }
            }

            conn.execute(
                "INSERT INTO scheduled_jobs (id, schedule, task, channel, chat_id, thread_id, one_shot, enabled, next_run_at, last_run_at, created_at, conversation_id, created_version, timezone)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    row.id,
                    row.schedule,
                    row.task,
                    row.channel,
                    row.chat_id,
                    row.thread_id,
                    row.one_shot as i32,
                    row.enabled as i32,
                    row.next_run_at.to_rfc3339(),
                    row.last_run_at.map(|t| t.to_rfc3339()),
                    row.created_at.to_rfc3339(),
                    row.conversation_id,
                    row.created_version,
                    row.timezone,
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;

        Ok(job)
    }

    /// List all scheduled jobs.
    pub async fn list_jobs(&self) -> Result<Vec<ScheduledJob>, Error> {
        with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM scheduled_jobs ORDER BY next_run_at",
                ))
                .map_err(|e| Error::Storage(e.to_string()))?;

            let jobs = stmt
                .query_map([], row_to_job)
                .map_err(|e| Error::Storage(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Error::Storage(e.to_string()))?;

            Ok(jobs)
        })
        .await
    }

    /// Fetch a single scheduled job by ID. Returns `NotFound` if absent.
    pub async fn get_job(&self, job_id: &str) -> Result<ScheduledJob, Error> {
        let job_id = job_id.to_string();
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!("SELECT {JOB_COLUMNS} FROM scheduled_jobs WHERE id = ?1"),
                params![job_id],
                row_to_job,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("job {job_id}")),
                other => Error::Storage(other.to_string()),
            })
        })
        .await
    }

    /// Delete a scheduled job by ID.
    ///
    /// Returns [`Error::NotFound`] if no row matched, mirroring [`get_job`].
    /// The earlier `Ok(false)` made "I deleted it" and "there was nothing to
    /// delete" the same successful call, which every caller then had to
    /// remember to distinguish — and one that forgot mistook a mistyped id
    /// for a completed replacement and created a second job beside the one
    /// it meant to remove.
    ///
    /// [`get_job`]: JobStore::get_job
    pub async fn delete_job(&self, job_id: &str) -> Result<(), Error> {
        let job_id = job_id.to_string();
        with_conn(&self.conn, move |conn| {
            let rows = conn
                .execute(
                    "DELETE FROM scheduled_jobs WHERE id = ?1",
                    params![job_id.clone()],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if rows == 0 {
                return Err(Error::NotFound(format!(
                    "job {job_id} — nothing was deleted. Call cron list to get \
                     the current job ids."
                )));
            }
            Ok(())
        })
        .await
    }

    /// Toggle a job's `enabled` flag. The cron poller skips disabled jobs.
    /// Used by the executor to retire jobs that turn out to be unrunnable
    /// (e.g. an empty task body persisted by an older build) so they stop
    /// firing every cycle.
    pub async fn set_enabled(&self, job_id: &str, enabled: bool) -> Result<(), Error> {
        let job_id = job_id.to_string();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE scheduled_jobs SET enabled = ?1 WHERE id = ?2",
                params![enabled as i32, job_id],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Attach a conversation id to a job. Called on the first run once the
    /// executor has created (or resumed) the conversation the agent uses.
    pub async fn set_conversation_id(
        &self,
        job_id: &str,
        conversation_id: &str,
    ) -> Result<(), Error> {
        let job_id = job_id.to_string();
        let conversation_id = conversation_id.to_string();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE scheduled_jobs SET conversation_id = ?1 WHERE id = ?2",
                params![conversation_id, job_id],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Return all enabled jobs whose `next_run_at` is at or before `now`.
    pub async fn get_due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<ScheduledJob>, Error> {
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM scheduled_jobs WHERE enabled = 1 AND next_run_at <= ?1",
                ))
                .map_err(|e| Error::Storage(e.to_string()))?;

            let jobs = stmt
                .query_map(params![now.to_rfc3339()], row_to_job)
                .map_err(|e| Error::Storage(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Error::Storage(e.to_string()))?;

            Ok(jobs)
        })
        .await
    }

    /// Mark a job as executed: update `last_run_at`, advance `next_run_at`
    /// for recurring jobs, or disable one-shot jobs.
    pub async fn mark_executed(&self, job_id: &str) -> Result<(), Error> {
        let job_id = job_id.to_string();
        with_conn(&self.conn, move |conn| {
            let now = Utc::now();

            // Read the job to determine schedule type. The stored zone —
            // not the process default — advances the schedule, so a job keeps
            // firing at the wall-clock time it was created with even if
            // RUSTYKRAB_TIMEZONE later changes.
            let (schedule, one_shot, timezone): (String, bool, Option<String>) = conn
                .query_row(
                    "SELECT schedule, one_shot, timezone FROM scheduled_jobs WHERE id = ?1",
                    params![job_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get::<_, i32>(1)? != 0,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let tz = zone_or_utc(timezone.as_deref());

            if one_shot {
                // Disable one-shot jobs after execution.
                conn.execute(
                    "UPDATE scheduled_jobs SET enabled = 0, last_run_at = ?1 WHERE id = ?2",
                    params![now.to_rfc3339(), job_id],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            } else {
                // Advance next_run_at for recurring jobs.
                let next = compute_next_cron_run(&schedule, now, tz)
                    .unwrap_or_else(|_| now + chrono::Duration::hours(1));
                conn.execute(
                    "UPDATE scheduled_jobs SET last_run_at = ?1, next_run_at = ?2 WHERE id = ?3",
                    params![now.to_rfc3339(), next.to_rfc3339(), job_id],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            }

            Ok(())
        })
        .await
    }
    /// Record a completed run for a job, pruning history beyond the most
    /// recent [`MAX_RUNS_PER_JOB`] entries for that job.
    pub async fn record_run(
        &self,
        job_id: &str,
        status: &str,
        output: Option<&str>,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
    ) -> Result<JobRun, Error> {
        let run = JobRun {
            id: Uuid::new_v4().to_string(),
            job_id: job_id.to_string(),
            status: status.to_string(),
            output: output.map(|s| s.to_string()),
            started_at,
            finished_at,
            rustykrab_version: Some(rustykrab_core::VERSION.to_string()),
        };
        let row = run.clone();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO job_runs (id, job_id, status, output, started_at, finished_at, rustykrab_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.id,
                    row.job_id,
                    row.status,
                    row.output,
                    row.started_at.to_rfc3339(),
                    row.finished_at.to_rfc3339(),
                    row.rustykrab_version,
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

            // Retention cap: drop rows older than the newest N for this job.
            conn.execute(
                "DELETE FROM job_runs
                 WHERE job_id = ?1
                   AND id NOT IN (
                       SELECT id FROM job_runs
                       WHERE job_id = ?1
                       ORDER BY finished_at DESC
                       LIMIT ?2
                   )",
                params![row.job_id, MAX_RUNS_PER_JOB],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

            Ok(())
        })
        .await?;

        Ok(run)
    }

    /// List recent runs for a job, newest first.
    ///
    /// Returns at most `limit` entries.
    pub async fn list_runs(&self, job_id: &str, limit: u32) -> Result<Vec<JobRun>, Error> {
        let job_id = job_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, job_id, status, output, started_at, finished_at, rustykrab_version
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
                        rustykrab_version: row.get::<_, Option<String>>(6)?,
                    })
                })
                .map_err(|e| Error::Storage(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Error::Storage(e.to_string()))?;

            Ok(runs)
        })
        .await
    }
}

/// Column list for `SELECT`s against `scheduled_jobs`. Kept in sync with
/// [`row_to_job`].
const JOB_COLUMNS: &str = "id, schedule, task, channel, chat_id, thread_id, one_shot, enabled, \
     next_run_at, last_run_at, created_at, conversation_id, created_version, timezone";

/// Decode a row produced by a `SELECT {JOB_COLUMNS}` into a [`ScheduledJob`].
fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledJob> {
    Ok(ScheduledJob {
        id: row.get(0)?,
        schedule: row.get(1)?,
        task: row.get(2)?,
        channel: row.get(3)?,
        chat_id: row.get(4)?,
        thread_id: row.get(5)?,
        one_shot: row.get::<_, i32>(6)? != 0,
        enabled: row.get::<_, i32>(7)? != 0,
        next_run_at: parse_stored_timestamp(row.get::<_, String>(8)?),
        last_run_at: row.get::<_, Option<String>>(9)?.map(parse_stored_timestamp),
        created_at: parse_stored_timestamp(row.get::<_, String>(10)?),
        conversation_id: row.get::<_, Option<String>>(11)?,
        created_version: row.get::<_, Option<String>>(12)?,
        timezone: zone_or_utc(row.get::<_, Option<String>>(13)?.as_deref())
            .name()
            .to_string(),
    })
}

/// Find an enabled recurring job already delivering `candidate`'s task to
/// the same destination, if one exists.
///
/// Identity is `(task, channel, chat_id, thread_id)` and deliberately
/// excludes `schedule`: two jobs running the same task on *different*
/// schedules is precisely the duplicate worth catching, since that is what a
/// failed replace leaves behind. `IS` rather than `=` so a NULL channel
/// matches a NULL channel — SQL equality would let an unaddressed job
/// duplicate freely.
///
/// One-shot jobs are exempt (the caller checks `one_shot` before calling):
/// two identical reminders at different times are ordinary, not a mistake.
fn find_duplicate(
    conn: &rusqlite::Connection,
    candidate: &ScheduledJob,
) -> Result<Option<(String, String)>, Error> {
    conn.query_row(
        "SELECT id, schedule FROM scheduled_jobs
          WHERE enabled = 1 AND one_shot = 0
            AND task = ?1 AND channel IS ?2 AND chat_id IS ?3 AND thread_id IS ?4
          LIMIT 1",
        params![
            candidate.task,
            candidate.channel,
            candidate.chat_id,
            candidate.thread_id
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(Error::Storage(other.to_string())),
    })
}

/// Resolve a stored zone name, falling back to UTC.
///
/// A row written before the `timezone` column existed, or one holding a name
/// this build's zone database does not know, still has to schedule. UTC is
/// the only defensible fallback: it is what those rows were already being
/// interpreted as, so nothing silently moves.
fn zone_or_utc(name: Option<&str>) -> Tz {
    name.and_then(|n| timezone::parse(n).ok())
        .unwrap_or(Tz::UTC)
}

/// Convert a wall-clock time in `tz` to the UTC instant it names.
///
/// Both DST edge cases have to resolve to *something*, because a job that
/// refuses to schedule is worse than one that fires a few minutes off:
///
/// - **Ambiguous** (clocks go back; 01:30 happens twice): take the earlier
///   instant, so the job fires once, on the first pass of that wall clock.
/// - **Nonexistent** (clocks go forward; 02:30 never occurs): take the
///   instant the gap ends, which is the first moment that day at or after
///   the requested wall-clock time.
fn local_to_utc(naive: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt.with_timezone(&Utc),
        LocalResult::Ambiguous(earlier, _) => earlier.with_timezone(&Utc),
        LocalResult::None => {
            // Walk forward a minute at a time out of the gap. Real gaps are
            // 30 or 60 minutes; 180 iterations covers every transition in
            // the IANA database with room to spare.
            let mut probe = naive;
            for _ in 0..180 {
                probe += chrono::Duration::minutes(1);
                if let Some(dt) = tz.from_local_datetime(&probe).earliest() {
                    return dt.with_timezone(&Utc);
                }
            }
            // Unreachable for any real zone, but a scheduler must not panic.
            probe.and_utc()
        }
    }
}

/// Parse a schedule string, returning `(is_one_shot, next_run_at)`.
///
/// `tz` is the lens: cron fields and offset-less timestamps are read as
/// wall-clock times in that zone. The returned instant is always UTC.
fn parse_schedule(
    schedule: &str,
    now: DateTime<Utc>,
    tz: Tz,
) -> Result<(bool, DateTime<Utc>), Error> {
    // Try ISO 8601 / RFC 3339 timestamp first (one-shot).
    if let Some(ts) = try_parse_datetime(schedule, tz) {
        if ts <= now {
            return Err(Error::Config(
                "one-shot schedule must be in the future".to_string(),
            ));
        }
        return Ok((true, ts));
    }

    // Try cron expression (recurring).
    let next = compute_next_cron_run(schedule, now, tz)?;
    Ok((false, next))
}

/// Try to parse a datetime string in multiple common formats.
///
/// Accepts RFC 3339 (`2025-04-12T14:30:00Z`) or an explicit offset
/// (`2025-04-12T14:30:00+02:00`), both of which already name an instant and
/// so ignore `tz`. A naive datetime (`2025-04-12T14:30:00`) is read as a
/// wall-clock time in `tz` — a user who omits the offset means their own
/// clock, not Greenwich.
fn try_parse_datetime(s: &str, tz: Tz) -> Option<DateTime<Utc>> {
    // RFC 3339 with timezone
    if let Ok(ts) = DateTime::parse_from_rfc3339(s) {
        return Some(ts.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, format) {
            return Some(local_to_utc(naive, tz));
        }
    }
    None
}

/// Compute the next occurrence of a cron expression after `after`.
///
/// Accepts standard 5-field cron (`minute hour dom month dow`), 6-field
/// with seconds, or 7-field with seconds and year.
///
/// The match runs against `after` rendered in `tz`, so the hour field means
/// the hour on the operator's clock. Converting the result back to UTC is
/// what keeps `"0 9 * * *"` at 09:00 local on both sides of a DST change
/// instead of drifting to 08:00 or 10:00.
fn compute_next_cron_run(
    expression: &str,
    after: DateTime<Utc>,
    tz: Tz,
) -> Result<DateTime<Utc>, Error> {
    let cron: Cron = expression
        .parse()
        .map_err(|e| {
            Error::Config(format!(
                "invalid cron expression '{expression}': {e}. \
                 Use standard 5-field format: minute(0-59) hour(0-23) day(1-31) month(1-12) weekday(0-6, 0=Sun). \
                 Example: '0 9 * * *' for daily at 9 AM"
            ))
        })?;

    cron.find_next_occurrence(&after.with_timezone(&tz), false)
        .map(|dt| dt.with_timezone(&Utc))
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

    #[tokio::test]
    async fn create_job_stamps_the_running_version() {
        // The `task` string is fixed at creation and never updated, so
        // knowing which build wrote it is what makes "find every job created
        // before fix X" a query instead of a guess.
        let s = in_memory_jobs();
        let job = s
            .create_job(
                "0 9 * * *",
                "Daily briefing.",
                None,
                None,
                None,
                "UTC",
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            job.created_version.as_deref(),
            Some(rustykrab_core::VERSION)
        );

        // And it survives the round-trip rather than only living on the
        // returned struct.
        let fetched = s.get_job(&job.id).await.unwrap();
        assert_eq!(
            fetched.created_version.as_deref(),
            Some(rustykrab_core::VERSION)
        );
    }

    #[tokio::test]
    async fn record_run_stamps_version_and_list_runs_reads_it_back() {
        let s = in_memory_jobs();
        let job = s
            .create_job(
                "0 9 * * *",
                "Daily briefing.",
                None,
                None,
                None,
                "UTC",
                false,
            )
            .await
            .unwrap();
        let now = Utc::now();
        let run = s
            .record_run(&job.id, "ok", Some("output"), now, now)
            .await
            .unwrap();
        assert_eq!(
            run.rustykrab_version.as_deref(),
            Some(rustykrab_core::VERSION)
        );

        let runs = s.list_runs(&job.id, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].rustykrab_version.as_deref(),
            Some(rustykrab_core::VERSION),
            "list_runs must project the version column, not drop it"
        );
    }

    #[tokio::test]
    async fn migration_adds_version_columns_without_backfilling_old_rows() {
        // A database created by a build that predates the version columns.
        // Rows already in it were produced by an unknown build, so they must
        // read back as None — back-filling them with the current version
        // would assert an attribution we don't actually have.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE scheduled_jobs (
                 id TEXT PRIMARY KEY, schedule TEXT NOT NULL, task TEXT NOT NULL,
                 channel TEXT, chat_id TEXT, thread_id TEXT,
                 one_shot INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1,
                 next_run_at TEXT NOT NULL, last_run_at TEXT, created_at TEXT NOT NULL,
                 conversation_id TEXT
             );
             CREATE TABLE job_runs (
                 id TEXT PRIMARY KEY, job_id TEXT NOT NULL, status TEXT NOT NULL,
                 output TEXT, started_at TEXT NOT NULL, finished_at TEXT NOT NULL
             );
             INSERT INTO scheduled_jobs (id, schedule, task, next_run_at, created_at)
                 VALUES ('old-job', '0 9 * * *', 'legacy task',
                         '2099-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00');
             INSERT INTO job_runs (id, job_id, status, output, started_at, finished_at)
                 VALUES ('old-run', 'old-job', 'ok', 'legacy output',
                         '2020-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00');",
        )
        .unwrap();

        // Migration must not fail on a table that already has rows.
        crate::Store::run_migrations(&conn).unwrap();
        let s = JobStore::new(Arc::new(Mutex::new(conn)));

        let job = s.get_job("old-job").await.unwrap();
        assert_eq!(job.task, "legacy task", "existing data must be preserved");
        assert_eq!(
            job.created_version, None,
            "pre-existing job must not be back-filled with the current version"
        );

        let runs = s.list_runs("old-job", 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].output.as_deref(), Some("legacy output"));
        assert_eq!(
            runs[0].rustykrab_version, None,
            "pre-existing run must not be back-filled with the current version"
        );

        // New writes into the migrated database do get stamped.
        let fresh = s
            .create_job("0 9 * * *", "new task", None, None, None, "UTC", false)
            .await
            .unwrap();
        assert_eq!(
            fresh.created_version.as_deref(),
            Some(rustykrab_core::VERSION)
        );
    }

    /// The exact shape of the 2026-06-24 incident: an agent that had lost
    /// the memory of scheduling the briefing tried to "replace" it, deleted
    /// nothing, and created a second job delivering the same task to the
    /// same Telegram thread on a different schedule. Four briefings a day
    /// followed, and nothing afterwards could tell the two apart.
    #[tokio::test]
    async fn create_refuses_a_second_job_for_the_same_task_and_target() {
        let jobs = in_memory_jobs();
        let first = jobs
            .create_job(
                "30 14,23 * * *",
                "Execute skill: daily_briefing",
                Some("telegram"),
                Some("-1003776932999"),
                Some("198"),
                "UTC",
                false,
            )
            .await
            .unwrap();

        let err = jobs
            .create_job(
                "30 7,16 * * *",
                "Execute skill: daily_briefing",
                Some("telegram"),
                Some("-1003776932999"),
                Some("198"),
                "UTC",
                false,
            )
            .await
            .expect_err("a duplicate delivery must be refused");

        assert!(
            matches!(err, Error::AlreadyExists(_)),
            "expected AlreadyExists, got {err:?}"
        );
        let msg = err.to_string();
        // The caller cannot act on "duplicate" alone — it needs the id to
        // delete, and the schedule to see whether the existing job is
        // already what was wanted.
        assert!(
            msg.contains(&first.id),
            "error must name the existing job: {msg}"
        );
        assert!(
            msg.contains("30 14,23 * * *"),
            "error must name its schedule: {msg}"
        );
        assert!(
            msg.contains("allow_duplicate"),
            "error must name the escape hatch: {msg}"
        );

        let listed = jobs.list_jobs().await.unwrap();
        assert_eq!(listed.len(), 1, "the second job must not have been written");
    }

    #[tokio::test]
    async fn allow_duplicate_permits_the_same_task_on_two_schedules() {
        // 8:00am and 5:30pm cannot be one cron expression — the minute
        // fields differ — so this genuinely is two jobs, and the check has
        // to be overridable rather than absolute.
        let jobs = in_memory_jobs();
        jobs.create_job(
            "0 8 * * *",
            "Execute skill: daily_briefing",
            Some("telegram"),
            Some("-1003776932999"),
            Some("198"),
            "America/Los_Angeles",
            false,
        )
        .await
        .unwrap();
        jobs.create_job(
            "30 17 * * *",
            "Execute skill: daily_briefing",
            Some("telegram"),
            Some("-1003776932999"),
            Some("198"),
            "America/Los_Angeles",
            true,
        )
        .await
        .unwrap();

        assert_eq!(jobs.list_jobs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_same_task_to_a_different_destination_is_not_a_duplicate() {
        // Identity includes the delivery target: the same briefing posted to
        // a second thread is a different job, not a mistake.
        let jobs = in_memory_jobs();
        for thread in ["198", "199"] {
            jobs.create_job(
                "0 9 * * *",
                "Execute skill: daily_briefing",
                Some("telegram"),
                Some("-1003776932999"),
                Some(thread),
                "UTC",
                false,
            )
            .await
            .unwrap_or_else(|e| panic!("thread {thread} should be allowed: {e}"));
        }
        assert_eq!(jobs.list_jobs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn one_shot_jobs_are_exempt_from_the_duplicate_check() {
        // Two identical reminders at different times are ordinary. Only
        // recurring jobs compound into a delivery-rate problem.
        let jobs = in_memory_jobs();
        for hour in ["2099-01-01T09:00:00Z", "2099-01-01T17:00:00Z"] {
            jobs.create_job(
                hour,
                "Remind me about the thing",
                None,
                None,
                None,
                "UTC",
                false,
            )
            .await
            .unwrap_or_else(|e| panic!("{hour} should be allowed: {e}"));
        }
        assert_eq!(jobs.list_jobs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_disabled_job_does_not_block_recreation() {
        // The executor disables jobs it finds unrunnable. A retired job must
        // not become a permanent tombstone that prevents scheduling the work
        // again.
        let jobs = in_memory_jobs();
        let first = jobs
            .create_job(
                "0 9 * * *",
                "briefing",
                Some("telegram"),
                Some("c"),
                None,
                "UTC",
                false,
            )
            .await
            .unwrap();
        jobs.set_enabled(&first.id, false).await.unwrap();

        jobs.create_job(
            "0 9 * * *",
            "briefing",
            Some("telegram"),
            Some("c"),
            None,
            "UTC",
            false,
        )
        .await
        .expect("a disabled job must not block a fresh one");
    }

    #[tokio::test]
    async fn delete_reports_not_found_rather_than_a_quiet_no_op() {
        // The other half of the incident: the delete named an id one
        // character off from the real one, returned "success", and the agent
        // proceeded to create a replacement for a job that still existed.
        let jobs = in_memory_jobs();
        let job = jobs
            .create_job("0 9 * * *", "briefing", None, None, None, "UTC", false)
            .await
            .unwrap();

        let mistyped = job.id.replace(
            job.id.chars().last().unwrap(),
            if job.id.ends_with('0') { "1" } else { "0" },
        );
        let err = jobs
            .delete_job(&mistyped)
            .await
            .expect_err("deleting an unknown id must not report success");
        assert!(
            matches!(err, Error::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert!(
            err.to_string().contains("cron list"),
            "error should say how to get valid ids: {err}"
        );

        // The real job is untouched, and deleting it properly succeeds.
        assert_eq!(jobs.list_jobs().await.unwrap().len(), 1);
        jobs.delete_job(&job.id).await.unwrap();
        assert!(jobs.list_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn migration_reads_pre_existing_jobs_as_utc() {
        // Rows written before the column existed had their cron fields
        // matched against UTC. Stamping them with the operator's local zone
        // would move every live job by the offset without anyone asking, so
        // the migration backfills the lens they were actually created under.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE scheduled_jobs (
                 id TEXT PRIMARY KEY, schedule TEXT NOT NULL, task TEXT NOT NULL,
                 channel TEXT, chat_id TEXT, thread_id TEXT,
                 one_shot INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1,
                 next_run_at TEXT NOT NULL, last_run_at TEXT, created_at TEXT NOT NULL,
                 conversation_id TEXT
             );
             CREATE TABLE job_runs (
                 id TEXT PRIMARY KEY, job_id TEXT NOT NULL, status TEXT NOT NULL,
                 output TEXT, started_at TEXT NOT NULL, finished_at TEXT NOT NULL
             );
             INSERT INTO scheduled_jobs (id, schedule, task, next_run_at, created_at)
                 VALUES ('old-job', '0 9 * * *', 'legacy task',
                         '2099-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00');",
        )
        .unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        let s = JobStore::new(Arc::new(Mutex::new(conn)));

        let job = s.get_job("old-job").await.unwrap();
        assert_eq!(job.timezone, "UTC");

        // And it keeps firing where it always did.
        s.mark_executed("old-job").await.unwrap();
        let advanced = s.get_job("old-job").await.unwrap();
        assert_eq!(advanced.next_run_at.format("%H:%M").to_string(), "09:00");
    }

    #[test]
    fn unknown_stored_zones_fall_back_to_utc_rather_than_failing() {
        // A row could hold a zone this build's database has dropped. The
        // scheduler must keep running; UTC is where those rows already were.
        assert_eq!(zone_or_utc(None), Tz::UTC);
        assert_eq!(zone_or_utc(Some("Mars/Olympus_Mons")), Tz::UTC);
        assert_eq!(
            zone_or_utc(Some("America/Los_Angeles")),
            Tz::America__Los_Angeles
        );
    }

    #[tokio::test]
    async fn create_job_records_the_zone_and_schedules_through_it() {
        let jobs = in_memory_jobs();
        let job = jobs
            .create_job(
                "0 9 * * *",
                "Daily briefing.",
                None,
                None,
                None,
                "America/Los_Angeles",
                false,
            )
            .await
            .unwrap();
        assert_eq!(job.timezone, "America/Los_Angeles");
        // 9am local lands on 16:00 or 17:00 UTC depending on the season, and
        // never on 09:00 UTC.
        let hour = job.next_run_at.format("%H:%M").to_string();
        assert!(
            hour == "16:00" || hour == "17:00",
            "expected 9am Pacific in UTC, got {hour}"
        );

        // And the zone survives the round-trip — it is what mark_executed
        // reads to advance the schedule.
        let reloaded = jobs.get_job(&job.id).await.unwrap();
        assert_eq!(reloaded.timezone, "America/Los_Angeles");
    }

    #[tokio::test]
    async fn create_job_rejects_an_unknown_zone() {
        let jobs = in_memory_jobs();
        let err = jobs
            .create_job("0 9 * * *", "task", None, None, None, "Pacific Time", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown timezone"), "got: {err}");
    }

    #[tokio::test]
    async fn mark_executed_advances_in_the_jobs_own_zone() {
        // The advance path reads the stored zone rather than the process
        // default, so a job keeps its wall-clock time even if the daemon's
        // RUSTYKRAB_TIMEZONE changes underneath it.
        let jobs = in_memory_jobs();
        let job = jobs
            .create_job(
                "0 9 * * *",
                "Daily briefing.",
                None,
                None,
                None,
                "America/Los_Angeles",
                false,
            )
            .await
            .unwrap();
        jobs.mark_executed(&job.id).await.unwrap();

        let advanced = jobs.get_job(&job.id).await.unwrap();
        let hour = advanced.next_run_at.format("%H:%M").to_string();
        assert!(
            hour == "16:00" || hour == "17:00",
            "advanced run should still be 9am Pacific, got {hour}"
        );
        assert!(advanced.last_run_at.is_some());
    }

    const LA: Tz = Tz::America__Los_Angeles;

    /// A UTC instant, written the way a test reads best.
    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn cron_hour_means_the_operators_hour_not_greenwichs() {
        // The bug this parameter exists to kill: "0 9 * * *" created by
        // someone in Los Angeles fired at 09:00 UTC, which is 01:00 for
        // them. Read through their zone it lands on 16:00 UTC — 9am Pacific.
        let next = compute_next_cron_run("0 9 * * *", utc(2026, 7, 1, 0, 0), LA).unwrap();
        assert_eq!(next, utc(2026, 7, 1, 16, 0), "9am PDT is 16:00 UTC");
    }

    #[test]
    fn the_same_expression_holds_its_wall_clock_across_dst() {
        // The reason a *zone* is threaded through and not the offset it
        // resolved to at creation. One expression, two different UTC
        // instants, because 9am Pacific is 16:00 UTC in July and 17:00 UTC
        // in January. A frozen -07:00 would start firing at 8am once the
        // clocks went back.
        let summer = compute_next_cron_run("0 9 * * *", utc(2026, 7, 1, 0, 0), LA).unwrap();
        let winter = compute_next_cron_run("0 9 * * *", utc(2026, 1, 1, 0, 0), LA).unwrap();
        assert_eq!(summer, utc(2026, 7, 1, 16, 0));
        assert_eq!(winter, utc(2026, 1, 1, 17, 0));
    }

    #[test]
    fn utc_zone_still_reads_cron_fields_as_utc() {
        // Every caller passes Tz::UTC for now, and existing jobs are stamped
        // 'UTC', so this is the path that must not move: same expression,
        // same instant as before.
        let next = compute_next_cron_run("0 9 * * *", utc(2026, 7, 1, 0, 0), Tz::UTC).unwrap();
        assert_eq!(next, utc(2026, 7, 1, 9, 0));
    }

    #[test]
    fn naive_one_shot_timestamps_are_local() {
        // Someone typing "2027-07-01T09:00" means nine in the morning where
        // they are. Reading it as UTC is the same off-by-the-offset bug as
        // the cron path, just on the one-shot side.
        let (one_shot, at) = parse_schedule("2027-07-01T09:00", utc(2026, 1, 1, 0, 0), LA).unwrap();
        assert!(one_shot);
        assert_eq!(at, utc(2027, 7, 1, 16, 0));
    }

    #[test]
    fn an_explicit_offset_overrides_the_zone() {
        // A trailing Z (or any explicit offset) already names an instant.
        // Re-interpreting it through the operator's zone would corrupt a
        // timestamp the caller was precise about.
        let (_, at) = parse_schedule("2027-07-01T09:00:00Z", utc(2026, 1, 1, 0, 0), LA).unwrap();
        assert_eq!(at, utc(2027, 7, 1, 9, 0));
    }

    #[test]
    fn ambiguous_local_times_take_the_earlier_instant() {
        // 2026-11-01 01:30 happens twice in Los Angeles. Picking the first
        // pass means the job fires once, at the first 01:30 — 08:30 UTC.
        let naive =
            NaiveDateTime::parse_from_str("2026-11-01T01:30:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(local_to_utc(naive, LA), utc(2026, 11, 1, 8, 30));
    }

    #[test]
    fn nonexistent_local_times_land_at_the_end_of_the_gap() {
        // 2026-03-08 02:30 never occurs in Los Angeles — the clock jumps
        // 02:00 → 03:00. The job has to fire anyway, at the first instant
        // that exists after the requested one: 03:00 PDT = 10:00 UTC.
        let naive =
            NaiveDateTime::parse_from_str("2026-03-08T02:30:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(local_to_utc(naive, LA), utc(2026, 3, 8, 10, 0));
    }

    #[test]
    fn compute_next_cron_5_field_expression() {
        let now = Utc::now();
        let result = compute_next_cron_run("*/5 * * * *", now, Tz::UTC);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap() > now);
    }

    #[test]
    fn compute_next_cron_6_field_expression() {
        let now = Utc::now();
        let result = compute_next_cron_run("0 */5 * * * *", now, Tz::UTC);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap() > now);
    }

    #[test]
    fn compute_next_cron_too_few_fields_gives_actionable_error() {
        let now = Utc::now();
        let err = compute_next_cron_run("* *", now, Tz::UTC).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid cron expression"), "got: {msg}");
        assert!(msg.contains("Example:"), "got: {msg}");
    }

    #[test]
    fn compute_next_cron_garbage_gives_actionable_error() {
        let now = Utc::now();
        let err = compute_next_cron_run("not a cron", now, Tz::UTC).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid cron expression"), "got: {msg}");
        assert!(msg.contains("Example:"), "got: {msg}");
    }

    #[test]
    fn compute_next_cron_unreachable_gives_actionable_error() {
        let now = Utc::now();
        // February 31st never exists
        let err = compute_next_cron_run("0 0 31 2 *", now, Tz::UTC).unwrap_err();
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
        let err = parse_schedule("2020-01-01T00:00:00Z", now, Tz::UTC).unwrap_err();
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
        let (one_shot, next_run) = parse_schedule(&future, now, Tz::UTC).unwrap();
        assert!(one_shot);
        assert!(next_run > now);
    }

    #[test]
    fn parse_schedule_valid_cron() {
        let now = Utc::now();
        let (one_shot, next_run) = parse_schedule("0 9 * * *", now, Tz::UTC).unwrap();
        assert!(!one_shot);
        assert!(next_run > now);
    }

    /// Build a [`JobStore`] backed by an in-memory SQLite connection.
    ///
    /// Uses the real `run_migrations` rather than a hand-written copy of
    /// the DDL: the duplicated copy silently drifted from production every
    /// time a column was added, failing these tests for a reason that had
    /// nothing to do with what they were checking.
    fn in_memory_jobs() -> JobStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        JobStore::new(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn conversation_id_round_trip() {
        let jobs = in_memory_jobs();
        let job = jobs
            .create_job("*/5 * * * *", "ping", None, None, None, "UTC", false)
            .await
            .unwrap();
        assert!(
            job.conversation_id.is_none(),
            "newly created jobs have no conversation yet"
        );

        jobs.set_conversation_id(&job.id, "conv-123").await.unwrap();
        let reloaded = jobs.get_job(&job.id).await.unwrap();
        assert_eq!(reloaded.conversation_id.as_deref(), Some("conv-123"));

        // list_jobs and get_due_jobs also propagate the column.
        let listed = jobs.list_jobs().await.unwrap();
        assert_eq!(listed[0].conversation_id.as_deref(), Some("conv-123"));
    }

    #[tokio::test]
    async fn thread_id_round_trip() {
        let jobs = in_memory_jobs();
        let job = jobs
            .create_job(
                "*/5 * * * *",
                "ping",
                Some("slack"),
                Some("C012345"),
                Some("1700000000.000100"),
                "UTC",
                false,
            )
            .await
            .unwrap();
        assert_eq!(job.thread_id.as_deref(), Some("1700000000.000100"));

        let reloaded = jobs.get_job(&job.id).await.unwrap();
        assert_eq!(reloaded.thread_id.as_deref(), Some("1700000000.000100"));
        assert_eq!(reloaded.channel.as_deref(), Some("slack"));
        assert_eq!(reloaded.chat_id.as_deref(), Some("C012345"));

        let listed = jobs.list_jobs().await.unwrap();
        assert_eq!(listed[0].thread_id.as_deref(), Some("1700000000.000100"));
    }

    #[tokio::test]
    async fn get_job_missing_returns_not_found() {
        let jobs = in_memory_jobs();
        let err = jobs.get_job("nope").await.unwrap_err();
        assert!(
            matches!(err, Error::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn record_run_caps_history_per_job() {
        let jobs = in_memory_jobs();
        let job = jobs
            .create_job("*/5 * * * *", "ping", None, None, None, "UTC", false)
            .await
            .unwrap();

        let base = Utc::now();
        for i in 0..(MAX_RUNS_PER_JOB + 10) {
            let ts = base + chrono::Duration::seconds(i as i64);
            jobs.record_run(&job.id, "ok", Some("out"), ts, ts)
                .await
                .unwrap();
        }

        let runs = jobs.list_runs(&job.id, MAX_RUNS_PER_JOB * 2).await.unwrap();
        assert_eq!(
            runs.len(),
            MAX_RUNS_PER_JOB as usize,
            "history should be capped at MAX_RUNS_PER_JOB"
        );
        // The newest run survives the prune; the oldest ten are gone.
        assert_eq!(
            runs[0].finished_at.timestamp(),
            (base + chrono::Duration::seconds((MAX_RUNS_PER_JOB + 9) as i64)).timestamp()
        );
    }
}
