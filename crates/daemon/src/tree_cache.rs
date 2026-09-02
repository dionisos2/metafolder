//! In-memory tree cache (spec-file-tracking "Tree Cache"): resolves path
//! strings to metarecord UUIDs without recursive SQL. One cache per repository,
//! shared across all TreeRef field names (the field name is the first level).
//! Starts empty and populates lazily; a min-heap of leaves drives LRU
//! eviction when the node limit is exceeded.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use uuid::Uuid;

use metafolder_core::metarecord::TreeName;
use metafolder_core::query::OsmProgress;

use crate::db;
use crate::log::MAX_TREE_DEPTH;

/// Default node limit, sized so that the cache stays around the spec's
/// 100 MB default (~200 bytes per node).
pub const DEFAULT_MAX_NODES: usize = 500_000;

/// Separator joining the components of a *sort key* — the form a `tree_ref`
/// value takes when a query sorts on it (spec-data-model "Sort specification").
///
/// It is deliberately not `/`: a path separator that sorts *below* every
/// character a name can contain turns a plain byte comparison of two keys into a
/// component-by-component comparison of the two paths, so a directory and its
/// contents stay together (`photos/2021` before `photos-old`, which a literal
/// `/` would interleave since `-` < `/`). Keys are internal — they are never
/// displayed, only compared and carried inside opaque cursors — and the SQL
/// engine builds the identical key (`query_exec::path_key_cte`).
pub const PATH_KEY_SEP: char = '\u{1}';

struct Node {
    field: String,
    /// The name's exact bytes — what identifies the node (spec-data-model
    /// "Tree names"). The children/roots maps are keyed by its *normalized*
    /// bytes, which fold case when the filesystem does but never merge two
    /// names that differ in an undecodable byte.
    name: TreeName,
    uuid: Uuid,
    parent: Option<usize>,
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
}

pub struct TreeCache {
    arena: Vec<Option<Node>>,
    free: Vec<usize>,
    fields: HashMap<String, FieldTree>,
    /// Lazy LRU heap of (last_used, node) candidates; stale metarecords are
    /// discarded or re-pushed at pop time.
    heap: BinaryHeap<Reverse<(u64, usize)>>,
    clock: u64,
    live: usize,
    max_nodes: usize,
    case_insensitive: bool,
    misses: u64,
    /// True when the entire TreeRef forest is resident in memory — eagerly
    /// loaded by [`Self::populate`] at repository load and kept in sync by the
    /// `apply_*` maintenance since. While complete, every read-side navigation
    /// (resolution, descendants, path reconstruction) is answered purely from
    /// memory; it drops back to `false` if eviction or a drop-and-reload
    /// shortcut ever removes a node we cannot prove is gone from the tree, in
    /// which case the DB fallbacks resume (correctness over speed).
    complete: bool,
}

impl TreeCache {
    pub fn new(case_insensitive: bool) -> Self {
        Self::with_limit(case_insensitive, DEFAULT_MAX_NODES)
    }

    pub fn with_limit(case_insensitive: bool, max_nodes: usize) -> Self {
        Self {
            arena: Vec::new(),
            free: Vec::new(),
            fields: HashMap::new(),
            heap: BinaryHeap::new(),
            clock: 0,
            live: 0,
            max_nodes,
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
    /// without per-node queries. A forest larger than the node budget is left
    /// in lazy mode (`is_complete()` stays `false`) and the DB fallbacks remain
    /// in use. Replaces any current contents.
    pub fn populate(&mut self, conn: &Connection) -> Result<()> {
        // Timed in two parts (logged when non-trivial): the `load_tree_forest`
        // SQL scan+sort, and the in-memory node linking — a persistent load
        // report, so it is clear which dominates on a large forest.
        let t_scan = std::time::Instant::now();
        let rows = db::load_tree_forest(conn)?;
        let scan = t_scan.elapsed();
        let n = rows.len();
        let t_link = std::time::Instant::now();
        let linked = self.populate_from_forest(rows);
        let link = t_link.elapsed();
        if linked && (scan + link).as_millis() >= 200 {
            eprintln!("[tree cache] {n} nodes: scan {scan:?}, link {link:?}");
        }
        Ok(())
    }

    /// Populates the cache from a forest already read out of the `field` table
    /// (`db::TreeRow`s in `field.id` order), skipping the DB scan `populate`
    /// does — used at load, where the index build's single pass over `field`
    /// collects them (see `RepoIndex::build_reported_collecting`). Returns
    /// whether the forest fit the node budget (else the cache stays lazy).
    /// Replaces any current contents.
    pub fn populate_from_forest(&mut self, rows: Vec<db::TreeRow>) -> bool {
        self.clear();
        if rows.len() > self.max_nodes {
            return false; // Over budget: stay lazy, DB fallbacks apply.
        }
        self.clock += 1;
        // Pass 1: create one detached node per position, registered by uuid so
        // pass 2 can resolve each child's parent to an arena index. Rows are
        // grouped by uuid, so `by_uuid` preserves position order (id order).
        let mut created: Vec<(usize, Option<Uuid>, String)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let node = Node {
                field: row.field_name.clone(),
                name: TreeName::from(row.name.clone()),
                uuid: row.uuid,
                parent: None,
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
                .entry(row.field_name.clone())
                .or_default()
                .by_uuid
                .entry(row.uuid)
                .or_default()
                .push(idx);
            self.heap.push(Reverse((self.clock, idx)));
            created.push((idx, row.parent, row.field_name.clone()));
        }
        // Pass 2: link each node under its parent's first position (directories
        // are single-position in practice), or into the roots map. A child
        // whose parent has no TreeRef row is left detached (data-integrity edge).
        for (idx, parent, field) in created {
            let norm = self.normalize(&self.node(idx).name.clone());
            match parent {
                None => {
                    let prev = self.fields.entry(field).or_default().roots.insert(norm, idx);
                    debug_assert!(
                        prev.is_none() || prev == Some(idx),
                        "two distinct roots share a normalized name in tree cache populate"
                    );
                }
                Some(p) => {
                    let Some(&pidx) = self
                        .fields
                        .get(&field)
                        .and_then(|ft| ft.by_uuid.get(&p))
                        .and_then(|v| v.first())
                    else {
                        continue;
                    };
                    self.node_mut(idx).parent = Some(pidx);
                    let prev = self.node_mut(pidx).children.insert(norm, idx);
                    debug_assert!(
                        prev.is_none() || prev == Some(idx),
                        "two distinct children share a normalized name under one parent in populate"
                    );
                }
            }
        }
        self.complete = true;
        true
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
        self.clock += 1;
        let comps: Vec<&str> = path.split('/').collect();

        let root_norm = self.normalize(&TreeName::from(comps[0]));
        let cached_root = self.fields.get(field).and_then(|ft| ft.roots.get(&root_norm)).copied();
        let mut cur = match cached_root {
            Some(idx) => idx,
            None => {
                if self.complete {
                    return Ok(None); // Full forest resident: a cache miss is absence.
                }
                self.misses += 1;
                let found =
                    db::find_tree_child_opts(conn, field, None, comps[0], self.case_insensitive)?;
                let Some(uuid) = found else {
                    return Ok(None);
                };
                self.insert_node(field, None, &TreeName::from(comps[0]), uuid)
            }
        };
        self.touch(cur);

        for comp in &comps[1..] {
            let norm = self.normalize(&TreeName::from(*comp));
            let cached_child = self
                .node(cur)
                .children
                .get(&norm)
                .copied()
                .or_else(|| self.child_by_display(cur, comp));
            cur = match cached_child {
                Some(idx) => idx,
                None => {
                    if self.complete {
                        return Ok(None); // Full forest resident: a cache miss is absence.
                    }
                    self.misses += 1;
                    let parent_uuid = self.node(cur).uuid;
                    let found = db::find_tree_child_opts(
                        conn,
                        field,
                        Some(parent_uuid),
                        comp,
                        self.case_insensitive,
                    )?;
                    let Some(uuid) = found else {
                        self.evict_to_limit();
                        return Ok(None);
                    };
                    self.insert_node(field, Some(cur), &TreeName::from(*comp), uuid)
                }
            };
            self.touch(cur);
        }

        let uuid = self.node(cur).uuid;
        self.evict_to_limit();
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
        Ok(Some(matched.into_iter().collect()))
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

    /// Notifies the cache that a metarecord was inserted under `parent`.
    pub fn apply_insert(&mut self, field: &str, parent: Option<Uuid>, name: &TreeName, uuid: Uuid) {
        self.clock += 1;
        match parent {
            None => {
                let norm = self.normalize(name);
                if self.fields.get(field).is_none_or(|ft| !ft.roots.contains_key(&norm)) {
                    self.insert_node(field, None, name, uuid);
                }
            }
            Some(p) => {
                let Some(parent_idx) = self.first_node_of(field, p) else {
                    return; // Parent not cached: nothing to maintain.
                };
                let norm = self.normalize(name);
                if !self.node(parent_idx).children.contains_key(&norm) {
                    self.insert_node(field, Some(parent_idx), name, uuid);
                }
            }
        }
        self.evict_to_limit();
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

        let new_parent_idx = match new_parent {
            None => None,
            Some(p) => match self.first_node_of(field, p) {
                Some(pi) => Some(pi),
                None => {
                    // Destination not cached: drop the subtree entirely. (In
                    // complete mode every parent is cached, so this only fires
                    // once already degraded; mark it so reads stay correct.)
                    self.complete = false;
                    self.remove_subtree_detached(field, idx);
                    return;
                }
            },
        };

        let norm = self.normalize(new_name);
        {
            let node = self.node_mut(idx);
            node.name = new_name.clone();
            node.parent = new_parent_idx;
        }
        match new_parent_idx {
            None => {
                self.fields.get_mut(field).unwrap().roots.insert(norm, idx);
            }
            Some(pi) => {
                self.node_mut(pi).children.insert(norm, idx);
            }
        }
    }

    /// Notifies the cache that a metarecord left the tree; drops its subtree.
    pub fn apply_remove(&mut self, field: &str, uuid: Uuid) {
        let nodes = self.fields.get(field).and_then(|ft| ft.by_uuid.get(&uuid)).cloned();
        for idx in nodes.unwrap_or_default() {
            self.remove_subtree(field, idx);
        }
    }

    /// Drops every cached node.
    pub fn clear(&mut self) {
        self.arena.clear();
        self.free.clear();
        self.fields.clear();
        self.heap.clear();
        self.live = 0;
        self.complete = false;
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// A child matched by its *displayed* name rather than by its bytes.
    ///
    /// The only handle anyone has on a file whose name does not decode is what
    /// it is shown as — U+FFFD where the undecodable bytes are — since there is
    /// no exact name to type. Resolution therefore accepts the displayed form,
    /// at the documented price that it is not guaranteed unique: two siblings
    /// differing only in undecodable bytes display alike (spec-data-model
    /// "Tree names"). This is the *fallback*, tried only when the exact byte
    /// key missed, so an ordinary path never pays for the scan.
    fn child_by_display(&self, parent: usize, comp: &str) -> Option<usize> {
        // Only a component that is itself lossy can match a lossy name; every
        // other component would already have matched on its exact bytes.
        if !comp.contains(char::REPLACEMENT_CHARACTER) {
            return None;
        }
        let wanted = self.normalize(&TreeName::from(comp));
        self.node(parent)
            .children
            .values()
            .find(|&&idx| {
                let name = &self.node(idx).name;
                !name.is_exact()
                    && self.normalize(&TreeName::from(name.display().as_ref())) == wanted
            })
            .copied()
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
    fn path_of_at(&self, mut idx: usize) -> String {
        let mut components = Vec::new();
        for _ in 0..MAX_TREE_DEPTH {
            let node = self.node(idx);
            components.push(node.name.display().into_owned());
            match node.parent {
                Some(p) => idx = p,
                None => {
                    components.reverse();
                    return components.join("/");
                }
            }
        }
        crate::diagnostics::error(
            "tree cache",
            format!("BUG: parent chain exceeds {MAX_TREE_DEPTH}; returning partial path"),
        );
        components.reverse();
        components.join("/")
    }

    fn path_of_in_cache(&self, field: &str, uuid: Uuid) -> Option<String> {
        let idx = self.first_node_of(field, uuid)?;
        Some(self.path_of_at(idx))
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
            match node.parent {
                None => paths.push(node.name.display().into_owned()),
                Some(p) => {
                    // Mirror `path_of_at`: the empty repo-root contributes a
                    // leading "/", so a filesystem path round-trips with the DSL
                    // / `resolve_path` (a named-root forest has no leading "/").
                    let parent_path = self.path_of_at(p);
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

    fn insert_node(
        &mut self,
        field: &str,
        parent: Option<usize>,
        name: &TreeName,
        uuid: Uuid,
    ) -> usize {
        let node = Node {
            field: field.to_string(),
            name: name.clone(),
            uuid,
            parent,
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

        let norm = self.normalize(name);
        match parent {
            None => {
                self.fields.entry(field.to_string()).or_default().roots.insert(norm, idx);
            }
            Some(pi) => {
                self.node_mut(pi).children.insert(norm, idx);
            }
        }
        self.fields
            .entry(field.to_string())
            .or_default()
            .by_uuid
            .entry(uuid)
            .or_default()
            .push(idx);
        self.heap.push(Reverse((self.clock, idx)));
        idx
    }

    /// Unlinks a node from its parent (or the roots map), without freeing it.
    fn detach(&mut self, field: &str, idx: usize) {
        let (parent, norm) = {
            let node = self.node(idx);
            (node.parent, self.normalize(&node.name))
        };
        match parent {
            None => {
                if let Some(ft) = self.fields.get_mut(field) {
                    ft.roots.remove(&norm);
                }
            }
            Some(pi) => {
                self.node_mut(pi).children.remove(&norm);
                let parent_node = self.node(pi);
                if parent_node.children.is_empty() {
                    self.heap.push(Reverse((parent_node.last_used, pi)));
                }
            }
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

    fn evict_to_limit(&mut self) {
        while self.live > self.max_nodes && self.evict_one() {}
    }

    /// Pops the least-recently-used leaf and frees it. Stale heap metarecords
    /// (touched since push, no longer a leaf, already freed) are skipped;
    /// touched leaves are re-pushed with their current timestamp.
    fn evict_one(&mut self) -> bool {
        while let Some(Reverse((t, idx))) = self.heap.pop() {
            let Some(node) = self.arena[idx].as_ref() else {
                continue;
            };
            if !node.children.is_empty() {
                continue;
            }
            if node.last_used != t {
                self.heap.push(Reverse((node.last_used, idx)));
                continue;
            }
            let field = node.field.clone();
            self.detach(&field, idx);
            self.remove_subtree_detached(&field, idx);
            // The forest no longer fits: we can no longer answer reads purely
            // from memory, so resume the DB fallbacks.
            self.complete = false;
            return true;
        }
        false
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
