//! Decides which tool calls run on their own and which wait for the user.
//!
//! - `Read` and `Stage` calls run (searching mail, writing a draft).
//! - `Commit` calls (sending, buying) ask, unless the user has said
//!   "always allow" for this tool and every counterparty on the call.
//! - Site scope ([`scope`]): with "Only act on focus sites" on, acting on
//!   any other site asks.
//! - Spending: purchases over the user's limit always ask, even where
//!   "always allow" applies.
//! - Per-turn caps on calls and committing actions ([`limits`]).
//!
//! Stored in `policy.json` in the config folder, or in `$UNIBROW_DATA_DIR`
//! for isolated test instances.

pub mod limits;
pub mod scope;

pub use limits::{Limits, TurnBudget};

use router::{Effect, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

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
    /// "Only act on focus sites" (6.3).
    #[serde(default)]
    focus_only: bool,
    /// Purchases over this amount always ask, even when always-allowed
    /// (6.6). `None`: no limit.
    #[serde(default)]
    purchase_limit: Option<f64>,
    #[serde(default)]
    limits: Limits,
}

/// Where the focus-site list comes from (the browsing store, set by the shell).
pub type FocusSource = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

pub struct Policy {
    stored: Stored,
    path: Option<PathBuf>,
    focus: Option<FocusSource>,
}

impl Policy {
    /// `$UNIBROW_DATA_DIR/policy.json` for test instances, otherwise
    /// `~/.config/agent-unibrow/policy.json`.
    pub fn default_path() -> Option<PathBuf> {
        match std::env::var_os("UNIBROW_DATA_DIR") {
            Some(d) => Some(PathBuf::from(d).join("policy.json")),
            None => dirs::config_dir().map(|d| d.join("agent-unibrow").join("policy.json")),
        }
    }

    /// Loads [`Policy::default_path`], or starts empty.
    pub fn load() -> Self {
        let path = Self::default_path();
        let stored = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { stored, path, focus: None }
    }

    pub fn in_memory() -> Self {
        Self { stored: Stored::default(), path: None, focus: None }
    }

    /// Tells the policy where to find the user's focus sites.
    pub fn set_focus_source(&mut self, source: FocusSource) {
        self.focus = Some(source);
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

    // ------------------------------------------------------------ scope

    pub fn focus_only(&self) -> bool {
        self.stored.focus_only
    }

    pub fn set_focus_only(&mut self, on: bool) {
        self.stored.focus_only = on;
        self.save();
    }

    /// Whether acting on `site` needs the user's approval under the site
    /// scope, and why. `None`: go ahead.
    pub fn scope_check(&self, tool: &str, site: &str) -> Option<String> {
        if !self.stored.focus_only {
            return None;
        }
        let focus = self.focus.as_ref().map(|f| f()).unwrap_or_default();
        let site = scope::normalize_site(site);
        if scope::in_focus(&site, &focus) || self.is_always_allowed(scope::SCOPE_KEY, std::slice::from_ref(&site)) {
            return None;
        }
        Some(format!("{tool} on {site}, which isn't one of your focus sites (you chose “Only act on focus sites”)"))
    }

    /// The user approved acting on `site` from now on.
    pub fn allow_site(&mut self, site: &str) {
        self.always_allow(scope::SCOPE_KEY, vec![scope::normalize_site(site)]);
    }

    // ---------------------------------------------------------- spending

    pub fn purchase_limit(&self) -> Option<f64> {
        self.stored.purchase_limit
    }

    pub fn set_purchase_limit(&mut self, limit: Option<f64>) {
        self.stored.purchase_limit = limit.filter(|l| l.is_finite() && *l >= 0.0);
        self.save();
    }

    /// Whether a purchase of `amount` must be shown to the user even if
    /// "always allow" covers it: it's over the limit, or the amount isn't
    /// known while a limit is set.
    pub fn purchase_needs_user(&self, amount: Option<f64>) -> bool {
        match (self.stored.purchase_limit, amount) {
            (None, _) => false,
            (Some(limit), Some(a)) => a > limit,
            (Some(_), None) => true,
        }
    }

    // ------------------------------------------------------------ limits

    pub fn limits(&self) -> Limits {
        self.stored.limits
    }

    // ------------------------------------------------------ always allow

    /// Whether the user has pre-approved `tool` for every one of `counterparties`.
    pub fn is_always_allowed(&self, tool: &str, counterparties: &[String]) -> bool {
        let parties = normalized(counterparties.to_vec());
        !parties.is_empty()
            && self.stored.always_allow.get(tool).is_some_and(|a| parties.iter().all(|p| a.contains(p)))
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

    #[test]
    fn scope_off_by_default() {
        let p = Policy::in_memory();
        assert_eq!(p.scope_check("browser_click", "example.com"), None);
    }

    #[test]
    fn scope_asks_outside_focus() {
        let mut p = Policy::in_memory();
        p.set_focus_source(Arc::new(|| vec!["shop.test".to_string()]));
        p.set_focus_only(true);
        assert_eq!(p.scope_check("browser_click", "shop.test"), None);
        assert_eq!(p.scope_check("browser_type", "www.shop.test"), None);
        assert_eq!(p.scope_check("browser_type", "checkout.shop.test"), None);
        let why = p.scope_check("browser_type", "evil.test").expect("asks");
        assert!(why.contains("evil.test") && why.contains("focus"));
        // "Always allow on this site" lets it through from then on.
        p.allow_site("www.evil.test");
        assert_eq!(p.scope_check("browser_type", "evil.test"), None);
        assert!(p.scope_check("browser_type", "other.test").is_some());
    }

    #[test]
    fn scope_with_no_focus_sites_asks_everywhere() {
        let mut p = Policy::in_memory();
        p.set_focus_only(true);
        assert!(p.scope_check("browser_click", "example.com").is_some());
    }

    #[test]
    fn purchase_limit() {
        let mut p = Policy::in_memory();
        assert!(!p.purchase_needs_user(Some(10_000.0)), "no limit set");
        assert!(!p.purchase_needs_user(None));
        p.set_purchase_limit(Some(50.0));
        assert!(!p.purchase_needs_user(Some(49.99)));
        assert!(!p.purchase_needs_user(Some(50.0)));
        assert!(p.purchase_needs_user(Some(50.01)));
        assert!(p.purchase_needs_user(None), "unknown amount can't be shown to be under the limit");
        p.set_purchase_limit(Some(f64::NAN));
        assert_eq!(p.purchase_limit(), None);
    }

    #[test]
    fn stored_fields_round_trip() {
        let s: Stored = serde_json::from_str(
            r#"{"always_allow":{},"focus_only":true,"purchase_limit":25.0,"limits":{"max_calls_per_turn":7}}"#,
        )
        .unwrap();
        assert!(s.focus_only);
        assert_eq!(s.purchase_limit, Some(25.0));
        assert_eq!(s.limits.max_calls_per_turn, 7);
        assert_eq!(s.limits.max_commits_per_turn, 5);
        // Old files without the new fields still load.
        let old: Stored = serde_json::from_str(r#"{"always_allow":{"send":["a@b.c"]}}"#).unwrap();
        assert!(!old.focus_only && old.purchase_limit.is_none());
        assert_eq!(old.limits, Limits::default());
    }
}
