//! The input waits of `POST /gui/input` and `POST /gui/prompt` (doc "GUI
//! scripting endpoints"): one wait per workspace — the one it is asked in, or
//! none for a wait owned by nobody — resolved by the `answer:send` command, the
//! command input (prompt), a timeout, or GUI teardown ("closed").

use metafolder_core::sync::MutexExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::oneshot;

#[derive(Debug, PartialEq)]
pub enum InputOutcome {
    Answer(String),
    Closed,
}

#[derive(Debug, PartialEq)]
pub enum PromptOutcome {
    Confirm(String),
    Cancel,
    Closed,
}

enum Active {
    Input(oneshot::Sender<InputOutcome>),
    Prompt(oneshot::Sender<PromptOutcome>),
}

struct Entry {
    id: u64,
    /// The workspace the wait belongs to; `None` = owned by nobody.
    owner: Option<String>,
    active: Active,
}

#[derive(Default)]
pub struct InputWait {
    waits: Mutex<Vec<Entry>>,
    next_id: AtomicU64,
}

impl InputWait {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any wait is up, in any workspace.
    pub fn is_active(&self) -> bool {
        !self.waits.lock_recover().is_empty()
    }

    fn begin(&self, owner: Option<String>, active: Active) -> Option<u64> {
        let mut waits = self.waits.lock_recover();
        if waits.iter().any(|w| w.owner == owner) {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        waits.push(Entry { id, owner, active });
        Some(id)
    }

    /// Registers an input wait in `owner`; `None` when that workspace already
    /// has one. Returns the wait's id with its receiver.
    pub fn begin_input(
        &self,
        owner: Option<String>,
    ) -> Option<(u64, oneshot::Receiver<InputOutcome>)> {
        let (sender, receiver) = oneshot::channel();
        self.begin(owner, Active::Input(sender)).map(|id| (id, receiver))
    }

    /// Registers a prompt wait; same rule as input waits.
    pub fn begin_prompt(
        &self,
        owner: Option<String>,
    ) -> Option<(u64, oneshot::Receiver<PromptOutcome>)> {
        let (sender, receiver) = oneshot::channel();
        self.begin(owner, Active::Prompt(sender)).map(|id| (id, receiver))
    }

    /// Takes the wait `id` (or, with no id, the first of the right kind) out
    /// of the set, when `kind` accepts it.
    fn take(&self, id: Option<u64>, kind: fn(&Active) -> bool) -> Option<Active> {
        let mut waits = self.waits.lock_recover();
        let index = waits.iter().position(|w| id.is_none_or(|id| w.id == id) && kind(&w.active))?;
        Some(waits.remove(index).active)
    }

    /// `answer:send <value>` — resolves input wait `id` (the first one, with no
    /// id). Returns false when there is no such input wait (prompts are not
    /// affected).
    pub fn resolve_answer(&self, id: Option<u64>, value: &str) -> bool {
        match self.take(id, |a| matches!(a, Active::Input(_))) {
            Some(Active::Input(sender)) => {
                let _ = sender.send(InputOutcome::Answer(value.to_string()));
                true
            }
            _ => false,
        }
    }

    /// Resolves prompt `id` (the first one, with no id): Enter → confirm with
    /// text, Escape → cancel. Returns false when there is no such prompt.
    pub fn resolve_prompt(&self, id: Option<u64>, confirm: bool, text: Option<String>) -> bool {
        match self.take(id, |a| matches!(a, Active::Prompt(_))) {
            Some(Active::Prompt(sender)) => {
                let outcome = if confirm {
                    PromptOutcome::Confirm(text.unwrap_or_default())
                } else {
                    PromptOutcome::Cancel
                };
                let _ = sender.send(outcome);
                true
            }
            _ => false,
        }
    }

    /// Forgets wait `id` without sending (a timeout, a dropped request).
    pub fn end(&self, id: u64) {
        self.waits.lock_recover().retain(|w| w.id != id);
    }

    /// GUI teardown: every waiter receives "closed".
    pub fn close_all(&self) {
        for entry in self.waits.lock_recover().drain(..) {
            match entry.active {
                Active::Input(sender) => {
                    let _ = sender.send(InputOutcome::Closed);
                }
                Active::Prompt(sender) => {
                    let _ = sender.send(PromptOutcome::Closed);
                }
            }
        }
    }

    /// Closes wait `id` with "closed" — `script:stop` on a question that has
    /// no script to signal.
    pub fn close(&self, id: u64) {
        let mut waits = self.waits.lock_recover();
        if let Some(index) = waits.iter().position(|w| w.id == id) {
            match waits.remove(index).active {
                Active::Input(sender) => {
                    let _ = sender.send(InputOutcome::Closed);
                }
                Active::Prompt(sender) => {
                    let _ = sender.send(PromptOutcome::Closed);
                }
            }
        }
    }
}
