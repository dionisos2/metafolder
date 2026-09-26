# Changelog

All notable changes to metafolder are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once the API stabilises.

**Stability:** the HTTP API, the CLI command tree and the configuration keys
still change between versions without deprecation. **On-disk formats are
preserved, though** — from v0.3 on, any change to a persisted format (the
SQLite schema, the `.metafolder/` layout, the config file format) ships with a
conversion path that migrates existing repositories and configuration rather
than breaking them.

## [Unreleased]

### Added
- **`metafolder-watchd`: the privileged fanotify broker** (`crates/watchd`,
  `scripts/metafolder-watchd.service`) — groundwork for the fanotify watch
  source (docs/watcher-fanotify.md "The broker"). One process per machine holds
  the fanotify group covering the mounts of subscribed repository roots
  (one mark per mount, no per-directory watches), resolves the kernel's file
  handles to paths, and streams events as NDJSON over a Unix socket — each
  subscriber seeing only what its own uid could discover. A subscriber that
  cannot keep up loses events and is told so (`Overflow`) rather than stalling
  the machine. **The daemon can now use it** — with no switch to set: at load
  it probes `[settings] watchd-socket` (default `/run/metafolder/watchd.sock`)
  and takes the broker when one answers, falling back to the inotify source
  with a message naming the socket when none does. Under the fanotify source one
  kernel registration covers the whole tree: no per-directory watches, no watch
  budget, and `mf watch check` answers from the coverage regime (spec-file-
  tracking "Watch sources and regimes"). `mfr_watch_exceeded` is honoured in
  both regimes — under coverage, what happens in such a subtree is dropped at
  ingestion, and a move out of it reads as an arrival.
- **`GET /watch` names the active watch source** — `backend` (`inotify`,
  `fanotify`, …) — and its two budget-only fields (`watched_dirs`,
  `watch_budget`) now answer `null` under the coverage regime, where one kernel
  registration covers the tree and there is no per-directory state to budget
  (spec-file-tracking "Watch sources and regimes"). `mf watch status` shows the
  coverage accordingly — `22838 directory(ies) watched (inotify)` or `tree
  covered (fanotify)` — and the budget line is budget-regime only. Groundwork
  for the fanotify watch source (`docs/watcher-fanotify.md`); the inotify
  source itself is unchanged.
- **Keyboard shortcuts for `repos:load` and `metarecord-list:folder`** — the
  two shipped commands that had none, now in the global `g` ("go") family:
  `g l` (go → load) picks a folder and loads the repository in it, the
  form-less twin of *Load repo…* in the repos panel; `g f` (go → folder) lists
  the selected metarecord's folder as metarecords — the keyboard form of the
  "Open folder in metarecord-list" menu item. Both are global, beside `g o` /
  `g r` / `g s` / `g w`, and read as a pair with `g o`: "point one out on disk"
  / "open one the daemon already knows" (spec-gui "Repository management",
  "Cross-panel selection").
- **`[ref-seeds]`: how a `ref` field's targets are *named* — everywhere at
  once** (spec-gui "Ref value seeds"). One rule per field (and a `*` default
  rule), written `["query", "columns"]`: which metarecords may be named (a
  `metarecord-list` query) and how each is shown (its columns syntax — the
  same vocabulary a list cell is built with). That one naming governs every
  value slot of the field: the candidates a completion offers (the whole line,
  then one column at a time — the views `completion:cycle` walks, a cycled
  view offering only the records that have its column), what typed text must
  spell to name a target (its label, whole, in some view; a uuid always wins;
  an ambiguous or unknown name is a hard error), what a value reads back as
  (its annotation line, the pre-fill of an `edit`), what `remove` and `edit`
  name a row by, and where the value picker opens. Candidates come one
  *counted page* at a time (≤ 100, sorted by the naming), narrowed on the
  typed text — a `*` rule may name every metarecord of the repository and
  still stays as cheap to offer as a closed list. The legacy
  `[picker-seeds]` / `[ref-completion-seeds]` tables are still read where no
  rule names a field; `[completion].label-separator` (default `" | "`) sets
  the join of a multi-column label.
- **Completion candidates are label/value couples, and a completion can have
  several views** (spec-gui "Completion views"). What a prompt's list shows
  and what the command receives are now distinct: a plain string candidate is
  its own label and value (nothing changes for the many completions that are
  plain lists), and where they differ the collected argument is the *value*
  — typed text that spells a label whole names it like a pick does. Two
  candidates that read the same are automatically suffixed with their values,
  so no listed row is a coin flip between two things. An argument may offer
  several *views* of its candidates (one builder per view, a title each),
  walked — wrapping — by `completion:cycle forward|back`, bound by default to
  `ctrl+,` / `ctrl+.`; a builder gets the typed text and answers one page at
  a time (`more`), so narrowing a huge candidate set re-queries instead of
  shipping every row.
- **`help:key` describes a key — Emacs `C-h k`.** Press `h k` (or run
  `help:key`), then the shortcut in question: the shell reports the command
  that key runs *in the current context* — its invocation and its description —
  or that it is not bound to any command, naming the contexts it does work in
  when the exclusion is only contextual (a `file-manager` shortcut asked about
  from another panel reads "not bound here (elsewhere: file-manager:find in
  file-manager, …)" rather than simply unbound). Sequences are read whole
  (`t l`, with the usual continuation hint), escape cancels, and the answer
  lands in the status bar and the message log (spec-gui "Help").
- **Watched-or-not is visible where files are.** "Is this file watched?" used
  to mean re-implementing the tracking algorithm in one's head and still
  missing the cases eligibility cannot see — a watch budget exclusion, an
  unplugged volume, the daemon's own runtime directory, a starved kernel
  budget. `POST /repos/:repo/watch/check` now answers for a batch of
  repo-root-relative paths against the *live* watch set, with the reason and
  both eligibility dry runs (the path's and its covering directory's)
  (spec-file-tracking "Watch check"). Built on it:
  - *`mf watch check <path>…`* — one line per path (watched, or not watched
    and why), exit code 1 when any path is not watched, so a script can assert
    on watchability;
  - the **metarecord-list** marks rows and cards whose tracked file is not
    watched in amber (theme variable `--mf-warning`), with the reason on
    hover — watched rows stay unmarked, their reason on hover too;
  - the **metarecord-detail** states the verdict in a note under the
    metarecord head (dim when watched, amber with the reason when not), and
    the *Watch and reconcile* button now follows the fetched answer instead of
    the record's raw `mf_watch` field — a record inheriting
    `mf_watch = true` no longer reads as unwatched.

### Changed
- **`metarecord:field remove` names a value, not a row — and a seeded ref by
  its path.** `m d` used to ask "Which value to remove?" over row labels of the
  form `tag = <uuid>`: deleting a tag meant knowing (or reading) its uuid — the
  one thing the completion seed exists to spare — and the label could not even
  be typed inline. It now asks the field, then the value, exactly like `add`
  (and like `mf metarecord field remove`): the value is parsed as one being
  added, so a `ref` with a completion seed is named by its path in the seed
  forest (a 32-hex uuid still wins), and *every row equal to it* is deleted.
  The candidates offered are the record's own values *read back* in the same
  raw vocabulary — a seeded ref as its path, ∅ for an explicit `Nothing` — and
  so is the pre-fill of an `edit`, which used to show the uuid too. A row's
  delete button and `metarecord:row-delete` still delete one specific row
  (spec-gui "Ref value completion").
- **`metarecord:field edit` asks the field first, and names the row to change
  readably.** `m v` picked its row over the same uuid labels; it now asks the
  field, then — only when it holds several values — which one, named by its
  readable raw form (∅ an explicit absence), then the new value, pre-filled
  with the one being edited as it reads. The replacement is read as the row's
  own type, so a seeded ref is edited by path on both sides; a `Nothing` row
  takes its type from the type argument, as before. With a single value the
  row is named by the field alone, so a whole edit is finally spellable inline
  (spec-gui "Ref value completion").
- **The checked selection is gathered wherever the user is, and kept.** The
  multi-selection (`selected_metarecords` — the `selection` target of
  `metarecord:bulk` and of the bulk form, what `mf gui selected` prints) used
  to be pruned to whatever the current query matched, so checking rows in one
  list threw away every check made in the previous one. It now survives
  changing the query or the finder: a selection can be gathered across several
  lists before anything acts on it. The **file manager** checks its rows into
  the same workspace-wide set — `Space`, `file-manager:select
  <toggle|all|none>`, and a row's right-click menu — only rows that have a
  metarecord; an untracked entry says so instead of silently doing nothing.
  `select all` checks a list's rows *on top of* what is already checked ("all
  of this list", never a replacement); `select none` (Ctrl+Space) still empties
  it. The one thing dropped from the selection is a metarecord that no longer
  exists (deleted or trashed from anywhere, the CLI included) — it could never
  be shown or unchecked again (spec-gui "The checked selection").
- **`user:tag-query` runs the search instead of parking the caret in the query
  zone.** The shipped example command spliced `#=<tag>` into the simplified
  query and stopped there: the focus sat in the zone, and the search still took
  an Enter. It now composes `metarecord-list:insert simplified "#=<tag>" stay`
  and `metarecord-list:apply simplified` — the filter lands, the search runs,
  and the focus is left wherever it was (the quoted text keeps a tag path that
  holds spaces one argument). The `stay` modifier is new on `insert`: the one
  `apply` already takes, with the same meaning — *the focus stays where it
  is*. Since it trails the free text, a trailing token that is not exactly
  `stay` is still text; a literal trailing "stay" takes double quotes (spec-gui
  "Command names (non-exhaustive default set)", "User commands").

### Fixed
- **A rollback left the watch set behind, and a nested tracking scope was never
  watched at all.** Two halves of one symptom — `mf watch check` answering
  `unwatched` for files that are tracked. History navigation restores
  `mf_watch`/`mf_ignore` rows like any other write but never re-placed the
  watches, so a rollback that brought the tracking back watched nothing until
  some unrelated write touched those fields (and one that removed tracking kept
  watching); the atomic navigation, the last step of a coordinated one and
  `rollback/abort` now recompute the watch set against the state HEAD landed on
  (spec-event-log "Upkeep after a navigation"). And the watch placement walked
  away when the *root* metarecord was opt-out, so `mf_watch = true` on a
  subdirectory — a tracking scope of its own — got no watch at all, while
  reconcile happily tracked its contents; the walk now judges each entry on its
  own, as spec-file-tracking "File Watcher" always said it did.
- **The repository root metarecord could be deleted.** One deletion and nothing
  in the repository resolved any more: every lookup through the `mfr_path`
  forest failed with "filesystem root entry missing", `track` could not re-create
  the root (it is not a creatable position), and re-creating it at a fresh uuid
  would have orphaned the whole forest, which keeps naming the deleted one.
  Every deletion path now refuses it with a `400` and nothing written —
  `DELETE …/metarecords/:uuid`, `POST …/query/delete` (atomically, whatever else
  the match set holds) and `POST …/metarecords/trash`, so `mf trash -f` on the
  root directory errors out before the bytes move. A root deleted before this is
  recovered by a rollback or a revert of the deletion, which put it back at its
  own uuid (spec-data-model "Referential integrity of a forest"). API_VERSION
  13 → 14.
- **`metarecord:field add` prompted with the wrong name.** "Value to add to
  …?" showed the field's *type* (or `undefined`, when no type argument was
  asked) instead of the field name — an argument index copied from the bulk
  form, whose arguments sit one slot further along.
- **`workspace-info` listed a `selected_entries` variable that never existed.**
  The standard one is `selected_metarecords` — the checked multi-selection —
  which the panel now shows in the standard group instead of among the custom
  keys.
- **A command that takes arguments waits for them in the minibuffer.**
  `panel:set` typed bare used to fail with `unknown setting: ""` instead of
  offering its choices; it now asks which setting, then which panel type —
  completing over the installed panel types — and `panel:set type` supplies
  the first and asks only the second. The shell builtins declare their
  arguments like panel commands already did, so an incomplete invocation is
  collected one argument at a time (`panel:reveal`, `panel:toggle`,
  `panel:focus`, `editing:goto`, `command-input:focus`, `workspace:goto`,
  `workspace:rename`, `mf:duplicate`, `daemon:set`, `answer:send`) rather than
  erroring or silently doing nothing (spec-gui "Command"). A trailing
  argument marked optional (`workspace:next slot`) is never asked for — its
  absence is the documented default — and an argument supplied inline is never
  re-asked.
- **`daemon:set url …` reached no handler at all**: the dispatcher read
  `daemon:set` as the name and `url` as its first argument, while the handler
  matched on the literal `"daemon:set url"`. The setting is an argument, like
  `panel:set`'s, so `daemon:set url <url>` runs and a bare `daemon:set` asks
  for both. `workspace:goto` now also says when its argument is not a
  workspace number, instead of doing nothing.
- **Panels had no `metafolder.invoke`.** The top-level alias of
  `commands.invoke` — "one alias, not two APIs", and what every documented
  composition is written with — was installed on the user-command API only, so
  a panel composing commands had to know the `commands.` path. `createPanelApi`
  installs the alias too, and `Metafolder.Api` now declares it: that
  declaration is also what lets the shipped `commands.js` join the typechecked
  GUI JavaScript (`checkJs`, no exceptions) instead of floating above it.

### Performance
- **Reading the log back is bounded again.** A listing of the most recent
  operations read every row of the `revision` table to build its timestamp map,
  so the cost of showing fifty operations grew with the whole log: 330 ms on a
  repository with 200 000 revisions, where the window itself is 12 KB. It now
  reads only the revisions of the operations it returns (15 ms on the same log).
- **`mf log list` asks for what it displays.** It fetched the *entire* log and
  trimmed it to twenty revisions locally — seven seconds of daemon time and two
  minutes of formatting on that same repository. `GET /log` gained a
  `revisions=N` parameter (whole revisions, most recent first), which is what
  the command now sends: 14 ms.
- A bounded log read that also filters (`metarecord_uuid`, `since`, `until`)
  now widens its walk until it has what was asked for: asking for the last ten
  operations of one metarecord answers with them instead of with "none of the
  repository's last ten operations are yours".

### Added
- **Performance-regression suite** (`docs/spec-perf.org`), in two layers:
  - *cost assertions* (`crates/daemon/tests/perf_cost.rs`), which count SQL
    statements and read query plans instead of a clock — they run in the
    ordinary `cargo test` pass and cannot flake under load;
  - *timed benchmarks* (`scripts/bench.sh`, `metafolder-bench regression`) over
    generated repositories, with a per-machine history kept in
    `benchmarks/history/<machine>.jsonl` and every run compared against the
    median of the last five.

### Changed
- **`scripts/prune-target.sh` prunes superseded `incremental/` caches too.**
  Cargo keeps one incremental-compilation cache directory per crate generation
  under `<profile>/incremental/`, keyed by a hash encoding that matches
  nothing in `deps/`, and never removes what an older generation left behind —
  on a rebuilt tree this grows to be the bulk of `target/`. A third pass now
  counts instead of matching: a crate name keeps at most one cache dir per
  surviving `deps/` generation (the most recently used) and loses its whole
  cache when nothing in `deps/` survives. The cap fires when the name's
  generation set is known to have changed — a gain since the previous run, or
  a first run with no recorded state — so a name that merely lost a
  generation keeps its cache until its next build, and `--dry-run` shows that
  truth. Deleting a cache dir costs a recompile, never correctness. To adopt
  the pass on a `target/` that predates it, delete `target/.prune-target-state`
  once: the next run re-baselines and reclaims the superseded caches.
- **Bulk commands name their target instead of inferring it.** The GUI's bulk
  writes inferred what they would act on from whether anything happened to be
  checked — a checkbox selection when non-empty, else the whole query — so a
  bulk edit meant for the checked rows could silently land on the query (or
  the reverse), and the list panel's bulk form disagreed with the command by
  always acting on the query. Both entry points now ask: `metarecord:bulk`
  takes the target as its first argument (`selection` or `query`, asked with
  completion, pre-filled with what a checked selection means and naming what
  each choice would act on), an empty selection is "nothing to do" rather
  than a detour onto the query, and an old positional invocation fails loudly
  with the new syntax in the error; the list panel's form gained a target
  drop-down (default: the query it has always shown). `m m <op>` pre-fills
  target and operation (selection-scoped), `m b` asks both, and the shipped
  scripts ask the same question before reading the scope — with `mf gui
  query` now printing only the list's query and the checked UUIDs split into
  their own `mf gui selected` (one per line, nothing checked = no output).

## [0.3.0] — 2026-08-16

First tagged release. Summarises the capability set built since the initial
proof of concept.

### Data model & queries
- Universal **metarecord** model: a UUID plus a multi-map of `(name, value)`
  fields over ten value types (`nothing`, `string`, `int`, `float`, `bool`,
  `datetime`, `ref`, `tree_ref`, `refbase`, `external_ref`), with three-valued
  logic (present / explicitly absent / unknown).
- **Query DSL** and JSON IR: boolean combinators, three-valued predicates,
  comparisons, regex `matches`, ordered-substring matching, `uuid_in`, and
  reference traversal (`->`, `->*`) over the `tree_ref` forest.
- **Simplified query language**: a user-editable grammar that expands
  client-side into the normal DSL (shared by the CLI and GUI).
- Reserved fields: daemon-owned `mfr_*` (require `force` to override) and the
  `mf_*` controls (`mf_watch`, `mf_ignore`, `mf_schema`, `mf_sync`).
- Optional per-repository **user schema** with strict write validation and
  read-side violation reporting.

### Daemon
- Axum + Tokio HTTP server managing one or more repositories over a REST API,
  with a resource layer (single addressed thing) and a set layer
  (`POST …/query/*`).
- **SQLite** EAV storage (WAL, exclusive lock) with a full **event log**:
  every write goes through one `Writer`, one revision per write, with atomic
  metadata-only rollback, coordinated (watcher-suspended) navigation for file
  moves, history reading and pruning.
- **Filesystem watcher** (inotify) with a batched, compacted pending-event
  pipeline, and **reconcile** with fingerprint-based move detection.
- In-memory **tree cache** (path↔UUID, O(1) rename/move) and a bitmap/BSI
  **query accelerator**, both warmed in the background at repo load as an
  observable task.
- FTS5 trigram pre-filter for regex `matches`; embedded media metadata
  extraction into `mfr_meta_*`; MIME detection.
- **Session-token authentication** (spec-auth): every daemon and GUI request
  is gated by a per-service token in a user-only runtime file, keeping browser
  content out.

### CLI (`mf`)
- `repo`, `metarecord` (query/id/simplified selectors, `field` verbs),
  `field`, `retype`, `reconcile`/`track`/`path`, `log` (list/show/rollback/
  prune), `task`, `schema`, and `trash`.
- `tag` — hierarchical tags with subsumption and exclusivity; `order` — number
  a folder's children; `sync` — cross-repository synchronisation
  (plan/run/show/status/link/unlink).
- `gui` — drive a running GUI through its scripting API.

### GUI (`metafolder-gui`)
- Tauri v2 + Svelte 5 desktop app: workspaces (tabs), two panel slots, a
  keybinding system, a command input with autocomplete and interactive
  argument collection, input history, and a local `/gui/*` scripting API.
- Built-in panel types (plain HTML/JS in Shadow DOM roots): repos,
  metarecord-list, metarecord-detail, file (with sandboxed media preview),
  file-manager, treeref, ref-list, sync, trash, recent, log, message, help.
- A shared in-realm daemon-data cache with change-feed invalidation; shared
  file operations (cut/copy/paste/rename/duplicate/trash) in every panel's
  context menu; a recently-viewed picker.
- Every untrusted-media decoder runs sandboxed (`bubblewrap` + rlimits); the
  WebView web process is sandboxed and the GUI refuses to start otherwise.

### Configuration & tooling
- Single git-backed user configuration repo at `~/.config/metafolder/`,
  applied by `metafolder-sync-config` (the only git actor); no runtime fallback
  to embedded defaults.
- `Makefile` + `scripts/check-deps.sh` for build/install with dependency
  checks; `scripts/check.sh` static-analysis pass; `scripts/complete-build.sh`
  and `scripts/prune-target.sh` for a small `target/`.

### Fixed (v0.3 hardening pass)
- Documentation ↔ code drift corrected across the README, specs and roadmap
  (authentication documented and shown in examples, `follows_transitive` body
  key, `bubblewrap` promoted to a required GUI dependency, full CLI surface).
- Removed panic-prone `unwrap`/`expect` on live daemon query/reconcile paths.
- GUI panel errors are now styled as errors (were rendered as info) with
  consistent timeouts; `external_ref` renders as `repo :: metarecord` instead
  of `[object Object]`.
- De-duplicated shared helpers (index acquisition, typed field readers, hex
  encoding, file-action helpers) and cleared the standing frontend-lint errors.

### Performance
- `executor::compact` is now O(n log n) (was O(n²)), verified byte-for-byte
  against the previous implementation by a fuzz test.
