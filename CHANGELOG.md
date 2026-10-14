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
- **Generic panel commands: one `cursor:next` for every list panel.** A panel
  hands the shell a few functions (`metafolder.provide({index, count, select,
  more, entries, enter})`) and `cursor:next`, `cursor:prev`, `cursor:first`,
  `cursor:last` and `cursor:find` run on them. Their first argument, required,
  is the panel: `self` (the focused one) or a panel type — the file panel's
  arrows run `cursor:next metarecord-list`. A key bound to one fires only in a
  panel providing what it needs, and passes through elsewhere, so `j` is one
  binding instead of eight. The detail panel gains `Home`/`End`. Each panel's
  generated note lists what it provides (doc "Generic panel commands").
- **`view:refresh <panel>`**: the same for reading a panel again — one `r`
  for the nine panels that had their own `<panel>:refresh` (the file panel
  keeps `r` for `file:restart`).
- **`cursor:parent <panel>` and `cursor:root <panel>`**: up one level and to
  the top — `Backspace`/`←` and `/` — for the file manager, the tree explorer
  and (up only) the log. The file manager gains `←`.
- **Walking down a path by name in the file manager and the tree explorer.**
  `d` runs `cursor:find self walk`: a directory (a node with children) named
  is entered and the question asked again from inside, a file (a leaf) named
  is selected. `shift+Enter` stops on a directory, selected — the new
  *alternate* answer of an interactive argument (`ArgSpec.alternate`), which
  rewrites the arguments answered before it, so the step runs, and is echoed,
  as `cursor:find self select`, what `f` runs (doc "Jumping to an entry").
- **Contexts: what one piece of work writes is one label.** `mf label begin
  NAME [DESCRIPTION]` opens a label and prints it (`<repo>:<id>`); exported as
  `METAFOLDER_LABEL`, every `mf` started meanwhile writes inside it, and `mf
  label show` reads it. Every command writing anything is a label of its own,
  named after its command line and described by its query — opened at its
  first write (`X-Metafolder-Command`), so a command that only reads opens
  none. In the GUI each command is a label, and a script it runs gets a
  context. The tagging scripts write each run, and each answer, in a label;
  going back reverts the answer's (`mf log undo --after` is no longer what it
  uses). Reverting a label that holds nothing reverts nothing instead of a
  `404` (doc "Labels and contexts").
- **The slow log names every pass of a reconcile.** `placed`, `refresh`,
  `mime` and `metadata` join `resolve` and `path_of`: an idle reconcile's keys
  are all attributed now (≈ 22 a path — 3 resolving it, 13 comparing its stat
  fields, 3 and 2 checking its MIME type and metadata), and a test holds that
  cost per path as the repository grows (doc "What the slow log records").
- **`mfr_mime_undetected`: a file reconcile found no MIME type in is not read
  again.** The MIME pass opened, at every reconcile, every regular file
  `infer` recognises nothing in (plain text, for one), since nothing said it
  had been tried. Such a file now gets `mfr_mime_undetected = true`; clear it to
  ask again. A file whose type was found carries no marker, so clearing
  `mfr_mime` still recomputes it. A symlink no longer receives its target's
  type at creation, as the MIME pass never gave it one (doc "File metadata
  fields").
- **The background tasks are in the slow log.** A reconcile, a duplicate scan,
  a relink and a load's warm-up are logged as `task <kind>` (the task's id in
  the context), their progress phases as the entry's phases, with `wait:conn`
  and `commit` like a request's — so a reader that waited on `wait:conn` can be
  matched with what held the repository, and how long each step of it took
  (doc "What the slow log records").
- **`POST /repos/:repo/metarecords` creates a path's metarecord** with
  `{"path", "fields", "parents"?}`: its `mfr_*` fields read from disk, the
  caller's fields beside them, and `mf_watch = false` unless the fields set
  `mf_watch` — in one revision, schema-checked. A path that already has its
  metarecord is a `409` naming it (`uuid`), and so is a parent directory
  without one (`missing_parents`, root first) unless `parents` asks for them;
  parents created that way carry no `mf_watch` of their own (doc "Recording a
  path").
- **Second mounts: a drive mounted twice in a repository is tracked once.** A
  mount point under a root showing what the repository reaches already — the
  same drive mounted again, a directory of it, or a directory of the root's own
  filesystem bound into it — is an entry, never walked: the reconcile, the
  watch placement, the event filter and the orphan scan leave it alone, and a
  change made through it is recorded where the drive was mounted first (the
  watcher could only ever report one of the two paths). `POST /watch/check`,
  `mf watch check` and the GUI say why with the reason `second_mount` (doc
  "Second mounts").
- **`in`: "this field is one of these values" as one query node** — JSON
  `{"type": "in", "field": …, "values": [...], "aspect"?}`, DSL
  `in(field[:aspect], v1, v2, …)`. It means exactly the `or` of one `eq` per
  value, aspect included, but counts as one node against the size caps (2000
  nodes, 200 operands per `and`/`or`) where that `or` counted N + 1 — so a long
  membership list is no longer refused (doc "Query IR", doc "Query limits").
- **`metafolder-watchd`: the privileged fanotify broker** (`crates/watchd`,
  `scripts/metafolder-watchd.service`) — groundwork for the fanotify watch
  source (docs/watcher-fanotify.md "The broker"). One process per machine holds
  the fanotify group covering the filesystems of subscribed repository roots
  (one mark per filesystem, those mounted beneath a root included and followed
  as drives come and go; no per-directory watches), resolves the kernel's file
  handles to paths, and streams events as NDJSON over a Unix socket — each
  subscriber seeing only what its own uid could discover — names that are not
  UTF-8 included, carried byte for byte. A subscriber that
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
- **"Revision" is gone from the vocabulary.** What one request writes is a
  *write*; what groups writes is a *label*. The watcher's flush report says
  `-> 2 writes`, the notes and messages say the same, and the notes "Reverting
  a revision" and "Revision commands in the GUI" are now "Reverting a label"
  and "Log commands in the GUI". The redesign of the event log is complete
  (doc "The operation log"); left for later by decision: the GUI's work
  context and "revert everything since a time".
- **The log panel is a tree of labels.** A row per unit of work — a command, a
  script's run, a context — newest first, opening onto the labels nested in it
  down to single operations; consecutive flushes of the watcher fold into one
  row. Badges (`watcher`, `undo of #N`, `undone by #N`, `partly undone`) are
  computed over everything under a row, and `log:revert` reverts a label whole
  or one operation (`log:revert label` widens from an operation). The
  `selected_log_entry` workspace variable is `{repo, label_id, op_id}` (doc
  "log panel").
- **Undo and redo by label.** Undo offers the labels your newest change still
  standing was written in, innermost first — the command, the script's answer,
  its run —, and undoing one takes back its changes still standing, its own
  undos and the watcher's operations left alone. `mf log undo` lists them and
  `mf log undo <label>` undoes one; in the GUI a change in one label is undone
  at once, one in several asks which. Redo is the same over the newest undo.
  A revert target takes `only: "changes" | "undos"` (doc "Undo and redo by
  label"). Removed: `mf log undo --after`, which the scripts no longer use.
  Layout 6 (the reverts of an operation indexed) empties the old log.
  `API_VERSION` 20: a daemon that ignored `only` would revert a whole label.
- **Labels replace revisions.** Each operation carries its innermost label
  (name, description, parent), when it was written and its origin. A write
  request names the label it writes in with `X-Metafolder-Label: <id>`;
  without one it opens a label of its own, named after the route (a revert:
  `revert of label N`). `POST /repos/:repo/labels` opens one, `GET
  /repos/:repo/labels/:id` reads it with its ancestors and children. The
  watcher writes one label per flush under its own, which no client may write
  under. `GET /log` answers `label_id`, `timestamp` and `origin` per
  operation and a `labels` list (ancestors included), and takes `labels=N`;
  `/log/revisions/:id` becomes `/log/labels/:id`; a revert targets `label_id`
  (its children included) and answers `label`; a prune targets a label id and
  keeps it whole. `mf log` lists, shows, reverts and prunes by label. Layout 5;
  the old log is emptied like the layouts before it. `API_VERSION` 19 (doc
  "Labels and contexts", doc "Log endpoints").
- **The log is kept by operations per origin.** `log-retention-watcher-ops`
  (default 50 000) and `log-retention-user-ops` (default 1 000 000) replace
  `log-retention-revisions` and `log-retention-keep-labels`, daemon-wide and
  per repository (`log_retention_watcher_ops`, `log_retention_user_ops` in
  `config.json`): the watcher's churn no longer pushes your own history out of
  a log that kept 200 revisions of both. The old keys are read, ignored, and
  named in a warning at startup. The store's layout 4 splits the operations by
  origin; like layout 3 it empties the log of an older store instead of
  converting it (doc "Automatic retention").
- **The event log is a sequence, not a tree: undo and redo are reverts only.**
  An operation names no parent; HEAD is the newest operation and only moves
  forward. Undo reverts your newest change still standing, redo the newest undo
  still standing — so a redo survives the watcher writing between it and its
  undo (it was lost: the undo had been a rollback, and the watcher's write
  started a branch), and two redos in a row both work (the second answered
  "Nothing to redo."). `mf log undo --after <op>` undoes back to a point; the
  tagging scripts' going back uses it instead of a rollback that unrecorded
  the watcher's writes meanwhile. A revert recreating a folder's file before
  the folder checks the forest at commit. `GET /log` gains `first` (where undo
  stops) and loses `mode` and `parent_id`. `API_VERSION` 18 (doc "Why the log
  stops being a tree", doc "The undo selection").
- **A revert is applied onto the current state; nothing blocks it any more.**
  It goes back to what each operation replaced, whatever was written on the
  same field since — the plan names that later write (`changed_since`) and
  the clients confirm before overwriting it. The dependency check,
  `with_dependents` (`--with-dependents`, `log:revert with-dependents`) and the
  plan's `blocked`/`dependents`/`revertable` are gone. An append is undone by
  value, so undo, redo, undo finds the value again; undoing the creation of a
  metarecord already gone writes nothing; recreating one whose uuid exists
  again is a `409`. `API_VERSION` 17 (doc "Reverting onto the current
  state"; first step of doc "The operation log").
- **The file manager's ''Add to DB'' is `file-manager:record`** (was
  `file-manager:track`, "Track" in the context menu): it creates the entry's
  metarecord through `POST /metarecords`, names the parent directories that have
  none in a confirmation and creates them only if it is accepted, and refuses an
  entry already recorded.
- **`mf track` is gone: `mf metarecord add --path <path> [--parents]`** creates a
  path's metarecord, with any field specs beside it
  (`mf metarecord add --path ./x.flac rating:int=5`). It refuses a path that
  already has one (`mf track` quietly answered the existing uuid) and parent
  directories without one unless `--parents` (`mf track` created them with
  `mf_watch = false`, cutting their subtree out of a watched area). The new
  `mf uuid <path>…` is the lookup, `mf path`'s inverse. `mf order` refuses a
  folder with no metarecord instead of creating it; the shipped scripts that
  recorded a path on the fly look it up first (doc "Recording a path").
- **GUI: commands open a panel type `here`, in the `other` slot or in a `new`
  workspace** (doc "Panel slots and layout"). `panel:open <target> <type>`
  replaces `panel:set type <type>` (now `panel:open here <type>`) and
  `panel:reveal <type>` (now `panel:open other <type>`): a keybinding or a
  `commands.js` entry still naming the old commands must be renamed. The `t`
  sequences open here; the same sequence after `w` opens in the other slot,
  after `b` in a new workspace — a copy of this one (`workspace:fork`), which
  shows the same thing because the panels now follow their variables (the
  list's query, the file manager's `file-manager:dir`/`:cursor`; the
  `{…, nonce}` requests are gone). Commands run in an execution context
  (`mf.withContext`, `mf.atTarget`), and `workspace:send` /
  `mf.workspace.send` is the one channel between workspaces.
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

### Removed
- **The per-panel cursor commands** — `<panel>:next`, `:prev`, `:first`,
  `:last` and `:find` of the metarecord list, file manager, tree explorer, log,
  trash, recent list and duplicates, and `metarecord:row-next`/`row-prev` —
  replaced by the generic `cursor:*` commands (see Added); a key of yours bound
  to one of them is reported as naming no command. The `/__find-entry.js`
  helper (`registerFind`) went with them.
- **The per-panel `<panel>:refresh` commands**, replaced by `view:refresh`.
- **`file-manager:parent`/`:root`, `treeref:parent`/`:root` and
  `log:parent`**, replaced by `cursor:parent` and `cursor:root`.
- **Labelling a revision after the fact**: `PATCH /log/revisions/:id`, the log
  panel's `log:mark` (Shift+L) and `mf log revert --label`. Renaming or
  describing a label once written is deferred; a context names the work while
  it is done (doc "Labels and contexts").
- **Rollback and the history tree.** `mf log rollback`, `log:rollback`,
  `POST /rollback` and its plan, summary and coordinated `start`/`step`/`abort`,
  the restorations a skipped step left, the `rollback` task, `mf log list
  --tree`/`--graph` and the log panel's graph, `mf log prune linearize`. A
  repository opened by this version has its log **emptied, not converted**
  (layout 3; the metarecords are kept) — an exception to the conversion rule,
  decided by the user. The trash's `rollback` reason is now `revert` (old
  entries read as such).
- **`POST /repos/:repo/track`** — `POST /repos/:repo/metarecords` with a `path`
  replaces it, refusing what it accepted (an already recorded path, missing
  parents without `parents`). `API_VERSION` 16. The bench's `api.track`
  scenario is `api.metarecord.create_at_path` (doc "Recording a path").
- **The SQLite backend** (**breaking**, `API_VERSION` 15). The key-value store
  (LMDB) is the only store a repository has: `mf repo convert`, `POST
  /repos/:repo/convert`, `mf repo init --storage`, `METAFOLDER_DEFAULT_STORAGE`
  and the `storage` field of `GET /repos`, check, backup, restore and reindex
  are gone, and so are the resident query index and the resident tree cache
  that served SQLite repositories. **No conversion ships with this version**:
  a repository still on SQLite (a `config.json` without `storage`, or with
  `"sqlite"`) is refused at load with a message saying to convert it with an
  earlier version (`mf repo convert --to kv`) — a deliberate exception to the
  on-disk policy above, taken when no real repository was on SQLite. A SQLite
  backup is refused by `mf repo restore` likewise. The test oracle is now a
  naive evaluator reading every row through the storage traits. The per-pair
  sync database (`internal/sync-<a>-<b>/`) and the trash index
  (`internal/trash/index/`) are key-value stores too; `rusqlite` has left the
  workspace. No `sync-*.sqlite` existed and no trash index held an entry, so
  neither is converted: an old `trash/index.sqlite` is ignored, and swept by
  `mf trash prune --all`.
- **Repositories on network filesystems.** A `.metafolder/` on NFS, SMB, SSHFS
  or another network filesystem is refused at init and at load (LMDB's memory
  map and locks are not safe there); the files may stay on the share, with the
  `.metafolder/` on a local disk (an external one). Cross-machine sync by
  mounting the remote repository over SSHFS is deferred (spec-sync).

### Fixed
- **A reconcile resolved every path four times.** Its refresh, MIME and
  metadata passes each resolved again the paths its index pass had just
  resolved, and its orphan scan read back the path of every file the walk had
  just found on disk. Each path is resolved once now, and a file the walk
  found is not checked again.
- **Reading the log right after an undo read the whole log.** With a
  continuation below HEAD, `GET /log?mode=active` rebuilt it from every
  operation of the table and HEAD's whole ancestry (351 000 keys for the
  redo's window, on a session's log); it now reads HEAD's subtree alone (doc
  "Reading the log costs the window").
- **The duplicate scan spent half its time committing.** Its hash cache went
  out in batches of 500 *fields* (167 files) and its groups one revision per
  size class: 503 revisions, 17 of 31 s, on 50 000 files. All its writes now
  go into one revision committed every two seconds (doc "The duplicate scan").
- **Resolving a path read every sibling of each of its components.** A name
  that is text was compared with the text of all the entries of its folder
  instead of being looked up by its bytes, which the note on case sensitivity
  had always said comes first. A reconcile resolves every file it finds on disk
  (four times with its optional passes): on 50 000 files, one with nothing to do
  read 242 million keys and took 43 s.
- **`mf metarecord -i` takes a UUID and nothing else.** Text that was not one
  parsed as DSL further down: `-i '<query>' get` ran the query, `field set` and
  `delete` wrote to its matches, `get --tsv` panicked. It is a usage error now.
- **`metafolder-watchd`: a drive mounted under two repositories was heard by one
  only.** The broker resolves a file through one mount point of its
  filesystem; the repository under another mount point of the same drive got
  nothing. Each event is now told again at every mount showing its path — the
  whole drive, or the part a bind mount shows.
- **A drive plugged in under a repository was never watched (inotify).** An
  inotify watch is on an inode: the one placed on the mount point stayed on the
  directory hidden beneath, and nothing placed the watches again, so nothing
  done on the drive was recorded until the daemon restarted or a watch rule
  changed — plugged in for the first time or plugged back. The daemon now
  follows the kernel's mount table: a mount point coming or going under a root
  drops what was watched there and places the watch set again (and the mount
  table snapshot is read afresh).
- **`metafolder-watchd`: a drive's files reported at a mount point it had
  left.** The broker remembers each resolved handle's path for two seconds; a
  drive mounted at two points of a repository and unmounted from the first —
  or moved from one repository to another — had its changes reported at the
  old mount point meanwhile, to the wrong repository or at a path holding
  something else. Every remembered path and verdict is now forgotten whenever
  the marks are placed again (a subscription or the mount table changing).
- **`metafolder-watchd`: a drive leaving a repository blinded the repository.**
  When a filesystem mounted under a root was unmounted there (unplugged, or
  still mounted elsewhere), the broker lifted its mark by the old mount point —
  by then a directory of the root's own filesystem — and so lifted the root's
  mark instead: nothing more was reported for that repository until the broker
  restarted, and a drive still mounted elsewhere stayed marked. A mark is now
  lifted through a descriptor checked to be on its filesystem, found at its
  path or at a mount point of the same device (doc "The fanotify broker").
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
- **A coordinated rollback plans its path once.** Each `POST /rollback/step`
  recomputed the whole path to its target to apply one operation, so a
  navigation of N operations read N²/2: one step read 173 keys 80 operations
  away against 13 now, whatever the distance.
- **The log trim reads what it cuts, not what it keeps.** The retention trim
  (once every tenth of the retention's writes) read every operation of the
  log; behind a kept reconcile of 2 000 operations it read 9 023 keys to cut
  100 one-operation revisions, now 1 027 either way.
- **Generating the benchmark repositories no longer syncs every revision**
  (`KvStore::open_unsynced`, also used by the test stores).
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
