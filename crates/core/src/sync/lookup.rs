//! The daemon reads the planner and the run share: a pair's links, records,
//! tree paths, and the [`Translator`] — the daemon-backed [`Translate`] that
//! finds a record's counterpart *link-first, path-fallback* (doc "Ref
//! translation during sync").

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::metarecord::{MetaRecord, TreeName, Value};

use super::model::{Lookup, Side, Translate};
use super::{SyncCtx as Ctx, SyncError};

/// The two repositories of a pair, in canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub a: Uuid,
    pub b: Uuid,
}

impl Pair {
    pub fn repo(&self, side: Side) -> Uuid {
        match side {
            Side::A => self.a,
            Side::B => self.b,
        }
    }

    /// `/sync/<a>/<b>`.
    pub fn prefix(&self) -> String {
        super::pair_prefix(self.a, self.b)
    }
}

/// One row of a pair's link table.
#[derive(Debug, Clone)]
pub struct LinkRow {
    pub uuid: Uuid,
    pub record_a: Uuid,
    pub record_b: Uuid,
    pub version_a: Option<u64>,
    pub version_b: Option<u64>,
}

fn uuid_at(v: &Json) -> Option<Uuid> {
    v.as_str().and_then(|s| Uuid::parse_str(s).ok())
}

/// Every link of the pair.
pub fn read_links(ctx: &Ctx, pair: Pair) -> Result<Vec<LinkRow>, SyncError> {
    let body = ctx.client.get(&format!("{}/links", pair.prefix()))?;
    Ok(body["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(LinkRow {
                uuid: uuid_at(&l["uuid"])?,
                record_a: uuid_at(&l["record_a"])?,
                record_b: uuid_at(&l["record_b"])?,
                version_a: l["version_a"].as_u64(),
                version_b: l["version_b"].as_u64(),
            })
        })
        .collect())
}

/// A pair's links as a two-way map between the records they join.
#[derive(Debug, Clone, Default)]
pub struct LinkTable {
    a_to_b: HashMap<Uuid, Uuid>,
    b_to_a: HashMap<Uuid, Uuid>,
}

impl LinkTable {
    pub fn from_rows(rows: &[LinkRow]) -> Self {
        let mut t = Self::default();
        for l in rows {
            t.insert(l.record_a, l.record_b);
        }
        t
    }

    pub fn insert(&mut self, a: Uuid, b: Uuid) {
        self.a_to_b.insert(a, b);
        self.b_to_a.insert(b, a);
    }

    /// The record `record` (of side `from`) is linked to.
    pub fn other(&self, from: Side, record: Uuid) -> Option<Uuid> {
        match from {
            Side::A => self.a_to_b.get(&record).copied(),
            Side::B => self.b_to_a.get(&record).copied(),
        }
    }
}

/// A metarecord as the daemon answers it.
pub fn parse_record(json: &Json) -> Result<MetaRecord, SyncError> {
    serde_json::from_value(json.clone())
        .map_err(|e| SyncError::Op(format!("the daemon answered an unreadable metarecord: {e}")))
}

/// A record, or `None` when the daemon says it does not exist.
pub fn get_record(ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<Option<MetaRecord>, SyncError> {
    match ctx.client.get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), uuid.as_simple())) {
        Ok(m) => Ok(Some(parse_record(&m)?)),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The paths of `record` in `field`'s forest (several only in an older
/// repository, where a record could hold two positions).
pub fn tree_paths(
    ctx: &Ctx,
    repo: Uuid,
    record: Uuid,
    field: &str,
) -> Result<Vec<String>, SyncError> {
    let resp = ctx.client.get(&format!(
        "/repos/{}/metarecords/{}/fields/{}/resolve-tree",
        repo.as_simple(),
        record.as_simple(),
        field
    ))?;
    Ok(resp["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(String::from))
        .collect())
}

/// A record's `mfr_path`, as a path (`None` when it has none).
pub fn mfr_path_of(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Option<String>, SyncError> {
    Ok(tree_paths(ctx, repo, record, "mfr_path")?.into_iter().next())
}

/// The record at `path` in `field`'s forest, whatever the forest's root
/// convention (doc "TreeRef path conventions").
pub fn resolve_path(
    ctx: &Ctx,
    repo: Uuid,
    field: &str,
    path: &str,
) -> Result<Option<Uuid>, SyncError> {
    let resp = ctx.client.post(
        &format!("/repos/{}/tree/resolve-path", repo.as_simple()),
        &json!({"field": field, "path": path}),
    )?;
    Ok(uuid_at(&resp["uuid"]))
}

/// A path's parent path and last name. `None` for the filesystem root (`""`),
/// which is never created; a parent of `None` for a named root (`music`).
fn split_path(path: &str) -> Option<(Option<&str>, &str)> {
    if path.is_empty() {
        return None;
    }
    Some(match path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, path),
    })
}

/// The daemon-backed [`Translate`]: link-first, then the record at the same
/// path on the other side. With `create`, a path with no record there yet is
/// made — its missing ancestors first — which is what the run needs to write a
/// reference whose target is out of scope; the planner only looks.
pub struct Translator<'a> {
    ctx: &'a Ctx<'a>,
    pair: Pair,
    links: LinkTable,
    create: bool,
    /// What `create` made, by repository: directories among them have no file
    /// yet, and the run refreshes them once its disk operations have made one.
    created: RefCell<Vec<(Uuid, Uuid)>>,
    paths: RefCell<HashMap<(Uuid, Uuid, String), Option<String>>>,
}

impl<'a> Translator<'a> {
    pub fn new(ctx: &'a Ctx<'a>, pair: Pair, links: LinkTable, create: bool) -> Self {
        Self {
            ctx,
            pair,
            links,
            create,
            created: RefCell::new(Vec::new()),
            paths: RefCell::new(HashMap::new()),
        }
    }

    pub fn links(&self) -> &LinkTable {
        &self.links
    }

    pub fn link(&mut self, a: Uuid, b: Uuid) {
        self.links.insert(a, b);
    }

    /// The records made by path fallback since the last call.
    pub fn take_created(&self) -> Vec<(Uuid, Uuid)> {
        std::mem::take(&mut self.created.borrow_mut())
    }

    fn path_in(&self, repo: Uuid, record: Uuid, field: &str) -> Result<Option<String>, SyncError> {
        let key = (repo, record, field.to_string());
        if let Some(p) = self.paths.borrow().get(&key) {
            return Ok(p.clone());
        }
        let p = tree_paths(self.ctx, repo, record, field)?.into_iter().next();
        self.paths.borrow_mut().insert(key, p.clone());
        Ok(p)
    }

    /// A record's identity for a `ref` fallback: its first TreeRef position.
    fn identity(&self, repo: Uuid, record: Uuid) -> Result<Option<(String, String)>, SyncError> {
        let Some(rec) = get_record(self.ctx, repo, record)? else { return Ok(None) };
        let mut fields: Vec<&str> = rec
            .fields
            .iter()
            .filter(|f| matches!(f.value, Value::TreeRef { .. }))
            .map(|f| f.name.as_str())
            .collect();
        fields.dedup();
        for field in fields {
            if let Some(path) = self.path_in(repo, record, field)? {
                return Ok(Some((field.to_string(), path)));
            }
        }
        Ok(None)
    }

    /// The record at `path` in `field`'s forest of `repo`, made (ancestors
    /// first) when absent.
    fn find_or_create(&self, repo: Uuid, field: &str, path: &str) -> Result<Uuid, SyncError> {
        if let Some(u) = resolve_path(self.ctx, repo, field, path)? {
            return Ok(u);
        }
        let Some((parent_path, name)) = split_path(path) else {
            return Err(SyncError::Op(format!(
                "no root of the {field} forest in {}",
                repo.as_simple()
            )));
        };
        let parent = match parent_path {
            Some(p) => Some(self.find_or_create(repo, field, p)?),
            None => None,
        };
        let value = Value::TreeRef { parent, name: TreeName::from(name) };
        let resp = self.ctx.client.post(
            &format!("/repos/{}/metarecords", repo.as_simple()),
            &json!({"fields": [{"name": field, "value": value}], "force": true}),
        )?;
        let uuid = uuid_at(&resp["uuid"])
            .ok_or_else(|| SyncError::Op("the daemon created a record with no uuid".into()))?;
        self.created.borrow_mut().push((repo, uuid));
        Ok(uuid)
    }
}

impl Translate for Translator<'_> {
    fn counterpart(
        &self,
        from: Side,
        record: Uuid,
        how: Lookup<'_>,
    ) -> Result<Option<Uuid>, SyncError> {
        if let Some(t) = self.links.other(from, record) {
            return Ok(Some(t));
        }
        let (src, dst) = (self.pair.repo(from), self.pair.repo(from.other()));
        let found = match how {
            Lookup::Ref => self.identity(src, record)?,
            Lookup::Parent { field } => {
                self.path_in(src, record, field)?.map(|p| (field.to_string(), p))
            }
        };
        let Some((field, path)) = found else { return Ok(None) };
        if self.create {
            Ok(Some(self.find_or_create(dst, &field, &path)?))
        } else {
            resolve_path(self.ctx, dst, &field, &path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_path;

    #[test]
    fn paths_split_by_the_root_convention_of_their_forest() {
        assert_eq!(split_path(""), None, "the filesystem root is never made");
        assert_eq!(split_path("/projects"), Some((Some(""), "projects")));
        assert_eq!(split_path("/projects/rust"), Some((Some("/projects"), "rust")));
        assert_eq!(split_path("music"), Some((None, "music")), "a named root");
        assert_eq!(split_path("music/jazz"), Some((Some("music"), "jazz")));
    }
}
