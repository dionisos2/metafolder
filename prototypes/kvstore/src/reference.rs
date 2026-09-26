//! The oracle: the same data and the same questions, answered the naive way —
//! a vector of records and a scan per question. The store must agree with it
//! on every query, after every kind of write.

use std::collections::HashMap;

use anyhow::{bail, Result};
use regex::Regex;
use uuid::Uuid;

use crate::model::{ordered_key, text_of, Record, Value, ROOT};
use crate::query::{Page, Sort, Q};

#[derive(Default)]
pub struct Reference {
    /// In creation order; a deleted record leaves `None` (ids are not reused).
    records: Vec<Option<Record>>,
    index: HashMap<Uuid, usize>,
}

impl Reference {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, uuid: Uuid) -> Option<&Record> {
        self.index.get(&uuid).and_then(|&i| self.records[i].as_ref())
    }

    pub fn create(&mut self, record: Record) -> Result<()> {
        if self.index.contains_key(&record.uuid) {
            bail!("duplicate uuid");
        }
        self.check_tree_rows(record.uuid, &record.fields)?;
        self.index.insert(record.uuid, self.records.len());
        self.records.push(Some(record));
        Ok(())
    }

    pub fn set_field(&mut self, uuid: Uuid, field: &str, values: Vec<Value>) -> Result<()> {
        let Some(old) = self.get(uuid) else { bail!("no such record") };
        let was_placed = old.tree(field).is_some();
        let mut fields: Vec<_> = old.fields.iter().filter(|(n, _)| n != field).cloned().collect();
        fields.extend(values.into_iter().map(|v| (field.to_string(), v)));
        self.check_tree_rows(uuid, &fields)?;
        let leaves = was_placed
            && !fields.iter().any(|(n, v)| n == field && matches!(v, Value::Tree { .. }));
        if leaves && self.live().any(|o| o.tree(field).is_some_and(|(p, _)| p == uuid)) {
            bail!("a node with children cannot leave the forest");
        }
        let i = self.index[&uuid];
        self.records[i].as_mut().unwrap().fields = fields;
        Ok(())
    }

    pub fn delete(&mut self, uuid: Uuid) -> Result<()> {
        let Some(r) = self.get(uuid) else { bail!("no such record") };
        for (field, _) in r.fields.iter().filter(|(_, v)| matches!(v, Value::Tree { .. })) {
            if self.live().any(|o| o.tree(field).is_some_and(|(p, _)| p == uuid)) {
                bail!("a node with children cannot be deleted");
            }
        }
        let i = self.index.remove(&uuid).unwrap();
        self.records[i] = None;
        Ok(())
    }

    /// The forest rules the store enforces: one position per field, a parent
    /// that is itself in the forest (or the root sentinel), a free name, no
    /// cycle.
    fn check_tree_rows(&self, uuid: Uuid, fields: &[(String, Value)]) -> Result<()> {
        let mut seen = Vec::new();
        for (field, v) in fields {
            let Value::Tree { parent, name } = v else { continue };
            if seen.contains(&field) {
                bail!("two positions in one forest");
            }
            seen.push(field);
            if *parent != ROOT && self.get(*parent).and_then(|r| r.tree(field)).is_none() {
                bail!("the parent is not in the forest");
            }
            let taken = self.live().any(|o| {
                o.uuid != uuid && o.tree(field).is_some_and(|(p, n)| p == *parent && n == name)
            });
            if taken {
                bail!("name taken");
            }
            let mut at = *parent;
            while at != ROOT {
                if at == uuid {
                    bail!("cycle");
                }
                at = self.get(at).and_then(|r| r.tree(field)).map_or(ROOT, |(p, _)| p);
            }
        }
        Ok(())
    }

    fn live(&self) -> impl Iterator<Item = &Record> {
        self.records.iter().flatten()
    }

    /// Path components from the root, or `None` when not in the forest.
    fn path(&self, field: &str, uuid: Uuid) -> Option<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut at = uuid;
        while at != ROOT {
            let (p, n) = self.get(at)?.tree(field)?;
            out.push(n.as_bytes().to_vec());
            at = p;
        }
        out.reverse();
        Some(out)
    }

    fn is_under(&self, field: &str, uuid: Uuid, node: Uuid) -> bool {
        let Some((mut at, _)) = self.get(uuid).and_then(|r| r.tree(field)) else { return false };
        loop {
            if at == node {
                return true;
            }
            if at == ROOT {
                return false;
            }
            at = self.get(at).and_then(|r| r.tree(field)).map_or(ROOT, |(p, _)| p);
        }
    }

    fn matches(&self, r: &Record, q: &Q) -> Result<bool> {
        Ok(match q {
            Q::All => true,
            Q::Present(f) => r.values(f).any(|v| *v != Value::Nothing),
            Q::Absent(f) => r.values(f).any(|v| *v == Value::Nothing),
            Q::Eq(f, x) => r.values(f).any(|v| v == x),
            Q::Range { field, lo, hi } => {
                let lo = lo.as_ref().and_then(ordered_key);
                let hi = hi.as_ref().and_then(ordered_key);
                let tag = lo.as_ref().or(hi.as_ref()).map(|k| k[0]);
                r.values(field).filter_map(ordered_key).any(|k| {
                    Some(k[0]) == tag
                        && lo.as_ref().is_none_or(|l| &k >= l)
                        && hi.as_ref().is_none_or(|h| &k <= h)
                })
            }
            Q::Child { field, node } => r.tree(field).is_some_and(|(p, _)| p == *node),
            Q::Under { field, node } => self.is_under(field, r.uuid, *node),
            Q::Contains { field, text } => {
                let t = text.to_lowercase();
                r.values(field).filter_map(text_of).any(|s| s.to_lowercase().contains(&t))
            }
            Q::Regex { field, pattern } => {
                let re = Regex::new(pattern)?;
                r.values(field).filter_map(text_of).any(|s| re.is_match(s))
            }
            Q::And(qs) => {
                for q in qs {
                    if !self.matches(r, q)? {
                        return Ok(false);
                    }
                }
                true
            }
            Q::Or(qs) => {
                for q in qs {
                    if self.matches(r, q)? {
                        return Ok(true);
                    }
                }
                false
            }
            Q::Not(q) => !self.matches(r, q)?,
        })
    }

    pub fn query(&self, q: &Q, sort: &Sort, limit: usize) -> Result<Page> {
        // (creation position, record) of every match.
        let mut hits = Vec::new();
        for (i, r) in self.records.iter().enumerate() {
            if let Some(r) = r {
                if self.matches(r, q)? {
                    hits.push((i, r));
                }
            }
        }
        let count = hits.len() as u64;
        match sort {
            Sort::None => {}
            Sort::Field { field, desc } => {
                let key = |r: &Record| {
                    let keys = r.values(field).filter(|v| **v != Value::Nothing);
                    let keys = keys.filter_map(ordered_key);
                    if *desc {
                        keys.max()
                    } else {
                        keys.min()
                    }
                };
                hits.sort_by(|(ia, a), (ib, b)| match (key(a), key(b)) {
                    (Some(ka), Some(kb)) if *desc => kb.cmp(&ka).then(ib.cmp(ia)),
                    (Some(ka), Some(kb)) => ka.cmp(&kb).then(ia.cmp(ib)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => ia.cmp(ib),
                });
            }
            Sort::Path { field } => {
                hits.sort_by(|(ia, a), (ib, b)| {
                    match (self.path(field, a.uuid), self.path(field, b.uuid)) {
                        (Some(pa), Some(pb)) => pa.cmp(&pb),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => ia.cmp(ib),
                    }
                });
            }
        }
        let uuids = hits.into_iter().take(limit).map(|(_, r)| r.uuid).collect();
        Ok(Page { uuids, count })
    }

    /// The record's path in the field's forest, components joined by `/`
    /// (the daemon's convention: a root named `""` gives `/a/b`).
    pub fn path_string(&self, field: &str, uuid: Uuid) -> Option<String> {
        let parts = self.path(field, uuid)?;
        Some(parts.into_iter().map(|p| String::from_utf8(p).unwrap()).collect::<Vec<_>>().join("/"))
    }

    /// Every record strictly below `node` in the field's forest.
    pub fn descendants(&self, field: &str, node: Uuid) -> Vec<Uuid> {
        self.live().filter(|r| self.is_under(field, r.uuid, node)).map(|r| r.uuid).collect()
    }

    pub fn uuids(&self) -> Vec<Uuid> {
        self.live().map(|r| r.uuid).collect()
    }
}
