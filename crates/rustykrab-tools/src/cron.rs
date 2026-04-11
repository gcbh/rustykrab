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
        "Manage scheduled tasks: create, list, or delete cron jobs."
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
                        "enum": ["create", "list", "delete"],
                        "description": "The action to perform"
                    },
                    "schedule": {
                        "type": "string",
                        "description": "Cron expression for recurring schedules (e.g. '0 9 * * *' for daily at 9am, '*/30 * * * *' for every 30 minutes) or an ISO 8601 timestamp for one-shot tasks (e.g. '2025-04-12T14:30:00Z'). Required for create."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task description or prompt to execute when the schedule fires (required for create)"
                    },
                    "channel": {
                        "type": "string",
                        "description": "Channel to deliver the result to (e.g. 'telegram', 'signal'). Include this so scheduled task results are sent to the right place."
                    },
                    "chat_id": {
                        "type": "string",
                        "description": "Chat identifier for the target channel (e.g. Telegram chat ID, Signal phone number)"
                    },
                    "job_id": {
                        "type": "string",
                        "description": "Job identifier (required for delete)"
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

                let result = self
                    .backend
                    .create_job(schedule, task, channel, chat_id)
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
            _ => Err(rustykrab_core::Error::ToolExecution(
                format!("unknown action: {action}").into(),
            )),
        }
    }
}
