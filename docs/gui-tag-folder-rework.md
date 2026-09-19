# `gui-tag-folder.sh` — the query is the walk state

Settled design, not yet coded. It **supersedes** `review-followups.md` §12
("Les scripts de tagging tiennent tout le scope en mémoire"), whose conclusion —
that paging costs either the walk order or the exact total — is wrong: this
design keeps both. Rewrite §12 when this lands.

## What is wrong today

`collect` reads the *whole* scope before the first question: ordered uuids and
`uuid → path` for each of the two kinds, then three more full-scope reads for the
already-decided sets. Seven reads, all of them before the user is asked
anything. The result lives in bash associative arrays (`RANK`, `KIND`,
`PATH_OF`, `DECIDED`, `SUBTREE`, `ENTRIES`, `STEPS`), so **the scope is the
memory of the run**, and the walk order is produced by an external `sort` over a
materialised file.

`MF_GUI_MAX_ENTRIES` (20000) caps it. That is a bound, not a fix: it makes a
large scope fail loudly instead of slowly.

## The principle

An answer (`y` / `n` / `m`) is already a **write**. So "what is left to ask" is a
*query*, not bookkeeping: each step asks one question with `--limit 1` over

    <scope> AND <not yet decided> AND <not skipped>

and every answer removes its subject from that set — a `y`/`n` on a folder
writes the tag over `(scope) AND mfr_path ->* "<path>"` (`apply_tree`, unchanged),
so the whole subtree leaves the query at once.

Deleted by construction: `DECIDED` / `load_decided`, `PRUNED` / `is_pruned` /
`prune_subtree`, `SUBTREE`, `RANK` / `KIND` / `PATH_OF` / `ENTRIES` / `WALK` /
`STEPS`, the external `sort`, most of `BACK_STACK` (`mf_log_back_to` un-decides
the entry, which brings it back on its own), and `MF_GUI_MAX_ENTRIES` for this
script.

Cost moves from "seven full-scope reads up front, zero per question" to "none up
front, two or three small round-trips per question" — each behind a human
keypress.

## Walk order

One string: the current folder. **The path is the stack** — going up is
trimming one component, so nothing else is held.

At folder `P`:

1. the first undecided direct child **in scope**, `--sort order_dir` /
   `order_file` then `mfr_path` → ask it. (This is what keeps `mf order`
   numbering honoured: an album is walked by track number.)
2. none? the first undecided in-scope **descendant** of `P`, by path → take its
   *parent* as the new current folder and descend there without asking.
3. neither? go up one component. Above the walk root, the walk is done.

`m` on a folder makes it the current folder. `y` / `n` settle its subtree by
writing it, so it never needs to be entered.

Rule 2 is not an optimisation, it is what keeps a **scattered scope** reachable:
the query is the scope, so `/a` may be out of scope while `/a/b/c.txt` is in it,
and the old (depth, parent, kind, rank) sort reached it. Sorting by path is
sorting in DFS order, so "the first in-scope descendant" is the first one the
walk should reach, and jumping straight to its parent skips every empty level in
one step. Coming back up finds the siblings that were left behind: with
`/a/b/z.txt` and `/a/b/c/d.txt` both in scope, path order yields `c/d.txt`
first, the walk descends to `/a/b/c`, empties it, comes up to `/a/b`, and finds
`z.txt` there.

There is no sort by depth and there cannot be one — spec-query: *"Sorting is not
aspect-aware: a `TreeRef` sort key always orders on the whole path"*. The DFS
order above is what replaces it.

## Skip

A marker field, **`gui_tag_skipped`**, holding a **`ref` to the tag entry** —
the same shape as `tag` / `negative_tag` / `mixed_tag`, not a string and not a
boolean:

- **Valued by tag, not by `true`.** Fields are a multi-map, so one record carries
  one marker per tag, and a skip left by a run on `music` does not silently skip
  the same entries for `jazz`.
- **One clause in the query**, whatever the number of skips:
  `NOT gui_tag_skipped -> (mf_schema = "tag" AND path = "<tag>")`.
- **Name constraints** (both checked): DSL identifiers are
  `[A-Za-z_][A-Za-z0-9_]*`, so no hyphen (`dsl.rs`); and `reserved.rs` rejects
  the `mf_` prefix outright and requires `force` for `mfr_`.
- The tag entry's uuid is read once at start. It may not exist yet (`mf tag`
  creates the vocabulary entry on first write), and "no such tag" must read the
  same as "no skips", not as an error.
- Side effect to accept: the marker shows up in the `treeref` panel's ref bar of
  that tag ("what points here?") beside the real tags.

**Cleanup** — offered on every exit that can still ask: `q`, normal end, error.
Escape kills the script, so nothing runs there; that is fine, and the cleanup
does not move into the GUI. It is offered **again at the next run's start** if
markers remain for this tag ("resume where you were / ask them again"), which is
where the choice is actually informed. `--redo` ignores them.

One call: `mf metarecord -q '<scope> AND gui_tag_skipped -> (…)' field delete
gui_tag_skipped:ref=<tag-uuid>` — `delete` removes that row only, `unset` would
remove every tag's marker.

## The counter

`total` and `N left` are both a `count` over `<scope> AND undecided AND NOT
skipped`: one round-trip per question, exact, and it drops by a whole subtree the
moment a folder is answered whole — which is exactly what `SUBTREE` /
`prune_subtree` existed to compute.

Skipped **folders** are the one correction: their subtree is still undecided and
in scope, so the *count* query carries a `NOT mfr_path ->* "<path>"` per skipped
folder. That list is bounded by how many times a human presses `s`, not by the
size of the scope. (The *step* query needs no such clause: the walk only enters a
folder on `m`.)

## Prerequisites

1. ~~**`:parent` served by the bitmap index.**~~ ✅ *Done (September 2026).* The
   step query is `mfr_path:parent = "<P>"`, and the index used to report
   `Unsupported` for `:parent`, so every step *and every count* of this walk fell
   to the SQL engine. It now serves the aspect from the reverse index's parent
   partition — the children of a caller-resolved node, passed in through
   `QueryRoots.node` exactly as `->*` path targets and exact-node equality are —
   and there is no SQL engine left to fall to (spec-indexing "No operand runs in
   SQL").
2. ~~**`mf … --count`.**~~ ✅ *Done (September 2026).* The CLI exposed no count
   at all, and counting by counting lines is O(scope) output per question, which
   defeats the design. `mf metarecord [-q …] get --count` now prints the match
   count in one round-trip (`count: true` on `POST /query`, O(1) on the index),
   for query selectors only. The `--cursor` half of that roadmap item is *not* a
   prerequisite (this walk never pages: the answered entries leave the set, so
   "the first undecided" is always the right next question).

Both prerequisites are met: the script can be written.

## Behaviour changes to write into the script header

The header currently promises the opposite of two of these:

- a skip now **survives the run** (it is recorded), instead of "the subtree is
  left for another run";
- a skip is **undoable** like any other answer, since it goes through the log;
- the walk order becomes DFS (a folder, then what is under it) instead of level
  by level;
- there is no scope cap any more.

## Tests

TDD against `scripts/test-gui-tag-folder.sh`, which already drives the hermetic
`mf` mock (`scripts/lib/mf-mock.sh`). The cases that must fail first: the
scattered scope (rule 2), a skipped folder excluded from the count but not from
the walk of its siblings, the per-tag isolation of `gui_tag_skipped`, cleanup
offered on `q` and on the next start, `--redo` ignoring markers, and `back`
across a skip.
