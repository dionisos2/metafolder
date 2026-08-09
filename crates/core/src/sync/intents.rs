//! The `mf sync plan` intents file (spec-sync "The intents file"): a TOML
//! document declaring the sync **scope** (directional link queries) and a few
//! policies (ordered conflict rules + batch/threshold settings). Parsing is
//! pure and offline — no daemon involvement.

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
    /// Ordered conflict rules; first match wins (spec-sync "Conflict resolution").
    #[serde(default)]
    pub conflict: Vec<ConflictRule>,
    /// Optional tuning.
    #[serde(default)]
    pub settings: Settings,
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

/// A resolved conflict policy (spec-sync "Conflict resolution").
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

/// Optional `[settings]` (spec-sync "The intents file").
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Links per `POST …/links/commit` call.
    #[serde(rename = "commit-batch-size", default = "default_commit_batch")]
    pub commit_batch_size: usize,
    /// File transfers between metadata batches.
    #[serde(rename = "transfer-batch-size", default = "default_transfer_batch")]
    pub transfer_batch_size: usize,
    /// Enable similar-record matching in the linking phase at this minimum
    /// score; absent means exact matches only.
    #[serde(rename = "similarity-threshold", default)]
    pub similarity_threshold: Option<f64>,
}

fn default_commit_batch() -> usize {
    50
}

fn default_transfer_batch() -> usize {
    20
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            commit_batch_size: default_commit_batch(),
            transfer_batch_size: default_transfer_batch(),
            similarity_threshold: None,
        }
    }
}

/// Parses and validates the intents TOML. Rejects an empty scope (nothing to
/// sync), an unknown key, and an invalid conflict policy.
pub fn parse_intents(text: &str) -> Result<Intents, CliError> {
    let intents: Intents = toml::from_str(text)
        .map_err(|e| CliError::Usage(format!("invalid intents file: {e}")))?;
    if intents.scope.is_empty() {
        return Err(CliError::Usage(
            "the intents file declares no [[intents]] scope entry".into(),
        ));
    }
    if intents.settings.similarity_threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
        return Err(CliError::Usage("similarity-threshold must be in [0, 1]".into()));
    }
    // Validate every policy eagerly so a typo fails at parse time, not mid-run.
    for rule in &intents.conflict {
        rule.parsed_policy()?;
    }
    Ok(intents)
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

        [settings]
        commit-batch-size    = 25
        transfer-batch-size  = 10
        similarity-threshold = 0.7
    "#;

    #[test]
    fn parses_scope_conflicts_and_settings() {
        let it = parse_intents(FULL).unwrap();
        assert_eq!(it.scope.len(), 2);
        assert_eq!(it.scope[0].repo, "laptop");
        assert!(!it.scope[0].simplified);
        assert!(it.scope[1].simplified);

        assert_eq!(it.conflict.len(), 3);
        assert_eq!(it.conflict[0].field.as_deref(), Some("rating"));
        assert_eq!(it.conflict[0].parsed_policy().unwrap(), Policy::Prefer("laptop".into()));
        assert_eq!(it.conflict[2].parsed_policy().unwrap(), Policy::Ask);

        assert_eq!(it.settings.commit_batch_size, 25);
        assert_eq!(it.settings.transfer_batch_size, 10);
        assert_eq!(it.settings.similarity_threshold, Some(0.7));
    }

    #[test]
    fn settings_default_when_absent() {
        let it = parse_intents("[[intents]]\nrepo='a'\nquery='x'\n").unwrap();
        assert_eq!(it.settings.commit_batch_size, 50);
        assert_eq!(it.settings.transfer_batch_size, 20);
        assert_eq!(it.settings.similarity_threshold, None);
        assert!(it.conflict.is_empty());
    }

    #[test]
    fn empty_scope_is_rejected() {
        let err = parse_intents("[[conflict]]\npolicy='ask'\n").unwrap_err();
        assert!(err.message().contains("no [[intents]]"), "{}", err.message());
    }

    #[test]
    fn invalid_policy_is_rejected() {
        let err =
            parse_intents("[[intents]]\nrepo='a'\nquery='x'\n[[conflict]]\npolicy='keep'\n")
                .unwrap_err();
        assert!(err.message().contains("invalid conflict policy"), "{}", err.message());
    }

    #[test]
    fn prefer_requires_a_repo() {
        assert!(parse_policy("prefer:").is_err());
        assert_eq!(parse_policy("prefer:nas").unwrap(), Policy::Prefer("nas".into()));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse_intents("[[intents]]\nrepo='a'\nquery='x'\nmethod='copy'\n").unwrap_err();
        assert!(err.message().contains("invalid intents file"), "{}", err.message());
    }
}
