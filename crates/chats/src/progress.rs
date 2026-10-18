//! Task plans (roadmap 7.4): a short checklist the agent keeps for a long
//! task, saved per chat so it survives a crash, a restart or a per-turn cap.
//!
//! The agent writes it with the `task_progress` tool ([`TaskProgress`]):
//! a plan of steps, then each step marked done with a note of what it found.
//! The chat shows it above the conversation, and while steps are left the
//! next message to the agent carries a one-line reminder ([`TaskPlan::reminder`]).

use crate::{ChatId, Chats, now, when};
use chrono::{DateTime, Local};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolOutput, ToolResult};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;

/// Most steps in a plan.
pub const MAX_STEPS: usize = 30;
const STEP_CHARS: usize = 200;
const NOTE_CHARS: usize = 400;
const GOAL_CHARS: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct TaskStep {
    pub text: String,
    pub done: bool,
    /// What the step found or decided, for picking up later.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskPlan {
    pub goal: String,
    pub steps: Vec<TaskStep>,
    pub updated: DateTime<Local>,
}

fn cut(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n { s.chars().take(n - 1).collect::<String>() + "…" } else { s }
}

impl TaskPlan {
    pub fn done_count(&self) -> usize {
        self.steps.iter().filter(|s| s.done).count()
    }

    /// The first step not done yet, with its 0-based index.
    pub fn next(&self) -> Option<(usize, &TaskStep)> {
        self.steps.iter().enumerate().find(|(_, s)| !s.done)
    }

    pub fn finished(&self) -> bool {
        self.next().is_none()
    }

    /// The plan as the agent reads it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.goal.is_empty() {
            out.push_str(&format!("Task: {}\n", self.goal));
        }
        match self.next() {
            Some((i, _)) => out.push_str(&format!(
                "Progress: {} of {} steps done. Next: step {}.\n",
                self.done_count(),
                self.steps.len(),
                i + 1
            )),
            None => out.push_str(&format!("All {} steps done.\n", self.steps.len())),
        }
        for (i, s) in self.steps.iter().enumerate() {
            let note = s.note.as_ref().map(|n| format!(" — {n}")).unwrap_or_default();
            out.push_str(&format!("{}. [{}] {}{note}\n", i + 1, if s.done { "x" } else { " " }, s.text));
        }
        out
    }

    /// One line for the next message to the agent, while steps are left.
    pub fn reminder(&self) -> Option<String> {
        let (i, step) = self.next()?;
        Some(format!(
            "[This chat has an unfinished task plan: {} of {} steps done, next is step {} “{}”. \
             Call task_progress to see it and the notes so far before carrying on; don't redo done steps. \
             If this message is about something else, replace or clear the plan.]",
            self.done_count(),
            self.steps.len(),
            i + 1,
            cut(&step.text, 80)
        ))
    }

    /// A new plan of `steps`. Steps already in the old plan (same words)
    /// keep their done mark and note, so re-planning doesn't lose progress.
    fn replan(old: Option<&TaskPlan>, goal: Option<&str>, steps: &[String]) -> TaskPlan {
        let key = |s: &str| s.trim().to_lowercase();
        let steps = steps
            .iter()
            .filter(|s| !s.trim().is_empty())
            .take(MAX_STEPS)
            .map(|s| {
                let text = cut(s, STEP_CHARS);
                let prev = old.and_then(|o| o.steps.iter().find(|p| key(&p.text) == key(&text)));
                TaskStep { done: prev.is_some_and(|p| p.done), note: prev.and_then(|p| p.note.clone()), text }
            })
            .collect();
        let goal = goal.map(|g| cut(g, GOAL_CHARS)).or_else(|| old.map(|o| o.goal.clone())).unwrap_or_default();
        TaskPlan { goal, steps, updated: Local::now() }
    }
}

impl Chats {
    /// The chat's task plan, if it has one.
    pub fn plan(&self, chat: ChatId) -> Option<TaskPlan> {
        let row: Option<(String, String, i64)> = self
            .conn()
            .query_row("SELECT goal, steps, updated FROM task_plans WHERE chat_id = ?1", params![chat], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()
            .ok()
            .flatten();
        let (goal, steps, updated) = row?;
        let steps: Vec<Value> = serde_json::from_str(&steps).ok()?;
        let steps = steps
            .iter()
            .filter_map(|s| {
                Some(TaskStep {
                    text: s["text"].as_str()?.to_string(),
                    done: s["done"].as_bool().unwrap_or(false),
                    note: s["note"].as_str().map(str::to_string),
                })
            })
            .collect();
        Some(TaskPlan { goal, steps, updated: when(updated) })
    }

    pub fn set_plan(&self, chat: ChatId, plan: &TaskPlan) {
        let steps: Vec<Value> =
            plan.steps.iter().map(|s| json!({"text": s.text, "done": s.done, "note": s.note})).collect();
        let _ = self.conn().execute(
            "INSERT INTO task_plans (chat_id, goal, steps, updated) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat_id) DO UPDATE SET goal = ?2, steps = ?3, updated = ?4",
            params![chat, plan.goal, Value::Array(steps).to_string(), now()],
        );
    }

    pub fn clear_plan(&self, chat: ChatId) {
        let _ = self.conn().execute("DELETE FROM task_plans WHERE chat_id = ?1", params![chat]);
    }

    /// The reminder line for the chat's next message, if a plan is unfinished.
    pub fn plan_reminder(&self, chat: ChatId) -> Option<String> {
        self.plan(chat)?.reminder()
    }
}

/// `task_progress`: the agent's checklist for the chat it works in.
pub struct TaskProgress {
    chats: Arc<Chats>,
    chat: ChatId,
}

impl TaskProgress {
    pub fn new(chats: Arc<Chats>, chat: ChatId) -> Arc<dyn Tool> {
        Arc::new(Self { chats, chat })
    }

    /// Applies one call to the stored plan and says what it looks like now.
    pub fn apply(chats: &Chats, chat: ChatId, input: &Value) -> Result<String, String> {
        if let Some(o) = input.as_object() {
            for k in o.keys() {
                if !["action", "goal", "steps", "step", "note"].contains(&k.as_str()) {
                    return Err(format!("Invalid input: unknown field `{k}`"));
                }
            }
        }
        let action = input["action"].as_str().unwrap_or("view");
        let old = chats.plan(chat);
        let texts = || -> Result<Vec<String>, String> {
            let steps: Vec<String> = input["steps"]
                .as_array()
                .ok_or("Invalid input: `steps` (a list of short step descriptions) is required")?
                .iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .filter(|s| !s.trim().is_empty())
                .collect();
            if steps.is_empty() {
                return Err("Invalid input: give at least one step".into());
            }
            Ok(steps)
        };
        let step_index = |plan: &TaskPlan| -> Result<usize, String> {
            let n = input["step"].as_u64().ok_or("Invalid input: `step` (the step's number, from 1) is required")?
                as usize;
            if n == 0 || n > plan.steps.len() {
                return Err(format!("There's no step {n}; the plan has {} steps.", plan.steps.len()));
            }
            Ok(n - 1)
        };
        let no_plan = "There's no task plan in this chat yet. Make one with action \"plan\".";
        let plan = match action {
            "view" => return Ok(old.map(|p| p.render()).unwrap_or_else(|| no_plan.to_string())),
            "clear" => {
                chats.clear_plan(chat);
                return Ok("Cleared the task plan.".into());
            }
            "plan" => TaskPlan::replan(old.as_ref(), input["goal"].as_str(), &texts()?),
            "add" => {
                let mut p = old.ok_or(no_plan)?;
                let mut steps: Vec<String> = p.steps.iter().map(|s| s.text.clone()).collect();
                steps.extend(texts()?);
                if steps.len() > MAX_STEPS {
                    return Err(format!("A plan holds at most {MAX_STEPS} steps; combine some."));
                }
                let goal = p.goal.clone();
                p = TaskPlan::replan(Some(&p), Some(&goal), &steps);
                p
            }
            "done" | "undo" | "note" => {
                let mut p = old.ok_or(no_plan)?;
                let i = step_index(&p)?;
                match action {
                    "done" => p.steps[i].done = true,
                    "undo" => p.steps[i].done = false,
                    _ => {}
                }
                if let Some(n) = input["note"].as_str().filter(|n| !n.trim().is_empty()) {
                    p.steps[i].note = Some(cut(n, NOTE_CHARS));
                } else if action == "note" {
                    return Err("Invalid input: `note` is required".into());
                }
                p
            }
            other => return Err(format!("Invalid input: unknown action `{other}`")),
        };
        chats.set_plan(chat, &plan);
        Ok(plan.render())
    }
}

impl Tool for TaskProgress {
    fn name(&self) -> &'static str {
        "task_progress"
    }
    fn description(&self) -> &'static str {
        "Your checklist for a long task in this chat (many steps, pages or turns). It's saved with the chat, \
         survives restarts, and the user sees it above the conversation. action \"plan\": write the steps \
         (short, in order; re-planning keeps steps already done). \"done\": mark step N done, with a `note` of \
         what it found that you'll need later (prices, names, codes). \"add\": append steps. \"note\": add a note \
         to a step. \"undo\": mark a step not done. \"view\": see the plan and notes: do this first when you \
         pick up a chat after a restart, a stop, or a \"Limit reached\". \"clear\": remove the plan."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["view", "plan", "add", "done", "undo", "note", "clear"], "description": "Default view."},
                "goal": {"type": "string", "description": "With plan: the task in one line."},
                "steps": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_STEPS, "description": "With plan or add: the steps."},
                "step": {"type": "integer", "minimum": 1, "description": "With done, undo, note: the step's number."},
                "note": {"type": "string", "description": "With done or note: what the step found, briefly."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        match input["action"].as_str().unwrap_or("view") {
            "plan" => "Write the task plan".into(),
            "done" => format!("Mark step {} done", input["step"]),
            "view" => "Check the task plan".into(),
            other => format!("Task plan: {other}"),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let (chats, chat) = (self.chats.clone(), self.chat);
        Box::pin(async move {
            let text = Self::apply(&chats, chat, &input)?;
            let summary = match chats.plan(chat) {
                Some(p) => format!("{} of {} steps done", p.done_count(), p.steps.len()),
                None => "no plan".into(),
            };
            Ok(ToolOutput::text(text, summary))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &Chats, chat: ChatId, v: Value) -> Result<String, String> {
        TaskProgress::apply(s, chat, &v)
    }

    #[test]
    fn plan_mark_and_view() {
        let s = Chats::in_memory();
        let a = s.create();
        assert!(call(&s, a, json!({})).unwrap().contains("no task plan"));
        assert!(s.plan_reminder(a).is_none());
        let out = call(&s, a, json!({"action": "plan", "goal": "Count the shelves", "steps": ["Shelf 1", "Shelf 2", " ", "Shelf 3"]})).unwrap();
        assert!(out.contains("0 of 3 steps done. Next: step 1."), "{out}");
        let out = call(&s, a, json!({"action": "done", "step": 1, "note": "17 crates, code OTTER"})).unwrap();
        assert!(out.contains("1. [x] Shelf 1 — 17 crates, code OTTER"), "{out}");
        assert!(out.contains("2. [ ] Shelf 2"));
        let r = s.plan_reminder(a).unwrap();
        assert!(r.contains("1 of 3 steps done") && r.contains("“Shelf 2”"), "{r}");
        assert!(call(&s, a, json!({"action": "done", "step": 9})).unwrap_err().contains("no step 9"));
        assert!(call(&s, a, json!({"action": "nope"})).is_err());
        assert!(call(&s, a, json!({"action": "plan", "bogus": 1})).is_err());
        // Survives reopening (it's in the database, not memory).
        let p = s.plan(a).unwrap();
        assert_eq!(p.goal, "Count the shelves");
        assert_eq!(p.done_count(), 1);
    }

    #[test]
    fn replanning_keeps_progress_and_finishing_drops_the_reminder() {
        let s = Chats::in_memory();
        let a = s.create();
        call(&s, a, json!({"action": "plan", "steps": ["Open list", "Read prices"]})).unwrap();
        call(&s, a, json!({"action": "done", "step": 1, "note": "3 items"})).unwrap();
        let out = call(&s, a, json!({"action": "plan", "steps": ["open list", "Read prices", "Report"]})).unwrap();
        assert!(out.contains("1. [x] open list — 3 items"), "{out}");
        call(&s, a, json!({"action": "add", "steps": ["Double-check"]})).unwrap();
        for n in 2..=4 {
            call(&s, a, json!({"action": "done", "step": n})).unwrap();
        }
        let p = s.plan(a).unwrap();
        assert!(p.finished());
        assert!(p.render().contains("All 4 steps done."));
        assert!(s.plan_reminder(a).is_none());
        call(&s, a, json!({"action": "undo", "step": 4})).unwrap();
        assert!(s.plan_reminder(a).is_some());
        call(&s, a, json!({"action": "clear"})).unwrap();
        assert!(s.plan(a).is_none());
    }

    #[test]
    fn plans_are_per_chat_and_go_with_the_chat() {
        let s = Chats::in_memory();
        let (a, b) = (s.create(), s.create());
        call(&s, a, json!({"action": "plan", "steps": ["One"]})).unwrap();
        assert!(s.plan(b).is_none());
        s.delete(a);
        assert!(s.plan(a).is_none());
    }
}
