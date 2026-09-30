//! Watch/ignore eligibility (doc "Watch and ignore fields"): decides
//! whether a repo-root-relative path should be tracked, from the `mf_watch`
//! and `mf_ignore` fields inherited along the `mfr_path` ancestor chain.
//!
//! Every decision is answered by the [`WatchRules`] index (doc "The watch rule index"):
//! the few metarecords holding a rule, keyed by their path,
//! so that evaluating a path reads nothing from the store.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use metafolder_core::metarecord::{TreeName, Value};
use regex::Regex;
use uuid::Uuid;

use crate::relpath::RelPath;
use crate::store::{Rows, Store};
use crate::tree_cache::{normalize_name, TreeCache};

/// The field recording a directory the watch budget could not afford
/// (doc "The watch budget"). Reserved (`mfr_*`), inherited down
/// the `mfr_path` tree like `mf_watch`.
pub const WATCH_EXCEEDED: &str = "mfr_watch_exceeded";

/// Why [`WatchRules::explain`] decided the way it did — the step of the eligibility
/// algorithm (doc "Eligibility") that settled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No `mf_watch` anywhere on the ancestor chain: the opt-in default.
    NoWatch,
    /// The nearest `mf_watch` is `false` (step 2).
    WatchFalse,
    /// `mf_watch` is set directly on the path: tracked unconditionally (step 3).
    DirectWatch,
    /// A pattern of the effective ignore set matched (step 5).
    Ignored,
    /// Nothing excluded it (step 6).
    Tracked,
}

impl Reason {
    /// The wire form used by `POST /repos/:repo/eligibility`.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NoWatch => "no_watch",
            Reason::WatchFalse => "watch_false",
            Reason::DirectWatch => "direct_watch",
            Reason::Ignored => "ignored",
            Reason::Tracked => "tracked",
        }
    }
}

/// A reasoned eligibility decision: the verdict plus what produced it, so a
/// client can show *why* a path is (not) tracked without re-implementing the
/// walk or re-running the patterns in another regex dialect.
#[derive(Debug, Clone)]
pub struct Explanation {
    pub eligible: bool,
    pub reason: Reason,
    /// Path of the tracking-scope root — the metarecord whose `mf_watch`
    /// decided, and the anchor patterns are matched against. `None` when no
    /// `mf_watch` was found at all.
    pub watch_scope: Option<String>,
    /// Path of the metarecord providing the effective ignore set, `None` when
    /// none does (or when the decision came before step 4).
    pub ignore_source: Option<String>,
    /// The pattern that matched, set only for [`Reason::Ignored`].
    pub pattern: Option<String>,
}

/// The `mf_ignore` set that *governs* `rel_path`, and where it comes from
/// (doc "Eligibility").
#[derive(Debug, Clone)]
pub struct EffectiveIgnore {
    /// Path of the metarecord providing the set, `None` when nothing on the
    /// chain (the path included) has an `mf_ignore` row.
    pub source: Option<String>,
    pub source_uuid: Option<Uuid>,
    /// Whether the source *is* `rel_path` itself — i.e. writing here replaces
    /// its own set rather than shadowing an inherited one.
    pub direct: bool,
    pub patterns: Vec<String>,
}

/// The metarecords existing along `rel_path`, as `(component_index, uuid)` from
/// the root down. A TreeRef child requires its parent metarecord, so the chain
/// stops at the first unresolved prefix. `rel_path` is repo-root-relative,
/// `/`-separated, leading slash; `""` is the root.
fn ancestor_chain(
    conn: &dyn Store,
    cache: &TreeCache,
    rel_path: &str,
) -> Result<Vec<(usize, Uuid)>> {
    let comps: Vec<&str> = rel_path.split('/').collect();
    // Prefixes from the root down: "" for the root, then "/a", "/a/b", …
    let prefixes: Vec<String> = (0..comps.len()).map(|i| comps[..=i].join("/")).collect();
    let mut chain: Vec<(usize, Uuid)> = Vec::new();
    for (i, prefix) in prefixes.iter().enumerate() {
        match cache.resolve_path(conn, "mfr_path", prefix)? {
            Some(uuid) => chain.push((i, uuid)),
            None => break,
        }
    }
    Ok(chain)
}

/// The effective `mf_sync` mode of the record at `rel_path` (spec-sync): the
/// value of the nearest ancestor (including the record itself) that defines
/// `mf_sync`, defaulting to `internal` when none does. `external` means an
/// external tool owns the content; anything else (incl. absent) is `internal`.
pub fn resolve_mf_sync(conn: &dyn Store, cache: &TreeCache, rel_path: &str) -> Result<String> {
    let chain = ancestor_chain(conn, cache, rel_path)?;
    for (_, uuid) in chain.iter().rev() {
        if let Some(v) = Rows::string_fields(conn, *uuid, "mf_sync")?.into_iter().next() {
            return Ok(if v == "external" { v } else { "internal".to_string() });
        }
    }
    Ok("internal".to_string())
}

/// Evaluates eligibility for `rel_path` (repo-root-relative, `/`-separated,
/// leading slash; `""` is the root itself), reading the rules afresh. A caller
/// with more than one path to ask loads a [`WatchRules`] once instead.
pub fn is_eligible(conn: &dyn Store, cache: &TreeCache, rel_path: &str) -> Result<bool> {
    Ok(explain(conn, cache, rel_path)?.eligible)
}

/// [`WatchRules::explain`] on rules read afresh.
pub fn explain(conn: &dyn Store, cache: &TreeCache, rel_path: &str) -> Result<Explanation> {
    let rules = WatchRules::load(conn, cache.is_case_insensitive())?;
    rules.explain(&rules.rel_of_text(rel_path))
}

/// [`WatchRules::effective_ignore`] on rules read afresh.
pub fn effective_ignore(
    conn: &dyn Store,
    cache: &TreeCache,
    rel_path: &str,
) -> Result<EffectiveIgnore> {
    let rules = WatchRules::load(conn, cache.is_case_insensitive())?;
    Ok(rules.effective_ignore(&rules.rel_of_text(rel_path)))
}

/// A path as the rule index keys it: its components' exact bytes, folded when
/// the filesystem is case-insensitive — the tree cache's own key, so a path
/// finds its rules exactly where the cache would find its metarecord.
type Key = Vec<Vec<u8>>;

/// The rules one metarecord holds.
#[derive(Debug)]
struct Carrier {
    uuid: Uuid,
    watch: Option<bool>,
    exceeded: Option<bool>,
    /// `mf_ignore`, in row order, each compiled once. A pattern that does not
    /// compile is kept with its error, raised when a path reaches it — as the
    /// chain walk did.
    ignore: Vec<(String, std::result::Result<Regex, String>)>,
}

/// The rule index (doc "The watch rule index"): every metarecord that
/// holds `mf_watch`, `mf_ignore` or `mfr_watch_exceeded`, keyed by its
/// `mfr_path`. Few entries — the directories the user chose plus the watch
/// budget's frontier — so a path is evaluated by walking up its own prefixes in
/// a map, without a store read, a tree lookup or a lock.
///
/// A snapshot: it answers for the state it was [`loaded`](Self::load) from,
/// recorded as [`Self::head`].
#[derive(Debug)]
pub struct WatchRules {
    case_insensitive: bool,
    head: Option<i64>,
    carriers: HashMap<Key, Carrier>,
    /// The carriers' keys and every prefix of them: the paths whose move or
    /// removal would move a rule ([`Self::touches`]).
    anchors: HashSet<Key>,
    /// The holders (placed or orphaned) and the ancestors of the placed ones:
    /// the metarecords whose `mfr_path` changing would move a rule
    /// ([`Self::affects`]).
    affected: HashSet<Uuid>,
}

impl WatchRules {
    /// Reads the rules from `store`: the holders of the three fields and their
    /// paths — proportional to the number of rules, not to the repository.
    pub fn load(store: &dyn Store, case_insensitive: bool) -> Result<Self> {
        let head = store.head()?;
        let mut holders: Vec<Uuid> = Vec::new();
        for name in ["mf_watch", "mf_ignore", WATCH_EXCEEDED] {
            holders.extend(store.holders(name)?);
        }
        holders.sort();
        holders.dedup();

        let mut rules = Self {
            case_insensitive,
            head,
            carriers: HashMap::new(),
            anchors: HashSet::new(),
            affected: HashSet::new(),
        };
        let mut placed: HashMap<Uuid, Option<Placement>> = HashMap::new();
        let mut compiled: HashMap<String, std::result::Result<Regex, String>> = HashMap::new();
        for uuid in holders {
            rules.affected.insert(uuid);
            let watch = store.bool_field(uuid, "mf_watch")?;
            let exceeded = store.bool_field(uuid, WATCH_EXCEEDED)?;
            let ignore: Vec<(String, std::result::Result<Regex, String>)> = store
                .string_fields(uuid, "mf_ignore")?
                .into_iter()
                .map(|pattern| {
                    let regex = compiled
                        .entry(pattern.clone())
                        .or_insert_with(|| {
                            crate::regexp::compile(&pattern).map_err(|e| e.to_string())
                        })
                        .clone();
                    (pattern, regex)
                })
                .collect();
            if watch.is_none() && exceeded.is_none() && ignore.is_empty() {
                continue; // Holds the name, but no value the algorithm reads.
            }
            let Some(placement) = placement(store, uuid, &mut placed)? else {
                continue; // Orphaned: no path, so it governs none.
            };
            rules.affected.extend(placement.chain.iter().copied());
            let key: Key =
                placement.names.iter().map(|n| normalize_name(n, case_insensitive)).collect();
            for depth in 0..=key.len() {
                rules.anchors.insert(key[..depth].to_vec());
            }
            rules.carriers.insert(key, Carrier { uuid, watch, exceeded, ignore });
        }
        Ok(rules)
    }

    /// The HEAD the rules were read at.
    pub fn head(&self) -> Option<i64> {
        self.head
    }

    /// A path given as *text* (an API parameter, a path typed by the user),
    /// read the way the tree cache reads one (`PathForm::Any`): a component
    /// that decodes as an escape (`%E9`) designates either the name that
    /// really is that text or the one holding the bytes it escapes. The rules
    /// only care about the reading that leads to one of them, so that reading
    /// is taken; the verbatim one otherwise — and when both would, which only
    /// two rule-carrying directories named alike could cause.
    pub fn rel_of_text(&self, path: &str) -> RelPath {
        let mut rel = RelPath::root();
        let mut key: Key = Vec::new();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let verbatim = TreeName::from(comp);
            let chosen = match metafolder_core::metarecord::escaped_to_bytes(comp) {
                Some(bytes) => {
                    let escaped = TreeName::from_bytes(bytes);
                    let leads = |name: &TreeName| {
                        let mut k = key.clone();
                        k.push(normalize_name(name, self.case_insensitive));
                        self.anchors.contains(&k)
                    };
                    if leads(&escaped) && !leads(&verbatim) {
                        escaped
                    } else {
                        verbatim
                    }
                }
                None => verbatim,
            };
            key.push(normalize_name(&chosen, self.case_insensitive));
            rel = rel.child(chosen);
        }
        rel
    }

    fn key(&self, rel: &RelPath) -> Key {
        rel.components().iter().map(|c| normalize_name(c, self.case_insensitive)).collect()
    }

    /// The eligibility algorithm itself (doc "Eligibility"), keeping the reason it stopped at.
    /// Every eligibility decision
    /// in the daemon comes here — the verdict and its explanation can therefore
    /// never disagree. Fails only on an `mf_ignore` pattern that does not
    /// compile, once a path reaches it.
    pub fn explain(&self, rel: &RelPath) -> Result<Explanation> {
        let comps = rel.components();
        let key = self.key(rel);
        let at = |depth: usize| self.carriers.get(&key[..depth]);

        // Steps 1–2: the nearest `mf_watch`, the path itself included. Its
        // depth marks the tracking-scope root, which ignore patterns are
        // matched relative to.
        let watch = (0..=comps.len())
            .rev()
            .find_map(|depth| at(depth).and_then(|c| c.watch).map(|value| (depth, value)));
        let Some((watch_depth, watch_value)) = watch else {
            return Ok(Explanation {
                eligible: false,
                reason: Reason::NoWatch,
                watch_scope: None,
                ignore_source: None,
                pattern: None,
            });
        };
        let scope = display_prefix(comps, watch_depth);
        if !watch_value {
            return Ok(Explanation {
                eligible: false,
                reason: Reason::WatchFalse,
                watch_scope: Some(scope),
                ignore_source: None,
                pattern: None,
            });
        }
        // Step 3: set directly on the path → tracked unconditionally.
        if watch_depth == comps.len() {
            return Ok(Explanation {
                eligible: true,
                reason: Reason::DirectWatch,
                watch_scope: Some(scope),
                ignore_source: None,
                pattern: None,
            });
        }
        // The path re-anchored at the tracking-scope root: a directly-watched
        // hidden directory (`.config`) does not prune its own subtree, while
        // `\.git` still applies inside the scope.
        let scoped = display_prefix(&comps[watch_depth..], comps.len() - watch_depth);

        // Steps 4–5: the nearest *strict* ancestor holding patterns provides
        // the effective set (sets replace each other, never merge).
        for depth in (0..comps.len()).rev() {
            let Some(carrier) = at(depth).filter(|c| !c.ignore.is_empty()) else {
                continue;
            };
            let source = display_prefix(comps, depth);
            for (pattern, regex) in &carrier.ignore {
                let regex = regex
                    .as_ref()
                    .map_err(|e| anyhow!("invalid mf_ignore pattern '{pattern}': {e}"))?;
                if regex.is_match(&scoped) {
                    return Ok(Explanation {
                        eligible: false,
                        reason: Reason::Ignored,
                        watch_scope: Some(scope),
                        ignore_source: Some(source),
                        pattern: Some(pattern.clone()),
                    });
                }
            }
            return Ok(Explanation {
                eligible: true,
                reason: Reason::Tracked,
                watch_scope: Some(scope),
                ignore_source: Some(source),
                pattern: None,
            });
        }
        Ok(Explanation {
            eligible: true,
            reason: Reason::Tracked,
            watch_scope: Some(scope),
            ignore_source: None,
            pattern: None,
        })
    }

    /// Whether `rel` is to be tracked.
    pub fn is_eligible(&self, rel: &RelPath) -> Result<bool> {
        Ok(self.explain(rel)?.eligible)
    }

    /// The `mf_ignore` set that governs writes at `rel` (doc "Eligibility").
    /// Unlike [`Self::explain`] this *includes* the
    /// path itself: the question is "which set governs writes here", not
    /// "which set filtered this entry".
    pub fn effective_ignore(&self, rel: &RelPath) -> EffectiveIgnore {
        let comps = rel.components();
        let key = self.key(rel);
        for depth in (0..=comps.len()).rev() {
            if let Some(carrier) = self.carriers.get(&key[..depth]) {
                if !carrier.ignore.is_empty() {
                    return EffectiveIgnore {
                        source: Some(display_prefix(comps, depth)),
                        source_uuid: Some(carrier.uuid),
                        direct: depth == comps.len(),
                        patterns: carrier.ignore.iter().map(|(p, _)| p.clone()).collect(),
                    };
                }
            }
        }
        EffectiveIgnore { source: None, source_uuid: None, direct: false, patterns: Vec::new() }
    }

    /// The directory whose `mfr_watch_exceeded = true` covers `rel` (the path
    /// itself included; the nearest value decides), `None` when nothing
    /// excludes it (doc "The watch budget").
    pub fn exceeded_by(&self, rel: &RelPath) -> Option<String> {
        let comps = rel.components();
        let key = self.key(rel);
        (0..=comps.len()).rev().find_map(|depth| {
            let value = self.carriers.get(&key[..depth]).and_then(|c| c.exceeded)?;
            Some(value.then(|| display_prefix(comps, depth)))
        })?
    }

    /// The `mfr_watch_exceeded` value set on `rel` itself, if any.
    pub fn exceeded_own(&self, rel: &RelPath) -> Option<bool> {
        self.carriers.get(&self.key(rel)).and_then(|c| c.exceeded)
    }

    /// Whether `rel` holds a rule or is an ancestor of one: moving or removing
    /// it would move rules, so the index no longer describes the disk until
    /// that change is committed.
    pub fn touches(&self, rel: &RelPath) -> bool {
        self.anchors.contains(&self.key(rel))
    }

    /// Whether writing `uuid`'s `mfr_path` could move a rule: it holds one
    /// (placed or orphaned), or is an ancestor of one that is placed.
    pub fn affects(&self, uuid: Uuid) -> bool {
        self.affected.contains(&uuid)
    }
}

/// `/a/b` for the first `depth` components (`""` for none — the root).
fn display_prefix(comps: &[TreeName], depth: usize) -> String {
    let mut out = String::new();
    for comp in &comps[..depth] {
        out.push('/');
        out.push_str(&comp.display());
    }
    out
}

/// Where a metarecord sits in the `mfr_path` forest.
#[derive(Clone)]
struct Placement {
    /// The names below the root, outermost first.
    names: Vec<TreeName>,
    /// The metarecords from the root down to this one, itself included.
    chain: Vec<Uuid>,
}

/// The deepest a placement walk goes before it gives up on a cycle the forest
/// constraint should have made impossible.
const MAX_DEPTH: usize = 1100;

/// The placement of `uuid`, memoised in `memo` (carriers share ancestors).
/// `None` when it has no `mfr_path` — an orphan — or hangs below one that has
/// none, or under a root that is not the repository's (named, not `""`).
fn placement(
    store: &dyn Store,
    uuid: Uuid,
    memo: &mut HashMap<Uuid, Option<Placement>>,
) -> Result<Option<Placement>> {
    // Up to the first known ancestor (or the root), then back down.
    let mut pending: Vec<(Uuid, TreeName)> = Vec::new();
    let mut cursor = uuid;
    let base: Option<Placement> = loop {
        if let Some(known) = memo.get(&cursor) {
            break known.clone();
        }
        if pending.len() > MAX_DEPTH {
            break None;
        }
        let position =
            store.rows_named(cursor, "mfr_path")?.into_iter().find_map(|r| match r.value {
                Value::TreeRef { parent, name } => Some((parent, name)),
                _ => None,
            });
        match position {
            None => break None,
            Some((None, name)) => {
                if name.as_bytes().is_empty() {
                    let root = Placement { names: Vec::new(), chain: vec![cursor] };
                    memo.insert(cursor, Some(root.clone()));
                    break Some(root);
                }
                break None;
            }
            Some((Some(parent), name)) => {
                pending.push((cursor, name));
                cursor = parent;
            }
        }
    };
    let mut current = base;
    for (node, name) in pending.into_iter().rev() {
        current = current.map(|mut p| {
            p.names.push(name);
            p.chain.push(node);
            p
        });
        memo.insert(node, current.clone());
    }
    if current.is_none() {
        memo.insert(uuid, None);
    }
    Ok(current)
}
