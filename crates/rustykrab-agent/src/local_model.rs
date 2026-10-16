//! One configuration per local model, read from `<data>/models.toml`.
//!
//! A local model's window used to be set in five places that had to agree:
//! `RUSTYKRAB_NUM_CTX` (the `num_ctx` sent to Ollama),
//! `RUSTYKRAB_MAX_CONTEXT_TOKENS` (the compaction budget, 32K by default
//! for Ollama whatever the window), `RUSTYKRAB_COMPACTION_CONTEXT_CEILING`,
//! the local worker's `context_tokens` in `local-resources.json`, and the
//! harness profile's `max_context_tokens`. They drifted apart: the live
//! daemon ran a 32K window under a 96K budget, and the local worker could
//! not be given a compaction policy at all.
//!
//! Now a model's settings are named once, by its tag, and every use of
//! that model (the chat agent, the memory distiller, the local workers,
//! sub-agents, the evaluation harness) resolves them through
//! [`LocalModels::for_model`]:
//!
//! ```toml
//! # Applies to every local model without an entry of its own.
//! [default]
//! context_window = 131072
//!
//! # Names only what differs from [default].
//! [models."qwen3.8:27b-mlx"]
//! context_window = 131072
//! compaction_strategy = "structured-message-tail"
//! ```
//!
//! `context_window` is the one number that sizes everything: the `num_ctx`
//! pinned on every request, the compaction budget, and its ceiling. A
//! missing file is the built-in defaults; a malformed one stops the daemon
//! rather than silently running a different window.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rustykrab_core::active_tools::LateToolBinding;
use serde::{Deserialize, Serialize};

use crate::compaction::CompactionStrategy;
use crate::harness::HarnessProfile;

/// The file, in the daemon's data directory.
pub const MODELS_FILE: &str = "models.toml";

/// The window when nothing names one: the size the Ollama provider has
/// pinned by default, and the size the owner asked every local model to
/// run at (2026-10-15).
pub const DEFAULT_CONTEXT_WINDOW: u32 = 131_072;

/// Smallest and largest windows accepted. Below the floor the fixed
/// reserves (a turn's output, the tool block, template framing) leave no
/// room for history; the ceiling is the largest window the local models in
/// use support.
pub const MIN_CONTEXT_WINDOW: u32 = 4_096;
pub const MAX_CONTEXT_WINDOW: u32 = 262_144;

/// Environment settings this file replaces. They are no longer read; the
/// daemon names any that are still set at startup.
pub const RETIRED_SETTINGS: [&str; 5] = [
    "RUSTYKRAB_NUM_CTX",
    "OLLAMA_NUM_CTX",
    "RUSTYKRAB_COMPACTION_CONTEXT_CEILING",
    "RUSTYKRAB_COMPACTION_EXPAND_CTX",
    "OLLAMA_KEEP_ALIVE",
];

/// Everything that is a property of one local model rather than of the
/// harness that runs it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalModel {
    /// The window, in tokens: the `num_ctx` on every request, the compaction
    /// budget, and its ceiling.
    pub context_window: u32,
    /// A turn's output allowance (`num_predict`).
    pub output_tokens: u32,
    /// How long the server keeps the model resident after a request, in
    /// Ollama's duration syntax. `None` takes the server's own default.
    pub keep_alive: Option<String>,
    /// Sampling. `None` keeps each caller's own default: the chat agent
    /// samples at 0.1, the local workers at 0.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Thinking. `None` keeps each caller's default: decided per model for
    /// chat, off for the local workers and the distiller.
    pub think: Option<bool>,
    /// How compaction rewrites history for this model.
    pub compaction_strategy: CompactionStrategy,
    /// Fraction of the usable window at which compaction fires; zero never
    /// compacts.
    pub compaction_threshold: f64,
    /// Serve compaction's summary calls at this larger window, where the
    /// provider resizes per request. Unset keeps `context_window`.
    pub compaction_summary_window: Option<u32>,
    /// How a tool found mid-run reaches this model. Unset takes the
    /// provider's capability data.
    pub late_tool_binding: Option<LateToolBinding>,
}

impl Default for LocalModel {
    fn default() -> Self {
        Self {
            context_window: DEFAULT_CONTEXT_WINDOW,
            output_tokens: 4_096,
            keep_alive: Some("30m".into()),
            temperature: None,
            top_p: None,
            think: None,
            compaction_strategy: CompactionStrategy::default(),
            compaction_threshold: 0.85,
            compaction_summary_window: None,
            late_tool_binding: None,
        }
    }
}

/// One table of the file: every field optional, so an entry names only
/// what differs from `[default]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    context_window: Option<u32>,
    output_tokens: Option<u32>,
    /// `"server"` takes the server's default.
    keep_alive: Option<String>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    think: Option<bool>,
    compaction_strategy: Option<CompactionStrategy>,
    compaction_threshold: Option<f64>,
    compaction_summary_window: Option<u32>,
    late_tool_binding: Option<LateToolBinding>,
}

impl Entry {
    fn over(&self, base: &LocalModel) -> LocalModel {
        LocalModel {
            context_window: self.context_window.unwrap_or(base.context_window),
            output_tokens: self.output_tokens.unwrap_or(base.output_tokens),
            keep_alive: match self.keep_alive.as_deref().map(str::trim) {
                None => base.keep_alive.clone(),
                Some(v) if v.is_empty() || v.eq_ignore_ascii_case("server") => None,
                Some(v) => Some(v.to_string()),
            },
            temperature: self.temperature.or(base.temperature),
            top_p: self.top_p.or(base.top_p),
            think: self.think.or(base.think),
            compaction_strategy: self.compaction_strategy.unwrap_or(base.compaction_strategy),
            compaction_threshold: self
                .compaction_threshold
                .unwrap_or(base.compaction_threshold),
            compaction_summary_window: self
                .compaction_summary_window
                .or(base.compaction_summary_window),
            late_tool_binding: self.late_tool_binding.or(base.late_tool_binding),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    default: Entry,
    #[serde(default)]
    models: BTreeMap<String, Entry>,
}

/// The configured local models: the `[default]` and each named model,
/// already resolved against it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalModels {
    default: LocalModel,
    models: BTreeMap<String, LocalModel>,
    source: Option<PathBuf>,
}

impl LocalModels {
    /// Read `<data_dir>/models.toml`. No file is the built-in defaults; an
    /// unreadable, malformed or out-of-range one is an error that names
    /// the file.
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join(MODELS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut models =
                    Self::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                models.source = Some(path);
                Ok(models)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Parse and validate a `models.toml` body.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|e| e.to_string())?;
        let default = file.default.over(&LocalModel::default());
        validate("[default]", &default)?;
        let mut models = BTreeMap::new();
        for (tag, entry) in &file.models {
            if tag.trim().is_empty() {
                return Err("a [models] entry has an empty model tag".into());
            }
            let model = entry.over(&default);
            validate(&format!("[models.\"{tag}\"]"), &model)?;
            models.insert(tag.clone(), model);
        }
        Ok(Self {
            default,
            models,
            source: None,
        })
    }

    /// The settings for `tag`: its own entry, else `[default]`.
    pub fn for_model(&self, tag: &str) -> LocalModel {
        self.models
            .get(tag)
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }

    /// Whether `tag` has an entry of its own.
    pub fn names(&self, tag: &str) -> bool {
        self.models.contains_key(tag)
    }

    /// `[default]` and every named model, for display.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &LocalModel)> {
        std::iter::once(("[default]", &self.default))
            .chain(self.models.iter().map(|(k, v)| (k.as_str(), v)))
    }

    /// The file these came from; `None` for built-in defaults.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// A `models.toml` body naming `window` for every model, for callers
    /// (the evaluation harness) that set up a data directory. A window too
    /// small for the default output allowance gets a quarter of itself, so
    /// the body loads for every window the daemon accepts.
    pub fn toml_with_window(window: u32) -> String {
        let default_output = LocalModel::default().output_tokens;
        if window / 4 < default_output {
            format!(
                "[default]\ncontext_window = {window}\noutput_tokens = {}\n",
                (window / 4).max(1)
            )
        } else {
            format!("[default]\ncontext_window = {window}\n")
        }
    }
}

fn validate(table: &str, m: &LocalModel) -> Result<(), String> {
    if !(MIN_CONTEXT_WINDOW..=MAX_CONTEXT_WINDOW).contains(&m.context_window) {
        return Err(format!(
            "{table}: context_window {} is outside {MIN_CONTEXT_WINDOW}..={MAX_CONTEXT_WINDOW}",
            m.context_window
        ));
    }
    if m.output_tokens == 0 || m.output_tokens >= m.context_window {
        return Err(format!(
            "{table}: output_tokens {} must be positive and below context_window {}",
            m.output_tokens, m.context_window
        ));
    }
    if !(0.0..=1.0).contains(&m.compaction_threshold) {
        return Err(format!(
            "{table}: compaction_threshold {} must be in 0..=1 (0 never compacts)",
            m.compaction_threshold
        ));
    }
    if let Some(w) = m.compaction_summary_window {
        if !(m.context_window..=MAX_CONTEXT_WINDOW).contains(&w) {
            return Err(format!(
                "{table}: compaction_summary_window {w} must be between context_window {} and {MAX_CONTEXT_WINDOW}",
                m.context_window
            ));
        }
    }
    for (name, v) in [("temperature", m.temperature), ("top_p", m.top_p)] {
        if v.is_some_and(|v| !(0.0..=2.0).contains(&v)) {
            return Err(format!("{table}: {name} must be between 0 and 2"));
        }
    }
    Ok(())
}

impl LocalModel {
    /// Size a harness profile to this model: the compaction budget and its
    /// ceiling are the window, and the model's compaction policy and late
    /// tool binding replace the profile's. Loop settings (iterations,
    /// retries, the result-report reminder) are the harness's and stay.
    pub fn apply_to(&self, profile: &mut HarnessProfile) {
        let window = self.context_window as usize;
        profile.max_context_tokens = window;
        profile.compaction_context_ceiling = window;
        profile.compaction_threshold_pct = self.compaction_threshold;
        profile.compaction_strategy = self.compaction_strategy;
        profile.compaction_expand_ctx = self.compaction_summary_window;
        if let Some(binding) = self.late_tool_binding {
            profile.late_tool_binding = Some(binding);
        }
    }

    /// The Ollama request settings for this model, starting from a
    /// caller's own sampling and thinking defaults (`role`).
    pub fn ollama_config(
        &self,
        role: rustykrab_providers::OllamaConfig,
    ) -> rustykrab_providers::OllamaConfig {
        rustykrab_providers::OllamaConfig {
            num_ctx: Some(self.context_window),
            num_predict: i32::try_from(self.output_tokens).unwrap_or(i32::MAX),
            keep_alive: self.keep_alive.clone(),
            temperature: self.temperature.unwrap_or(role.temperature),
            top_p: self.top_p.unwrap_or(role.top_p),
            think: self.think.or(role.think),
        }
    }
}

/// The daemon's configured local models, installed once at startup so
/// every harness built for a local provider (the chat agent, the local
/// workers, sub-agents) resolves the same settings without each caller
/// threading them through. Nothing installed is the built-in defaults.
static INSTALLED: std::sync::RwLock<Option<std::sync::Arc<LocalModels>>> =
    std::sync::RwLock::new(None);

/// Make `models` the configuration every later lookup sees.
pub fn install(models: LocalModels) {
    *INSTALLED.write().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(models));
}

/// The installed configuration, or the built-in defaults.
pub fn installed() -> std::sync::Arc<LocalModels> {
    INSTALLED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The settings for the local model `provider` runs; `None` for a hosted
/// provider, whose harness keeps its own context budget.
pub fn for_provider(provider: &dyn rustykrab_core::model::ModelProvider) -> Option<LocalModel> {
    provider
        .local_model_tag()
        .map(|tag| installed().for_model(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_file_is_the_built_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let models = LocalModels::load(dir.path()).unwrap();
        assert_eq!(models.source(), None);
        let m = models.for_model("anything:1b");
        assert_eq!(m, LocalModel::default());
        assert_eq!(m.context_window, 131_072);
    }

    #[test]
    fn a_model_entry_names_only_what_differs_from_default() {
        let models = LocalModels::from_toml(
            "[default]\ncontext_window = 65536\noutput_tokens = 2048\n\
             [models.\"qwen3.8:27b-mlx\"]\ncontext_window = 131072\n\
             compaction_strategy = \"structured-message-tail\"\n",
        )
        .unwrap();
        let qwen = models.for_model("qwen3.8:27b-mlx");
        assert_eq!(qwen.context_window, 131_072);
        assert_eq!(qwen.output_tokens, 2_048, "inherited from [default]");
        assert_eq!(
            qwen.compaction_strategy,
            CompactionStrategy::StructuredMessageTail
        );
        let other = models.for_model("gemma4:26b");
        assert_eq!(other.context_window, 65_536);
        assert_eq!(other.compaction_strategy, CompactionStrategy::default());
        assert!(models.names("qwen3.8:27b-mlx") && !models.names("gemma4:26b"));
    }

    #[test]
    fn the_window_sizes_the_budget_the_ceiling_and_num_ctx() {
        let m = LocalModels::from_toml("[default]\ncontext_window = 98304\n")
            .unwrap()
            .for_model("m");
        let mut profile = HarnessProfile {
            max_context_tokens: 32_000,
            compaction_context_ceiling: 16_000,
            ..HarnessProfile::default()
        };
        m.apply_to(&mut profile);
        assert_eq!(profile.max_context_tokens, 98_304);
        assert_eq!(profile.compaction_context_ceiling, 98_304);
        let ollama = m.ollama_config(rustykrab_providers::OllamaConfig::default());
        assert_eq!(ollama.num_ctx, Some(98_304));
        assert_eq!(ollama.num_predict, 4_096);
    }

    #[test]
    fn a_callers_sampling_stays_unless_the_model_names_its_own() {
        let role = rustykrab_providers::OllamaConfig {
            temperature: 0.0,
            think: Some(false),
            ..rustykrab_providers::OllamaConfig::default()
        };
        let plain = LocalModel::default().ollama_config(role.clone());
        assert_eq!((plain.temperature, plain.think), (0.0, Some(false)));
        let tuned = LocalModels::from_toml("[default]\ntemperature = 0.6\nthink = true\n")
            .unwrap()
            .for_model("m")
            .ollama_config(role);
        assert_eq!((tuned.temperature, tuned.think), (0.6, Some(true)));
    }

    #[test]
    fn keep_alive_server_takes_the_servers_default() {
        let m = LocalModels::from_toml("[default]\nkeep_alive = \"server\"\n")
            .unwrap()
            .for_model("m");
        assert_eq!(m.keep_alive, None);
    }

    #[test]
    fn out_of_range_or_unknown_settings_are_refused_with_their_table() {
        for (body, needle) in [
            (
                "[default]\ncontext_window = 1024\n",
                "[default]: context_window",
            ),
            (
                "[models.\"m\"]\ncontext_window = 300000\n",
                "[models.\"m\"]: context_window",
            ),
            (
                "[default]\ncontext_window = 8192\noutput_tokens = 8192\n",
                "output_tokens",
            ),
            (
                "[default]\ncompaction_threshold = 1.5\n",
                "compaction_threshold",
            ),
            ("[default]\nnum_ctx = 65536\n", "unknown field"),
            (
                "[default]\ncompaction_strategy = \"bogus\"\n",
                "unknown variant",
            ),
        ] {
            let err = LocalModels::from_toml(body).unwrap_err();
            assert!(err.contains(needle), "{body:?} -> {err}");
        }
    }

    #[test]
    fn a_generated_body_loads_for_every_window_the_daemon_accepts() {
        for window in [
            MIN_CONTEXT_WINDOW,
            8_192,
            16_384,
            32_768,
            131_072,
            MAX_CONTEXT_WINDOW,
        ] {
            let models = LocalModels::from_toml(&LocalModels::toml_with_window(window))
                .unwrap_or_else(|e| panic!("window {window}: {e}"));
            let model = models.for_model("any");
            assert_eq!(model.context_window, window);
            assert!(model.output_tokens < window, "window {window}");
        }
    }

    #[test]
    fn a_malformed_file_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MODELS_FILE),
            "[default]\ncontext_window = \"big\"\n",
        )
        .unwrap();
        let err = LocalModels::load(dir.path()).unwrap_err();
        assert!(err.contains("models.toml"), "{err}");
    }
}
