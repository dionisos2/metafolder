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

/// The walk strategy of a path sort starts below the lowest ancestor the
/// matches share, not at the forest root: browsing a folder must not pay for
/// the siblings of its ancestors.
#[test]
fn a_path_walk_starts_at_the_matches_common_ancestor() {
    let (_a, small) = build(40);
    let (_b, big) = build(640);
    for s in [&small, &big] {
        s.set_thresholds(metafolder_kv_proto::store::Thresholds {
            sort: metafolder_kv_proto::store::SortChoice::Walk,
            ..Default::default()
        });
    }
    let path = Sort::Path { field: P.into() };
    for (label, q) in [("folder", child as fn(&Store) -> Q), ("subtree", under)] {
        let (rs, rb) = (reads(&small, &q(&small), &path, 100), reads(&big, &q(&big), &path, 100));
        assert_eq!(rs, rb, "{label}: {rs} keys on the small repository, {rb} on the big one");
    }
}

/// One folder of `n` files under the root.
fn flat(n: usize) -> (TempDir, Store) {
    let dir = TempDir::new("flat");
    let store = Store::open(dir.path()).unwrap();
    let mut w = store.write().unwrap();
    let (root, folder) = (Uuid::from_u128(1), Uuid::from_u128(2));
    w.create(Record { uuid: root, fields: vec![(P.into(), tree(ROOT, ""))] }).unwrap();
    w.create(Record { uuid: folder, fields: vec![(P.into(), tree(root, "big"))] }).unwrap();
    for i in 0..n {
        let fields = vec![
            (P.to_string(), tree(folder, &format!("file{i:06}.txt"))),
            ("mfr_mtime".into(), Value::Time((i * 7919 % 100_003) as i64)),
        ];
        w.create(Record { uuid: Uuid::from_u128(10 + i as u128), fields }).unwrap();
    }
    w.commit().unwrap();
    (dir, store)
}

/// The first page of a folder costs the page, not the folder: a folder of
/// 8 000 files opens with as few reads as one of 500 (give or take a key per
/// bitmap chunk).
#[test]
fn a_giant_folder_opens_at_the_cost_of_its_first_page() {
    let (_a, small) = flat(500);
    let (_b, big) = flat(8_000);
    let q = |s: &Store| Q::Child { field: P.into(), node: s.resolve(P, "/big").unwrap().unwrap() };
    let sort = Sort::Path { field: P.into() };
    let (rs, rb) = (reads(&small, &q(&small), &sort, 100), reads(&big, &q(&big), &sort, 100));
    assert!(rb <= rs + 10, "{rs} keys for 500 files, {rb} for 8 000");
}

/// A name search that does not ask for a count verifies only the names its
/// sort visits: its first page costs the same among 500 or 8 000 matches.
#[test]
fn a_name_search_page_costs_the_page() {
    let (_a, small) = flat(500);
    let (_b, big) = flat(8_000);
    let q = Q::Contains { field: P.into(), text: "file".into() };
    let sort = Sort::Path { field: P.into() };
    let page_reads = |s: &Store| {
        s.take_reads();
        s.page(&q, &sort, 100).unwrap();
        s.take_reads()
    };
    let (rs, rb) = (page_reads(&small), page_reads(&big));
    assert!(rb <= rs + 10, "{rs} keys among 500 matches, {rb} among 8 000");
}
