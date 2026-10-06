//! Agent definitions for this daemon: the built-in definition files and
//! `<data dir>/agents/<name>.md`, which replace a built-in of the same name
//! (plan `docs/plans/control-layer-and-worker-fleet.md`, section 12). One
//! registry feeds both consumers: the `subagents` tool, and the control
//! layer's local worker, whose definition is the `worker` file.

use std::path::Path;

use rustykrab_core::{AgentDefinition, AgentRegistry};

/// The definition the control layer's local workers run.
const WORKER: &str = "worker";
const WORK_PLANNER: &str = "work-planner";

/// Every definition: the built-ins, then the data dir's files.
pub fn load(data_dir: &Path) -> AgentRegistry {
    rustykrab_skills::agent_registry(Some(&data_dir.join(rustykrab_skills::AGENTS_DIR)))
}

/// The definitions the `subagents` tool offers: all but the worker's, which
/// is the controller's and ends only on a `result_report` a sub-agent run
/// does not hold.
pub fn subagents(all: &AgentRegistry) -> AgentRegistry {
    let mut out = AgentRegistry::new();
    for def in all
        .list()
        .into_iter()
        .filter(|d| d.id != WORKER && d.id != WORK_PLANNER)
    {
        out.insert((*def).clone());
    }
    out
}

/// The local worker `name`'s definition: the `worker` definition (a data-dir
/// file when there is one), under the worker's name.
pub fn worker_definition(all: &AgentRegistry, name: &str) -> AgentDefinition {
    match all.get(WORKER) {
        Some(def) => rustykrab_agent::LocalWorker::named_definition(&def, name),
        None => rustykrab_agent::LocalWorker::default_definition(name),
    }
}

/// Controller-only planner, kept separate from the read-only planning subagent.
pub fn planner_definition(all: &AgentRegistry) -> AgentDefinition {
    let mut definition = match all.get(WORK_PLANNER) {
        Some(def) => rustykrab_agent::LocalWorker::named_definition(&def, "planner"),
        None => rustykrab_agent::LocalWorker::planner_definition(),
    };
    // The host assigns this role, including to older data-dir overrides.
    definition.planning_only = true;
    definition
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_dir_worker_file_is_the_local_workers_definition() {
        let dir = std::env::temp_dir().join(format!("rk-cli-agents-{}", std::process::id()));
        let agents = dir.join(rustykrab_skills::AGENTS_DIR);
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("worker.md"),
            "---\ndescription = \"Mine.\"\ntools = [\"caldav\"]\n---\nYou are {name}, mine.",
        )
        .unwrap();

        std::fs::write(
            agents.join("work-planner.md"),
            "---\ndescription = \"Mine.\"\ntools = [\"work_plan\", \"work_status\"]\nallowed_tools = [\"work_plan\", \"work_status\"]\n---\nYou are {name}, my planner.",
        ).unwrap();
        let all = load(&dir);
        let planner = planner_definition(&all);
        assert_eq!(planner.id, "planner");
        assert!(planner.planning_only);
        assert_eq!(planner.system_prompt, "You are planner, my planner.");
        assert_eq!(planner.tools, ["work_plan", "work_status"]);
        assert_eq!(planner.allowed_tools.unwrap(), ["work_plan", "work_status"]);

        let worker = worker_definition(&all, "pinch");
        assert_eq!(worker.id, "pinch");
        assert_eq!(worker.system_prompt, "You are pinch, mine.");
        assert_eq!(worker.tools, ["caldav"]);

        let offered: Vec<String> = subagents(&all)
            .list()
            .iter()
            .map(|d| d.id.clone())
            .collect();
        assert_eq!(offered, ["coder", "planner", "researcher"]);
        let _ = std::fs::remove_dir_all(&dir);

        // Without a data-dir file, the built-in.
        let builtin = worker_definition(&rustykrab_skills::agent_registry(None), "pinch");
        assert!(builtin
            .system_prompt
            .starts_with("You are pinch, a RustyKrab worker."));
    }
}
