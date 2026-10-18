//! The models each backend offers, for the model menu.

use crate::BackendKind;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct ModelChoice {
    /// What the backend is given (`--model`, `-m`, the API's `model`).
    /// Empty means the backend's own default.
    pub id: String,
    pub label: String,
    /// A few words on what it's for.
    pub note: String,
}

fn choice(id: &str, label: &str, note: &str) -> ModelChoice {
    ModelChoice { id: id.into(), label: label.into(), note: note.into() }
}

/// The models known without asking the backend. OpenCode's come from
/// [`opencode_models`], which runs a command.
pub fn known(kind: BackendKind) -> Vec<ModelChoice> {
    match kind {
        BackendKind::ClaudeCode => vec![
            choice("fable", "Fable 5.1", "Most capable"),
            choice("opus", "Opus 5.5", "Deep work"),
            choice("sonnet", "Sonnet 5.5", "Fast and capable"),
            choice("haiku", "Haiku 5.5", "Fastest"),
        ],
        BackendKind::AnthropicApi => vec![
            choice("claude-fable-5-1", "Fable 5.1", "Most capable"),
            choice("claude-opus-5-5", "Opus 5.5", "Deep work"),
            choice("claude-sonnet-5-5", "Sonnet 5.5", "Fast and capable"),
            choice("claude-haiku-5-5", "Haiku 5.5", "Fastest"),
        ],
        BackendKind::Codex => codex_models(),
        BackendKind::OpenCode => vec![],
    }
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
}

/// Codex's default plus the models it lists, from the cache Codex keeps
/// of what the account can use.
fn codex_models() -> Vec<ModelChoice> {
    let mut out = vec![choice("", "Default", "Codex's own setting")];
    let Some(text) = codex_home().and_then(|h| std::fs::read_to_string(h.join("models_cache.json")).ok()) else {
        return out;
    };
    let Ok(cache) = serde_json::from_str::<serde_json::Value>(&text) else { return out };
    let mut models: Vec<&serde_json::Value> = cache["models"]
        .as_array()
        .map(|a| a.iter().filter(|m| m["visibility"].as_str() == Some("list")).collect())
        .unwrap_or_default();
    models.sort_by_key(|m| m["priority"].as_i64().unwrap_or(i64::MAX));
    out.extend(models.into_iter().filter_map(|m| {
        let id = m["slug"].as_str()?;
        Some(choice(id, m["display_name"].as_str().unwrap_or(id), m["description"].as_str().unwrap_or("")))
    }));
    out
}

/// The `provider/model` names OpenCode can use (`opencode models`). Blocks
/// while the command runs.
pub fn opencode_models() -> Vec<ModelChoice> {
    let Ok(out) = std::process::Command::new("opencode").arg("models").output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('/') && !l.contains(' '))
        .map(|id| {
            let (provider, name) = id.split_once('/').unwrap_or(("", id));
            choice(id, name, provider)
        })
        .collect()
}

/// The menu label for a model id: its known name, else the id itself.
pub fn label(kind: BackendKind, id: &str, extra: &[ModelChoice]) -> String {
    known(kind)
        .iter()
        .chain(extra)
        .find(|c| c.id == id)
        .map(|c| c.label.clone())
        .unwrap_or_else(|| if id.is_empty() { "Default".into() } else { id.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_models_have_names() {
        assert_eq!(label(BackendKind::ClaudeCode, "sonnet", &[]), "Sonnet 5.5");
        assert_eq!(label(BackendKind::AnthropicApi, "claude-haiku-5-5", &[]), "Haiku 5.5");
        assert_eq!(label(BackendKind::OpenCode, "x/y", &[]), "x/y");
        assert!(known(BackendKind::Codex).iter().any(|c| c.id.is_empty()), "Codex always offers its default");
    }
}
