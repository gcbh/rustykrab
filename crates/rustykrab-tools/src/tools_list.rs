use std::collections::BTreeMap;

use async_trait::async_trait;
use rustykrab_core::active_tools::{with_session_context, LateToolBinding, SessionToolContext};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::tool_catalog::{self, Category, MAX_APPENDED_PER_SEARCH};

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
    let result = tool_catalog::search(query, &catalog);
    let quoted = query.trim();

    if result.found.is_empty() {
        if result.near.is_empty() {
            return format!(
                "No tool matched \"{quoted}\": none of its words appear in the tool catalog. \
                 Search again with other words, or call tools_list without a query to see the \
                 catalog by category."
            );
        }
        let near: Vec<Value> = result
            .near
            .iter()
            .map(|n| json!({ "name": n.name, "not_a_match": n.why() }))
            .collect();
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
