//! The tool catalog behind `tools_list` and `tools_load`: categories, MCP
//! grouping, the text form of an appended tool, and the host-side
//! plausibility test that decides whether a search found anything.
//!
//! # Why the host validates a search
//!
//! Given a catalog that lacked the tool a task needed, both default local
//! models substituted the nearest one: a one-day forecast for current
//! weather, a list of recent shipments for a tracking number (late binding
//! experiment, 2026-09-24; plan section 12). A search result that calls a
//! near-miss "found" invites exactly that. So a tool is labelled found only
//! when [`plausible`] holds for it; when none does the result says nothing
//! matched and names the nearest tools explicitly as non-matches.
//!
//! # The plausibility test
//!
//! Deterministic and lexical, over words rather than meanings:
//!
//! 1. The query and every candidate's *document* (its name split on `_`,
//!    `-`, `.` and case changes, its description, its parameter names and
//!    string enum values) become sets of [`terms`]: lowercased words minus
//!    [`STOPWORDS`] and pure numbers, each reduced by a small suffix
//!    [`stem`] (`tracking` to `track`, `events` to `event`).
//! 2. Two terms [`match`](terms_match) when equal, or when one is a prefix
//!    of the other and the shorter has at least five letters (`schedul` and
//!    `schedule`).
//! 3. The query's *known* terms are those matching some term of some
//!    candidate. A word no tool mentions (`lisbon`, a tracking number) is an
//!    argument, not a capability, and cannot tell tools apart, so it is
//!    left out rather than counted against every tool.
//! 4. A candidate is plausible when it matches at least two thirds of the
//!    known terms, and at least one. For a two-word query that means both
//!    words: `current weather` finds `get_weather` ("Get the current weather
//!    for a city") and not `get_forecast` ("a multi-day weather forecast"),
//!    which misses `current`. A query that names a tool exactly finds it.

use std::collections::BTreeMap;

use rustykrab_core::mcp_server_of;
use rustykrab_core::types::ToolSchema;
use serde_json::{json, Value};

/// Words that say how a request is phrased, not what capability it needs.
pub(crate) const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "of",
    "for",
    "to",
    "in",
    "on",
    "at",
    "by",
    "with",
    "from",
    "into",
    "about",
    "as",
    "is",
    "are",
    "be",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "my",
    "me",
    "i",
    "you",
    "your",
    "we",
    "our",
    "some",
    "any",
    "can",
    "could",
    "would",
    "should",
    "will",
    "please",
    "need",
    "want",
    "like",
    "tool",
    "tools",
    "function",
    "functions",
    "get",
    "use",
    "using",
    "do",
    "does",
    "able",
    "which",
    "what",
    "where",
    "when",
    "how",
    "who",
    "up",
    "so",
    "look",
    "find",
    "check",
    "show",
    "tell",
    "give",
    "help",
    "let",
    "via",
    "one",
    "all",
    "if",
    "not",
    "no",
    "but",
    "than",
    "then",
    "there",
    "their",
    "them",
    "they",
];

/// Most tools one search appends; the rest of its matches are named so the
/// model can load them by name.
pub(crate) const MAX_APPENDED_PER_SEARCH: usize = 8;

/// Non-matches named when a search finds nothing.
pub(crate) const MAX_NEAR_MISSES: usize = 5;

/// A tool's category for catalog listings. MCP tools are grouped by the
/// server in their name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Category {
    Named(&'static str),
    Mcp(String),
}

impl Category {
    /// Whether a `category` filter selects this category: its label,
    /// `mcp` for every server, or `mcp:<server>`.
    pub(crate) fn selected_by(&self, filter: &str) -> bool {
        let filter = filter.trim().to_ascii_lowercase();
        match self {
            Category::Named(name) => *name == filter,
            Category::Mcp(server) => {
                filter == "mcp" || filter == format!("mcp:{}", server.to_ascii_lowercase())
            }
        }
    }
}

/// Categorize a tool by its name. Keeps the Tool trait minimal while still
/// giving agents a useful axis to filter on.
pub(crate) fn categorize(name: &str) -> Category {
    if let Some(server) = mcp_server_of(name) {
        return Category::Mcp(server.to_string());
    }
    Category::Named(match name {
        "tools_list" | "tools_load" => "meta",
        "read" | "write" | "edit" | "apply_patch" => "filesystem",
        "exec" | "process" | "code_execution" => "runtime",
        "web_fetch" | "web_search" | "x_search" | "http_request" | "http_session" | "browser" => {
            "web"
        }
        "image" | "video" | "canvas" => "media",
        "cron" | "gateway" => "automation",
        "gmail" | "caldav" | "notion" | "obsidian" => "integration",
        "skills" => "skills",
        "message" => "messaging",
        "nodes" => "devices",
        _ if name.starts_with("memory_") => "memory",
        _ if name.starts_with("sessions_")
            || name.starts_with("session_")
            || name.starts_with("agents_")
            || name == "subagents" =>
        {
            "session"
        }
        _ if name.starts_with("credential_") => "credentials",
        _ => "general",
    })
}

/// A tool as it is appended to a conversation: the same function object a
/// declared tool has in the tools array, so a model reads both alike.
pub(crate) fn definition(schema: &ToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": schema.name,
            "description": schema.description,
            "parameters": schema.parameters,
        }
    })
}

/// The first sentence of a description, for a catalog listing.
pub(crate) fn summary(description: &str) -> String {
    const MAX: usize = 160;
    let first = description
        .split_inclusive(". ")
        .next()
        .unwrap_or(description)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first.chars().count() <= MAX {
        return first;
    }
    let mut cut: String = first.chars().take(MAX - 3).collect();
    cut.push_str("...");
    cut
}

/// Reduce a lowercased word to a crude stem: enough to meet `tracking` with
/// `track` and `events` with `event`, not a linguistic claim.
pub(crate) fn stem(word: &str) -> String {
    let n = word.len();
    let strip = |k: usize| word[..n - k].to_string();
    if n > 5 && word.ends_with("ing") {
        return strip(3);
    }
    if n > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..n - 3]);
    }
    if n > 4 && word.ends_with("ed") {
        return strip(2);
    }
    if n > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| word.ends_with(s))
    {
        return strip(2);
    }
    if n > 3 && word.ends_with('s') && !["ss", "us", "is"].iter().any(|s| word.ends_with(s)) {
        return strip(1);
    }
    word.to_string()
}

/// Split text into stemmed terms, keeping each term's first spelling (for
/// telling the model which of its words a tool missed). Splits on anything
/// that is not a letter or digit, and on a lower-to-upper case change, so
/// `create_calendar_event` and `createCalendarEvent` read alike.
pub(crate) fn terms(text: &str) -> BTreeMap<String, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            current.extend(c.to_lowercase());
        } else {
            prev_lower = false;
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    let mut out = BTreeMap::new();
    for word in words {
        if STOPWORDS.contains(&word.as_str()) || word.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        out.entry(stem(&word)).or_insert(word);
    }
    out
}

/// Whether two stemmed terms name the same thing.
pub(crate) fn terms_match(a: &str, b: &str) -> bool {
    a == b || (a.len().min(b.len()) >= 5 && (a.starts_with(b) || b.starts_with(a)))
}

/// The plausibility rule: at least two thirds of the query's known terms,
/// and at least one.
pub(crate) fn plausible(matched: usize, known: usize) -> bool {
    known > 0 && matched > 0 && 3 * matched >= 2 * known
}

/// The words a tool is searched by: its name, description, parameter names
/// and string enum values.
fn document(schema: &ToolSchema) -> Vec<String> {
    let mut text = format!("{} {}", schema.name, schema.description);
    if let Some(props) = schema
        .parameters
        .get("properties")
        .and_then(Value::as_object)
    {
        for (name, prop) in props {
            text.push(' ');
            text.push_str(name);
            for value in prop
                .get("enum")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                text.push(' ');
                text.push_str(value);
            }
        }
    }
    terms(&text).into_keys().collect()
}

/// A tool a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub name: String,
    pub matched: usize,
}

/// A tool that matched some of a search's words and not enough of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NearMiss {
    pub name: String,
    pub matched: usize,
    pub of: usize,
    /// The query's words (as the caller wrote them) this tool lacks.
    pub missing: Vec<String>,
}

impl NearMiss {
    pub(crate) fn why(&self) -> String {
        format!(
            "matches {} of {} search terms; missing: {}",
            self.matched,
            self.of,
            self.missing.join(", ")
        )
    }
}

/// What a search found, and what came close.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Search {
    /// The query's terms some candidate knows, as the caller wrote them.
    pub known: Vec<String>,
    /// Plausible tools: most matched terms first, then catalog order.
    pub found: Vec<Found>,
    /// When nothing was found: the closest tools, labelled non-matches.
    pub near: Vec<NearMiss>,
}

/// Search `candidates` for `query`. See the module docs for the rule.
pub(crate) fn search(query: &str, candidates: &[ToolSchema]) -> Search {
    let query_terms = terms(query);
    let docs: Vec<Vec<String>> = candidates.iter().map(document).collect();
    let known: Vec<(&String, &String)> = query_terms
        .iter()
        .filter(|(term, _)| docs.iter().flatten().any(|d| terms_match(term, d)))
        .collect();
    let exact = query.trim().to_ascii_lowercase();

    let mut scored: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for (index, doc) in docs.iter().enumerate() {
        let mut matched = 0;
        let mut missing = Vec::new();
        for (term, spelled) in &known {
            if doc.iter().any(|d| terms_match(term, d)) {
                matched += 1;
            } else {
                missing.push((*spelled).clone());
            }
        }
        scored.push((index, matched, missing));
    }

    let mut found: Vec<Found> = scored
        .iter()
        .filter(|(index, matched, _)| {
            candidates[*index].name.eq_ignore_ascii_case(&exact) || plausible(*matched, known.len())
        })
        .map(|(index, matched, _)| Found {
            name: candidates[*index].name.clone(),
            matched: *matched,
        })
        .collect();
    // Stable: equal scores keep catalog order.
    found.sort_by_key(|f| std::cmp::Reverse(f.matched));

    let near = if found.is_empty() {
        let mut close: Vec<&(usize, usize, Vec<String>)> = scored
            .iter()
            .filter(|(_, matched, _)| *matched > 0)
            .collect();
        close.sort_by_key(|c| std::cmp::Reverse(c.1));
        close
            .into_iter()
            .take(MAX_NEAR_MISSES)
            .map(|(index, matched, missing)| NearMiss {
                name: candidates[*index].name.clone(),
                matched: *matched,
                of: known.len(),
                missing: missing.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };

    Search {
        known: known
            .iter()
            .map(|(_, spelled)| (*spelled).clone())
            .collect(),
        found,
        near,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, params: &[&str]) -> ToolSchema {
        let props: serde_json::Map<String, Value> = params
            .iter()
            .map(|p| ((*p).to_string(), json!({"type": "string"})))
            .collect();
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters: json!({"type": "object", "properties": props}),
        }
    }

    /// The late binding experiment's weather catalog: the target and the
    /// near-misses it was buried among.
    fn weather(with_target: bool) -> Vec<ToolSchema> {
        let mut c = vec![
            tool(
                "get_forecast",
                "Get a multi-day weather forecast for a city.",
                &["city", "days"],
            ),
            tool(
                "get_weather_history",
                "Get recorded weather for a city on a past date.",
                &["city", "date"],
            ),
            tool(
                "get_air_quality",
                "Get the current air quality index for a city.",
                &["city"],
            ),
            tool(
                "get_sunrise_sunset",
                "Get today's sunrise and sunset times for a city.",
                &["city"],
            ),
            tool(
                "convert_temperature",
                "Convert a temperature between celsius and fahrenheit.",
                &["value", "to_unit"],
            ),
            tool(
                "get_timezone",
                "Get the IANA timezone and current local time for a city.",
                &["city"],
            ),
            tool(
                "get_pollen_index",
                "Get today's pollen index for a city.",
                &["city"],
            ),
            tool(
                "get_marine_conditions",
                "Get sea state and swell for a coastal location.",
                &["location"],
            ),
            tool(
                "get_uv_index",
                "Get the current UV index for a city.",
                &["city"],
            ),
        ];
        if with_target {
            let mut target = tool(
                "get_weather",
                "Get the current weather for a city.",
                &["city", "unit"],
            );
            target.parameters["properties"]["unit"]["enum"] = json!(["celsius", "fahrenheit"]);
            c.insert(5, target);
        }
        c
    }

    fn calendar() -> Vec<ToolSchema> {
        vec![
            tool(
                "list_calendar_events",
                "List events on the user's calendar in a date range.",
                &["start", "end"],
            ),
            tool(
                "create_reminder",
                "Create a reminder notification at a given time.",
                &["text", "at"],
            ),
            tool(
                "create_calendar_event",
                "Create an event on the user's calendar.",
                &["title", "start", "duration_minutes"],
            ),
            tool(
                "update_calendar_event",
                "Change the time or title of an existing calendar event.",
                &["event_id", "title", "start"],
            ),
            tool(
                "delete_calendar_event",
                "Delete a calendar event by id.",
                &["event_id"],
            ),
            tool(
                "find_free_time",
                "Find free slots on the user's calendar on a date.",
                &["date", "duration_minutes"],
            ),
        ]
    }

    fn packages(with_target: bool) -> Vec<ToolSchema> {
        let mut c = vec![
            tool(
                "lookup_order",
                "Look up an online order by order number.",
                &["order_number"],
            ),
            tool(
                "estimate_delivery_date",
                "Estimate the delivery date for a shipment between two postcodes.",
                &["origin", "destination"],
            ),
            tool(
                "list_recent_shipments",
                "List the user's recent inbound and outbound shipments.",
                &["days"],
            ),
            tool(
                "create_shipment",
                "Create a new outbound shipment and get a label.",
                &["destination", "weight_kg"],
            ),
            tool(
                "cancel_shipment",
                "Cancel a shipment that has not yet been picked up.",
                &["shipment_id"],
            ),
        ];
        if with_target {
            c.insert(
                2,
                tool(
                    "track_package",
                    "Look up the location and status of a shipment by tracking number.",
                    &["tracking_number"],
                ),
            );
        }
        c
    }

    fn names(found: &[Found]) -> Vec<&str> {
        found.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn the_rule_is_two_thirds_of_the_known_terms() {
        assert!(!plausible(0, 0));
        assert!(!plausible(0, 1));
        assert!(plausible(1, 1));
        assert!(!plausible(1, 2));
        assert!(plausible(2, 2));
        assert!(plausible(2, 3));
        assert!(!plausible(2, 4));
        assert!(plausible(3, 4));
        assert!(!plausible(3, 5));
        assert!(plausible(4, 5));
    }

    #[test]
    fn terms_are_stemmed_words_without_filler() {
        let t = terms("Please find the createCalendarEvent tool for tracking 3 events, x_y");
        let keys: Vec<&str> = t.keys().map(String::as_str).collect();
        assert_eq!(keys, ["calendar", "create", "event", "track", "x", "y"]);
        assert_eq!(t["track"], "tracking");
        assert_eq!(stem("shipments"), "shipment");
        assert_eq!(stem("queries"), "query");
        assert_eq!(stem("boxes"), "box");
        assert_eq!(stem("status"), "status");
        assert_eq!(stem("address"), "address");
        assert!(terms_match("schedul", "schedule"));
        assert!(
            !terms_match("cur", "current"),
            "short prefixes are not matches"
        );
        assert!(!terms_match("current", "currency"));
    }

    #[test]
    fn current_weather_finds_the_tool_and_not_the_forecast() {
        let s = search("current weather", &weather(true));
        assert_eq!(names(&s.found), ["get_weather"]);
        assert!(s.near.is_empty());
        // Words no tool knows (a city) are arguments, not capabilities.
        let s = search("current weather in Lisbon, in celsius", &weather(true));
        assert_eq!(names(&s.found), ["get_weather"]);
        assert_eq!(s.known, ["celsius", "current", "weather"]);
        // A one-word query finds every tool that has the word.
        let s = search("weather", &weather(true));
        assert_eq!(
            names(&s.found),
            ["get_forecast", "get_weather_history", "get_weather"]
        );
    }

    #[test]
    fn a_catalog_without_the_target_finds_nothing_and_names_non_matches() {
        let s = search("current weather", &weather(false));
        assert!(s.found.is_empty(), "{:?}", s.found);
        let near: Vec<&str> = s.near.iter().map(|n| n.name.as_str()).collect();
        assert!(near.contains(&"get_forecast"), "{near:?}");
        let forecast = s.near.iter().find(|n| n.name == "get_forecast").unwrap();
        assert_eq!(
            forecast.why(),
            "matches 1 of 2 search terms; missing: current"
        );
        assert!(s.near.len() <= MAX_NEAR_MISSES);

        let s = search("package tracking", &packages(false));
        assert!(s.found.is_empty(), "{:?}", s.found);
        assert!(!s.near.iter().any(|n| n.name == "list_recent_shipments"));
    }

    #[test]
    fn distractors_that_fit_are_found_with_the_target() {
        let s = search("calendar event", &calendar());
        let found = names(&s.found);
        assert!(found.contains(&"create_calendar_event"), "{found:?}");
        assert!(!found.contains(&"create_reminder"));
        assert!(!found.contains(&"find_free_time"));
        let s = search("create a calendar event", &calendar());
        assert_eq!(s.found[0].name, "create_calendar_event", "most terms first");

        let s = search("package tracking", &packages(true));
        assert_eq!(names(&s.found), ["track_package"]);
        let s = search("track a shipment", &packages(true));
        assert_eq!(names(&s.found)[0], "track_package");
    }

    #[test]
    fn a_query_naming_a_tool_finds_it_and_nonsense_finds_nothing() {
        let s = search("get_uv_index", &weather(true));
        assert!(names(&s.found).contains(&"get_uv_index"));
        let s = search("teleport me", &weather(true));
        assert!(s.found.is_empty() && s.near.is_empty() && s.known.is_empty());
    }

    #[test]
    fn mcp_tools_are_categorized_by_server() {
        assert_eq!(
            categorize("mcp__linear__create_issue"),
            Category::Mcp("linear".into())
        );
        assert_eq!(categorize("read"), Category::Named("filesystem"));
        assert_eq!(categorize("memory_get"), Category::Named("memory"));
        let linear = categorize("mcp__linear__search");
        assert!(linear.selected_by("mcp") && linear.selected_by("MCP:Linear"));
        assert!(!linear.selected_by("mcp:jira") && !linear.selected_by("web"));
        // Server names are search terms too.
        let s = search(
            "linear issue",
            &[tool(
                "mcp__linear__create_issue",
                "Create an issue.",
                &["title"],
            )],
        );
        assert_eq!(s.found.len(), 1);
    }

    #[test]
    fn a_definition_is_the_tools_array_shape_and_a_summary_is_one_sentence() {
        let t = tool(
            "get_weather",
            "Get the weather. Returns celsius.",
            &["city"],
        );
        let d = definition(&t);
        assert_eq!(d["type"], "function");
        assert_eq!(d["function"]["name"], "get_weather");
        assert_eq!(
            d["function"]["parameters"]["properties"]["city"]["type"],
            "string"
        );
        assert_eq!(summary(&t.description), "Get the weather.");
        assert!(summary(&"word ".repeat(100)).ends_with("..."));
    }
}
