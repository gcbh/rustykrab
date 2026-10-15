//! `rustykrab-cli models`: the effective per-model configuration.
//!
//! Prints what `<data>/models.toml` resolves to for `[default]`, for every
//! named model, and for the models this daemon would run (the chat model
//! and the local workers' model), so an operator can see the one window
//! each runs at before restarting. It also names any retired environment
//! setting still set, which no longer changes anything.

use std::path::Path;

use rustykrab_agent::local_model::{LocalModels, MODELS_FILE, RETIRED_SETTINGS};
use rustykrab_agent::LocalModel;

fn describe(model: &LocalModel) -> String {
    let mut parts = vec![
        format!("context_window={}", model.context_window),
        format!("output_tokens={}", model.output_tokens),
        format!(
            "keep_alive={}",
            model.keep_alive.as_deref().unwrap_or("server")
        ),
        format!("compaction_strategy={}", model.compaction_strategy.name()),
        format!("compaction_threshold={}", model.compaction_threshold),
    ];
    if let Some(t) = model.temperature {
        parts.push(format!("temperature={t}"));
    }
    if let Some(p) = model.top_p {
        parts.push(format!("top_p={p}"));
    }
    if let Some(think) = model.think {
        parts.push(format!("think={think}"));
    }
    if let Some(w) = model.compaction_summary_window {
        parts.push(format!("compaction_summary_window={w}"));
    }
    if let Some(b) = model.late_tool_binding {
        parts.push(format!("late_tool_binding={b:?}"));
    }
    parts.join(" ")
}

/// The retired environment settings that are still set, for the startup
/// warning and the report: they no longer change anything.
pub(crate) fn retired_settings_in_env() -> Vec<&'static str> {
    RETIRED_SETTINGS
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect()
}

/// The report, as text.
pub(crate) fn report(
    data_dir: &Path,
    chat_model: Option<&str>,
    worker_model: Option<&str>,
) -> Result<String, String> {
    let models = LocalModels::load(data_dir)?;
    let mut out = String::new();
    match models.source() {
        Some(path) => out.push_str(&format!("{}\n", path.display())),
        None => out.push_str(&format!(
            "{} not found: built-in defaults\n",
            data_dir.join(MODELS_FILE).display()
        )),
    }
    for (name, model) in models.entries() {
        out.push_str(&format!("  {name}: {}\n", describe(model)));
    }
    for (role, tag) in [("chat", chat_model), ("local workers", worker_model)] {
        if let Some(tag) = tag {
            let from = if models.names(tag) { tag } else { "[default]" };
            out.push_str(&format!(
                "{role} model {tag} runs with {from}: {}\n",
                describe(&models.for_model(tag))
            ));
        }
    }
    let retired = retired_settings_in_env();
    if !retired.is_empty() {
        out.push_str(&format!(
            "ignored (set these in {MODELS_FILE} instead): {}\n",
            retired.join(", ")
        ));
    }
    Ok(out)
}

pub(crate) fn run(data_dir: &Path) -> anyhow::Result<()> {
    let chat = std::env::var("OLLAMA_MODEL").ok();
    let worker = crate::local_resources::Config::from_env()
        .ok()
        .flatten()
        .map(|c| c.model);
    let text = report(data_dir, chat.as_deref(), worker.as_deref()).map_err(anyhow::Error::msg)?;
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_the_file_each_entry_and_what_each_model_resolves_to() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MODELS_FILE),
            "[default]\ncontext_window = 65536\n[models.\"qwen3.8:27b-mlx\"]\ncontext_window = 131072\n",
        )
        .unwrap();
        let text = report(dir.path(), Some("gemma4:26b"), Some("qwen3.8:27b-mlx")).unwrap();
        assert!(text.contains("models.toml"), "{text}");
        assert!(text.contains("[default]: context_window=65536"), "{text}");
        assert!(
            text.contains("chat model gemma4:26b runs with [default]: context_window=65536"),
            "{text}"
        );
        assert!(
            text.contains(
                "local workers model qwen3.8:27b-mlx runs with qwen3.8:27b-mlx: context_window=131072"
            ),
            "{text}"
        );
    }

    #[test]
    fn no_file_reports_the_built_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let text = report(dir.path(), None, None).unwrap();
        assert!(text.contains("not found: built-in defaults"), "{text}");
        assert!(text.contains("context_window=131072"), "{text}");
    }

    #[test]
    fn a_file_the_daemon_would_refuse_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MODELS_FILE),
            "[default]\ncontext_window = 1000\n",
        )
        .unwrap();
        let err = report(dir.path(), None, None).unwrap_err();
        assert!(err.contains("context_window 1000"), "{err}");
    }
}
