//! The store against the naive reference, on the same writes: every query,
//! every sort, and the descendant bitmaps, after creates, moves, renames,
//! field rewrites, deletes and rejected writes.

use metafolder_kv_proto::model::{Record, Value, ROOT};
use metafolder_kv_proto::query::{Sort, Q};
use metafolder_kv_proto::reference::Reference;
use metafolder_kv_proto::store::{Store, Thresholds};
use uuid::Uuid;

mod common;
use common::TempDir;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const P: &str = "mfr_path";

fn tree(parent: Uuid, name: &str) -> Value {
    Value::Tree { parent, name: name.to_string() }
}

/// Applies one write to both sides; they must agree on whether it is allowed.
struct Both {
    store: Store,
    reference: Reference,
}

enum Op {
    Create(Record),
    Set(Uuid, &'static str, Vec<Value>),
    Delete(Uuid),
}

impl Both {
    fn apply(&mut self, ops: Vec<Op>) {
        let mut w = self.store.write().unwrap();
        for op in ops {
            let (a, b) = match op {
                Op::Create(r) => (w.create(r.clone()), self.reference.create(r)),
                Op::Set(u, f, v) => {
                    (w.set_field(u, f, v.clone()), self.reference.set_field(u, f, v))
                }
                Op::Delete(u) => (w.delete(u), self.reference.delete(u)),
            };
            assert_eq!(a.is_ok(), b.is_ok(), "store {a:?} vs reference {b:?}");
        }
        w.commit().unwrap();
    }

    fn check(&self, dirs: &[Uuid]) {
        let tags = ["red", "green", "blue"];
        let mut queries = vec![
            Q::All,
            Q::Present("tag".into()),
            Q::Absent("rating".into()),
            Q::Eq("kind".into(), Value::Str("photo".into())),
            Q::Eq("tag".into(), Value::Str(tags[1].into())),
            Q::Eq("size".into(), Value::Int(42)),
            Q::Range { field: "size".into(), lo: Some(Value::Int(100)), hi: Some(Value::Int(600)) },
            Q::Range { field: "mtime".into(), lo: Some(Value::Time(5_000)), hi: None },
            Q::Range { field: "title".into(), lo: None, hi: Some(Value::Str("m".into())) },
            Q::Contains { field: P.into(), text: "FILE1".into() },
            Q::Contains { field: P.into(), text: "e1".into() },
            Q::Contains { field: "title".into(), text: "lorem 3".into() },
            Q::Regex { field: P.into(), pattern: "^f.*7$".into() },
            Q::Regex { field: "title".into(), pattern: "[0-9]{3}".into() },
            Q::Not(Box::new(Q::Present("tag".into()))),
            Q::Or(vec![
                Q::Eq("tag".into(), Value::Str("red".into())),
                Q::Range { field: "size".into(), lo: Some(Value::Int(900)), hi: None },
            ]),
            Q::Child { field: P.into(), node: ROOT },
            Q::Under { field: P.into(), node: ROOT },
        ];
        for &d in dirs.iter().take(6) {
            queries.push(Q::Child { field: P.into(), node: d });
            queries.push(Q::Under { field: P.into(), node: d });
            queries.push(Q::And(vec![
                Q::Under { field: P.into(), node: d },
                Q::Contains { field: P.into(), text: "file".into() },
                Q::Not(Box::new(Q::Eq("kind".into(), Value::Str("note".into())))),
            ]));
        }
        let sorts = [
            Sort::None,
            Sort::Field { field: "size".into(), desc: false },
            Sort::Field { field: "mtime".into(), desc: true },
            Sort::Field { field: "tag".into(), desc: false },
            Sort::Field { field: "tag".into(), desc: true },
            Sort::Field { field: "title".into(), desc: false },
            Sort::Path { field: P.into() },
        ];
        // Both ways of answering: always the small-set strategy, never it.
        for t in [
            Thresholds { small_match: u64::MAX, small_candidates: u64::MAX },
            Thresholds { small_match: 0, small_candidates: 0 },
        ] {
            self.store.set_thresholds(t);
            self.check_queries(&queries, &sorts, t);
        }
        for &d in dirs.iter().chain([&ROOT]) {
            let mut want = self.reference.descendants(P, d);
            let mut got = self.store.descendants(P, d).unwrap();
            want.sort();
            got.sort();
            assert_eq!(got, want, "descendants of {d}");
        }
        for u in self.reference.uuids() {
            assert_eq!(self.store.get(u).unwrap().as_ref(), self.reference.get(u));
        }
    }

    fn check_queries(&self, queries: &[Q], sorts: &[Sort], t: Thresholds) {
        for q in queries {
            for s in sorts {
                for limit in [5, 100, usize::MAX] {
                    let want = self.reference.query(q, s, limit).unwrap();
                    let got = self.store.query(q, s, limit).unwrap();
                    assert_eq!(got, want, "query {q:?} sort {s:?} limit {limit} {t:?}");
                }
            }
        }
    }
}

fn uuid(r: &mut Rng) -> Uuid {
    Uuid::from_u128(((r.next() as u128) << 64) | r.next() as u128)
}

#[test]
fn the_store_agrees_with_the_reference() {
    let dir = TempDir::new("oracle");
    let mut both = Both { store: Store::open(dir.path()).unwrap(), reference: Reference::new() };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // A root, then an 4-way tree of directories.
    let root = uuid(&mut rng);
    let mut dirs = vec![root];
    let mut ops = vec![Op::Create(Record { uuid: root, fields: vec![(P.into(), tree(ROOT, ""))] })];
    for k in 1..40 {
        let u = uuid(&mut rng);
        let parent = dirs[(k - 1) / 4];
        ops.push(Op::Create(Record {
            uuid: u,
            fields: vec![(P.into(), tree(parent, &format!("dir{k}")))],
        }));
        dirs.push(u);
    }
    both.apply(ops);

    let mut files = Vec::new();
    let file = |rng: &mut Rng, i: usize, parent: Uuid| {
        let mut fields = vec![
            (P.to_string(), tree(parent, &format!("file{i}.txt"))),
            ("kind".into(), Value::Str(["photo", "note", "song"][rng.below(3)].into())),
            ("size".into(), Value::Int(rng.below(1000) as i64)),
            ("mtime".into(), Value::Time(rng.below(10_000) as i64)),
            ("title".into(), Value::Str(format!("lorem {}", rng.below(2000)))),
        ];
        for _ in 0..rng.below(3) {
            fields.push(("tag".into(), Value::Str(["red", "green", "blue"][rng.below(3)].into())));
        }
        if rng.below(4) == 0 {
            fields.push(("rating".into(), Value::Nothing));
        } else if rng.below(2) == 0 {
            fields.push(("rating".into(), Value::Int(rng.below(5) as i64)));
        }
        fields
    };
    // Several transactions, so the store is read back between them.
    for batch in 0..3 {
        let mut ops = Vec::new();
        for i in batch * 200..(batch + 1) * 200 {
            let u = uuid(&mut rng);
            let parent = dirs[rng.below(dirs.len())];
            ops.push(Op::Create(Record { uuid: u, fields: file(&mut rng, i, parent) }));
            files.push(u);
        }
        both.apply(ops);
    }
    both.check(&dirs);

    // Moves (subtrees), renames, a cycle and a taken name (both rejected).
    let mut ops = Vec::new();
    for _ in 0..15 {
        let d = dirs[1 + rng.below(dirs.len() - 1)];
        let to = dirs[rng.below(dirs.len())];
        ops.push(Op::Set(d, P, vec![tree(to, &format!("moved{}", rng.below(1000)))]));
    }
    ops.push(Op::Set(dirs[1], P, vec![tree(dirs[5], "cycle")]));
    ops.push(Op::Set(files[0], P, vec![tree(root, "dir1")]));
    both.apply(ops);
    both.check(&dirs);

    // Field rewrites, files leaving the forest, deletes (a directory with
    // children is refused), creations after deletes.
    let mut ops = Vec::new();
    for k in 0..60 {
        let f = files[rng.below(files.len())];
        match k % 4 {
            0 => ops.push(Op::Set(f, "tag", vec![])),
            1 => ops.push(Op::Set(f, "size", vec![Value::Int(42), Value::Int(700)])),
            2 => ops.push(Op::Set(f, P, vec![Value::Nothing])),
            _ => ops.push(Op::Set(f, "kind", vec![Value::Str("photo".into())])),
        }
    }
    for _ in 0..40 {
        ops.push(Op::Delete(files[rng.below(files.len())]));
    }
    ops.push(Op::Delete(dirs[1]));
    for i in 1000..1050 {
        let u = uuid(&mut rng);
        let parent = dirs[rng.below(dirs.len())];
        ops.push(Op::Create(Record { uuid: u, fields: file(&mut rng, i, parent) }));
    }
    both.apply(ops);
    both.check(&dirs);

    // Reopening: nothing lives only in memory.
    let Both { store, reference } = both;
    drop(store);
    let both = Both { store: Store::open(dir.path()).unwrap(), reference };
    both.check(&dirs);
}
