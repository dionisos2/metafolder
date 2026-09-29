//! The forest's questions (spec-file-tracking "Tree Cache"): path strings to
//! metarecords and back, children, descendants, path matches and path sort
//! keys, answered from the store (doc "The forest in the store"). One per
//! repository, shared across all TreeRef field names (the field name is the
//! first level).
//!
//! Despite the name, nothing is cached: every lookup reads the store, so there
//! is nothing to keep in step with a write, nothing to go stale and nothing to
//! lock. A [`TreeCache`] is only the repository's case sensitivity. (A resident
//! forest, kept in step with every write, was the SQLite backend's and went
//! with it in September 2026; the name stayed.)

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
/// displayed, only compared and carried inside opaque cursors — and the oracle
/// builds the identical key (`metafolder-query-oracle`, from this constant).
pub const PATH_KEY_SEP: char = '\u{1}';

/// What resolving one path component yielded.
enum Resolved<T> {
    /// The node it names.
    Found(T),
    /// Nothing by that name.
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
/// where the store does.
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

/// The forest lookups of one repository: its case sensitivity, which decides
/// how a typed name is matched. Holds nothing else (see the module doc).
#[derive(Debug, Clone, Copy)]
pub struct TreeCache {
    case_insensitive: bool,
}

impl TreeCache {
    pub fn new(case_insensitive: bool) -> Self {
        Self { case_insensitive }
    }

    /// Resolves a path string to a metarecord UUID. Path format: components
    /// joined by `/`; the first component is the root's own name (so
    /// filesystem paths start with `/` because the root is named `""`).
    pub fn resolve_path(&self, store: &dyn Rows, field: &str, path: &str) -> Result<Option<Uuid>> {
        self.resolve_path_as(store, field, path, PathForm::Any)
    }

    /// [`Self::resolve_path`] restricted to one reading of the typed text
    /// (spec-data-model "Tree names"). Naming a reading is what makes the
    /// lookup unambiguous when a path could designate two different files.
    pub fn resolve_path_as(
        &self,
        store: &dyn Rows,
        field: &str,
        path: &str,
        form: PathForm,
    ) -> Result<Option<Uuid>> {
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

        let mut cur = None;
        for comp in comps {
            match self.child(store, field, cur, comp, form)? {
                Some(uuid) => cur = Some(uuid),
                None => return Ok(None),
            }
        }
        Ok(cur)
    }

    /// Reconstructs the path string of a metarecord by walking up its parents
    /// in the database.
    pub fn path_of(&self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Option<String>> {
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
    pub fn paths_of(&self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Vec<String>> {
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
    /// match instead of walking the whole stored forest (doc "The forest in the store"): an exact
    /// path costs its depth, a prefix the subtrees it
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
    pub fn descendants(&self, store: &dyn Rows, field: &str, uuid: Uuid) -> Result<Vec<Uuid>> {
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
        &self,
        store: &dyn Rows,
        field: &str,
        uuid: Uuid,
    ) -> Result<Vec<(String, Uuid)>> {
        // `tree_children` yields `(child_uuid, name)`; expose `(name, child_uuid)`.
        Ok(store.children(field, uuid)?.into_iter().map(|(u, n)| (n, u)).collect())
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// The child of `parent` (a root when `None`) a typed component names,
    /// trying the byte readings [`Self::readings`] gives. `None` when there is
    /// none — or when the readings name two different children, which makes
    /// the component designate neither.
    fn child(
        &self,
        store: &dyn Rows,
        field: &str,
        parent: Option<Uuid>,
        comp: &str,
        form: PathForm,
    ) -> Result<Option<Uuid>> {
        let mut hits = Vec::new();
        for name in Self::readings(comp, form) {
            // By bytes, always: the text column now holds the *escaped* display,
            // so `caf%E9.mp4` is what a file really named that AND one named
            // with the byte 0xE9 both store — comparing it would confuse the two
            // readings the caller just asked to tell apart.
            let mut found = store.child_by_bytes(field, parent, name.as_bytes())?;
            if found.is_none() && self.case_insensitive {
                // Only a case-insensitive filesystem needs the text compare, for
                // its case folding; it cannot distinguish the two readings,
                // which is why it is the fallback rather than the rule.
                found =
                    store.child_by_text(field, parent, &name.display(), self.case_insensitive)?;
            }
            hits.extend(found);
        }
        // Arbitrated on the uuid: one file reached by both readings is one
        // answer; two different files are none.
        Ok(match Self::arbitrate(&hits, comp) {
            Resolved::Found(uuid) => Some(uuid),
            Resolved::Missing | Resolved::Ambiguous => None,
        })
    }

    /// The one match, or why there is none. Both readings landing on the *same*
    /// node is not an ambiguity.
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
        &self,
        store: &dyn Rows,
        field: &str,
        rel: &crate::relpath::RelPath,
    ) -> Result<Option<Uuid>> {
        // The forest root: the empty-named node.
        let Some(mut cur) = store.child_by_bytes(field, None, TreeName::default().as_bytes())?
        else {
            return Ok(None);
        };
        for name in rel.components() {
            let found = if name.is_exact() {
                store.child_by_text(field, Some(cur), &name.display(), self.case_insensitive)?
            } else {
                store.child_by_bytes(field, Some(cur), name.as_bytes())?
            };
            match found {
                Some(uuid) => cur = uuid,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
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

    /// Whether names are compared case-insensitively (the filesystem's own
    /// behaviour, probed at load).
    pub fn is_case_insensitive(&self) -> bool {
        self.case_insensitive
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

    /// The first read error, if any (and forgets it): the
    /// keys it left missing must not be served.
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.borrow_mut().take()
    }

    /// A store read, its error kept.
    fn read<T>(&self, r: Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.error.borrow_mut().get_or_insert(e);
                None
            }
        }
    }

    /// The key of `uuid` at its first position (memoised).
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
/// at a time in name order from the store (doc "The forest in the store").
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

/// Walks `field`'s forest in the store from its roots, depth first: a node is
/// visited once per position, with the path of
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
