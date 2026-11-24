//! Format 2 of a pair's sync store, read only to convert it to the current
//! format when it is opened (doc "Persisted format changes").
//!
//! | table  | key               | value                                     |
//! |--------|-------------------|-------------------------------------------|
//! | meta   | name              | text: `format_version`, `repo_a`, `repo_b`, `host` |
//! | links  | link uuid         | the same table now: kept as it is         |
//! | by_a   | record_a          | link uuid                                 |
//! | by_b   | record_b          | link uuid                                 |
//! | snaps  | link uuid · index | one snapshot field, JSON in column form   |

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{Database, Env, WithoutTls};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use metafolder_core::hex;

use super::{enc_snapshot, Side, SnapshotField, SyncDb, FORMAT_KEY, FORMAT_VERSION};
use crate::rows;

/// The tables of format 2 that format 3 does not have, in this order. `links`
/// is the same table in both, with the same encoding: it stays as it is.
const NAMES: [&str; 4] = ["meta", "by_a", "by_b", "snaps"];

/// The tables of a format 2 store that go.
pub(super) struct Tables([Database<'static, Bytes, Bytes>; 4]);

/// Opens the tables of format 2, if the store has them: its `meta` table
/// says it is of that format.
pub(super) fn open(env: &Env<WithoutTls>) -> Result<Option<Tables>> {
    let r = env.read_txn()?;
    let found = env
        .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
            let mut found = Vec::new();
            for name in NAMES {
                found.push(dbo.open_database::<Bytes, Bytes>(r, Some(name))?);
            }
            let found: [_; 4] = found.try_into().expect("one per name");
            Ok(heed::CommitOrAbort::<_, ()>::Commit(found))
        })?
        .commit()
        .expect("the databases are committed");
    let [Some(meta), by_a, by_b, snaps] = found else { return Ok(None) };
    let missing = || "a sync store of format 2 without all its tables";
    Ok(Some(Tables([
        meta,
        by_a.with_context(missing)?,
        by_b.with_context(missing)?,
        snaps.with_context(missing)?,
    ])))
}

/// Converts a format 2 store into the current tables of `db`, in one
/// transaction, and removes the tables of format 2.
pub(super) fn convert(db: &SyncDb, old: Tables) -> Result<()> {
    let [meta, by_a, by_b, snaps] = old.0;
    let mut w = db.env.write_txn()?;

    let text = |w: &heed::RwTxn, key: &str| -> Result<Option<String>> {
        Ok(meta.get(w, key.as_bytes())?.map(|v| String::from_utf8_lossy(v).into_owned()))
    };
    match text(&w, "format_version")?.as_deref() {
        Some("2") => {}
        other => bail!("a sync store with a meta table of format {other:?}, not 2"),
    }
    let uuid = |w: &heed::RwTxn, key: &str| -> Result<Option<Uuid>> {
        text(w, key)?.map(|t| Uuid::parse_str(&t).context("a uuid of the meta table")).transpose()
    };
    let pair = [uuid(&w, "repo_a")?, uuid(&w, "repo_b")?, uuid(&w, "host")?];
    for (key, value) in super::REPOSITORY_KEYS.iter().zip(pair) {
        if let Some(u) = value {
            db.repositories.put(&mut w, key, u.as_bytes())?;
        }
    }

    // The link indexes keep their encoding; `links` is unchanged.
    for (from, to) in [(by_a, db.links_by_record_a), (by_b, db.links_by_record_b)] {
        let rows = from
            .iter(&w)?
            .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect::<heed::Result<Vec<_>>>()?;
        for (k, v) in rows {
            to.put(&mut w, &k, &v)?;
        }
    }

    // A link's fields, one key each in index order, become one value.
    let mut snapshots: Vec<(Uuid, Vec<SnapshotField>)> = Vec::new();
    for e in snaps.iter(&w)? {
        let (k, v) = e?;
        let link = Uuid::from_slice(k.get(..16).context("a snapshot key")?)?;
        let field = dec_field(v)?;
        match snapshots.last_mut() {
            Some((l, fields)) if *l == link => fields.push(field),
            _ => snapshots.push((link, vec![field])),
        }
    }
    for (link, fields) in snapshots {
        db.snapshots.put(&mut w, link.as_bytes(), &enc_snapshot(&fields))?;
    }

    db.version.put(&mut w, FORMAT_KEY, &FORMAT_VERSION.to_be_bytes())?;
    for table in [meta, by_a, by_b, snaps] {
        // SAFETY: these handles were opened for this conversion only, no other
        // copy of them exists, and this transaction has only read them.
        unsafe { table.remove(&mut w)? };
    }
    w.commit()?;
    Ok(())
}

/// A snapshot field as format 2 stored it: the value in its column form
/// (doc "The column form of a value"), byte strings in hex.
#[derive(Serialize, Deserialize)]
struct StoredField {
    name: String,
    value_type: String,
    text: Option<String>,
    int: Option<i64>,
    real: Option<f64>,
    uuid: Option<String>,
    ref_repo: Option<String>,
    value_name: Option<String>,
    name_bytes: Option<String>,
    uuid_b: Option<String>,
    /// `"a"` / `"b"` for a per-side entry; absent for a common one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    side: Option<String>,
}

fn dec_field(b: &[u8]) -> Result<SnapshotField> {
    let s: StoredField = serde_json::from_slice(b).context("a snapshot field")?;
    let bytes = |h: Option<String>| -> Result<Option<Vec<u8>>> {
        h.map(|h| hex::decode(&h).context("a hex column")).transpose()
    };
    let value = rows::decode_value(rows::RawValue {
        value_type: s.value_type,
        text: s.text,
        int: s.int,
        real: s.real,
        uuid: bytes(s.uuid)?,
        ref_repo: bytes(s.ref_repo)?,
        name: s.value_name,
        name_bytes: bytes(s.name_bytes)?,
    })?;
    let value_uuid_b = bytes(s.uuid_b)?.map(|b| Uuid::from_slice(&b)).transpose()?;
    let side = s.side.as_deref().map(Side::parse).transpose()?;
    Ok(SnapshotField { name: s.name, value, value_uuid_b, side })
}

/// Writes a store of format 2, as the daemon did before format 3, to test
/// its conversion.
#[cfg(test)]
pub(super) mod fixture {
    use std::path::Path;

    use heed::EnvOpenOptions;

    use super::*;

    fn enc_field(f: &SnapshotField) -> Vec<u8> {
        let e = rows::encode_value(&f.value);
        let h = |b: Option<Vec<u8>>| b.map(|b| hex::encode(&b));
        serde_json::to_vec(&StoredField {
            name: f.name.clone(),
            value_type: e.value_type.to_string(),
            text: e.text,
            int: e.int,
            real: e.real,
            uuid: h(e.uuid),
            ref_repo: h(e.ref_repo),
            value_name: e.name,
            name_bytes: h(e.name_bytes),
            uuid_b: f.value_uuid_b.map(|u| hex::encode(u.as_bytes())),
            side: f.side.map(|s| s.name().to_string()),
        })
        .unwrap()
    }

    /// One link of the fixture: its records, versions and snapshot.
    pub(in crate::sync) struct OldLink {
        pub uuid: Uuid,
        pub record_a: Uuid,
        pub record_b: Uuid,
        pub versions: Option<(u64, u64)>,
        pub snapshot: Vec<SnapshotField>,
    }

    /// Writes the store at `path` and closes it.
    pub(in crate::sync) fn write(path: &Path, pair: [Uuid; 3], links: &[OldLink]) {
        std::fs::create_dir_all(path).unwrap();
        // SAFETY: the store is new and opened once, closed before the test
        // opens it again.
        let env = unsafe {
            let mut options = EnvOpenOptions::new().read_txn_without_tls();
            options.map_size(1 << 24).max_dbs(8);
            options.open(path)
        }
        .unwrap();
        let w = env.write_txn().unwrap();
        let created = env
            .create_databases_and_commit(w, |dbo, w| -> heed::Result<_> {
                let mut tables = Vec::new();
                for name in ["meta", "links", "by_a", "by_b", "snaps"] {
                    tables.push(dbo.create_database::<Bytes, Bytes>(w, Some(name))?);
                }
                let tables: [_; 5] = tables.try_into().expect("one per name");
                Ok(heed::CommitOrAbort::<_, ()>::Commit(tables))
            })
            .unwrap();
        let [meta, links_t, by_a, by_b, snaps] = created.commit().expect("committed");
        let mut w = env.write_txn().unwrap();
        for (k, v) in [
            ("format_version", "2".to_string()),
            ("repo_a", pair[0].as_simple().to_string()),
            ("repo_b", pair[1].as_simple().to_string()),
            ("host", pair[2].as_simple().to_string()),
        ] {
            meta.put(&mut w, k.as_bytes(), v.as_bytes()).unwrap();
        }
        for l in links {
            let mut value = Vec::new();
            value.extend_from_slice(l.record_a.as_bytes());
            value.extend_from_slice(l.record_b.as_bytes());
            for v in [l.versions.map(|v| v.0), l.versions.map(|v| v.1)] {
                match v {
                    None => value.push(0),
                    Some(v) => {
                        value.push(1);
                        value.extend_from_slice(&v.to_be_bytes());
                    }
                }
            }
            links_t.put(&mut w, l.uuid.as_bytes(), &value).unwrap();
            by_a.put(&mut w, l.record_a.as_bytes(), l.uuid.as_bytes()).unwrap();
            by_b.put(&mut w, l.record_b.as_bytes(), l.uuid.as_bytes()).unwrap();
            for (i, f) in l.snapshot.iter().enumerate() {
                let mut key = l.uuid.as_bytes().to_vec();
                key.extend_from_slice(&(i as u32).to_be_bytes());
                snaps.put(&mut w, &key, &enc_field(f)).unwrap();
            }
        }
        w.commit().unwrap();
        env.prepare_for_closing().wait();
    }
}
