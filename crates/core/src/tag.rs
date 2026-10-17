//! The hierarchical-tag model (doc "Hierarchical tags"): the field-name
//! convention, the pure subsumption helpers, and the rewrite one tag answer
//! makes (doc "Tag subsumption") — shared by `mf tag` and the GUI's `mf:tag`,
//! like `crate::order`, so a record is rewritten by the same rules whichever
//! front end answered.
//!
//! Convention (all names configurable via the CLI's `[tag]` config table): a
//! *tag entry* is a metarecord `mf_schema = <entry_type>` (`"tag"`) whose
//! position in the tag hierarchy is a `<path_field>` (`"path"`, a `tree_ref` or
//! a `hierarchy_ref`) — its resolved paths (`"musique/jazz"`) name the tag, and
//! an optional `label` may name it. Records reference entries through the
//! multi-map Ref fields `<positive>` / `<negative>` / `<mixed>`
//! (`tag`/`negative_tag`/`mixed_tag`). Three `bool` flags on an entry
//! (doc "Tag flags"): `<partition>` makes its children exclude one another,
//! and `<always>`/`<sparse>` only steer the classification script.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::daemon_client::{DaemonClient, DaemonError};

/// The tag-model field names (the `[tag]` config table). Defaults reproduce the
/// convention the scripts already use, so an empty config changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct TagConfig {
    /// Field name holding the entry-type marker (the schema field by default).
    pub type_field: String,
    /// `mf_schema` value marking a metarecord as a tag entry.
    pub entry_type: String,
    /// The tag entry's hierarchy field — a `TreeRef` whose resolved path is the
    /// tag's identity.
    pub path_field: String,
    /// Multi-map Ref field: the record *has* the tag.
    pub positive: String,
    /// Multi-map Ref field: the record does *not* have the tag.
    pub negative: String,
    /// Multi-map Ref field: a folder is *mixed* w.r.t. the tag.
    pub mixed: String,
    /// Bool flag: the classification adds the tag without asking.
    pub always: String,
    /// Bool flag: the entry's children exclude one another.
    pub partition: String,
    /// Bool flag: the classification picks the entry's children one by one.
    pub sparse: String,
}

impl Default for TagConfig {
    fn default() -> Self {
        TagConfig {
            type_field: "mf_schema".into(),
            entry_type: "tag".into(),
            path_field: "path".into(),
            positive: "tag".into(),
            negative: "negative_tag".into(),
            mixed: "mixed_tag".into(),
            always: "always".into(),
            partition: "partition".into(),
            sparse: "sparse".into(),
        }
    }
}

/// The parent path of a tag (`"a/b/c"` → `"a/b"`), or `None` for a top-level tag.
pub fn parent(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// Whether `a` is a strict ancestor path of `b` (`"a"` of `"a/b"`).
pub fn is_ancestor(a: &str, b: &str) -> bool {
    a != b && b.starts_with(a) && b.as_bytes().get(a.len()) == Some(&b'/')
}

/// The proper ancestor paths of `path`, deepest first (`"a/b/c"` → `["a/b","a"]`).
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = path;
    while let Some(par) = parent(p) {
        out.push(par.to_string());
        p = par;
    }
    out
}

/// The vocabulary names strictly under `path`.
pub fn descendants(path: &str, vocab: &[String]) -> Vec<String> {
    vocab.iter().filter(|n| is_ancestor(path, n)).cloned().collect()
}

/// One answer about a tag — what `mf tag <verb>` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The record has the tag.
    Add,
    /// The record has not the tag.
    Deny,
    /// The folder partly has the tag.
    Mixed,
    /// Take the positive ref back, nothing else.
    Remove,
}

impl Answer {
    /// The answer a verb names (`add`, `deny`, `mixed`, `remove`).
    pub fn parse(verb: &str) -> Option<Answer> {
        match verb {
            "add" => Some(Answer::Add),
            "deny" => Some(Answer::Deny),
            "mixed" => Some(Answer::Mixed),
            "remove" => Some(Answer::Remove),
            _ => None,
        }
    }
}

/// The flags of a tag entry (doc "Tag flags"); a flag not written is false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub always: bool,
    pub partition: bool,
    pub sparse: bool,
}

/// The tag vocabulary, loaded once per answer. A tag in a hierarchy has one
/// path per way up (doc "Hierarchy references"), and every path is listed, so
/// the string ancestors of all its paths are exactly its ancestors by every
/// parent, and the same for its descendants.
#[derive(Debug, Clone)]
pub struct Vocab {
    /// Tag path → entry uuid (hex). Several paths may name one entry.
    name2uuid: HashMap<String, String>,
    /// Entry uuid → all its paths.
    paths: HashMap<String, Vec<String>>,
    /// All tag paths (for descendant scans).
    names: Vec<String>,
    /// Entry uuid → its flags, for the entries with one set.
    flags: HashMap<String, Flags>,
    /// The value type entries are placed with: the field's own, else
    /// `hierarchy_ref` for a vocabulary not begun yet.
    path_type: String,
}

impl Vocab {
    /// Loads the vocabulary (`<type_field> = <entry_type>`): the resolved paths
    /// of every entry's `path_field`, in one `resolve-tree` round-trip (uuid →
    /// paths); an entry with no resolvable path is not part of it. The flags
    /// cost one query each, answered by the uuids of the few entries they are
    /// set on.
    pub fn load(
        client: &dyn DaemonClient,
        repo: &str,
        cfg: &TagConfig,
    ) -> Result<Vocab, DaemonError> {
        let query = json!({
            "type": "eq", "field": cfg.type_field,
            "value": {"type": "string", "value": cfg.entry_type},
        });
        let resolved = client.post(
            &format!("/repos/{repo}/query/fields/resolve-tree"),
            &json!({"query": query, "field": cfg.path_field}),
        )?;
        let fields = client.get(&format!("/repos/{repo}/fields"))?;
        let path_type = fields
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["name"].as_str() == Some(cfg.path_field.as_str()))
            .and_then(|f| f["type"].as_str())
            .filter(|t| crate::metarecord::is_position_type(t))
            .unwrap_or("hierarchy_ref")
            .to_string();
        let mut vocab = Vocab {
            name2uuid: HashMap::new(),
            paths: HashMap::new(),
            names: Vec::new(),
            flags: HashMap::new(),
            path_type,
        };
        for flag in [&cfg.always, &cfg.partition, &cfg.sparse] {
            let flagged = json!({"type": "and", "operands": [query, {
                "type": "eq", "field": flag, "value": {"type": "bool", "value": true},
            }]});
            let mut cursor: Option<String> = None;
            loop {
                let mut body = json!({"query": flagged, "limit": 500});
                if let Some(c) = &cursor {
                    body["cursor"] = json!(c);
                }
                let resp = client.post(&format!("/repos/{repo}/query"), &body)?;
                for uuid in resp["results"].as_array().into_iter().flatten() {
                    let Some(uuid) = uuid.as_str() else { continue };
                    let flags = vocab.flags.entry(uuid.to_string()).or_default();
                    if flag == &cfg.always {
                        flags.always = true;
                    } else if flag == &cfg.partition {
                        flags.partition = true;
                    } else {
                        flags.sparse = true;
                    }
                }
                match resp["next_cursor"].as_str() {
                    Some(c) => cursor = Some(c.to_string()),
                    None => break,
                }
            }
        }
        for (uuid, paths) in resolved.as_object().into_iter().flatten() {
            let paths: Vec<String> = paths
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str())
                .map(String::from)
                .collect();
            for path in &paths {
                vocab.name2uuid.insert(path.clone(), uuid.to_string());
                vocab.names.push(path.clone());
            }
            if !paths.is_empty() {
                vocab.paths.insert(uuid.to_string(), paths);
            }
        }
        Ok(vocab)
    }

    /// Every tag path with its entry uuid and flags, path-sorted (`mf tag list`).
    pub fn entries(&self) -> Vec<(String, String, Flags)> {
        let mut names = self.names.clone();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let uuid = self.name2uuid[&name].clone();
                let flags = self.flags_of(&uuid);
                (name, uuid, flags)
            })
            .collect()
    }

    /// The flags of the entry `uuid`.
    fn flags_of(&self, uuid: &str) -> Flags {
        self.flags.get(uuid).copied().unwrap_or_default()
    }

    /// The tags a partition excludes once the tag at `path` is had: for the
    /// tag and each of its ancestors, by every path, whose parent is a
    /// partition, the parent's other children and everything under them —
    /// never the tag itself nor one of its ancestors, which it implies.
    fn excluded_by_partitions(&self, path: &str) -> Vec<String> {
        let implied: Vec<&String> =
            self.with_ancestors(path).iter().filter_map(|p| self.name2uuid.get(p)).collect();
        let mut out: Vec<String> = Vec::new();
        for p in self.paths_of(path) {
            let mut node = p.as_str();
            while let Some(par) = parent(node) {
                let is_partition =
                    self.name2uuid.get(par).is_some_and(|u| self.flags_of(u).partition);
                if is_partition {
                    for sibling in &self.names {
                        if parent(sibling) != Some(par) || sibling == node {
                            continue;
                        }
                        for excluded in self.with_descendants(sibling) {
                            let uuid = self.name2uuid.get(&excluded);
                            if uuid.is_some_and(|u| implied.contains(&u)) {
                                continue;
                            }
                            if !out.contains(&excluded) {
                                out.push(excluded);
                            }
                        }
                    }
                }
                node = par;
            }
        }
        out
    }

    /// Whether a tag is named `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.name2uuid.contains_key(path)
    }

    /// The paths of the tag at `path` — all of them when it exists, else the
    /// one it is named by.
    fn paths_of(&self, path: &str) -> Vec<String> {
        self.name2uuid
            .get(path)
            .and_then(|u| self.paths.get(u))
            .cloned()
            .unwrap_or_else(|| vec![path.to_string()])
    }

    /// The tags strictly above the tag at `path`, by every parent.
    fn ancestors(&self, path: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in self.paths_of(path) {
            for a in ancestors(&p) {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
        out
    }

    /// The tags strictly below the tag at `path`, by every parent.
    fn descendants(&self, path: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in self.paths_of(path) {
            for d in descendants(&p, &self.names) {
                if !out.contains(&d) {
                    out.push(d);
                }
            }
        }
        out
    }

    /// The tag at `path` and its proper ancestors, by every parent.
    fn with_ancestors(&self, path: &str) -> Vec<String> {
        std::iter::once(path.to_string()).chain(self.ancestors(path)).collect()
    }

    /// The tag at `path` and its descendants in the vocabulary, by every parent.
    fn with_descendants(&self, path: &str) -> Vec<String> {
        std::iter::once(path.to_string()).chain(self.descendants(path)).collect()
    }
}

/// The writes one answer makes, sent as a single `POST /query/fields/batch` so
/// the answer is one write — undone by one undo, rewrites and created entries
/// included. Every op reads the selection as it stood before the batch.
struct TagBatch<'a> {
    cfg: &'a TagConfig,
    query: Json,
    ops: Vec<Json>,
}

impl<'a> TagBatch<'a> {
    fn new(cfg: &'a TagConfig, query: Json) -> Self {
        TagBatch { cfg, query, ops: Vec::new() }
    }

    /// The entry uuid for `path`, planning the creation of the tag entry (and
    /// of any missing ancestor) when the vocabulary lacks it: the parent `a/b`
    /// of `a/b/c` is ensured first, then `c` is created under it. The uuid is
    /// allocated here, so the refs of the same batch can point at it.
    fn ensure_entry(&mut self, vocab: &mut Vocab, path: &str) -> String {
        if let Some(uuid) = vocab.name2uuid.get(path) {
            return uuid.clone();
        }
        let (parent, leaf) = match parent(path) {
            Some(parent_path) => {
                (json!(self.ensure_entry(vocab, parent_path)), &path[parent_path.len() + 1..])
            }
            None => (Json::Null, path),
        };
        let uuid = Uuid::new_v4().as_simple().to_string();
        self.ops.push(json!({"op": "create", "uuid": uuid, "fields": [
            {"name": self.cfg.type_field, "value": {"type": "string", "value": self.cfg.entry_type}},
            {"name": self.cfg.path_field, "value": {"type": vocab.path_type,
                "value": {"parent": parent, "name": leaf}}},
        ]}));
        vocab.name2uuid.insert(path.to_string(), uuid.clone());
        vocab.paths.insert(uuid.clone(), vec![path.to_string()]);
        vocab.names.push(path.to_string());
        uuid
    }

    /// Plans `field:ref=<tag>` onto the selection; returns the op's index.
    fn add(&mut self, field: &str, tag_uuid: &str) -> usize {
        self.ops.push(json!({"op": "add", "query": self.query, "name": field,
            "value": {"type": "ref", "value": tag_uuid}}));
        self.ops.len() - 1
    }

    /// Plans the removal of `field:ref=<tag>` from the selection, for each tag
    /// of `paths` present in the vocabulary — once per tag, however many of
    /// its paths are given.
    fn remove(&mut self, vocab: &Vocab, field: &str, paths: &[String]) {
        let mut done: Vec<&String> = Vec::new();
        for path in paths {
            if let Some(uuid) = vocab.name2uuid.get(path) {
                if done.contains(&uuid) {
                    continue;
                }
                done.push(uuid);
                self.ops.push(json!({"op": "remove", "query": self.query, "name": field,
                    "value": {"type": "ref", "value": uuid}}));
            }
        }
    }

    /// Sends the batch; returns the `updated` count of the op at `counted`.
    fn send(
        self,
        client: &dyn DaemonClient,
        repo: &str,
        counted: usize,
    ) -> Result<u64, DaemonError> {
        let resp =
            client.post(&format!("/repos/{repo}/query/fields/batch"), &json!({"ops": self.ops}))?;
        Ok(resp["results"][counted]["updated"].as_u64().unwrap_or(0))
    }
}

/// Writes `answer` about the tag at `path` onto the records `query` (a Query IR,
/// as JSON) selects, rewriting them as doc "Tag subsumption" says — in one
/// write. Returns how many records gained the answer (lost the tag, for
/// `Remove`). The tag entry, and any missing ancestor, is created by an answer
/// other than `Remove`, which writes nothing for a tag that does not exist.
///
/// * `Add` drops the positives on the tag's ancestors and on what a partition
///   excludes, and the negatives and mixed marks on the tag and its ancestors;
/// * `Deny` drops the negatives on its descendants, and the positives and mixed
///   marks on the tag and its descendants;
/// * `Mixed` drops the positives on the tag and its descendants, and the
///   negatives on the tag and its ancestors.
///
/// Adding a ref a record already carries is a no-op of the daemon (doc "No
/// duplicate rows"), so answering twice leaves the rows, ids included, as they
/// were.
pub fn apply(
    client: &dyn DaemonClient,
    repo: &str,
    cfg: &TagConfig,
    query: &Json,
    answer: Answer,
    path: &str,
) -> Result<u64, DaemonError> {
    let mut vocab = Vocab::load(client, repo, cfg)?;
    let mut batch = TagBatch::new(cfg, query.clone());
    let counted = match answer {
        Answer::Remove => {
            if !vocab.contains(path) {
                return Ok(0);
            }
            batch.remove(&vocab, &cfg.positive, &[path.to_string()]);
            0
        }
        Answer::Add => {
            let uuid = batch.ensure_entry(&mut vocab, path);
            let counted = batch.add(&cfg.positive, &uuid);
            batch.remove(&vocab, &cfg.positive, &vocab.ancestors(path));
            // Under a partition, having the tag is not having its siblings.
            batch.remove(&vocab, &cfg.positive, &vocab.excluded_by_partitions(path));
            // Having the tag means wholly having every ancestor: a negative on
            // any of them, or on the tag itself, is now false, and so is a
            // mixed mark.
            let contradicted = vocab.with_ancestors(path);
            batch.remove(&vocab, &cfg.negative, &contradicted);
            batch.remove(&vocab, &cfg.mixed, &contradicted);
            counted
        }
        Answer::Deny => {
            let uuid = batch.ensure_entry(&mut vocab, path);
            let counted = batch.add(&cfg.negative, &uuid);
            batch.remove(&vocab, &cfg.negative, &vocab.descendants(path));
            // Not having the tag means having none of its descendants: a
            // positive on any of them, or on the tag itself, is now false, and
            // so is a mixed mark.
            let contradicted = vocab.with_descendants(path);
            batch.remove(&vocab, &cfg.positive, &contradicted);
            batch.remove(&vocab, &cfg.mixed, &contradicted);
            counted
        }
        Answer::Mixed => {
            // "Partly X" is false once the folder wholly has X or a descendant
            // of X, or wholly lacks X or an ancestor of X.
            let uuid = batch.ensure_entry(&mut vocab, path);
            let counted = batch.add(&cfg.mixed, &uuid);
            batch.remove(&vocab, &cfg.positive, &vocab.with_descendants(path));
            batch.remove(&vocab, &cfg.negative, &vocab.with_ancestors(path));
            counted
        }
    };
    batch.send(client, repo, counted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn vocab() -> Vec<String> {
        ["musique", "musique/jazz", "musique/jazz/bebop", "musique/rock", "administratif"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn test_parent_and_ancestors() {
        assert_eq!(parent("musique/jazz/bebop"), Some("musique/jazz"));
        assert_eq!(parent("musique"), None);
        assert_eq!(ancestors("musique/jazz/bebop"), vec!["musique/jazz", "musique"]);
        assert!(ancestors("musique").is_empty());
    }

    #[test]
    fn test_is_ancestor() {
        assert!(is_ancestor("musique", "musique/jazz"));
        assert!(is_ancestor("musique", "musique/jazz/bebop"));
        assert!(!is_ancestor("musique", "musique"));
        assert!(!is_ancestor("musique/jazz", "musique"));
        // Prefix that is not a path boundary must not match.
        assert!(!is_ancestor("mus", "musique"));
    }

    #[test]
    fn test_descendants() {
        assert_eq!(
            descendants("musique", &vocab()),
            vec!["musique/jazz", "musique/jazz/bebop", "musique/rock"]
        );
        assert!(descendants("administratif", &vocab()).is_empty());
    }

    /// Answers the vocabulary reads from a fixed table (uuid → paths, flag
    /// name → the uuids it is true on) and records the batch it is sent.
    struct Stub {
        entries: Json,
        flags: Json,
        batches: RefCell<Vec<Json>>,
    }

    impl Stub {
        fn new(entries: Json) -> Self {
            Stub::flagged(entries, json!({}))
        }
        fn flagged(entries: Json, flags: Json) -> Self {
            Stub { entries, flags, batches: RefCell::new(Vec::new()) }
        }
        fn ops(&self) -> Vec<Json> {
            let batches = self.batches.borrow();
            assert_eq!(batches.len(), 1, "one answer is one write");
            batches[0]["ops"].as_array().unwrap().clone()
        }
    }

    impl DaemonClient for Stub {
        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Json>,
        ) -> Result<Json, DaemonError> {
            match (method, path) {
                ("POST", "/repos/r/query/fields/resolve-tree") => {
                    assert_eq!(body.unwrap()["field"], "path");
                    Ok(self.entries.clone())
                }
                ("POST", "/repos/r/query") => {
                    // `mf_schema = "tag" AND <flag> = true`: the uuids it is true on.
                    let q = &body.unwrap()["query"];
                    assert_eq!(q["type"], "and");
                    assert_eq!(q["operands"][0]["field"], "mf_schema");
                    assert_eq!(q["operands"][1]["value"], json!({"type": "bool", "value": true}));
                    let flag = q["operands"][1]["field"].as_str().unwrap();
                    Ok(json!({"results": self.flags.get(flag).cloned().unwrap_or(json!([]))}))
                }
                ("GET", "/repos/r/fields") => {
                    Ok(json!([{"name": "path", "type": "hierarchy_ref"}]))
                }
                ("POST", "/repos/r/query/fields/batch") => {
                    self.batches.borrow_mut().push(body.unwrap().clone());
                    Ok(json!({"results": [{"updated": 3}]}))
                }
                _ => Err(DaemonError::local(format!("unexpected {method} {path}"))),
            }
        }
    }

    const MUSIQUE: &str = "11111111111111111111111111111111";
    const JAZZ: &str = "22222222222222222222222222222222";
    const BEBOP: &str = "33333333333333333333333333333333";

    fn music() -> Stub {
        Stub::new(json!({
            MUSIQUE: ["musique"],
            JAZZ: ["musique/jazz"],
            BEBOP: ["musique/jazz/bebop"],
        }))
    }

    fn sel() -> Json {
        json!({"type": "uuid_in", "uuids": ["aa"]})
    }

    /// The ops as `(op, field, target)` triples, the query left out.
    fn summary(ops: &[Json]) -> Vec<(String, String, String)> {
        ops.iter()
            .map(|o| {
                (
                    o["op"].as_str().unwrap().to_string(),
                    o["name"].as_str().unwrap_or("").to_string(),
                    o["value"]["value"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn t(op: &str, field: &str, target: &str) -> (String, String, String) {
        (op.into(), field.into(), target.into())
    }

    #[test]
    fn add_drops_the_ancestors_and_what_it_contradicts() {
        let stub = music();
        let n =
            apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "musique/jazz").unwrap();
        assert_eq!(n, 3);
        let ops = stub.ops();
        assert!(ops.iter().all(|o| o["query"] == sel()));
        assert_eq!(
            summary(&ops),
            vec![
                t("add", "tag", JAZZ),
                t("remove", "tag", MUSIQUE),
                t("remove", "negative_tag", JAZZ),
                t("remove", "negative_tag", MUSIQUE),
                t("remove", "mixed_tag", JAZZ),
                t("remove", "mixed_tag", MUSIQUE),
            ]
        );
    }

    #[test]
    fn deny_drops_the_descendants_and_what_it_contradicts() {
        let stub = music();
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Deny, "musique/jazz").unwrap();
        assert_eq!(
            summary(&stub.ops()),
            vec![
                t("add", "negative_tag", JAZZ),
                t("remove", "negative_tag", BEBOP),
                t("remove", "tag", JAZZ),
                t("remove", "tag", BEBOP),
                t("remove", "mixed_tag", JAZZ),
                t("remove", "mixed_tag", BEBOP),
            ]
        );
    }

    #[test]
    fn mixed_drops_the_whole_answers_it_contradicts() {
        let stub = music();
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Mixed, "musique/jazz").unwrap();
        assert_eq!(
            summary(&stub.ops()),
            vec![
                t("add", "mixed_tag", JAZZ),
                t("remove", "tag", JAZZ),
                t("remove", "tag", BEBOP),
                t("remove", "negative_tag", JAZZ),
                t("remove", "negative_tag", MUSIQUE),
            ]
        );
    }

    #[test]
    fn an_answer_creates_the_missing_entries_in_the_same_write() {
        let stub = music();
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "livre/roman").unwrap();
        let ops = stub.ops();
        assert_eq!(ops[0]["op"], "create");
        assert_eq!(ops[0]["fields"][1]["value"]["value"], json!({"parent": null, "name": "livre"}));
        assert_eq!(ops[1]["op"], "create");
        assert_eq!(
            ops[1]["fields"][1]["value"],
            json!({"type": "hierarchy_ref", "value": {"parent": ops[0]["uuid"], "name": "roman"}})
        );
        // The ref points at the entry the batch creates, before it exists.
        assert_eq!(summary(&ops[2..3]), vec![t("add", "tag", ops[1]["uuid"].as_str().unwrap())]);
    }

    #[test]
    fn remove_of_an_unknown_tag_writes_nothing() {
        let stub = music();
        let n = apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Remove, "livre").unwrap();
        assert_eq!(n, 0);
        assert!(stub.batches.borrow().is_empty());
    }

    #[test]
    fn a_tag_of_two_parents_reaches_the_ancestors_of_both() {
        const SPORT: &str = "44444444444444444444444444444444";
        const EVENT: &str = "66666666666666666666666666666666";
        const MARATON: &str = "55555555555555555555555555555555";
        let stub = Stub::new(json!({
            SPORT: ["sport"],
            EVENT: ["évènement"],
            MARATON: ["sport/maraton", "évènement/maraton"],
        }));
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "évènement/maraton").unwrap();
        let negatives: Vec<_> = summary(&stub.ops())
            .into_iter()
            .filter(|(op, f, _)| op == "remove" && f == "negative_tag")
            .map(|(_, _, target)| target)
            .collect();
        // The tag itself once, and the ancestors by both of its paths.
        assert_eq!(negatives.len(), 3);
        for uuid in [MARATON, SPORT, EVENT] {
            assert!(negatives.iter().any(|n| n == uuid), "{uuid} in {negatives:?}");
        }
    }

    const ROCK: &str = "77777777777777777777777777777777";
    const PUNK: &str = "88888888888888888888888888888888";

    /// musique is a partition: its children exclude one another.
    fn partitioned_music() -> Stub {
        Stub::flagged(
            json!({
                MUSIQUE: ["musique"],
                JAZZ: ["musique/jazz"],
                BEBOP: ["musique/jazz/bebop"],
                ROCK: ["musique/rock"],
                PUNK: ["musique/rock/punk"],
            }),
            json!({"partition": [MUSIQUE]}),
        )
    }

    fn removed_positives(ops: &[Json]) -> Vec<String> {
        summary(ops)
            .into_iter()
            .filter(|(op, f, _)| op == "remove" && f == "tag")
            .map(|(_, _, target)| target)
            .collect()
    }

    #[test]
    fn the_vocabulary_reads_the_flags_of_its_entries() {
        let stub = Stub::flagged(
            json!({MUSIQUE: ["musique"], JAZZ: ["musique/jazz"], ROCK: ["musique/rock"]}),
            json!({"always": [JAZZ], "partition": [MUSIQUE], "sparse": [MUSIQUE, ROCK]}),
        );
        let vocab = Vocab::load(&stub, "r", &TagConfig::default()).unwrap();
        let flags: Vec<(String, Flags)> =
            vocab.entries().into_iter().map(|(path, _, flags)| (path, flags)).collect();
        assert_eq!(
            flags,
            vec![
                ("musique".into(), Flags { always: false, partition: true, sparse: true }),
                ("musique/jazz".into(), Flags { always: true, partition: false, sparse: false }),
                ("musique/rock".into(), Flags { always: false, partition: false, sparse: true }),
            ]
        );
    }

    #[test]
    fn add_under_a_partition_drops_the_siblings_and_what_they_imply() {
        let stub = partitioned_music();
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "musique/jazz").unwrap();
        let removed = removed_positives(&stub.ops());
        // rock, and punk under it (punk is rock), besides the ancestor musique.
        for uuid in [MUSIQUE, ROCK, PUNK] {
            assert!(removed.iter().any(|r| r == uuid), "{uuid} in {removed:?}");
        }
        assert!(!removed.iter().any(|r| r == JAZZ || r == BEBOP), "{removed:?}");
    }

    #[test]
    fn add_below_a_partition_child_excludes_that_childs_siblings() {
        // bebop is jazz, so it excludes rock as jazz does.
        let stub = partitioned_music();
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "musique/jazz/bebop")
            .unwrap();
        let removed = removed_positives(&stub.ops());
        for uuid in [ROCK, PUNK] {
            assert!(removed.iter().any(|r| r == uuid), "{uuid} in {removed:?}");
        }
        assert!(!removed.iter().any(|r| r == BEBOP), "{removed:?}");
    }

    #[test]
    fn a_partition_never_drops_the_tag_added_nor_what_it_implies() {
        // fusion is both jazz and rock: adding it excludes neither of them.
        const FUSION: &str = "99999999999999999999999999999999";
        const POP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let stub = Stub::flagged(
            json!({
                MUSIQUE: ["musique"],
                JAZZ: ["musique/jazz"],
                ROCK: ["musique/rock"],
                POP: ["musique/pop"],
                FUSION: ["musique/jazz/fusion", "musique/rock/fusion"],
            }),
            json!({"partition": [MUSIQUE]}),
        );
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "musique/jazz/fusion")
            .unwrap();
        let ops = stub.ops();
        let removed = removed_positives(&ops);
        assert!(removed.iter().any(|r| r == POP), "{removed:?}");
        assert!(!removed.iter().any(|r| r == FUSION), "{removed:?}");
        // Each tag is removed once, whichever sibling reached it.
        let mut unique = removed.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), removed.len(), "{removed:?}");
    }

    #[test]
    fn without_a_partition_siblings_go_together() {
        let stub = Stub::new(json!({
            MUSIQUE: ["musique"], JAZZ: ["musique/jazz"], ROCK: ["musique/rock"],
        }));
        apply(&stub, "r", &TagConfig::default(), &sel(), Answer::Add, "musique/jazz").unwrap();
        assert_eq!(removed_positives(&stub.ops()), vec![MUSIQUE.to_string()]);
    }
}
