use async_trait::async_trait;
use rustykrab_core::active_tools::{with_session_context, LateToolBinding};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::tool_catalog::definition;

/// Meta-tool that makes tools callable by exact name.
///
/// One contract with `tools_list` (plan section 12): a tool is made callable
/// by the conversation's [`LateToolBinding`], appended (its definition
/// returned as text, the tools array untouched) or declared. A tool that is
/// callable already, declared or appended by an earlier search, is a cheap
/// no-op that answers "already callable" and never touches the array: gemma4
/// called `tools_load` on a third of the tools a search had just delivered,
/// and each of those used to cost a full re-prefill.
pub struct ToolsLoadTool;

impl ToolsLoadTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ToolsLoadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ToolsLoadTool {
    fn name(&self) -> &str {
        "tools_load"
    }

    fn description(&self) -> &str {
        "Make tools callable by exact name, for a name you know that is not callable yet, \
         such as one from a tools_list catalog listing. Not needed for a tool a tools_list \
         search returned, or one in your tool list: those are callable already, so call them \
         directly. Loading an already callable tool changes nothing."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Exact tool names to make callable."
                    }
                },
                "required": ["names"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let names: Vec<String> = args
            .get("names")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::ToolExecution("missing `names` array".into()))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();

        if names.is_empty() {
            return Err(Error::ToolExecution(
                "`names` must contain at least one tool name".into(),
            ));
        }

        let result = with_session_context(|ctx| {
            let caps = ctx.capabilities.clone();

            let mut loaded = Vec::new();
            let mut unknown = Vec::new();
            let mut forbidden = Vec::new();

            for requested in &names {
                let matched = ctx
                    .all_tools
                    .iter()
                    .find(|t| t.name() == requested.as_str() && t.available());

                match matched {
                    None => unknown.push(requested.clone()),
                    Some(tool) => {
                        if caps.can_use_tool(tool.name()) {
                            if !loaded.iter().any(|n| n == tool.name()) {
                                loaded.push(tool.name().to_string());
                            }
                        } else {
                            forbidden.push(requested.clone());
                        }
                    }
                }
            }

            let made = ctx
                .active_tools
                .make_callable(ctx.conversation_id, loaded.iter().cloned());
            let appended: Vec<String> = match made.binding {
                LateToolBinding::Append => made.newly.clone(),
                LateToolBinding::Rerender => Vec::new(),
            };
            // The append itself: the model's only copy of these definitions.
            let definitions: Vec<Value> = appended
                .iter()
                .filter_map(|name| ctx.all_tools.iter().find(|t| t.name() == name.as_str()))
                .map(|tool| definition(&tool.schema()))
                .collect();

            let callable_now: Vec<String> = {
                let mut v: Vec<String> = ctx
                    .active_tools
                    .callable_for(ctx.conversation_id)
                    .into_iter()
                    .filter(|name| {
                        caps.can_use_tool(name)
                            && ctx
                                .all_tools
                                .iter()
                                .any(|tool| tool.name() == name && tool.available())
                    })
                    .collect();
                v.sort();
                v
            };

            let mut note = Vec::new();
            if !made.newly.is_empty() {
                note.push(match made.binding {
                    LateToolBinding::Append => format!(
                        "Callable now: {}. Call with your normal tool-call format, exactly \
                         like your other tools; the definitions are in `tools`.",
                        made.newly.join(", ")
                    ),
                    LateToolBinding::Rerender => format!(
                        "Callable now: {}. Call with your normal tool-call format.",
                        made.newly.join(", ")
                    ),
                });
            }
            if !made.already.is_empty() {
                note.push(format!(
                    "Already callable, nothing to load: {}. Call directly.",
                    made.already.join(", ")
                ));
            }
            if !unknown.is_empty() {
                note.push(format!(
                    "Not in the catalog: {}. Search with tools_list and a query.",
                    unknown.join(", ")
                ));
            }
            if !forbidden.is_empty() {
                note.push(format!("Not permitted here: {}.", forbidden.join(", ")));
            }

            json!({
                "note": note.join(" "),
                "loaded": loaded,
                "already_callable": made.already,
                "appended": appended,
                "tools": definitions,
                "unknown": unknown,
                "forbidden": forbidden,
                "active": callable_now,
            })
        })
        .ok_or_else(|| {
            Error::ToolExecution("tools_load invoked outside of an agent session context".into())
        })?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::active_tools::{
        ActiveToolsRegistry, SessionToolContext, SESSION_TOOL_CONTEXT,
    };
    use rustykrab_core::{capability::CapabilitySet, recall::RecallStore, todo::TodoStore};
    use std::sync::Arc;

    struct FixtureTool(&'static str, bool);
    #[async_trait]
    impl Tool for FixtureTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "inert availability fixture"
        }
        fn available(&self) -> bool {
            self.1
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.0.into(),
                description: self.description().into(),
                parameters: json!({"type":"object"}),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    #[tokio::test]
    async fn active_report_excludes_missing_unavailable_and_forbidden_seed_names() {
        let ctx = SessionToolContext {
            conversation_id: uuid::Uuid::new_v4(),
            capabilities: Arc::new(CapabilitySet::for_tools(&["browser", "unavailable"])),
            all_tools: Arc::new(vec![
                Arc::new(FixtureTool("browser", true)),
                Arc::new(FixtureTool("unavailable", false)),
                Arc::new(FixtureTool("forbidden", true)),
            ]),
            active_tools: Arc::new(ActiveToolsRegistry::with_seed([
                "memory_search",
                "unavailable",
                "forbidden",
            ])),
            recall: Arc::new(RecallStore::new()),
            todos: Arc::new(TodoStore::new()),
        };
        let result = SESSION_TOOL_CONTEXT
            .scope(
                ctx,
                ToolsLoadTool::new().execute(
                    json!({"names":["browser","memory_search","unavailable","forbidden"]}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result["active"], json!(["browser"]));
        assert_eq!(result["loaded"], json!(["browser"]));
        assert_eq!(result["unknown"], json!(["memory_search", "unavailable"]));
        assert_eq!(result["forbidden"], json!(["forbidden"]));
    }

    fn weather_ctx(binding: LateToolBinding) -> SessionToolContext {
        let registry = ActiveToolsRegistry::new();
        let conversation_id = uuid::Uuid::new_v4();
        registry.set_late_binding(conversation_id, binding);
        registry.activate(conversation_id, ["memory_search"]);
        SessionToolContext {
            conversation_id,
            capabilities: Arc::new(CapabilitySet::for_tools(&[
                "memory_search",
                "get_weather",
                "track_package",
            ])),
            all_tools: Arc::new(vec![
                Arc::new(FixtureTool("memory_search", true)),
                Arc::new(FixtureTool("get_weather", true)),
                Arc::new(FixtureTool("track_package", true)),
            ]),
            active_tools: Arc::new(registry),
            recall: Arc::new(RecallStore::new()),
            todos: Arc::new(TodoStore::new()),
        }
    }

    async fn load(ctx: &SessionToolContext, names: Value) -> Value {
        SESSION_TOOL_CONTEXT
            .scope(
                ctx.clone(),
                ToolsLoadTool::new().execute(json!({ "names": names })),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn under_append_a_load_returns_the_definition_and_keeps_the_array() {
        let ctx = weather_ctx(LateToolBinding::Append);
        let r = load(&ctx, json!(["get_weather"])).await;
        assert_eq!(r["appended"], json!(["get_weather"]));
        assert_eq!(r["tools"][0]["function"]["name"], "get_weather");
        assert!(r["note"]
            .as_str()
            .unwrap()
            .starts_with("Callable now: get_weather."));
        assert_eq!(r["active"], json!(["get_weather", "memory_search"]));
        assert!(ctx
            .active_tools
            .is_appended(ctx.conversation_id, "get_weather"));
        assert_eq!(
            ctx.active_tools.version(ctx.conversation_id),
            1,
            "only the seed's"
        );
    }

    #[tokio::test]
    async fn loading_a_callable_tool_is_a_no_op_that_says_so() {
        let ctx = weather_ctx(LateToolBinding::Append);
        // Appended by an earlier search, as tools_list does.
        ctx.active_tools
            .make_callable(ctx.conversation_id, ["get_weather"]);
        let version = ctx.active_tools.version(ctx.conversation_id);

        let r = load(&ctx, json!(["get_weather", "memory_search"])).await;
        assert_eq!(
            r["already_callable"],
            json!(["get_weather", "memory_search"])
        );
        assert_eq!(r["appended"], json!([]));
        assert_eq!(r["tools"], json!([]), "nothing repeated");
        assert!(r["note"]
            .as_str()
            .unwrap()
            .starts_with("Already callable, nothing to load: get_weather, memory_search."));
        assert_eq!(ctx.active_tools.version(ctx.conversation_id), version);
    }

    #[tokio::test]
    async fn under_rerender_a_load_declares_the_tool() {
        let ctx = weather_ctx(LateToolBinding::Rerender);
        let before = ctx.active_tools.version(ctx.conversation_id);
        let r = load(&ctx, json!(["track_package", "nope"])).await;
        assert_eq!(r["loaded"], json!(["track_package"]));
        assert_eq!(r["appended"], json!([]));
        assert_eq!(r["tools"], json!([]));
        assert_eq!(r["unknown"], json!(["nope"]));
        assert!(ctx
            .active_tools
            .is_active(ctx.conversation_id, "track_package"));
        assert!(ctx.active_tools.version(ctx.conversation_id) > before);
    }
}
