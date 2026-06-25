//! In-memory tree cache (spec-file-tracking "Tree Cache"): resolves path
//! strings to metarecord UUIDs without recursive SQL. One cache per repository,
//! shared across all TreeRef field names (the field name is the first level).
//! Starts empty and populates lazily; a min-heap of leaves drives LRU
//! eviction when the node limit is exceeded.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;
use uuid::Uuid;

use crate::db;
use crate::log::MAX_TREE_DEPTH;

/// Default node limit, sized so that the cache stays around the spec's
/// 100 MB default (~200 bytes per node).
pub const DEFAULT_MAX_NODES: usize = 500_000;

struct Node {
    field: String,
    /// Original (on-disk) casing; children/roots maps are keyed normalized.
    name: String,
    uuid: Uuid,
    parent: Option<usize>,
    children: HashMap<String, usize>,
    last_used: u64,
}

#[derive(Default)]
struct FieldTree {
    /// Root nodes by normalized name.
    roots: HashMap<String, usize>,
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
        self.clear();
        let rows = db::load_tree_forest(conn)?;
        if rows.len() > self.max_nodes {
            return Ok(()); // Over budget: stay lazy, DB fallbacks apply.
        }
        self.clock += 1;
        // Pass 1: create one detached node per position, registered by uuid so
        // pass 2 can resolve each child's parent to an arena index. Rows are
        // grouped by uuid, so `by_uuid` preserves position order (id order).
        let mut created: Vec<(usize, Option<Uuid>, String)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let node = Node {
                field: row.field_name.clone(),
                name: row.name.clone(),
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
                    self.fields.entry(field).or_default().roots.insert(norm, idx);
                }
                Some(p) => {
                    let Some(&pidx) =
                        self.fields.get(&field).and_then(|ft| ft.by_uuid.get(&p)).and_then(|v| v.first())
                    else {
                        continue;
                    };
                    self.node_mut(idx).parent = Some(pidx);
                    self.node_mut(pidx).children.insert(norm, idx);
                }
            }
        }
        self.complete = true;
        Ok(())
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

        let root_norm = self.normalize(comps[0]);
        let cached_root =
            self.fields.get(field).and_then(|ft| ft.roots.get(&root_norm)).copied();
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
                self.insert_node(field, None, comps[0], uuid)
            }
        };
        self.touch(cur);

        for comp in &comps[1..] {
            let norm = self.normalize(comp);
            let cached_child = self.node(cur).children.get(&norm).copied();
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
                    self.insert_node(field, Some(cur), comp, uuid)
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
    pub fn path_of(&mut self, conn: &Connection, field: &str, uuid: Uuid) -> Result<Option<String>> {
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
                        let joined = if parent_path.is_empty() {
                            name
                        } else {
                            format!("{parent_path}/{name}")
                        };
                        // `path_of` keeps the empty repo-root as a leading "/";
                        // paths are root-relative for clients, so drop it.
                        paths.push(joined.strip_prefix('/').map(str::to_string).unwrap_or(joined));
                    }
                }
            }
        }
        Ok(paths)
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

    /// Notifies the cache that a metarecord was inserted under `parent`.
    pub fn apply_insert(&mut self, field: &str, parent: Option<Uuid>, name: &str, uuid: Uuid) {
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
    pub fn apply_rename(&mut self, field: &str, uuid: Uuid, new_parent: Option<Uuid>, new_name: &str) {
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
            node.name = new_name.to_string();
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

    fn normalize(&self, name: &str) -> String {
        if self.case_insensitive {
            name.to_lowercase()
        } else {
            name.to_string()
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
            components.push(node.name.clone());
            match node.parent {
                Some(p) => idx = p,
                None => {
                    components.reverse();
                    return components.join("/");
                }
            }
        }
        eprintln!("BUG: tree cache parent chain exceeds {MAX_TREE_DEPTH}; returning partial path");
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
                None => paths.push(node.name.clone()),
                Some(p) => {
                    let parent_path = self.path_of_at(p);
                    let joined = if parent_path.is_empty() {
                        node.name.clone()
                    } else {
                        format!("{parent_path}/{}", node.name)
                    };
                    // Drop the leading "/" the repo root contributes: paths are
                    // root-relative for clients.
                    paths.push(joined.strip_prefix('/').map(str::to_string).unwrap_or(joined));
                }
            }
        }
        paths
    }

    fn touch(&mut self, idx: usize) {
        let clock = self.clock;
        self.node_mut(idx).last_used = clock;
    }

    fn insert_node(&mut self, field: &str, parent: Option<usize>, name: &str, uuid: Uuid) -> usize {
        let node = Node {
            field: field.to_string(),
            name: name.to_string(),
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
