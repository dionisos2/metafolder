//! The input-wait lock shared by `POST /gui/input` and `POST /gui/prompt`
//! (spec-gui "Scripting / GUI API"): one wait at a time, resolved by the
//! `answer:send` command, the command input (prompt), a timeout, or GUI
//! teardown ("closed").

use metafolder_core::sync::MutexExt;
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

#[derive(Default)]
pub struct InputWait {
    active: Mutex<Option<Active>>,
}

impl InputWait {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active.lock_recover().is_some()
    }

    /// Registers an input wait; `None` when another wait holds the lock.
    pub fn begin_input(&self) -> Option<oneshot::Receiver<InputOutcome>> {
        let mut active = self.active.lock_recover();
        if active.is_some() {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        *active = Some(Active::Input(sender));
        Some(receiver)
    }

    /// Registers a prompt wait; same lock as input waits.
    pub fn begin_prompt(&self) -> Option<oneshot::Receiver<PromptOutcome>> {
        let mut active = self.active.lock_recover();
        if active.is_some() {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        *active = Some(Active::Prompt(sender));
        Some(receiver)
    }

    /// `answer:send <value>` — resolves the active input wait. Returns
    /// false when no input wait is active (prompts are not affected).
    pub fn resolve_answer(&self, value: &str) -> bool {
        let mut active = self.active.lock_recover();
        match active.take() {
            Some(Active::Input(sender)) => {
                let _ = sender.send(InputOutcome::Answer(value.to_string()));
                true
            }
            other => {
                *active = other;
                false
            }
        }
    }

    /// Resolves the active prompt (Enter → confirm with text, Escape →
    /// cancel). Returns false when no prompt is active.
    pub fn resolve_prompt(&self, confirm: bool, text: Option<String>) -> bool {
        let mut active = self.active.lock_recover();
        match active.take() {
            Some(Active::Prompt(sender)) => {
                let outcome = if confirm {
                    PromptOutcome::Confirm(text.unwrap_or_default())
                } else {
                    PromptOutcome::Cancel
                };
                let _ = sender.send(outcome);
                true
            }
            other => {
                *active = other;
                false
            }
        }
    }

    /// Clears the lock without sending (after a timeout).
    pub fn end(&self) {
        self.active.lock_recover().take();
    }

    /// GUI teardown: every waiter receives "closed".
    pub fn close_all(&self) {
        match self.active.lock_recover().take() {
            Some(Active::Input(sender)) => {
                let _ = sender.send(InputOutcome::Closed);
            }
            Some(Active::Prompt(sender)) => {
                let _ = sender.send(PromptOutcome::Closed);
            }
            None => {}
        }
    }
}
