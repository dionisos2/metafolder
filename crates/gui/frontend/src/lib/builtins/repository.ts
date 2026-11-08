// The builtins acting on the workspace's repository and the daemon: ignore
// presets, ordering, watch activity, reconcile, orphans, duplicates, work
// contexts, undo and redo, opening a file with a program, the clipboard,
// `config:reload`, `daemon:set-url`.

import { registerArgs } from "../commandArgs";
import { reloadUserCommands } from "../commandRegistry";
import { collectArgs, promptForArg, runShell, surfacePanel } from "../commands";
import {
  ignorePresetCandidates,
  ignoreTarget,
  orderTargetDir,
  resolvePresetName,
  targetDir,
  type TargetDirOptions,
} from "../ignore";
import { invoke } from "../ipc";
import { changeFeed } from "../panels/changes";
import { shellQuote } from "../placeholders";
import { status } from "../status";
import { focusedWs, store, workspaceById } from "../store.svelte";
import { isWirePath } from "../treePath";
import type { ExecContext, WorkContext } from "../types";
import { daemonWork } from "../working";
import type { Builtin, Builtins } from "./types";

/** A daemon round-trip through the proxy, throwing the daemon's error on >=400. */
async function daemonJson(
  method: string,
  path: string,
  body: unknown = null,
): Promise<unknown> {
  const res = await daemonWork.track(
    invoke<{ status: number; body: unknown }>("daemon_request", {
      method,
      path,
      body,
    }),
    `${method} ${path.split("?")[0]}`,
  );
  if (res.status >= 400) {
    const err = (res.body as { error?: string })?.error;
    throw new Error(
      err ?? `daemon ${method} ${path} failed (HTTP ${res.status})`,
    );
  }
  return res.body;
}

/** The active repo of the workspace `ws` (the one a command runs in), or null. */
export function repoOf(ws: string | null): string | null {
  return workspaceById(ws)?.active_repo ?? null;
}

/** The repository's filesystem root (via GET /repos), or '' when not found. */
export async function repoRoot(repo: string): Promise<string> {
  const repos = (await daemonJson("GET", "/repos")) as {
    repo_uuid: string;
    root: string;
  }[];
  const norm = repo.replace(/-/g, "");
  return repos.find((r) => r.repo_uuid.replace(/-/g, "") === norm)?.root ?? "";
}

// ── Ignore presets (the `ignore:*` builtins) ────────────────────────────────
// The GUI half of `mf ignore` (doc "Setting ignore patterns"). Preset expansion
// lives in the backend (it reads a config file); the target directory and the
// copy-on-write prompt are shared with the file manager's Ignore menu
// (`lib/ignore.ts`).

registerArgs("ignore:add", [
  {
    name: "preset",
    prompt: () => "Ignore preset to add:",
    complete: () => ignorePresetCandidates(invoke),
  },
]);
registerArgs("ignore:remove", [
  {
    name: "preset",
    prompt: () => "Ignore preset to remove:",
    complete: () => ignorePresetCandidates(invoke),
  },
]);
registerArgs("ignore:set", [
  {
    name: "preset",
    prompt: () => "Replace the ignore set with:",
    complete: () => ignorePresetCandidates(invoke),
  },
]);

/** The directory the `ignore:*` commands act on, as a repo-root-relative path,
 *  plus the repo it belongs to. Null (with a status message) when there is no
 *  active repository. */
async function ignoreContext(
  ws: string | null,
): Promise<{ repo: string; dir: string } | null> {
  const repo = repoOf(ws);
  if (!repo) {
    await status("no active repository");
    return null;
  }
  return { repo, dir: await defaultTargetDir(repo, ws) };
}

/** The directory a path-scoped builtin acts on by default: the file manager's
 *  current directory, else the selected metarecord's directory, else the
 *  repository root — as a repo-root-relative path (`''` is the root). Shared by
 *  the `ignore:*` commands. */
async function defaultTargetDir(
  repo: string,
  ws: string | null,
): Promise<string> {
  return await targetDir(await targetDirContext(repo, ws));
}

/** What the target-directory rules read: the file manager's current directory
 *  and the workspace's selected metarecord. */
async function targetDirContext(
  repo: string,
  ws: string | null,
): Promise<TargetDirOptions> {
  const fmDir = ws
    ? await invoke<unknown>("ws_get_var", {
        wsId: ws,
        key: "file-manager:dir",
      })
    : null;
  const selected = ws
    ? await invoke<{ uuid: string } | null>("ws_get_var", {
        wsId: ws,
        key: "selected_metarecord",
      })
    : null;
  return {
    call: daemonJson,
    repo,
    repoRoot: await repoRoot(repo),
    // Exact (doc "Tree names"): a folder that is not text is refused there,
    // not taken for no folder.
    fmDir: isWirePath(fmDir) ? fmDir : null,
    selected: selected?.uuid ? { uuid: selected.uuid } : null,
  };
}

/** Applies one preset to the context directory with the given mode, reporting
 *  the target it resolved — applying a preset to the wrong directory would be
 *  silent otherwise. */
async function runIgnore(
  choice: string,
  mode: "add" | "remove" | "set",
  ws: string | null,
): Promise<void> {
  const context = await ignoreContext(ws);
  if (!context) return;
  const preset = await resolvePresetName(choice, invoke);
  if (!preset) {
    await status(`no such ignore preset: ${choice}`);
    return;
  }
  const target = await ignoreTarget({
    call: daemonJson,
    repo: context.repo,
    relPath: context.dir,
    confirm: (question) => window.confirm(question),
    // A whole-set replacement drops the inherited patterns on purpose.
    copy: mode !== "set",
  });
  if (!target) {
    await status(
      `${context.dir || "/"} is not tracked: nothing to write the patterns on`,
    );
    return;
  }
  if (target.copied.length > 0) {
    await invoke("ignore_write", {
      repo: context.repo,
      target: target.uuid,
      patterns: target.copied,
    });
  }
  const result = await invoke<string[]>("ignore_apply", {
    repo: context.repo,
    target: target.uuid,
    presets: [preset],
    mode,
  });
  await status(
    `Ignore: ${preset} ${mode === "remove" ? "removed from" : "applied to"} ` +
      `${context.dir || "/"} — ${result.length} pattern(s)`,
    "info",
  );
}

/** `ignore:list`: the installed presets and the target's active set, in the
 *  message panel (read-only, so it is also the safe way to look before
 *  applying). */
async function listIgnore(ws: string | null, slot: ExecContext["slot"]): Promise<void> {
  const context = await ignoreContext(ws);
  if (!context) return;
  if (!ws) return;
  const presets =
    await invoke<{ name: string; description: string; patterns: string[] }[]>(
      "ignore_presets",
    );
  const lines = ["Ignore presets:"];
  for (const preset of presets) {
    lines.push(
      `  ${preset.name.padEnd(14)}${preset.description} (${preset.patterns.length})`,
    );
  }
  const effective = (await daemonJson(
    "GET",
    `/repos/${context.repo}/ignore/effective?path=${encodeURIComponent(context.dir)}`,
  )) as { source: string | null; direct: boolean; patterns: string[] };

  const here = context.dir || "/";
  const source = effective.source === "" ? "/" : effective.source;
  lines.push(
    "",
    effective.source === null
      ? `No ignore patterns govern ${here}.`
      : effective.direct
        ? `Patterns of ${here} (its own):`
        : `Patterns governing ${here} (inherited from ${source}):`,
  );
  for (const pattern of effective.patterns) lines.push(`  ${pattern}`);
  await surfacePanel({ ws, slot }, "message");
  await invoke("append_message", { wsId: ws, text: lines.join("\n") });
}

// ── Order a folder's children (the `mf:order` builtin) ─────────────────────
// The GUI half of `mf order` (doc "Ordering a folder"): the heuristic and the daemon
// work are shared Rust (`metafolder_core::order`, behind the `order_run`
// command); the shell only works out *which* folder — the selection's, asked
// of nobody.

/** `mf:order`: numbers the direct children of the selected folder (or of the
 *  selected file's folder; the file manager's directory when nothing is
 *  selected), files and directories independently, and marks the folder. */
async function runOrder(ws: string | null): Promise<void> {
  const repo = repoOf(ws);
  if (!repo) {
    await status("no active repository");
    return;
  }
  const path = await orderTargetDir(await targetDirContext(repo, ws));
  if (path === null) {
    await status("no folder or file is selected: nothing to number");
    return;
  }
  const report = await invoke<{ message: string }>("order_run", { repo, path });
  await status(report.message, "info");
}

// ── Watch activity (the `mf:watch-activity` builtin) ────────────────────────────────

/** `mf:watch-activity reset`: the GUI half of `mf watch activity --reset` (doc
 *  "Watch activity") — the active repository's event counts start over. The
 *  counts live in the daemon's memory and in no log, so the change feed's poll
 *  cannot report the reset: the panels are told here, or a file manager would
 *  keep showing the old counts. */
async function resetWatchActivity(ws: string | null): Promise<void> {
  const repo = repoOf(ws);
  if (!repo) {
    await status("no active repository");
    return;
  }
  try {
    await daemonJson("POST", `/repos/${repo}/watch/activity/reset`, {});
  } catch (e) {
    await status(e instanceof Error ? e.message : String(e));
    return;
  }
  changeFeed.notify(repo);
  await status("watch activity reset", "info");
}

// ── Reloading the user configuration ───────────────────────────────────────
// What the GUI can re-read without a restart. Kept in step with
// `RELOAD_TARGETS` in crates/gui/src/commands.rs, which is where the
// omissions (config.toml, panel-types, ignore-presets) are argued.
const RELOAD_TARGETS = ["keybindings", "style", "grammar", "commands"];

registerArgs("config:reload", [
  {
    name: "target",
    prompt: () => `Reload what? (${[...RELOAD_TARGETS, "all"].join(" / ")})`,
    complete: () => [...RELOAD_TARGETS, "all"],
  },
]);

// ── Opening a file with another program (the `file:open-with` builtin) ──────
// A shell builtin, so it works from whichever panel carries the selection
// (`selected_paths`). The program is collected in the command input, completing
// over the configured `open-with` list — candidates, not a whitelist: any
// command line may be typed, and it is run exactly as a `!` command is (output
// in the message panel), without stealing the focused slot.

/** The configured `open-with` candidates (config.toml), or none on failure. */
async function openWithPrograms(): Promise<string[]> {
  try {
    return await invoke<string[]>("open_with_programs");
  } catch {
    return [];
  }
}

/** Runs `<commandLine> <paths…>`. `commandLine` is inserted verbatim (so
 *  `gimp -n` or `env FOO=1 mpv` work); only the paths are quoted. */
async function openWith(commandLine: string, ws: string | null): Promise<void> {
  const program = commandLine.trim();
  if (!program) return;
  if (!ws) return;
  const paths = await invoke("ws_get_var", { wsId: ws, key: "selected_paths" });
  // Exact (doc "Tree names"): a path that is not text is quoted as its bytes.
  const files = (Array.isArray(paths) ? paths : []).filter(
    (p): p is Metafolder.WirePath => isWirePath(p) && p !== "",
  );
  if (files.length === 0) {
    await status("no file or folder is selected");
    return;
  }
  await runShell([program, ...files.map(shellQuote)].join(" "), { ws });
}

registerArgs("file:open-with", [
  {
    name: "program",
    prompt: () => "Open with which program?",
    complete: () => openWithPrograms(),
  },
]);

registerArgs("context:set", [
  {
    name: "name",
    prompt: async () => {
      const ws = focusedWs();
      const current = ws
        ? await invoke<WorkContext | null>("context_active", {
            wsId: ws,
          }).catch(() => null)
        : null;
      return current
        ? `Work context name? (closes "${current.name}")`
        : "Work context name?";
    },
  },
  {
    name: "description",
    prompt: () => "Description? (what the work is; empty for none)",
  },
]);

/** `context:set <name> [description]`: opens a work context on the
 *  workspace's repository — every later change the GUI makes there is written
 *  in it — closing the one in force, if any (one at a time). A name with
 *  spaces is quoted on the command line; the description is the rest. */
async function setWorkContext(
  ws: string,
  name: string,
  description: string,
): Promise<void> {
  if (!name) return;
  const opened = await invoke<WorkContext>("context_set", {
    wsId: ws,
    name,
    description: description || null,
  });
  await status(
    `in work context "${opened.name}" (label ${opened.label})`,
    "info",
  );
}

/** `context:unset`: closes the workspace's repository's work context. */
async function unsetWorkContext(ws: string): Promise<void> {
  const closed = await invoke<WorkContext | null>("context_unset", {
    wsId: ws,
  });
  await status(
    closed ? `work context "${closed.name}" closed` : "no work context is open",
    "info",
  );
}

/** `log:undo` / `log:redo` (doc "Undo and redo by label"): the label named,
 *  or — none named — the one the newest change (undo) was written in; when it
 *  was written in several, which one is asked, innermost first. */
async function undoOrRedo(
  ws: string,
  redo: boolean,
  named: string | undefined,
): Promise<void> {
  let label: number | null = null;
  if (named !== undefined) {
    label = Number.parseInt(named, 10);
    if (!Number.isFinite(label)) {
      await status(`not a label: "${named}"`);
      return;
    }
  } else {
    const candidates = await invoke<{ id: number; name: string }[]>(
      "log_candidates",
      {
        wsId: ws,
        redo,
      },
    );
    if (candidates.length > 1) {
      const chosen = await collectArgs(
        [
          {
            name: "label",
            prompt: () => `${redo ? "Redo" : "Undo"} which label?`,
            complete: () => candidates.map((c) => `${c.id} ${c.name}`),
          },
        ],
        [],
        promptForArg,
      );
      if (chosen === null) return;
      label = Number.parseInt(chosen[0] ?? "", 10);
      if (!Number.isFinite(label)) return;
    }
  }
  await invoke("log_navigate", { wsId: ws, redo, label });
}

const applyIgnore: Builtin = async (args, context, name) => {
  // The `preset` argument was collected by dispatch (with completion).
  const { ws } = context;
  const choice = args.join(" ").trim();
  if (choice)
    await runIgnore(
      choice,
      name.slice("ignore:".length) as "add" | "remove" | "set",
      ws,
    );
};

const undoOrRedoLabel: Builtin = async (args, context, name) => {
  const { ws } = context;
  if (ws) await undoOrRedo(ws, name === "log:redo", args[0]);
};

export const repositoryBuiltins: Builtins = {
  "config:reload": async (args) => {
    const report = await invoke<string>("config_reload", { what: args[0] });
    // The command module is the shell's to re-import; Rust has only checked
    // that the file is readable.
    if (args[0] === "commands" || args[0] === "all")
      await reloadUserCommands();
    await status(report, "info");
  },
  "ignore:add": applyIgnore,
  "ignore:remove": applyIgnore,
  "ignore:set": applyIgnore,
  "ignore:list": async (_args, context) => {
    const { ws } = context;
    await listIgnore(ws, context.slot);
  },
  "mf:order": async (_args, context) => {
    const { ws } = context;
    await runOrder(ws);
  },
  "reconcile:run": async (_args, context) => {
    const { ws } = context;
    if (ws) await invoke("reconcile_run", { wsId: ws });
  },
  "mf:duplicate": async (args, context) => {
    const { ws } = context;
    if (args[0] !== "scan") {
      await status(`unknown operation: "${args[0] ?? ""}" (expected scan)`);
      return;
    }
    // The GUI half of `mf duplicate scan` (doc "duplicates panel"). Options
    // stay CLI-only, as `mf trash prune`'s do: this runs the ordinary
    // whole-repository scan, and the Rust side posts its own status.
    if (ws) await invoke("duplicate_scan", { wsId: ws });
  },
  "mf:watch-activity": async (args, context) => {
    const { ws } = context;
    if (args[0] !== "reset") {
      await status(`unknown operation: "${args[0] ?? ""}" (expected reset)`);
      return;
    }
    await resetWatchActivity(ws);
  },
  "orphan:detect": async (_args, context) => {
    // Mark the orphaned metarecords (doc "Finding and clearing orphans").
    // The Rust side posts its own status; swallow the rejection so an error
    // is not surfaced twice.
    const { ws } = context;
    if (ws) await invoke("orphan_detect", { wsId: ws }).catch(() => 0);
  },
  "context:set": async (args, context) => {
    // Both were collected by dispatch, the description with all its words.
    const { ws } = context;
    if (ws)
      await setWorkContext(
        ws,
        (args[0] ?? "").trim(),
        args.slice(1).join(" ").trim(),
      );
  },
  "context:unset": async (_args, context) => {
    const { ws } = context;
    if (ws) await unsetWorkContext(ws);
  },
  "clipboard:clear": async (_args, context) => {
    const { ws } = context;
    await invoke("global_send", { vars: { clipboard: null } });
    if (ws) {
      await invoke("post_status", {
        wsId: ws,
        text: "Clipboard emptied",
        kind: "info",
        timeoutMs: 5000,
      });
    }
  },
  "log:undo": undoOrRedoLabel,
  "log:redo": undoOrRedoLabel,
  "file:open-with": async (args, context) => {
    // The `program` argument was collected by dispatch (completing over the
    // configured list); the whole tail is the command line, so `gimp -n` and
    // any other flags survive.
    const { ws } = context;
    await openWith(args.join(" "), ws);
  },
  "daemon:set-url": async (args) => {
    // The URL was collected by dispatch (inline or in the minibuffer).
    const url = args[0] ?? "";
    if (url === "") {
      await status("no daemon URL given");
      return;
    }
    const connected = await invoke<boolean>("daemon_set_url", { url });
    store.daemonUrl = url;
    await status(
      `daemon URL set; ${connected ? "connected" : "unreachable"}`,
      "info",
    );
  },
};
