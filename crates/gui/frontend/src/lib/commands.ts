// Command invocation parsing, autocomplete filtering and dispatch
// (doc "The command input"). Parsing and filtering are pure and unit
// tested; dispatch routes to Tauri commands and panel instances.

import { isWirePath, wirePaths } from "./treePath";
import { confirmClosing } from "./unsavedEdits";
import { recordPath } from "../../../panel-shim/file-actions.js";
import { pathText } from "../../../panel-shim/ui.js";
import { type FileOps } from "./fileCommands";
import { cycleStep } from "./focusCycle";
import { bindView, completionLabels } from "./completions";
import type {
  CompletionItem,
} from "./completions";
import { setHelpCursor } from "./cursor";
import { closeFind, openFind, stepFind } from "./find";
import { openHints } from "./hints";
import {
  ignorePresetCandidates,
  ignoreTarget,
  orderTargetDir,
  resolvePresetName,
  targetDir,
  type TargetDirOptions,
} from "./ignore";
import { invoke } from "./ipc";
import {
  type ExpandDeps,
  expandShellPlaceholders,
  shellQuote,
} from "./placeholders";
import {
  activeQuestion,
  flashStatus,
  focusedContext,
  focusedWs,
  hasPromptIn,
  offerPrompt,
  slotPayload,
  store,
  workspaceById,
} from "./store.svelte";
import { isTarget, resolveTarget, TARGETS } from "./targets";
import { macroEntry, macros } from "./macro";
import { daemonWork } from "./working";
import {
  HISTORY_COMMANDS,
  SELF_PANEL,
  SELF_FOCUS,
  historyFieldNames,
  runHistoryCommand,
  validateField,
} from "./historyCommands";
import {
  functionProvider,
  GENERIC_COMMANDS,
  runGeneric,
} from "./panelFunctions";
import { changeFeed } from "./panels/changes";
import type {
  CommandDef,
  ExecContext,
  LayoutView,
  WorkContext,
  WorkspaceInfo,
} from "./types";
import { type ArgPromptFn, type ArgPromptRequest, type ArgSpec, type AskMarker, type InvocationParts, type PromptAnswer, argSpecFor, isAsk, panelArgs, registerArgs } from "./commandArgs";
import { parseInvocation, shouldLogCommand } from "./commandParsing";
import { isUserCommand, reloadUserCommands, runUserCommand, runsInTransaction } from "./commandRegistry";
import { genericPanel, savedWorkspaces } from "./builtinArgs";

export * from "./commandParsing";
export * from "./commandArgs";
export * from "./commandRegistry";

// ── Installed helper scripts (the `script:run` builtin) ─────────────────────

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
function repoOf(ws: string | null): string | null {
  return workspaceById(ws)?.active_repo ?? null;
}

/** The repository's filesystem root (via GET /repos), or '' when not found. */
async function repoRoot(repo: string): Promise<string> {
  const repos = (await daemonJson("GET", "/repos")) as {
    repo_uuid: string;
    root: string;
  }[];
  const norm = repo.replace(/-/g, "");
  return repos.find((r) => r.repo_uuid.replace(/-/g, "") === norm)?.root ?? "";
}

// ── Installed helper scripts (the `script:run` builtin) ─────────────────────
// The shipped scripts live in ~/.config/metafolder/scripts/; a launchable one
// carries a `# Summary:` header (doc "Shipped defaults in the source tree"), enumerated by
// the `list_scripts` command. The argument completes to "<name> — <summary>"
// lines; picking one runs the script as a subprocess whose output streams to
// the message panel (like a `!` command), and the script drives the GUI back
// through `mf gui`.

interface ScriptInfo {
  name: string;
  summary: string;
  path: string;
}

/** Candidate display line → absolute script path, rebuilt on each completion
 *  pass (and consulted again at launch). */
const scriptChoices = new Map<string, string>();

/** The installed launchable scripts, newest listing each call. */
function installedScripts(): Promise<ScriptInfo[]> {
  return invoke<ScriptInfo[]>("list_scripts");
}

/** One display line per installed script; also (re)builds `scriptChoices`. */
async function scriptCandidates(): Promise<string[]> {
  scriptChoices.clear();
  const scripts = await installedScripts();
  return scripts.map((s) => {
    const line = `${s.name} — ${s.summary}`;
    scriptChoices.set(line, s.path);
    return line;
  });
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

registerArgs("script:run", [
  {
    name: "script",
    prompt: () => "Run script:",
    complete: () => scriptCandidates(),
  },
]);

registerArgs("completion:cycle", [
  {
    name: "direction",
    // Optional and never asked for: its absence *is* the forward cycle — the
    // point of the default binding (doc "Completion views").
    optional: true,
    prompt: () => "Cycle which way? (forward / back)",
    complete: () => ["forward", "back"],
  },
]);

for (const command of HISTORY_COMMANDS) {
  registerArgs(command, [
    {
      name: "field",
      prompt: () => `Which field? (${SELF_FOCUS}, ${SELF_PANEL}, or a field of the focused panel)`,
      complete: () => [
        SELF_FOCUS,
        ...(command === "history:search" ? [] : [SELF_PANEL]),
        ...historyFieldNames(focusedPanelRoot()),
      ],
    },
  ]);
}

/** Resolves a picked argument to an installed script's path. Accepts the full
 *  "<name> — <summary>" completion line, or a bare name (e.g. from a
 *  keybinding), with or without the `.sh` extension. Null when nothing matches. */
async function resolveScriptPath(choice: string): Promise<string | null> {
  const mapped = scriptChoices.get(choice);
  if (mapped) return mapped;
  const want = choice.trim();
  const scripts = await installedScripts();
  const hit = scripts.find(
    (s) =>
      `${s.name} — ${s.summary}` === want ||
      s.name === want ||
      s.name === `${want}.sh`,
  );
  return hit?.path ?? null;
}

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

/** The repositories already asked about this session: a context is the
 *  repository's, so a second workspace on it is not asked again. */
const offeredRepos = new Set<string>();

/** How many times, a second apart, a repository still loading is asked what
 *  it remembers before giving up. */
const PENDING_ATTEMPTS = 30;

/**
 * Asks, for each repository newly opened in a workspace, whether to stay in
 * the work context it was left in or to end it (doc "The GUI's work context").
 * Once per repository and session; Escape leaves things as they were — the
 * context stays in force.
 */
export async function offerWorkContexts(
  workspaces: WorkspaceInfo[],
): Promise<void> {
  for (const w of workspaces) {
    const repo = w.active_repo;
    if (!repo || offeredRepos.has(repo)) continue;
    offeredRepos.add(repo);
    await offerWorkContext(w.id, w.repo_name ?? repo.slice(0, 8));
  }
}

async function offerWorkContext(ws: string, repoName: string): Promise<void> {
  type Pending = {
    state: "none" | "open" | "gone";
    label?: number;
    name?: string;
    description?: string | null;
  };
  let pending: Pending | null = null;
  for (let attempt = 1; pending === null; attempt++) {
    try {
      pending = await invoke<Pending>("context_pending", { wsId: ws });
    } catch (error) {
      // A repository still loading answers 503 for a while.
      if (attempt >= PENDING_ATTEMPTS) {
        await status(`work context of ${repoName}: ${String(error)}`);
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }
  if (pending.state === "gone") {
    await status(
      `work context "${pending.name}" of ${repoName} no longer exists (label ${pending.label} pruned): closed`,
      "info",
    );
    return;
  }
  if (pending.state !== "open") return;
  const chosen = await collectArgs(
    [
      {
        name: "answer",
        prompt: () =>
          `Work context "${pending.name}"${pending.description ? ` (${pending.description})` : ""} is still open in ${repoName}: stay in it or end it? (stay / end)`,
        complete: () => ["stay", "end"],
      },
    ],
    [],
    promptForArg,
  );
  if (chosen?.[0] === "end") {
    await invoke("context_unset", { wsId: ws });
    await status(`work context "${pending.name}" closed`, "info");
  } else {
    await invoke("context_resume", { wsId: ws });
    await status(`in work context "${pending.name}"`, "info");
  }
}

/** Runs an installed script, surfacing its output in the shell panel exactly
 *  as a `!` command does. */
async function runScript(path: string, context: ExecContext): Promise<void> {
  await surfacePanel(context, "shell");
  // A script's writes are one label in the log, its commands' nested in it
  // (doc "Labels and contexts").
  await runShell(`bash ${shellQuote(path)}`, {
    label: `script ${path.split("/").pop() ?? path}`,
    ws: context.ws,
  });
}

/**
 * Assembles a command's full argument list from the inline-`provided` prefix,
 * gathering any missing trailing arguments through `promptFn`. The last
 * declared argument absorbs extra inline tokens (joined by space), matching
 * CLI behaviour (`args.slice(n).join(' ')`). Each spec function receives the
 * arguments already collected. Returns null if the user cancels (Escape) at
 * any argument.
 */
export async function collectArgs(
  specs: ArgSpec[],
  provided: (string | AskMarker)[],
  promptFn: ArgPromptFn,
): Promise<string[] | null> {
  const result: string[] = [];
  // Index into `provided`, distinct from the spec index: a spec dropped by
  // `when` consumes no token, so the tokens stay aligned with the arguments
  // actually asked for.
  let used = 0;
  for (let i = 0; i < specs.length; i++) {
    const spec = specs[i];
    if (spec.when && !spec.when(result)) continue;
    const isLast = i === specs.length - 1;
    // An argument given as `mf.ask` is asked, like one left out, whatever
    // its place — an optional one too, since it was named.
    let asked: AskMarker | null = null;
    if (used < provided.length) {
      const rest = provided.slice(used);
      if (isLast && rest.length > 1 && rest.some(isAsk)) {
        throw new Error(
          `the last argument (${spec.name}) takes every element left: it cannot ask for one of them`,
        );
      }
      const given = provided[used];
      used += 1;
      if (isAsk(given)) {
        asked = given;
      } else {
        // Inline-provided: the last declared argument absorbs the remaining
        // tokens so a value may contain spaces without quoting.
        result.push(isLast ? (rest as string[]).join(" ") : given);
        if (isLast) used = provided.length;
        continue;
      }
    }
    // Nothing left to fill it with: an optional argument is absent, not asked.
    if (spec.optional && asked === null) continue;
    // `prompt` and `initial` are awaited — they are what the input shows and
    // pre-fills. The candidates are NOT: the first page is handed over as it
    // comes (an array, or a promise the driver resolves once the input is
    // already open), so a slow candidate list never delays the prompt. What
    // stands behind that page goes with the request too: the views
    // `completion:cycle` walks — `spec.views` when *which* views there are
    // depends on the arguments collected so far, else the one builder (or the
    // declared array) — each bound to `prior` and memoized, so the eager first
    // page and the driver's own first load are one and the same call.
    const prior = [...result];
    const hasSource = spec.complete !== undefined || spec.views !== undefined;
    const source = hasSource
      ? Promise.resolve(spec.views ? spec.views(prior) : [])
          // `views` wins over `complete` — unless it has none to offer, where
          // `complete` is the (single-view) answer. A generic command's arg
          // spec delegates to its operations and lets an operation that names
          // no views fall back to its plain candidates.
          .then((list) =>
            list.length > 0
              ? list
              : Array.isArray(spec.complete)
                ? spec.complete
                : spec.complete
                  ? [{ items: spec.complete }]
                  : [],
          )
          .then((list) => ({
            views: list.map((view) => bindView(view, prior)),
          }))
      : null;
    const answer = await promptFn({
      argName: spec.name,
      position: result.length,
      prompt: await spec.prompt(result),
      initial:
        asked?.initial !== undefined
          ? asked.initial
          : spec.initial
            ? await spec.initial(result)
            : "",
      completions: source
        ? source.then(
            ({ views }) => views[0]?.items("").then((page) => page.items) ?? [],
          )
        : [],
      ...(source ? { source } : {}),
      ...(spec.alternate ? { alternate: spec.alternate.hint } : {}),
    });
    if (answer === null) return null;
    if (typeof answer === "string") {
      result.push(answer);
      continue;
    }
    if (answer.alternate && spec.alternate) {
      result.splice(0, result.length, ...spec.alternate.prior(prior));
    }
    result.push(answer.value);
  }
  return result;
}

// ── Editing target ─────────────────────────────────────────────────────
// The focused text input registers handlers for the editing:* commands
// (which fire with text-input = true keybindings).

export interface EditingTarget {
  confirm(): void;
  unfocus(): void;
  /** Clear the input's content, then unfocus it. */
  discard(): void;
  lineStart(): void;
  lineEnd(): void;
  /** Remove what follows the caret on its line (`editing:clear-line-end`). */
  clearToLineEnd(): void;
  /** Cycles the prompt's completion views (`completion:cycle`) by `delta`
   *  (1 forward, -1 back). Absent on an input whose prompt offers a single
   *  view — or none. */
  cycleCompletion?: (delta: number) => void;
}

let editingTarget: EditingTarget | null = null;

export function setEditingTarget(target: EditingTarget | null) {
  editingTarget = target;
}

/** Whether an editing:* command currently has a registered handler. */
export function hasEditingTarget(): boolean {
  return editingTarget !== null;
}

/** The innermost focused element, piercing panel Shadow DOM roots. */
export function deepActiveElement(): Element | null {
  let el: Element | null = document.activeElement;
  while (el?.shadowRoot?.activeElement) el = el.shadowRoot.activeElement;
  return el;
}

/** The Shadow root of the focused slot's panel, null when there is none. */
export function focusedPanelRoot(): ShadowRoot | null {
  const { workspace_id: ws, panel_type: type } = slotPayload(store.layout.focused);
  return ws && type ? (functionProvider()?.root(ws, type) ?? null) : null;
}

/** `editing:discard` on an element with no registered editing target — a
 *  panel's own input. Empties it (firing `input`, so the panel's own listener
 *  sees the change like a user deletion) and removes the focus; a non-text
 *  element has nothing to empty and is only blurred. Returns whether anything
 *  was cleared. */
export function discardActiveInput(el: Element | null): boolean {
  if (!el) return false;
  const cleared =
    el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement;
  if (cleared) {
    el.value = "";
    el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  }
  (el as HTMLElement).blur?.();
  return cleared;
}

/** `editing:clear-line-end` on an element with no registered editing target —
 *  a panel's own input: removes what follows the caret (or the selection's
 *  start) up to the end of its line, as Emacs' kill-line does — so at the end
 *  of a textarea line, the line break, joining the next one. Fires `input` so
 *  the panel sees it as typed; the field keeps the keyboard. Returns whether
 *  anything was removed. */
export function clearToLineEnd(el: Element | null): boolean {
  if (!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement))
    return false;
  const start = el.selectionStart ?? el.value.length;
  let end = el.value.length;
  if (el instanceof HTMLTextAreaElement) {
    const newline = el.value.indexOf("\n", start);
    if (newline === start) end = start + 1;
    else if (newline !== -1) end = newline;
  }
  if (end <= start) return false;
  el.setRangeText("", start, end, "start");
  el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  return true;
}

/** `editing:uppercase`: the selected text of a text field in capitals, still
 *  selected. Through the editor's own insertion where there is one, so that the
 *  field's undo takes it back. */
export function uppercaseSelection(el: Element | null): void {
  if (!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement))
    return;
  const start = el.selectionStart ?? 0;
  const end = el.selectionEnd ?? start;
  if (start === end) return;
  const upper = el.value.slice(start, end).toUpperCase();
  // execCommand: the only insertion the undo of a text field sees.
  if (!el.ownerDocument.execCommand?.("insertText", false, upper)) {
    el.setRangeText(upper, start, end, "preserve");
    el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  }
  el.setSelectionRange(start, start + upper.length);
}

/** `editing:goto` on an element with no registered editing target — a panel's
 *  own input: the caret goes to the edge of its line, which in a textarea is not
 *  the edge of the value. */
export function gotoLineEdge(
  el: Element | null,
  edge: "line-start" | "line-end",
): void {
  const input = el as HTMLInputElement | HTMLTextAreaElement | null;
  const value = input?.value ?? "";
  let at = edge === "line-start" ? 0 : value.length;
  if (el instanceof HTMLTextAreaElement) {
    const caret = el.selectionStart ?? 0;
    if (edge === "line-start") {
      at = caret === 0 ? 0 : value.lastIndexOf("\n", caret - 1) + 1;
    } else {
      const newline = value.indexOf("\n", caret);
      if (newline !== -1) at = newline;
    }
  }
  input?.setSelectionRange?.(at, at);
}

// ── Panel dispatch hook (wired by PanelHost) ───────────────────────────

export type PanelDispatch = (
  command: CommandDef,
  args: string[],
  context: ExecContext,
) => Promise<void>;
let panelDispatch: PanelDispatch | null = null;

export function setPanelDispatch(fn: PanelDispatch | null) {
  panelDispatch = fn;
}

/** What the `file:` commands work with (doc "File commands"), in workspace
 *  `ws` and the run of `context`. */
function fileOps(ws: string, context: ExecContext): FileOps {
  const repo = () => {
    const active = repoOf(ws);
    if (!active) throw new Error("no active repository");
    return active;
  };
  return {
    readDir: async (dir) =>
      await invoke<{ name: string }[]>("fs_read_dir", { path: dir }),
    move: async (from, to) => void (await invoke("fs_move", { from, to })),
    copy: async (from, to) => void (await invoke("fs_copy", { from, to })),
    mkdir: async (path) => void (await invoke("fs_mkdir", { path })),
    createFile: async (path) => void (await invoke("fs_create_file", { path })),
    trash: async (path) =>
      (await invoke<Metafolder.Trashed>("trash_path", { repo: repoOf(ws) ?? null, path }))
        .desktop,
    record: async (path) => {
      const daemon = {
        request: (method: string, url: string, body: unknown) =>
          invoke<Metafolder.DaemonResponse>("daemon_request", {
            method,
            path: url,
            body,
          }),
      };
      const outcome = await recordPath(daemon, repo(), path, (question: string) =>
        window.confirm(question),
      );
      if ("error" in outcome) throw new Error(outcome.error);
      return !("cancelled" in outcome);
    },
    confirm: async (question, many) =>
      (!many && store.panelSettings["confirm-trash"] === false) || window.confirm(question),
    ask: async (question, initial = "") => {
      const answer = await promptForArg({
        argName: "name",
        position: 0,
        prompt: question,
        initial,
        completions: [],
      });
      if (answer === null) return null;
      return typeof answer === "string" ? answer : answer.value;
    },
    clipboard: async () => {
      const value = await invoke<{ paths: string[]; mode: "copy" | "cut" } | null>(
        "global_get_var",
        { key: "clipboard", txn: context.txn },
      );
      return value && Array.isArray(value.paths) ? value : null;
    },
    clearClipboard: async () =>
      void (await invoke("global_send", {
        vars: { clipboard: null },
        txn: context.txn,
      })),
    status: async (text) =>
      void (await invoke("post_status", {
        wsId: ws,
        text,
        kind: "info",
        timeoutMs: 5000,
      })),
    changed: async () =>
      void (await invoke("ws_set_var", {
        wsId: ws,
        key: "metarecords:dirty",
        value: Date.now(),
        txn: context.txn,
      })),
  };
}

/** `panel:focus cycle` (F6): the left panel, the right one, the script's
 *  question when one awaits keys, and round again (`cycleStep`). */
async function focusCycle(): Promise<void> {
  const step = cycleStep({
    split: store.layout.left.visible && store.layout.right.visible,
    focused: store.layout.focused,
    question: (activeQuestion()?.keys.length ?? 0) > 0,
    scriptKeys: store.ui.scriptKeys,
  });
  if (step.scriptKeys !== undefined && step.scriptKeys !== store.ui.scriptKeys) {
    await invoke("script_keys_toggle");
  }
  if (step.focus !== undefined) await invoke("focus_slot", { slot: step.focus });
}



// ── Dispatch ───────────────────────────────────────────────────────────

/** Posts a status message on the focused workspace's status bar (and so to
 *  its message log). `keys.ts` reports a `help:key` answer through it. */
export async function status(text: string, kind = "error") {
  const ws = focusedWs();
  if (ws)
    await invoke("post_status", { wsId: ws, text, kind, timeoutMs: 5000 });
}

/** Why `panelType`, a viewer of the selection, has nothing of `ws`'s selection
 *  to show — or `null` when it has, or is no viewer (doc "Opening a row"). A
 *  metarecord without a file is nothing for the viewer panel, a path without a
 *  metarecord nothing for the detail panel; no selection at all is not refused,
 *  so a viewer can still be opened to set up a layout. */
async function viewerHasNothing(
  panelType: string,
  ws: string | null,
): Promise<string | null> {
  if (!ws || (panelType !== "viewer" && panelType !== "metarecord-detail"))
    return null;
  const [metarecord, paths] = await Promise.all([
    invoke("ws_get_var", { wsId: ws, key: "selected_metarecord" }),
    invoke("ws_get_var", { wsId: ws, key: "selected_paths" }),
  ]);
  const hasMetarecord = metarecord !== null && typeof metarecord === "object";
  const hasPaths = Array.isArray(paths) && paths.length > 0;
  if (panelType === "viewer" && hasMetarecord && !hasPaths) {
    return "no file or folder for this metarecord — open it in metarecord-detail instead";
  }
  if (panelType === "metarecord-detail" && hasPaths && !hasMetarecord) {
    return "no metarecord for this entry — record it first";
  }
  return null;
}

/** Opens the help panel in `slot` on `topic` (empty: its index). The topic is
 *  handed to the panel through a workspace var; the `nonce` makes an
 *  identical repeated topic still re-trigger the panel's onChange. */
async function openHelp(
  ws: string,
  slot: ExecContext["slot"],
  topic: string,
  focus = false,
): Promise<void> {
  await invoke("ws_set_var", {
    wsId: ws,
    key: "help.request",
    value: { topic, nonce: Date.now(), focus },
  });
  await invoke("panel_set_type", { slot, panelType: "help" });
}

/** Immersive mode: mirror the flag into the store (the shell hides all
 *  chrome but the focused panel) and drive the OS window fullscreen. */
export async function setFullscreen(on: boolean): Promise<void> {
  store.ui.fullscreen = on;
  try {
    await invoke("set_fullscreen", { on });
  } catch (error) {
    await status(String(error));
  }
}

/** Runs a shell line in workspace `ws` (the command's, the focused one by
 *  default). `typed`: a line the user typed
 *  (after `!` or in the shell panel), run with their shell configuration;
 *  anything else — a script, a program opened on a file — runs in a plain
 *  `sh`, whatever that configuration says (doc "shell panel"). */
export async function runShell(
  commandLine: string,
  {
    label,
    typed = false,
    ws = focusedWs(),
  }: { label?: string; typed?: boolean; ws?: string | null } = {},
): Promise<void> {
  if (!ws) return;
  try {
    await invoke("run_shell", {
      wsId: ws,
      commandLine,
      label: label ?? null,
      typed,
    });
  } catch (error) {
    await status(String(error));
  }
}

/** Whether surfacing output should switch the focused slot to `panelType`:
 *  true unless some visible slot of `ws` already shows it (which also avoids
 *  the "two visible slots, same type" rejection). */
export function needsPanel(
  layout: LayoutView,
  ws: string | null,
  panelType: string,
): boolean {
  if (!ws) return false;
  const shows = (slot: LayoutView["left"]) =>
    slot.visible && slot.workspace_id === ws && slot.panel_type === panelType;
  return !(shows(layout.left) || shows(layout.right));
}

/** Surfaces the output of a command run in `context`: its slot switches to
 *  `panelType` unless a visible slot of its workspace already shows one — and
 *  only when that slot shows the workspace: a command run in a workspace out
 *  of sight leaves the screen alone, its output kept in the workspace's log. */
async function surfacePanel(
  context: ExecContext,
  panelType: string,
): Promise<void> {
  if (!needsPanel(store.layout, context.ws, panelType)) return;
  if (slotPayload(context.slot).workspace_id !== context.ws) return;
  await invoke("panel_set_type", { slot: context.slot, panelType });
}

/** Data sources for `%`-placeholder expansion, reading the selection from the
 *  workspace var store and the metarecord/tree data through the daemon proxy. */
function shellExpandDeps(ws: string | null): ExpandDeps {
  const daemon = async (path: string) => {
    // Through invoke's type parameter, not an `as` cast: same result, but the
    // shape is asked for rather than asserted after the fact.
    const res = await invoke<{ status: number; body: unknown }>(
      "daemon_request",
      {
        method: "GET",
        path,
        body: null,
      },
    );
    if (res.status !== 200) throw new Error(`HTTP ${res.status}`);
    return res.body;
  };
  const wsVar = async (key: string) =>
    ws ? await invoke("ws_get_var", { wsId: ws, key }) : null;
  return {
    async selected() {
      const value = await wsVar("selected_metarecord");
      return value && typeof value === "object"
        ? (value as { uuid: string; repo: string })
        : null;
    },
    metarecord: (repo, uuid) =>
      daemon(`/repos/${repo}/metarecords/${uuid}`) as Promise<{
        version?: number;
        fields?: never[];
      }>,
    async treePaths(repo, uuid, field) {
      const body = (await daemon(
        `/repos/${repo}/metarecords/${uuid}/fields/${encodeURIComponent(field)}/resolve-tree`,
      )) as { paths?: unknown };
      return wirePaths(body.paths);
    },
    async selectedPaths() {
      const value = await wsVar("selected_paths");
      return Array.isArray(value)
        ? value.filter(isWirePath)
        : [];
    },
    async activeRepo() {
      const value = await wsVar("active_repo");
      return typeof value === "string" ? value : null;
    },
    async repoName(repo) {
      const body = (await daemon(`/repos/${repo}`)) as { name?: string };
      if (!body.name) throw new Error(`repository ${repo} has no name`);
      return body.name;
    },
  };
}

/**
 * Prompt driver for interactive argument collection: opens the command input
 * as a frontend-resolved prompt (doc "Interactive command arguments")
 * and resolves to the chosen *value* (a candidate's, or the typed text when
 * none names it), or null on Escape. Refuses (null) when the workspace already
 * has a prompt — an interactive collection or a script prompt: one per
 * workspace (doc "Script sessions").
 */
async function promptForArg(
  request: ArgPromptRequest,
): Promise<string | PromptAnswer | null> {
  const ws = focusedWs();
  if (hasPromptIn(ws)) {
    await status("the command input is busy with another prompt");
    return null;
  }
  return new Promise<string | PromptAnswer | null>((resolve) => {
    const resolver = (value: string | null, alternate = false) =>
      resolve(value === null ? null : { value, alternate });
    // The collection belongs to the workspace the command was invoked from:
    // switching tab puts its question away with the rest of that workspace's
    // work, and any other workspace's prompt waits parked meanwhile (doc
    // "Script sessions"). The command input focuses itself when the prompt is
    // on screen.
    offerPrompt({
      id: null,
      text: request.prompt,
      completions: [],
      items: [],
      more: false,
      source: null,
      viewIndex: 0,
      workspaces: ws === null ? [] : [ws],
      task: null,
      draft: request.initial,
      resolver,
      alternate: request.alternate ?? null,
    });
    if (request.source) {
      // The live source owns the list; its first load is the eager page's own
      // memoized call. A prompt that ended or changed while either is in
      // flight is left alone.
      void Promise.resolve(request.source).then(
        (source) => {
          if (store.ui.promptResolver !== resolver) return;
          store.ui.promptSource = source;
          void loadPromptCompletions("");
        },
        () => {
          /* a source that fails to build just means no completions */
        },
      );
    } else if (Array.isArray(request.completions)) {
      setPromptItems(request.completions);
    } else {
      // Pending candidates land later; ignore them if the user has meanwhile
      // answered or cancelled and another prompt owns the input.
      void request.completions.then(
        (items) => {
          if (store.ui.promptResolver === resolver) setPromptItems(items);
        },
        () => {
          /* a failed candidate list just means no completions */
        },
      );
    }
  });
}

/** Publishes a prompt's candidates: labels normalized, and identical labels
 *  suffixed with their values so every listed row names exactly one thing
 *  (doc "Completion views"). */
function setPromptItems(items: CompletionItem[]): void {
  const pairs = completionLabels(items);
  store.ui.promptItems = pairs;
  store.ui.promptCompletions = pairs.map((item) => item.label);
}

/**
 * Reloads the active prompt's candidates from its source — the view being
 * shown, for the text typed so far — and publishes them. `more` comes with the
 * page: a truncated one means narrowing is another request (the driver asks
 * again as the user types), a complete one that narrowing is a local filter.
 */
export async function loadPromptCompletions(partial: string): Promise<void> {
  const source = store.ui.promptSource;
  const view = source?.views[store.ui.promptViewIndex];
  if (!source || !view) return;
  const page = await view.items(partial);
  // The prompt has ended, or another one owns the input now.
  if (store.ui.promptSource !== source) return;
  setPromptItems(page.items);
  store.ui.promptMore = page.more === true;
}

// What `dispatch` waits for once a command has run (lib/settle.ts): installed
// by the shell at startup, absent in tests that do not ask for it.
let settler: (() => Promise<void>) | null = null;

/** Installs the wait a command ends with (doc "When a command has finished"). */
export function setSettler(fn: (() => Promise<void>) | null): void {
  settler = fn;
}

/** Delivers what command transaction `txn` wrote so far and, when there was
 *  anything, waits until it shows (doc "Command transactions"). */
export async function flushTransaction(txn: number): Promise<void> {
  if ((await invoke<boolean>("txn_flush", { txn })) && settler) await settler();
}

/** How a dispatch came about. `record`: run from the keyboard — a key or the
 *  command input — so a macro being recorded records it (doc "Macros"). */
export interface DispatchOptions {
  record?: boolean;
}

/** Outcome of a dispatch, reported back to `POST /gui/command` waiters.
 *  `cancelled`: abandoned at one of its questions — nothing ran, and nothing
 *  failed either. */
export type DispatchResult =
  { ok: true; cancelled?: true } | { ok: false; error: string };

/** An array invocation in the shape `parseInvocation` gives a string one. */
function fromParts(
  parts: InvocationParts,
): { name: string; args: (string | AskMarker)[] } | { shell: string } | null {
  const [head, ...args] = parts;
  if (typeof head !== "string" || head.trim() === "") return null;
  if (head.startsWith("!")) {
    const shell = head.slice(1).trim();
    return shell === "" ? null : { shell };
  }
  return { name: head, args };
}

/**
 * Executes one invocation string (from a keybinding, the command input, or an
 * external `POST /gui/command`). The result lets external callers observe
 * success/failure; internal callers (keybindings, command input) ignore it.
 */
export async function dispatch(
  invocation: string | InvocationParts,
  context: ExecContext = focusedContext(),
  options: DispatchOptions = {},
): Promise<DispatchResult> {
  const parsed =
    typeof invocation === "string"
      ? parseInvocation(invocation)
      : fromParts(invocation);
  if (parsed === null) return { ok: true };
  // Invoked by a user command, in its transaction (doc "Command
  // transactions"): another user command joins it; anything else runs after
  // the writes so far are delivered, and outside it.
  let txn = context.txn;
  if (txn !== undefined && ("shell" in parsed || !isUserCommand(parsed.name))) {
    await flushTransaction(txn);
    context = { ws: context.ws, slot: context.slot };
    txn = undefined;
  }
  // A macro records what the keyboard runs (doc "Macros"), as it ran — and
  // neither the macro commands themselves nor the keystroke-level ones.
  const recorded =
    options.record === true &&
    macros.recording() &&
    ("shell" in parsed ||
      (!parsed.name.startsWith("macro:") &&
        shouldLogCommand(store.commands, parsed.name)));
  if ("shell" in parsed) {
    const { ws } = context;
    const expanded = await expandShellPlaceholders(
      parsed.shell,
      shellExpandDeps(ws),
    );
    if (!expanded.ok) {
      await status(expanded.error);
      return { ok: false, error: expanded.error };
    }
    await surfacePanel(context, "shell");
    await runShell(expanded.value, { typed: true, ws });
    // Recorded once it has run: the user who sees its effect may stop the
    // recording while the settler still waits.
    if (recorded) macros.record(`!${parsed.shell}`, [], new Set());
    if (settler) await settler();
    return { ok: true };
  }

  const { name, args: given } = parsed;
  let args = given as string[];
  const { ws } = context;

  // Interactive arguments (doc "Interactive command arguments"): a command declaring arguments
  // invoked with fewer than declared collects the missing tail through the
  // command input. Escape (null) abandons the whole invocation silently.
  // A panel command's spec belongs to the focused workspace's instance of the
  // owning panel, so that instance is mounted before the spec is read — the
  // same one `runCommand` will hand the collected arguments to.
  // (`if`, not `?.`: awaiting the undefined of an absent source would defer
  // the rest of the dispatch by a microtask for nothing.)
  if (panelArgs) await panelArgs.prepare(name, ws);
  const specs = argSpecFor(name, ws);
  // The arguments `macro:ask` marked while this command asked for them.
  const marks = new Set<number>();
  if (specs) {
    // Asking the user shows the screen: the caller's writes are delivered first.
    const caller = txn;
    const prompt: typeof promptForArg =
      caller === undefined
        ? promptForArg
        : async (request) => {
            await flushTransaction(caller);
            return promptForArg(request);
          };
    let collected: string[] | null;
    try {
      collected = await collectArgs(
        specs,
        given,
        recorded
          ? (request) =>
              macros.asking(marks, request.position, () => prompt(request))
          : prompt,
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      await status(message);
      return { ok: false, error: message };
    }
    if (collected === null) return { ok: true, cancelled: true };
    args = collected;
  } else if (given.some(isAsk)) {
    const message = `${name} declares no arguments to ask for`;
    await status(message);
    return { ok: false, error: message };
  }

  if (ws) store.lastCommand[ws] = name;

  // Echo the invocation to the message panel (unless the command opts out,
  // e.g. the basic editing primitives). Awaited so it lands before any
  // output the command itself appends. Interactively-collected arguments are
  // reassembled so the echo reflects what actually ran.
  if (ws && shouldLogCommand(store.commands, name)) {
    const echo = [name, ...args].join(" ").trim();
    await invoke("append_message", { wsId: ws, text: `> ${echo}` });
  }

  // What the command writes is one label in the log, named after it (doc
  // "Labels and contexts"): the proxy tells the daemon while it runs.
  const commandKey = await beginCommand([name, ...args].join(" ").trim());
  // A user command not invoked by another, a panel's command and a generic
  // one run in a transaction of their own.
  let own: number | null = null;
  if (txn === undefined && runsInTransaction(name)) {
    const begun = await invoke<unknown>("txn_begin");
    if (typeof begun === "number") {
      own = begun;
      context = { ...context, txn: own };
    }
  }
  try {
    const handled = await runCommand(name, args, context);
    if (!handled) {
      const message = `unknown command: ${name}`;
      await status(message);
      return { ok: false, error: message };
    }
    // Delivered before the wait, which is for what the writes set off.
    if (own !== null) {
      const ending = own;
      own = null;
      await invoke("txn_end", { txn: ending });
    }
    // Recorded once it has run, as the shell line above.
    if (recorded) macros.record(name, args, marks);
    // Resolved when its effect is visible, not when its handler returned.
    if (settler) await settler();
    return { ok: true };
  } catch (error) {
    // An Error says its message; the "Error: " of its String() is noise.
    const message = error instanceof Error ? error.message : String(error);
    await status(message);
    return { ok: false, error: message };
  } finally {
    // A failed command delivers what it wrote, as its daemon writes stay.
    if (own !== null) await invoke("txn_end", { txn: own });
    await endCommand(commandKey);
  }
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

/** Tells the daemon proxy a command starts; its key, or null when the proxy
 *  cannot be told (a test without the Rust side). */
async function beginCommand(name: string): Promise<string | null> {
  try {
    return await invoke<string>("command_begin", { name });
  } catch {
    return null;
  }
}

/** Tells the daemon proxy the command `key` ended. */
async function endCommand(key: string | null): Promise<void> {
  if (key === null) return;
  try {
    await invoke("command_end", { key });
  } catch {
    // The proxy keeps nothing that outlives the next command's begin.
  }
}

/**
 * Routes a parsed command to its handler. Returns true when the command was
 * recognised (a shell builtin, a goto-tab shortcut, or a panel command),
 * false for an unknown name. Throws on handler failure (caught by `dispatch`).
 */
async function runCommand(
  name: string,
  args: string[],
  context: ExecContext,
): Promise<boolean> {
  const { ws } = context;
  switch (name) {
    case "command-input:focus":
      // One widget, two modes: command (`:`) and bash (`!`, the line runs as a
      // shell command). It is always visible, so focusing is all there is to
      // do. Default to the command mode when nothing is named.
      if (args[1] !== undefined && args[1] !== "clear") {
        await status(`unknown modifier: "${args[1]}" (expected clear)`);
        return true;
      }
      store.ui.commandInputClear = args[1] === "clear";
      if (args[0] === "bash") store.ui.bashInputFocusTick += 1;
      else if (args[0] === undefined || args[0] === "command")
        store.ui.commandInputFocusTick += 1;
      else {
        await status(`unknown mode: "${args[0]}" (expected command / bash)`);
        return true;
      }
      return true;
    // editing:* acts on the shell command input (editingTarget) when set,
    // otherwise on the deep-focused panel input (replacing the old per-panel
    // shim handlers). Only `confirm` stays command-input-only — Enter must
    // reach a panel form's own keydown handler (see keys.ts).
    case "editing:unfocus":
      if (editingTarget) editingTarget.unfocus();
      else (deepActiveElement() as HTMLElement | null)?.blur();
      return true;
    case "editing:validate":
      validateField(deepActiveElement());
      return true;
    case "editing:discard":
      if (editingTarget) editingTarget.discard();
      else discardActiveInput(deepActiveElement());
      return true;
    case "editing:confirm":
      editingTarget?.confirm();
      return true;
    case "editing:uppercase":
      uppercaseSelection(deepActiveElement());
      return true;
    case "editing:clear-line-end":
      if (editingTarget) editingTarget.clearToLineEnd();
      else clearToLineEnd(deepActiveElement());
      return true;
    case "history:prev":
    case "history:next":
    case "history:search":
      await runHistoryCommand(name, args[0], deepActiveElement(), focusedPanelRoot());
      return true;
    case "completion:cycle": {
      // Walks the views of the prompt's completion (doc "Completion
      // views"): one builder is the common case and needs no cycling, so this
      // is a no-op — a keystroke on a plain prompt must not shout. The input
      // owns the cycle (it owns the draft the candidates narrow on).
      if (
        args[0] !== undefined &&
        args[0] !== "forward" &&
        args[0] !== "back"
      ) {
        await status(
          `unknown direction: "${args[0]}" (expected forward / back)`,
        );
        return true;
      }
      editingTarget?.cycleCompletion?.(args[0] === "back" ? -1 : 1);
      return true;
    }
    case "editing:goto": {
      if (args[0] !== "line-start" && args[0] !== "line-end") {
        await status(
          `unknown target: "${args[0] ?? ""}" (expected line-start / line-end)`,
        );
        return true;
      }
      if (args[0] === "line-start") {
        if (editingTarget) editingTarget.lineStart();
        else gotoLineEdge(deepActiveElement(), "line-start");
        return true;
      }
      if (editingTarget) {
        editingTarget.lineEnd();
      } else {
        gotoLineEdge(deepActiveElement(), "line-end");
      }
      return true;
    }
    case "workspace:new":
      // Optional parameter: the repo UUID of the new workspace
      // (used by the repos panel).
      await invoke("workspace_new", { activeRepo: args[0] ?? null });
      return true;
    case "workspace:fork":
      // A copy of the context's workspace, in a new tab (doc "Workspaces").
      if (ws) await invoke("workspace_fork", { wsId: ws });
      return true;
    case "workspace:send": {
      // `workspace:send <id> <key> <value…>`: the value is JSON when it parses,
      // the text otherwise — the last argument absorbs the remaining tokens,
      // so a query needs no quoting.
      const [target, key, ...rest] = args;
      if (!target || !key) return true;
      const text = rest.join(" ");
      let value: unknown = text;
      try {
        value = JSON.parse(text);
      } catch {
        /* plain text */
      }
      await invoke("workspace_send", { wsId: target, vars: { [key]: value } });
      return true;
    }
    case "workspace:close":
      if (!confirmClosing(ws ?? null)) return true;
      // The context's workspace, which a command may have moved to
      // (`mf.withContext`): the focused one only without a context.
      if (ws) await invoke("workspace_close_ws", { wsId: ws });
      else await invoke("workspace_close");
      return true;
    case "workspace:rename": {
      // The name was collected by dispatch (inline or in the minibuffer); as
      // the last argument it absorbs the remaining tokens, so a name may
      // contain spaces without quoting.
      const name = args.join(" ");
      if (ws) await invoke("workspace_rename", { wsId: ws, name });
      return true;
    }

    // Saved workspaces (doc "Saved workspaces"). The name is the last
    // argument and absorbs the remaining tokens, as workspace:rename's does.
    case "workspace:save": {
      const [kind, ...rest] = args;
      const name = rest.join(" ");
      if (!ws || !kind || !name) return true;
      const exists = (await savedWorkspaces()).some((w) => w.name === name);
      if (
        exists &&
        !window.confirm(`Replace the workspace saved as "${name}"?`)
      )
        return true;
      await invoke("saved_workspace_save", { wsId: ws, kind, name });
      await status(`workspace saved as "${name}" (${kind})`, "info");
      return true;
    }
    case "workspace:open": {
      const [target, ...rest] = args;
      const name = rest.join(" ");
      if (!ws || !target || !name) return true;
      await invoke("saved_workspace_open", { wsId: ws, target, name });
      return true;
    }
    case "workspace:set-default": {
      const name = args.join(" ");
      if (!ws || !name) return true;
      await invoke("saved_workspace_set_default", { wsId: ws, name });
      await status(`"${name}" is the repository's default workspace`, "info");
      return true;
    }
    case "workspace:unset-default":
      if (!ws) return true;
      await status(
        (await invoke<boolean>("saved_workspace_unset_default", { wsId: ws }))
          ? "the repository has no default workspace any more"
          : "the repository had no default workspace",
        "info",
      );
      return true;
    case "workspace:delete-saved": {
      const name = args.join(" ");
      if (!ws || !name) return true;
      const deleted = await invoke<boolean>("saved_workspace_delete", {
        wsId: ws,
        name,
      });
      await status(
        deleted
          ? `saved workspace "${name}" deleted`
          : `no workspace saved as "${name}"`,
        deleted ? "info" : "error",
      );
      return true;
    }

    case "workspace:goto": {
      // The 1-based workspace position is the parameter (no longer baked
      // into the command name). Moves BOTH panels.
      const n = Number(args[0]);
      if (Number.isInteger(n) && n > 0) await invoke("workspace_goto", { n });
      else await status(`not a workspace number: "${args[0] ?? ""}"`);
      return true;
    }
    // Bare, both panels move together so the two slots never drift onto
    // different workspaces unnoticed; `slot` moves only the focused one.
    case "workspace:next":
      await invoke(
        args[0] === "slot" ? "workspace_next_in_slot" : "workspace_next",
      );
      return true;
    case "workspace:prev":
      await invoke(
        args[0] === "slot" ? "workspace_prev_in_slot" : "workspace_prev",
      );
      return true;
    case "panel:split":
      await invoke("panel_split");
      return true;
    case "panel:unsplit":
      await invoke("panel_unsplit");
      return true;
    case "panel:hide":
      await invoke("slot_hide", { slot: context.slot });
      return true;
    case "panel:toggle":
      if (args[0] === "split") await invoke("panel_split_toggle");
      else if (args[0] === "fullscreen")
        await setFullscreen(!store.ui.fullscreen);
      else
        await status(
          `unknown flag: "${args[0] ?? ""}" (expected split / fullscreen)`,
        );
      return true;
    case "panel:focus":
      // `next` is the other slot; `left`/`right` name one outright, so a
      // keybinding can reach a slot without knowing which one holds the focus.
      if (args[0] === undefined || args[0] === "next")
        await invoke("panel_focus_next");
      else if (args[0] === "cycle") await focusCycle();
      else if (args[0] === "left" || args[0] === "right")
        await invoke("focus_slot", { slot: args[0] });
      else
        await status(
          `unknown slot: "${args[0]}" (expected next / left / right / cycle)`,
        );
      return true;
    case "panel:open": {
      // `panel:open <here|other|new> <type>` (doc "Panel slots and layout").
      const [where, panelType] = args;
      if (!isTarget(where)) {
        await status(
          `unknown target: "${where ?? ""}" (expected ${TARGETS.join(" / ")})`,
        );
        return true;
      }
      if (!panelType) return true;
      const nothing = await viewerHasNothing(panelType, context.ws);
      if (nothing) {
        if (context.ws) {
          await invoke("post_status", {
            wsId: context.ws,
            text: nothing,
            kind: "error",
            timeoutMs: 5000,
          });
        }
        return true;
      }
      const target = await resolveTarget(where, context, invoke);
      // A context's workspace need not be on screen: shown in its slot first.
      if (
        target.ws &&
        slotPayload(target.slot).workspace_id !== target.ws &&
        where === "here"
      ) {
        await invoke("tab_assign", { wsId: target.ws, slot: target.slot });
      }
      await invoke("panel_set_type", { slot: target.slot, panelType });
      return true;
    }
    case "slot:show": {
      // `slot:show <left|right> <type> [path]` (doc "slot:show"). The slot
      // first: refused, it leaves the selection as it was. The pair is then
      // held by the transaction, so it reaches the panels together.
      const [slot, panelType, path] = args;
      if (!panelType) throw new Error("slot:show needs a slot and a panel type");
      const target = await invoke<{
        ws: string;
        selection: Record<string, unknown> | null;
      }>("slot_show_target", {
        slot,
        path: panelType === "viewer" ? (path ?? null) : null,
      });
      await invoke("tab_assign", { wsId: target.ws, slot });
      await invoke("panel_set_type", { slot, panelType });
      if (target.selection) {
        await invoke("workspace_send", {
          wsId: target.ws,
          vars: target.selection,
          txn: context.txn,
        });
      }
      return true;
    }
    case "panel:swap":
      await invoke("panel_swap");
      return true;
    case "message:clear":
      if (ws) await invoke("clear_messages", { wsId: ws });
      return true;
    case "shell:clear":
      if (ws) await invoke("clear_shell", { wsId: ws });
      return true;
    case "config:reload": {
      const report = await invoke<string>("config_reload", { what: args[0] });
      // The command module is the shell's to re-import; Rust has only checked
      // that the file is readable.
      if (args[0] === "commands" || args[0] === "all")
        await reloadUserCommands();
      await status(report, "info");
      return true;
    }
    case "ignore:add":
    case "ignore:remove":
    case "ignore:set": {
      // The `preset` argument was collected by dispatch (with completion).
      const choice = args.join(" ").trim();
      if (choice)
        await runIgnore(
          choice,
          name.slice("ignore:".length) as "add" | "remove" | "set",
          ws,
        );
      return true;
    }
    case "ignore:list":
      await listIgnore(ws, context.slot);
      return true;
    case "mf:order":
      await runOrder(ws);
      return true;
    case "reconcile:run":
      if (ws) await invoke("reconcile_run", { wsId: ws });
      return true;
    case "mf:duplicate":
      if (args[0] !== "scan") {
        await status(`unknown operation: "${args[0] ?? ""}" (expected scan)`);
        return true;
      }
      // The GUI half of `mf duplicate scan` (doc "duplicates panel"). Options
      // stay CLI-only, as `mf trash prune`'s do: this runs the ordinary
      // whole-repository scan, and the Rust side posts its own status.
      if (ws) await invoke("duplicate_scan", { wsId: ws });
      return true;
    case "mf:watch-activity":
      if (args[0] !== "reset") {
        await status(`unknown operation: "${args[0] ?? ""}" (expected reset)`);
        return true;
      }
      await resetWatchActivity(ws);
      return true;
    case "orphan:detect":
      // Mark the orphaned metarecords (doc "Finding and clearing orphans").
      // The Rust side posts its own status; swallow the rejection so an error
      // is not surfaced twice.
      if (ws) await invoke("orphan_detect", { wsId: ws }).catch(() => 0);
      return true;
    case "context:set":
      // Both were collected by dispatch, the description with all its words.
      if (ws)
        await setWorkContext(
          ws,
          (args[0] ?? "").trim(),
          args.slice(1).join(" ").trim(),
        );
      return true;
    case "context:unset":
      if (ws) await unsetWorkContext(ws);
      return true;
    case "clipboard:clear":
      await invoke("global_send", { vars: { clipboard: null } });
      if (ws) {
        await invoke("post_status", {
          wsId: ws,
          text: "Clipboard emptied",
          kind: "info",
          timeoutMs: 5000,
        });
      }
      return true;
    case "log:undo":
    case "log:redo":
      if (ws) await undoOrRedo(ws, name === "log:redo", args[0]);
      return true;
    case "repos:open":
      await invoke("panel_set_type", {
        slot: context.slot,
        panelType: "repos",
      });
      return true;
    case "file:open-with":
      // The `program` argument was collected by dispatch (completing over the
      // configured list); the whole tail is the command line, so `gimp -n` and
      // any other flags survive.
      await openWith(args.join(" "), ws);
      return true;
    case "script:run": {
      // The `script` argument was collected by dispatch (with completion);
      // args[0] is the picked "<name> — <summary>" line (or a bare name).
      const choice = args.join(" ").trim();
      if (!choice) return true;
      const path = await resolveScriptPath(choice);
      if (!path) {
        // Where to look, and how to start one, for whoever tried a name
        // (doc "mf script").
        await status(
          `no installed script matches "${choice}" (mf script lists them; mf script new starts one from a template)`,
        );
        return true;
      }
      await runScript(path, context);
      return true;
    }
    case "find:open":
      openFind(args[0]);
      return true;
    case "find:next":
      stepFind(1);
      return true;
    case "find:prev":
      stepFind(-1);
      return true;
    case "find:close":
      closeFind();
      return true;

    case "hints:open":
      openHints();
      return true;

    case "help:open":
      if (ws) await openHelp(ws, context.slot, args.join(" "), true);
      return true;
    case "help:command": {
      // The command's page (doc "In-app help"): what `help:open <name>` finds,
      // asked with the registered commands as completions. A name nothing
      // registered is refused rather than searched for.
      const name = args[0] ?? "";
      if (!store.commands.some((c) => c.name === name)) {
        await status(`no command named ${name}`);
        return true;
      }
      if (ws) await openHelp(ws, context.slot, name);
      return true;
    }
    case "help:cursor":
      // Arm the `?` cursor: the next click (or escape) is intercepted in keys.ts.
      // One help gesture at a time: arming it drops a pending describe-key wait.
      store.ui.describeKeys = null;
      store.ui.pendingKeys = null;
      store.ui.helpCursorActive = true;
      setHelpCursor(true);
      return true;
    case "help:key":
      // Arm the describe-key wait (doc "In-app help"): the next key combo is
      // swallowed and reported on instead of dispatched. Like the help cursor
      // above, it takes over from the other help gesture.
      if (store.ui.helpCursorActive) {
        store.ui.helpCursorActive = false;
        setHelpCursor(false);
      }
      store.ui.describeKeys = [];
      store.ui.pendingKeys = null;
      flashStatus("Describe which key? (escape cancels)");
      return true;
    case "daemon:set-url": {
      // The URL was collected by dispatch (inline or in the minibuffer).
      const url = args[0] ?? "";
      if (url === "") {
        await status("no daemon URL given");
        return true;
      }
      const connected = await invoke<boolean>("daemon_set_url", { url });
      store.daemonUrl = url;
      await status(
        `daemon URL set; ${connected ? "connected" : "unreachable"}`,
        "info",
      );
      return true;
    }
    case "answer:send":
      // Resolves a script's POST /gui/input wait: the question on screen
      // (doc "Script sessions").
      await invoke("answer_send", {
        value: args.join(" "),
        id: activeQuestion()?.id ?? null,
      });
      return true;
    // Macros (doc "Macros").
    case "macro:toggle-recording": {
      const done = macros.toggle();
      if (done === null) {
        await status(
          "recording a macro: macro:toggle-recording again to stop",
          "info",
        );
      } else if (done.length === 0) {
        await status(
          "nothing recorded: the macro recorded before stays",
          "info",
        );
      } else {
        await status(
          `macro recorded: ${done.length} command${done.length === 1 ? "" : "s"}`,
          "info",
        );
      }
      return true;
    }
    case "macro:ask": {
      const marked = macros.mark();
      if (marked === null)
        throw new Error(
          "macro:ask marks the question on screen, while recording",
        );
      await status(
        marked
          ? "asked again each time the macro runs"
          : "answered once for all, as recorded",
        "info",
      );
      return true;
    }
    case "macro:run": {
      if (macros.recording()) {
        throw new Error(
          "a macro is being recorded: stop it first (macro:toggle-recording)",
        );
      }
      const steps = macros.last();
      if (steps === null) throw new Error("no macro recorded yet");
      for (const [i, step] of steps.entries()) {
        // Each step from the focus as it is then, like a typed command.
        const result = await dispatch(step);
        const where = `step ${i + 1} of ${steps.length} (${step[0]})`;
        if (!result.ok)
          throw new Error(`macro stopped at ${where}: ${result.error}`);
        if (result.cancelled) {
          await status(`macro abandoned at ${where}`, "info");
          return true;
        }
      }
      return true;
    }
    case "macro:save": {
      const steps = macros.last();
      if (steps === null) throw new Error("no macro recorded yet");
      const given = args.join(" ").trim();
      const name = given.startsWith("user:") ? given : `user:${given}`;
      if (!/^user:[A-Za-z0-9_][A-Za-z0-9_-]*$/.test(name)) {
        throw new Error(
          `"${given}" is not a command name: one word, letters, digits, - and _`,
        );
      }
      if (store.commands.some((command) => command.name === name)) {
        throw new Error(`${name} is already a command: pick another name`);
      }
      await invoke("macro_save", { name, entry: macroEntry(name, steps) });
      await reloadUserCommands();
      await status(`macro saved as ${name}, in macros.js`, "info");
      return true;
    }
    case "script-keys:toggle": {
      // The question bar's checkbox: hand the script's keys back to the panels,
      // or take them again. Rust owns the flag and re-broadcasts the questions
      // with it (doc "Script sessions").
      const enabled = await invoke<boolean>("script_keys_toggle");
      await status(
        enabled ? "script keys enabled" : "script keys disabled",
        "info",
      );
      return true;
    }
    case "script:stop":
      // Escape during a question, or a manual invocation: end the run (and the
      // question with it). No argument = the script asking the question on
      // screen.
      await invoke("script_stop", {
        task: args[0] ?? null,
        kill: false,
        question: activeQuestion()?.id ?? null,
      });
      return true;
    case "script:kill":
      // Ctrl+escape during a question: the same, with SIGKILL — for a script
      // that did not end on the stop.
      await invoke("script_stop", {
        task: args[0] ?? null,
        kill: true,
        question: activeQuestion()?.id ?? null,
      });
      return true;
    case "status:clear": {
      // Dismiss the transient status-bar message (and the last-command echo).
      if (ws) {
        store.status[ws] = { text: "", kind: "info", timeout_ms: null };
        store.lastCommand[ws] = "";
      }
      return true;
    }
    case "pick:confirm":
    case "pick:cancel":
      // Hands the focused picker's selection back to the calling form (confirm)
      // or abandons it (cancel). Best-effort: a stray press outside a picker is
      // a silent no-op rather than an error toast.
      try {
        await invoke(name === "pick:confirm" ? "pick_confirm" : "pick_cancel");
      } catch {
        /* no active value picker */
      }
      return true;
    case "devtools:open":
      await invoke("open_devtools");
      return true;
    case "quit":
      if (!confirmClosing(null)) return true;
      await invoke("quit");
      return true;
  }

  // A generic panel command: run on the functions its panel provides.
  if (GENERIC_COMMANDS.includes(name)) {
    const [panel, ...rest] = args;
    const panelType = genericPanel(panel, context);
    if (!ws) throw new Error("no workspace in the focused slot");
    // Bound to this run, like a panel command's handler: what they write is
    // in its transaction.
    const functions = await functionProvider()?.get(ws, panelType, context);
    await runGeneric(name, functions ?? {}, rest, {
      panelType,
      files: fileOps(ws, context),
      reinvoke: (invocation) => void dispatch(invocation, context),
      clip: async (paths, mode) => {
        // The file clipboard (doc "Global GUI variables"), written in the run.
        await invoke("global_send", {
          vars: { clipboard: { paths, mode } },
          txn: context.txn,
        });
        const what =
          paths.length === 1
            ? pathText(paths[0]).replace(/\/+$/, "").split("/").pop()
            : `${paths.length} files`;
        await invoke("post_status", {
          wsId: ws,
          text: `${mode === "cut" ? "Cut" : "Copied"} ${what}`,
          kind: "info",
          timeoutMs: 5000,
        });
      },
    });
    return true;
  }

  // Not a shell builtin: a user command, then a panel's. Builtins win, so a
  // commands.js entry can add to the set but never quietly replace part of it.
  if (await runUserCommand(name, args, context)) return true;

  const command = store.commands.find((c) => c.name === name);
  if (command && command.owner && panelDispatch) {
    await panelDispatch(command, args, context);
    return true;
  }
  return false;
}
