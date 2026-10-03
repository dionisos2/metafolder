//! The pure half of sync (doc "Change detection in sync"): a link's snapshot,
//! the translation of a value from one repository's perspective to the other's,
//! and the three-way decisions — per field, and per *aspect* of a file (its
//! content, its mode). Nothing here talks to the daemon or the disk: the
//! planner and the run feed it records they read, and a [`Translate`] that
//! answers "which record is this one's counterpart".

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::metarecord::{MetaRecord, Value};

use super::SyncError;

/// One of the two repositories of a pair, in canonical order (the smaller
/// uuid is `A`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    A,
    B,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::A => Side::B,
            Side::B => Side::A,
        }
    }

    /// `"a"` / `"b"`, as the plan repo and the daemon spell it.
    pub fn name(self) -> &'static str {
        match self {
            Side::A => "a",
            Side::B => "b",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "a" => Some(Side::A),
            "b" => Some(Side::B),
            _ => None,
        }
    }

    fn index(self) -> usize {
        match self {
            Side::A => 0,
            Side::B => 1,
        }
    }
}

/// Whether the metadata diff compares and writes this field: user fields, the
/// `mf_*` settings, and `mfr_path` — but no other `mfr_*` field, which each
/// repository derives from its own files (doc "What sync copies").
pub fn is_synced_field(name: &str) -> bool {
    !name.starts_with("mfr_") || name == "mfr_path"
}

/// The stat fields that say whether a file's *content* changed on one side:
/// the same evidence the watcher's refresh relies on (doc "Change detection in
/// sync").
pub const CONTENT_STAMP: &[&str] = &["mfr_type", "mfr_size", "mfr_mtime", "mfr_symlink_target"];

/// The stat field that says whether a file's *mode* changed on one side.
pub const MODE_STAMP: &[&str] = &["mfr_permissions"];

/// The pseudo-field a content conflict is planned under: there is no field to
/// hold a file's bytes, and a `[[conflict]]` rule must be able to name it.
pub const CONTENT_FIELD: &str = "mfr_content";

/// A total order on values, for comparing multisets.
fn key(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Values in canonical order, each once (a field never holds a value twice —
/// doc "No duplicate rows").
pub fn canon(mut values: Vec<Value>) -> Vec<Value> {
    values.sort_by_key(key);
    values.dedup_by(|x, y| key(x) == key(y));
    values
}

/// A record's values for `name`, canonical.
pub fn values_of(rec: &MetaRecord, name: &str) -> Vec<Value> {
    canon(rec.get_all(name).into_iter().cloned().collect())
}

/// A value as one line of text, for a person: a string as it is, anything else
/// as its JSON value (a reference as the uuid it names).
pub fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Nothing => "nothing".into(),
        other => match serde_json::to_value(other) {
            Ok(j) => match &j["value"] {
                Json::String(s) => s.clone(),
                inner => inner.to_string(),
            },
            Err(_) => String::new(),
        },
    }
}

/// The synced field names a record carries.
pub fn synced_names(rec: &MetaRecord) -> BTreeSet<String> {
    rec.fields.iter().map(|f| f.name.clone()).filter(|n| is_synced_field(n)).collect()
}

/// Whether the record sits at a position of the filesystem forest — has a
/// file, or had one the last time it was looked at.
pub fn has_real_path(rec: &MetaRecord) -> bool {
    matches!(rec.get("mfr_path"), Some(Value::TreeRef { .. }))
}

// ── The snapshot ────────────────────────────────────────────────────────────

/// A link's snapshot: what the two repositories held at the last sync.
///
/// `common` is what they agreed on, per field name, each value in both
/// perspectives — repo A's local value and repo B's — so the next diff compares
/// each side with its own past without translating anything. `sides` holds what
/// each held alone: the stamps of what sync does not equalize (a file's size,
/// mtime, mode).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub common: BTreeMap<String, Vec<(Value, Value)>>,
    pub sides: [BTreeMap<String, Value>; 2],
}

impl Snapshot {
    /// Decodes `GET /sync/…/links/:link`'s `snapshot` array.
    ///
    /// The B perspective of a common entry is rebuilt from `value_b`: the target
    /// of a `ref`, the parent of a `tree_ref`; any other value is the same on
    /// both sides. An entry from before `mfr_path` was snapshotted as a
    /// `tree_ref` — a plain string — is left out: the next sync of that link
    /// compares the two paths afresh.
    pub fn from_wire(entries: &[Json]) -> Snapshot {
        let mut snap = Snapshot::default();
        for e in entries {
            let Some(name) = e["name"].as_str() else { continue };
            let Ok(value) = serde_json::from_value::<Value>(e["value"].clone()) else { continue };
            if let Some(side) = e["side"].as_str().and_then(Side::parse) {
                snap.sides[side.index()].insert(name.to_string(), value);
                continue;
            }
            if name == "mfr_path" && !matches!(value, Value::TreeRef { .. } | Value::Nothing) {
                continue; // the old string form
            }
            let value_b = e["value_b"].as_str().and_then(|s| Uuid::parse_str(s).ok());
            let b = match &value {
                Value::Ref(_) => match value_b {
                    Some(u) => Value::Ref(u),
                    None => continue, // no B perspective was ever recorded
                },
                Value::TreeRef { name: n, .. } => {
                    Value::TreeRef { parent: value_b, name: n.clone() }
                }
                other => other.clone(),
            };
            snap.common.entry(name.to_string()).or_default().push((value, b));
        }
        snap
    }

    /// Encodes the snapshot as `POST …/links/commit` takes it.
    pub fn to_wire(&self) -> Vec<Json> {
        let mut out = Vec::new();
        for (name, pairs) in &self.common {
            for (a, b) in pairs {
                let value_b = match b {
                    Value::Ref(u) => Some(*u),
                    Value::TreeRef { parent, .. } => *parent,
                    _ => None,
                };
                out.push(json!({
                    "name": name,
                    "value": a,
                    "value_b": value_b.map(|u| u.as_simple().to_string()),
                }));
            }
        }
        for side in [Side::A, Side::B] {
            for (name, value) in &self.sides[side.index()] {
                out.push(json!({"name": name, "value": value, "side": side.name()}));
            }
        }
        out
    }

    /// What `side` held alone at the last sync.
    pub fn side(&self, side: Side) -> &BTreeMap<String, Value> {
        &self.sides[side.index()]
    }

    /// The values `side` held for `name` at the last sync, canonical.
    pub fn perspective(&self, name: &str, side: Side) -> Vec<Value> {
        let pairs = self.common.get(name).map(Vec::as_slice).unwrap_or_default();
        canon(
            pairs
                .iter()
                .map(|(a, b)| match side {
                    Side::A => a.clone(),
                    Side::B => b.clone(),
                })
                .collect(),
        )
    }
}

// ── Translation ─────────────────────────────────────────────────────────────

/// What a counterpart is looked up for: the target of a `ref`, or the parent of
/// a `tree_ref` in `field`'s forest — the path that falls back on differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    Ref,
    Parent { field: &'a str },
}

/// Answers, for a record of one side, its counterpart on the other side (doc
/// "Ref translation during sync"): the record it is linked to, else the one at
/// the same path. `Ok(None)` when there is none.
pub trait Translate {
    fn counterpart(
        &self,
        from: Side,
        record: Uuid,
        how: Lookup<'_>,
    ) -> Result<Option<Uuid>, SyncError>;
}

/// `value`, a value of `field` as `from` holds it, as the other side would
/// hold it. `Ok(None)` when it names a record that has no counterpart.
/// `ExternalRef` and `RefBase` are copied as they are: they already say which
/// repository they mean.
pub fn translate(
    value: &Value,
    from: Side,
    field: &str,
    tr: &dyn Translate,
) -> Result<Option<Value>, SyncError> {
    Ok(match value {
        Value::Ref(u) => tr.counterpart(from, *u, Lookup::Ref)?.map(Value::Ref),
        Value::TreeRef { parent: Some(p), name } => tr
            .counterpart(from, *p, Lookup::Parent { field })?
            .map(|q| Value::TreeRef { parent: Some(q), name: name.clone() }),
        other => Some(other.clone()),
    })
}

/// [`translate`] over a whole value set; `None` as soon as one value has no
/// counterpart.
pub fn translate_all(
    values: &[Value],
    from: Side,
    field: &str,
    tr: &dyn Translate,
) -> Result<Option<Vec<Value>>, SyncError> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        match translate(v, from, field, tr)? {
            Some(t) => out.push(t),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// When the two sides hold the same values for `name` (after translating A's),
/// the snapshot pairs they agree on; `None` when they differ or A's do not
/// translate.
pub fn agree(
    name: &str,
    a: &[Value],
    b: &[Value],
    tr: &dyn Translate,
) -> Result<Option<Vec<(Value, Value)>>, SyncError> {
    let Some(ta) = translate_all(a, Side::A, name, tr)? else { return Ok(None) };
    if canon(ta.clone()) != canon(b.to_vec()) {
        return Ok(None);
    }
    Ok(Some(a.iter().cloned().zip(ta).collect()))
}

// ── Decisions ───────────────────────────────────────────────────────────────

/// What a link needs for one field, or one aspect of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The two sides already agree.
    InSync,
    /// They disagree, yet neither changed since the last sync — something the
    /// last sync left that way (a conflict skipped, a value with no
    /// counterpart). Left alone.
    Untouched,
    /// Only `from` changed: its value goes to the other side.
    Propagate { from: Side },
    /// Both changed, to different values.
    Conflict,
}

/// The three-way decision for field `name` of a link (doc "Change detection in
/// sync"): each side compared with its own perspective of the snapshot, the
/// two sides with each other after translation. With no snapshot entry for the
/// name — a first sync, or a field new to both — a value on one side only is
/// that side's change, and two different values a conflict.
pub fn decide_field(
    name: &str,
    a: &[Value],
    b: &[Value],
    snap: &Snapshot,
    tr: &dyn Translate,
) -> Result<Decision, SyncError> {
    if agree(name, a, b, tr)?.is_some() {
        return Ok(Decision::InSync);
    }
    let a_changed = canon(a.to_vec()) != snap.perspective(name, Side::A);
    let b_changed = canon(b.to_vec()) != snap.perspective(name, Side::B);
    Ok(match (a_changed, b_changed) {
        (false, false) => Decision::Untouched,
        (true, false) => Decision::Propagate { from: Side::A },
        (false, true) => Decision::Propagate { from: Side::B },
        (true, true) => Decision::Conflict,
    })
}

/// A record's stamp: its first value of each of `names`.
pub fn stamp(rec: &MetaRecord, names: &[&str]) -> BTreeMap<String, Value> {
    names.iter().filter_map(|n| rec.get(n).map(|v| (n.to_string(), v.clone()))).collect()
}

/// Whether `rec`'s stamp over `names` differs from what its side held at the
/// last sync. A side the snapshot holds no stamp for at all has changed: there
/// is nothing to say it did not.
pub fn stamp_changed(rec: &MetaRecord, snap: &Snapshot, side: Side, names: &[&str]) -> bool {
    let held = snap.side(side);
    if !names.iter().any(|n| held.contains_key(*n)) {
        return true;
    }
    names.iter().any(|n| rec.get(n) != held.get(*n))
}

/// The three-way decision for an aspect sync does not equalize field by field
/// — a file's content, its mode. `equal` is asked only when a side changed: it
/// may have to read the files.
pub fn decide_aspect(
    a_changed: bool,
    b_changed: bool,
    equal: impl FnOnce() -> Result<bool, SyncError>,
) -> Result<Decision, SyncError> {
    if !a_changed && !b_changed {
        return Ok(Decision::Untouched);
    }
    if equal()? {
        return Ok(Decision::InSync);
    }
    Ok(match (a_changed, b_changed) {
        (true, false) => Decision::Propagate { from: Side::A },
        (false, true) => Decision::Propagate { from: Side::B },
        _ => Decision::Conflict,
    })
}

/// The snapshot a successful sync of a link records: every synced field the
/// two sides now agree on, and each side's stamps.
///
/// A link is committed only once the two sides agree on all of it — a skipped
/// conflict, or a value with no counterpart, leaves the link to the next plan,
/// which then sees the same disagreement and decides the same way. Should a
/// field still disagree, its entries of the `old` snapshot are kept for the
/// same reason: recording either side's value would make the side that did not
/// win look like the one that changed.
pub fn next_snapshot(
    a: &MetaRecord,
    b: &MetaRecord,
    old: &Snapshot,
    tr: &dyn Translate,
) -> Result<Snapshot, SyncError> {
    let mut names = synced_names(a);
    names.extend(synced_names(b));
    names.extend(old.common.keys().cloned());
    let mut next = Snapshot::default();
    for name in names {
        let (va, vb) = (values_of(a, &name), values_of(b, &name));
        match agree(&name, &va, &vb, tr)? {
            Some(pairs) if pairs.is_empty() => {}
            Some(pairs) => {
                next.common.insert(name, pairs);
            }
            None => {
                if let Some(pairs) = old.common.get(&name) {
                    next.common.insert(name, pairs.clone());
                }
            }
        }
    }
    for (side, rec) in [(Side::A, a), (Side::B, b)] {
        next.sides[side.index()] = stamp(rec, CONTENT_STAMP);
        next.sides[side.index()].extend(stamp(rec, MODE_STAMP));
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::metarecord::{Field, TreeName};

    /// Links given as A→B pairs; no path fallback.
    struct Links(HashMap<Uuid, Uuid>);

    impl Translate for Links {
        fn counterpart(
            &self,
            from: Side,
            record: Uuid,
            _: Lookup<'_>,
        ) -> Result<Option<Uuid>, SyncError> {
            Ok(match from {
                Side::A => self.0.get(&record).copied(),
                Side::B => self.0.iter().find(|(_, b)| **b == record).map(|(a, _)| *a),
            })
        }
    }

    fn u(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn rec(fields: &[(&str, Value)]) -> MetaRecord {
        MetaRecord {
            uuid: Uuid::new_v4(),
            version: 1,
            fields: fields.iter().map(|(n, v)| Field::new(*n, v.clone())).collect(),
        }
    }

    fn s(x: &str) -> Value {
        Value::String(x.into())
    }

    fn no_links() -> Links {
        Links(HashMap::new())
    }

    #[test]
    fn a_first_sync_propagates_a_one_sided_value_and_conflicts_on_two() {
        let snap = Snapshot::default();
        let tr = no_links();
        assert_eq!(
            decide_field("tag", &[s("x")], &[], &snap, &tr).unwrap(),
            Decision::Propagate { from: Side::A }
        );
        assert_eq!(
            decide_field("tag", &[s("x")], &[s("x")], &snap, &tr).unwrap(),
            Decision::InSync
        );
        assert_eq!(
            decide_field("tag", &[s("x")], &[s("y")], &snap, &tr).unwrap(),
            Decision::Conflict
        );
    }

    #[test]
    fn a_resync_tells_which_side_changed() {
        let mut snap = Snapshot::default();
        snap.common.insert("tag".into(), vec![(s("old"), s("old"))]);
        let tr = no_links();
        let d = |a: &[Value], b: &[Value]| decide_field("tag", a, b, &snap, &tr).unwrap();
        assert_eq!(d(&[s("new")], &[s("old")]), Decision::Propagate { from: Side::A });
        assert_eq!(d(&[s("old")], &[s("new")]), Decision::Propagate { from: Side::B });
        assert_eq!(d(&[], &[s("old")]), Decision::Propagate { from: Side::A }, "a removal");
        assert_eq!(d(&[s("p")], &[s("q")]), Decision::Conflict);
        assert_eq!(d(&[s("p")], &[s("p")]), Decision::InSync, "the same change on both sides");
    }

    #[test]
    fn refs_compare_through_the_links_and_each_side_with_its_own_past() {
        // A's ref to u(1) is B's ref to u(2): in sync, though the uuids differ.
        let tr = Links(HashMap::from([(u(1), u(2)), (u(3), u(4))]));
        let snap = Snapshot::default();
        let d = |a: Value, b: Value| decide_field("author", &[a], &[b], &snap, &tr).unwrap();
        assert_eq!(d(Value::Ref(u(1)), Value::Ref(u(2))), Decision::InSync);
        assert_eq!(d(Value::Ref(u(3)), Value::Ref(u(2))), Decision::Conflict);

        // After a sync agreeing on u(1)/u(2), A moving to u(3) is A's change.
        let mut snap = Snapshot::default();
        snap.common.insert("author".into(), vec![(Value::Ref(u(1)), Value::Ref(u(2)))]);
        assert_eq!(
            decide_field("author", &[Value::Ref(u(3))], &[Value::Ref(u(2))], &snap, &tr).unwrap(),
            Decision::Propagate { from: Side::A }
        );
    }

    #[test]
    fn a_tree_ref_translates_its_parent_and_keeps_its_name() {
        let tr = Links(HashMap::from([(u(1), u(2))]));
        let name = TreeName::from("jazz");
        let a = Value::TreeRef { parent: Some(u(1)), name: name.clone() };
        let b = Value::TreeRef { parent: Some(u(2)), name: name.clone() };
        assert_eq!(translate(&a, Side::A, "category", &tr).unwrap(), Some(b.clone()));
        assert_eq!(translate(&b, Side::B, "category", &tr).unwrap(), Some(a));
        let unlinked = Value::TreeRef { parent: Some(u(9)), name };
        assert_eq!(translate(&unlinked, Side::A, "category", &tr).unwrap(), None);
    }

    #[test]
    fn the_snapshot_round_trips_both_perspectives_and_the_sides() {
        let mut snap = Snapshot::default();
        snap.common.insert("tag".into(), vec![(s("x"), s("x"))]);
        snap.common.insert("author".into(), vec![(Value::Ref(u(1)), Value::Ref(u(2)))]);
        let name = TreeName::from("f");
        snap.common.insert(
            "mfr_path".into(),
            vec![(
                Value::TreeRef { parent: Some(u(3)), name: name.clone() },
                Value::TreeRef { parent: Some(u(4)), name },
            )],
        );
        snap.sides[0].insert("mfr_size".into(), Value::Int(3));
        snap.sides[1].insert("mfr_size".into(), Value::Int(4));
        assert_eq!(Snapshot::from_wire(&snap.to_wire()), snap);
    }

    #[test]
    fn an_old_string_mfr_path_entry_is_dropped() {
        let wire =
            vec![json!({"name": "mfr_path", "value": {"type": "string", "value": "/a.txt"}})];
        assert!(Snapshot::from_wire(&wire).common.is_empty());
    }

    #[test]
    fn a_field_still_disagreeing_keeps_its_old_entries() {
        let mut old = Snapshot::default();
        old.common.insert("tag".into(), vec![(s("old"), s("old"))]);
        let a = rec(&[("tag", s("jazz")), ("rating", Value::Int(3))]);
        let b = rec(&[("tag", s("rock")), ("rating", Value::Int(3))]);
        let next = next_snapshot(&a, &b, &old, &no_links()).unwrap();
        assert_eq!(next.common["tag"], vec![(s("old"), s("old"))], "still disagreeing");
        assert_eq!(next.common["rating"], vec![(Value::Int(3), Value::Int(3))]);
        // So the next decision is the conflict again, not a propagation.
        assert_eq!(
            decide_field("tag", &[s("jazz")], &[s("rock")], &next, &no_links()).unwrap(),
            Decision::Conflict
        );
    }

    #[test]
    fn stamps_say_which_side_changed_its_content() {
        let a = rec(&[("mfr_size", Value::Int(3)), ("mfr_mtime", Value::DateTime(10))]);
        let b = rec(&[("mfr_size", Value::Int(3)), ("mfr_mtime", Value::DateTime(20))]);
        let old = next_snapshot(&a, &b, &Snapshot::default(), &no_links()).unwrap();
        assert!(!stamp_changed(&a, &old, Side::A, CONTENT_STAMP));
        assert!(!stamp_changed(&b, &old, Side::B, CONTENT_STAMP), "each side has its own mtime");
        let a2 = rec(&[("mfr_size", Value::Int(4)), ("mfr_mtime", Value::DateTime(11))]);
        assert!(stamp_changed(&a2, &old, Side::A, CONTENT_STAMP));
        assert!(stamp_changed(&a, &Snapshot::default(), Side::A, CONTENT_STAMP), "never recorded");
    }

    #[test]
    fn an_aspect_reads_the_files_only_when_a_side_changed() {
        let never = || -> Result<bool, SyncError> { panic!("compared for nothing") };
        assert_eq!(decide_aspect(false, false, never).unwrap(), Decision::Untouched);
        assert_eq!(
            decide_aspect(true, false, || Ok(false)).unwrap(),
            Decision::Propagate { from: Side::A }
        );
        assert_eq!(decide_aspect(true, true, || Ok(true)).unwrap(), Decision::InSync);
        assert_eq!(decide_aspect(true, true, || Ok(false)).unwrap(), Decision::Conflict);
    }
}
