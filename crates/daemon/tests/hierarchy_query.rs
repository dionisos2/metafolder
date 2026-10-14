//! Querying a `hierarchy_ref` field (doc "Hierarchy references"): a node has
//! one path per way up to a root, and every tree question — a subtree, a
//! parent, a path, a `:path` comparison, an `osm` path — reads all of them.
//! Each query runs through the oracle and the serving path, which must agree,
//! and the answers are pinned.

use std::collections::BTreeSet;

use metafolder_core::dsl::parse_query;
use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::Query;
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::tree_cache::TreeCache;
use uuid::Uuid;

mod common;

struct Fixture {
    store: KvStore,
    _dir: common::TempDir,
    sport: Uuid,
    courir: Uuid,
    event: Uuid,
    maraton: Uuid,
    boston: Uuid,
    /// Records pointing at the tags with a `tag` ref.
    a_run: Uuid,
    a_boston: Uuid,
    a_party: Uuid,
}

fn create(store: &mut KvStore, fields: Vec<Field>) -> Uuid {
    let mut w = Writer::begin(store, None).unwrap();
    let uuid = w.create_metarecord(fields).unwrap().uuid;
    w.commit().unwrap();
    uuid
}

fn node(store: &mut KvStore, positions: &[(Option<Uuid>, &str)]) -> Uuid {
    let fields = positions
        .iter()
        .map(|(parent, name)| {
            Field::new("path", Value::HierarchyRef { parent: *parent, name: (*name).into() })
        })
        .chain([Field::new("mf_schema", Value::String("tag".into()))])
        .collect();
    create(store, fields)
}

/// `sport/courir/maraton/boston` and `évènement/maraton/boston`, `fête`
/// beside `maraton`, and three records tagged with them.
fn fixture() -> Fixture {
    let (mut store, dir) = common::kv::store();
    let sport = node(&mut store, &[(None, "sport")]);
    let courir = node(&mut store, &[(Some(sport), "courir")]);
    let event = node(&mut store, &[(None, "évènement")]);
    let maraton = node(&mut store, &[(Some(courir), "maraton"), (Some(event), "maraton")]);
    let boston = node(&mut store, &[(Some(maraton), "boston")]);
    let party = node(&mut store, &[(Some(event), "fête")]);
    let tagged = |store: &mut KvStore, tag: Uuid| {
        create(store, vec![Field::new("tag", Value::Ref(tag)), Field::new("kind", Value::Int(1))])
    };
    let a_run = tagged(&mut store, courir);
    let a_boston = tagged(&mut store, boston);
    let a_party = tagged(&mut store, party);
    Fixture { store, _dir: dir, sport, courir, event, maraton, boston, a_run, a_boston, a_party }
}

impl Fixture {
    fn run(&self, dsl: &str) -> BTreeSet<Uuid> {
        let query: Query = parse_query(dsl).unwrap();
        common::engines::both(&self.store, &query, &[]).into_iter().collect()
    }
}

fn set(items: &[Uuid]) -> BTreeSet<Uuid> {
    items.iter().copied().collect()
}

#[test]
fn a_subtree_is_everything_below_by_any_path() {
    let f = fixture();
    assert_eq!(f.run(r#"path =>* "évènement""#), set(&[f.event, f.maraton, f.boston, f.party()]));
    assert_eq!(f.run(r#"path ->* "sport""#), set(&[f.courir, f.maraton, f.boston]));
    assert_eq!(f.run(r#"tag -> (path =>* "évènement")"#), set(&[f.a_boston, f.a_party]));
    assert_eq!(f.run(r#"tag -> (path =>* "sport")"#), set(&[f.a_run, f.a_boston]));
}

#[test]
fn a_path_through_either_parent_names_the_node() {
    let f = fixture();
    assert_eq!(f.run(r#"path = "évènement/maraton""#), set(&[f.maraton]));
    assert_eq!(f.run(r#"path = "sport/courir/maraton/boston""#), set(&[f.boston]));
    assert_eq!(f.run(r#"path = "évènement/maraton/boston""#), set(&[f.boston]));
    assert_eq!(f.run(r#"path -> "évènement""#), set(&[f.maraton, f.party()]));
    assert_eq!(f.run(r#"path -> "sport/courir""#), set(&[f.maraton]));
}

#[test]
fn the_path_aspect_reads_every_path() {
    let f = fixture();
    assert_eq!(f.run(r#"path:path = "évènement/maraton/boston""#), set(&[f.boston]));
    assert_eq!(f.run(r#"path:path = "sport/courir/maraton""#), set(&[f.maraton]));
    assert_eq!(f.run(r#"path:path MATCHES "^évènement/""#), set(&[f.maraton, f.boston, f.party()]));
    assert_eq!(f.run(r#"path:parent IS ABSENT"#), set(&[f.sport, f.event]));
}

#[test]
fn an_osm_path_matches_any_path() {
    let f = fixture();
    assert!(f.run(r#"osm(path, "év bost")"#).contains(&f.boston));
    assert!(f.run(r#"osm(path, "cour bost")"#).contains(&f.boston));
    assert!(!f.run(r#"osm(path, "fête bost")"#).contains(&f.boston));
}

#[test]
fn sorting_by_a_hierarchy_runs_on_both_engines() {
    let f = fixture();
    let query = parse_query(r#"path =>* "évènement""#).unwrap();
    let sort = [SortKey { field: "path".into(), order: SortOrder::Asc }];
    let got = common::engines::both(&f.store, &query, &sort);
    assert_eq!(got.len(), 4);
}

#[test]
fn a_node_has_one_path_per_way_up() {
    let f = fixture();
    let cache = TreeCache::new(false);
    let mut paths = cache.paths_of(&f.store, "path", f.boston).unwrap();
    paths.sort();
    assert_eq!(paths, vec!["sport/courir/maraton/boston", "évènement/maraton/boston"]);
    assert_eq!(cache.resolve_path(&f.store, "path", "évènement/maraton").unwrap(), Some(f.maraton));
}

impl Fixture {
    fn party(&self) -> Uuid {
        let cache = TreeCache::new(false);
        cache.resolve_path(&self.store, "path", "évènement/fête").unwrap().unwrap()
    }
}
