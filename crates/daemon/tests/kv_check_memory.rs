//! What a check of the derived key spaces costs in memory (doc "Backups and
//! restore"): it runs inside the daemon — on every automatic backup — so it
//! must hold the store to a rebuild without holding the rebuild. Measured as
//! the heap it allocates, not as a duration: the peak is the same on a loaded
//! machine.
//!
//! One test in its own binary: the allocator below counts every thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;

mod common;
use common::TempDir;

struct Peak;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters are only observed.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOCATOR: Peak = Peak;

/// A text of `len` letters no other call repeats: nearly every trigram of it
/// is its own entry of the trigram index.
fn text(seed: &mut u64, len: usize) -> String {
    (0..len)
        .map(|_| {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (b'a' + ((*seed >> 33) % 26) as u8) as char
        })
        .collect()
}

#[test]
fn a_check_does_not_hold_the_derived_index_in_memory() {
    const RECORDS: usize = 600;
    let dir = TempDir::new("kv-check-memory");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let mut seed = 1;
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "loc",
            Value::TreeRef { parent: None, name: TreeName::from("") },
        )])
        .unwrap()
        .uuid;
    for i in 0..RECORDS {
        w.create_metarecord(vec![
            Field::new(
                "loc",
                Value::TreeRef {
                    parent: Some(root),
                    name: TreeName::from(format!("f{i}").as_str()),
                },
            ),
            Field::new("a", Value::String(text(&mut seed, 300))),
            Field::new("b", Value::String(text(&mut seed, 300))),
            Field::new("c", Value::String(text(&mut seed, 300))),
        ])
        .unwrap();
    }
    w.commit().unwrap();

    // Some 530 000 trigram entries. Held as a set to compare, each costs well
    // over a hundred bytes — and there are two sides.
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let diff = store.check_derived().unwrap();
    let peak = PEAK.load(Ordering::Relaxed) - before;

    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
    assert!(
        peak < 16 << 20,
        "the check allocated {} MiB at its peak for {RECORDS} records",
        peak >> 20
    );
}
