use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rustykrab_core::timezone;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::cron_backend::CronBackend;

/// A tool that manages scheduled tasks: create, list, or delete cron jobs.
pub struct CronTool {
    backend: Arc<dyn CronBackend>,
}

impl CronTool {
    pub fn new(backend: Arc<dyn CronBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Manage scheduled tasks: create, list, delete cron jobs, or view run history."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "list", "delete", "list_runs"],
                        "description": "The action to perform"
                    },
                    "schedule": {
                        "type": "string",
                        "description": concat!(
                            "Required for create. Must be ONE of:\n",
                            "\n",
                            "All times are interpreted in the USER'S local timezone, not UTC. ",
                            "Write the hour the user says: if they ask for 9am, that is '0 9 * * *'. ",
                            "Do NOT convert to UTC yourself — the scheduler stores the zone with the ",
                            "job and re-derives the offset on every fire, so the job stays at 9am ",
                            "local across daylight-saving changes. Converting by hand freezes the ",
                            "offset and the job drifts an hour twice a year.\n",
                            "\n",
                            "1) Standard 5-field cron expression: minute hour day-of-month month day-of-week\n",
                            "   Fields: minute(0-59) hour(0-23) day(1-31) month(1-12) weekday(0-6, 0=Sun)\n",
                            "   Allowed operators: * (any), */N (every N), N-M (range), N,M (list)\n",
                            "   Examples:\n",
                            "   - '0 9 * * *'     → daily at 9:00 AM\n",
                            "   - '*/30 * * * *'  → every 30 minutes\n",
                            "   - '0 9 * * 1-5'   → weekdays at 9:00 AM\n",
                            "   - '0 0 1 * *'     → first day of every month at midnight\n",
                            "   - '0 8,12,18 * * *' → daily at 8 AM, noon, and 6 PM\n",
                            "\n",
                            "2) ISO 8601 timestamp for one-shot tasks (must be in the future):\n",
                            "   - '2025-04-12T14:30' or '2025-04-12T14:30:00' → local time\n",
                            "   - '2025-04-12T14:30:00Z' → explicit UTC (trailing Z), used as-is\n",
                            "\n",
                            "IMPORTANT: Use only the standard 5-field format. Do NOT use non-standard extensions, ",
                            "named months/days (like 'MON'), or 6-field expressions.",
                        )
                    },
                    "task": {
                        "type": "string",
                        "description": concat!(
                            "Required for create. The prompt that will be executed when the schedule fires.\n",
                            "\n",
                            "CRITICAL: this string is the ONLY thing carried forward from this conversation. ",
                            "When the job fires — possibly days later — a fresh agent run receives this text and ",
                            "nothing else: no chat history, no memory of what the user just told you, no access ",
                            "to what you and the user worked out together. A short label like 'daily briefing' ",
                            "or 'check emails' will produce a generic, useless result.\n",
                            "\n",
                            "Write a self-contained brief. Fold in everything the user told you that the future ",
                            "run needs:\n",
                            "  - What to do, as an explicit instruction (not a topic label)\n",
                            "  - Which sources/tools to use, and what to ignore\n",
                            "  - Any filters, thresholds, names, accounts, or projects the user named\n",
                            "  - Output format, length, tone, and ordering the user asked for\n",
                            "  - Any standing preferences or constraints from this conversation\n",
                            "\n",
                            "BAD  (too thin — produces generic output):\n",
                            "  'Morning inbox summary'\n",
                            "\n",
                            "GOOD (self-contained):\n",
                            "  'Summarize my unread email from the last 24h. Only include mail from real ",
                            "people — skip newsletters, receipts, and automated notifications. Lead with ",
                            "anything from my direct team (Ana, Priya, Marcus). Format as at most 5 bullets, ",
                            "each one line, no preamble or sign-off. If nothing qualifies, reply exactly ",
                            "\"Nothing needing attention.\"'\n",
                            "\n",
                            "Exception: if the task is exactly a registered SKILL.md skill name (e.g. ",
                            "'morning-briefing'), the bare name is fine — the skill body is injected ",
                            "automatically at fire time.",
                        )
                    },
                    "channel": {
                        "type": "string",
                        "description": "Channel to deliver the result to (e.g. 'telegram', 'slack', 'signal'). Include this so scheduled task results are sent to the right place."
                    },
                    "chat_id": {
                        "type": "string",
                        "description": "Chat identifier for the target channel (Telegram chat ID, Slack channel ID, Signal phone number)"
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Optional thread identifier so the result lands in the same thread that scheduled it. Telegram: forum topic thread_id. Slack: thread_ts (e.g. '1700000000.000100'). Omit for top-level."
                    },
                    "allow_duplicate": {
                        "type": "boolean",
                        "description": concat!(
                            "Optional for create, default false. Creating a second recurring ",
                            "job with the same task and the same delivery target is refused, ",
                            "because that is what a failed replace looks like and it silently ",
                            "doubles how often the user is messaged. If the create fails this ",
                            "way, do NOT retry with allow_duplicate — call list, and either use ",
                            "the job that already exists or delete it and recreate. Set this ",
                            "only when the user genuinely wants the same task on two schedules ",
                            "that cannot be one expression (e.g. 8:00am and 5:30pm, whose ",
                            "minute fields differ).",
                        )
                    },
                    "timezone": {
                        "type": "string",
                        "description": concat!(
                            "Optional IANA timezone name for create, e.g. 'America/Los_Angeles' ",
                            "or 'Europe/Berlin'. Omit it unless the user explicitly asks for a ",
                            "schedule in some OTHER zone than their own — omitted means the ",
                            "operator's configured local zone, which is almost always what is ",
                            "wanted. Never pass a fixed offset like 'UTC-8'; it does not track ",
                            "daylight saving.",
                        )
                    },
                    "job_id": {
                        "type": "string",
                        "description": "Job identifier (required for delete and list_runs)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of run records to return (default 20, used with list_runs)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing action".into()))?;

        match action {
            "create" => {
                let schedule = args["schedule"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "missing schedule for create action".into(),
                    )
                })?;

                let task = args["task"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing task for create action".into())
                })?;
                if task.trim().is_empty() {
                    // An empty task makes the scheduled prompt collapse to
                    // "Task: " on every fire, which the model reasonably
                    // refuses ("no task or instruction has been provided").
                    // Reject at creation time so the operator sees the
                    // error instead of a string of mysterious cron failures.
                    return Err(rustykrab_core::Error::ToolExecution(
                        "task must be a non-empty description of the work to perform".into(),
                    ));
                }

                let channel = args["channel"].as_str();
                let chat_id = args["chat_id"].as_str();
                let thread_id = args["thread_id"].as_str();
                let timezone = args["timezone"].as_str();
                let allow_duplicate = args["allow_duplicate"].as_bool().unwrap_or(false);

                let result = self
                    .backend
                    .create_job(
                        schedule,
                        task,
                        channel,
                        chat_id,
                        thread_id,
                        timezone,
                        allow_duplicate,
                    )
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                Ok(json!({
                    "action": "create",
                    "job": result,
                }))
            }
            "list" => {
                let mut jobs = self
                    .backend
                    .list_jobs()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                annotate_local_times(&mut jobs);

                Ok(json!({
                    "action": "list",
                    "jobs": jobs,
                }))
            }
            "delete" => {
                let job_id = args["job_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("missing job_id for delete action".into())
                })?;

                let result = self
                    .backend
                    .delete_job(job_id)
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                Ok(json!({
                    "action": "delete",
                    "result": result,
                }))
            }
            "list_runs" => {
                let job_id = args["job_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "missing job_id for list_runs action".into(),
                    )
                })?;

                let limit = args["limit"].as_u64().unwrap_or(20) as u32;

                let runs = self
                    .backend
                    .list_runs(job_id, limit)
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                Ok(json!({
                    "action": "list_runs",
                    "job_id": job_id,
                    "runs": runs,
                }))
            }
            _ => Err(rustykrab_core::Error::ToolExecution(
                format!("unknown action: {action}").into(),
            )),
        }
    }
}

/// Add a `next_run_local` field to each listed job, rendered in that job's
/// own zone.
///
/// The stored `next_run_at` is UTC, which is right for the database and
/// useless in a chat reply: a user who asked for a 9am briefing and is shown
/// `16:00Z` cannot tell at a glance whether it worked. Rendering it beside
/// the UTC value — rather than replacing it — keeps the answer checkable in
/// both directions. Jobs missing or carrying an unparseable zone are left
/// untouched rather than guessed at.
fn annotate_local_times(jobs: &mut Value) {
    let Some(entries) = jobs.as_array_mut() else {
        return;
    };
    for job in entries {
        let Some(tz) = job["timezone"]
            .as_str()
            .and_then(|n| timezone::parse(n).ok())
        else {
            continue;
        };
        let Some(next) = job["next_run_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            continue;
        };
        let local = tz.from_utc_datetime(&next.with_timezone(&Utc).naive_utc());
        if let Some(obj) = job.as_object_mut() {
            obj.insert(
                "next_run_local".to_string(),
                json!(local.format("%Y-%m-%d %H:%M %Z").to_string()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub backend that records whether `create_job` was reached, and
    /// with which timezone.
    struct SpyBackend {
        called: std::sync::atomic::AtomicBool,
        timezone: std::sync::Mutex<Option<String>>,
        allow_duplicate: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl CronBackend for SpyBackend {
        async fn create_job(
            &self,
            _schedule: &str,
            _task: &str,
            _channel: Option<&str>,
            _chat_id: Option<&str>,
            _thread_id: Option<&str>,
            timezone: Option<&str>,
            allow_duplicate: bool,
        ) -> Result<Value> {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            *self.timezone.lock().unwrap() = timezone.map(str::to_string);
            self.allow_duplicate
                .store(allow_duplicate, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({"ok": true}))
        }
        async fn list_jobs(&self) -> Result<Value> {
            Ok(json!([{
                "id": "job-1",
                "schedule": "0 9 * * *",
                "timezone": "America/Los_Angeles",
                "next_run_at": "2026-07-01T16:00:00+00:00",
            }]))
        }
        async fn delete_job(&self, _job_id: &str) -> Result<Value> {
            Ok(json!({"deleted": false}))
        }
        async fn list_runs(&self, _job_id: &str, _limit: u32) -> Result<Value> {
            Ok(json!([]))
        }
    }

    fn spy() -> (Arc<SpyBackend>, CronTool) {
        let backend = Arc::new(SpyBackend {
            called: std::sync::atomic::AtomicBool::new(false),
            timezone: std::sync::Mutex::new(None),
            allow_duplicate: std::sync::atomic::AtomicBool::new(true),
        });
        let tool = CronTool::new(backend.clone());
        (backend, tool)
    }

    #[tokio::test]
    async fn create_rejects_empty_task() {
        // Empty/whitespace tasks would propagate to the executor as
        // "Task: " with no body, prompting the model to refuse with
        // "no task or instruction has been provided" on every fire.
        // Catch it at creation time.
        let (backend, tool) = spy();
        let err = tool
            .execute(json!({
                "action": "create",
                "schedule": "0 9 * * *",
                "task": "",
            }))
            .await
            .expect_err("empty task must be rejected");
        assert!(
            err.to_string().to_lowercase().contains("non-empty"),
            "error should explain why: got {err}"
        );
        assert!(
            !backend.called.load(std::sync::atomic::Ordering::SeqCst),
            "backend.create_job should not have been reached"
        );
    }

    #[tokio::test]
    async fn create_rejects_whitespace_only_task() {
        let (backend, tool) = spy();
        tool.execute(json!({
            "action": "create",
            "schedule": "0 9 * * *",
            "task": "   \t\n  ",
        }))
        .await
        .expect_err("whitespace-only task must be rejected");
        assert!(!backend.called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn task_schema_demands_a_self_contained_brief() {
        // The task string is the only context that survives into the
        // scheduled run — no chat history travels with it. If the schema
        // doesn't say so, models write short topic labels ("daily
        // briefing") and the fired job produces generic output.
        let (_, tool) = spy();
        let schema = tool.schema();
        let task_desc = schema.parameters["properties"]["task"]["description"]
            .as_str()
            .expect("task description should be a string");

        assert!(
            task_desc.contains("ONLY thing carried forward"),
            "schema must warn that task is the sole surviving context: {task_desc}"
        );
        assert!(
            task_desc.contains("no chat history"),
            "schema must state that chat history is not carried over: {task_desc}"
        );
        // A worked BAD/GOOD pair moves models off one-line labels far more
        // reliably than an abstract instruction does.
        assert!(
            task_desc.contains("BAD") && task_desc.contains("GOOD"),
            "schema should show a contrasting example pair: {task_desc}"
        );
        // Bare skill names must stay explicitly legal — resolve_skill_for_task
        // relies on exact-name tasks and there is no minimum-length gate.
        assert!(
            task_desc.contains("SKILL.md skill name"),
            "schema must keep the bare-skill-name path legal: {task_desc}"
        );
    }

    #[tokio::test]
    async fn bare_skill_name_task_is_still_accepted() {
        // Guard against anyone "fixing" thin tasks with a length check:
        // `task = "morning-briefing"` is a supported path that gets the
        // skill body injected at fire time.
        let (backend, tool) = spy();
        tool.execute(json!({
            "action": "create",
            "schedule": "0 9 * * *",
            "task": "morning-briefing",
        }))
        .await
        .expect("bare skill name should remain a valid task");
        assert!(backend.called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn create_accepts_real_task() {
        let (backend, tool) = spy();
        let result = tool
            .execute(json!({
                "action": "create",
                "schedule": "0 9 * * *",
                "task": "Write the daily briefing.",
            }))
            .await
            .expect("real task should succeed");
        assert_eq!(result["action"], "create");
        assert!(backend.called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn create_passes_the_timezone_through_and_omits_it_by_default() {
        // Omitted means "the operator's zone", which only the CLI adapter
        // knows how to resolve — the tool must forward the absence rather
        // than substituting UTC and re-introducing the off-by-the-offset bug.
        let (backend, tool) = spy();
        tool.execute(json!({
            "action": "create",
            "schedule": "0 9 * * *",
            "task": "Write the daily briefing.",
        }))
        .await
        .unwrap();
        assert_eq!(*backend.timezone.lock().unwrap(), None);

        tool.execute(json!({
            "action": "create",
            "schedule": "0 9 * * *",
            "task": "Write the daily briefing.",
            "timezone": "Europe/Berlin",
        }))
        .await
        .unwrap();
        assert_eq!(
            backend.timezone.lock().unwrap().as_deref(),
            Some("Europe/Berlin"),
            "an explicit zone must reach the backend unchanged"
        );
    }

    #[tokio::test]
    async fn list_renders_next_run_in_the_jobs_own_zone() {
        // "16:00Z" tells a user who asked for a 9am briefing nothing about
        // whether they got one. Show both.
        let (_, tool) = spy();
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        let job = &result["jobs"][0];
        assert_eq!(
            job["next_run_local"], "2026-07-01 09:00 PDT",
            "16:00 UTC is 9am Pacific; got {:?}",
            job["next_run_local"]
        );
        assert_eq!(
            job["next_run_at"], "2026-07-01T16:00:00+00:00",
            "the UTC value must survive alongside the rendering"
        );
    }

    #[test]
    fn schedule_schema_tells_the_model_not_to_convert_to_utc() {
        // Left to itself the model helpfully converts 9am to 16:00 and
        // writes '0 16 * * *', which freezes the summer offset and drifts
        // an hour when the clocks change. The schema has to forbid it.
        let (_, tool) = spy();
        let schema = tool.schema();
        let desc = schema.parameters["properties"]["schedule"]["description"]
            .as_str()
            .expect("schedule description should be a string");
        assert!(
            desc.contains("USER'S local timezone"),
            "schema must name the lens: {desc}"
        );
        assert!(
            desc.contains("Do NOT convert to UTC"),
            "schema must forbid hand-conversion: {desc}"
        );
    }

    #[tokio::test]
    async fn create_defaults_allow_duplicate_to_false() {
        // The default has to be the safe one: an agent that omits the flag
        // is exactly the agent that does not know a duplicate is possible.
        let (backend, tool) = spy();
        tool.execute(json!({
            "action": "create",
            "schedule": "0 9 * * *",
            "task": "Write the daily briefing.",
        }))
        .await
        .unwrap();
        assert!(
            !backend
                .allow_duplicate
                .load(std::sync::atomic::Ordering::SeqCst),
            "an omitted allow_duplicate must reach the backend as false"
        );

        tool.execute(json!({
            "action": "create",
            "schedule": "30 17 * * *",
            "task": "Write the daily briefing.",
            "allow_duplicate": true,
        }))
        .await
        .unwrap();
        assert!(backend
            .allow_duplicate
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn allow_duplicate_schema_warns_against_using_it_as_a_retry() {
        // The failure mode this flag invites: create is refused, the model
        // reads "refused" as "try harder", sets the flag, and writes the
        // duplicate the check existed to stop.
        let (_, tool) = spy();
        let schema = tool.schema();
        let desc = schema.parameters["properties"]["allow_duplicate"]["description"]
            .as_str()
            .expect("allow_duplicate description should be a string");
        assert!(
            desc.contains("do NOT retry with allow_duplicate"),
            "schema must forbid using the flag as a retry: {desc}"
        );
    }
}
