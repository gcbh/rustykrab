//! The worker fleet as the daemon builds it
//! (`docs/plans/control-layer-and-worker-fleet.md`, sections 5, 8, 10 and
//! 13): the registry of named workers, the factory that builds external
//! ones from their spec, routing by record, and the skills a capability
//! build writes while the daemon runs.
//!
//! `main` opens a [`Fleet`] once the store and the skill registry exist,
//! names the local worker with it, and hands the registry, catalog and
//! routing to the controller and the registry to the gateway. Everything
//! else about workers happens through the registry: `rustykrab worker
//! add` (over `POST /api/workers`) builds an [`ExternalWorker`] or a peer
//! (`peers.rs`) through [`AgentFactory`], and a restart rebuilds every
//! stored one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rustykrab_agent::{ExternalConfig, ExternalWorker, LateTools};
use rustykrab_control::controller::{ControllerConfig, Routing, ToolCatalog};
use rustykrab_control::registry::{WorkerFactory, WorkerRegistry, WorkerSpec};
use rustykrab_control::routing::RecordRouting;
use rustykrab_control::worker::Worker;
use rustykrab_core::Tool;
use rustykrab_skills::SkillRegistry;
use rustykrab_store::Store;
use rustykrab_tools::ToolState;

use crate::RegistryCatalog;

/// Builds `claude_code` and `codex` workers, and peers (`peers.rs`), from
/// their registry spec.
pub(crate) struct AgentFactory {
    data_dir: PathBuf,
}

#[async_trait::async_trait]
impl WorkerFactory for AgentFactory {
    fn build(&self, name: &str, spec: &WorkerSpec) -> Result<Arc<dyn Worker>, String> {
        if spec.kind == rustykrab_core::work::WorkerKind::Peer {
            return crate::peers::build(name, spec);
        }
        let config = ExternalConfig::from_spec(spec, &self.data_dir)?;
        Ok(Arc::new(ExternalWorker::new(name, config)))
    }

    /// A peer's pairing code, redeemed at its node.
    async fn prepare(&self, name: &str, spec: WorkerSpec) -> Result<WorkerSpec, String> {
        crate::peers::pair(name, spec).await
    }
}

/// Skills written while the daemon runs, as tools: a capability build's
/// `SKILL.md` (section 8, rung 2b). Each refresh loads the skill
/// directories not seen before, registers them with the daemon's skill
/// registry and exposes each as a tool named after the skill, unless the
/// name is already a tool's. The local worker takes them as late tools;
/// the controller's catalog knows them, which is how a build is verified.
pub(crate) struct RuntimeSkills {
    dir: PathBuf,
    registry: Arc<SkillRegistry>,
    /// Tool names at boot, which a late skill may not take.
    reserved: HashSet<String>,
    /// Skill directories already loaded, at boot or since.
    seen: RwLock<HashSet<String>>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
}

impl RuntimeSkills {
    fn new(dir: &Path, registry: Arc<SkillRegistry>, boot_tools: &[Arc<dyn Tool>]) -> Self {
        let seen = registry
            .md_skills()
            .iter()
            .map(|s| s.frontmatter.name.clone())
            .collect();
        RuntimeSkills {
            dir: dir.to_path_buf(),
            registry,
            reserved: boot_tools.iter().map(|t| t.name().to_string()).collect(),
            seen: RwLock::new(seen),
            tools: RwLock::new(Vec::new()),
        }
    }

    /// Load skill directories written since the last look. Returns the
    /// names of the tools added.
    pub(crate) fn refresh(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut added = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            let md = dir.join("SKILL.md");
            let Some(key) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if !md.is_file()
                || self
                    .seen
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&key)
            {
                continue;
            }
            match rustykrab_skills::load_single_skill(&dir, &md) {
                Ok(skill) => {
                    let name = skill.frontmatter.name.clone();
                    let skill = Arc::new(skill);
                    self.seen
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key);
                    if self.registry.get_md(&name).is_some() || self.has(&name) {
                        continue;
                    }
                    self.registry.register_md(skill.clone());
                    if self.reserved.contains(&name) {
                        tracing::warn!(skill = %name, "a skill written at run time shares a tool's name; not exposed as a tool");
                        continue;
                    }
                    self.tools
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(Arc::new(rustykrab_tools::SkillTool::new(skill)));
                    tracing::info!(skill = %name, "skill written at run time loaded as a tool");
                    added.push(name);
                }
                Err(e) => {
                    // Not marked seen: a half-written file is read again.
                    tracing::debug!(path = %md.display(), error = %e, "skill not loaded yet");
                }
            }
        }
        added
    }

    fn has(&self, name: &str) -> bool {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|t| t.name() == name)
    }
}

impl LateTools for RuntimeSkills {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.refresh();
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The controller's catalog: the boot registry's, plus the skills written
/// since, which count as registered and not yet loaded.
struct FleetCatalog {
    boot: Arc<RegistryCatalog>,
    skills: Arc<RuntimeSkills>,
}

impl ToolCatalog for FleetCatalog {
    fn tool_state(&self, name: &str) -> ToolState {
        match ToolCatalog::tool_state(self.boot.as_ref(), name) {
            ToolState::Unknown if self.skills.has(name) => ToolState::RegisteredUnloaded,
            state => state,
        }
    }

    fn mcp_server_configured(&self, name: &str) -> bool {
        ToolCatalog::mcp_server_configured(self.boot.as_ref(), name)
    }

    fn refresh(&self) {
        self.skills.refresh();
    }
}

/// The daemon's workers and what the controller needs to use them.
pub(crate) struct Fleet {
    pub registry: Arc<WorkerRegistry>,
    /// Routing by record, its class default tiers seeded and read back.
    routing: Arc<RecordRouting>,
    pub skills: Arc<RuntimeSkills>,
    /// The local worker's registry name, stable across restarts.
    pub local_name: String,
    data_dir: PathBuf,
}

impl Fleet {
    /// Open the registry over `store` and pick the local worker's name.
    pub(crate) async fn open(
        store: &Store,
        data_dir: &Path,
        skills_dir: &Path,
        skill_registry: Arc<SkillRegistry>,
        boot_tools: &[Arc<dyn Tool>],
    ) -> anyhow::Result<Fleet> {
        let registry = Arc::new(WorkerRegistry::new(store.clone()).with_factory(Arc::new(
            AgentFactory {
                data_dir: data_dir.to_path_buf(),
            },
        )));
        registry.load().await?;
        let local_name = registry.local_name().await?;
        let routing = Arc::new(RecordRouting::new(registry.clone()));
        routing.load().await?;
        Ok(Fleet {
            registry,
            routing,
            skills: Arc::new(RuntimeSkills::new(skills_dir, skill_registry, boot_tools)),
            local_name,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// The controller's configuration: `code` runs get worktrees under
    /// `<data dir>/worktrees`.
    pub(crate) fn config(&self, base: ControllerConfig) -> ControllerConfig {
        ControllerConfig {
            worktree_root: Some(self.data_dir.join("worktrees")),
            ..base
        }
    }

    pub(crate) fn catalog(&self, boot: Arc<RegistryCatalog>) -> Arc<dyn ToolCatalog> {
        Arc::new(FleetCatalog {
            boot,
            skills: self.skills.clone(),
        })
    }

    pub(crate) fn routing(&self) -> Arc<dyn Routing> {
        self.routing.clone()
    }

    /// Register the local worker and rebuild the stored external ones.
    pub(crate) async fn start(&self, local: Arc<dyn Worker>) -> anyhow::Result<()> {
        self.registry
            .register(local, serde_json::json!({}), None)
            .await?;
        let restored = self.registry.restore().await?;
        tracing::info!(
            local = %self.local_name,
            restored = ?restored,
            "worker registry ready"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(
            dir.join(name).join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Tide times for a port\n---\nRead the table.\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_skill_written_at_run_time_becomes_a_tool_once() {
        let dir = tempfile::tempdir().unwrap();
        skill(dir.path(), "at_boot");
        let registry = Arc::new(SkillRegistry::new());
        for s in rustykrab_skills::load_skills_from_dir(dir.path()).unwrap() {
            registry.register_md(Arc::new(s));
        }
        let skills = RuntimeSkills::new(dir.path(), registry.clone(), &[]);
        assert!(skills.refresh().is_empty(), "boot skills are not late");

        skill(dir.path(), "tide_table");
        assert_eq!(skills.refresh(), ["tide_table"]);
        assert!(skills.refresh().is_empty(), "loaded once");
        assert!(registry.get_md("tide_table").is_some());
        let names: Vec<String> = LateTools::tools(&skills)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, ["tide_table"]);

        let catalog = FleetCatalog {
            boot: Arc::new(RegistryCatalog::default()),
            skills: Arc::new(skills),
        };
        assert_eq!(
            catalog.tool_state("tide_table"),
            ToolState::RegisteredUnloaded
        );
        assert_eq!(catalog.tool_state("nothing"), ToolState::Unknown);
    }
}
