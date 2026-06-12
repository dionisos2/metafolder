//! Command registry (spec-gui "Command"): named operations registered by
//! the shell (builtin) or by panel types, listed by the command input
//! autocomplete. Builtins default to global scope; panel commands to local.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Local,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommandDef {
    pub name: String,
    pub label: String,
    pub scope: Scope,
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
            .lock()
            .expect("CommandRegistry lock poisoned")
            .insert(def.name.clone(), def);
    }

    /// Registers a shell builtin (global scope).
    pub fn register_builtin(&self, name: &str, label: &str) {
        self.insert(CommandDef {
            name: name.to_string(),
            label: label.to_string(),
            scope: Scope::Global,
            owner: None,
            reveal: false,
        });
    }

    /// Registers a command from a panel type. `scope` defaults to local.
    /// Re-registering the same name replaces the previous definition
    /// (panels re-register on iframe reload).
    pub fn register_panel(
        &self,
        panel_type: &str,
        name: &str,
        label: &str,
        scope: Option<Scope>,
        reveal: bool,
    ) {
        self.insert(CommandDef {
            name: name.to_string(),
            label: label.to_string(),
            scope: scope.unwrap_or(Scope::Local),
            owner: Some(panel_type.to_string()),
            reveal,
        });
    }

    pub fn get(&self, name: &str) -> Option<CommandDef> {
        self.commands
            .lock()
            .expect("CommandRegistry lock poisoned")
            .get(name)
            .cloned()
    }

    /// Autocomplete listing: all global commands, plus local commands of
    /// the focused panel type; sorted by name.
    pub fn list(&self, focused_panel: Option<&str>) -> Vec<CommandDef> {
        let commands = self.commands.lock().expect("CommandRegistry lock poisoned");
        let mut listed: Vec<CommandDef> = commands
            .values()
            .filter(|def| match def.scope {
                Scope::Global => true,
                Scope::Local => def.owner.as_deref() == focused_panel && focused_panel.is_some(),
            })
            .cloned()
            .collect();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_is_global_and_always_listed() {
        let registry = CommandRegistry::new();
        registry.register_builtin("tab:new", "New workspace tab");

        let def = registry.get("tab:new").unwrap();
        assert_eq!(def.scope, Scope::Global);
        assert_eq!(def.owner, None);
        assert!(!def.reveal);

        assert!(registry.list(None).iter().any(|c| c.name == "tab:new"));
        assert!(registry
            .list(Some("record-list"))
            .iter()
            .any(|c| c.name == "tab:new"));
    }

    #[test]
    fn test_panel_command_defaults_to_local_scope() {
        let registry = CommandRegistry::new();
        registry.register_panel("record-list", "record-list:next", "Next entry", None, false);

        let def = registry.get("record-list:next").unwrap();
        assert_eq!(def.scope, Scope::Local);
        assert_eq!(def.owner.as_deref(), Some("record-list"));

        // Listed only when its panel type is focused.
        assert!(registry
            .list(Some("record-list"))
            .iter()
            .any(|c| c.name == "record-list:next"));
        assert!(!registry
            .list(Some("file"))
            .iter()
            .any(|c| c.name == "record-list:next"));
        assert!(!registry.list(None).iter().any(|c| c.name == "record-list:next"));
    }

    #[test]
    fn test_panel_command_can_opt_into_global_scope() {
        let registry = CommandRegistry::new();
        registry.register_panel(
            "my-panel",
            "my-panel:global-action",
            "Global action",
            Some(Scope::Global),
            true,
        );

        let def = registry.get("my-panel:global-action").unwrap();
        assert_eq!(def.scope, Scope::Global);
        assert!(def.reveal);
        assert!(registry
            .list(Some("file"))
            .iter()
            .any(|c| c.name == "my-panel:global-action"));
    }

    #[test]
    fn test_reregistration_replaces() {
        let registry = CommandRegistry::new();
        registry.register_panel("p", "p:cmd", "First", None, false);
        registry.register_panel("p", "p:cmd", "Second", None, true);

        let def = registry.get("p:cmd").unwrap();
        assert_eq!(def.label, "Second");
        assert!(def.reveal);
        assert_eq!(registry.list(Some("p")).len(), 1);
    }

    #[test]
    fn test_list_is_sorted_by_name() {
        let registry = CommandRegistry::new();
        registry.register_builtin("tab:new", "b");
        registry.register_builtin("panel:split", "a");
        registry.register_builtin("quit", "c");

        let names: Vec<String> = registry.list(None).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["panel:split", "quit", "tab:new"]);
    }

    #[test]
    fn test_get_unknown_returns_none() {
        let registry = CommandRegistry::new();
        assert!(registry.get("nope").is_none());
    }
}
