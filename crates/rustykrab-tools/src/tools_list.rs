use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use rustykrab_core::active_tools::{
    with_session_context, LateToolBinding, SearchMiss, SessionToolContext,
};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::tool_catalog::{self, Category, MAX_APPENDED_PER_SEARCH};
use crate::work_backend::with_work_run;

/// The two meta-tools never appear in their own catalog.
const META: &[&str] = &["tools_list", "tools_load"];

/// Meta-tool that searches the tool catalog and makes what it finds
/// callable, or lists the catalog by category.
///
/// Search and load are one contract (plan section 12): a search that finds
/// a tool makes it callable in the same step, by the conversation's
/// [`LateToolBinding`]. Under `Append` the result carries each found tool's
/// definition as text and the tools array is left alone, so the cached
/// prompt prefix survives; under `Rerender` the found tools join the array.
/// Either way the result says the tools are callable now, and `tools_load`
/// on one of them answers that it already is. Two descriptions that
/// disagreed about this cost gemma4 a third of its late-bound calls.
///
/// A match is labelled found only when [`tool_catalog`]'s plausibility test
/// passes; otherwise the result says nothing matched and names the nearest
/// tools as non-matches, so a near-miss is not taken for the tool.
///
/// A need searched again and again is answered as final: once a run's
/// searches for one need (see [`tool_catalog`]) have found nothing as many
/// times as the harness profile's `tool_search_miss_limit` (2 by default),
/// the next one that finds nothing says no tool provides it and that the
/// model should tell the user so (a worker: report `needs_tool`), and the
/// need is recorded as a tool gap in the run's registry, logged as
/// `capability_gap/tool`, for a worker run's ladder to take (control plan
/// section 8, order 2). Within a need, a tool already named a non-match is
/// not found by a later, broader search.
pub struct ToolsListTool;

impl ToolsListTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ToolsListTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The permitted, available tools a session can search, in catalog order,
/// with their schemas. Never the meta-tools themselves.
fn candidates(ctx: &SessionToolContext) -> Vec<ToolSchema> {
    ctx.all_tools
        .iter()
        .filter(|t| t.available() && ctx.capabilities.can_use_tool(t.name()))
        .filter(|t| !META.contains(&t.name()))
        .map(|t| t.schema())
        .collect()
}

/// A search: find, make callable, and describe the result as text the model
/// reads like the late binding experiment's framing (its first line), with
/// the definitions or the non-matches as JSON after it.
fn search(ctx: &SessionToolContext, query: &str) -> String {
    let catalog = candidates(ctx);
    let mut result = tool_catalog::search(query, &catalog);
    let quoted = query.trim();

    // Earlier misses of this need, and the tools they named non-matches,
    // which a broader search for it may not now call found.
    let need = tool_catalog::need_terms(query, &result);
    let earlier: Vec<SearchMiss> = ctx
        .active_tools
        .search_misses(ctx.conversation_id)
        .into_iter()
        .filter(|miss| tool_catalog::same_need(&miss.terms, &need))
        .collect();
    let ruled_out: HashSet<&str> = earlier
        .iter()
        .flat_map(|miss| miss.near.iter().map(String::as_str))
        .collect();
    let (found, excluded): (Vec<_>, Vec<_>) = std::mem::take(&mut result.found)
        .into_iter()
        .partition(|f| !ruled_out.contains(f.name.as_str()));
    result.found = found;

    if result.found.is_empty() {
        let near: Vec<Value> = if excluded.is_empty() {
            result
                .near
                .iter()
                .map(|n| json!({ "name": n.name, "not_a_match": n.why() }))
                .collect()
        } else {
            let first = earlier.first().map_or(quoted, |m| m.query.as_str());
            excluded
                .iter()
                .map(|f| {
                    json!({
                        "name": f.name,
                        "not_a_match": format!(
                            "already named a non-match for the same need ('{first}')"
                        ),
                    })
                })
                .collect()
        };
        ctx.active_tools.record_search_miss(
            ctx.conversation_id,
            SearchMiss {
                terms: need,
                query: quoted.to_string(),
                near: near
                    .iter()
                    .filter_map(|n| n["name"].as_str().map(str::to_string))
                    .collect(),
            },
        );
        if earlier.len() >= ctx.active_tools.search_miss_limit(ctx.conversation_id) {
            return no_tool_provides(ctx, &earlier, quoted);
        }
        if near.is_empty() {
            return format!(
                "No tool matched \"{quoted}\": none of its words appear in the tool catalog. \
                 Search again with other words, or call tools_list without a query to see the \
                 catalog by category."
            );
        }
        return format!(
            "No tool matched \"{quoted}\". The nearest tools are listed below as non-matches: \
             none of them does what you searched for, so do not use one in its place. Search \
             again with other words, or tell the user no tool can do this.\n{}",
            Value::Array(near)
        );
    }

    let (appendable, overflow) = if result.found.len() > MAX_APPENDED_PER_SEARCH {
        result.found.split_at(MAX_APPENDED_PER_SEARCH)
    } else {
        (&result.found[..], &[][..])
    };
    let made = ctx.active_tools.make_callable(
        ctx.conversation_id,
        appendable.iter().map(|f| f.name.clone()),
    );
    let schema_of = |name: &str| catalog.iter().find(|s| s.name == name);

    let mut head = match made.newly.len() {
        0 => format!("Found {} tool(s) for \"{quoted}\".", appendable.len()),
        1 => format!(
            "Found 1 tool for \"{quoted}\". It is callable now: call it with your normal \
             tool-call format, exactly like your other tools. No tools_load call is needed."
        ),
        n => format!(
            "Found {n} tools for \"{quoted}\". They are callable now: call them with your \
             normal tool-call format, exactly like your other tools. No tools_load call is \
             needed."
        ),
    };
    if !made.already.is_empty() {
        head.push_str(&format!(
            " Already callable, so not repeated below: {}.",
            made.already.join(", ")
        ));
    }
    if !overflow.is_empty() {
        let more: Vec<&str> = overflow.iter().map(|f| f.name.as_str()).collect();
        head.push_str(&format!(
            " {} more matched and were not loaded: {}. Load one with tools_load if you need it.",
            more.len(),
            more.join(", ")
        ));
    }
    let body = match made.binding {
        // The definitions are the append: the model's only copy of them.
        LateToolBinding::Append => Value::Array(
            made.newly
                .iter()
                .filter_map(|n| schema_of(n))
                .map(tool_catalog::definition)
                .collect(),
        ),
        // They are in the tools array from the next request; names suffice.
        LateToolBinding::Rerender => json!(made.newly),
    };
    format!("{head}\n{body}")
}

/// The final answer to a need searched past the run's limit: no tool
/// provides it, stop, and say so (a worker: report it, typed). Records the
/// need as the run's tool gap under its first wording.
fn no_tool_provides(ctx: &SessionToolContext, earlier: &[SearchMiss], quoted: &str) -> String {
    let need = earlier.first().map_or(quoted, |m| m.query.as_str());
    let searches = earlier.len() + 1;
    ctx.active_tools
        .record_tool_gap(ctx.conversation_id, need.to_string());
    tracing::warn!(
        conversation_id = %ctx.conversation_id,
        class = "capability_gap",
        subclass = "tool",
        need,
        searches,
        "no tool provides this need; searching stopped"
    );
    let head = format!(
        "No tool provides \"{need}\". The tool catalog has been searched for it {searches} \
         times in this run and nothing matched, so this is final: do not search for it again, \
         and do not use a near-miss in its place."
    );
    if with_work_run(|_| ()).is_some() {
        let needs = Value::Array(vec![Value::String(need.to_string())]);
        format!(
            "{head} End your work item now with result_report, setting blocked.reason to \
             \"needs_tool\" and blocked.needs to {needs}, so the controller can acquire or \
             build the tool."
        )
    } else {
        format!("{head} Tell the user plainly that you have no tool that can do this.")
    }
}

/// The catalog by category, MCP tools grouped by server. Loads nothing.
fn listing(ctx: &SessionToolContext, filter: Option<&str>) -> Value {
    let mut groups: BTreeMap<Category, Vec<Value>> = BTreeMap::new();
    for schema in candidates(ctx) {
        let category = tool_catalog::categorize(&schema.name);
        if filter.is_some_and(|f| !category.selected_by(f)) {
            continue;
        }
        groups.entry(category).or_default().push(json!({
            "name": schema.name,
            "description": tool_catalog::summary(&schema.description),
        }));
    }
    let categories: Vec<Value> = groups
        .into_iter()
        .map(|(category, tools)| match category {
            Category::Named(name) => json!({ "category": name, "tools": tools }),
            Category::Mcp(server) => json!({ "category": "mcp", "server": server, "tools": tools }),
        })
        .collect();
    json!({
        "note": "Nothing was loaded. Call tools_list with a query to make a tool callable, \
                 or tools_load with its exact name.",
        "categories": categories,
    })
}

#[async_trait]
impl Tool for ToolsListTool {
    fn name(&self) -> &str {
        "tools_list"
    }

    fn description(&self) -> &str {
        "Search the tool catalog for a capability you do not have. With `query` (the \
         capability, in a few words), every matching tool becomes callable at once: the \
         result carries each match's definition and you call it directly with your normal \
         tool-call format, with no tools_load call. If nothing matches, the result says so \
         and names the nearest tools as non-matches. Without `query`, lists the catalog by \
         category (MCP tools grouped by server) and loads nothing."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The capability you need, in a few words (e.g. \"current weather\")."
                    },
                    "category": {
                        "type": "string",
                        "description": "Without a query: list only this category (e.g. \"filesystem\", \"web\", \"mcp\", \"mcp:<server>\")."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_owned);
        let category = args
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_owned);

        with_session_context(|ctx| match &query {
            Some(q) => Value::String(search(ctx, q)),
            None => listing(ctx, category.as_deref()),
        })
        .ok_or_else(|| {
            Error::ToolExecution("tools_list invoked outside of an agent session context".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::active_tools::{ActiveToolsRegistry, SESSION_TOOL_CONTEXT};
    use rustykrab_core::{capability::CapabilitySet, recall::RecallStore, todo::TodoStore};
    use std::sync::Arc;
    use uuid::Uuid;

    struct Fixture(&'static str, &'static str);
    #[async_trait]
    impl Tool for Fixture {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            self.1
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.0.into(),
                description: self.1.into(),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    fn ctx(binding: LateToolBinding) -> SessionToolContext {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ToolsListTool::new()),
            Arc::new(Fixture(
                "get_forecast",
                "Get a multi-day weather forecast for a city.",
            )),
            Arc::new(Fixture(
                "get_weather",
                "Get the current weather for a city.",
            )),
            Arc::new(Fixture(
                "get_uv_index",
                "Get the current UV index for a city.",
            )),
            Arc::new(Fixture(
                "read",
                "Read the contents of a file at a given path.",
            )),
            Arc::new(Fixture(
                "mcp__linear__create_issue",
                "Create a Linear issue.",
            )),
            Arc::new(Fixture("mcp__jira__search", "Search Jira.")),
            Arc::new(Fixture(
                "forbidden_weather",
                "Current weather, but not yours to call.",
            )),
        ];
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.name())
            .filter(|n| *n != "forbidden_weather")
            .collect();
        let registry = ActiveToolsRegistry::new();
        let conversation_id = Uuid::new_v4();
        registry.set_late_binding(conversation_id, binding);
        SessionToolContext {
            conversation_id,
            capabilities: Arc::new(CapabilitySet::for_tools(&names)),
            all_tools: Arc::new(tools),
            active_tools: Arc::new(registry),
            recall: Arc::new(RecallStore::new()),
            todos: Arc::new(TodoStore::new()),
        }
    }

    async fn call(ctx: &SessionToolContext, args: Value) -> Value {
        SESSION_TOOL_CONTEXT
            .scope(ctx.clone(), ToolsListTool::new().execute(args))
            .await
            .unwrap()
    }

    /// Split a search result into its framing line and its JSON.
    fn parts(result: &Value) -> (String, Value) {
        let text = result.as_str().expect("a search answers with text");
        let (head, body) = text.split_once('\n').unwrap_or((text, "null"));
        (head.to_string(), serde_json::from_str(body).unwrap())
    }

    #[tokio::test]
    async fn a_search_that_finds_appends_the_definition_and_says_it_is_callable() {
        let c = ctx(LateToolBinding::Append);
        let (head, body) = parts(&call(&c, json!({"query": "current weather"})).await);
        assert!(
            head.starts_with("Found 1 tool for \"current weather\"."),
            "{head}"
        );
        assert!(head.contains("callable now") && head.contains("No tools_load call"));
        assert_eq!(body[0]["function"]["name"], "get_weather");
        assert_eq!(
            body.as_array().unwrap().len(),
            1,
            "the near-miss is not found"
        );
        assert!(c.active_tools.is_appended(c.conversation_id, "get_weather"));
        assert_eq!(
            c.active_tools.version(c.conversation_id),
            0,
            "the tools array did not change"
        );
        // Found again: callable already, no definition repeated.
        let (head, body) = parts(&call(&c, json!({"query": "current weather"})).await);
        assert!(head.contains("Already callable"), "{head}");
        assert_eq!(body, json!([]));
    }

    #[tokio::test]
    async fn under_rerender_a_search_declares_what_it_finds() {
        let c = ctx(LateToolBinding::Rerender);
        let (head, body) = parts(&call(&c, json!({"query": "current weather"})).await);
        assert!(head.contains("callable now"), "{head}");
        assert_eq!(body, json!(["get_weather"]));
        assert!(c.active_tools.is_active(c.conversation_id, "get_weather"));
    }

    #[tokio::test]
    async fn a_search_that_finds_nothing_says_so_and_names_non_matches() {
        let c = ctx(LateToolBinding::Append);
        let (head, body) = parts(&call(&c, json!({"query": "current forecast"})).await);
        assert!(head.starts_with("No tool matched"), "{head}");
        assert!(head.contains("do not use one in its place"));
        let near: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert!(near.contains(&"get_weather"), "{near:?}");
        assert!(body[0]["not_a_match"].as_str().unwrap().contains("missing"));
        assert!(c.active_tools.appended_for(c.conversation_id).is_empty());
        // A forbidden tool is never offered, even as a non-match.
        assert!(!near.contains(&"forbidden_weather"));

        let nothing = call(&c, json!({"query": "teleportation"})).await;
        assert!(nothing
            .as_str()
            .unwrap()
            .contains("none of its words appear"));
    }

    #[tokio::test]
    async fn a_need_that_keeps_missing_is_answered_as_final_and_recorded_as_a_gap() {
        let c = ctx(LateToolBinding::Append);
        let text = |v: Value| v.as_str().unwrap().to_string();

        let first = text(call(&c, json!({"query": "current forecast"})).await);
        assert!(first.starts_with("No tool matched"), "{first}");
        assert!(first.contains("get_forecast"), "{first}");
        // Broader, the same need: the tool it named a non-match stays one,
        // rather than being found on one word of two.
        let second = text(call(&c, json!({"query": "forecast"})).await);
        assert!(
            second.starts_with("No tool matched \"forecast\""),
            "{second}"
        );
        assert!(
            second.contains("already named a non-match for the same need ('current forecast')"),
            "{second}"
        );
        assert!(c.active_tools.appended_for(c.conversation_id).is_empty());
        assert!(c.active_tools.tool_gaps(c.conversation_id).is_empty());

        // The limit's worth of misses: the next is final.
        let third = text(call(&c, json!({"query": "the current forecast, please"})).await);
        assert!(
            third.starts_with("No tool provides \"current forecast\"."),
            "{third}"
        );
        assert!(third.contains("searched for it 3 times"), "{third}");
        assert!(third.contains("Tell the user plainly"), "{third}");
        assert!(
            !third.contains("get_forecast"),
            "no near-miss is offered: {third}"
        );
        assert_eq!(
            c.active_tools.tool_gaps(c.conversation_id),
            ["current forecast"]
        );

        // Another need is counted on its own, and a find is still a find.
        let other = text(call(&c, json!({"query": "teleportation"})).await);
        assert!(other.contains("none of its words appear"), "{other}");
        let found = text(call(&c, json!({"query": "current weather"})).await);
        assert!(found.starts_with("Found 1 tool"), "{found}");
    }

    #[tokio::test]
    async fn the_limit_is_the_runs_and_a_worker_is_told_to_report_the_gap() {
        let c = ctx(LateToolBinding::Append);
        c.active_tools.begin_tool_search(c.conversation_id, 1);
        let binding = crate::work_backend::WorkRunContext {
            item: "item-7".into(),
            actor: "worker:pinch".into(),
        };
        let run = async {
            let first = call(&c, json!({"query": "teleportation"})).await;
            let second = call(&c, json!({"query": "teleport"})).await;
            (first, second)
        };
        let (first, second) = crate::work_backend::WORK_RUN_CONTEXT
            .scope(binding, run)
            .await;
        assert!(first.as_str().unwrap().starts_with("No tool matched"));
        let second = second.as_str().unwrap();
        assert!(
            second.starts_with("No tool provides \"teleportation\"."),
            "{second}"
        );
        assert!(
            second.contains("blocked.reason to \"needs_tool\"")
                && second.contains("[\"teleportation\"]"),
            "{second}"
        );
        assert_eq!(
            c.active_tools.tool_gaps(c.conversation_id),
            ["teleportation"]
        );

        // A new run starts clean.
        c.active_tools.begin_tool_search(c.conversation_id, 2);
        let again = call(&c, json!({"query": "teleportation"})).await;
        assert!(again.as_str().unwrap().starts_with("No tool matched"));
        assert!(c.active_tools.tool_gaps(c.conversation_id).is_empty());
    }

    #[tokio::test]
    async fn the_listing_groups_mcp_tools_by_server_and_loads_nothing() {
        let c = ctx(LateToolBinding::Append);
        let listing = call(&c, json!({})).await;
        let categories = listing["categories"].as_array().unwrap();
        let mcp: Vec<(&str, &str)> = categories
            .iter()
            .filter(|g| g["category"] == "mcp")
            .map(|g| {
                (
                    g["server"].as_str().unwrap(),
                    g["tools"][0]["name"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            mcp,
            [
                ("jira", "mcp__jira__search"),
                ("linear", "mcp__linear__create_issue")
            ]
        );
        assert!(
            !listing.to_string().contains("tools_list\""),
            "no meta tools"
        );
        assert!(!listing.to_string().contains("forbidden_weather"));
        assert!(c.active_tools.appended_for(c.conversation_id).is_empty());

        let linear = call(&c, json!({"category": "mcp:linear"})).await;
        assert_eq!(linear["categories"].as_array().unwrap().len(), 1);
        let files = call(&c, json!({"category": "filesystem"})).await;
        assert_eq!(files["categories"][0]["tools"][0]["name"], "read");
    }
}
