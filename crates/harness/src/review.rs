//! Looking back over a chat once it's done, to keep what's worth keeping (the "learning loop" of
//! Hermes Agent's `background_review.py`, MIT). After a turn the app may start a review: a copy of
//! the chat gets one more message asking what in it should go to memory, and only the memory
//! tools run. Everything else stays as it was: the review is a fork in `<session>.agents/`, the
//! chat's own log never sees it.
//!
//! It asks with the chat's own model, system prompt and tools (the calls outside memory are refused,
//! not left out), so the request starts like the chat's last one and the provider's prompt cache
//! covers most of it.
use crate::agent::{Agent, Cancel, Inbox};
use crate::approval::{DenyAll, Policy};
use crate::message::{Block, Usage};
use crate::session::{Session, SessionEvent};
use crate::{Error, Result};
use serde::Serialize;
use std::sync::Arc;

/// Steps a review takes at most.
pub const REVIEW_STEPS: u32 = 8;

/// The review's message, in the user's place.
pub const REVIEW_PROMPT: &str = "\
[Background review: this message is from DevCat, not from the user. The user won't see your answer.]
The conversation above is done for now. Look back at it and decide whether anything in it is worth keeping in your memory for future sessions. If so, save it with `memory_save`; look with `memory_search` first, and update an entry that already covers it instead of adding a second one.

Worth keeping:
- facts about the user (their role, what they know, how they like to work) that they said or clearly showed;
- corrections they gave you on how to work, and approaches they confirmed, each with the reason;
- facts about the project that the code, its history and its rule files don't say (decisions, constraints, where things live, the commands that work).

Write each as a fact, not an order: \"The user prefers short answers\", not \"Always answer briefly\". A stored order gets read later as if the user had just given it.

Don't save: what the code, git history or rule files already say; a one-off request; guesses about the user's character; a hiccup (a timeout, a tool that failed once) or a claim that a tool is broken; a dead end you didn't solve; anything secret (keys, passwords, tokens, other people's personal details).

Most conversations have nothing worth saving: then save nothing and answer \"Nothing to save.\" Otherwise save at most 3 entries and answer with one short line per entry. Call no other tools.";

/// The tools a review may use: the memory's, to look and to save.
pub const REVIEW_TOOLS: &[&str] = &["memory_search", "memory_read", "memory_save"];

/// What a review did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reviewed {
    /// One line per memory entry it saved (the entry's name).
    pub saved: Vec<String>,
    pub usage: Usage,
}

/// A review ready to run: its own agent and its copy of the chat, so the chat itself is free while it
/// runs (see [`Agent::reviewer`]).
pub struct Reviewer {
    agent: Agent,
    session: Session,
}

impl Agent {
    /// A review of `session` (see the module docs), or None when this agent has no memory to save
    /// to. The copy of the chat goes in `<session>.agents/`, next to the helpers' logs.
    pub async fn reviewer(&self, session: &Session) -> Result<Option<Reviewer>> {
        if self.tools.get("memory_save").is_none() {
            return Ok(None);
        }
        let dir = session.path().with_extension("agents");
        let copy = session.fork(&dir, session.events().len(), "Background review").await?;
        let mut agent = Agent::new(self.model.clone(), self.tools.clone(), Policy::AutoEdit, Arc::new(DenyAll), self.system.clone());
        agent.context_window = self.context_window;
        agent.max_steps = REVIEW_STEPS;
        agent.inbox = Inbox::default();
        agent.only = Some(REVIEW_TOOLS.iter().map(|s| s.to_string()).collect());
        Ok(Some(Reviewer { agent, session: copy }))
    }
}

impl Reviewer {
    /// Run it: what it saved, and the tokens it took.
    pub async fn run(mut self, cancel: &Cancel) -> Result<Reviewed> {
        let from = self.session.events().len();
        let res = self.agent.turn(&mut self.session, REVIEW_PROMPT, cancel, &mut |_| {}).await;
        if let Err(e) = res {
            if matches!(e, Error::Cancelled) {
                return Err(e);
            }
            // what it saved before it stopped still counts
            if saved(&self.session, from).is_empty() {
                return Err(e);
            }
        }
        let used = |e: &SessionEvent| match e {
            SessionEvent::Assistant { usage, .. } => *usage,
            _ => Usage::default(),
        };
        let mut usage = Usage::default();
        for e in &self.session.events()[from..] {
            usage += used(e);
        }
        Ok(Reviewed { saved: saved(&self.session, from), usage })
    }
}

/// The names of the memory entries saved in `session` from event `from` on (calls that worked).
fn saved(session: &Session, from: usize) -> Vec<String> {
    let events = &session.events()[from..];
    let mut calls = Vec::new();
    for e in events {
        if let SessionEvent::Assistant { content, .. } = e {
            for b in content {
                if let Block::ToolUse { id, name, input } = b
                    && name == "memory_save"
                {
                    let n = input.get("name").and_then(serde_json::Value::as_str).unwrap_or("").trim().to_string();
                    calls.push((id.clone(), if n.is_empty() { "an entry".to_string() } else { n }));
                }
            }
        }
    }
    let ok = |id: &str| {
        events.iter().any(|e| matches!(e, SessionEvent::ToolResults { results, .. } if results.iter().any(|r| matches!(r, Block::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == id))))
    };
    calls.into_iter().filter(|(id, _)| ok(id)).map(|(_, n)| n).collect()
}
