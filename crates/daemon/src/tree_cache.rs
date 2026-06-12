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
        }
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

    /// Collects all descendants of a metarecord (excluding itself), walking the
    /// tree breadth-first from the database.
    pub fn descendants(&mut self, conn: &Connection, field: &str, uuid: Uuid) -> Result<Vec<Uuid>> {
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
            // will be lazily reloaded on the next resolution.
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
                    // Destination not cached: drop the subtree entirely.
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
            return true;
        }
        false
    }
}
