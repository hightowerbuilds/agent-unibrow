//! Per-turn caps (roadmap 6.6): a runaway agent stops and reports instead
//! of making hundreds of calls or a string of purchases.
//!
//! - Every tool call counts toward `max_calls_per_turn`. Once it's used up,
//!   every further call in the turn fails.
//! - Actions that send, buy, post or delete count toward
//!   `max_commits_per_turn`: `Commit`-effect tools, and mid-call approvals
//!   (a click on "Place order"), whether the user approved them or
//!   "always allow" did. Once it's used up, further such actions fail;
//!   reading still works so the agent can report.
//!
//! A turn is one user message and the agent's work on it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_calls_per_turn: u32,
    pub max_commits_per_turn: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_calls_per_turn: 60, max_commits_per_turn: 5 }
    }
}

/// What one turn has used so far.
#[derive(Debug, Default)]
pub struct TurnBudget {
    calls: u32,
    commits: u32,
    /// A mid-call approval was refused because of the commit cap; the
    /// call's error is replaced with [`commit_cap_message`].
    commit_blocked: bool,
}

impl TurnBudget {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Counts a tool call. Fails once the turn's calls are used up.
    pub fn start_call(&mut self, limits: &Limits) -> Result<(), String> {
        if self.calls >= limits.max_calls_per_turn {
            return Err(calls_cap_message(limits));
        }
        self.calls += 1;
        Ok(())
    }

    /// Whether another committing action may run this turn.
    pub fn check_commit(&mut self, limits: &Limits) -> Result<(), String> {
        if self.commits >= limits.max_commits_per_turn {
            self.commit_blocked = true;
            return Err(commit_cap_message(limits));
        }
        Ok(())
    }

    /// Counts a committing action that is going ahead.
    pub fn record_commit(&mut self) {
        self.commits += 1;
    }

    /// True once after [`TurnBudget::check_commit`] refused an action.
    pub fn take_commit_blocked(&mut self) -> bool {
        std::mem::take(&mut self.commit_blocked)
    }

    pub fn calls(&self) -> u32 {
        self.calls
    }

    pub fn commits(&self) -> u32 {
        self.commits
    }
}

/// Starts with "Limit reached" so the trace and evals can spot it.
pub fn calls_cap_message(limits: &Limits) -> String {
    format!(
        "Limit reached: this turn has used its {} tool calls (a safety cap). Don't call any more tools \
         (task_progress still works, to note where you stopped). Stop now and tell the user what you've done, what's left, and that they can say “continue” to go on.",
        limits.max_calls_per_turn
    )
}

pub fn commit_cap_message(limits: &Limits) -> String {
    format!(
        "Limit reached: this turn has already taken {} actions that send, buy, post or delete (a safety cap). \
         Don't try any more of them. Stop and tell the user what you did and what's left; they can say \
         “continue” to allow more.",
        limits.max_commits_per_turn
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_cap() {
        let l = Limits { max_calls_per_turn: 3, max_commits_per_turn: 1 };
        let mut b = TurnBudget::default();
        for _ in 0..3 {
            assert!(b.start_call(&l).is_ok());
        }
        let e = b.start_call(&l).unwrap_err();
        assert!(e.starts_with("Limit reached") && e.contains("3 tool calls"));
        assert!(b.start_call(&l).is_err(), "stays shut for the rest of the turn");
        b.reset();
        assert!(b.start_call(&l).is_ok(), "a new turn starts fresh");
    }

    #[test]
    fn commit_cap() {
        let l = Limits { max_calls_per_turn: 60, max_commits_per_turn: 2 };
        let mut b = TurnBudget::default();
        for _ in 0..2 {
            assert!(b.check_commit(&l).is_ok());
            b.record_commit();
        }
        assert!(!b.take_commit_blocked());
        assert!(b.check_commit(&l).unwrap_err().contains("2 actions"));
        assert!(b.take_commit_blocked());
        assert!(!b.take_commit_blocked(), "reported once");
        b.reset();
        assert!(b.check_commit(&l).is_ok());
    }

    #[test]
    fn defaults_fill_missing_fields() {
        let l: Limits = serde_json::from_str(r#"{"max_commits_per_turn": 2}"#).unwrap();
        assert_eq!(l, Limits { max_calls_per_turn: 60, max_commits_per_turn: 2 });
    }
}
