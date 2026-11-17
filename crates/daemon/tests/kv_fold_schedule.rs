//! What a store answers does not depend on when it folds (doc "Folding in idle
//! time"): a fold writes in what the writes to many records left pending, and
//! changes no answer. The same random writes — to one record and to many,
//! deletions, undos of earlier ones — run on two stores, one folded whole after
//! each write and one folded a list at a time, at random; after each write,
//! both read the same rows, the same log and the same indexes.

use std::collections::BTreeSet;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::revert;
use metafolder_daemon::store::{Begin, Log, Rows};
use uuid::Uuid;

mod common;
use common::TempDir;

/// A small deterministic generator (xorshift64*): the cases are reproducible
/// from their seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// `k` distinct elements of `from`, at random.
    fn some<T: Copy>(&mut self, from: &[T], k: usize) -> Vec<T> {
        let mut pool: Vec<T> = from.to_vec();
        let mut out = Vec::new();
        while out.len() < k && !pool.is_empty() {
            out.push(pool.swap_remove(self.below(pool.len())));
        }
        out
    }
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

const TAGS: [&str; 4] = ["a", "b", "c", "d"];

/// The first records are folders at the root of a forest (`loc`), the others
/// files placed under them.
const FOLDERS: usize = 4;

/// The position of record `u` under `parent`, named after it: never taken by
/// another.
fn place(parent: Option<Uuid>, u: Uuid) -> Field {
    let name =
        metafolder_core::metarecord::TreeName::from(format!("n{}", u.as_u128() % 100_000).as_str());
    Field::new("loc", Value::TreeRef { parent, name })
}

/// The live records placed under `folder`.
fn children(
    store: &dyn metafolder_daemon::store::Store,
    folder: Uuid,
    uuids: &[Uuid],
) -> Vec<Uuid> {
    uuids
        .iter()
        .copied()
        .filter(|&u| {
            store.rows(u).unwrap().iter().any(|r| {
                r.name == "loc"
                    && matches!(&r.value, Value::TreeRef { parent: Some(p), .. } if *p == folder)
            })
        })
        .collect()
}

/// One step of a case, drawn once and played on both stores.
#[derive(Clone, Debug)]
enum Step {
    MassAdd(Vec<Uuid>, &'static str),
    MassRemove(Vec<Uuid>, &'static str),
    MassSet(Vec<Uuid>, i64),
    MassUnset(Vec<Uuid>),
    /// Points a reference of the records at another record.
    MassRef(Vec<Uuid>, Uuid),
    MassDelete(Vec<Uuid>),
    Append(Uuid, &'static str),
    Remove(Uuid, &'static str),
    Set(Uuid, i64),
    Delete(Uuid),
    Create(Uuid),
    /// Undoes the write of the `n`-th label taken so far.
    Undo(usize),
    /// Deletes a folder with every record under it: a deletion of many records
    /// that takes a folder whole.
    DeleteFolder(Uuid),
    /// Moves a file under a folder.
    Move(Uuid, Uuid),
}

impl Step {
    /// The records a write to many names.
    fn records(&self) -> &[Uuid] {
        match self {
            Step::MassAdd(u, _)
            | Step::MassRemove(u, _)
            | Step::MassSet(u, _)
            | Step::MassUnset(u)
            | Step::MassRef(u, _)
            | Step::MassDelete(u) => u,
            _ => &[],
        }
    }
}

/// Each record — whether it exists, its rows — and each operation of the log.
type State = (Vec<(Uuid, bool, Vec<String>)>, Vec<String>);

struct Side {
    store: KvStore,
    labels: Vec<i64>,
    /// Writes to many records made as a write to each, as before they were
    /// one entry: the oracle of what they do to the records.
    one_by_one: bool,
    uuids: Vec<Uuid>,
    _dir: TempDir,
}

impl Side {
    fn new(uuids: &[Uuid], one_by_one: bool) -> Side {
        let dir = TempDir::new("kv-fold-schedule");
        let mut store = KvStore::open_unsynced(dir.path()).unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        for (i, &u) in uuids.iter().enumerate() {
            let parent = (i >= FOLDERS).then(|| uuids[i % FOLDERS]);
            w.create_metarecord_with_uuid(
                u,
                vec![
                    place(parent, u),
                    Field::new("tag", s(TAGS[i % 2])),
                    Field::new("rank", Value::Int(i as i64 % 3)),
                ],
            )
            .unwrap();
        }
        w.commit().unwrap();
        Side { store, labels: Vec::new(), one_by_one, uuids: uuids.to_vec(), _dir: dir }
    }

    /// Plays `step`; whether it was written (an undo may be refused). An
    /// `abort`ed step is written and dropped before its commit: as if it never
    /// was.
    fn play(&mut self, step: &Step, abort: bool) -> Result<(), String> {
        let mut w = Writer::begin(&mut self.store, Some("step".into())).unwrap();
        let label = w.label_id();
        let ids = |w: &Writer, uuids: &[Uuid]| w.dense_ids(uuids).unwrap();
        let one_by_one = self.one_by_one;
        let done: anyhow::Result<()> = (|| {
            let each =
                |w: &mut Writer, f: &mut dyn FnMut(&mut Writer, Uuid) -> anyhow::Result<()>| {
                    for &u in step.records() {
                        if w.store().exists(u)? {
                            f(w, u)?;
                        }
                    }
                    anyhow::Ok(())
                };
            match step {
                Step::MassAdd(_, t) if one_by_one => {
                    each(&mut w, &mut |w, u| w.append_field(u, "tag", s(t)).map(|_| ()))?;
                }
                Step::MassRemove(_, t) if one_by_one => {
                    each(&mut w, &mut |w, u| w.delete_fields_valued(u, "tag", &s(t)).map(|_| ()))?;
                }
                Step::MassSet(_, r) if one_by_one => {
                    each(&mut w, &mut |w, u| w.set_field_multi(u, "rank", vec![Value::Int(*r)]))?;
                }
                Step::MassUnset(_) if one_by_one => {
                    each(&mut w, &mut |w, u| w.delete_fields_named(u, "rank").map(|_| ()))?;
                }
                Step::MassRef(_, to) if one_by_one => {
                    each(&mut w, &mut |w, u| w.set_field_multi(u, "by", vec![Value::Ref(*to)]))?;
                }
                Step::MassRef(u, to) => {
                    let ids = ids(&w, u);
                    w.mass_set_ids("by", vec![Value::Ref(*to)], &ids)?;
                }
                Step::MassDelete(_) if one_by_one => {
                    each(&mut w, &mut |w, u| w.delete_metarecord(u))?;
                }
                Step::MassAdd(u, t) => {
                    let ids = ids(&w, u);
                    w.mass_add_ids("tag", s(t), &ids)?;
                }
                Step::MassRemove(u, t) => {
                    let ids = ids(&w, u);
                    w.mass_remove_ids("tag", &s(t), &ids)?;
                }
                Step::MassSet(u, r) => {
                    let ids = ids(&w, u);
                    w.mass_set_ids("rank", vec![Value::Int(*r)], &ids)?;
                }
                Step::MassUnset(u) => {
                    let ids = ids(&w, u);
                    w.mass_unset_ids("rank", &ids)?;
                }
                Step::MassDelete(u) => {
                    let ids = ids(&w, u);
                    w.mass_delete_ids(&ids)?;
                }
                Step::Append(u, t) => {
                    if w.store().exists(*u)? {
                        w.append_field(*u, "tag", s(t))?;
                    }
                }
                Step::Remove(u, t) => {
                    if w.store().exists(*u)? {
                        w.delete_fields_valued(*u, "tag", &s(t))?;
                    }
                }
                Step::Set(u, r) => {
                    if w.store().exists(*u)? {
                        w.set_field_multi(*u, "rank", vec![Value::Int(*r)])?;
                    }
                }
                Step::Delete(u) => {
                    if w.store().exists(*u)? {
                        w.delete_metarecord(*u)?;
                    }
                }
                Step::Create(u) => {
                    if !w.store().exists(*u)? {
                        let fields = vec![place(None, *u), Field::new("tag", s("a"))];
                        w.create_metarecord_with_uuid(*u, fields)?;
                    }
                }
                Step::Undo(n) => {
                    let ops = revert::label_ops(w.store(), self.labels[*n])?;
                    revert::apply(&mut w, &ops)?;
                }
                Step::DeleteFolder(f) => {
                    if w.store().exists(*f)? {
                        let mut all = children(w.store(), *f, &self.uuids);
                        all.push(*f);
                        if one_by_one {
                            for u in all {
                                w.delete_metarecord(u)?;
                            }
                        } else {
                            let ids = ids(&w, &all);
                            w.mass_delete_ids(&ids)?;
                        }
                    }
                }
                Step::Move(u, f) => {
                    if w.store().exists(*u)? {
                        let Field { value, .. } = place(Some(*f), *u);
                        w.set_field_multi(*u, "loc", vec![value])?;
                    }
                }
            }
            Ok(())
        })();
        match done {
            _ if abort => Err("aborted".into()),
            Ok(()) => {
                w.commit().map_err(|e| format!("{e:#}"))?;
                self.labels.push(label);
                Ok(())
            }
            Err(e) => Err(format!("{e:#}")),
        }
    }

    /// What the store answers, in a form two stores compare on.
    fn state(&self, uuids: &[Uuid]) -> State {
        let mut records = Vec::new();
        for &u in uuids {
            let mut rows: Vec<String> = Rows::rows(&self.store, u)
                .unwrap()
                .iter()
                .map(|r| format!("{}={:?}", r.name, r.value))
                .collect();
            rows.sort();
            records.push((u, Rows::exists(&self.store, u).unwrap(), rows));
        }
        let mut log = Vec::new();
        for op in self.store.ops_after(0).unwrap() {
            let before: BTreeSet<String> = self
                .store
                .snapshots(op.id)
                .unwrap()
                .iter()
                .map(|r| format!("{}={:?}", r.name, r.value))
                .collect();
            log.push(format!(
                "{} {} {} {:?} {:?} before={before:?}",
                op.id, op.op_type, op.entity_uuid, op.field_name, op.reverts_op_id,
            ));
        }
        (records, log)
    }
}

fn draw(rng: &mut Rng, uuids: &[Uuid], labels: usize) -> Step {
    let one = uuids[rng.below(uuids.len())];
    let many = {
        let k = 16 + rng.below(uuids.len() - 16);
        rng.some(uuids, k)
    };
    let tag = TAGS[rng.below(TAGS.len())];
    // Writes to one record often, so that some are newer than the entries a
    // write to many left; undos of the newest label most, which the one-pass
    // paths of an undo require.
    let folder = uuids[rng.below(FOLDERS)];
    let file = uuids[FOLDERS + rng.below(uuids.len() - FOLDERS)];
    match rng.below(21) {
        16 => return Step::DeleteFolder(folder),
        17 | 18 => return Step::Move(file, folder),
        19 | 20 => return Step::MassRef(many, one),
        _ => {}
    }
    match rng.below(16) {
        0 => Step::MassAdd(many, tag),
        1 => Step::MassRemove(many, tag),
        2 => Step::MassSet(many, rng.below(3) as i64),
        3 => Step::MassUnset(many),
        4 | 5 => Step::MassDelete(many),
        6 | 7 => Step::Append(one, tag),
        8 => Step::Remove(one, tag),
        9 => Step::Set(one, rng.below(3) as i64),
        10 => Step::Delete(one),
        11 => Step::Create(one),
        _ if labels > 0 && rng.below(2) == 0 => Step::Undo(labels - 1),
        _ if labels > 0 => Step::Undo(labels - 1 - rng.below(labels.min(4))),
        _ => Step::Create(one),
    }
}

fn run_case(seed: u64, steps: usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let uuids: Vec<Uuid> =
        (0..28u128).map(|i| Uuid::from_u128(seed as u128 * 1000 + i + 1)).collect();
    // `whole` folds everything after each step; `steps` a few lists, at random.
    let mut whole = Side::new(&uuids, false);
    let mut lazy = Side::new(&uuids, false);
    let mut oracle = Side::new(&uuids, true);
    let mut played = Vec::new();
    for at in 0..steps {
        let step = draw(&mut rng, &uuids, whole.labels.len());
        played.push(step.clone());
        let abort = rng.below(8) == 0;
        let (a, b, c) =
            (whole.play(&step, abort), lazy.play(&step, abort), oracle.play(&step, abort));
        if std::env::var_os("FOLD_SCHEDULE_TRACE").is_some() {
            eprintln!(
                "TRACE {} {:?}",
                a.is_ok(),
                a.as_ref().err().map(|e| e.chars().take(90).collect::<String>())
            );
        } // TRACE
        assert_eq!(a.is_ok(), b.is_ok(), "seed {seed}, step {at}: {a:?} / {b:?}\n{played:#?}");
        assert_eq!(
            a.is_ok(),
            c.is_ok(),
            "seed {seed}, step {at}: {a:?} / one by one {c:?}\n{played:#?}"
        );
        whole.store.fold().unwrap();
        for _ in 0..rng.below(3) {
            lazy.store.fold_step().unwrap();
        }
        if rng.below(20) == 0 {
            whole.store.reindex().unwrap();
            lazy.store.reindex().unwrap();
            oracle.store.reindex().unwrap();
        }
        let (sa, sb) = (whole.state(&uuids), lazy.state(&uuids));
        let sc = oracle.state(&uuids);
        for (ra, rc) in sa.0.iter().zip(&sc.0) {
            assert_eq!(
                ra, rc,
                "seed {seed}, step {at}: a record differs from one by one\n{played:#?}"
            );
        }
        for (ra, rb) in sa.0.iter().zip(&sb.0) {
            assert_eq!(ra, rb, "seed {seed}, step {at}: a record differs\n{played:#?}");
        }
        for (la, lb) in sa.1.iter().zip(&sb.1) {
            assert_eq!(la, lb, "seed {seed}, step {at}: the log differs\n{played:#?}");
        }
        assert_eq!(sa.1.len(), sb.1.len(), "seed {seed}, step {at}: the log's length");
        let diff = lazy.store.check_derived().unwrap();
        assert!(diff.is_empty(), "seed {seed}, step {at}: {diff:#?}\n{played:#?}");
    }
    whole.store.fold().unwrap();
    lazy.store.fold().unwrap();
    assert_eq!(whole.state(&uuids), lazy.state(&uuids), "seed {seed}: after a fold");
    for side in [&whole, &lazy] {
        let diff = side.store.check().unwrap();
        assert!(diff.is_empty(), "seed {seed}: {diff:#?}\n{played:#?}");
    }
}

/// Eight seeds by default; `FOLD_SCHEDULE_SEEDS=300` for a long run (in
/// release: about a minute), `FOLD_SCHEDULE_SEED=n` for one case.
#[test]
fn the_answers_do_not_depend_on_when_the_store_folds() {
    let seeds: u64 =
        std::env::var("FOLD_SCHEDULE_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    let only: Option<u64> = std::env::var("FOLD_SCHEDULE_SEED").ok().and_then(|s| s.parse().ok());
    for seed in only.map_or(1..=seeds, |s| s..=s) {
        run_case(seed, 40);
    }
}
