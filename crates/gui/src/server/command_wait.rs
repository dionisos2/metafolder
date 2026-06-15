//! The command-dispatch wait registry shared by `POST /gui/command`
//! (spec-gui "Scripting / GUI API"): an external invocation is run by the
//! frontend's own `dispatch()` — the exact same path as the command input and
//! keybindings — and its outcome is reported back through the `command_done`
//! Tauri command. Unlike the input/prompt wait, several command waits may be
//! in flight at once, so each is keyed by a generated id.

use metafolder_core::sync::MutexExt;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Debug, PartialEq)]
pub enum CommandOutcome {
    /// The frontend's `dispatch()` completed without error.
    Ok,
    /// `dispatch()` reported a failure (e.g. unknown command).
    Error(String),
    /// GUI teardown before the command resolved.
    Closed,
}

#[derive(Default)]
pub struct CommandWait {
    pending: Mutex<HashMap<Uuid, oneshot::Sender<CommandOutcome>>>,
}

impl CommandWait {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new command wait, returning its id (forwarded to the
    /// frontend) and the receiver the HTTP handler awaits.
    pub fn begin(&self) -> (Uuid, oneshot::Receiver<CommandOutcome>) {
        let id = Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        self.pending.lock_recover().insert(id, sender);
        (id, receiver)
    }

    /// Resolves the wait with the given id. Returns false when no wait is
    /// registered under that id (already resolved, timed out, or unknown).
    pub fn resolve(&self, id: Uuid, outcome: CommandOutcome) -> bool {
        match self.pending.lock_recover().remove(&id) {
            Some(sender) => {
                let _ = sender.send(outcome);
                true
            }
            None => false,
        }
    }

    /// Discards a wait without sending (after a timeout).
    pub fn end(&self, id: Uuid) {
        self.pending.lock_recover().remove(&id);
    }

    /// GUI teardown: every pending waiter receives "closed".
    pub fn close_all(&self) {
        for (_, sender) in self.pending.lock_recover().drain() {
            let _ = sender.send(CommandOutcome::Closed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_matches_the_right_wait() {
        let wait = CommandWait::new();
        let (id_a, mut rx_a) = wait.begin();
        let (id_b, mut rx_b) = wait.begin();
        assert_ne!(id_a, id_b);

        assert!(wait.resolve(id_a, CommandOutcome::Ok));
        assert_eq!(rx_a.try_recv().unwrap(), CommandOutcome::Ok);
        // The other wait is untouched.
        assert!(rx_b.try_recv().is_err());

        assert!(wait.resolve(id_b, CommandOutcome::Error("boom".into())));
        assert_eq!(rx_b.try_recv().unwrap(), CommandOutcome::Error("boom".into()));
    }

    #[test]
    fn test_resolve_unknown_id_returns_false() {
        let wait = CommandWait::new();
        assert!(!wait.resolve(Uuid::new_v4(), CommandOutcome::Ok));
    }

    #[test]
    fn test_close_all_sends_closed_to_every_waiter() {
        let wait = CommandWait::new();
        let (_, mut rx) = wait.begin();
        wait.close_all();
        assert_eq!(rx.try_recv().unwrap(), CommandOutcome::Closed);
    }

    #[test]
    fn test_end_discards_without_sending() {
        let wait = CommandWait::new();
        let (id, mut rx) = wait.begin();
        wait.end(id);
        assert!(rx.try_recv().is_err());
        assert!(!wait.resolve(id, CommandOutcome::Ok));
    }
}
