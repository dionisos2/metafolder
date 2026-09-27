//! The rule index against the chain walk it replaced (spec-file-tracking "The
//! rule index"): on generated trees carrying generated `mf_watch` / `mf_ignore`
//! / `mfr_watch_exceeded` rules, every probe must get the same verdict, the same
//! reason and the same provenance from both.
//!
//! The oracle below is the old algorithm, kept verbatim in spirit: one tree
//! lookup per ancestor and the fields read from the store — slow, obviously
//! right, and independent of the index.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::eligibility::{Reason, WatchRules};
use metafolder_daemon::log::Writer;
use metafolder_daemon::relpath::RelPath;
use metafolder_daemon::store::Rows;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const NAMES: &[&str] = &["a", "b", "src", ".hidden", "node_modules", "target", "Cache"];
const PATTERNS: &[&str] =
    &[r"node_modules(/.*)?$", r"(^|/)\.[^/]+", r"^/b(/.*)?$", r"target(/.*)?$", r"src/a$"];

/// What the old chain walk answered, in the fields both sides expose.
#[derive(Debug, PartialEq)]
struct Verdict {
    eligible: bool,
    reason: Reason,
    watch_scope: Option<String>,
    ignore_source: Option<String>,
    pattern: Option<String>,
}

fn prefixes(path: &str) -> Vec<String> {
    let comps: Vec<&str> = path.split('/').collect();
    (0..comps.len()).map(|i| comps[..=i].join("/")).collect()
}

/// The chain walk (spec-file-tracking "Eligibility algorithm"), reading the
/// store at every step.
fn oracle(conn: &Connection, cache: &mut TreeCache, path: &str) -> Verdict {
    let comps: Vec<&str> = path.split('/').collect();
    let full_idx = comps.len() - 1;
    let mut chain = Vec::new();
    for (i, prefix) in prefixes(path).iter().enumerate() {
        match cache.resolve_path(conn, "mfr_path", prefix).unwrap() {
            Some(u) => chain.push((i, u)),
            None => break,
        }
    }
    let own = chain.last().and_then(|(i, u)| (*i == full_idx).then_some(*u));
    let watch = chain
        .iter()
        .rev()
        .find_map(|(i, u)| Rows::bool_field(conn, *u, "mf_watch").unwrap().map(|v| (*i, *u, v)));
    let Some((widx, wuuid, wval)) = watch else {
        return Verdict {
            eligible: false,
            reason: Reason::NoWatch,
            watch_scope: None,
            ignore_source: None,
            pattern: None,
        };
    };
    let scope = comps[..=widx].join("/");
    if !wval {
        return Verdict {
            eligible: false,
            reason: Reason::WatchFalse,
            watch_scope: Some(scope),
            ignore_source: None,
            pattern: None,
        };
    }
    if own == Some(wuuid) {
        return Verdict {
            eligible: true,
            reason: Reason::DirectWatch,
            watch_scope: Some(scope),
            ignore_source: None,
            pattern: None,
        };
    }
    let scoped = format!("/{}", comps[widx + 1..].join("/"));
    for (i, u) in chain.iter().rev() {
        if *i == full_idx {
            continue;
        }
        let patterns = Rows::string_fields(conn, *u, "mf_ignore").unwrap();
        if patterns.is_empty() {
            continue;
        }
        let source = comps[..=*i].join("/");
        for p in &patterns {
            if metafolder_daemon::regexp::compile(p).unwrap().is_match(&scoped) {
                return Verdict {
                    eligible: false,
                    reason: Reason::Ignored,
                    watch_scope: Some(scope),
                    ignore_source: Some(source),
                    pattern: Some(p.clone()),
                };
            }
        }
        return Verdict {
            eligible: true,
            reason: Reason::Tracked,
            watch_scope: Some(scope),
            ignore_source: Some(source),
            pattern: None,
        };
    }
    Verdict {
        eligible: true,
        reason: Reason::Tracked,
        watch_scope: Some(scope),
        ignore_source: None,
        pattern: None,
    }
}

/// The nearest `mfr_watch_exceeded` on the path's own prefixes, the path
/// included: `Some(prefix)` when it is `true` there.
fn oracle_exceeded(conn: &Connection, cache: &mut TreeCache, path: &str) -> Option<String> {
    for prefix in prefixes(path).iter().rev() {
        if let Some(u) = cache.resolve_path(conn, "mfr_path", prefix).unwrap() {
            if let Some(v) = Rows::bool_field(conn, u, "mfr_watch_exceeded").unwrap() {
                return v.then(|| prefix.clone());
            }
        }
    }
    None
}

/// The nearest set of patterns on the path's prefixes, the path included.
fn oracle_effective(conn: &Connection, cache: &mut TreeCache, path: &str) -> Option<String> {
    for prefix in prefixes(path).iter().rev() {
        if let Some(u) = cache.resolve_path(conn, "mfr_path", prefix).unwrap() {
            if !Rows::string_fields(conn, u, "mf_ignore").unwrap().is_empty() {
                return Some(prefix.clone());
            }
        }
    }
    None
}

/// A random tree of tracked directories, each carrying random rules. Returns the
/// connection and every tracked path.
fn generate(rng: &mut Rng) -> (Connection, Vec<String>) {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut nodes: Vec<(Uuid, String)> = Vec::new();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let rules = |rng: &mut Rng, fields: &mut Vec<Field>| {
        match rng.below(4) {
            0 => fields.push(Field::new("mf_watch", Value::Bool(true))),
            1 => fields.push(Field::new("mf_watch", Value::Bool(false))),
            _ => {}
        }
        if rng.below(4) == 0 {
            for p in PATTERNS {
                if rng.below(2) == 0 {
                    fields.push(Field::new("mf_ignore", Value::String((*p).into())));
                }
            }
        }
        if rng.below(8) == 0 {
            let v = rng.below(2) == 0;
            fields.push(Field::new("mfr_watch_exceeded", Value::Bool(v)));
        }
    };
    let mut root_fields =
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })];
    rules(rng, &mut root_fields);
    let root = w.create_metarecord(root_fields).unwrap().uuid;
    nodes.push((root, String::new()));
    for _ in 0..25 {
        let (parent, ppath) = nodes[rng.below(nodes.len() as u64) as usize].clone();
        if ppath.matches('/').count() >= 4 {
            continue;
        }
        let name = NAMES[rng.below(NAMES.len() as u64) as usize];
        let path = format!("{ppath}/{name}");
        if nodes.iter().any(|(_, p)| *p == path) {
            continue;
        }
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.into() },
        )];
        rules(rng, &mut fields);
        let uuid = w.create_metarecord(fields).unwrap().uuid;
        nodes.push((uuid, path));
    }
    w.commit().unwrap();
    (conn, nodes.into_iter().map(|(_, p)| p).collect())
}

#[test]
fn the_rule_index_answers_like_the_chain_walk() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut probes_checked = 0;
    for _ in 0..60 {
        let (conn, tracked) = generate(&mut rng);
        let mut cache = TreeCache::new(false);
        let rules = WatchRules::load(&conn, false).unwrap();
        // Every tracked path, an untracked child of each, and one deeper.
        let mut probes: Vec<String> = Vec::new();
        for p in &tracked {
            probes.push(p.clone());
            probes.push(format!("{p}/new.txt"));
            probes.push(format!("{p}/fresh/node_modules/x"));
        }
        for probe in &probes {
            let want = oracle(&conn, &mut cache, probe);
            let got = rules.explain(&RelPath::from_display(probe)).unwrap();
            let got = Verdict {
                eligible: got.eligible,
                reason: got.reason,
                watch_scope: got.watch_scope,
                ignore_source: got.ignore_source,
                pattern: got.pattern,
            };
            assert_eq!(got, want, "explain({probe:?})");
            assert_eq!(
                rules.exceeded_by(&RelPath::from_display(probe)),
                oracle_exceeded(&conn, &mut cache, probe),
                "exceeded_by({probe:?})"
            );
            assert_eq!(
                rules.effective_ignore(&RelPath::from_display(probe)).source,
                oracle_effective(&conn, &mut cache, probe),
                "effective_ignore({probe:?})"
            );
            probes_checked += 1;
        }
    }
    assert!(probes_checked > 1000, "only {probes_checked} probes");
}

#[test]
fn a_case_insensitive_repository_finds_its_rules_whatever_the_case() {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
            Field::new("mf_watch", Value::Bool(true)),
        ])
        .unwrap()
        .uuid;
    w.create_metarecord(vec![
        Field::new("mfr_path", Value::TreeRef { parent: Some(root), name: "Cache".into() }),
        Field::new("mf_watch", Value::Bool(false)),
    ])
    .unwrap();
    w.commit().unwrap();

    let insensitive = WatchRules::load(&conn, true).unwrap();
    assert!(!insensitive.is_eligible(&RelPath::from_display("/cache/x")).unwrap());
    assert!(!insensitive.is_eligible(&RelPath::from_display("/CACHE/x")).unwrap());
    let sensitive = WatchRules::load(&conn, false).unwrap();
    assert!(sensitive.is_eligible(&RelPath::from_display("/cache/x")).unwrap());
    assert!(!sensitive.is_eligible(&RelPath::from_display("/Cache/x")).unwrap());
}

#[test]
fn a_name_that_is_not_text_is_matched_by_its_bytes() {
    use metafolder_core::metarecord::TreeName;
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
            Field::new("mf_watch", Value::Bool(true)),
        ])
        .unwrap()
        .uuid;
    let raw = TreeName::from_bytes(vec![b'd', 0xE9]);
    w.create_metarecord(vec![
        Field::new("mfr_path", Value::TreeRef { parent: Some(root), name: raw.clone() }),
        Field::new("mf_watch", Value::Bool(false)),
    ])
    .unwrap();
    w.commit().unwrap();

    let rules = WatchRules::load(&conn, false).unwrap();
    let inside = RelPath::root().child(raw).child(TreeName::from("f"));
    assert!(!rules.is_eligible(&inside).unwrap());
    // A look-alike whose name is the replacement character is another file.
    assert!(rules.is_eligible(&RelPath::from_display("/d\u{FFFD}/f")).unwrap());
}

#[test]
fn touches_names_the_rule_carriers_and_their_ancestors_only() {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
            Field::new("mf_watch", Value::Bool(true)),
        ])
        .unwrap()
        .uuid;
    let cfg = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root), name: "cfg".into() },
        )])
        .unwrap()
        .uuid;
    let db_dir = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: Some(cfg), name: "db".into() }),
            Field::new("mf_watch", Value::Bool(false)),
        ])
        .unwrap()
        .uuid;
    let other = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root), name: "other".into() },
        )])
        .unwrap()
        .uuid;
    w.commit().unwrap();

    let rules = WatchRules::load(&conn, false).unwrap();
    let p = RelPath::from_display;
    assert!(rules.touches(&p("/cfg")));
    assert!(rules.touches(&p("/cfg/db")));
    assert!(!rules.touches(&p("/cfg/db/file")), "below a carrier is governed, not a carrier");
    assert!(!rules.touches(&p("/other")));
    assert!(rules.affects(cfg) && rules.affects(db_dir) && rules.affects(root));
    assert!(!rules.affects(other));
}
