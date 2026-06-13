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
    /// Whether each invocation is echoed to the workspace message panel.
    /// Defaults to true; basic editing primitives opt out to avoid noise.
    pub log: bool,
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

    /// Registers a shell builtin. `log` controls whether invocations are
    /// echoed to the message panel (false for basic editing primitives).
    pub fn register_builtin(&self, name: &str, label: &str, log: bool) {
        self.insert(CommandDef {
            name: name.to_string(),
            label: label.to_string(),
            owner: None,
            reveal: false,
            log,
        });
    }

    /// Registers a command from a panel type. Re-registering the same
    /// name replaces the previous definition (panels re-register on
    /// iframe reload).
    pub fn register_panel(&self, panel_type: &str, name: &str, label: &str, reveal: bool, log: bool) {
        self.insert(CommandDef {
            name: name.to_string(),
            label: label.to_string(),
            owner: Some(panel_type.to_string()),
            reveal,
            log,
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
        registry.register_builtin("tab:new", "New workspace tab", true);

        let def = registry.get("tab:new").unwrap();
        assert_eq!(def.owner, None);
        assert!(!def.reveal);
        assert!(def.log);
        assert!(registry.list().iter().any(|c| c.name == "tab:new"));
    }

    #[test]
    fn test_log_flag_records_command_visibility() {
        // The `log` flag controls whether an invocation is echoed to the
        // message panel; basic editing primitives opt out.
        let registry = CommandRegistry::new();
        registry.register_builtin("editing:confirm", "Confirm", false);
        registry.register_panel("p", "p:open", "Open", false, true);

        assert!(!registry.get("editing:confirm").unwrap().log);
        assert!(registry.get("p:open").unwrap().log);
    }

    #[test]
    fn test_panel_commands_are_listed_regardless_of_focus() {
        // Commands are dispatched to their owning panel type, so a panel's
        // command is invocable (and listed) even when another panel is
        // focused or the owner is not displayed at all.
        let registry = CommandRegistry::new();
        registry.register_panel("metarecord-list", "metarecord-list:next", "Next entry", false, true);

        let def = registry.get("metarecord-list:next").unwrap();
        assert_eq!(def.owner.as_deref(), Some("metarecord-list"));
        assert!(registry.list().iter().any(|c| c.name == "metarecord-list:next"));
    }

    #[test]
    fn test_reregistration_replaces() {
        let registry = CommandRegistry::new();
        registry.register_panel("p", "p:cmd", "First", false, true);
        registry.register_panel("p", "p:cmd", "Second", true, true);

        let def = registry.get("p:cmd").unwrap();
        assert_eq!(def.label, "Second");
        assert!(def.reveal);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn test_list_is_sorted_by_name() {
        let registry = CommandRegistry::new();
        registry.register_builtin("tab:new", "b", true);
        registry.register_panel("p", "panel:split", "a", false, true);
        registry.register_builtin("quit", "c", true);

        let names: Vec<String> = registry.list().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["panel:split", "quit", "tab:new"]);
    }

    #[test]
    fn test_get_unknown_returns_none() {
        let registry = CommandRegistry::new();
        assert!(registry.get("nope").is_none());
    }
}
