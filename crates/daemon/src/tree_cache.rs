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
use rusqlite::Connection;
use uuid::Uuid;

use metafolder_core::metarecord::TreeName;
use metafolder_core::query::OsmProgress;

use crate::db;
use crate::log::MAX_TREE_DEPTH;

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

struct Node {
    /// The name's exact bytes — what identifies the node (spec-data-model
    /// "Tree names"). The children/roots maps are keyed by its *normalized*
    /// bytes, which fold case when the filesystem does but never merge two
    /// names that differ in an undecodable byte.
    name: TreeName,
    uuid: Uuid,
    parent: Option<usize>,
    /// The metarecord this node's position names as its parent, when that
    /// metarecord holds no position of its own and there is therefore no node
    /// to hang from. `None` once the node is linked — under a parent or in the
    /// roots map — so a waiting node is never mistaken for a root.
    waiting_for: Option<Uuid>,
    children: HashMap<Vec<u8>, usize>,
    last_used: u64,
}

#[derive(Default)]
struct FieldTree {
    /// Root nodes by normalized name bytes.
    roots: HashMap<Vec<u8>, usize>,
    /// Cached nodes by metarecord UUID. A metarecord with several positions
    /// (multi-map TreeRef) can have several nodes.
    by_uuid: HashMap<Uuid, Vec<usize>>,
    /// Nodes waiting for a parent, by the metarecord uuid they wait for. What
    /// makes the upkeep independent of the order positions arrive in: a child
    /// settled before its parent is linked when the parent shows up, so no
    /// producer has to sort its work — and none of them can
    /// (see [`TreeCache::adopt`]).
    waiting: HashMap<Uuid, Vec<usize>>,
}

/// One `(field name, metarecord)` TreeRef cell being settled by
/// [`TreeCache::apply_cells`]: the nodes it had and the positions the database
/// now holds for it.
struct Cell<'a> {
    field: &'a str,
    uuid: Uuid,
    was: Vec<usize>,
    target: db::TreePositions,
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

pub struct TreeCache {
    arena: Vec<Option<Node>>,
    free: Vec<usize>,
    fields: HashMap<String, FieldTree>,
    clock: u64,
    live: usize,
    case_insensitive: bool,
    misses: u64,
    /// Whether the forest has been loaded ([`Self::populate`]). A repository
    /// serves nothing until it has (spec-main "POST /repos/load"), so through
    /// the API this is always true; it is false only on a cache built directly,
    /// as unit tests do, where the DB fallbacks below still apply. Nothing
    /// clears it any more: there is no eviction to lose a node to.
    complete: bool,
}

impl TreeCache {
    pub fn new(case_insensitive: bool) -> Self {
        Self {
            arena: Vec::new(),
            free: Vec::new(),
            fields: HashMap::new(),
            clock: 0,
            live: 0,
            case_insensitive,
            misses: 0,
            complete: false,
        }
    }

    /// True while the whole forest is resident in memory (see [`Self::populate`]).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Eagerly loads the entire TreeRef forest (all field names) into memory in
    /// a single DB scan, so that subsequent read-side navigation is served
    /// without per-node queries. Replaces any current contents.
    pub fn populate(&mut self, conn: &Connection) -> Result<()> {
        // Timed in two parts (logged when non-trivial): the `load_tree_forest`
        // SQL scan+sort, and the in-memory node linking — a persistent load
        // report, so it is clear which dominates on a large forest.
        let t_scan = std::time::Instant::now();
        let rows = db::load_tree_forest(conn)?;
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
        self.clear();
        self.clock += 1;
        // Pass 1: create one detached node per position, registered by uuid so
        // pass 2 can resolve each child's parent to an arena index. Rows are
        // grouped by uuid, so `by_uuid` preserves position order (id order).
        let mut created: Vec<(usize, Option<Uuid>, String)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let idx = self.insert_bare(&row.field_name, &row.name, row.uuid);
            created.push((idx, row.parent, row.field_name.clone()));
        }
        // Pass 2: link each node under its parent's first position (directories
        // are single-position in practice), or into the roots map. A child whose
        // parent has no TreeRef row of its own is left *waiting* for it — the
        // data-integrity edge, and the state a position restored before its
        // parent's passes through.
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
        conn: &Connection,
        field: &str,
        path: &str,
    ) -> Result<Option<Uuid>> {
        self.resolve_path_as(conn, field, path, PathForm::Any)
    }

    /// [`Self::resolve_path`] restricted to one reading of the typed text
    /// (spec-data-model "Tree names"). Naming a reading is what makes the
    /// lookup unambiguous when a path could designate two different files.
    pub fn resolve_path_as(
        &mut self,
        conn: &Connection,
        field: &str,
        path: &str,
        form: PathForm,
    ) -> Result<Option<Uuid>> {
        self.clock += 1;
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
                let Some((uuid, name)) = self.db_child(conn, field, None, comps[0], form)? else {
                    return Ok(None);
                };
                self.insert_node(field, None, &name, uuid)
            }
        };
        self.touch(cur);

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
                        self.db_child(conn, field, Some(parent_uuid), comp, form)?
                    else {
                        return Ok(None);
                    };
                    self.insert_node_at(field, Some(cur), &name, uuid)
                }
            };
            self.touch(cur);
        }

        let uuid = self.node(cur).uuid;
        Ok(Some(uuid))
    }

    /// Reconstructs the path string of a metarecord by walking up its parents
    /// in the database (first position for multi-map fields).
    pub fn path_of(
        &mut self,
        conn: &Connection,
        field: &str,
        uuid: Uuid,
    ) -> Result<Option<String>> {
        if self.complete {
            return Ok(self.path_of_in_cache(field, uuid));
        }
        self.misses += 1;
        let mut components = Vec::new();
        let mut cur = uuid;
        for _ in 0..MAX_TREE_DEPTH {
            let Some((parent, name)) = db::tree_position(conn, field, cur)? else {
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
    /// position (fields are a multi-map: e.g. hardlinks give several
    /// `mfr_path`). Positions whose parent is not in the forest (stale) are
    /// skipped. The reverse of [`Self::resolve_path`].
    pub fn paths_of(&mut self, conn: &Connection, field: &str, uuid: Uuid) -> Result<Vec<String>> {
        if self.complete {
            return Ok(self.paths_of_in_cache(field, uuid));
        }
        self.misses += 1;
        let mut paths = Vec::new();
        for (parent, name) in db::tree_positions(conn, field, uuid)? {
            match parent {
                None => paths.push(name),
                Some(parent) => {
                    if let Some(parent_path) = self.path_of(conn, field, parent)? {
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
    pub fn descendants(&mut self, conn: &Connection, field: &str, uuid: Uuid) -> Result<Vec<Uuid>> {
        if self.complete {
            return Ok(self.descendants_in_cache(field, uuid));
        }
        self.misses += 1;
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut frontier = vec![uuid];
        visited.insert(uuid);
        while let Some(node) = frontier.pop() {
            for (child, _name) in db::tree_children(conn, field, node)? {
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
        conn: &Connection,
        field: &str,
        uuid: Uuid,
    ) -> Result<Vec<(String, Uuid)>> {
        if self.complete {
            return Ok(self.children_of_in_cache(field, uuid));
        }
        self.misses += 1;
        // `tree_children` yields `(child_uuid, name)`; expose `(name, child_uuid)`.
        Ok(db::tree_children(conn, field, uuid)?.into_iter().map(|(u, n)| (n, u)).collect())
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
        self.clock += 1;
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
        self.clock += 1;
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
        let nodes = self.fields.get(field).and_then(|ft| ft.by_uuid.get(&uuid)).cloned();
        for idx in nodes.unwrap_or_default() {
            self.remove_subtree(field, idx);
        }
    }

    /// Brings the cache back in step with the database after a *manual* API
    /// write, reconciling only the `(field name, metarecord)` TreeRef **cells**
    /// that revision changed. The order they arrive in does not matter: a cell
    /// settled before the one that gives its parent a position waits for it
    /// ([`Self::link`]) and is linked when it arrives.
    ///
    /// Manual writes bypass the incremental upkeep the watcher does, so the
    /// cache used to be rebuilt by a full [`Self::populate`] after every one of
    /// them: a single-row field write — setting a tag's `path`, say — paid one
    /// scan of the whole `field` table, seconds on a large repository, with the
    /// repository connection held for the duration so nothing else could be
    /// read meanwhile.
    ///
    /// Returns `false` only when the cache is not resident, which through the
    /// API never happens: a repository serves nothing before its initial load
    /// (spec-main "POST /repos/load"). Every other shape is settled here — see
    /// the two phases below.
    ///
    /// The result is the forest a fresh [`Self::populate`] would build, and the
    /// tests assert exactly that. The one place they can differ is degenerate
    /// and predates this: two siblings whose names differ only in case, on a
    /// case-insensitive repository, collide in the children map (which is keyed
    /// by *normalized* bytes while the database's uniqueness is on the exact
    /// ones), and which of the two survives then depends on the order they are
    /// placed in. A load has the same collision, and resolves it by row id.
    pub fn apply_cells(&mut self, conn: &Connection, cells: &[(String, Uuid)]) -> Result<bool> {
        if !self.complete {
            return Ok(false);
        }
        self.clock += 1;
        let mut batch = Vec::with_capacity(cells.len());
        // One batched read for the whole revision. Asked per cell, this was the
        // whole cost of settling a large one — 13x a rebuild on a batch the size
        // of the forest, which is what dropping the cell list past a threshold
        // used to hide rather than fix.
        let uuids: Vec<Uuid> = {
            let mut seen = HashSet::with_capacity(cells.len());
            cells.iter().map(|(_, uuid)| *uuid).filter(|uuid| seen.insert(*uuid)).collect()
        };
        let mut positions = db::tree_positions_for(conn, &uuids)?;

        // Phase 1 — unlink every cell being settled, keeping the nodes and the
        // subtrees hanging off them. Emptying all of the slots first is what
        // lets phase 2 ignore the order the names are taken and released in: two
        // siblings that swap names have no valid order otherwise.
        for (field, uuid) in cells {
            let nodes: Vec<usize> = self
                .fields
                .get(field)
                .and_then(|ft| ft.by_uuid.get(uuid))
                .cloned()
                .unwrap_or_default();
            for &idx in &nodes {
                self.detach(field, idx);
                self.node_mut(idx).parent = None;
            }
            let target = positions.remove(&(*uuid, field.clone())).unwrap_or_default();
            batch.push(Cell { field, uuid: *uuid, was: nodes, target });
        }

        // Phase 2 — place each cell, in any order. A cell placed before the one
        // that gives its parent a position waits for it and is linked when it
        // arrives ([`Self::link`], [`Self::adopt`]), so this pass no longer has
        // to sort the batch by the parent relation — and a producer that hands
        // over *part* of a revision, one operation at a time, is settled just
        // as correctly as one that hands over all of it.
        for i in 0..batch.len() {
            self.place(&mut batch, i);
        }
        Ok(true)
    }

    /// Gives `batch[i]` the positions the database now holds for it.
    ///
    /// The nodes it already had are reused *in order*, so the first one — the
    /// one a load hangs this metarecord's children under — keeps its subtree
    /// through a rename, a move, or a change in how many positions there are.
    /// Positions left over are freed; they carry no children, since a load
    /// places children under the first position only.
    fn place(&mut self, batch: &mut [Cell<'_>], i: usize) {
        let (field, uuid) = (batch[i].field.to_string(), batch[i].uuid);
        let was = std::mem::take(&mut batch[i].was);
        let target = std::mem::take(&mut batch[i].target);
        let mut kept = Vec::with_capacity(target.len());
        for (pos, (parent, name)) in target.iter().enumerate() {
            let idx = match was.get(pos) {
                Some(&idx) => {
                    self.node_mut(idx).name = name.clone();
                    idx
                }
                None => self.insert_bare(&field, name, uuid),
            };
            self.link(&field, idx, *parent);
            kept.push(idx);
        }
        for &idx in was.iter().skip(target.len()) {
            self.remove_subtree_detached(&field, idx);
        }
        let entry = self.fields.entry(field.clone()).or_default();
        if kept.is_empty() {
            entry.by_uuid.remove(&uuid);
        } else {
            // Before adopting: whoever waits for this metarecord hangs from its
            // *first* position, and that list is what names it.
            entry.by_uuid.insert(uuid, kept);
            self.adopt(&field, uuid);
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
        conn: &Connection,
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
            let mut found = db::find_tree_child_by_bytes(conn, field, parent, name.as_bytes())?;
            if found.is_none() && self.case_insensitive {
                // Only a case-insensitive filesystem needs the text compare, for
                // its COLLATE NOCASE; it cannot distinguish the two readings,
                // which is why it is the fallback rather than the rule.
                found = db::find_tree_child_opts(
                    conn,
                    field,
                    parent,
                    &name.display(),
                    self.case_insensitive,
                )?;
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
        conn: &Connection,
        field: &str,
        rel: &crate::relpath::RelPath,
    ) -> Result<Option<Uuid>> {
        self.clock += 1;
        let mut cur = match self.root_node(conn, field)? {
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
                        db::find_tree_child_opts(
                            conn,
                            field,
                            Some(parent_uuid),
                            &name.display(),
                            self.case_insensitive,
                        )?
                    } else {
                        db::find_tree_child_by_bytes(
                            conn,
                            field,
                            Some(parent_uuid),
                            name.as_bytes(),
                        )?
                    };
                    let Some(uuid) = found else {
                        return Ok(None);
                    };
                    self.insert_node_at(field, Some(cur), name, uuid)
                }
            };
            self.touch(cur);
        }
        let uuid = self.node(cur).uuid;
        Ok(Some(uuid))
    }

    /// The forest root of `field` (the empty-named node), cached or fetched.
    fn root_node(&mut self, conn: &Connection, field: &str) -> Result<Option<usize>> {
        let empty = TreeName::default();
        let norm = self.normalize(&empty);
        if let Some(idx) = self.fields.get(field).and_then(|ft| ft.roots.get(&norm)).copied() {
            self.touch(idx);
            return Ok(Some(idx));
        }
        if self.complete {
            return Ok(None);
        }
        self.misses += 1;
        let Some(uuid) = db::find_tree_child_by_bytes(conn, field, None, empty.as_bytes())? else {
            return Ok(None);
        };
        let idx = self.insert_node(field, None, &empty, uuid);
        self.touch(idx);
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

    /// The map key for a name: its exact bytes, with the *decodable* runs
    /// lowercased when the filesystem is case-insensitive.
    ///
    /// Folding only what decodes is what keeps two names differing in an
    /// undecodable byte apart: lowercasing the lossy text would map both onto
    /// the same replacement character and merge two distinct files.
    fn normalize(&self, name: &TreeName) -> Vec<u8> {
        let bytes = name.as_bytes();
        if !self.case_insensitive {
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
            if node.waiting_for.is_some() {
                return None;
            }
            components.push(node.name.display().into_owned());
            match node.parent {
                Some(p) => idx = p,
                None => {
                    components.reverse();
                    return Some(components.join("/"));
                }
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
        let mut paths = Vec::new();
        for &idx in idxs {
            let node = self.node(idx);
            if node.waiting_for.is_some() {
                continue;
            }
            match node.parent {
                None => paths.push(node.name.display().into_owned()),
                Some(p) => {
                    // Mirror `path_of_at`: the empty repo-root contributes a
                    // leading "/", so a filesystem path round-trips with the DSL
                    // / `resolve_path` (a named-root forest has no leading "/").
                    let Some(parent_path) = self.path_of_at(p) else { continue };
                    paths.push(format!("{parent_path}/{}", node.name.display()));
                }
            }
        }
        paths
    }

    fn touch(&mut self, idx: usize) {
        let clock = self.clock;
        self.node_mut(idx).last_used = clock;
    }

    /// Creates a node for one position, registered by uuid and linked nowhere.
    /// Every way of putting a position into the forest goes through this and
    /// then [`Self::link`] — the load included — so there is one description of
    /// what a position becomes.
    fn insert_bare(&mut self, field: &str, name: &TreeName, uuid: Uuid) -> usize {
        let node = Node {
            name: name.clone(),
            uuid,
            parent: None,
            waiting_for: None,
            children: HashMap::new(),
            last_used: self.clock,
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
        self.fields
            .entry(field.to_string())
            .or_default()
            .by_uuid
            .entry(uuid)
            .or_default()
            .push(idx);
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
                    let node = self.node_mut(idx);
                    node.parent = None;
                    node.waiting_for = Some(p);
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
        {
            let node = self.node_mut(idx);
            node.parent = parent_idx;
            node.waiting_for = None;
        }
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
            if self.node(idx).waiting_for != Some(uuid) {
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
        let idx = self.insert_bare(field, name, uuid);
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
        let idx = self.insert_bare(field, name, uuid);
        self.link_at(field, idx, parent_idx);
        self.adopt(field, uuid);
        idx
    }

    /// Unlinks a node from wherever it hangs — its parent, the roots map, or
    /// the waiting index — without freeing it or its subtree.
    fn detach(&mut self, field: &str, idx: usize) {
        let (parent, waiting_for, norm) = {
            let node = self.node(idx);
            (node.parent, node.waiting_for, self.normalize(&node.name))
        };
        match (parent, waiting_for) {
            (Some(pi), _) => {
                self.node_mut(pi).children.remove(&norm);
            }
            // Waiting for a parent that has no node: it was never in the roots
            // map, and removing the *root of that name* would evict a stranger.
            (None, Some(p)) => {
                self.stop_waiting(field, idx, p);
            }
            (None, None) => {
                if let Some(ft) = self.fields.get_mut(field) {
                    ft.roots.remove(&norm);
                }
            }
        }
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
            node.waiting_for = None;
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
            if let Some(p) = self.arena[i].as_ref().and_then(|n| n.waiting_for) {
                self.stop_waiting(field, i, p);
            }
            let Some(node) = self.arena[i].take() else {
                continue;
            };
            stack.extend(node.children.values().copied());
            if let Some(ft) = self.fields.get_mut(field) {
                if let Some(list) = ft.by_uuid.get_mut(&node.uuid) {
                    list.retain(|&n| n != i);
                    if list.is_empty() {
                        ft.by_uuid.remove(&node.uuid);
                    }
                }
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
pub struct SortKeys<'a> {
    cache: &'a TreeCache,
    dirs: RefCell<HashMap<usize, Arc<str>>>,
}

impl<'a> SortKeys<'a> {
    pub fn new(cache: &'a TreeCache) -> Self {
        Self { cache, dirs: RefCell::new(HashMap::new()) }
    }

    /// Whether the keys can be served at all — the forest is fully resident
    /// ([`TreeCache::is_complete`]). Checked once per sort key rather than per
    /// metarecord.
    pub fn is_resident(&self) -> bool {
        self.cache.complete
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
        match node.parent {
            None => Arc::from(node.name.display().as_ref()),
            Some(parent) => join_key(&self.dir_key(parent), &node.name.display()),
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
            match self.cache.node(cur).parent {
                None => break,
                Some(parent) => {
                    if let Some(k) = self.dirs.borrow().get(&parent) {
                        base = Some(k.clone());
                        break;
                    }
                    cur = parent;
                }
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

fn join_key(parent_key: &str, name: &str) -> Arc<str> {
    let mut key = String::with_capacity(parent_key.len() + name.len() + 1);
    key.push_str(parent_key);
    key.push(PATH_KEY_SEP);
    key.push_str(name);
    key.into()
}
