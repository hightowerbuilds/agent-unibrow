//! Decides which tool calls run on their own and which wait for the user.
//!
//! - `Read` and `Stage` calls run (searching mail, writing a draft).
//! - `Commit` calls (sending, buying) ask, unless the user has said
//!   "always allow" for this tool and every counterparty on the call.
//!
//! Spend caps and merchant allowlists for purchases land here in M3.

use router::{Effect, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { reason: String },
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    /// tool name → counterparties the user has pre-approved for that tool.
    #[serde(default)]
    always_allow: BTreeMap<String, BTreeSet<String>>,
}

pub struct Policy {
    stored: Stored,
    path: Option<PathBuf>,
}

impl Policy {
    /// Loads `~/.config/agent-unibrow/policy.json`, or starts empty.
    pub fn load() -> Self {
        let path = dirs::config_dir().map(|d| d.join("agent-unibrow").join("policy.json"));
        let stored = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { stored, path }
    }

    pub fn in_memory() -> Self {
        Self { stored: Stored::default(), path: None }
    }

    pub fn decide(&self, tool: &dyn Tool, input: &Value) -> Decision {
        match tool.effect() {
            Effect::Read | Effect::Stage => Decision::Allow,
            Effect::Commit => {
                let parties = normalized(tool.counterparties(input));
                let allowed = self.stored.always_allow.get(tool.name());
                let covered = !parties.is_empty()
                    && allowed.is_some_and(|a| parties.iter().all(|p| a.contains(p)));
                if covered {
                    Decision::Allow
                } else if parties.is_empty() {
                    Decision::Ask { reason: format!("{} has an external effect", tool.name()) }
                } else {
                    Decision::Ask {
                        reason: format!("{} to {}", tool.name(), parties.into_iter().collect::<Vec<_>>().join(", ")),
                    }
                }
            }
        }
    }

    /// Records "always allow" for this tool and these counterparties.
    pub fn always_allow(&mut self, tool: &str, counterparties: Vec<String>) {
        self.stored
            .always_allow
            .entry(tool.to_string())
            .or_default()
            .extend(normalized(counterparties));
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.stored) {
            let _ = std::fs::write(path, s);
        }
    }
}

fn normalized(parties: Vec<String>) -> BTreeSet<String> {
    parties.into_iter().map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use router::ToolResult;

    struct Send;
    impl Tool for Send {
        fn name(&self) -> &'static str { "send" }
        fn description(&self) -> &'static str { "" }
        fn input_schema(&self) -> Value { Value::Null }
        fn effect(&self) -> Effect { Effect::Commit }
        fn counterparties(&self, input: &Value) -> Vec<String> {
            input["to"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default()
        }
        fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> { unreachable!() }
    }

    #[test]
    fn commit_asks_until_allowed() {
        let mut p = Policy::in_memory();
        let input = serde_json::json!({"to": "Zack@Example.com"});
        assert!(matches!(p.decide(&Send, &input), Decision::Ask { .. }));
        p.always_allow("send", vec!["zack@example.com".into()]);
        assert_eq!(p.decide(&Send, &input), Decision::Allow);
        let other = serde_json::json!({"to": "someone@else.com"});
        assert!(matches!(p.decide(&Send, &other), Decision::Ask { .. }));
    }
}
