//! Scale by counting (docs/spec-storage.org "Testing"): the everyday gestures
//! read the same number of keys on a repository sixteen times larger. Keys
//! read, not time, so the assertions cannot flake under load.

use metafolder_kv_proto::model::{Record, Value, ROOT};
use metafolder_kv_proto::query::{Sort, Q};
use metafolder_kv_proto::store::Store;
use uuid::Uuid;

mod common;
use common::TempDir;

const P: &str = "mfr_path";
const PER_DIR: usize = 50;

/// `dirs` directories of `PER_DIR` files under one root. Every file has a
/// date and a type; one in five carries the tag `red`.
fn build(dirs: usize) -> (TempDir, Store) {
    let dir = TempDir::new("cost");
    let store = Store::open(dir.path()).unwrap();
    let mut w = store.write().unwrap();
    let root = Uuid::from_u128(1);
    w.create(Record { uuid: root, fields: vec![(P.into(), tree(ROOT, ""))] }).unwrap();
    let mut n = 2u128;
    for d in 0..dirs {
        let du = Uuid::from_u128(n);
        n += 1;
        w.create(Record { uuid: du, fields: vec![(P.into(), tree(root, &format!("d{d}")))] })
            .unwrap();
        for f in 0..PER_DIR {
            let mut fields = vec![
                (P.to_string(), tree(du, &format!("file{f}.txt"))),
                ("mfr_type".into(), Value::Str("file".into())),
                ("mfr_mtime".into(), Value::Time((n * 7919 % 1_000_003) as i64)),
            ];
            if f % 5 == 0 {
                fields.push(("tag".into(), Value::Str("red".into())));
            }
            w.create(Record { uuid: Uuid::from_u128(n), fields }).unwrap();
            n += 1;
        }
    }
    w.commit().unwrap();
    (dir, store)
}

fn tree(parent: Uuid, name: &str) -> Value {
    Value::Tree { parent, name: name.into() }
}

fn folder(s: &Store) -> Uuid {
    s.resolve(P, "/d3").unwrap().unwrap()
}

fn child(s: &Store) -> Q {
    Q::Child { field: P.into(), node: folder(s) }
}

fn under(s: &Store) -> Q {
    Q::Under { field: P.into(), node: folder(s) }
}

/// Keys read by one query (plus the page's records, as a client reads them).
fn reads(store: &Store, q: &Q, sort: &Sort, limit: usize) -> u64 {
    store.take_reads();
    let page = store.query(q, sort, limit).unwrap();
    for u in page.uuids {
        store.get(u).unwrap();
    }
    store.take_reads()
}

#[test]
fn everyday_gestures_do_not_grow_with_the_repository() {
    let (_a, small) = build(40);
    let (_b, big) = build(640);
    let by_date = || Sort::Field { field: "mfr_mtime".into(), desc: true };
    let path = || Sort::Path { field: P.into() };

    // Exactly the same work: a folder is a folder, whatever surrounds it.
    for (label, q, sort) in [
        ("folder by name", child as fn(&Store) -> Q, path()),
        ("folder by date", child, by_date()),
        ("subtree by path", under, path()),
        ("subtree by date", under, by_date()),
    ] {
        let (rs, rb) = (reads(&small, &q(&small), &sort, 100), reads(&big, &q(&big), &sort, 100));
        assert_eq!(rs, rb, "{label}: {rs} keys on the small repository, {rb} on the big one");
    }

    // A page of a repository-wide filter: the page, plus one key per bitmap
    // chunk (65 536 ids) — which the big repository does not yet exceed.
    let tag = Q::Eq("tag".into(), Value::Str("red".into()));
    for (label, q, sort) in [
        ("tag page", tag.clone(), Sort::None),
        ("tag by date", tag, by_date()),
        ("files by date", Q::Eq("mfr_type".into(), Value::Str("file".into())), by_date()),
    ] {
        let (rs, rb) = (reads(&small, &q, &sort, 100), reads(&big, &q, &sort, 100));
        assert!(
            rb <= rs + rs / 5,
            "{label}: {rs} keys on the small repository, {rb} on the big one"
        );
    }
}
