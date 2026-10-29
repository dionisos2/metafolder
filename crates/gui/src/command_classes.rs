//! The class of each command (doc "Command classes"): how often it is used,
//! which decides what key it may have (doc "Choosing keybindings") — read from
//! `gui/command-classes.toml`, a file of its own so that `keybindings.toml`
//! stays a list of keys.
//!
//! A key of the file is an invocation or its first words: `"log:revert"`
//! classes every revert, `"log:revert op"` that one only, and the longest key
//! that fits an invocation wins.

use serde::Serialize;
use std::collections::BTreeMap;

/// How often a command is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Class {
    /// All the time: one key.
    A,
    /// Several times a session: one modifier, or two keys.
    B,
    /// Now and then: two or three keys.
    C,
    /// Rarely: the command input only.
    D,
    /// Cannot be undone: never a key.
    X,
}

impl Class {
    fn parse(text: &str) -> Option<Class> {
        match text {
            "A" => Some(Class::A),
            "B" => Some(Class::B),
            "C" => Some(Class::C),
            "D" => Some(Class::D),
            "X" => Some(Class::X),
            _ => None,
        }
    }

    /// Whether a command of this class may have a key at all.
    pub fn may_be_bound(self) -> bool {
        !matches!(self, Class::D | Class::X)
    }
}

/// The classes of `gui/command-classes.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandClasses {
    /// Invocation prefix (its words joined by one space) → class.
    by_prefix: BTreeMap<String, Class>,
}

/// An invocation's words joined by one space.
fn words(invocation: &str) -> String {
    invocation.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl CommandClasses {
    pub fn parse(source: &str) -> Result<Self, String> {
        let table: toml::Table =
            toml::from_str(source).map_err(|e| format!("invalid command-classes.toml: {e}"))?;
        let mut by_prefix = BTreeMap::new();
        for (key, value) in table {
            let class = value.as_str().and_then(Class::parse).ok_or_else(|| {
                format!("invalid command-classes.toml: \"{key}\" must be one of A, B, C, D, X")
            })?;
            let prefix = words(&key);
            if prefix.is_empty() {
                return Err("invalid command-classes.toml: an empty command".into());
            }
            if by_prefix.insert(prefix.clone(), class).is_some() {
                return Err(format!("invalid command-classes.toml: \"{prefix}\" is given twice"));
            }
        }
        Ok(CommandClasses { by_prefix })
    }

    /// The class of `invocation`: that of the longest key made of its first
    /// words, `None` when no key is.
    pub fn class_of(&self, invocation: &str) -> Option<Class> {
        let words: Vec<&str> = invocation.split_whitespace().collect();
        (1..=words.len()).rev().find_map(|n| self.by_prefix.get(&words[..n].join(" ")).copied())
    }

    /// Every key of the file with its class, sorted.
    pub fn entries(&self) -> impl Iterator<Item = (&str, Class)> {
        self.by_prefix.iter().map(|(k, c)| (k.as_str(), *c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_longest_prefix_of_whole_words_wins() {
        let classes = CommandClasses::parse(
            "\"log:revert\" = \"B\"\n\"log:revert op\" = \"C\"\n\"log:prune\" = \"X\"\n",
        )
        .unwrap();
        assert_eq!(classes.class_of("log:revert"), Some(Class::B));
        assert_eq!(classes.class_of("log:revert label"), Some(Class::B));
        assert_eq!(classes.class_of("log:revert  op"), Some(Class::C));
        assert_eq!(classes.class_of("log:revert op 12"), Some(Class::C));
        // Words, not characters: `log:revert opx` is not `log:revert op`.
        assert_eq!(classes.class_of("log:revert opx"), Some(Class::B));
        assert_eq!(classes.class_of("log:revertx"), None);
        assert_eq!(classes.class_of("log:prune"), Some(Class::X));
        assert!(!Class::X.may_be_bound() && !Class::D.may_be_bound() && Class::C.may_be_bound());
    }

    #[test]
    fn test_a_malformed_file_is_refused_with_the_key_at_fault() {
        let error = CommandClasses::parse("\"quit\" = \"E\"\n").unwrap_err();
        assert!(error.contains("\"quit\""), "{error}");
        assert!(CommandClasses::parse("\"quit\" = 1\n").is_err());
        assert!(CommandClasses::parse("quit = [").is_err());
        // Two spellings of one prefix.
        let error = CommandClasses::parse("\"a:b c\" = \"A\"\n\"a:b  c\" = \"B\"\n").unwrap_err();
        assert!(error.contains("twice"), "{error}");
    }

    /// The shipped file classes every shipped command, and the shipped keys
    /// follow the classes (doc "Choosing keybindings"): a class D or X command
    /// has no key, a class A one has a key of one chord, and a single letter
    /// is spent on class A only.
    #[test]
    fn test_the_shipped_classes_cover_every_command_and_fit_the_keys() {
        let classes =
            CommandClasses::parse(include_str!("../default-config/command-classes.toml")).unwrap();
        // A command of the user's own module is the user's to class.
        let unclassed: Vec<String> = crate::doc_gen::commands()
            .into_iter()
            .filter(|c| c.owner != crate::doc_gen::Owner::UserCommands("user-commands.js"))
            .filter(|c| classes.class_of(&c.name).is_none())
            .map(|c| c.name)
            .collect();
        assert!(unclassed.is_empty(), "commands with no class: {unclassed:?}");

        let table = crate::doc_gen::default_bindings();
        let mut wrong = Vec::new();
        for binding in &table {
            let class = classes.class_of(&binding.invocation);
            if class.is_some_and(|c| !c.may_be_bound()) {
                wrong.push(format!(
                    "{} has a key ({}): class {class:?}",
                    binding.invocation,
                    binding.keys.join(" ")
                ));
            }
            // A letter typed alone, without shift; shift+x is a variant, of
            // whatever class it has.
            let key = binding.keys[0].as_str();
            let letter = binding.keys.len() == 1
                && key.len() == 1
                && key.chars().all(|c| c.is_ascii_lowercase());
            if letter && class != Some(Class::A) {
                wrong.push(format!(
                    "{} is a letter alone for {}: class {class:?}",
                    binding.keys[0], binding.invocation
                ));
            }
        }
        // Each key naming a class A invocation exactly has a one-chord binding.
        for (prefix, class) in classes.entries() {
            let bindings: Vec<_> = table
                .iter()
                .filter(|b| {
                    b.invocation == prefix || b.invocation.starts_with(&format!("{prefix} "))
                })
                .filter(|b| classes.class_of(&b.invocation) == Some(class))
                .collect();
            if class == Class::A
                && !bindings.is_empty()
                && bindings.iter().all(|b| b.keys.len() > 1)
            {
                wrong.push(format!("{prefix} is class A but has no key of one chord"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
