use async_trait::async_trait;
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
                            "   - '2025-04-12T14:30:00Z'\n",
                            "\n",
                            "IMPORTANT: Use only the standard 5-field format. Do NOT use non-standard extensions, ",
                            "named months/days (like 'MON'), or 6-field expressions.",
                        )
                    },
                    "task": {
                        "type": "string",
                        "description": "The task description or prompt to execute when the schedule fires (required for create)"
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

                let channel = args["channel"].as_str();
                let chat_id = args["chat_id"].as_str();
                let thread_id = args["thread_id"].as_str();

                let result = self
                    .backend
                    .create_job(schedule, task, channel, chat_id, thread_id)
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                Ok(json!({
                    "action": "create",
                    "job": result,
                }))
            }
            "list" => {
                let jobs = self
                    .backend
                    .list_jobs()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

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
