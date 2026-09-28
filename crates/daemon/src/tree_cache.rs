//! The forest's questions (spec-file-tracking "Tree Cache"): path strings to
//! metarecords and back, children, descendants, path matches and path sort
//! keys, answered from the store (spec-storage increment 4 e). One per
//! repository, shared across all TreeRef field names (the field name is the
//! first level). It keeps nothing between lookups: the nodes one lookup brings
//! in are dropped by the next, so nothing grows and nothing goes stale. A
//! resident forest, kept in step with every write, was the SQLite backend's
//! and went with it (September 2026).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use metafolder_core::metarecord::TreeName;

use crate::log::MAX_TREE_DEPTH;
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
        }
    }

    /// Forgets what the last lookup brought in: nothing is kept between
    /// lookups, so nothing grows and nothing goes stale.
    fn scratch(&mut self) {
        if self.live > 0 {
            self.clear();
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

    /// The nodes of `field`'s forest with a path satisfying `pred`: a walk of
    /// the store's positions.
    pub fn path_matches_with(
        &self,
        store: &dyn Rows,
        field: &str,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Uuid>> {
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

    /// The nodes of `field`'s forest whose path matches `terms` as ordered,
    /// case-insensitive substrings (the `osm` path mode), from the store. A
    /// branch that has consumed every term matches whole.
    pub fn osm_path_matches_with(
        &self,
        store: &dyn Rows,
        field: &str,
        terms: &[String],
    ) -> Result<Vec<Uuid>> {
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

    /// Collects all descendants of a metarecord (excluding itself), walking the
    /// tree breadth-first from the database.
    pub fn descendants(&mut self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Vec<Uuid>> {
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
    /// pairs — the one-level counterpart of [`Self::descendants`]: one store
    /// read. Lets a caller list
    /// a directory's tracked entries (names + metarecords) without a query and a
    /// per-record fetch of each child.
    pub fn children_of(
        &mut self,
        store: &dyn Rows,
        field: &str,
        uuid: Uuid,
    ) -> Result<Vec<(String, Uuid)>> {
        self.misses += 1;
        // `tree_children` yields `(child_uuid, name)`; expose `(name, child_uuid)`.
        Ok(store.children(field, uuid)?.into_iter().map(|(u, n)| (n, u)).collect())
    }

    /// Drops every cached node.
    pub fn clear(&mut self) {
        self.arena.clear();
        self.free.clear();
        self.fields.clear();
        self.live = 0;
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

    /// Creates a node for one position, registered by uuid and linked nowhere.
    /// Every way of putting a position into the forest goes through this and
    /// then [`Self::link`] — the load included — so there is one description of
    /// what a position becomes.
    fn insert_bare(&mut self, field: &str, name: &TreeName, uuid: Uuid) -> usize {
        let node =
            Node { name: name.clone(), uuid, place: Placement::Unlinked, children: HashMap::new() };
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
}

/// Resolver handing out the full-path *sort keys* of a forest's nodes
/// ([`PATH_KEY_SEP`]), for the duration of one query.
///
/// The keys are rebuilt on demand rather than stored: a directory rename
/// changes the path of its whole subtree while touching a single field row, so
/// a materialised key would go stale. They are read from the store, and cheap
/// because ancestors are memoised — a directory's key is assembled once and
/// then shared by every file in it. A position's key is its parent's key — at
/// the parent's first position — then its name; a root's, or a detached
/// node's, is its bare name.
pub struct SortKeys<'a> {
    store: &'a dyn Rows,
    /// A metarecord's key at its first position, memoised.
    firsts: RefCell<HashMap<Uuid, Option<Arc<str>>>>,
    /// The first read error, which leaves keys missing.
    error: RefCell<Option<anyhow::Error>>,
}

impl<'a> SortKeys<'a> {
    /// Keys read from `store`.
    pub fn new(store: &'a dyn Rows) -> Self {
        Self { store, firsts: RefCell::new(HashMap::new()), error: RefCell::new(None) }
    }

    /// The first read error of the store mode, if any (and forgets it): the
    /// keys it left missing must not be served.
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.borrow_mut().take()
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
    /// It returns the chosen key rather than the list so the common
    /// single-position row costs no allocation beyond its own key.
    pub fn pick(&self, field: &str, uuid: Uuid, want_max: bool) -> Option<Arc<str>> {
        let store = self.store;
        let positions = self.read(store.positions(field, uuid))?;
        let keys = positions.into_iter().map(|(parent, name)| {
            match parent.and_then(|p| self.first_key(store, field, p)) {
                Some(pk) => join_key(&pk, &name),
                None => Arc::from(name.as_str()),
            }
        });
        if want_max {
            keys.max()
        } else {
            keys.min()
        }
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
        // The store is walked below a bound only: a whole-forest walk would
        // have to find the detached nodes, which only a scan does — the caller
        // fetches the keys instead.
        match within {
            Some(within) => {
                self.walk_stored_sorted(self.store, field, descending, within, resume, visit)
            }
            None => WalkEnd::Refused,
        }
    }

    /// Whether one of `uuid`'s positions in `field` has a name `keep` accepts
    /// — the per-record form of the index's name scan (the same display form
    /// of the same names), for the text checks a walked page defers.
    pub fn any_name(&self, field: &str, uuid: Uuid, keep: &dyn Fn(&str) -> bool) -> bool {
        let positions = self.read(self.store.positions(field, uuid)).unwrap_or_default();
        positions.iter().any(|(_, name)| keep(name))
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
