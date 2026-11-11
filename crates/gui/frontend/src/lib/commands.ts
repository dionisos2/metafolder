// Command dispatch (doc "The command input"): an invocation's arguments
// collected, then routed — to a builtin (one table per family, in
// `builtins/`), a generic panel command, a user command, or a panel's own.

import { isWirePath, wirePaths } from "./treePath";
import { CONSTRAIN_VAR, recordPath } from "../../../panel-shim/file-actions.js";
import { pathText } from "../../../panel-shim/ui.js";
import { type FileOps } from "./fileCommands";
import { bindView, completionLabels } from "./completions";
import type { CompletionItem } from "./completions";
import { invoke } from "./ipc";
import { type ExpandDeps, expandShellPlaceholders } from "./placeholders";
import {
  focusedContext,
  focusedWs,
  hasPromptIn,
  offerPrompt,
  slotPayload,
  store,
} from "./store.svelte";
import { functionProvider, GENERIC_COMMANDS, runGeneric } from "./panelFunctions";
import type { CommandDef, ExecContext, LayoutView, WorkspaceInfo } from "./types";
import { type ArgPromptFn, type ArgPromptRequest, type ArgSpec, type AskMarker, type InvocationParts, type PromptAnswer, argSpecFor, isAsk, panelArgs } from "./commandArgs";
import { parseInvocation, shouldLogCommand } from "./commandParsing";
import { isUserCommand, runUserCommand, runsInTransaction } from "./commandRegistry";
import { genericPanel } from "./builtinArgs";
import { macros } from "./macro";
import { status } from "./status";
import { builtins } from "./builtins";
import { repoOf, repoRoot } from "./builtins/repository";

export * from "./commandParsing";
export * from "./commandArgs";
export * from "./commandRegistry";
export { status } from "./status";
export { setFullscreen } from "./builtins/layout";
export {
  type EditingTarget,
  clearToLineEnd,
  deepActiveElement,
  discardActiveInput,
  focusedPanelRoot,
  gotoLineEdge,
  hasEditingTarget,
  setEditingTarget,
  uppercaseSelection,
} from "./builtins/editing";


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
    constrainedRoot: async () => {
      const active = repoOf(ws);
      if (!active) return null;
      const constrained = await invoke<unknown>("ws_get_var", {
        wsId: ws,
        key: CONSTRAIN_VAR,
        txn: context.txn,
      });
      if (constrained === false) return null;
      return (await repoRoot(active)) || null;
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

// ── Dispatch ───────────────────────────────────────────────────────────


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
 *  swapping the two slots, what asking for a type the other slot shows does). */
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
export async function surfacePanel(
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
export async function promptForArg(
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
  const builtin = builtins()[name];
  if (builtin) {
    await builtin(args, context, name);
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
