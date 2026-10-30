// ===========================================================================
// Sub-agents — the main agent hands a self-contained task to a fresh agent
//
// A sub-agent works in the same project with the same model and tools (minus
// spawn_agent, so there is one level only) and has its own conversation.
// spawn_agent returns at once: the sub-agent runs in the background while the
// main agent keeps working. Its final report is queued for the main agent,
// which reads it at its next step, collects it with wait_for_agents, or gets
// it before its final answer (the agent loop waits for running sub-agents and
// then has the main agent summarize everyone's work).
//
// Front-ends see everything a sub-agent does as `AgentEvent::SubAgent`
// wrapping its own events, bracketed by `SubAgentStarted` / `SubAgentFinished`.
// Approval requests arrive wrapped too and are answered with `respond_approval`.
// ===========================================================================

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::json;
use tokio::sync::mpsc;

use super::{Agent, AgentEvent};
use crate::ai::types::Role;
use crate::tools::{Tool, ToolContext, ToolResult};

pub const TOOL_NAME: &str = "spawn_agent";

/// Controls for a running sub-agent.
struct Handle {
    name: String,
    approval_tx: mpsc::UnboundedSender<bool>,
    cancel_tx: mpsc::UnboundedSender<()>,
    /// Its inbox: message_agent drops notes here.
    inbox: mpsc::UnboundedSender<String>,
    /// Inbox of the agent that started it, which identifies that agent.
    owner: Option<Inbox>,
}

type Inbox = mpsc::UnboundedSender<String>;

/// Whether something started by `started_by` belongs to `owner`.
fn owned(started_by: &Option<Inbox>, owner: &Inbox) -> bool {
    started_by.as_ref().is_some_and(|s| s.same_channel(owner))
}

/// The main agent's inbox, known once it has started a sub-agent.
static MAIN_INBOX: Lazy<Mutex<Option<mpsc::UnboundedSender<String>>>> =
    Lazy::new(|| Mutex::new(None));

pub const MESSAGE_TOOL_NAME: &str = "message_agent";
pub const WAIT_TOOL_NAME: &str = "wait_for_agents";

/// A finished sub-agent's report, waiting for the main agent to read it.
pub struct Report {
    pub id: String,
    pub name: String,
    pub success: bool,
    pub text: String,
    owner: Option<Inbox>,
}

impl Report {
    /// How the report reads in the main agent's conversation.
    pub fn note(&self) -> String {
        let state = if self.success { "finished" } else { "stopped" };
        format!(
            "[Sub-agent \"{}\" {state}. Its report:]\n{}",
            self.name, self.text
        )
    }
}

static REPORTS: Lazy<Mutex<Vec<Report>>> = Lazy::new(|| Mutex::new(Vec::new()));
/// Signalled whenever a sub-agent ends.
static FINISHED: Lazy<tokio::sync::Notify> = Lazy::new(tokio::sync::Notify::new);

fn reports() -> std::sync::MutexGuard<'static, Vec<Report>> {
    REPORTS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Unread reports of the sub-agents `owner` started (removes them).
pub fn take_reports(owner: &Inbox) -> Vec<Report> {
    let (mine, rest) = std::mem::take(&mut *reports())
        .into_iter()
        .partition(|r| owned(&r.owner, owner));
    *reports() = rest;
    mine
}

/// Number of sub-agents `owner` started that are still working.
pub fn running_count(owner: &Inbox) -> usize {
    running()
        .values()
        .filter(|h| owned(&h.owner, owner))
        .count()
}

/// True while `owner`'s sub-agents work or their reports are unread.
pub fn outstanding(owner: &Inbox) -> bool {
    running_count(owner) > 0 || reports().iter().any(|r| owned(&r.owner, owner))
}

/// Stop the sub-agents `owner` started.
pub fn cancel_owned(owner: &Inbox) {
    for h in running().values().filter(|h| owned(&h.owner, owner)) {
        let _ = h.cancel_tx.send(());
    }
}

/// Wait until every sub-agent in `ids` (all of `owner`'s when `None`) has ended.
pub async fn wait_until_done(owner: &Inbox, ids: Option<&[String]>) {
    loop {
        let notified = FINISHED.notified();
        let busy = {
            let agents = running();
            match ids {
                Some(ids) => ids.iter().any(|id| agents.contains_key(id)),
                None => agents.values().any(|h| owned(&h.owner, owner)),
            }
        };
        if !busy {
            return;
        }
        notified.await;
    }
}

static RUNNING: Lazy<Mutex<HashMap<String, Handle>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn running() -> std::sync::MutexGuard<'static, HashMap<String, Handle>> {
    RUNNING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Answer the pending approval request of sub-agent `id`.
pub fn respond_approval(id: &str, approved: bool) {
    if let Some(h) = running().get(id) {
        let _ = h.approval_tx.send(approved);
    }
}

/// Stop sub-agent `id`; its spawn_agent call returns what it had so far.
pub fn cancel(id: &str) {
    if let Some(h) = running().get(id) {
        let _ = h.cancel_tx.send(());
    }
}

/// Stop every running sub-agent.
pub fn cancel_all() {
    for h in running().values() {
        let _ = h.cancel_tx.send(());
    }
}

/// (id, name) of the sub-agents running right now.
pub fn list_running() -> Vec<(String, String)> {
    let mut list: Vec<_> = running()
        .iter()
        .map(|(id, h)| (id.clone(), h.name.clone()))
        .collect();
    list.sort();
    list
}

/// Removes the registry entry however the run ends (including the parent
/// being cancelled, which drops the tool's future).
struct Registration {
    id: String,
    parent: Option<mpsc::UnboundedSender<AgentEvent>>,
    finished: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        running().remove(&self.id);
        if !self.finished {
            if let Some(tx) = &self.parent {
                let _ = tx.send(AgentEvent::SubAgentFinished {
                    id: self.id.clone(),
                    success: false,
                    summary: "Cancelled".into(),
                });
            }
        }
        FINISHED.notify_waiters();
    }
}

pub struct SpawnAgentTool;

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Start a sub-agent: a separate agent with the same tools that works on one self-contained \
         task in this project. It returns at once and the sub-agent runs in the background, so you \
         keep working at the same time (take a part of the job yourself, or coordinate). Use it when \
         the user asks for agents or sub-agents (\"use 3 agents\" means you orchestrate 3 sub-agents), \
         or for independent parts of a big job that can proceed in parallel (research, a separate \
         module, tests, a review). Its report arrives as a \"[Sub-agent ... finished]\" note; call \
         wait_for_agents when you need results before going on. Before your final answer you receive \
         every outstanding report: then give the user a summary of what each agent did and the \
         overall result. The sub-agent cannot see this conversation: give it everything it needs in \
         `task` (goal, files, constraints, what to report). Don't give two agents the same files to \
         edit; message_agent lets you talk to them while they work."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Short label shown to the user, e.g. \"tests\" or \"docs research\"" },
                "task": { "type": "string", "description": "Complete, self-contained instructions for the sub-agent" }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let task = args["task"].as_str().unwrap_or("").trim().to_string();
        if task.is_empty() {
            return ToolResult::err("'task' must describe what the sub-agent should do");
        }
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| crate::util::truncate_chars(n, 40))
            .unwrap_or_else(|| crate::util::truncate_chars(&task, 40));
        let id = format!("agent-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let ctx = ctx.clone();
        let (run_id, run_name) = (id.clone(), name.clone());
        tokio::spawn(async move { run_subagent(run_id, run_name, task, &ctx).await });
        ToolResult::ok(format!(
            "Started sub-agent \"{name}\" (id {id}). It works in the background; keep going with \
             your own part. Its report arrives as a note, or call wait_for_agents to collect it."
        ))
    }
}

async fn run_subagent(id: String, name: String, task: String, ctx: &ToolContext) {
    let parent = ctx.events.clone();
    let send = |event: AgentEvent| {
        if let Some(tx) = &parent {
            let _ = tx.send(event);
        }
    };

    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<()>();
    let mut agent = Agent::new((*ctx.settings).clone(), sub_tx);
    agent.make_subagent(&name);
    agent.approval_disabled = ctx.approvals_off;
    // Sub-agents can write back to the agent that started them.
    if ctx.agent_name.is_none() {
        if let Some(inbox) = &ctx.inbox {
            *MAIN_INBOX.lock().unwrap_or_else(|p| p.into_inner()) = Some(inbox.clone());
        }
    }

    running().insert(
        id.clone(),
        Handle {
            name: name.clone(),
            approval_tx: agent.approval_sender(),
            cancel_tx,
            inbox: agent.inbox_sender(),
            owner: ctx.inbox.clone(),
        },
    );
    let mut registration = Registration {
        id: id.clone(),
        parent: parent.clone(),
        finished: false,
    };
    send(AgentEvent::SubAgentStarted {
        id: id.clone(),
        name: name.clone(),
        task: task.clone(),
    });

    // Everything the sub-agent reports goes to the front-end, tagged with its id.
    let forward = {
        let parent = parent.clone();
        let id = id.clone();
        tokio::spawn(async move {
            while let Some(event) = sub_rx.recv().await {
                if let Some(tx) = &parent {
                    let _ = tx.send(AgentEvent::SubAgent {
                        id: id.clone(),
                        event: Box::new(event),
                    });
                }
            }
        })
    };

    let outcome = agent.process_message(&task, &mut cancel_rx).await;
    let report = agent
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
        .map(|m| m.text())
        .unwrap_or_default();
    drop(agent);
    let _ = forward.await;

    registration.finished = true;
    let (success, summary) = match &outcome {
        Ok(_) if !report.trim().is_empty() => (true, report.clone()),
        Ok(_) => (false, "The sub-agent stopped without a report.".to_string()),
        Err(e) => (false, format!("The sub-agent failed: {e}")),
    };
    send(AgentEvent::SubAgentFinished {
        id: id.clone(),
        success,
        summary: crate::util::truncate_chars(&summary, 400),
    });
    let text = if success || report.trim().is_empty() {
        summary
    } else {
        format!("{summary}\n\nIts last message:\n{report}")
    };
    // Queue the report before the registration goes, so waiters find it.
    reports().push(Report {
        id: id.clone(),
        name,
        success,
        text,
        owner: ctx.inbox.clone(),
    });
    drop(registration);
}

pub struct WaitForAgentsTool;

#[async_trait]
impl Tool for WaitForAgentsTool {
    fn name(&self) -> &str {
        WAIT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Wait for running sub-agents to finish and return their reports. Give `agents` (names or ids) \
         to wait for some of them, or leave it out to wait for all. Use it when your next step needs \
         their results; otherwise keep working and their reports arrive on their own."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Sub-agent names or ids; all running ones when omitted"
                },
                "timeout_secs": { "type": "integer", "description": "Give up after this long (default 1800)" }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(owner) = ctx.inbox.clone() else {
            return ToolResult::err("Only the main agent can wait for sub-agents.");
        };
        let wanted: Vec<String> = args["agents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let matches = |id: &str, name: &str| {
            wanted.is_empty()
                || wanted
                    .iter()
                    .any(|w| w == id || w.eq_ignore_ascii_case(name))
        };
        let ids: Option<Vec<String>> = (!wanted.is_empty()).then(|| {
            running()
                .iter()
                .filter(|(id, h)| owned(&h.owner, &owner) && matches(id, &h.name))
                .map(|(id, _)| id.clone())
                .collect()
        });
        let timeout =
            std::time::Duration::from_secs(args["timeout_secs"].as_u64().unwrap_or(1800).max(1));
        let timed_out = tokio::time::timeout(timeout, wait_until_done(&owner, ids.as_deref()))
            .await
            .is_err();

        let (mine, rest): (Vec<_>, Vec<_>) = take_reports(&owner)
            .into_iter()
            .partition(|r| matches(&r.id, &r.name));
        // Reports nobody asked for stay queued for the main agent.
        reports().extend(rest);

        let mut out: Vec<String> = mine.iter().map(Report::note).collect();
        let still: Vec<String> = running()
            .values()
            .filter(|h| owned(&h.owner, &owner))
            .map(|h| h.name.clone())
            .collect();
        if timed_out && !still.is_empty() {
            out.push(format!("Timed out; still running: {}", still.join(", ")));
        }
        if out.is_empty() {
            out.push("No sub-agent reports to collect (none running, or all already read).".into());
        }
        ToolResult::ok(out.join("\n\n"))
    }
}

/// Deliver `text` from `from` to the agent called `to` ("main", or a running
/// sub-agent's name or id). Returns the recipient's name.
pub fn deliver(from: &str, to: &str, text: &str) -> Result<String, String> {
    let note = if from == "user" {
        format!("[Message from the user] {text}")
    } else {
        format!("[Message from agent \"{from}\"] {text}")
    };
    let target = to.trim();
    if target.eq_ignore_ascii_case("main") {
        let inbox = MAIN_INBOX.lock().unwrap_or_else(|p| p.into_inner()).clone();
        return match inbox {
            Some(tx) if tx.send(note).is_ok() => Ok("main".into()),
            _ => Err("The main agent can't receive messages right now.".into()),
        };
    }
    let agents = running();
    let found = agents
        .iter()
        .find(|(id, h)| id.as_str() == target || h.name.eq_ignore_ascii_case(target));
    match found {
        Some((_, h)) if h.inbox.send(note).is_ok() => Ok(h.name.clone()),
        Some((_, h)) => Err(format!("Sub-agent \"{}\" has already finished.", h.name)),
        None => {
            let names: Vec<&str> = agents.values().map(|h| h.name.as_str()).collect();
            Err(format!(
                "No running agent called \"{target}\". Running: main{}{}",
                if names.is_empty() { "" } else { ", " },
                names.join(", ")
            ))
        }
    }
}

pub struct MessageAgentTool;

#[async_trait]
impl Tool for MessageAgentTool {
    fn name(&self) -> &str {
        MESSAGE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Send a note to another running agent: \"main\" (the agent that started the sub-agents) or a \
         sub-agent by name. It reads the note at its next step; replies come back the same way, as a \
         \"[Message from agent ...]\" note. Use it to share findings, agree who edits which file, hand \
         over results early, or (as a sub-agent) ask the main agent something. Keep notes short."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "\"main\" or a sub-agent's name" },
                "message": { "type": "string", "description": "The note" }
            },
            "required": ["to", "message"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let to = args["to"].as_str().unwrap_or("").trim();
        let text = args["message"].as_str().unwrap_or("").trim();
        if to.is_empty() || text.is_empty() {
            return ToolResult::err("Give both 'to' and 'message'.");
        }
        let from = ctx.agent_name.clone().unwrap_or_else(|| "main".into());
        if to.eq_ignore_ascii_case(&from) {
            return ToolResult::err("That is you; pick another agent.");
        }
        match deliver(&from, to, text) {
            Ok(recipient) => {
                if let Some(tx) = &ctx.events {
                    let _ = tx.send(AgentEvent::AgentMessage {
                        from,
                        to: recipient.clone(),
                        text: text.to_string(),
                    });
                }
                ToolResult::ok(format!(
                    "Delivered to {recipient}; it reads it at its next step."
                ))
            }
            Err(e) => ToolResult::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_reach_running_agents_by_name() {
        let (inbox, mut rx) = mpsc::unbounded_channel();
        let (approval_tx, _a) = mpsc::unbounded_channel();
        let (cancel_tx, _c) = mpsc::unbounded_channel();
        running().insert(
            "agent-test-msg".into(),
            Handle {
                name: "Docs".into(),
                approval_tx,
                cancel_tx,
                inbox,
                owner: None,
            },
        );
        assert_eq!(
            deliver("main", "docs", "use README.md").as_deref(),
            Ok("Docs")
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            "[Message from agent \"main\"] use README.md"
        );
        assert!(
            deliver("main", "nobody", "hi")
                .unwrap_err()
                .contains("Docs")
        );
        running().remove("agent-test-msg");
    }

    #[tokio::test]
    async fn waiting_returns_once_the_agent_reports() {
        let (inbox, _i) = mpsc::unbounded_channel();
        let (approval_tx, _a) = mpsc::unbounded_channel();
        let (cancel_tx, _c) = mpsc::unbounded_channel();
        let (owner, _o) = mpsc::unbounded_channel();
        let id = "agent-test-wait".to_string();
        running().insert(
            id.clone(),
            Handle {
                name: "Waiter".into(),
                approval_tx,
                cancel_tx,
                inbox,
                owner: Some(owner.clone()),
            },
        );
        assert_eq!(running_count(&owner), 1);
        let finisher = {
            let id = id.clone();
            let owner = owner.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                reports().push(Report {
                    id: id.clone(),
                    name: "Waiter".into(),
                    success: true,
                    text: "all done".into(),
                    owner: Some(owner),
                });
                drop(Registration {
                    id,
                    parent: None,
                    finished: true,
                });
            })
        };
        let ids = [id.clone()];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_until_done(&owner, Some(&ids)),
        )
        .await
        .expect("the wait ends when the sub-agent does");
        finisher.await.unwrap();
        assert!(outstanding(&owner));
        let taken = take_reports(&owner);
        assert_eq!(taken.len(), 1);
        assert!(!outstanding(&owner));
        assert!(taken[0].note().contains("\"Waiter\" finished"));
        assert!(taken[0].note().contains("all done"));
    }

    #[test]
    fn controls_ignore_unknown_agents() {
        respond_approval("agent-missing", true);
        cancel("agent-missing");
        assert!(list_running().iter().all(|(id, _)| id != "agent-missing"));
    }
}
