# A fanotify watch source behind a privileged broker

**Status: implemented (September 2026)** — the broker (`crates/watchd`) and the
daemon's fanotify source (`daemon/src/watcher/fanotify.rs`) are in the tree.
What is still missing is a run against a real privileged broker on a real
machine: the tests cover the kernel side in a user namespace (marks, events,
records) and the resolution side with a stand-in, but never both at once (see
"Tests"). This note records why a second watch source is wanted, its exact
shape, and the permission model it must obey. spec-file-tracking "File
Watcher", "Watch sources and regimes" and "The watch budget" are the reference.

## What is wrong today

The watcher places **one inotify watch per eligible directory**
(`daemon/src/watcher/inotify.rs`), which costs in three places:

- *Placement.* Every load and every eligibility change re-walks the tree to
  compute the watch set (`compute_watched_dirs_timed`); the code itself logs any
  walk over 100 ms. On a large repository this is felt.
- *The budget.* A tree of N directories costs N kernel watches out of
  `fs.inotify.max_user_watches` — a **per-user** limit shared with every other
  program on the machine. The daemon caps itself to a share (`watch_budget_share`)
  and records what it cannot afford as `mfr_watch_exceeded`: the frontier
  machinery of spec-file-tracking "The watch budget". A personal repository
  sitting close to the limit is the motivating case for this note.
- *Maintenance.* Appearances and departures are tracked by hand
  (`maintain_watches` / `forget_subtree`), and one notify watcher per repository
  is one inotify *instance* per repository against
  `fs.inotify.max_user_instances`.

## The decision

**Two watch sources, one pipeline.** The source produces events; everything
downstream — the persistent buffer, compaction, rename correlation, `scan_dir`,
the executor, and the per-event eligibility check already present there — is
shared and unchanged.

- **The current source stays**, no privileges required (notify's
  `RecommendedWatcher`: inotify on Linux, native backends elsewhere). It remains
  the default and the fallback.
- **A fanotify source** is added on Linux, fronted by a small **privileged
  broker**. One mark covers a whole filesystem: no per-directory watch, no
  budget, no placement walk.

Never both at once for a given repository: one source is chosen at load time —
fanotify when the broker is reachable and permitted, inotify otherwise.

## Why a broker, and what the kernel actually allows

Verified against man-pages 6.19 and the kernel documentation:

- **Unprivileged fanotify is not enough, structurally.** Since 5.13,
  `fanotify_init()` works without `CAP_SYS_ADMIN`, but such a group may mark
  **inodes only** — and directory monitoring "is not recursive … This approach is
  racy … Monitoring mounts offers the capability to monitor a whole directory
  tree in a race-free manner" (fanotify(7)). `FAN_MARK_MOUNT` /
  `FAN_MARK_FILESYSTEM` need `CAP_SYS_ADMIN`. There is no useful half-privileged
  middle ground.
- **It must be a filesystem mark, not a mount mark.** The kernel refuses every
  entry event — `FAN_CREATE`, `FAN_DELETE`, `FAN_MOVE*`, `FAN_RENAME`,
  `FAN_ATTRIB`, `FAN_DELETE_SELF` — on a `FAN_MARK_MOUNT` (EINVAL; only the
  data events are allowed there), and accepts them on `FAN_MARK_FILESYSTEM`.
  Verified on 7.2 (September 2026), in a user namespace marking a tmpfs it
  mounted itself. The first broker used mount marks and could not have
  started. A filesystem mark reaches every mount of that filesystem — bind
  mounts, other mount namespaces (the systemd unit's own `ProtectHome=` one
  included) — so events outside the subscribed roots arrive too, and the
  per-subscriber filter drops them — after the broker's own parent-directory
  filter has dropped most of them unresolved (see "The broker").
- **Events carry file handles, not paths.** `FAN_REPORT_FID` events identify
  objects by handle; resolving a handle to a path is `open_by_handle_at`
  (`CAP_DAC_READ_SEARCH`) — or a maintained fid→path cache. The kernel also
  warns the object may no longer be where the handle points when it is resolved
  (`ESTALE`).
- **`FAN_RENAME` is a strict improvement over the inotify cookie.** Both sides
  (OLD_DFID_NAME + NEW_DFID_NAME) arrive in one event, and a move *into or out
  of* the repository is seen as a move rather than delete+create — which is what
  "metadata follows files" wants.
- **Versions.** `FAN_REPORT_FID` (5.1), `FAN_REPORT_DFID_NAME` (5.9),
  `FAN_REPORT_TARGET_FID` (5.17, also 5.15.154 / 5.10.220). Feature-detect at
  startup; the full form is what makes renames complete.
- **Limits on this side too.** `max_user_groups`, `max_user_marks`,
  `max_queued_events`; an overflow is `FAN_Q_OVERFLOW` and must be announced,
  never a silent gap. *Announced*, not acted on: the daemon suggests
  `mf reconcile`, it does not start one (decided September 2026: reconciling
  is purely manual — an overflow comes from load, and an automatic reconcile
  would add to it).
- **Caveats to design for.** No events for `mmap`/`msync`/`munmap` writes
  (`FAN_CLOSE_WRITE` plus the fingerprint reconcile are the backstop — relevant
  to hash-based identity); no remote events on network filesystems; some FUSE
  report a zero fsid; a mount mark sees one mount only (bind mounts); a
  filesystem without export file handles cannot use `FAN_REPORT_FID` at all.

## The broker

A small system service (e.g. `metafolder-watchd`), root or
`AmbientCapabilities=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH`, **one fanotify group for
the machine**:

- marks the filesystems holding subscribed repository roots — the root's own,
  and every one mounted beneath a root, following `/proc/self/mountinfo`
  (`POLLPRI`) as mounts come and go;
- resolves FID → path (it holds `CAP_DAC_READ_SEARCH`);
- filters per subscriber (next section) and streams events over a Unix socket.

No business logic in it: no eligibility, no compaction — those stay in the
daemon. The protocol is *subscribe to roots* + *a stream of (path, kind)*, with
explicit backpressure rules: a slow consumer makes the broker drop and signal
overflow, a disconnect or reconnect makes the daemon announce the gap — either
way a reconcile, suggested and run by the user, closes it (the posture of a
daemon that was down; see spec-file-tracking "Watch sources and regimes"). One group for all
repositories also removes the per-repository inotify instance limit.

**Landed (September 2026)** as `crates/watchd` (`metafolder-watchd`, unit
`scripts/metafolder-watchd.service`): `proto` (the NDJSON wire — `Subscribe`,
`Subscribed`, `Event`, `Overflow`, `Error`), `filter` (the per-uid DAC filter
over a `CredSource` seam), `fanotify` (the group, the filesystem marks, the
mount-table watch, a *pure* record parser, and `preflight` which fails at
startup naming the two capabilities), `server` (one bounded queue per
subscriber, in-order `Overflow` markers, `RootSink` telling the marks what to
cover).

Paths on the wire are byte strings (`proto::WirePath`): a plain JSON string
when the path is text, otherwise `{"text", "bytes"}` — the object form of a
tree name on the daemon's API (spec-data-model "Tree names"), `bytes` in
lowercase hex and authoritative. A file with a Latin-1 name is watched like
any other; the first version dropped its events.

Three things the first version got wrong, fixed since (each with its test):

- **Mount marks** — refused for every entry event (above). Now one filesystem
  mark per filesystem, nested mounts included, the mount table followed.
- **The reading thread held the group's lock while blocked in `read(2)`**, so
  on a quiet machine the first subscription could never place its marks and
  was never answered (its daemon fell back to inotify after the handshake
  timeout). The group is read through a `Reader` sharing only the descriptor.
- **One descriptor kept open per covered filesystem** (for
  `open_by_handle_at`) — which makes `umount` fail with EBUSY, so a drive under
  a repository could not be unplugged. Descriptors now live for one batch of
  events.

The socket is world-connectable, so what a peer can make the broker hold is
bounded (`server::Limits`): a message is at most 256 KiB (a line is buffered
whole before it is parsed — `lines()` would have buffered one without end), a
uid holds at most 256 connections and the machine 1024 (two threads each),
and a peer that hangs up is let go entirely — the first version left the
writer thread of every closed connection waiting for ever on its queue.

Events outside the roots are dropped before their paths are resolved: a
directory's "under a root?" verdict is resolved once per directory handle and
remembered (`Scope`, see "Open questions"), and the server tests the roots
before the permission filter stats anything.

48 tests. The kernel-side ones run for real: an unprivileged smoke test (group,
inode mark, record layout), and tests that re-run themselves under
`unshare -rm` — a user namespace may mark a tmpfs it mounted itself (Linux
6.8+) — covering a root with a filesystem mounted beneath it and one mounted
after the subscription. Plus an end-to-end over a real socket with
`SO_PEERCRED`.

## Permissions

Deployment model decided: **strictly single-user daemon** (spec-auth stays as it
is — bearer token per service, multi-user control explicitly out of scope). The
rule, at every hop:

> No hop reveals more than the next hop's own credentials could discover by
> walking the filesystem themselves.

- **broker → daemon** (the critical hop — the broker sees the whole mount): each
  subscriber is identified by `SO_PEERCRED` (its uid), and an event is sent only
  if that uid could itself discover it by walking down from its root: search
  every ancestor, and *list* every directory from the root down to the entry's
  parent (the first version asked it of the parent only, so the names of the
  subdirectories of a `--x` directory leaked). Evaluated *as the subscriber's
  identity* — its uid and its groups; the directories' modes are cached, the
  verdict is not (two processes of one uid need not hold the same groups). Subscribing to a root passes the
  same check — and so does *resolving* it (symlinks, `..`) to the name the
  kernel reports events under: `filter::AccessFilter::resolve` walks it one
  component at a time, looking an entry up only in a directory the subscriber
  may traverse. The first version called `canonicalize` with the broker's
  `CAP_DAC_READ_SEARCH`, which made `/secret/x/../../tmp` an oracle for the
  existence of `/secret/x` and read links inside private directories.
- **daemon → client**: under the single-user model the caller's rights are the
  daemon's own rights, so the invariant is simply "never report a path this
  daemon could not stat/read itself". A shared or root daemon would need real
  caller identity (Unix socket + `SO_PEERCRED`) — deliberately out of scope.

Test case: a repository containing a `0700` directory owned by another uid →
nothing about it anywhere. The inotify placement walk already skips it (it
cannot readdir it); the broker must filter it out the same way.

## What changes observably — settled in spec-file-tracking

The regime split is now specified in spec-file-tracking "Watch sources and
regimes"; this section records what was settled (it revises the first draft of
this note, which said the watch-budget vocabulary simply "has no meaning" under
fanotify):

- **Two regimes**, by how the kernel covers a tree: *budget* (per-directory
  watches — inotify) and *coverage* (one tree-wide registration — FSEvents,
  ReadDirectoryChangesW, fanotify). Everything regime-independent (eligibility
  per event, internal dir, offline subtrees, batching) is written as common.
- **`mfr_watch_exceeded` is honoured in *both* regimes** — its meaning is "leave
  this subtree uncovered", not "place no inotify watch here". Honouring it under
  fanotify is deliberate: `mf watch exceeded set` and `watch check`'s `excluded`
  then mean the same thing whatever the backend, which is the real "works on my
  machine" trap. Only its *automatic* writer (the budget's frontier) is
  budget-only.
- **`watch check` keeps one wire format and one ladder** across regimes; the
  only budget-only reason is `unwatched` (the starved state).
- **`GET /watch` reports the active `backend`**, and `watched_dirs` /
  `watch_budget` are `null` under coverage (no per-directory state, nothing to
  run out of).
- **`watched_dir`** is "the directory the answer was computed against" in both
  regimes: the covering directory whose watch (budget) or subtree coverage
  (coverage) reaches the path.

## The seam

`watcher.rs` splits into:

- `watcher/mod.rs` — translate + ingest + the channel to the executor (the
  shared pipeline), the `Source` facade, the watch-check ladder and the
  coverage-regime honour of `mfr_watch_exceeded` (`drop_excluded`, applied at
  ingest) — **landed (September 2026)**;
- `watcher/inotify.rs` — the placement walk, budget, maintenance — **landed**;
- `watcher/fanotify.rs` — the broker client behind the same `Source` trait
  (`name` / `regime` / `refresh` / `watched` / `watched_set` / `maintain`):
  a synchronous handshake (its answer *means* "you are covered"), then a
  reconnecting reader thread that ends with the repository (dropping the source
  shuts the stream down; the first version reconnected for ever after an
  unload) — **landed**.

Source choice: none to make. At load the daemon probes `[settings]
watchd-socket` and takes the broker when one answers; when none does it says
so and watches with inotify (spec-file-tracking "Watch sources and regimes",
spec-main "Daemon configuration"). A path where nothing listens is how one
stays on inotify deliberately.

The source facade is small: *events*, *refresh*, *watched_set*. The conformance
tests run against both implementations.

## Prior art

- **macOS `fseventsd`**: the OS does this natively — a daemon multiplexing
  volume events to clients. The broker is "fseventsd for Linux".
- **`cuilan/FsEventBridge`** (C17): `FAN_MARK_FILESYSTEM` → anchor-subtree
  filter → NDJSON over a Unix socket, root/`CAP_SYS_ADMIN`, systemd unit. The
  closest existing shape; too small to adopt, useful for known pitfalls (slow
  clients / queue-full policy, NFS caveats).
- fanotify consumers (`fatrace`, `fapolicyd`, `clamonacc`, …) are monolithic
  privileged apps, not subscription brokers — nothing to reuse.
- Rust: `fanotify-rs`, `naughtyfy`, `fanotify-fid` (FID/file-handle utilities),
  `tributary-fs` (Sans-IO core with inotify *and* fanotify backends — prior art
  for the dual-source facade). Thin ecosystem: `open_by_handle_at` and friends
  will likely go through `nix`/`rustix`/`libc`.

## Open questions

- ACL/LSM evaluation in the broker filter: **decided for now** — mode bits
  only, which answers *stricter* than the filesystem would (an ACL-granted
  access is not seen), never looser. If a repository with ACLs is denied
  wrongly, the fallback is the inotify source.
- Network filesystems: fanotify coverage there is kernel- and version-dependent
  ("does not catch remote events") — the inotify source is the fallback. The
  global option was decided *against* (September 2026): the socket is the
  switch — probe it and follow the answer. A per-repository override is the
  question that remains, and it has not been asked for.
- Multi-mount repositories: **settled and implemented** — one filesystem mark
  per filesystem under the root, the mount table followed as drives come and
  go (the daemon's side, "Offline subtrees", is unchanged). One limit: a mark
  whose filesystem is still mounted elsewhere after it left the root cannot be
  lifted by path, and stays until the broker restarts — its events are dropped
  by the filter, so it costs work, not correctness.
- Broker death mid-stream: **settled as the daemon-down posture** — the gap is
  announced (a diagnostic on disconnect, reconnect and overflow, each naming
  `mf reconcile`) and a reconcile closes it (spec-file-tracking "Watch sources
  and regimes"). **Reconciling is purely manual** (decided September 2026):
  the suggestion is the whole answer, after an overflow as after a
  reconnection — an automatic one would add to the load that caused an
  overflow, and fire every time a flaky socket blinks.
- The cost of filesystem marks: the broker receives every event of the
  filesystems it covers, inside the roots or not. **Filtered by parent
  directory (September 2026)**: `fanotify::Scope` answers "under a root?"
  from the event's directory handles, resolving each directory once and
  remembering the verdict — which only a directory move or a change of roots
  can invalidate, and either forgets them all. A busy `~/.cache` then costs one
  resolution, not one per event. What remains is the kernel side: those events
  are still queued and read, and a flood of them can still overflow the
  group's queue for everyone. Ignore marks (`FAN_MARK_IGNORE`, 6.0+) on the
  busiest outside directories would stop them in the kernel — not built.

## Tests

Done:

- The daemon's half through the real pipeline, against a stand-in broker
  (`daemon/tests/watch_source.rs`): the source choice at load, wire events
  becoming metarecords, `mfr_watch_exceeded` honoured under coverage, and the
  client ending with its repository. Unit tests for the client's translation
  (internal directory, non-UTF-8 names, overflow).
- The broker's kernel side in a user namespace (see "The broker"), its
  protocol, filter and server with synthetic events, and one end-to-end over a
  socket.
- The broker's filter for a directory the subscriber may not list (the `0700`
  case, as a `CredSource` table).

To do:

- A run against a real privileged broker (root, or the unit's two
  capabilities): the only place handle resolution and marks meet.
- The conformance battery (create/remove/rename/modify semantics through the
  pipeline) run against both sources.
- `tests/watch_leak.rs` as a backend test — under fanotify there is no
  per-directory state to leak at all.
- The `0700` other-uid directory case on a real filesystem, at both hops.
- Overflow → the announced suggestion, end to end.
