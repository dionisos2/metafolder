//! The `mf sync plan` intents file (doc "The intents file"): a TOML document
//! declaring the sync **scope** (directional link queries) and its policies
//! (ordered conflict rules). Parsing is pure and offline — no daemon
//! involvement.

use serde::Deserialize;

use super::SyncError as CliError;

/// A parsed intents file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intents {
    /// The scope: each entry contributes the records its query selects (on its
    /// source repo) to the set that must be linked. Their union is the scope.
    #[serde(default, rename = "intents")]
    pub scope: Vec<Intent>,
    /// Ordered conflict rules; first match wins (doc "Sync conflicts").
    #[serde(default)]
    pub conflict: Vec<ConflictRule>,
}

/// One directional scope entry: `query` runs on `repo` (one of the pair) only.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    /// Source repo (name or UUID; one of the pair).
    pub repo: String,
    /// The selection query (normal DSL, or simplified when `simplified = true`).
    pub query: String,
    /// Expand `query` with the simplified grammar before use.
    #[serde(default)]
    pub simplified: bool,
}

/// One conflict rule. A rule matches a conflicting `(record, field)` when its
/// `field` (if given) equals the field name and its `query` (if given, tested
/// against either endpoint) matches; a rule with neither is the default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictRule {
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    pub policy: String,
}

/// A resolved conflict policy (doc "Sync conflicts").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Interactive prompt at plan time.
    Ask,
    /// Leave both sides untouched.
    Skip,
    /// Take the value from the named repo (name or UUID).
    Prefer(String),
}

impl ConflictRule {
    /// The rule's policy, validated.
    pub fn parsed_policy(&self) -> Result<Policy, CliError> {
        parse_policy(&self.policy)
    }
}

/// Parses a policy string: `ask`, `skip`, or `prefer:<repo>`.
pub fn parse_policy(s: &str) -> Result<Policy, CliError> {
    match s {
        "ask" => Ok(Policy::Ask),
        "skip" => Ok(Policy::Skip),
        other => match other.strip_prefix("prefer:") {
            Some(repo) if !repo.is_empty() => Ok(Policy::Prefer(repo.to_string())),
            _ => Err(CliError::Usage(format!(
                "invalid conflict policy '{s}': expected 'ask', 'skip', or 'prefer:<repo>'"
            ))),
        },
    }
}

/// Parses and validates the intents TOML. Rejects an empty scope (nothing to
/// sync), an unknown key, the removed `[settings]`, and an invalid conflict
/// policy.
pub fn parse_intents(text: &str) -> Result<Intents, CliError> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| CliError::Usage(format!("invalid intents file: {e}")))?;
    if let Some(settings) = table.get("settings") {
        return Err(removed_settings(settings));
    }
    let intents: Intents =
        toml::from_str(text).map_err(|e| CliError::Usage(format!("invalid intents file: {e}")))?;
    if intents.scope.is_empty() {
        return Err(CliError::Usage("the intents file declares no [[intents]] scope entry".into()));
    }
    // Validate every policy eagerly so a typo fails at parse time, not mid-run.
    for rule in &intents.conflict {
        rule.parsed_policy()?;
    }
    Ok(intents)
}

/// `[settings]` held the batch sizes of a run, which now commits each link on
/// its own (doc "Why a run commits each link on its own"): a file that still
/// has the section is told what to remove, key by key.
fn removed_settings(settings: &toml::Value) -> CliError {
    let keys: Vec<&str> = match settings.as_table() {
        Some(t) => t.keys().map(String::as_str).collect(),
        None => Vec::new(),
    };
    CliError::Usage(format!(
        "invalid intents file: [settings] was removed (a run commits each link on its own); \
         delete the section{}",
        if keys.is_empty() { String::new() } else { format!(" and its keys: {}", keys.join(", ")) }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
        [[intents]]
        repo  = "laptop"
        query = 'tag = "sync_pc_b"'

        [[intents]]
        repo       = "laptop"
        query      = 'projects'
        simplified = true

        [[conflict]]
        field  = "rating"
        policy = "prefer:laptop"

        [[conflict]]
        query  = 'mfr_path ->* "/photos"'
        policy = "prefer:nas"

        [[conflict]]
        policy = "ask"
    "#;

    #[test]
    fn parses_scope_and_conflicts() {
        let it = parse_intents(FULL).unwrap();
        assert_eq!(it.scope.len(), 2);
        assert_eq!(it.scope[0].repo, "laptop");
        assert!(!it.scope[0].simplified);
        assert!(it.scope[1].simplified);

        assert_eq!(it.conflict.len(), 3);
        assert_eq!(it.conflict[0].field.as_deref(), Some("rating"));
        assert_eq!(it.conflict[0].parsed_policy().unwrap(), Policy::Prefer("laptop".into()));
        assert_eq!(it.conflict[2].parsed_policy().unwrap(), Policy::Ask);
    }

    #[test]
    fn empty_scope_is_rejected() {
        let err = parse_intents("[[conflict]]\npolicy='ask'\n").unwrap_err();
        assert!(err.message().contains("no [[intents]]"), "{}", err.message());
    }

    #[test]
    fn invalid_policy_is_rejected() {
        let err = parse_intents("[[intents]]\nrepo='a'\nquery='x'\n[[conflict]]\npolicy='keep'\n")
            .unwrap_err();
        assert!(err.message().contains("invalid conflict policy"), "{}", err.message());
    }

    #[test]
    fn prefer_requires_a_repo() {
        assert!(parse_policy("prefer:").is_err());
        assert_eq!(parse_policy("prefer:nas").unwrap(), Policy::Prefer("nas".into()));
    }

    /// Matching is by path, exact: there is no similarity to tune, and a file
    /// still asking for one is told so rather than silently ignored.
    #[test]
    fn a_similarity_threshold_is_refused() {
        let err = parse_intents(
            "[[intents]]\nrepo='a'\nquery='x'\n[settings]\nsimilarity-threshold = 0.7\n",
        )
        .unwrap_err();
        assert!(err.message().contains("similarity-threshold"), "{}", err.message());
    }

    /// A run commits each link on its own: the batch sizes are gone, and a file
    /// still setting one is told so by name rather than refused as a mere typo.
    #[test]
    fn a_batch_size_is_refused_as_removed() {
        for key in ["commit-batch-size", "transfer-batch-size"] {
            let err = parse_intents(&format!(
                "[[intents]]\nrepo='a'\nquery='x'\n[settings]\n{key} = 50\n"
            ))
            .unwrap_err();
            let message = err.message();
            assert!(message.contains(key), "{message}");
            assert!(message.contains("removed"), "{message}");
        }
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse_intents("[[intents]]\nrepo='a'\nquery='x'\nmethod='copy'\n").unwrap_err();
        assert!(err.message().contains("invalid intents file"), "{}", err.message());
    }
}
