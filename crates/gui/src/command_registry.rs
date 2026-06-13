//! Command registry (spec-gui "Command"): named operations registered by
//! the shell (builtin) or by panel types, listed by the command input
//! autocomplete. Every registered command is listed and invocable
//! regardless of which panel is focused — invocations are dispatched to
//! the owning panel type, so acting on an unfocused (or hidden) panel is
//! legitimate; keybindings scope with `when` where focus matters.

use metafolder_core::sync::MutexExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommandDef {
    pub name: String,
    pub label: String,
    /// Panel type that registered the command; `None` for builtins.
    pub owner: Option<String>,
    /// Whether invoking the command should reveal the owning panel type
    /// when it is not displayed (spec-gui open question, resolved).
    pub reveal: bool,
}

#[derive(Default)]
pub struct CommandRegistry {
    commands: Mutex<HashMap<String, CommandDef>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, def: CommandDef) {
        self.commands
            .lock_recover()
            .insert(def.name.clone(), def);
    }

    /// Registers a shell builtin.
    pub fn register_builtin(&self, name: &str, label: &str) {
        self.insert(CommandDef {
            name: name.to_string(),
            label: label.to_string(),
            owner: None,
            reveal: false,
        });
    }

    /// Registers a command from a panel type. Re-registering the same
    /// name replaces the previous definition (panels re-register on
    /// iframe reload).
    pub fn register_panel(&self, panel_type: &str, name: &str, label: &str, reveal: bool) {
        self.insert(CommandDef {
            name: name.to_string(),
            label: label.to_string(),
            owner: Some(panel_type.to_string()),
            reveal,
        });
    }

    pub fn get(&self, name: &str) -> Option<CommandDef> {
        self.commands
            .lock_recover()
            .get(name)
            .cloned()
    }

    /// Autocomplete listing: every registered command, sorted by name
    /// (the fuzzy filter narrows it down; execution routes to the owner).
    pub fn list(&self) -> Vec<CommandDef> {
        let commands = self.commands.lock_recover();
        let mut listed: Vec<CommandDef> = commands.values().cloned().collect();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_is_always_listed() {
        let registry = CommandRegistry::new();
        registry.register_builtin("tab:new", "New workspace tab");

        let def = registry.get("tab:new").unwrap();
        assert_eq!(def.owner, None);
        assert!(!def.reveal);
        assert!(registry.list().iter().any(|c| c.name == "tab:new"));
    }

    #[test]
    fn test_panel_commands_are_listed_regardless_of_focus() {
        // Commands are dispatched to their owning panel type, so a panel's
        // command is invocable (and listed) even when another panel is
        // focused or the owner is not displayed at all.
        let registry = CommandRegistry::new();
        registry.register_panel("metarecord-list", "metarecord-list:next", "Next entry", false);

        let def = registry.get("metarecord-list:next").unwrap();
        assert_eq!(def.owner.as_deref(), Some("metarecord-list"));
        assert!(registry.list().iter().any(|c| c.name == "metarecord-list:next"));
    }

    #[test]
    fn test_reregistration_replaces() {
        let registry = CommandRegistry::new();
        registry.register_panel("p", "p:cmd", "First", false);
        registry.register_panel("p", "p:cmd", "Second", true);

        let def = registry.get("p:cmd").unwrap();
        assert_eq!(def.label, "Second");
        assert!(def.reveal);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn test_list_is_sorted_by_name() {
        let registry = CommandRegistry::new();
        registry.register_builtin("tab:new", "b");
        registry.register_panel("p", "panel:split", "a", false);
        registry.register_builtin("quit", "c");

        let names: Vec<String> = registry.list().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["panel:split", "quit", "tab:new"]);
    }

    #[test]
    fn test_get_unknown_returns_none() {
        let registry = CommandRegistry::new();
        assert!(registry.get("nope").is_none());
    }
}
