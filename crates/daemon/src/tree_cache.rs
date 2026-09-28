//! In-memory tree cache (spec-file-tracking "Tree Cache"): resolves path
//! strings to metarecord UUIDs without recursive SQL. One cache per repository,
//! shared across all TreeRef field names (the field name is the first level).
//! Populated whole at repository load and kept in step by the `apply_*`
//! maintenance; the forest is resident, with no node budget — the same memory
//! policy as the query index it is built alongside.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use metafolder_core::metarecord::TreeName;
use metafolder_core::query::OsmProgress;

use crate::db;
use crate::log::{TreeOp, TreePos, MAX_TREE_DEPTH, UNKNOWN_ROW};
use crate::store::Rows;

/// Separator joining the components of a *sort key* — the form a `tree_ref`
/// value takes when a query sorts on it (spec-data-model "Sort specification").
///
/// It is deliberately not `/`: a path separator that sorts *below* every
/// character a name can contain turns a plain byte comparison of two keys into a
/// component-by-component comparison of the two paths, so a directory and its
/// contents stay together (`photos/2021` before `photos-old`, which a literal
/// `/` would interleave since `-` < `/`). Keys are internal — they are never
/// displayed, only compared and carried inside opaque cursors — and the SQL
/// oracle builds the identical key (`metafolder-query-oracle`'s `path_key_cte`).
pub const PATH_KEY_SEP: char = '\u{1}';

/// Where a node hangs, which is the whole of what linking and unlinking change.
/// Kept as one value rather than as a parent index *and* a flag: the states are
/// exclusive, and a node that is momentarily in none of the maps has to be
/// distinguishable from a root, or unlinking it a second time evicts whichever
/// root happens to share its name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// In the field's roots map: the position names no parent.
    Root,
    /// In this node's children map.
    Under(usize),
    /// In the field's waiting index, under the metarecord the position names as
    /// its parent: that metarecord holds no position of its own. The node is
    /// resident and findable by uuid, and in no path — which is where a fresh
    /// load leaves it too.
    Waiting(Uuid),
    /// In no map at all, briefly and on purpose: a settle unlinks every cell it
    /// is about to move before it moves any, so two siblings that swap names
    /// have no order to get wrong.
    Unlinked,
}

struct Node {
    /// The name's exact bytes — what identifies the node (spec-data-model
    /// "Tree names"). The children/roots maps are keyed by its *normalized*
    /// bytes, which fold case when the filesystem does but never merge two
    /// names that differ in an undecodable byte.
    name: TreeName,
    uuid: Uuid,
    /// The `field` row this position comes from ([`TreePos::row`]), or
    /// [`UNKNOWN_ROW`] when the producer had none. What keeps a metarecord's
    /// positions in the order a load would read them.
    row: i64,
    place: Placement,
    children: HashMap<Vec<u8>, usize>,
}

#[derive(Default)]
struct FieldTree {
    /// Root nodes by normalized name bytes.
    roots: HashMap<Vec<u8>, usize>,
    /// Cached nodes by metarecord UUID: one each (spec-data-model "One
    /// position per forest"), save in a repository an older daemon let hold
    /// more, which `mf repo check` names.
    by_uuid: HashMap<Uuid, Vec<usize>>,
    /// Nodes waiting for a parent, by the metarecord uuid they wait for. What
    /// makes the upkeep independent of the order positions arrive in: a child
    /// settled before its parent is linked when the parent shows up, so no
    /// producer has to sort its work — and none of them can
    /// (see [`TreeCache::adopt`]).
    waiting: HashMap<Uuid, Vec<usize>>,
}

impl FieldTree {
    fn push_node(&mut self, uuid: Uuid, idx: usize) {
        self.by_uuid.entry(uuid).or_default().push(idx);
    }

    fn set_nodes(&mut self, uuid: Uuid, nodes: Vec<usize>) {
        if nodes.is_empty() {
            self.by_uuid.remove(&uuid);
        } else {
            self.by_uuid.insert(uuid, nodes);
        }
    }

    fn drop_node(&mut self, uuid: Uuid, idx: usize) {
        let Some(list) = self.by_uuid.get_mut(&uuid) else { return };
        list.retain(|&n| n != idx);
        if list.is_empty() {
            self.by_uuid.remove(&uuid);
        }
    }
}

/// What resolving one path component yielded.
enum Resolved<T> {
    /// The node it names.
    Found(T),
    /// Not here — another source (the database) may still know.
    Missing,
    /// The readings name different files: the path designates neither, and no
    /// further lookup may override that.
    Ambiguous,
}

/// Which reading of a typed path to resolve (spec-data-model "Tree names").
///
/// A component can name two different files at once — one whose name really is
/// `%E9.txt`, one whose name is the byte `0xE9` — so a lookup that must yield a
/// *single* metarecord has to be told which, or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathForm {
    /// Try both; resolve to nothing when they name different files.
    #[default]
    Any,
    /// The bytes as typed — what finds a file whose name really contains them.
    Verbatim,
    /// The bytes the escaped form decodes to.
    Escaped,
}

/// The map key for a name: its exact bytes, with the *decodable* runs
/// lowercased when the filesystem is case-insensitive. Shared with the rule
/// index ([`crate::eligibility::WatchRules`]), which must find a path exactly
/// where this cache does.
///
/// Folding only what decodes is what keeps two names differing in an
/// undecodable byte apart: lowercasing the lossy text would map both onto
/// the same replacement character and merge two distinct files.
pub fn normalize_name(name: &TreeName, case_insensitive: bool) -> Vec<u8> {
    let bytes = name.as_bytes();
    if !case_insensitive {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.extend_from_slice(text.to_lowercase().as_bytes());
                return out;
            }
            Err(err) => {
                let (good, bad) = rest.split_at(err.valid_up_to());
                // `good` is valid UTF-8 by construction.
                out.extend_from_slice(
                    std::str::from_utf8(good).unwrap_or_default().to_lowercase().as_bytes(),
                );
                // The undecodable bytes pass through untouched: they are
                // what distinguishes this name from its look-alike.
                let skip = err.error_len().unwrap_or(bad.len());
                out.extend_from_slice(&bad[..skip]);
                rest = &bad[skip..];
            }
        }
    }
}

pub struct TreeCache {
    arena: Vec<Option<Node>>,
    free: Vec<usize>,
    fields: HashMap<String, FieldTree>,
    live: usize,
    case_insensitive: bool,
    misses: u64,
    /// Whether the forest has been loaded ([`Self::populate`]). A repository
    /// serves nothing until it has (spec-main "POST /repos/load"), so through
    /// the API this is always true; it is false only on a cache built directly,
    /// as unit tests do, where the DB fallbacks below still apply. Nothing
    /// clears it any more: there is no eviction to lose a node to.
    complete: bool,
    /// Whether the forest is ever kept here. A key-value repository keeps
    /// none (spec-storage increment 4 e): its store answers every forest
    /// question, a load leaves this empty, and a lookup starts from scratch —
    /// what one lookup brings in is dropped by the next, so nothing grows and
    /// nothing goes stale, there being no upkeep.
    resident: bool,
}

impl TreeCache {
    pub fn new(case_insensitive: bool) -> Self {
        Self {
            arena: Vec::new(),
            free: Vec::new(),
            fields: HashMap::new(),
            live: 0,
            case_insensitive,
            misses: 0,
            complete: false,
            resident: true,
        }
    }

    /// A cache that keeps no forest: see [`Self::keeps_forest`].
    pub fn without_forest(mut self) -> Self {
        self.resident = false;
        self
    }

    /// Whether the forest is kept here at all (false on a key-value
    /// repository, whose store is asked instead).
    pub fn keeps_forest(&self) -> bool {
        self.resident
    }

    /// Without a resident forest, forgets what the last lookup brought in.
    fn scratch(&mut self) {
        if !self.resident && self.live > 0 {
            self.clear();
        }
    }

    /// True while the whole forest is resident in memory (see [`Self::populate`]).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Eagerly loads the entire TreeRef forest (all field names) into memory in
    /// a single DB scan, so that subsequent read-side navigation is served
    /// without per-node queries. Replaces any current contents.
    pub fn populate(&mut self, store: &dyn Rows) -> Result<()> {
        if !self.resident {
            self.clear();
            return Ok(());
        }
        // Timed in two parts (logged when non-trivial): the `load_tree_forest`
        // SQL scan+sort, and the in-memory node linking — a persistent load
        // report, so it is clear which dominates on a large forest.
        let t_scan = std::time::Instant::now();
        let rows = store.forest()?;
        let scan = t_scan.elapsed();
        let n = rows.len();
        let t_link = std::time::Instant::now();
        self.populate_from_forest(rows);
        let link = t_link.elapsed();
        if (scan + link).as_millis() >= 200 {
            eprintln!("[tree cache] {n} nodes: scan {scan:?}, link {link:?}");
        }
        Ok(())
    }

    /// Populates the cache from a forest already read out of the `field` table
    /// (`db::TreeRow`s in `field.id` order), skipping the DB scan `populate`
    /// does — used at load, where the index build's single pass over `field`
    /// collects them (see `RepoIndex::build_reported_collecting`). Replaces any
    /// current contents.
    pub fn populate_from_forest(&mut self, rows: Vec<db::TreeRow>) {
        if !self.resident {
            self.clear();
            return;
        }
        self.clear();
        // The same two steps every other producer uses — create the node, link
        // it where its position says — in two passes, so a child's parent is in
        // the arena by the time it is linked. The order the rows arrive in does
        // not matter beyond that: anything a pass leaves waiting is adopted
        // when its parent is placed.
        //
        // It stops short of going through [`Self::apply_ops`] like a revision
        // does. Settling reads what each cell held and unlinks it before
        // placing anything, which at load is known to be nothing, and paying
        // for that per cell measured 4.6 s against 1.3 s on a 200 000-node
        // forest (`bench_forest_load`). The load is the one caller that can
        // skip it, and the one where it costs.
        let mut created: Vec<(usize, Option<Uuid>, String)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let idx = self.insert_bare(&row.field_name, row.id, &row.name, row.uuid);
            created.push((idx, row.parent, row.field_name.clone()));
        }
        for (idx, parent, field) in created {
            self.link(&field, idx, parent);
        }
        self.complete = true;
    }

    /// Number of cached nodes.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cumulative number of DB fallback lookups (for tests/diagnostics).
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Resolves a path string to a metarecord UUID. Path format: components
    /// joined by `/`; the first component is the root's own name (so
    /// filesystem paths start with `/` because the root is named `""`).
    pub fn resolve_path(
        &mut self,
        store: &dyn Rows,
        field: &str,
        path: &str,
    ) -> Result<Option<Uuid>> {
        self.resolve_path_as(store, field, path, PathForm::Any)
    }

    /// [`Self::resolve_path`] restricted to one reading of the typed text
    /// (spec-data-model "Tree names"). Naming a reading is what makes the
    /// lookup unambiguous when a path could designate two different files.
    pub fn resolve_path_as(
        &mut self,
        store: &dyn Rows,
        field: &str,
        path: &str,
        form: PathForm,
    ) -> Result<Option<Uuid>> {
        self.scratch();
        // A node's name is never empty — only the filesystem forest's root has
        // one, and it is always the first component. So an empty component
        // *after* the first can only come from a redundant slash, and dropping
        // it makes every spelling of a path name the same node: `"/"` (how a UI
        // spells the repository root, whose canonical form `path_of` gives is
        // `""`), a trailing `"/music/"`, a doubled `"//"`. Keeping them meant
        // looking for a child with an empty name, which nothing can match.
        let split: Vec<&str> = path.split('/').collect();
        let mut comps: Vec<&str> = Vec::with_capacity(split.len());
        comps.push(split[0]); // `split` always yields at least one component.
        comps.extend(split[1..].iter().copied().filter(|c| !c.is_empty()));

        let roots: Vec<usize> = Self::readings(comps[0], form)
            .iter()
            .filter_map(|name| {
                let norm = self.normalize(name);
                self.fields.get(field).and_then(|ft| ft.roots.get(&norm)).copied()
            })
            .collect();
        let mut cur = match Self::arbitrate(&roots, comps[0]) {
            Resolved::Ambiguous => return Ok(None),
            Resolved::Found(idx) => idx,
            Resolved::Missing => {
                if self.complete {
                    return Ok(None); // Full forest resident: a cache miss is absence.
                }
                self.misses += 1;
                let Some((uuid, name)) = self.db_child(store, field, None, comps[0], form)? else {
                    return Ok(None);
                };
                self.insert_node(field, None, &name, uuid)
            }
        };

        for comp in &comps[1..] {
            cur = match self.pick(cur, comp, form) {
                Resolved::Ambiguous => return Ok(None),
                Resolved::Found(idx) => idx,
                Resolved::Missing => {
                    if self.complete {
                        return Ok(None); // Full forest resident: a cache miss is absence.
                    }
                    self.misses += 1;
                    let parent_uuid = self.node(cur).uuid;
                    let Some((uuid, name)) =
                        self.db_child(store, field, Some(parent_uuid), comp, form)?
                    else {
                        return Ok(None);
                    };
                    self.insert_node_at(field, Some(cur), &name, uuid)
                }
            };
        }

        let uuid = self.node(cur).uuid;
        Ok(Some(uuid))
    }

    /// Reconstructs the path string of a metarecord by walking up its parents
    /// in the database.
    pub fn path_of(&mut self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Option<String>> {
        if self.complete {
            return Ok(self.path_of_in_cache(field, uuid));
        }
        self.misses += 1;
        let mut components = Vec::new();
        let mut cur = uuid;
        for _ in 0..MAX_TREE_DEPTH {
            let Some((parent, name)) = store.positions(field, cur)?.into_iter().next() else {
                return Ok(None);
            };
            components.push(name);
            match parent {
                Some(p) => cur = p,
                None => {
                    components.reverse();
                    return Ok(Some(components.join("/")));
                }
            }
        }
        anyhow::bail!("TreeRef chain deeper than {MAX_TREE_DEPTH} for entry {uuid}")
    }

    /// All filesystem-style paths of a metarecord in `field`'s forest, one per
    /// position — one, save in a repository an older daemon let hold more
    /// (spec-data-model "One position per forest"). Positions whose parent is
    /// not in the forest (stale) are skipped. The reverse of
    /// [`Self::resolve_path`].
    pub fn paths_of(&mut self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Vec<String>> {
        if self.complete {
            return Ok(self.paths_of_in_cache(field, uuid));
        }
        self.misses += 1;
        let mut paths = Vec::new();
        for (parent, name) in store.positions(field, uuid)? {
            match parent {
                None => paths.push(name),
                Some(parent) => {
                    if let Some(parent_path) = self.path_of(store, field, parent)? {
                        // Mirror `path_of` exactly: the empty repo-root gives
                        // `parent_path == ""`, so a top-level filesystem node
                        // joins to a leading-"/" path (`/file.txt`) — the same
                        // form the DSL and `resolve_path` use, so the two
                        // round-trip. A named-root forest (e.g. tags) has a
                        // non-empty root name and so no leading "/".
                        paths.push(format!("{parent_path}/{name}"));
                    }
                }
            }
        }
        Ok(paths)
    }

    /// The metarecords of `field`'s forest whose assembled path matches `terms`
    /// as ordered, non-overlapping, case-insensitive substrings — the OSM `Path`
    /// semantics of spec-query, answered by one walk of the resident forest.
    ///
    /// `None` while the cache is incomplete: the walk visits every node, so
    /// without the forest in memory it would be a database query per node and
    /// the caller's candidate-pruning path is the right one.
    ///
    /// Each node is visited once per *position* (a multi-map TreeRef has one
    /// path per position, and a node is reached from each of its parents),
    /// carrying its parent's match progress — so no path is assembled or
    /// rescanned from the start. Once a branch has consumed every term the whole
    /// subtree below it matches, since a descendant's path only extends it: it
    /// is taken wholesale and the walk prunes there.
    pub fn osm_path_matches(&self, field: &str, terms: &[String]) -> Result<Option<Vec<Uuid>>> {
        if !self.complete {
            return Ok(None);
        }
        let Some(ft) = self.fields.get(field) else {
            return Ok(Some(Vec::new()));
        };
        let terms_lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        let mut matched: HashSet<Uuid> = HashSet::new();
        // The accumulated lower-cased path of the branch being walked. Segments
        // are lower-cased one by one, which agrees with lower-casing the whole
        // path: the separator is a word boundary, so no context-dependent casing
        // straddles it.
        let mut path = String::new();

        enum Step {
            Enter(usize, OsmProgress, usize),
            /// Truncate the accumulated path back to a parent's length.
            Leave(usize),
        }
        let start = OsmProgress::default();
        let mut stack: Vec<Step> =
            ft.roots.values().map(|&node| Step::Enter(node, start, 0)).collect();

        while let Some(step) = stack.pop() {
            let (node, inherited, depth) = match step {
                Step::Leave(len) => {
                    path.truncate(len);
                    continue;
                }
                Step::Enter(node, at, depth) => (node, at, depth),
            };
            if depth >= MAX_TREE_DEPTH {
                anyhow::bail!("TreeRef chain deeper than {MAX_TREE_DEPTH} in field '{field}'");
            }
            let node = self.node(node);
            let parent_len = path.len();
            // A root's path is its bare name; every other node joins with '/'.
            if depth > 0 {
                path.push('/');
            }
            // Lower-casing dominates this walk (one pass per node), and node
            // names are overwhelmingly ASCII: fold those in place, byte-wise,
            // and keep the Unicode iterator for the rest.
            let name_start = path.len();
            let display = node.name.display();
            path.push_str(&display);
            if display.is_ascii() {
                path[name_start..].make_ascii_lowercase();
            } else {
                let lowered: String = display.chars().flat_map(char::to_lowercase).collect();
                path.truncate(name_start);
                path.push_str(&lowered);
            }

            let at = metafolder_core::query::osm_advance(&path, &terms_lower, inherited);
            if at.matched == terms_lower.len() {
                matched.insert(node.uuid);
                self.collect_subtree(node, &mut matched);
                path.truncate(parent_len);
                continue;
            }
            stack.push(Step::Leave(parent_len));
            for &child in node.children.values() {
                stack.push(Step::Enter(child, at, depth + 1));
            }
        }
        // Unordered: the caller (`forest_query`) is the chokepoint that pins
        // the order — a rewritten leaf is hashed into the cursor.
        Ok(Some(matched.into_iter().collect()))
    }

    /// The metarecords of `field`'s forest with at least one *assembled path*
    /// satisfying `pred` — the `:path` aspect of spec-query, answered by one
    /// walk of the resident forest and no SQL.
    ///
    /// Like [`Self::osm_path_matches`] the walk carries the branch's path down
    /// and visits a node once per *position*, so a multi-map TreeRef is tested
    /// on each of its paths and no path is reassembled from the root. Unlike it
    /// there is no subtree shortcut: an arbitrary predicate says nothing about
    /// the descendants of a node that matched.
    ///
    /// `None` only when the answer would not be authoritative: an incomplete
    /// cache. A field with *no* forest answers the empty set — either it holds
    /// no data at all, which matches nothing in both engines, or it holds
    /// another type, which the shared type validation has already refused with a
    /// 400 before any of this runs (spec-query "Field aspects").
    pub fn path_matches(
        &self,
        field: &str,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<Option<Vec<Uuid>>> {
        if !self.complete {
            return Ok(None);
        }
        let Some(ft) = self.fields.get(field) else { return Ok(Some(Vec::new())) };
        let mut matched: HashSet<Uuid> = HashSet::new();
        let mut path = String::new();

        enum Step {
            Enter(usize, usize),
            /// Truncate the accumulated path back to a parent's length.
            Leave(usize),
        }
        let mut stack: Vec<Step> = ft.roots.values().map(|&node| Step::Enter(node, 0)).collect();

        while let Some(step) = stack.pop() {
            let (node, depth) = match step {
                Step::Leave(len) => {
                    path.truncate(len);
                    continue;
                }
                Step::Enter(node, depth) => (node, depth),
            };
            if depth >= MAX_TREE_DEPTH {
                anyhow::bail!("TreeRef chain deeper than {MAX_TREE_DEPTH} in field '{field}'");
            }
            let node = self.node(node);
            let parent_len = path.len();
            // A root's path is its bare name; every other node joins with '/' —
            // the convention `path_of` / `paths_of` use, so the two agree.
            if depth > 0 {
                path.push('/');
            }
            path.push_str(&node.name.display());
            if pred(&path) {
                matched.insert(node.uuid);
            }
            stack.push(Step::Leave(parent_len));
            for &child in node.children.values() {
                stack.push(Step::Enter(child, depth + 1));
            }
        }
        let mut out: Vec<Uuid> = matched.into_iter().collect();
        // The caller rewrites this into a `uuid_in` leaf, and a cursor is bound
        // to a hash of the rewritten query: an unordered set would break page 2
        // (see docs/spec-query.org "Pagination").
        out.sort_unstable();
        Ok(Some(out))
    }

    /// [`Self::path_matches`], from the store when the forest is not resident
    /// (a key-value repository keeps no tree cache, spec-storage increment 4
    /// e): the same walk, over the store's positions.
    pub fn path_matches_with(
        &self,
        store: &dyn Rows,
        field: &str,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Uuid>> {
        if let Some(out) = self.path_matches(field, pred)? {
            return Ok(out);
        }
        let mut matched = HashSet::new();
        walk_stored(store, field, (), &mut |(), uuid, path| {
            if pred(path) {
                matched.insert(uuid);
            }
            Some(())
        })?;
        let mut out: Vec<Uuid> = matched.into_iter().collect();
        out.sort_unstable();
        Ok(out)
    }

    /// [`Self::path_matches_with`], seeking in the store what `seek` says can
    /// match instead of walking the whole stored forest (spec-storage "The
    /// forest"): an exact path costs its depth, a prefix the subtrees it
    /// reaches. Visits and paths are the walk's, so the answer is too.
    pub fn path_matches_seeking(
        &self,
        store: &dyn Rows,
        field: &str,
        seek: PathSeek<'_>,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Uuid>> {
        if let Some(out) = self.path_matches(field, pred)? {
            return Ok(out);
        }
        let mut matched = HashSet::new();
        match seek {
            PathSeek::Exact(path) => {
                if let Some(node) = seek_stored(store, field, path)? {
                    if pred(path) {
                        matched.insert(node);
                    }
                }
            }
            PathSeek::Prefix(prefix) => {
                // The node holding the prefix's last separator, and the start
                // of the names below it the prefix goes on with.
                let (above, start) = match prefix.rfind('/') {
                    None => (None, prefix),
                    Some(i) => (Some(&prefix[..i]), &prefix[i + 1..]),
                };
                let (parent, depth) = match above {
                    None => (None, 1),
                    Some(dir) => match seek_stored(store, field, dir)? {
                        Some(node) => (Some(node), dir.split('/').count() + 1),
                        None => return Ok(Vec::new()),
                    },
                };
                for (child, name) in children_starting(store, field, parent, start)? {
                    let path = match above {
                        None => name.clone(),
                        Some(dir) => format!("{dir}/{name}"),
                    };
                    if pred(&path) {
                        matched.insert(child);
                    }
                    walk_stored_from(store, field, (child, path, (), depth), &mut |(), u, p| {
                        if pred(p) {
                            matched.insert(u);
                        }
                        Some(())
                    })?;
                }
            }
            PathSeek::Below(may_hold) => {
                walk_stored(store, field, (), &mut |(), uuid, path| {
                    if pred(path) {
                        matched.insert(uuid);
                    }
                    may_hold(&format!("{path}/")).then_some(())
                })?;
            }
        }
        let mut out: Vec<Uuid> = matched.into_iter().collect();
        out.sort_unstable();
        Ok(out)
    }

    /// [`Self::osm_path_matches`], from the store when the forest is not
    /// resident. A branch that has consumed every term matches whole, as in
    /// the resident walk.
    pub fn osm_path_matches_with(
        &self,
        store: &dyn Rows,
        field: &str,
        terms: &[String],
    ) -> Result<Vec<Uuid>> {
        if let Some(out) = self.osm_path_matches(field, terms)? {
            return Ok(out);
        }
        let terms_lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        let mut matched = HashSet::new();
        // `None` below a branch that matched whole: everything there matches.
        let start = Some(metafolder_core::query::OsmProgress::default());
        walk_stored(store, field, start, &mut |at, uuid, path| {
            let at = match at {
                None => None,
                Some(at) => {
                    let lower = path.to_lowercase();
                    let at = metafolder_core::query::osm_advance(&lower, &terms_lower, at);
                    (at.matched < terms_lower.len()).then_some(at)
                }
            };
            if at.is_none() {
                matched.insert(uuid);
            }
            Some(at)
        })?;
        Ok(matched.into_iter().collect())
    }

    /// Adds every metarecord below `node` (excluding it) to `out`.
    fn collect_subtree(&self, node: &Node, out: &mut HashSet<Uuid>) {
        let mut frontier: Vec<usize> = node.children.values().copied().collect();
        while let Some(idx) = frontier.pop() {
            let child = self.node(idx);
            out.insert(child.uuid);
            frontier.extend(child.children.values().copied());
        }
    }

    /// Collects all descendants of a metarecord (excluding itself), walking the
    /// tree breadth-first from the database.
    pub fn descendants(&mut self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Vec<Uuid>> {
        if self.complete {
            return Ok(self.descendants_in_cache(field, uuid));
        }
        self.misses += 1;
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut frontier = vec![uuid];
        visited.insert(uuid);
        while let Some(node) = frontier.pop() {
            for (child, _name) in store.children(field, node)? {
                if visited.insert(child) {
                    result.push(child);
                    frontier.push(child);
                }
            }
        }
        Ok(result)
    }

    /// The direct children of `uuid` in `field`'s forest as `(name, child_uuid)`
    /// pairs — the one-level counterpart of [`Self::descendants`]. Served from
    /// memory while the cache is complete, else one DB query. Lets a caller list
    /// a directory's tracked entries (names + metarecords) without a query and a
    /// per-record fetch of each child.
    pub fn children_of(
        &mut self,
        store: &dyn Rows,
        field: &str,
        uuid: Uuid,
    ) -> Result<Vec<(String, Uuid)>> {
        if self.complete {
            return Ok(self.children_of_in_cache(field, uuid));
        }
        self.misses += 1;
        // `tree_children` yields `(child_uuid, name)`; expose `(name, child_uuid)`.
        Ok(store.children(field, uuid)?.into_iter().map(|(u, n)| (n, u)).collect())
    }

    fn children_of_in_cache(&self, field: &str, uuid: Uuid) -> Vec<(String, Uuid)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let Some(ft) = self.fields.get(field) else {
            return out;
        };
        let Some(starts) = ft.by_uuid.get(&uuid) else {
            return out;
        };
        for &start in starts {
            for &child in self.node(start).children.values() {
                let node = self.node(child);
                if seen.insert(node.uuid) {
                    out.push((node.name.display().into_owned(), node.uuid));
                }
            }
        }
        out
    }

    /// Notifies the cache that a metarecord was inserted under `parent`. An
    /// uncached parent does not lose the position: it waits for it
    /// ([`Self::link`]).
    pub fn apply_insert(&mut self, field: &str, parent: Option<Uuid>, name: &TreeName, uuid: Uuid) {
        if !self.resident {
            return;
        }
        let norm = self.normalize(name);
        let taken = match parent.and_then(|p| self.first_node_of(field, p)) {
            Some(parent_idx) => self.node(parent_idx).children.contains_key(&norm),
            None if parent.is_none() => {
                self.fields.get(field).is_some_and(|ft| ft.roots.contains_key(&norm))
            }
            None => false,
        };
        if !taken {
            self.insert_node(field, parent, name, uuid);
        }
    }

    /// Notifies the cache that a metarecord was renamed and/or moved. The cached
    /// subtree follows its directory when the new parent is cached too.
    pub fn apply_rename(
        &mut self,
        field: &str,
        uuid: Uuid,
        new_parent: Option<Uuid>,
        new_name: &TreeName,
    ) {
        if !self.resident {
            return;
        }
        let nodes = self.fields.get(field).and_then(|ft| ft.by_uuid.get(&uuid)).cloned();
        let Some(nodes) = nodes else {
            return;
        };
        if nodes.len() != 1 {
            // Multi-position metarecord: drop all cached positions; the new one
            // will be lazily reloaded on the next resolution. We can no longer
            // prove the forest is fully resident, so leave complete mode.
            self.complete = false;
            for idx in nodes {
                self.remove_subtree(field, idx);
            }
            return;
        }
        let idx = nodes[0];
        self.detach(field, idx);
        self.node_mut(idx).name = new_name.clone();
        // A destination that holds no position yet used to cost the subtree —
        // dropped, and the whole cache declared incomplete. It waits instead.
        self.link(field, idx, new_parent);
    }

    /// Notifies the cache that a metarecord left the tree; drops its subtree.
    pub fn apply_remove(&mut self, field: &str, uuid: Uuid) {
        if !self.resident {
            return;
        }
        let nodes = self.fields.get(field).and_then(|ft| ft.by_uuid.get(&uuid)).cloned();
        for idx in nodes.unwrap_or_default() {
            self.remove_subtree(field, idx);
        }
    }

    /// Follows a revision — or any run of operations — through the forest.
    ///
    /// Each [`TreeOp`] says what one operation did to one `(field name,
    /// metarecord)` cell, taken from the rows that operation moved: nothing is
    /// read back from the database here, and this function takes no connection
    /// to read it with. The producers are interchangeable — a [`crate::log::Writer`]
    /// records its own as it goes, and the coordinated navigation derives the
    /// same thing from an operation read back out of the log.
    ///
    /// Returns `false` only when the cache is not resident, which through the
    /// API never happens: a repository serves nothing before its initial load
    /// (spec-main "POST /repos/load"). Every other shape is settled here.
    ///
    /// The result is the forest a fresh [`Self::populate`] would build, and the
    /// tests assert exactly that. The one place they can differ is degenerate
    /// and predates this: two siblings whose names differ only in case, on a
    /// case-insensitive repository, collide in the children map (which is keyed
    /// by *normalized* bytes while the database's uniqueness is on the exact
    /// ones), and which of the two survives then depends on the order they are
    /// placed in. A load has the same collision, and resolves it by row id.
    pub fn apply_ops(&mut self, ops: &[TreeOp]) -> bool {
        if !self.resident {
            return true; // nothing kept, nothing to keep up
        }
        if !self.complete {
            return false;
        }

        // Phase 1 — unlink every cell the run touches, keeping the nodes and
        // the subtrees hanging off them, and remember what each held. Emptying
        // all of the name slots before filling any is what lets phase 2 ignore
        // the order names are taken and released in: two siblings that swap
        // names have no valid one-at-a-time order. And the remembered positions
        // are what an `Add` or a `Remove` needs — it names the positions it
        // moves, not the ones it leaves alone.
        let mut held: HashMap<(String, Uuid), Vec<TreePos>> = HashMap::new();
        for op in ops {
            let key = (op.field().to_string(), op.uuid());
            if held.contains_key(&key) {
                continue;
            }
            held.insert(key, self.positions_of(op.field(), op.uuid()));
            for idx in self.nodes_of(op.field(), op.uuid()) {
                self.detach(op.field(), idx);
            }
        }

        // Phase 2 — apply the operations in order, each against what its cell
        // holds after the ones before it.
        for op in ops {
            let key = (op.field().to_string(), op.uuid());
            let positions = held.entry(key).or_default();
            match op {
                TreeOp::Set { positions: set, .. } => positions.clone_from(set),
                TreeOp::Add { positions: added, .. } => positions.extend(added.iter().cloned()),
                // By value, not by row id: the database refuses two identical
                // positions under one field name, so a position identifies the
                // row that holds it.
                TreeOp::Remove { positions: gone, .. } => {
                    positions.retain(|pos| !gone.contains(pos))
                }
            }
            let positions = positions.clone();
            self.set_positions(op.field(), op.uuid(), &positions);
        }
        true
    }

    /// The positions a cell holds, as the forest has them. The mirror of
    /// [`Self::set_positions`], and what phase 1 above reads before unlinking
    /// anything.
    fn positions_of(&self, field: &str, uuid: Uuid) -> Vec<TreePos> {
        self.nodes_of(field, uuid)
            .into_iter()
            .map(|idx| {
                let node = self.node(idx);
                let parent = match node.place {
                    Placement::Under(pi) => Some(self.node(pi).uuid),
                    Placement::Waiting(p) => Some(p),
                    Placement::Root | Placement::Unlinked => None,
                };
                TreePos { row: node.row, parent, name: node.name.clone() }
            })
            .collect()
    }

    fn nodes_of(&self, field: &str, uuid: Uuid) -> Vec<usize> {
        self.fields.get(field).and_then(|ft| ft.by_uuid.get(&uuid)).cloned().unwrap_or_default()
    }

    /// Gives a cell exactly `positions`, unlinking whatever it held.
    ///
    /// The nodes it already had are reused *in order*, so the first one — the
    /// one a load hangs this metarecord's children under — keeps its subtree
    /// through a rename, a move, or a change in how many positions there are.
    /// Positions left over are freed; they carry no children, since a load
    /// places children under the first position only.
    fn set_positions(&mut self, field: &str, uuid: Uuid, positions: &[TreePos]) {
        let was = self.nodes_of(field, uuid);
        // A no-op after phase 1, and the point of this after a second operation
        // on the same cell.
        for &idx in &was {
            self.detach(field, idx);
        }
        // By row id, which is the order a load reads them in.
        let mut positions = positions.to_vec();
        positions.sort_by_key(|pos| pos.row);
        let mut kept = Vec::with_capacity(positions.len());
        for (slot, pos) in positions.iter().enumerate() {
            let idx = match was.get(slot) {
                Some(&idx) => {
                    let node = self.node_mut(idx);
                    node.name = pos.name.clone();
                    node.row = pos.row;
                    idx
                }
                None => self.insert_bare(field, pos.row, &pos.name, uuid),
            };
            self.link(field, idx, pos.parent);
            kept.push(idx);
        }
        for &idx in was.iter().skip(positions.len()) {
            self.remove_subtree_detached(field, idx);
        }
        let entry = self.fields.entry(field.to_string()).or_default();
        let adopt = !kept.is_empty();
        // Before adopting: whoever waits for this metarecord hangs from its
        // *first* position, and that list is what names it.
        entry.set_nodes(uuid, kept);
        if adopt {
            self.adopt(field, uuid);
        }
    }

    /// Drops every cached node.
    pub fn clear(&mut self) {
        self.arena.clear();
        self.free.clear();
        self.fields.clear();
        self.live = 0;
        self.complete = false;
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// Resolves one path component against the database, trying the same byte
    /// readings [`Self::readings`] gives. Returns the name that matched, so the
    /// node is cached under the name it really has rather than under what was
    /// typed.
    fn db_child(
        &self,
        store: &dyn Rows,
        field: &str,
        parent: Option<Uuid>,
        comp: &str,
        form: PathForm,
    ) -> Result<Option<(Uuid, TreeName)>> {
        let mut hits = Vec::new();
        for name in Self::readings(comp, form) {
            // By bytes, always: the text column now holds the *escaped* display,
            // so `caf%E9.mp4` is what a file really named that AND one named
            // with the byte 0xE9 both store — comparing it would confuse the two
            // readings the caller just asked to tell apart.
            let mut found = store.child_by_bytes(field, parent, name.as_bytes())?;
            if found.is_none() && self.case_insensitive {
                // Only a case-insensitive filesystem needs the text compare, for
                // its COLLATE NOCASE; it cannot distinguish the two readings,
                // which is why it is the fallback rather than the rule.
                found =
                    store.child_by_text(field, parent, &name.display(), self.case_insensitive)?;
            }
            if let Some(uuid) = found {
                hits.push((uuid, name));
            }
        }
        // Arbitrated on the uuid: one file reached by both readings is one
        // answer; two different files are none.
        let uuids: Vec<Uuid> = hits.iter().map(|(uuid, _)| *uuid).collect();
        let Resolved::Found(uuid) = Self::arbitrate(&uuids, comp) else {
            return Ok(None);
        };
        Ok(hits.into_iter().find(|(candidate, _)| *candidate == uuid))
    }

    /// The cached child of `parent` a typed component names, or why there is
    /// none — the two readings naming *different* children means the path
    /// designates neither, and picking one would be a silent coin toss.
    fn pick(&self, parent: usize, comp: &str, form: PathForm) -> Resolved<usize> {
        let found: Vec<usize> = Self::readings(comp, form)
            .iter()
            .filter_map(|name| self.node(parent).children.get(&self.normalize(name)).copied())
            .collect();
        Self::arbitrate(&found, comp)
    }

    /// The one match, or why there is none. Both readings landing on the *same*
    /// node is not an ambiguity.
    ///
    /// Telling `Ambiguous` from `Missing` is the whole point: they were one
    /// value once, and the database fallback then re-introduced the very guess
    /// the in-memory side had just refused.
    fn arbitrate<T: Copy + PartialEq>(found: &[T], comp: &str) -> Resolved<T> {
        match found {
            [] => Resolved::Missing,
            [one] => Resolved::Found(*one),
            _ if found.iter().all(|x| x == &found[0]) => Resolved::Found(found[0]),
            _ => {
                crate::diagnostics::warn(
                    "tree cache",
                    format!(
                        "{comp:?} names two different files — one whose name really is that \
                         text, one whose name holds the bytes it escapes; say which with the \
                         `form` parameter"
                    ),
                );
                Resolved::Ambiguous
            }
        }
    }

    /// Resolves a path the daemon built itself, **by exact bytes**.
    ///
    /// Its own walk holds the real name, so it must never land on a file that
    /// merely *displays* the same text: re-parsing the displayed path would let
    /// a file really named `caf%E9.mp4` answer for one named with the byte
    /// `0xE9`, and reconcile would then reuse the wrong metarecord.
    pub fn resolve_rel(
        &mut self,
        store: &dyn Rows,
        field: &str,
        rel: &crate::relpath::RelPath,
    ) -> Result<Option<Uuid>> {
        self.scratch();
        let mut cur = match self.root_node(store, field)? {
            Some(idx) => idx,
            None => return Ok(None),
        };
        for name in rel.components() {
            let norm = self.normalize(name);
            cur = match self.node(cur).children.get(&norm).copied() {
                Some(idx) => idx,
                None => {
                    if self.complete {
                        return Ok(None);
                    }
                    self.misses += 1;
                    let parent_uuid = self.node(cur).uuid;
                    let found = if name.is_exact() {
                        store.child_by_text(
                            field,
                            Some(parent_uuid),
                            &name.display(),
                            self.case_insensitive,
                        )?
                    } else {
                        store.child_by_bytes(field, Some(parent_uuid), name.as_bytes())?
                    };
                    let Some(uuid) = found else {
                        return Ok(None);
                    };
                    self.insert_node_at(field, Some(cur), name, uuid)
                }
            };
        }
        let uuid = self.node(cur).uuid;
        Ok(Some(uuid))
    }

    /// The forest root of `field` (the empty-named node), cached or fetched.
    fn root_node(&mut self, store: &dyn Rows, field: &str) -> Result<Option<usize>> {
        let empty = TreeName::default();
        let norm = self.normalize(&empty);
        if let Some(idx) = self.fields.get(field).and_then(|ft| ft.roots.get(&norm)).copied() {
            return Ok(Some(idx));
        }
        if self.complete {
            return Ok(None);
        }
        self.misses += 1;
        let Some(uuid) = store.child_by_bytes(field, None, empty.as_bytes())? else {
            return Ok(None);
        };
        let idx = self.insert_node(field, None, &empty, uuid);
        Ok(Some(idx))
    }

    /// The byte readings a typed path component can have: what the user typed,
    /// verbatim, and — when it spells the escaped form — the bytes that form
    /// decodes to (spec-data-model "Tree names").
    ///
    /// Both are searched and the results added, because neither reading is
    /// wrong: `%E9.txt` is how an undecodable byte is shown *and* a perfectly
    /// legal file name. Which one the user meant is told apart by the marker on
    /// the answer, not guessed here.
    fn readings(comp: &str, form: PathForm) -> Vec<TreeName> {
        let verbatim = TreeName::from(comp);
        let escaped = metafolder_core::metarecord::escaped_to_bytes(comp).map(TreeName::from_bytes);
        match (form, escaped) {
            (PathForm::Verbatim, _) => vec![verbatim],
            // A component with nothing to decode reads the same either way, and
            // must still resolve: naming the escaped form of `/dir/caf%E9.mp4`
            // says how to read that last component, not that `dir` holds an
            // escape too.
            (PathForm::Escaped, Some(escaped)) => vec![escaped],
            (PathForm::Escaped, None) => vec![verbatim],
            (PathForm::Any, Some(escaped)) => vec![verbatim, escaped],
            (PathForm::Any, None) => vec![verbatim],
        }
    }

    /// The map key for a name (see [`normalize_name`]).
    fn normalize(&self, name: &TreeName) -> Vec<u8> {
        normalize_name(name, self.case_insensitive)
    }

    /// Whether names are compared case-insensitively (the filesystem's own
    /// behaviour, probed at load).
    pub fn is_case_insensitive(&self) -> bool {
        self.case_insensitive
    }

    fn node(&self, idx: usize) -> &Node {
        self.arena[idx].as_ref().expect("dangling tree cache index")
    }

    fn node_mut(&mut self, idx: usize) -> &mut Node {
        self.arena[idx].as_mut().expect("dangling tree cache index")
    }

    fn first_node_of(&self, field: &str, uuid: Uuid) -> Option<usize> {
        self.fields.get(field)?.by_uuid.get(&uuid)?.first().copied()
    }

    /// In-memory equivalent of the DB descendant walk, used while complete.
    /// Walks the cached subtree(s) of every position of `uuid`.
    fn descendants_in_cache(&self, field: &str, uuid: Uuid) -> Vec<Uuid> {
        let Some(ft) = self.fields.get(field) else {
            return Vec::new();
        };
        let Some(starts) = ft.by_uuid.get(&uuid) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut seen_idx = HashSet::new();
        let mut seen_uuid = HashSet::new();
        let mut stack: Vec<usize> = starts.clone();
        while let Some(idx) = stack.pop() {
            if !seen_idx.insert(idx) {
                continue;
            }
            for &child in self.node(idx).children.values() {
                stack.push(child);
                let cu = self.node(child).uuid;
                if seen_uuid.insert(cu) {
                    result.push(cu);
                }
            }
        }
        result
    }

    /// In-memory reconstruction of a node's path by walking parent links up to
    /// a root, used while complete. Mirrors [`Self::path_of`]'s DB walk (the
    /// repo root's empty name yields a leading "/"). The walk is bounded by
    /// `MAX_TREE_DEPTH` like its DB counterpart: the forest invariant forbids
    /// cycles, but a corrupted in-memory forest must degrade (return the partial
    /// path) rather than spin forever.
    /// The full path of a node, or `None` when the walk up reaches a node that
    /// is still *waiting* for its parent: that subtree hangs from a metarecord
    /// with no position of its own, so it is in no path at all — the same
    /// answer the database fallback gives for a stale parent.
    fn path_of_at(&self, mut idx: usize) -> Option<String> {
        let mut components = Vec::new();
        for _ in 0..MAX_TREE_DEPTH {
            let node = self.node(idx);
            components.push(node.name.display().into_owned());
            match node.place {
                Placement::Under(p) => idx = p,
                Placement::Root => {
                    components.reverse();
                    return Some(components.join("/"));
                }
                // The walk reached a node hanging from a metarecord with no
                // position of its own: this subtree is in no path at all.
                Placement::Waiting(_) | Placement::Unlinked => return None,
            }
        }
        crate::diagnostics::error(
            "tree cache",
            format!("BUG: parent chain exceeds {MAX_TREE_DEPTH}; returning partial path"),
        );
        components.reverse();
        Some(components.join("/"))
    }

    fn path_of_in_cache(&self, field: &str, uuid: Uuid) -> Option<String> {
        let idx = self.first_node_of(field, uuid)?;
        self.path_of_at(idx)
    }

    /// In-memory equivalent of the DB [`Self::paths_of`], one root-relative
    /// path per cached position of `uuid`.
    fn paths_of_in_cache(&self, field: &str, uuid: Uuid) -> Vec<String> {
        let Some(ft) = self.fields.get(field) else {
            return Vec::new();
        };
        let Some(idxs) = ft.by_uuid.get(&uuid) else {
            return Vec::new();
        };
        idxs.iter().filter_map(|&idx| self.path_of_at(idx)).collect()
    }

    /// Creates a node for one position, registered by uuid and linked nowhere.
    /// Every way of putting a position into the forest goes through this and
    /// then [`Self::link`] — the load included — so there is one description of
    /// what a position becomes.
    fn insert_bare(&mut self, field: &str, row: i64, name: &TreeName, uuid: Uuid) -> usize {
        let node = Node {
            name: name.clone(),
            uuid,
            row,
            place: Placement::Unlinked,
            children: HashMap::new(),
        };
        let idx = match self.free.pop() {
            Some(slot) => {
                self.arena[slot] = Some(node);
                slot
            }
            None => {
                self.arena.push(Some(node));
                self.arena.len() - 1
            }
        };
        self.live += 1;
        self.fields.entry(field.to_string()).or_default().push_node(uuid, idx);
        idx
    }

    /// Links `idx` where its position says: under `parent`'s first node, or in
    /// the roots map when it has no parent. When the parent metarecord holds no
    /// position yet there is nothing to link to, and the node *waits* for it —
    /// resident, findable by uuid, in no path, which is exactly where a fresh
    /// load leaves it.
    ///
    /// The node must be unlinked (fresh, or [`Self::detach`]ed) when this is
    /// called.
    fn link(&mut self, field: &str, idx: usize, parent: Option<Uuid>) {
        match parent {
            None => self.link_at(field, idx, None),
            Some(p) => match self.first_node_of(field, p) {
                Some(pi) => self.link_at(field, idx, Some(pi)),
                None => {
                    self.node_mut(idx).place = Placement::Waiting(p);
                    self.fields
                        .entry(field.to_string())
                        .or_default()
                        .waiting
                        .entry(p)
                        .or_default()
                        .push(idx);
                }
            },
        }
    }

    /// [`Self::link`] to a node already in hand — the lazy DB fallback walks
    /// down from a *node*, and a multi-position parent would not resolve back
    /// to the one it descended through.
    fn link_at(&mut self, field: &str, idx: usize, parent_idx: Option<usize>) {
        let norm = self.normalize(&self.node(idx).name.clone());
        self.node_mut(idx).place = match parent_idx {
            Some(pi) => Placement::Under(pi),
            None => Placement::Root,
        };
        match parent_idx {
            Some(pi) => {
                let prev = self.node_mut(pi).children.insert(norm, idx);
                debug_assert!(
                    prev.is_none_or(|p| p == idx),
                    "two distinct children share a normalized name under one parent"
                );
            }
            None => {
                let prev =
                    self.fields.entry(field.to_string()).or_default().roots.insert(norm, idx);
                debug_assert!(
                    prev.is_none_or(|p| p == idx),
                    "two distinct roots share a normalized name"
                );
            }
        }
    }

    /// Links everything that was waiting for `uuid`, now that it has a node.
    ///
    /// No recursion: a node that waits is still the root of its own resident
    /// subtree — its children found *it* and hung from it — so linking it
    /// brings the whole subtree along.
    fn adopt(&mut self, field: &str, uuid: Uuid) {
        let Some(waiting) = self.fields.get_mut(field).and_then(|ft| ft.waiting.remove(&uuid))
        else {
            return;
        };
        for idx in waiting {
            // The node may have been freed, or re-placed elsewhere, since it
            // was listed: only one that is still waiting for *this* uuid moves.
            if self.arena.get(idx).is_none_or(Option::is_none) {
                continue;
            }
            if self.node(idx).place != Placement::Waiting(uuid) {
                continue;
            }
            self.link(field, idx, Some(uuid));
        }
    }

    /// Creates a position and links it in one go.
    fn insert_node(
        &mut self,
        field: &str,
        parent: Option<Uuid>,
        name: &TreeName,
        uuid: Uuid,
    ) -> usize {
        let idx = self.insert_bare(field, UNKNOWN_ROW, name, uuid);
        self.link(field, idx, parent);
        self.adopt(field, uuid);
        idx
    }

    /// [`Self::insert_node`] under a node already in hand.
    fn insert_node_at(
        &mut self,
        field: &str,
        parent_idx: Option<usize>,
        name: &TreeName,
        uuid: Uuid,
    ) -> usize {
        let idx = self.insert_bare(field, UNKNOWN_ROW, name, uuid);
        self.link_at(field, idx, parent_idx);
        self.adopt(field, uuid);
        idx
    }

    /// Unlinks a node from wherever it hangs — its parent, the roots map, or
    /// the waiting index — without freeing it or its subtree.
    fn detach(&mut self, field: &str, idx: usize) {
        let (place, norm) = {
            let node = self.node(idx);
            (node.place, self.normalize(&node.name))
        };
        match place {
            Placement::Under(pi) => {
                self.node_mut(pi).children.remove(&norm);
            }
            Placement::Waiting(p) => self.stop_waiting(field, idx, p),
            Placement::Root => {
                if let Some(ft) = self.fields.get_mut(field) {
                    ft.roots.remove(&norm);
                }
            }
            Placement::Unlinked => {}
        }
        self.node_mut(idx).place = Placement::Unlinked;
    }

    /// Takes `idx` out of the waiting index. An arena slot is reused after a
    /// free, so a stale entry here would hand a later node's index to the
    /// wrong parent.
    fn stop_waiting(&mut self, field: &str, idx: usize, parent: Uuid) {
        if let Some(ft) = self.fields.get_mut(field) {
            if let Some(list) = ft.waiting.get_mut(&parent) {
                list.retain(|&n| n != idx);
                if list.is_empty() {
                    ft.waiting.remove(&parent);
                }
            }
        }
        if let Some(Some(node)) = self.arena.get_mut(idx) {
            node.place = Placement::Unlinked;
        }
    }

    fn remove_subtree(&mut self, field: &str, idx: usize) {
        self.detach(field, idx);
        self.remove_subtree_detached(field, idx);
    }

    /// Frees a node and its whole subtree; the node must already be detached.
    fn remove_subtree_detached(&mut self, field: &str, idx: usize) {
        let mut stack = vec![idx];
        while let Some(i) = stack.pop() {
            if let Some(Placement::Waiting(p)) = self.arena[i].as_ref().map(|n| n.place) {
                self.stop_waiting(field, i, p);
            }
            let Some(node) = self.arena[i].take() else {
                continue;
            };
            stack.extend(node.children.values().copied());
            if let Some(ft) = self.fields.get_mut(field) {
                ft.drop_node(node.uuid, i);
            }
            self.free.push(i);
            self.live -= 1;
        }
    }
}

/// Resolver handing out the full-path *sort keys* of a forest's nodes
/// ([`PATH_KEY_SEP`]), for the duration of one query.
///
/// The keys are rebuilt on demand rather than stored in the index: a directory
/// rename changes the path of its whole subtree while touching a single field
/// row, so a materialised key would go stale behind the index's incremental
/// refresh. Rebuilding is cheap because ancestors are memoised — a directory's
/// key is assembled once and then shared by every file in it — while leaves,
/// which are the bulk of a match set and are each needed once, are not kept.
///
/// Without a resident forest (a key-value repository keeps none, spec-storage
/// increment 4 e) the keys are read from a store instead, by the same rule:
/// a position's key is its parent's key — at the parent's first position —
/// then its name; a root's, or a detached node's, is its bare name.
pub struct SortKeys<'a> {
    cache: &'a TreeCache,
    store: Option<&'a dyn Rows>,
    dirs: RefCell<HashMap<usize, Arc<str>>>,
    /// Store mode: a metarecord's key at its first position, memoised.
    firsts: RefCell<HashMap<Uuid, Option<Arc<str>>>>,
    /// Store mode: the first read error, which leaves keys missing.
    error: RefCell<Option<anyhow::Error>>,
}

impl<'a> SortKeys<'a> {
    pub fn new(cache: &'a TreeCache) -> Self {
        Self {
            cache,
            store: None,
            dirs: RefCell::new(HashMap::new()),
            firsts: RefCell::new(HashMap::new()),
            error: RefCell::new(None),
        }
    }

    /// Keys from `cache` while it holds the whole forest, else from `store`.
    pub fn with_store(cache: &'a TreeCache, store: &'a dyn Rows) -> Self {
        Self { store: Some(store), ..Self::new(cache) }
    }

    /// Whether the keys can be served at all: the forest is fully resident
    /// ([`TreeCache::is_complete`]), or a store stands in for it. Checked once
    /// per sort key rather than per metarecord.
    pub fn is_resident(&self) -> bool {
        self.cache.complete || self.store.is_some()
    }

    /// The first read error of the store mode, if any (and forgets it): the
    /// keys it left missing must not be served.
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.borrow_mut().take()
    }

    /// The store behind the keys, when the forest is not resident.
    fn stored(&self) -> Option<&'a dyn Rows> {
        self.store.filter(|_| !self.cache.complete)
    }

    /// A read of the store mode, its error kept.
    fn read<T>(&self, r: Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.error.borrow_mut().get_or_insert(e);
                None
            }
        }
    }

    /// Store mode: the key of `uuid` at its first position (memoised).
    fn first_key(&self, store: &dyn Rows, field: &str, uuid: Uuid) -> Option<Arc<str>> {
        if let Some(k) = self.firsts.borrow().get(&uuid) {
            return k.clone();
        }
        // Up the chain of first positions to a known key or a root, then
        // down again, memoising each.
        let mut chain: Vec<(Uuid, String)> = Vec::new();
        let mut base: Option<Arc<str>> = None;
        let mut cur = uuid;
        let mut ended = false;
        for _ in 0..MAX_TREE_DEPTH {
            let Some(first) = self.read(store.positions(field, cur))?.into_iter().next() else {
                // No position: `cur` is in no path.
                if chain.is_empty() {
                    self.firsts.borrow_mut().insert(uuid, None);
                    return None;
                }
                ended = true;
                break;
            };
            chain.push((cur, first.1));
            match first.0 {
                None => {
                    ended = true;
                    break;
                }
                Some(p) => {
                    if let Some(k) = self.firsts.borrow().get(&p) {
                        base = k.clone();
                        ended = true;
                        break;
                    }
                    cur = p;
                }
            }
        }
        if !ended {
            let e = anyhow::anyhow!("TreeRef chain deeper than {MAX_TREE_DEPTH} for {uuid}");
            self.error.borrow_mut().get_or_insert(e);
            return None;
        }
        let mut key = base;
        for (node, name) in chain.iter().rev() {
            let k = match &key {
                None => Arc::from(name.as_str()),
                Some(parent) => join_key(parent, name),
            };
            self.firsts.borrow_mut().insert(*node, Some(k.clone()));
            key = Some(k);
        }
        key
    }

    /// The sort key `uuid` takes in `field`'s forest for the requested
    /// direction — the multi-map rule over its positions: the smallest path
    /// ascending, the largest descending. `None` when the metarecord is not in
    /// the forest (it then sorts last, like any missing value).
    ///
    /// Only ever called after [`Self::is_resident`]; it returns the chosen key
    /// rather than the list so the common single-position row costs no
    /// allocation beyond its own key.
    pub fn pick(&self, field: &str, uuid: Uuid, want_max: bool) -> Option<Arc<str>> {
        if let Some(store) = self.stored() {
            let positions = self.read(store.positions(field, uuid))?;
            let keys = positions.into_iter().map(|(parent, name)| {
                match parent.and_then(|p| self.first_key(store, field, p)) {
                    Some(pk) => join_key(&pk, &name),
                    None => Arc::from(name.as_str()),
                }
            });
            return if want_max { keys.max() } else { keys.min() };
        }
        let idxs = self.cache.fields.get(field)?.by_uuid.get(&uuid)?;
        let mut best: Option<Arc<str>> = None;
        for &idx in idxs {
            let key = self.key_at(idx);
            let better = match &best {
                None => true,
                Some(b) => {
                    if want_max {
                        key > *b
                    } else {
                        key < *b
                    }
                }
            };
            if better {
                best = Some(key);
            }
        }
        best
    }

    /// The key of one node: its parent's key (memoised) plus its own name. The
    /// repo root's empty name gives the leading separator that mirrors the
    /// leading "/" of `path_of_at`.
    fn key_at(&self, idx: usize) -> Arc<str> {
        let node = self.cache.node(idx);
        match node.place {
            Placement::Under(parent) => join_key(&self.dir_key(parent), &node.name.display()),
            _ => Arc::from(node.name.display().as_ref()),
        }
    }

    /// The memoised key of an ancestor node. Walks up to the nearest node whose
    /// key is already known (or to a root), then fills the chain downward, so a
    /// deep directory is assembled once per query however many files hang off it.
    fn dir_key(&self, idx: usize) -> Arc<str> {
        if let Some(k) = self.dirs.borrow().get(&idx) {
            return k.clone();
        }
        let mut chain = Vec::new();
        // The key of the topmost chain node's parent, when the walk stopped on a
        // memoised ancestor rather than on a root.
        let mut base: Option<Arc<str>> = None;
        let mut cur = idx;
        for _ in 0..MAX_TREE_DEPTH {
            chain.push(cur);
            match self.cache.node(cur).place {
                Placement::Under(parent) => {
                    if let Some(k) = self.dirs.borrow().get(&parent) {
                        base = Some(k.clone());
                        break;
                    }
                    cur = parent;
                }
                _ => break,
            }
        }
        let mut key = base;
        for &i in chain.iter().rev() {
            let name = self.cache.node(i).name.display();
            let k = match &key {
                None => Arc::from(name.as_ref()),
                Some(parent) => join_key(parent, &name),
            };
            self.dirs.borrow_mut().insert(i, k.clone());
            key = Some(k);
        }
        key.expect("the chain holds at least `idx`")
    }
}

/// How a sorted walk of a forest ended ([`SortKeys::walk_sorted`]).
#[derive(Debug, PartialEq, Eq)]
pub enum WalkEnd {
    /// Every node was visited.
    Completed,
    /// The visitor asked to stop.
    Stopped,
    /// The walk cannot reproduce the key order here — the caller sorts
    /// instead: the resume position is gone (or outside the bound), two
    /// siblings display the same name, or a name holds a character that sorts
    /// at or below [`PATH_KEY_SEP`].
    Refused,
}

/// A level of a walk of the stored forest: `parent`'s children, read a page
/// at a time in name order from the store (spec-storage increment 4 e).
struct StoredFrame {
    parent: Uuid,
    /// The last name handed out: the next page starts after it.
    after: Option<Vec<u8>>,
    ready: std::collections::VecDeque<(Uuid, Vec<u8>)>,
    chunk: usize,
    exhausted: bool,
    /// Descending walks only: the node visited once its children are done.
    then: Option<Uuid>,
}

impl StoredFrame {
    fn new(parent: Uuid, after: Option<Vec<u8>>, then: Option<Uuid>) -> StoredFrame {
        StoredFrame {
            parent,
            after,
            ready: std::collections::VecDeque::new(),
            chunk: 16,
            exhausted: false,
            then,
        }
    }
}

/// Siblings in key order, sorted a chunk at a time — a chunk twice the size of
/// the last each time, so a page of a folder of 250 000 entries sorts the first
/// few hundred of them, and a walk through all of it stays O(n log n).
struct Frame {
    rest: Vec<usize>,
    /// The next sorted chunk, reversed (popped from the end).
    ready: Vec<usize>,
    chunk: usize,
    /// The last sibling handed out, which the next chunk's first must follow
    /// strictly (equal names straddle a chunk boundary only there).
    last: Option<usize>,
    /// Descending walks only: the node these siblings are the children of,
    /// visited once they are all done (its key is their common prefix, the
    /// smallest of them all).
    then: Option<usize>,
}

impl Frame {
    fn new(rest: Vec<usize>, then: Option<usize>) -> Frame {
        Frame { rest, ready: Vec::new(), chunk: 128, last: None, then }
    }
}

impl<'a> SortKeys<'a> {
    /// Visits `field`'s forest in sort-key order — ascending: pre-order, each
    /// node's children by name; descending: the mirror, post-order with the
    /// children by descending name (a node's key prefixes, so precedes, its
    /// descendants') — calling `visit` with each metarecord at its
    /// representative position only (the smallest key ascending, the largest
    /// descending, as [`Self::pick`] chooses), until it returns `false`.
    /// Detached nodes are roots by their bare names, as in [`Self::pick`].
    ///
    /// `within` restricts the walk to the descendants of that metarecord (the
    /// caller knows every match is one of them: a path-target follow) — each
    /// of which has its one position, so its one sort key, below it.
    /// `resume = (uuid, key)` starts strictly after the node of `uuid` whose
    /// key is `key` (a keyset cursor). The page strategies of the query index
    /// use this to stop at the page's end (spec-indexing "A page costs the
    /// page").
    pub fn walk_sorted(
        &self,
        field: &str,
        descending: bool,
        within: Option<Uuid>,
        resume: Option<(Uuid, &str)>,
        visit: &mut dyn FnMut(Uuid) -> bool,
    ) -> WalkEnd {
        use std::cmp::Ordering;
        // Without the resident forest the store is walked, below a bound
        // only: a whole-forest walk would have to find the detached nodes,
        // which only a scan does — the caller fetches the keys instead.
        if let Some(store) = self.stored() {
            return match within {
                Some(within) => {
                    self.walk_stored_sorted(store, field, descending, within, resume, visit)
                }
                None => WalkEnd::Refused,
            };
        }
        let Some(tree) = self.cache.fields.get(field) else { return WalkEnd::Completed };
        // The top of the walk: the forest's roots (detached nodes among them),
        // or the children of the bounding metarecord's positions.
        let (top, bound): (Vec<usize>, Option<usize>) = match within {
            None => (
                tree.roots.values().chain(tree.waiting.values().flatten()).copied().collect(),
                None,
            ),
            Some(node) => {
                let Some(idxs) = tree.by_uuid.get(&node) else { return WalkEnd::Completed };
                (self.cache.node(idxs[0]).children.values().copied().collect(), Some(idxs[0]))
            }
        };
        // Which way "after" a sibling lies.
        let after = if descending { Ordering::Less } else { Ordering::Greater };
        let mut stack: Vec<Frame> = Vec::new();
        match resume {
            None => stack.push(Frame::new(top, None)),
            Some((uuid, key)) => {
                let at = tree.by_uuid.get(&uuid).and_then(|idxs| {
                    idxs.iter().copied().find(|&i| self.key_at(i).as_ref() == key)
                });
                let Some(at) = at else { return WalkEnd::Refused };
                // Each ancestor level's siblings still to come, outermost
                // first; ascending, then the resume node's own children
                // (descending, those came before it).
                let mut chain = vec![at];
                while let Placement::Under(p) = self.cache.node(*chain.last().unwrap()).place {
                    if Some(p) == bound {
                        break;
                    }
                    chain.push(p);
                }
                if let Some(b) = bound {
                    // The resume node must lie below the bound.
                    let top_of_chain = self.cache.node(*chain.last().unwrap()).place;
                    if top_of_chain != Placement::Under(b) {
                        return WalkEnd::Refused;
                    }
                }
                chain.reverse();
                let mut level = top;
                let mut parent: Option<usize> = None;
                for &on_path in &chain {
                    let name = self.cache.node(on_path).name.display();
                    let mut later = Vec::new();
                    for i in level {
                        if i == on_path {
                            continue;
                        }
                        match self.cache.node(i).name.display().as_ref().cmp(name.as_ref()) {
                            Ordering::Equal => return WalkEnd::Refused,
                            o if o == after => later.push(i),
                            _ => {}
                        }
                    }
                    let mut frame = Frame::new(later, parent.filter(|_| descending));
                    frame.last = Some(on_path);
                    stack.push(frame);
                    level = self.cache.node(on_path).children.values().copied().collect();
                    parent = Some(on_path);
                }
                if !descending {
                    stack.push(Frame::new(level, None));
                }
            }
        }
        let emit = |idx: usize, visit: &mut dyn FnMut(Uuid) -> bool| {
            !self.is_representative(tree, idx, descending) || visit(self.cache.node(idx).uuid)
        };
        while let Some(frame) = stack.last_mut() {
            let next = match self.next_sibling(frame, descending) {
                Ok(n) => n,
                Err(()) => return WalkEnd::Refused,
            };
            let Some(idx) = next else {
                let done = stack.pop().expect("the frame just read");
                if let Some(p) = done.then {
                    if !emit(p, visit) {
                        return WalkEnd::Stopped;
                    }
                }
                continue;
            };
            let node = self.cache.node(idx);
            let children: Vec<usize> = node.children.values().copied().collect();
            if descending {
                // Its descendants first; itself when they are done.
                if children.is_empty() {
                    if !emit(idx, visit) {
                        return WalkEnd::Stopped;
                    }
                } else {
                    stack.push(Frame::new(children, Some(idx)));
                }
            } else {
                if !emit(idx, visit) {
                    return WalkEnd::Stopped;
                }
                if !children.is_empty() {
                    stack.push(Frame::new(children, None));
                }
            }
        }
        WalkEnd::Completed
    }

    /// Whether one of `uuid`'s positions in `field` has a name `keep` accepts
    /// — the per-record form of the index's name scan (the same display form
    /// of the same names), for the text checks a walked page defers.
    pub fn any_name(&self, field: &str, uuid: Uuid, keep: &dyn Fn(&str) -> bool) -> bool {
        if let Some(store) = self.stored() {
            let positions = self.read(store.positions(field, uuid)).unwrap_or_default();
            return positions.iter().any(|(_, name)| keep(name));
        }
        let idxs = self.cache.fields.get(field).and_then(|ft| ft.by_uuid.get(&uuid));
        idxs.is_some_and(|idxs| idxs.iter().any(|&i| keep(&self.cache.node(i).name.display())))
    }

    /// [`Self::walk_sorted`] over the store, below `within`: each folder's
    /// children read a page at a time in the store's name order, which is the
    /// key order as long as every name is plain UTF-8 above the separator and
    /// short enough to be keyed whole — and as long as every node met holds
    /// one position (a node at two could sort on the other, outside the
    /// bound). Anything else refuses, and the caller fetches the keys.
    fn walk_stored_sorted(
        &self,
        store: &dyn Rows,
        field: &str,
        descending: bool,
        within: Uuid,
        resume: Option<(Uuid, &str)>,
        visit: &mut dyn FnMut(Uuid) -> bool,
    ) -> WalkEnd {
        let mut stack: Vec<StoredFrame> = Vec::new();
        match resume {
            None => stack.push(StoredFrame::new(within, None, None)),
            Some((uuid, key)) => {
                // Down the resume key's components from the bound, one frame
                // per level: its siblings still to come.
                let Some(base) = self.first_key(store, field, within) else {
                    return WalkEnd::Refused;
                };
                let Some(rest) =
                    key.strip_prefix(base.as_ref()).and_then(|r| r.strip_prefix(PATH_KEY_SEP))
                else {
                    return WalkEnd::Refused;
                };
                let mut parent = within;
                let mut above: Option<Uuid> = None;
                for name in rest.split(PATH_KEY_SEP) {
                    let Some(child) =
                        self.read(store.child_by_bytes(field, Some(parent), name.as_bytes()))
                    else {
                        return WalkEnd::Refused;
                    };
                    let Some(child) = child else { return WalkEnd::Refused };
                    let then = above.filter(|_| descending);
                    stack.push(StoredFrame::new(parent, Some(name.as_bytes().to_vec()), then));
                    above = Some(child);
                    parent = child;
                }
                if parent != uuid {
                    return WalkEnd::Refused;
                }
                if !descending {
                    stack.push(StoredFrame::new(uuid, None, None));
                }
            }
        }
        while let Some(frame) = stack.last_mut() {
            if frame.ready.is_empty() && !frame.exhausted {
                let Some(page) = self.read(store.children_page(
                    field,
                    frame.parent,
                    frame.after.as_deref(),
                    descending,
                    frame.chunk,
                )) else {
                    return WalkEnd::Refused;
                };
                frame.exhausted = page.len() < frame.chunk;
                frame.chunk = (frame.chunk * 2).min(4096);
                frame.ready.extend(page);
            }
            let Some((uuid, name)) = frame.ready.pop_front() else {
                let done = stack.pop().expect("the frame just read");
                if let Some(node) = done.then {
                    if !visit(node) {
                        return WalkEnd::Stopped;
                    }
                }
                continue;
            };
            frame.after = Some(name.clone());
            // The store's order is the key order only for such names.
            let plain = std::str::from_utf8(&name).is_ok_and(|n| {
                n.chars().all(|c| c > PATH_KEY_SEP) && n.len() <= 300 && !n.is_empty()
            });
            if !plain {
                return WalkEnd::Refused;
            }
            match self.read(store.positions(field, uuid)) {
                Some(positions) if positions.len() == 1 => {}
                _ => return WalkEnd::Refused,
            }
            if descending {
                // Its descendants first; itself when they are done.
                stack.push(StoredFrame::new(uuid, None, Some(uuid)));
            } else {
                if !visit(uuid) {
                    return WalkEnd::Stopped;
                }
                stack.push(StoredFrame::new(uuid, None, None));
            }
        }
        WalkEnd::Completed
    }

    /// The next sibling of a frame in name order (descending when asked);
    /// `Err` when the order cannot be the key order (equal names, or a
    /// character at or below the separator).
    fn next_sibling(&self, frame: &mut Frame, descending: bool) -> Result<Option<usize>, ()> {
        if let Some(i) = frame.ready.pop() {
            frame.last = Some(i);
            return Ok(Some(i));
        }
        if frame.rest.is_empty() {
            return Ok(None);
        }
        let name = |i: &usize| self.cache.node(*i).name.display();
        let cmp = |a: &usize, b: &usize| {
            let o = name(a).as_ref().cmp(name(b).as_ref());
            if descending {
                o.reverse()
            } else {
                o
            }
        };
        let n = frame.rest.len().min(frame.chunk);
        frame.chunk = frame.chunk.saturating_mul(2);
        if n < frame.rest.len() {
            frame.rest.select_nth_unstable_by(n - 1, cmp);
        }
        let mut chunk: Vec<usize> = frame.rest.drain(..n).collect();
        chunk.sort_unstable_by(cmp);
        let adjacent = frame.last.iter().chain(chunk.first()).copied().collect::<Vec<_>>();
        for w in adjacent.windows(2).chain(chunk.windows(2)) {
            if cmp(&w[0], &w[1]) != std::cmp::Ordering::Less {
                return Err(());
            }
        }
        if chunk.iter().any(|i| name(i).chars().any(|c| c <= PATH_KEY_SEP)) {
            return Err(());
        }
        chunk.reverse();
        frame.ready = chunk;
        let first = frame.ready.pop();
        frame.last = first;
        Ok(first)
    }

    /// Whether `idx` is the position [`Self::pick`] represents its metarecord
    /// by — the smallest key, or the largest when `want_max`; on equal keys the
    /// first in the metarecord's list, as `pick` keeps the first it meets.
    /// Trivially true for the common single-position metarecord.
    fn is_representative(&self, tree: &FieldTree, idx: usize, want_max: bool) -> bool {
        let uuid = self.cache.node(idx).uuid;
        let Some(idxs) = tree.by_uuid.get(&uuid).filter(|idxs| idxs.len() > 1) else {
            return true;
        };
        let mut best: Option<(Arc<str>, usize)> = None;
        for &o in idxs {
            let k = self.key_at(o);
            let better = match &best {
                None => true,
                Some((b, _)) => {
                    if want_max {
                        k > *b
                    } else {
                        k < *b
                    }
                }
            };
            if better {
                best = Some((k, o));
            }
        }
        best.is_some_and(|(_, o)| o == idx)
    }
}

/// Walks `field`'s forest in the store from its roots, depth first, as the
/// resident walks do: a node is visited once per position, with the path of
/// that position (a root's is its bare name, every other joins its parent's
/// with `/`). A node whose parent holds no position is in no path, so it is
/// never reached. `visit` gets the state its parent passed
/// down and returns the state for the node's children, or `None` to skip them.
fn walk_stored<S: Copy>(
    store: &dyn Rows,
    field: &str,
    start: S,
    visit: &mut dyn FnMut(S, Uuid, &str) -> Option<S>,
) -> Result<()> {
    walk_stored_from(store, field, (Uuid::nil(), String::new(), start, 0), visit)
}

/// [`walk_stored`] below one node: `(node, its path, the state it passes
/// down, its depth)`, a root at depth 1 — `Uuid::nil()` at depth 0 being the
/// forest itself.
fn walk_stored_from<S: Copy>(
    store: &dyn Rows,
    field: &str,
    top: (Uuid, String, S, usize),
    visit: &mut dyn FnMut(S, Uuid, &str) -> Option<S>,
) -> Result<()> {
    // (parent, the parent's path, the state it passes down, depth)
    let mut stack: Vec<(Uuid, String, S, usize)> = vec![top];
    while let Some((parent, parent_path, state, depth)) = stack.pop() {
        if depth >= MAX_TREE_DEPTH {
            anyhow::bail!("TreeRef chain deeper than {MAX_TREE_DEPTH} in field '{field}'");
        }
        for (child, name) in store.children(field, parent)? {
            let path = if depth == 0 { name.clone() } else { format!("{parent_path}/{name}") };
            let Some(next) = visit(state, child, &path) else { continue };
            stack.push((child, path, next, depth + 1));
        }
    }
    Ok(())
}

/// What a `:path` predicate lets a walk of the stored forest leave out.
pub enum PathSeek<'a> {
    /// The one path that can match.
    Exact(&'a str),
    /// The text every matching path starts with.
    Prefix(&'a str),
    /// Whether the subtree of a node can hold a match, given the text all its
    /// paths start with (the node's path and the separator).
    Below(&'a dyn Fn(&str) -> bool),
}

/// The node at `path` in the stored forest: one keyed read per component.
fn seek_stored(store: &dyn Rows, field: &str, path: &str) -> Result<Option<Uuid>> {
    let mut parent = None;
    for name in path.split('/') {
        match store.child_by_bytes(field, parent, name.as_bytes())? {
            Some(child) => parent = Some(child),
            None => return Ok(None),
        }
    }
    Ok(parent)
}

/// The children of `parent` (`None`: the roots) whose name starts with
/// `start`, read from the store in name order from there on.
fn children_starting(
    store: &dyn Rows,
    field: &str,
    parent: Option<Uuid>,
    start: &str,
) -> Result<Vec<(Uuid, String)>> {
    let mut out = Vec::new();
    if let Some(child) = store.child_by_bytes(field, parent, start.as_bytes())? {
        out.push((child, start.to_string()));
    }
    let (mut after, mut chunk) = (start.as_bytes().to_vec(), 64);
    loop {
        let page = store.children_page(
            field,
            parent.unwrap_or(Uuid::nil()),
            Some(&after),
            false,
            chunk,
        )?;
        let last_page = page.len() < chunk;
        for (child, name) in page {
            if !name.starts_with(start.as_bytes()) {
                return Ok(out);
            }
            out.push((child, String::from_utf8_lossy(&name).into_owned()));
            after = name;
        }
        if last_page {
            return Ok(out);
        }
        chunk = (chunk * 2).min(4096);
    }
}

fn join_key(parent_key: &str, name: &str) -> Arc<str> {
    let mut key = String::with_capacity(parent_key.len() + name.len() + 1);
    key.push_str(parent_key);
    key.push(PATH_KEY_SEP);
    key.push_str(name);
    key.into()
}
