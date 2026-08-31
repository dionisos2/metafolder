//! Watch/ignore eligibility (spec-file-tracking "Watch and Ignore"): decides
//! whether a repo-root-relative path should be tracked, from the `mf_watch`
//! and `mf_ignore` fields inherited along the `mfr_path` ancestor chain.

use std::collections::HashMap;

use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::Connection;
use uuid::Uuid;

use crate::db;
use crate::tree_cache::TreeCache;

/// Per-run memoisation for [`is_eligible_cached`]. Ancestor `mf_watch` /
/// `mf_ignore` values and compiled `mf_ignore` regexes are stable for the
/// duration of a reconcile walk, so caching them turns the walk's per-entry
/// cost from O(depth) SQLite queries + a regex recompile each into a handful of
/// lookups. Reused across the whole walk; never persisted.
#[derive(Default)]
pub struct EligibilityCache {
    regex: HashMap<String, Regex>,
    watch: HashMap<Uuid, Option<bool>>,
    ignore: HashMap<Uuid, Vec<String>>,
}

/// Why [`explain`] decided the way it did — the step of the eligibility
/// algorithm (spec-file-tracking "Eligibility algorithm") that settled it.
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

/// Evaluates eligibility for `rel_path` (repo-root-relative, `/`-separated,
/// leading slash; `""` is the root itself). Single-shot: compiles regexes and
/// reads ancestor fields fresh. Hot loops (the reconcile walk) should use
/// [`is_eligible_cached`] with a shared [`EligibilityCache`].
pub fn is_eligible(conn: &Connection, cache: &mut TreeCache, rel_path: &str) -> Result<bool> {
    is_eligible_cached(conn, cache, rel_path, &mut EligibilityCache::default())
}

/// Like [`is_eligible`] but memoising ancestor field reads and compiled regexes
/// in `ec` across calls (spec-tasks "walk perf").
pub fn is_eligible_cached(
    conn: &Connection,
    cache: &mut TreeCache,
    rel_path: &str,
    ec: &mut EligibilityCache,
) -> Result<bool> {
    Ok(explain_cached(conn, cache, rel_path, ec)?.eligible)
}

/// [`explain_cached`] with a throwaway cache.
pub fn explain(conn: &Connection, cache: &mut TreeCache, rel_path: &str) -> Result<Explanation> {
    explain_cached(conn, cache, rel_path, &mut EligibilityCache::default())
}

/// The eligibility algorithm itself, keeping the reason it stopped at. Every
/// eligibility decision in the daemon goes through this function — the verdict
/// and its explanation can therefore never disagree.
pub fn explain_cached(
    conn: &Connection,
    cache: &mut TreeCache,
    rel_path: &str,
    ec: &mut EligibilityCache,
) -> Result<Explanation> {
    let full_idx = rel_path.split('/').count() - 1;
    let chain = ancestor_chain(conn, cache, rel_path)?;
    // The path's own metarecord, when it already exists.
    let own_entry: Option<Uuid> = chain.last().and_then(|(i, u)| (*i == full_idx).then_some(*u));

    // Steps 1–2: nearest metarecord (including the path itself) defining
    // mf_watch. Its component index (`watch_idx`) marks the tracking-scope root:
    // ignore patterns for a descendant are matched *relative to it* (below).
    let mut watch: Option<(usize, Uuid, bool)> = None;
    for (idx, uuid) in chain.iter().rev() {
        if let Some(value) = cached_watch(conn, ec, *uuid)? {
            watch = Some((*idx, *uuid, value));
            break;
        }
    }
    let Some((watch_idx, watch_entry, watch_value)) = watch else {
        // No mf_watch anywhere: opt-in default.
        return Ok(Explanation {
            eligible: false,
            reason: Reason::NoWatch,
            watch_scope: None,
            ignore_source: None,
            pattern: None,
        });
    };
    let scope = prefix_path(rel_path, watch_idx);
    if !watch_value {
        return Ok(Explanation {
            eligible: false,
            reason: Reason::WatchFalse,
            watch_scope: Some(scope),
            ignore_source: None,
            pattern: None,
        });
    }
    // Step 3: mf_watch set directly on the metarecord → tracked unconditionally.
    if own_entry == Some(watch_entry) {
        return Ok(Explanation {
            eligible: true,
            reason: Reason::DirectWatch,
            watch_scope: Some(scope),
            ignore_source: None,
            pattern: None,
        });
    }

    // The path against which ignore patterns are tested: `rel_path` re-anchored
    // at the tracking-scope root (the directly-watched ancestor), i.e. with the
    // watched directory's prefix stripped (spec-file-tracking "Eligibility
    // algorithm"). When the scope root is the repository root (`watch_idx == 0`)
    // this is `rel_path` unchanged. So a directly-watched hidden directory (e.g.
    // `.config`) no longer prunes its own subtree, while patterns like `\.git`
    // still apply *inside* the scope.
    let comps: Vec<&str> = rel_path.split('/').collect();
    let scoped = format!("/{}", comps[watch_idx + 1..].join("/"));

    // Steps 4–5: nearest strict ancestor with mf_ignore rows provides the
    // effective pattern set (sets are replaced, never merged).
    for (i, uuid) in chain.iter().rev() {
        if *i == full_idx && own_entry == Some(*uuid) {
            continue; // The entry itself is excluded from the ignore search.
        }
        let patterns = cached_ignore(conn, ec, *uuid)?;
        if patterns.is_empty() {
            continue;
        }
        let source = prefix_path(rel_path, *i);
        for pattern in &patterns {
            if cached_regex(ec, pattern)?.is_match(&scoped) {
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

/// The `mf_ignore` set that *governs* `rel_path`, and where it comes from
/// (spec-file-tracking "Effective ignore set").
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

/// Resolves the effective ignore set of `rel_path`. Unlike the eligibility walk
/// this *includes* the path itself: the question is "which set governs writes
/// here", not "which set filtered this entry" (the algorithm's step 4 excludes
/// the entry, which only matters for a file being tested).
pub fn effective_ignore(
    conn: &Connection,
    cache: &mut TreeCache,
    rel_path: &str,
) -> Result<EffectiveIgnore> {
    let full_idx = rel_path.split('/').count() - 1;
    let chain = ancestor_chain(conn, cache, rel_path)?;
    for (i, uuid) in chain.iter().rev() {
        let patterns = db::string_fields(conn, *uuid, "mf_ignore")?;
        if patterns.is_empty() {
            continue;
        }
        return Ok(EffectiveIgnore {
            source: Some(prefix_path(rel_path, *i)),
            source_uuid: Some(*uuid),
            direct: *i == full_idx,
            patterns,
        });
    }
    Ok(EffectiveIgnore { source: None, source_uuid: None, direct: false, patterns: Vec::new() })
}

/// The prefix of `rel_path` down to component `idx` — the path of the ancestor
/// the chain's `(idx, uuid)` pair denotes (`""` for the repository root).
fn prefix_path(rel_path: &str, idx: usize) -> String {
    rel_path.split('/').take(idx + 1).collect::<Vec<_>>().join("/")
}

/// The metarecords existing along `rel_path`, as `(component_index, uuid)` from
/// the root down. A TreeRef child requires its parent metarecord, so the chain
/// stops at the first unresolved prefix. `rel_path` is repo-root-relative,
/// `/`-separated, leading slash; `""` is the root.
fn ancestor_chain(
    conn: &Connection,
    cache: &mut TreeCache,
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
pub fn resolve_mf_sync(conn: &Connection, cache: &mut TreeCache, rel_path: &str) -> Result<String> {
    let chain = ancestor_chain(conn, cache, rel_path)?;
    for (_, uuid) in chain.iter().rev() {
        if let Some(v) = db::string_fields(conn, *uuid, "mf_sync")?.into_iter().next() {
            return Ok(if v == "external" { v } else { "internal".to_string() });
        }
    }
    Ok("internal".to_string())
}

/// Cached `mf_watch` of a metarecord.
fn cached_watch(conn: &Connection, ec: &mut EligibilityCache, uuid: Uuid) -> Result<Option<bool>> {
    if let Some(v) = ec.watch.get(&uuid) {
        return Ok(*v);
    }
    let v = db::bool_field(conn, uuid, "mf_watch")?;
    ec.watch.insert(uuid, v);
    Ok(v)
}

/// Cached `mf_ignore` patterns of a metarecord.
fn cached_ignore(conn: &Connection, ec: &mut EligibilityCache, uuid: Uuid) -> Result<Vec<String>> {
    if let Some(v) = ec.ignore.get(&uuid) {
        return Ok(v.clone());
    }
    let v = db::string_fields(conn, uuid, "mf_ignore")?;
    ec.ignore.insert(uuid, v.clone());
    Ok(v)
}

/// Cached compiled `mf_ignore` regex (compiled once per distinct pattern).
/// `Regex` clones share the underlying automaton, so this is cheap.
fn cached_regex(ec: &mut EligibilityCache, pattern: &str) -> Result<Regex> {
    if let Some(re) = ec.regex.get(pattern) {
        return Ok(re.clone());
    }
    let re = crate::regexp::compile(pattern)
        .with_context(|| format!("invalid mf_ignore pattern '{pattern}'"))?;
    ec.regex.insert(pattern.to_string(), re.clone());
    Ok(re)
}
