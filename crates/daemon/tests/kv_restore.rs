//! A record given back keeps its dense id, whatever took it away and whatever
//! gives it back (doc "Deleting a metarecord in the store"): a deletion of one
//! record or of many, still pending or folded, given back one record at a time
//! or all at once, by a revert or by a creation under its uuid. After each, the
//! rows are the ones expected, the dense id is the same, and the indexes agree
//! with the rows (`check_derived`).

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::revert;
use metafolder_daemon::rows::FieldRow;
use metafolder_daemon::store::{Log, Rows};
use roaring::RoaringBitmap;
use uuid::Uuid;

mod common;
use common::TempDir;

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn tree(parent: Uuid, name: &str) -> Value {
    Value::TreeRef { parent: Some(parent), name: TreeName::from(name) }
}

struct Fx {
    store: KvStore,
    _dir: TempDir,
    root: Uuid,
    folder: Uuid,
    files: Vec<Uuid>,
}

fn fields(folder: Uuid, i: usize) -> Vec<Field> {
    vec![
        Field::new("loc", tree(folder, &format!("f{i}.txt"))),
        Field::new("tag", s(["blue", "green"][i % 2])),
        Field::new("rank", Value::Int(i as i64 % 7)),
    ]
}

/// A root, a folder and `n` files in it.
fn fixture(n: usize) -> Fx {
    let dir = TempDir::new("kv-restore");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "loc",
            Value::TreeRef { parent: None, name: TreeName::from("") },
        )])
        .unwrap()
        .uuid;
    let folder = w.create_metarecord(vec![Field::new("loc", tree(root, "dir"))]).unwrap().uuid;
    let files = (0..n).map(|i| w.create_metarecord(fields(folder, i)).unwrap().uuid).collect();
    w.commit().unwrap();
    Fx { store, _dir: dir, root, folder, files }
}

impl Fx {
    fn rows(&self, u: Uuid) -> Vec<String> {
        let mut rows: Vec<String> = Rows::rows(&self.store, u)
            .unwrap()
            .into_iter()
            .map(|r: FieldRow| format!("{} {:?}", r.name, r.value))
            .collect();
        rows.sort();
        rows
    }

    fn all_rows(&self) -> Vec<Vec<String>> {
        let mut all = vec![self.root, self.folder];
        all.extend(&self.files);
        all.into_iter().map(|u| self.rows(u)).collect()
    }

    fn ids(&self, uuids: &[Uuid]) -> RoaringBitmap {
        self.store.dense_ids(uuids).unwrap()
    }

    fn check(&self) {
        let diff = self.store.check_derived().unwrap();
        assert!(diff.is_empty(), "the indexes differ from the rows: {diff:#?}");
    }

    fn write<T>(&mut self, f: impl FnOnce(&mut Writer) -> T) -> T {
        let mut w = Writer::begin(&mut self.store, None).unwrap();
        let out = f(&mut w);
        w.commit().unwrap();
        out
    }

    fn delete(&mut self, u: Uuid) {
        self.write(|w| w.delete_metarecord(u).unwrap());
    }

    /// A deletion of many records, as one entry; its label.
    fn mass_delete(&mut self, uuids: &[Uuid]) -> i64 {
        let ids = self.ids(uuids);
        let mut w = Writer::begin(&mut self.store, Some("delete".into())).unwrap();
        let label = w.label_id();
        w.mass_delete_ids(&ids).unwrap();
        w.commit().unwrap();
        label
    }

    /// Reverts the operations of `label` that name `only` (all when empty).
    fn revert_label(&mut self, label: i64, only: &[Uuid]) {
        let ops: Vec<_> = revert::label_ops(&self.store, label)
            .unwrap()
            .into_iter()
            .filter(|o| only.is_empty() || only.contains(&o.entity_uuid))
            .collect();
        let mut w = Writer::begin(&mut self.store, None).unwrap();
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();
    }

    fn fold(&mut self) {
        while self.store.fold_step().unwrap() {}
    }
}

/// A mass add still pending names the record; it is deleted one by one, then
/// given back: its rows are those it had, the add included, and the entry does
/// not touch them twice.
#[test]
fn a_record_named_by_a_pending_entry_comes_back_with_its_rows() {
    let mut fx = fixture(40);
    let ids = fx.ids(&fx.files);
    fx.write(|w| w.mass_add_ids("jazz", s("x"), &ids).unwrap());
    let before = fx.all_rows();
    let id = fx.ids(&fx.files[3..4]);
    let head = fx.store.head().unwrap();
    fx.delete(fx.files[3]);
    common::kv::revert_since(&mut fx.store, head);
    assert_eq!(fx.all_rows(), before);
    assert_eq!(fx.ids(&fx.files[3..4]), id);
    fx.check();
    fx.fold();
    assert_eq!(fx.all_rows(), before);
    fx.check();
}

/// The same, with the add folded into the records first.
#[test]
fn a_record_named_by_a_folded_entry_comes_back_with_its_rows() {
    let mut fx = fixture(40);
    let ids = fx.ids(&fx.files);
    fx.write(|w| w.mass_add_ids("jazz", s("x"), &ids).unwrap());
    fx.fold();
    let before = fx.all_rows();
    let head = fx.store.head().unwrap();
    fx.delete(fx.files[3]);
    common::kv::revert_since(&mut fx.store, head);
    assert_eq!(fx.all_rows(), before);
    fx.check();
}

/// A mass deletion given back one record at a time while it is pending: the
/// records come back under their ids, the others stay deleted.
#[test]
fn a_mass_deletion_is_given_back_one_record_at_a_time_while_pending() {
    let mut fx = fixture(40);
    let before = fx.all_rows();
    let ids = fx.ids(&fx.files);
    let some = [fx.files[5], fx.files[6]];
    let some_ids = fx.ids(&some);
    let label = fx.mass_delete(&fx.files.clone());
    fx.revert_label(label, &some);
    assert_eq!(fx.ids(&some), some_ids);
    assert_eq!(fx.rows(some[0]), before[2 + 5]);
    assert_eq!(fx.rows(some[1]), before[2 + 6]);
    assert!(Rows::rows(&fx.store, fx.files[7]).unwrap().is_empty());
    fx.check();
    fx.fold();
    fx.check();
    // The rest, folded, then.
    let rest: Vec<Uuid> = fx.files.iter().copied().filter(|u| !some.contains(u)).collect();
    fx.revert_label(label, &rest);
    assert_eq!(fx.all_rows(), before);
    assert_eq!(fx.ids(&fx.files), ids);
    fx.check();
}

/// Given back at once, pending and folded.
#[test]
fn a_mass_deletion_is_given_back_at_once_pending_or_folded() {
    for folded in [false, true] {
        let mut fx = fixture(40);
        let before = fx.all_rows();
        let ids = fx.ids(&fx.files);
        let label = fx.mass_delete(&fx.files.clone());
        if folded {
            fx.fold();
        }
        fx.check();
        fx.revert_label(label, &[]);
        assert_eq!(fx.all_rows(), before, "folded: {folded}");
        assert_eq!(fx.ids(&fx.files), ids, "folded: {folded}");
        fx.check();
    }
}

/// Deleted in bulk, one given back, deleted in bulk again (the mark is cut
/// into pieces), folded, then everything given back.
#[test]
fn marks_cut_by_a_restoration_are_given_back_in_the_end() {
    let mut fx = fixture(40);
    let before = fx.all_rows();
    let ids = fx.ids(&fx.files);
    let first = fx.mass_delete(&fx.files.clone());
    fx.revert_label(first, &[fx.files[10]]);
    fx.check();
    let pair = fx.files[10..12].to_vec();
    let second = fx.mass_delete(&pair);
    fx.check();
    fx.fold();
    fx.check();
    fx.revert_label(second, &[]);
    let rest: Vec<Uuid> = fx.files.iter().copied().filter(|u| *u != fx.files[10]).collect();
    fx.revert_label(first, &rest);
    assert_eq!(fx.all_rows(), before);
    assert_eq!(fx.ids(&fx.files), ids);
    fx.check();
}

/// A folder deleted with its whole subtree in bulk, given back: the files
/// first, then the folder.
#[test]
fn a_subtree_deleted_in_bulk_is_given_back() {
    for folded in [false, true] {
        let mut fx = fixture(40);
        let before = fx.all_rows();
        let mut doomed = fx.files.clone();
        doomed.push(fx.folder);
        let label = fx.mass_delete(&doomed);
        if folded {
            fx.fold();
        }
        fx.check();
        fx.revert_label(label, &[]);
        assert_eq!(fx.all_rows(), before, "folded: {folded}");
        fx.check();
    }
}

/// A position a live record took while the record was deleted is not given
/// back: the revert is refused, and nothing is half done.
#[test]
fn a_position_taken_meanwhile_refuses_the_restoration() {
    let mut fx = fixture(3);
    let head = fx.store.head().unwrap();
    let (folder, file) = (fx.folder, fx.files[0]);
    fx.delete(file);
    let taken = fx.write(|w| {
        w.create_metarecord(vec![Field::new("loc", tree(folder, "f0.txt"))]).unwrap().uuid
    });
    let ops = fx.store.ops_after(head.unwrap_or(0)).unwrap();
    let deletion = ops.into_iter().find(|o| o.op_type == "delete_metarecord").unwrap();
    {
        let mut w = Writer::begin(&mut fx.store, None).unwrap();
        assert!(revert::apply(&mut w, &[deletion]).is_err() || w.commit().is_err());
    }
    assert!(Rows::exists(&fx.store, taken).unwrap());
    assert!(!Rows::exists(&fx.store, file).unwrap());
    fx.check();
}

/// Delete, give back, delete again, give back: the id never moves, and each
/// cycle leaves the indexes whole.
#[test]
fn many_cycles_keep_the_id_and_the_indexes() {
    for mass in [false, true] {
        let mut fx = fixture(40);
        let before = fx.all_rows();
        let ids = fx.ids(&fx.files);
        for cycle in 0..3 {
            if mass {
                let label = fx.mass_delete(&fx.files.clone());
                fx.check();
                fx.revert_label(label, &[]);
            } else {
                let head = fx.store.head().unwrap();
                fx.delete(fx.files[4]);
                fx.check();
                common::kv::revert_since(&mut fx.store, head);
            }
            assert_eq!(fx.all_rows(), before, "mass {mass}, cycle {cycle}");
            assert_eq!(fx.ids(&fx.files), ids, "mass {mass}, cycle {cycle}");
            fx.check();
        }
    }
}

/// A record written to after it was given back, then deleted and given back
/// again, comes back as it was last.
#[test]
fn a_record_changed_between_two_deletions_comes_back_as_last_seen() {
    let mut fx = fixture(40);
    let id = fx.ids(&fx.files[2..3]);
    let head = fx.store.head().unwrap();
    fx.delete(fx.files[2]);
    common::kv::revert_since(&mut fx.store, head);
    let file = fx.files[2];
    fx.write(|w| {
        w.set_field(file, "tag", s("purple")).unwrap();
        w.append_field(file, "extra", Value::Int(9)).unwrap();
    });
    let last = fx.rows(fx.files[2]);
    let head = fx.store.head().unwrap();
    fx.delete(fx.files[2]);
    common::kv::revert_since(&mut fx.store, head);
    assert_eq!(fx.rows(fx.files[2]), last);
    assert_eq!(fx.ids(&fx.files[2..3]), id);
    fx.check();
}

/// A reindex numbers the ids again and frees the records of the deleted
/// metarecords: a revert after it gives them back under new ids.
#[test]
fn a_revert_after_a_reindex_gives_the_record_back() {
    let mut fx = fixture(40);
    let before = fx.all_rows();
    let head = fx.store.head().unwrap();
    fx.delete(fx.files[1]);
    let batch = fx.files[10..30].to_vec();
    let label = fx.mass_delete(&batch);
    fx.store.reindex().unwrap();
    fx.check();
    let _ = label;
    common::kv::revert_since(&mut fx.store, head);
    assert_eq!(fx.all_rows(), before);
    fx.check();
}

/// A creation under the uuid of a deleted record: with the deletion pending in
/// bulk, folded, or one by one, the record comes back under its id with the
/// fields it is created with, and nothing of what it had.
#[test]
fn a_creation_under_a_known_uuid_gives_the_new_fields_only() {
    for how in ["one", "pending", "folded"] {
        let mut fx = fixture(40);
        let uuid = fx.files[8];
        let id = fx.ids(&[uuid]);
        match how {
            "one" => fx.delete(uuid),
            _ => {
                fx.mass_delete(&fx.files.clone());
                if how == "folded" {
                    fx.fold();
                }
            }
        }
        let folder = fx.folder;
        fx.write(|w| {
            w.create_metarecord_with_uuid(
                uuid,
                vec![Field::new("loc", tree(folder, "new.txt")), Field::new("tag", s("fresh"))],
            )
            .unwrap()
        });
        assert_eq!(fx.ids(&[uuid]), id, "{how}");
        assert_eq!(fx.rows(uuid).len(), 2, "{how}");
        assert!(fx.rows(uuid).iter().any(|r| r.contains("fresh")), "{how}");
        fx.check();
        fx.fold();
        fx.check();
    }
}

/// A deletion, its record kept, and a store of the version before: the record
/// of a deleted metarecord was removed then, with indexes still describing it.
/// Opening such a store derives everything again, and the revert still works.
#[test]
fn a_store_of_the_version_before_is_derived_again_when_it_opens() {
    use common::kv::{open_raw, raw_env};
    let mut fx = fixture(10);
    let before = fx.all_rows();
    let head = fx.store.head().unwrap();
    let id = fx.ids(&fx.files[4..5]).iter().next().unwrap();
    fx.delete(fx.files[4]);
    let Fx { store, _dir, files, .. } = fx;
    drop(store);
    {
        // As the version before left it: no record, an older `derived`, no flag.
        let env = raw_env(_dir.path());
        let records = open_raw(&env, "records");
        let meta = open_raw(&env, "version");
        let mut w = env.write_txn().unwrap();
        assert!(records.delete(&mut w, &id.to_be_bytes()).unwrap(), "the record was kept");
        meta.put(&mut w, b"derived_version", &14i64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let mut store = KvStore::open(_dir.path()).unwrap();
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    assert!(!Rows::exists(&store, files[4]).unwrap());
    // Derived again: the ids were numbered anew, without the deleted one.
    assert!(store.dense_ids(&files[4..5]).unwrap().is_empty());
    common::kv::revert_since(&mut store, head);
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    let mut rows: Vec<String> = Rows::rows(&store, files[4])
        .unwrap()
        .into_iter()
        .map(|r| format!("{} {:?}", r.name, r.value))
        .collect();
    rows.sort();
    assert_eq!(rows, before[2 + 4]);
}

/// What the trash does: the deleted record is created again under its uuid,
/// with its fields, while another record holds its position: refused.
#[test]
fn a_creation_under_a_known_uuid_is_refused_when_its_position_is_taken() {
    let mut fx = fixture(3);
    let (folder, file) = (fx.folder, fx.files[0]);
    let own = fields(folder, 0);
    fx.delete(file);
    let taken = fx.write(|w| {
        w.create_metarecord(vec![Field::new("loc", tree(folder, "f0.txt"))]).unwrap().uuid
    });
    {
        let mut w = Writer::begin(&mut fx.store, None).unwrap();
        let made = w.create_metarecord_with_uuid(file, own);
        assert!(made.is_err() || w.commit().is_err(), "the position is taken");
    }
    assert!(Rows::exists(&fx.store, taken).unwrap());
    assert!(!Rows::exists(&fx.store, file).unwrap());
    assert!(Rows::rows(&fx.store, file).unwrap().is_empty());
    fx.check();
}

/// Given back in bulk while a live record took a position of one of them: the
/// revert is refused, and the positions stay unique.
#[test]
fn a_bulk_restoration_does_not_take_a_position_back_from_a_live_record() {
    for folded in [false, true] {
        let mut fx = fixture(40);
        let (folder, file) = (fx.folder, fx.files[3]);
        let label = fx.mass_delete(&fx.files.clone());
        if folded {
            fx.fold();
        }
        let taken = fx.write(|w| {
            w.create_metarecord(vec![Field::new("loc", tree(folder, "f3.txt"))]).unwrap().uuid
        });
        {
            let ops = revert::label_ops(&fx.store, label).unwrap();
            let mut w = Writer::begin(&mut fx.store, None).unwrap();
            let done = revert::apply(&mut w, &ops);
            assert!(done.is_err() || w.commit().is_err(), "folded: {folded}");
        }
        assert!(Rows::exists(&fx.store, taken).unwrap());
        assert!(!Rows::exists(&fx.store, file).unwrap(), "folded: {folded}");
        fx.check();
    }
}

/// What the trash does after a bulk deletion: a record is created again under
/// its uuid while another holds its position, which the deletion left in the
/// forest. Refused — two live records never share a position.
#[test]
fn a_creation_under_a_uuid_deleted_in_bulk_is_refused_when_its_position_is_taken() {
    let (mut store, _dir) = common::kv::store();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: None, name: TreeName::from("") },
        )])
        .unwrap()
        .uuid;
    let m1 = w.create_metarecord(vec![Field::new("mfr_path", tree(root, "A.txt"))]).unwrap().uuid;
    let m2 = w.create_metarecord(vec![Field::new("label", s("other"))]).unwrap().uuid;
    w.commit().unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let ids = w.dense_ids(&[m1]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(m2, "mfr_path", tree(root, "A.txt")).unwrap();
    w.commit().unwrap();
    {
        let mut w = Writer::begin(&mut store, None).unwrap();
        let made =
            w.create_metarecord_with_uuid(m1, vec![Field::new("mfr_path", tree(root, "A.txt"))]);
        assert!(made.is_err() || w.commit().is_err(), "the position is taken");
    }
    assert!(Rows::rows(&store, m1).unwrap().is_empty());
    assert_eq!(Rows::rows(&store, m2).unwrap().len(), 2);
    assert!(store.check_derived().unwrap().is_empty());
}
